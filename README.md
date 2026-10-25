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
| `Dockerfile`, `deploy/` | Container image, Docker Compose stack (TokenLens + Redis + Postgres 17), Helm chart |
| `docs/install.md`, `docs/operations.md` | Installing, upgrading and backing up; runbooks, metrics and alerts |
| `.github/workflows/ci.yml` | fmt, clippy (warnings as errors), tests against real Redis and Postgres, SDK tests, dependency audits, SBOM |
| `.github/workflows/release.yml` | On a `v*` tag: multi-arch image to GHCR, SDKs to PyPI and npm |
| `gateway/` | Legacy scaffold (Stripe + landing page); moves into the control plane later |

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

## Deploy it

For a team install, run the container image with Redis and Postgres: Docker Compose
(`deploy/docker-compose.yml`) or Kubernetes (`deploy/helm/tokenlens`). See
[docs/install.md](docs/install.md) and, for running it, [docs/operations.md](docs/operations.md).

## Run it locally

Prereqs: Rust (stable), `redis-server`. Postgres is optional for trying it out and recommended
for real use (see [Config history and audit log](#config-history-and-audit-log)).

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

## Without the proxy

TokenLens doesn't have to sit in the request path. Agents and SDKs can call providers directly
and report usage to TokenLens, which charges the same budgets and feeds the same dashboard. All
of these use a TokenLens key (`Authorization: Bearer tl_…`) on the `:8787` listener:

| Endpoint | Used by | What it does |
|---|---|---|
| `POST /v1/usage` | SDK direct mode, any client | Report calls after they happen: `{"events": [{run_id, parent_run_id, agent, user, provider, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd?, duration_ms}]}`. Priced from the model list unless `cost_usd` is given. Returns the enforced limits now used up |
| `POST /v1/check` | SDK direct mode, agent hooks | "May this run make another call?" (`run_id`, optional `estimate_usd` or `model` + `max_output_tokens`). Answers `allowed` plus a reason; paused or killed runs and used-up enforced budgets are refused |
| `POST /v1/otel/v1/logs` | Claude Code, Codex CLI, Gemini CLI telemetry | OpenTelemetry logs over HTTP/JSON. Each model call becomes a call on the session's run (`cc-…`, `codex-…`, `gemini-…`; sub-agents on `…-sub`), with model, tokens, cost (Claude Code's own; priced by TokenLens for the others) and the user's email |
| `POST /v1/otel/v1/traces`, `/v1/otel/v1/metrics` | Gemini CLI telemetry | Accepted and dropped (Gemini CLI sends them next to its logs) |
| `POST /v1/hooks/{claude-code,codex,gemini,cursor}` | The agents' hooks | Blocks the next prompt or tool call (and, where the agent allows it, sub-agent or model call) when the session's run is paused, killed or over an enforced budget, answering in each agent's own format; fails open if the ledger is down |

Enforcement here happens between calls: the call that crosses a limit has already been made,
and the next one is refused. Use the proxy when you need every call checked before it is sent.

**Claude Code** needs no code: add to `~/.claude/settings.json` (replace `tl_…` with a key from
the dashboard):

```json
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "http/json",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://127.0.0.1:8787/v1/otel/v1/logs",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS": "Authorization=Bearer tl_…,x-tl-agent=claude-code",
    "TOKENLENS_API_KEY": "tl_…"
  },
  "hooks": {
    "UserPromptSubmit": [{"hooks": [{"type": "http", "url": "http://127.0.0.1:8787/v1/hooks/claude-code",
      "headers": {"Authorization": "Bearer $TOKENLENS_API_KEY"}, "allowedEnvVars": ["TOKENLENS_API_KEY"], "timeout": 5}]}],
    "PreToolUse": [{"matcher": "*", "hooks": [{"type": "http", "url": "http://127.0.0.1:8787/v1/hooks/claude-code",
      "headers": {"Authorization": "Bearer $TOKENLENS_API_KEY"}, "allowedEnvVars": ["TOKENLENS_API_KEY"], "timeout": 5}]}]
  }
}
```

New sessions then report every model call; this works with a Claude subscription login too
(cost is Claude Code's own estimate).

**OpenAI Codex CLI, Gemini CLI and Cursor** connect the same way, with one command from the
dashboard's Connect page (pick a project, create a key):

```bash
curl -fsSL http://127.0.0.1:8788/connect/codex.py | python3 - --key tl_… --url http://127.0.0.1:8787
curl -fsSL http://127.0.0.1:8788/connect/gemini.py | python3 - --key tl_… --url http://127.0.0.1:8787
curl -fsSL http://127.0.0.1:8788/connect/cursor.py | python3 - --key tl_… --url http://127.0.0.1:8787
```

Each script backs up the files it changes and is undone with `--remove`; the Codex and Gemini
CLI scripts refuse to replace another OpenTelemetry collector. Codex and Gemini CLI turn on their own OpenTelemetry export
(`~/.codex/config.toml`, `~/.gemini/settings.json`) and get command hooks; Codex runs new hooks
only after you trust them with `/hooks`. Their hooks run `~/.tokenlens/hook.py`, which forwards
the hook event to TokenLens and lets everything through if TokenLens can't be reached. Cursor
gets hooks only (`~/.cursor/hooks.json`): it has no local telemetry, so a team admin can import
its usage (model, tokens, cost per call) hourly from Cursor's Admin API by saving an Admin API
key on the Connect page (stored encrypted, like provider keys). Runs are named `codex-<session>`,
`gemini-<session>` and `cursor-<conversation>`. The fields each connector reads, with links to
the agents' docs, are in [docs/connectors.md](docs/connectors.md).

**Model prices** cover OpenAI, Anthropic, Google Gemini, Mistral, DeepSeek, xAI, Groq and
Cohere models; local providers (`ollama`, `vllm`, `lmstudio`, `llamacpp`, `local`) cost $0.
Reseller names are understood (`us.anthropic.claude-…`, `anthropic/claude-…`,
`models/gemini-…`). Unknown models are charged at the provider's most expensive price until
you add an override.

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

Setup pages:

- **Connect**: pick what to guard. *Claude Code*: choose a project, click **Create key & show
  command**, and run the one-line command it gives on the machine with Claude Code (it edits
  `~/.claude/settings.json` with a backup; `--remove` undoes it). *Your app*: the SDK snippet
  (direct mode). *Any tool with a base URL*: the proxy address for strict mode.
- **Keys & limits**: provider keys, projects, TokenLens keys, budgets and packs.
- **Models & prices**: every model TokenLens can price (search, filter by provider), models
  used recently that have no price, and your own prices or overrides (US dollars per million
  tokens; stored with the managed config, applied within seconds).

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

## Config history and audit log

Set `TL_DATABASE_URL` to a Postgres database (`postgres://user:password@host/db`; the env var
name and pool size can be changed under `[database]` in the config) and TokenLens keeps:

- **Every version of the managed config** (orgs, projects, keys, budgets, prices), with who saved
  it. The schema is created and upgraded automatically at start-up. When you add a database to
  an install that ran without one, it takes over the config already in Redis.
- **An audit log** of every change made in the dashboard or the admin API: who (and from which
  address), when, the action, and each field that changed with its old and new value. Provider
  keys never appear in it (only their last 4 characters). Pausing, resuming and killing runs and
  testing provider keys are recorded too. Entries are append-only and are also written to the
  log (`target: "audit"`) for shipping to a SIEM.

The dashboard's **Audit log** page lists, filters and downloads it as CSV. Without a database the
proxy still works: the current config lives in Redis and the audit log keeps the last 10,000
entries there. Redis stays the hot path either way; Postgres is never called per request.

## People, roles and sign-in

With a database, people sign in to the dashboard with their own account: email and password,
single sign-on (OpenID Connect: Okta, Microsoft Entra ID, Google Workspace, Auth0, Keycloak, …),
or both.

1. Create the first account on the server; it prints a one-time link to set the password:

   ```bash
   cargo run -p tokenlens-proxy --release -- create-owner you@company.com
   ```

2. Add people under **People & access**, per org, with a role:

   | Role | Can |
   |---|---|
   | viewer | see runs, agents, spend, activity and settings |
   | approver | also pause, resume and kill runs |
   | admin | also change projects, keys, limits, provider keys and members (up to admin), see the org's audit log |
   | owner | also make owners and delete the org |

   **Instance admins** manage every org plus model prices, all accounts and single sign-on.
   People who use single sign-on only need their email added; others get a one-time link (72
   hours) to set a password. Orgs a person has no role in don't show up for them at all.

3. For single sign-on, set `auth.public_url` in the config to the dashboard's address, register
   TokenLens with your identity provider using the redirect URI shown under People & access
   (`<public_url>/auth/sso/callback`), and fill in the issuer, client ID and secret there. Options:
   allowed email domains, create accounts on first sign-in (with a default org and role), and
   **roles from groups** (`group = org:role`, synced at every sign-in). The client secret is
   encrypted with `TL_SECRETS_KEY` (or read from an env var you name). Set
   `auth.password_login = false` to require single sign-on.

Security details: passwords are hashed with argon2id (at least 12 characters, not containing the
email); 10 wrong passwords lock an account for 15 minutes, and sign-in attempts are rate-limited
per address. Sessions are HttpOnly, SameSite=Lax cookies (Secure when `public_url` is https)
stored as hashes in Postgres; they end after `auth.session_hours` (12) or `auth.idle_minutes`
(120) without use, and every change needs the session's `x-csrf-token` header. Single sign-on
uses the authorization code flow with PKCE, checks the ID token's signature, issuer, audience,
expiry and nonce, and refuses unverified emails. Personal **API tokens** (`tlu_…`, 1-365 days)
act as their owner for scripts. Sign-ins, failed sign-ins, role changes, tokens and single
sign-on changes are all in the audit log.

The admin token (`TL_ADMIN_TOKEN`) still works as an instance admin; keep it as a break-glass
key or unset it once people can sign in. Without a database it is the only way in.

## Admin API (`:8788`)

Authenticate with `Authorization: Bearer tlu_…` (a personal API token), a dashboard session, or
`Authorization: Bearer $TL_ADMIN_TOKEN`. Each endpoint needs a role in the org it names (viewer
to read, approver for run actions, admin to change things); `/admin/orgs` (create), `/admin/pricing`,
`/admin/users` and `/admin/sso` need an instance admin.

| Method | Path | Effect |
|---|---|---|
| `GET` | `/` | The dashboard page (no data; it calls the API below) |
| `GET` | `/admin/overview` | Orgs, projects, keys (prefix only), budgets, this month's spend |
| `GET` | `/auth/config` | How people can sign in here (no auth needed) |
| `POST` | `/auth/login`, `/auth/setup`, `/auth/logout` | Sign in `{email, password}`; set a password from a setup link `{token, password, name?}`; sign out |
| `GET` / `PUT` | `/auth/me`, `/auth/password` | Who you are and your roles; change your password `{current, new}` |
| `GET` / `POST` / `DELETE` | `/auth/tokens`, `/auth/tokens/{id}` | Your API tokens; create `{name, expires_days?}` (shown once); revoke |
| `GET` | `/auth/sso/start`, `/auth/sso/callback` | Single sign-on |
| `GET` / `POST` | `/admin/orgs/{org}/members` | An org's members; add `{email, role}` (returns a setup link for people without a password) |
| `PUT` / `DELETE` | `/admin/orgs/{org}/members/{user}` | Change `{role}` / remove (an org with owners keeps at least one) |
| `POST` | `/admin/orgs/{org}/members/{user}/setup-link` | A new one-time password link |
| `GET` / `POST` / `PUT` | `/admin/users`, `/admin/users/{user}` | Every account; add an instance admin `{email}`; set `{instance_admin?, disabled?}` |
| `GET` / `PUT` / `POST` | `/admin/sso`, `/admin/sso/test` | Single sign-on settings (secret never returned); check the issuer |
| `GET` | `/admin/audit?org=&limit=&before=&format=csv` | Audit log, newest first; `next` is the cursor for `before`; `format=csv` downloads up to 10,000 entries |
| `POST` | `/admin/orgs` | Create an org `{id, fail_mode?, budgets?}` |
| `PUT` / `DELETE` | `/admin/orgs/{org}` | Set `{fail_mode?, budgets?, anthropic_workspace_id?}` (`""` clears it) / delete the org and its keys |
| `PUT` / `DELETE` | `/admin/orgs/{org}/provider-keys/{openai\|anthropic}` | Save `{api_key}` (encrypted) / remove it (falls back to the environment) |
| `POST` | `/admin/orgs/{org}/provider-keys/{provider}/test` | Check the key with the provider's (free) model list |
| `GET` | `/admin/packs` | Guardrail packs and their budgets |
| `GET` | `/admin/models?org=` | Priced models (built-in and yours), models seen recently, unpriced ones |
| `PUT` | `/admin/pricing` | Replace your price overrides `{overrides: [{provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok?, cache_write_per_mtok?}]}` |
| `GET` | `/connect/{claude-code,codex,gemini,cursor}.py` | The coding-agent connector scripts (no secrets; run with `--key`, undo with `--remove`) |
| `GET` / `PUT` / `DELETE` | `/admin/orgs/{org}/connectors/cursor` | Cursor usage import: status (last run, calls imported, error) / save `{project, api_key?}` (encrypted; omit the key to keep it) / remove |
| `POST` | `/admin/orgs/{org}/connectors/cursor/sync` | Import Cursor usage now (it also runs hourly) |
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
| `GET` | `/healthz`, `/readyz` | Liveness; Redis (and database, if configured) reachability (no token) |
| `GET` | `/metrics` | Prometheus metrics (no token; counts only, no org or user ids) |

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

85+ tests: unit tests plus integration tests that start a real `redis-server` (and, for config
history, the audit log, accounts and single sign-on, a throwaway Postgres cluster and a mock
OpenID Connect provider) and a fake OpenAI/Anthropic upstream. They include a concurrency test (500 parallel
reservations against a budget that fits exactly 100 — exactly 100 succeed),
streaming settlement for both providers, child-run spend counting against the
parent, shadow mode, pause/resume/kill, and the dashboard API (keys and limits created
there apply immediately, other instances pick them up, runs / agents / events are
recorded, provider keys saved there are encrypted, used and never returned, guardrail packs, the
Check-mode review, run timelines and spend by model and user, and the proxy-free endpoints:
reported usage, checks, Claude Code, Codex CLI and Gemini CLI telemetry, all four agents'
hooks, the Cursor usage import against a fake Admin API, the connect scripts run with `python3`
against a throwaway home directory, the audit log with and without Postgres, roles, sessions,
CSRF, lockout, API tokens and single sign-on with group roles), plus operations: Prometheus
metrics, admin security headers, secrets key rotation, and what keeps working while Redis or
Postgres is down. Integration tests skip if `redis-server` or Postgres (`initdb`) is missing unless
`TL_REQUIRE_REDIS=1` / `TL_REQUIRE_POSTGRES=1` (both set in CI).

Local smoke run of the release binary (Apple Silicon laptop, Redis on the same
machine, sequential requests to a stub upstream): proxy overhead before the
upstream call p50 75 µs, p99 92 µs over 1,000 requests; recorded spend matched
expected cost exactly. This is not a production load test.

## Not built yet (rest of Phase 1)

- Loop detectors (identical-call repeats, spend velocity, run structure, retry storms)
- Slack / email alerts with Resume / Raise / Kill
- KMS instead of `TL_SECRETS_KEY` for provider keys; SCIM provisioning; Stripe
- Event pipeline to ClickHouse (stdout today), replay simulation, shadow-mode report
- Responses API: context size of a `previous_response_id` the proxy never saw is
  assumed to be 32k tokens
