# Developer guide

How to put Tokenrein in front of your own agents and coding tools, what happens on each call,
and how to run Tokenrein from source.

- [Concepts](#concepts)
- [Connect an app with the SDKs](#connect-an-app-with-the-sdks)
- [Connect coding agents](#connect-coding-agents)
- [Connect anything through the proxy](#connect-anything-through-the-proxy)
- [Budgets, modes and what callers see](#budgets-modes-and-what-callers-see)
- [Loop guard](#loop-guard)
- [Proxy-free endpoints](#proxy-free-endpoints)
- [Model prices](#model-prices)
- [Run from source](#run-from-source)

## Concepts

| Term | Meaning |
|---|---|
| **Org** | A team or business unit. Has members, provider keys, alert channels and default budgets. |
| **Project** | Something that spends: an app, a team's coding agents, a pipeline. Has its own Tokenrein keys (`tr_…`) and can override the org's budgets. |
| **Agent** | A named actor inside a project (`support-bot`, `claude-code`). Gets a daily budget. |
| **Run** | One task: a ticket, a conversation, a coding session. Runs nest: spend of a sub-agent run counts against its parent too. A run can be paused, resumed or killed. |
| **User** | The person or service an agent works for, for spend reports (emails allowed). |

Budgets are set at org level and inherited by projects, which can override them.

## Connect an app with the SDKs

Python (`pip install tokenrein`) and TypeScript (`npm install @tokenrein/sdk`) wrap the official
OpenAI and Anthropic clients. Your provider key stays with your client; the SDK needs a Tokenrein
key from the dashboard.

```bash
export TOKENREIN_API_KEY=tr_…                 # a key for your project
export TOKENREIN_BASE_URL=https://tokenrein.example.com:8787
```

```python
from anthropic import Anthropic
from tokenrein import guard, BudgetExceeded, RunPaused

client = guard.anthropic(Anthropic(), agent="support-bot", user="ana@example.com")

try:
    with guard.run("ticket-4812", budget_usd=5):          # root run, $5
        plan = client.messages.create(model="claude-sonnet-5-5", max_tokens=1024, messages=[...])
        with guard.run(budget_usd=1):                     # sub-agent run: parent = ticket-4812
            research(client)
except RunPaused as e:        # waiting for an approver, or paused by an admin
    queue_for_later(e.run_id)
except BudgetExceeded as e:   # an enforced budget is used up
    log.warning("stopped %s: %s", e.run_id, e.message)
```

```ts
import Anthropic from '@anthropic-ai/sdk';
import { guard, BudgetExceeded, RunPaused } from '@tokenrein/sdk';

const client = guard.anthropic(new Anthropic(), { agent: 'support-bot' });
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, () =>
  client.messages.create({ model: 'claude-sonnet-5-5', max_tokens: 1024, messages: [...] }));
```

What the SDK does on each call (direct mode, the default):

1. Asks Tokenrein whether the run may continue (`POST /v1/check`), with the run, agent, model, the
   output cap and a **fingerprint**: a hash of the model and the last message, used to spot a run
   repeating itself. The content itself is never sent.
2. Calls your provider as usual.
3. Reads the token usage from the response (streams too) and reports it in the background
   (`POST /v1/usage`).

If Tokenrein cannot be reached, calls go through (fail open) and reports are retried. Short-lived
scripts call `tokenrein.flush()` (Python flushes at exit too). `mode="proxy"` sends calls through
the proxy instead, for exact enforcement under heavy concurrency.

Other OpenAI-compatible providers work through the OpenAI client's `base_url`: Google Gemini,
Groq, Mistral, DeepSeek, xAI, OpenRouter, Together, Fireworks, Azure OpenAI, and local Ollama,
vLLM or LM Studio (priced at $0). The full SDK reference, including errors and a local call cap
that holds even when Tokenrein is unreachable, is in [sdk/README.md](../sdk/README.md).

## Connect coding agents

Each coding agent is connected with one command per machine. The dashboard's **Connect** page
creates a key for the project you pick and shows the command:

```bash
curl -fsSL https://tokenrein.example.com/connect/claude-code.py | python3 - --key tr_… --url https://tokenrein.example.com:8787
curl -fsSL https://tokenrein.example.com/connect/codex.py       | python3 - --key tr_… --url …
curl -fsSL https://tokenrein.example.com/connect/gemini.py      | python3 - --key tr_… --url …
curl -fsSL https://tokenrein.example.com/connect/cursor.py      | python3 - --key tr_… --url …
```

The scripts use only the Python standard library, back up every file they change, and are undone
with `--remove`. Optional: `--agent NAME`, `--user EMAIL`.

| Agent | Usage comes from | Stopped through |
|---|---|---|
| Claude Code | its OpenTelemetry logs (model, tokens, cost per call; works with subscriptions too) | HTTP hooks: the next prompt, tool call or sub-agent is refused |
| Codex CLI | its OpenTelemetry logs | command hooks (run `/hooks` once in Codex to trust them) |
| Gemini CLI | its OpenTelemetry logs | command hooks, including before each model call |
| Cursor | the team's Admin API (an admin saves its key on the Connect page; imported hourly) | hooks before prompts, tool calls, shell and MCP commands |

Each session is a run (`cc-<session>`, `codex-<session>`, `gemini-<session>`,
`cursor-<conversation>`); sub-agents get their own runs under it. Hooks also feed the loop guard:
the same tool call with the same input over and over pauses the session. What each connector reads
and changes is in [connectors.md](connectors.md).

## Connect anything through the proxy

Any OpenAI- or Anthropic-compatible tool (Cline, Aider, Continue, LibreChat, n8n, your own HTTP
calls) can use Tokenrein as its base URL: `https://tokenrein.example.com:8787` (OpenAI-style:
`…/v1`) with a Tokenrein key as the API key. Tokenrein forwards to the provider with the org's
provider key, which agents never see.

| Request header | Meaning |
|---|---|
| `Authorization: Bearer tr_…` or `x-api-key: tr_…` | Tokenrein key |
| `x-tr-run-id` | Run id; without it each request is its own run |
| `x-tr-parent-run-id` | Parent run, for sub-agents (send it on every call of the child run) |
| `x-tr-agent` | Agent name (default `default`) |
| `x-tr-user` | The person or service the agent works for (not sent upstream) |
| `x-tr-run-budget-usd` | Budget for this run, fixed on first use, capped by the org's `max_run_usd` |

Responses carry `x-tr-run-id`, and `x-tr-warning` when a Check-mode budget would have blocked the
call.

## Budgets, modes and what callers see

| Budget | Scope |
|---|---|
| `org_monthly` | everything in the org, per calendar month (UTC) |
| `project_monthly` | each project, per month |
| `agent_daily` | each agent name within a project, per day (UTC) |
| `run` | each run (unless the caller sends `x-tr-run-budget-usd`), fixed when the run starts |
| `max_run_usd` | the most a caller may ask for with `x-tr-run-budget-usd` |

| Mode | Over the limit |
|---|---|
| `skip` | spend is only tracked |
| `check` (default) | allowed, with a warning header and an entry in the Check-mode review |
| `enforce` | refused: HTTP 402 `tokenrein_budget_exceeded` (SDKs raise `BudgetExceeded`) |
| `ask` (run budgets) | the run is paused and an approver is asked: HTTP 403 `tokenrein_run_paused` until they add budget (the run continues) or stop it (`tokenrein_run_killed`) |

Errors have the provider's own shape (`{"error": {"type": …}}` for OpenAI,
`{"type": "error", "error": {…}}` for Anthropic), so existing error handling keeps working, and
SDKs do not retry them. In direct mode the call that crosses a limit has already happened; the
next one is refused. In proxy mode the worst-case cost is reserved first, so the crossing call
itself is refused.

Each org also has a `fail_mode`: `open` (default, keep serving when Redis is unreachable, marked
`x-tr-degraded`) or `closed` (HTTP 503).

## Loop guard

Set under Keys & limits (org default, project override) or as `loops` in the budgets:

```toml
[orgs.budgets]
loops = { action = "pause", max_repeats = 6, repeat_window_secs = 600, max_usd_per_minute = 2.0, max_calls_per_run = 300 }
```

| Signal | Trips when |
|---|---|
| Repeats | the same request (model and last message) or the same tool call is seen `max_repeats` times within the window |
| Velocity | the run spends more than `max_usd_per_minute` within one minute |
| Calls | the run makes more than `max_calls_per_run` model calls |

`action`: `warn` (alert only), `pause` (the run waits for an approver; letting it continue resets
the guard for that run) or `kill`. Only hashes of requests are kept.

## Proxy-free endpoints

These are on the proxy listener (`:8787`) and take a Tokenrein key. The SDKs and connectors use
them; you can call them from anything.

| Endpoint | What it does |
|---|---|
| `POST /v1/check` | "May this run make another call?" Body: `run_id`, `parent_run_id`, `agent`, `user`, `model`, `max_output_tokens` or `estimate_usd`, `fingerprint` (16-128 hex). Answers `{allowed, reason, scope, run_state}` |
| `POST /v1/usage` | Report calls that happened: `{"events": [{run_id, parent_run_id, agent, user, provider, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd?, duration_ms}]}`. Priced from the model list unless `cost_usd` is given. Answers the enforced limits now used up |
| `POST /v1/otel/v1/logs` | OpenTelemetry logs (HTTP/JSON) from Claude Code, Codex CLI and Gemini CLI |
| `POST /v1/hooks/{claude-code,codex,gemini,cursor}` | The agents' hooks; answers in each agent's own format and fails open |

## Model prices

Built-in prices cover OpenAI, Anthropic, Google Gemini, Mistral, DeepSeek, xAI, Groq and Cohere;
local providers cost $0, and reseller model names are understood (`us.anthropic.claude-…`,
`anthropic/claude-…`). Unknown models are charged at the provider's most expensive price until you
add one under **Models & prices**. Check built-in prices against your provider's price page
before using them for chargeback.

## Run from source

Prerequisites: Rust (stable), `redis-server`; Postgres 15+ for accounts, config history and the
audit log (optional for trying it out).

```bash
redis-server --daemonize yes
printf 'TOKENREIN_ADMIN_TOKEN=%s\nTOKENREIN_SECRETS_KEY=%s\n' "$(openssl rand -hex 32)" "$(openssl rand -hex 32)" > .env
# optional: echo 'TOKENREIN_DATABASE_URL=postgres://localhost/tokenrein' >> .env
set -a; source .env; set +a
cargo run -p tokenrein --release              # config: config/dev.toml (TOKENREIN_CONFIG)
```

The dashboard is on <http://127.0.0.1:8788/> (sign in with the admin token, or create an account
with `cargo run -p tokenrein -- create-owner you@example.com` when a database is set), the proxy
on `:8787`. Keep `TOKENREIN_SECRETS_KEY` the same across restarts: provider keys saved in the
dashboard are encrypted with it.

Tests:

```bash
cargo test                                    # TOKENREIN_REQUIRE_REDIS=1 / _POSTGRES=1 make missing servers fail
cd sdk/python && .venv/bin/pytest
cd sdk/typescript && npm test
```

The Rust integration tests start their own `redis-server` and Postgres (`initdb`) on free ports,
fake OpenAI and Anthropic upstreams, a mock OpenID Connect provider, and fake Slack, PagerDuty and
webhook receivers. Measured on a laptop with local Redis: proxy overhead before the upstream call
p50 75 µs, p99 92 µs (not a production load test).
