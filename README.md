# TokenLens — runtime spend guardrails for AI agents

A drop-in proxy for OpenAI and Anthropic APIs that enforces per-run budgets
**before** each paid call, so a looping agent is stopped instead of discovered
on the invoice. Product plan, research and go-to-market: [docs/PLAN.md](docs/PLAN.md).

## Repository layout

| Path | What it is |
|---|---|
| `proxy/` | Rust data plane: the proxy, Redis budget ledger, admin API |
| `config/dev.toml` | Local development config |
| `docs/PLAN.md` | Implementation plan, pricing, marketing calendar |
| `.github/workflows/ci.yml` | fmt, clippy (warnings as errors), tests against real Redis |
| `gateway/`, `docker-compose.yml` | Legacy Langfuse-based scaffold; `gateway/` (Stripe + landing page) moves into the control plane later |

## How it works

```
agent ──▶ proxy (:8787) ──reserve worst-case cost──▶ Redis (one Lua script)
            │                                          run → ancestors → agent/day → project/month → org/month
            ├─ forward to OpenAI / Anthropic with the customer's real key
            └─ settle actual cost from the usage the provider reports ──▶ Redis
```

1. The client authenticates with a TokenLens virtual key (never the real provider key).
2. The proxy estimates the worst-case cost (pessimistic input estimate + output cap; it
   injects an output cap if the request has none) and reserves it atomically on every
   budget scope, or on none.
3. Over an **enforce** budget → `402` with a provider-shaped error body (SDKs don't
   auto-retry 402). Over a **check** budget → allowed with an `x-tl-warning` header
   (shadow mode). Paused or killed run (or ancestor) → `403`.
4. After the response (or the end of the stream), the reservation is replaced by the
   actual cost. A sweeper releases reservations whose request died.

## Run it locally

Prereqs: Rust (stable), `redis-server`.

```bash
redis-server --daemonize yes
```

Once, create the two proxy secrets and keep them (e.g. in `.env`, which is gitignored):

```bash
printf 'TL_ADMIN_TOKEN=%s\nTL_SECRETS_KEY=%s\n' "$(openssl rand -hex 32)" "$(openssl rand -hex 32)" > .env
```

Then start the proxy with them:

```bash
set -a; source .env; set +a; cargo run -p tokenlens-proxy --release
```

Open the dashboard at http://127.0.0.1:8788/, sign in with `TL_ADMIN_TOKEN`, and save your
OpenAI / Anthropic keys under Keys & limits (or put `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` in
`.env` instead). Use the same `TL_SECRETS_KEY` on every restart: keys saved in the dashboard can
only be decrypted with it.

Point any OpenAI or Anthropic SDK at the proxy:

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="tl_dev_support_bot_key_change_me",
                default_headers={"x-tl-agent": "support-bot", "x-tl-run-id": "ticket-4812"})
```

Create a real customer key (store only the hash in config):

```bash
cargo run -p tokenlens-proxy -- gen-key
```

## Request headers

| Header | Meaning |
|---|---|
| `Authorization: Bearer tl_…` or `x-api-key: tl_…` | TokenLens virtual key |
| `x-tl-run-id` | Run (task) id; defaults to one run per request |
| `x-tl-parent-run-id` | Parent run for sub-agents — child spend counts against every ancestor's budget. Send it on every call of the child run |
| `x-tl-agent` | Agent name for the per-agent daily budget (default `default`) |
| `x-tl-user` | Optional: the person or service the agent works for, for spend reports by user (not sent upstream) |
| `x-tl-run-budget-usd` | Budget for this run, fixed on first use and capped by `max_run_usd`; enforced unless the org's run policy says otherwise |

## Budgets and modes

Set in config at org level and overridden per project: `org_monthly`,
`project_monthly`, `agent_daily`, `run`, `max_run_usd`. Each budget has a mode:
`skip`, `check` (default — shadow mode: allow and warn) or `enforce`. Each org has
a `fail_mode`: `open` (default — keep serving if Redis is unreachable, flagged
`x-tl-degraded`) or `closed` (`503`).

Unknown models are reserved and charged at the provider's most expensive
built-in price until you add a `[[pricing]]` override. Built-in prices were last
checked in 2025-11 — **verify them before charging customers.**

## Dashboard

Open **http://127.0.0.1:8788/** (the admin listener) and sign in with `TL_ADMIN_TOKEN`.

New installs get a **Get started** checklist on the Overview, which stays until every step is
done (or you hide it):

1. Connect a provider (save the OpenAI / Anthropic key; a **Test** button checks it for free).
2. Create a project key.
3. Connect your agent: copy-paste snippets (Python, TypeScript, any SDK, curl); the step turns
   green as soon as the first request arrives, naming the agent and model.
4. Pick a **guardrail pack** — Coding agents, Production agents, or Batch and background jobs. A
   pack sets a project's limits in **Check** mode: nothing is blocked yet.
5. Review and enforce: the Overview lists what Enforce **would have blocked** (requests, their
   cost, the runs) for every limit still in Check mode, with a **Switch to Enforce** button each.

Views:

- **Overview**: this month's org and project spend against their limits, spend today, live agents,
  active runs, blocked requests, and the Check-mode review.
- **Runs**: every run of the last 7 days, sub-agent runs nested under their parent, with state,
  project / agent / model, request and blocked counts, spend against the run limit, and
  Pause / Resume / Kill buttons. Click a run for its page: spend including sub-agents, budget,
  sub-agent tree, and a timeline of every request (agent, user, model, decision, tokens, cost,
  latency, "would block" warnings).
- **Agents**: each agent's status — **Live** (a request in the last minute), **Idle** (last 15
  minutes) or **Offline** — and its spend today against the agent daily budget.
- **Spend**: cost per day for the last 7 or 30 days (chart or table) and breakdowns by model,
  user, project and agent.
- **Activity**: the latest requests (decision, run, model, reserved and actual cost, tokens, latency).
- **Keys & limits**: save each org's real OpenAI / Anthropic API keys, create projects and orgs,
  create virtual keys (shown once; only the hash is stored) and revoke them, set every budget
  (limit + enforce / check / skip) and the org's fail mode.

Orgs, projects, keys and budgets are kept in Redis (`tl:control:*`). On first start they are
seeded from the config file's `[[orgs]]`; after that Redis is the source of truth (edit them in
the dashboard; later changes to `[[orgs]]` in the file are not re-read). Every proxy instance
picks up changes within 2 seconds, no restart needed. Monthly and daily limits apply to the next
request; a run's limit is fixed when the run starts, so a new run policy applies to new runs.
### Provider API keys

The real OpenAI / Anthropic keys can come from two places; a key saved in the dashboard wins:

1. **Dashboard** (Keys & limits → Provider API keys), per org. Needs `TL_SECRETS_KEY` on the
   proxy: 64 hex characters (`openssl rand -hex 32`). Keys are encrypted with it (AES-256-GCM,
   bound to the org and provider) before they are stored in Redis, are never returned by the API
   (only the last 4 characters), and every instance needs the same `TL_SECRETS_KEY` to use them.
   If it is missing or changed, the proxy falls back to the environment and the dashboard says why.
2. **Environment variables** named by `api_key_env` in the config (or an org's `provider_key_env`).

The proxy starts without any provider key, so you can add them from the dashboard; calls to a
provider with no key get `400 tokenlens_provider_not_configured` until one is set.

**Anthropic workspace ID.** Personal or service-account Anthropic keys that span several
workspaces need an `anthropic-workspace-id` header on every call. Set the org's workspace ID
(`wrkspc_…`, from the Claude Console under Settings → Workspaces) next to the Anthropic key in
the dashboard, or as `anthropic_workspace_id` on the org in the config; the proxy adds it to the
org's Claude calls. A client that sends its own `anthropic-workspace-id` header keeps it.

## Admin API (`:8788`, `Authorization: Bearer $TL_ADMIN_TOKEN`)

| Method | Path | Effect |
|---|---|---|
| `GET` | `/` | The dashboard page (no data; it calls the API below) |
| `GET` | `/admin/overview` | Orgs, projects, keys (prefix only), budgets, this month's spend |
| `POST` | `/admin/orgs` | Create an org `{id, fail_mode?, budgets?}` |
| `PUT` / `DELETE` | `/admin/orgs/{org}` | Set `{fail_mode?, budgets?, anthropic_workspace_id?}` (`""` clears it) / delete the org and its keys |
| `PUT` / `DELETE` | `/admin/orgs/{org}/provider-keys/{openai\|anthropic}` | Save `{api_key}` (encrypted) / remove it (falls back to the environment) |
| `POST` | `/admin/orgs/{org}/provider-keys/{provider}/test` | Check the key with the provider's (free) model list |
| `GET` | `/admin/packs` | Guardrail packs and their budgets |
| `POST` | `/admin/orgs/{org}/projects/{project}/pack` | Apply `{pack}`: replaces the project's budgets, all in Check mode |
| `GET` | `/admin/orgs/{org}/review` | What Enforce would have blocked: per limit, requests / cost / runs, from recent events |
| `GET` | `/admin/orgs/{org}/spend?days=` | Cost and requests per day, and by model / user / project / agent (up to 40 days) |
| `GET` | `/admin/orgs/{org}/runs/{run}/timeline` | A run, its sub-agent runs, and every request in them |
| `POST` | `/admin/orgs/{org}/projects` | Create a project `{id, budgets?}` |
| `PUT` / `DELETE` | `/admin/orgs/{org}/projects/{project}` | Set `{budgets}` (blank = inherit from org) / delete |
| `POST` | `/admin/orgs/{org}/projects/{project}/keys` | Create a virtual key `{name}`; the response has the key, once |
| `DELETE` | `/admin/orgs/{org}/projects/{project}/keys/{key_id}` | Revoke a key |
| `GET` | `/admin/orgs/{org}/runs?limit=` | Recent runs with parent, state, project, agent, counts, spend |
| `GET` | `/admin/orgs/{org}/agents?limit=` | Agents seen recently with today's spend |
| `GET` | `/admin/orgs/{org}/events?limit=` | Latest request events |
| `GET` | `/admin/orgs/{org}/runs/{run}` | State, parent, limit / spent / reserved |
| `POST` | `/admin/orgs/{org}/runs/{run}/pause` | Block the run and its children |
| `POST` | `/admin/orgs/{org}/runs/{run}/resume` | Unblock (not allowed after kill) |
| `POST` | `/admin/orgs/{org}/runs/{run}/kill` | Block permanently |
| `GET` | `/healthz`, `/readyz` | Liveness; Redis reachability |

Budgets use the config file's shape, e.g.
`{"budgets": {"agent_daily": {"limit_usd": 20, "mode": "enforce"}, "max_run_usd": 10}}`.
Edits are validated like the config file and are compare-and-set, so concurrent edits never
overwrite each other (the loser gets `409`).

Logs go to stderr. Stdout carries one JSON event per request (decision, estimate,
actual cost, usage, `pre_upstream_us` overhead) for the analytics pipeline. The same events
feed the dashboard: a background task writes them to Redis (a capped stream of the last 5,000
per org, the last 500 per run, run / sub-run / agent indexes, and daily spend totals by model,
user, project and agent), off the request path; if Redis is slow they are dropped, never the
request. Spend reports come from these events; budgets are always enforced from the ledger.

## Tests

```bash
cargo test
```

58 tests: unit tests plus integration tests that start a real `redis-server` and a
fake OpenAI/Anthropic upstream. They include a concurrency test (500 parallel
reservations against a budget that fits exactly 100 — exactly 100 succeed),
streaming settlement for both providers, child-run spend counting against the
parent, shadow mode, pause/resume/kill, and the dashboard API (keys and limits created
there apply immediately, other instances pick them up, runs / agents / events are
recorded, provider keys saved there are encrypted, used and never returned, guardrail packs, the
Check-mode review, run timelines and spend by model and user). Integration tests skip if
`redis-server` is missing unless `TL_REQUIRE_REDIS=1` (set in CI).

Local smoke run of the release binary (Apple Silicon laptop, Redis on the same
machine, sequential requests to a stub upstream): proxy overhead before the
upstream call p50 75 µs, p99 92 µs over 1,000 requests; recorded spend matched
expected cost exactly. This is not a production load test.

## Not built yet (rest of Phase 1)

- Loop detectors (identical-call repeats, spend velocity, run structure, retry storms)
- Slack / email alerts with Resume / Raise / Kill
- Control plane: signup and multi-user login (the dashboard has one admin token today),
  per-org access, KMS instead of `TL_SECRETS_KEY` for provider keys, Stripe
- Event pipeline to ClickHouse (stdout today), replay simulation, shadow-mode report
- Responses API: context size of a `previous_response_id` the proxy never saw is
  assumed to be 32k tokens
