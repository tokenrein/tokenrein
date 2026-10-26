# Admin API

The admin API serves the dashboard and automation on the admin listener (`:8788`). The
proxy-free endpoints agents use (`/v1/check`, `/v1/usage`, telemetry and hooks) are on the proxy
listener and are described in the [developer guide](developers.md#proxy-free-endpoints).

Authenticate with `Authorization: Bearer tru_…` (a personal API token), a dashboard session, or
`Authorization: Bearer $TOKENREIN_ADMIN_TOKEN`. Each endpoint needs a role in the org it names (viewer
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
| `GET` | `/admin/orgs/{org}/approvals` | Runs waiting for approval and recent decisions (`pending` count) — viewer |
| `POST` | `/admin/orgs/{org}/approvals/{run}/approve` | Let the run continue; `{add_usd?, note?}` adds budget to the run budget that ran out — approver |
| `POST` | `/admin/orgs/{org}/approvals/{run}/deny` | Stop the run for good `{note?}` — approver |
| `GET` / `POST` | `/admin/orgs/{org}/alerts` | Alert channels (targets never returned, last delivery per channel) / add `{kind: slack\|teams\|webhook\|pagerduty, name, target, events?}`; a webhook's signing secret is returned once — admin |
| `DELETE` | `/admin/orgs/{org}/alerts/{channel}` | Remove a channel — admin |
| `POST` | `/admin/orgs/{org}/alerts/{channel}/test` | Send a test alert now — admin |
| `GET` | `/healthz`, `/readyz` | Liveness; Redis (and database, if configured) reachability (no token) |
| `GET` | `/metrics` | Prometheus metrics (no token; counts only, no org or user ids) |

Budgets use the config file's shape, e.g.
`{"budgets": {"agent_daily": {"limit_usd": 20, "mode": "enforce"}, "run": {"limit_usd": 5, "mode": "ask"}, "max_run_usd": 10, "loops": {"action": "pause", "max_repeats": 6}}}`.
Edits are validated like the config file and are compare-and-set, so concurrent edits never
overwrite each other (the loser gets `409`).

Logs go to stderr. Stdout carries one JSON event per request (decision, estimate,
actual cost, usage, `pre_upstream_us` overhead) for the analytics pipeline. The same events
feed the dashboard: a background task writes them to Redis (a capped stream of the last 5,000
per org, the last 500 per run, run / sub-run / agent indexes, and daily spend totals by model,
user, project and agent), off the request path; if Redis is slow they are dropped, never the
request. Spend reports come from these events; budgets are always enforced from the ledger.
