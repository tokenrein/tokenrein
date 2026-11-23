# Changelog

All notable changes to Tokenrein are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). One version number covers the proxy image and both
SDKs; the release workflow refuses a tag that does not match all three.

## [Unreleased]

### Added
- Insights page and `GET /admin/orgs/{org}/insights?days=`: where agents waste tokens, from the
  usage and step history, with what to fix first. Cache health compares each call with the
  previous call of its run and puts misses down to a cause (caching not turned on, the start of
  the prompt changing, the cache expiring, a model switch, calls sent in parallel), priced as an
  estimate, alongside what caching saved and the write premium paid for entries never read. Tool
  health shows each tool's failure rate, repeated identical calls, retries of a failing input,
  oversized outputs and timings. Agents with no person attached are flagged as Batch API
  candidates. Rules only; no model is asked. Needs the usage history.
- Proxy mode reads each request's shape without keeping it: hashes and sizes of the tool
  definitions, system prompt, cache-relevant settings and conversation. Insights then names what
  broke the cache (the tool list, the system prompt, a setting or an earlier message changed),
  shows tool definitions that take much of every prompt, and lists tools an agent is given but
  never calls. Request events carry it as `prompt`, and the trace shows it on each model call.
- Proxy-only runs get tool health too: tool calls answered in a proxied conversation are logged as
  steps (metadata only), unless the run's agent logs its own through hooks or an SDK.
- Fix suggestions (Team): **Suggest a fix** on an Insights finding asks a model you choose, behind
  any OpenAI-compatible endpoint such as LiteLLM (`[analysis]`), for a concrete plan. Only the
  finding and its metrics are sent, never prompts or tool text; each answer shows what was sent,
  every request is audited, and spend is capped per month (`monthly_budget_usd`, default $20).
- Weekly insights digest to Slack, Teams and webhooks (alert event `insights`; never PagerDuty).
- Agent steps are kept in the usage history folder (`<dir>/steps/`) as metadata only: names,
  outcomes, sizes, timings and input fingerprints, never content. Tool health covers the whole
  history retention instead of the 7 days Redis keeps.
- Token counts include long-lived cache writes (Anthropic 1-hour, OpenAI 12-hour) and reasoning
  tokens: read from provider responses in proxy mode, from both SDKs, from Codex and Gemini CLI
  telemetry, and accepted by `POST /v1/usage` as `cache_write_long_tokens` and
  `reasoning_tokens`. Stored in the usage history and shown in the trace.
- Long-context prices: Claude Haiku 5.5 prompts over 100k tokens and Gemini 2.5 Pro prompts over
  200k tokens are billed at their higher rates.
- Built-in prices for GPT-5.4, GPT-5.5, GPT-5.6 Sol, GPT-6 Luna, GPT-6.1 Terra, GPT-6.2 Sol and
  Astra, and Claude Fable 5.2 (checked against the providers' pricing pages on 2026-11-23).
- Spend page: pick the window (last 15 minutes to 12 months, or a custom range), group it by
  minute, hour or day, and filter by project, agent, user, model and status. Breakdowns follow
  the filters, and clicking a row filters by it; the CSV download uses the same window, grouping
  and filters. Minutes, hours and filters need the usage history; without it the page stays on
  daily totals and says how to turn it on.
- `GET /admin/orgs/{org}/spend` takes `from`, `to`, `bucket` and filter parameters (see
  docs/api.md). Range reports include requests from the last few seconds that are not yet written
  to the history files.

### Changed
- Keys & limits is split into tabs (Projects, Org limits, Provider keys, Alerts, License, Orgs)
  instead of one long page. Projects are a compact list (spend, keys, guardrail pack, agent logs);
  one project opens at a time, with its keys, pack, agent logs and limits. Loop guard settings fold
  away until set. The page went from about 4,700 to 1,200 pixels tall.
- Long run ids are shortened in tables (full id on hover), and Activity links each run.
- Activity shows each call's total tokens and how much came from the cache.
- Tool calls whose result never arrived show "no result" after 10 minutes instead of "running".

### Fixed
- Spend breakdown tables no longer cut off the request count on narrow cards.
- The License tab lists fix suggestions with the other paid features.
- Anthropic 1-hour cache writes were priced at the 5-minute rate (1.25x input instead of 2x).
- OpenAI cache writes (GPT-5.6 and later) were ignored and billed as plain input, in the proxy,
  both SDKs and Codex telemetry.
- GPT-5.5 and newer OpenAI models matched no price and were charged at the most expensive OpenAI
  rate.
- Claude Sonnet 5.5 cache reads were priced at $0.20 per million tokens instead of $0.10, and
  Gemini 2.5 Pro cache reads at $0.31 instead of $0.125.
- A custom price without a cached-input price billed every cache read as fresh input; it now
  defaults to a tenth of input for providers with prompt caching.
- The run page's Tokens figure left out cache reads and writes, often most of a run's tokens; it
  now counts all of them and shows how much of the input came from the cache. Log details show
  cache writes too.
- Proxy request events carry `source: "proxy"`.

## [0.2.0] - 2026-11-17

### Added
- Agent logs: what agents do between model calls (prompts, tool calls with outcome and duration,
  sub-agents, turn and session ends), from the connectors' hooks and from `guard.log()` /
  `guard.step()` in both SDKs (`POST /v1/steps`). Kept 7 days with the run detail, never charged
  to a budget, and not written to the usage history.
- Content capture per project (`capture_content`, Keys & limits): agent logs keep a redacted copy
  of prompt and tool text, cut to 2,000 characters. Off by default: only kind, tool name, size,
  outcome and timing are kept.
- Dashboard: a Logs page (search and filter steps and model calls across runs), a trace on each run
  page (model and tool calls on one timeline, with durations, errors and repeated tool calls), and
  a last-hour activity card on the overview (calls, steps, errors, spend, model latency).
- `GET /admin/orgs/{org}/logs`; the run timeline also returns `steps`, and runs carry `steps` and
  `errors` counts.
- Tool calls logged through `POST /v1/steps` with the same `name` and `content` are flagged as
  repeated in the run's trace (a hash is compared, so this works with content capture off).
- The README demo shows the run trace and the Logs page, and has screenshots of the dashboard.

### Changed
- Dashboard restyle: neutral surfaces with hairline structure, ink primary buttons, colour kept
  for data and status (a dot and a word instead of filled pills), stats in one strip, sentence-case
  labels, tabular numbers, shorter page copy, and a new logo mark. Light and dark themes, still no
  external fonts or scripts.
- The connect scripts add an after-tool hook (`PostToolUse`, `AfterTool` or `postToolUse`) and, for
  Claude Code, Cursor and Copilot, the tool-failure hook (`PostToolUseFailure` /
  `postToolUseFailure`), so tool results and failures are logged. Run a connect script again to add
  them to an existing connection.
- The dashboard opens in light mode; a Dark mode button in the sidebar switches it.

### Fixed
- Integration tests could fail when two tests started Redis on the same free port.
- Copilot CLI marks its shell tool as successful even when the command fails; agent logs now read
  the exit code it reports (`completed with exit code N`), so failed commands show as errors.
  Checked live with Copilot CLI 1.0.105.
- Codex CLI's tool-result hook carries only the command's output, so failed commands showed as
  ok. The hook now reads the call's exit code and duration from the local session transcript
  (only those numbers are sent). Checked live with Codex CLI 0.169.0.
- A tool call's duration in the trace now uses the duration the agent reported with the result,
  not only the time between the two log entries (which is 0 when both arrive together).
- A refused `POST /v1/check` dropped the `provider` it was sent, so the spend breakdown listed those
  requests under a separate `/model` row. It keeps the provider now, and a model with no provider
  shows as just its name.
- A stopped run's page no longer shows it as Live.

### Security
- `cargo audit` ignores RUSTSEC-2026-0459 (SQLite): `libsqlite3-sys` is listed in `Cargo.lock` by
  sqlx but never compiled (see `.cargo/audit.toml`).

## [0.1.4] - 2026-11-01

### Added
- `demo/run.sh`: four misbehaving agents against a fake model, no API key needed. The README's demo
  video is recorded from it (`demo/record.cjs`).

### Fixed
- Retries refused while a run waited for an approver counted toward the loop guard, so approving
  a budget could pause the run again at once as a loop. Only calls that are let through count now.
- Letting a looping run continue now resets its repeat counts, as documented; the next identical
  request used to trip the guard again straight away.

## [0.1.3] - 2026-11-01

### Added
- GitHub Copilot CLI connector (`/connect/copilot.py`): usage (model and tokens of each call)
  from Copilot's OpenTelemetry traces, read at `POST /v1/otel/v1/traces`, and command hooks at
  `POST /v1/hooks/copilot` that refuse the next tool call when a run is paused, over budget or
  looping. `--policy` sets up every user of a machine with Copilot policy hooks and managed
  telemetry settings. Spend is an estimate at API prices; GitHub bills premium requests.
- `deploy/setup.sh` writes `deploy/.env` with fresh secrets, and `docs/quickstart.md` walks
  through a first install step by step.
- `TOKENREIN_PROXY_URL`: where agents reach the proxy, used in the dashboard's connect commands
  (they assumed this host on port 8787).

### Fixed
- The site's deploy commands stopped at a missing Postgres password; they now run `setup.sh`.
- The dashboard's Get started checklist no longer suggests a provider key is required; it is only
  for proxy mode.
- Passwords are no longer refused for containing a short email name (`dev@`, `sam@`).

## [0.1.2] - 2026-10-31

### Added
- A security policy (private reports through GitHub or email), contributing notes, issue forms
  and Dependabot updates.

### Fixed
- The SDK READMEs on PyPI and npm linked to `../README.md`, which does not exist there; they link
  to tokenrein.com and the SDK guide on GitHub.
- The container image's license label is `Elastic-2.0` (it read `NOASSERTION`).

## [0.1.1] - 2026-10-31

The first public release.

### Added
- Free tier and license keys: self-hosting is free up to 25 developers and $10,000 of AI spend a
  month with every guardrail (going over only reminds, never blocks agents); a Team key adds
  single sign-on, audit export, history past 90 days, PagerDuty and the Cursor import, and an
  Enterprise key adds more orgs. Keys are checked offline; see docs/licensing.md.
- Load test (`loadtest/`, k6): Tokenrein adds about 1 ms per call (p99 2.2 ms at 10,000 calls
  per second on one instance); results are in the README.

### Changed
- The repository moved to `github.com/tokenrein/tokenrein`, and the container image is now
  `ghcr.io/tokenrein/tokenrein`. 0.1.0 stays at `ghcr.io/geektuhin123/tokenrein`.
- Release images are built on a native runner per platform instead of under emulation.
- The SDKs are published with trusted publishing (no stored registry tokens) and name their
  source repository and homepage.
- The product site is at tokenrein.com.

## [0.1.0] - 2026-10-29

First release.

### Added

- **License**: the server is published under the Elastic License 2.0 (`LICENSE`); the SDKs and
  the coding-agent connector scripts stay Apache-2.0.
- **Approvals**: run budgets can be in Ask mode. A run that uses up its budget is paused and
  waits for an approver, who adds budget (the run continues) or stops it. Approvals page,
  `/admin/orgs/{org}/approvals`.
- **Loop guard**: pauses, stops or reports runs that repeat the same request or tool call, spend
  too fast or make too many calls, in the proxy, SDK checks, agent hooks and reported usage. The
  SDKs send a hash of each request (never its content) for this.
- **Alerts** to Slack, Microsoft Teams, PagerDuty and HMAC-signed webhooks: budget 80% used,
  budget used up, run waiting for approval, runaway loop.
- **Usage history**: with `TOKENREIN_HISTORY_DIR` (Compose: on by default; Helm:
  `history.enabled`), every request event is also kept as Parquet files and read with DuckDB
  2.0 (built in): Spend ranges of 90 days, 6 and 12 months, **Download CSV** for chargeback
  (per day, project, agent, user and model), and run timelines older than 7 days. Kept 400 days
  by default; finished days are merged hourly; replicas share the folder.
- `TOKENREIN_PUBLIC_URL` (Helm `publicUrl`) sets the dashboard's address for links and SSO.
- Developer, enterprise and admin API guides.

### Renamed: TokenLens is now Tokenrein

| Before | Now |
|---|---|
| `pip install tokenlens`, `import tokenlens` | `pip install tokenrein`, `import tokenrein` |
| `@tokenlens/sdk` | `@tokenrein/sdk` |
| `TOKENLENS_API_KEY`, `TOKENLENS_BASE_URL`, `TOKENLENS_MODE` | `TOKENREIN_API_KEY`, `TOKENREIN_BASE_URL`, `TOKENREIN_MODE` |
| `TL_ADMIN_TOKEN`, `TL_SECRETS_KEY`, `TL_DATABASE_URL`, `TL_CONFIG` (and `_FILE`) | `TOKENREIN_ADMIN_TOKEN`, `TOKENREIN_SECRETS_KEY`, `TOKENREIN_DATABASE_URL`, `TOKENREIN_CONFIG` |
| `x-tl-*` headers | `x-tr-*` headers |
| `tl_` keys, `tlu_` API tokens | `tr_` keys, `tru_` API tokens |
| `tokenlens_*` error types | `tokenrein_*` error types |
| `tokenlens-proxy` binary, image and Helm chart | `tokenrein` |

Until v1.1 the old environment variables (with a warning), the old request headers, and keys
and tokens made before the rename keep working. Re-run an agent connector to move its settings
to the new names. Dashboard sessions start over once (the cookie was renamed).

### Proxy

- Drop-in proxy for the OpenAI Chat Completions and Responses APIs and the Anthropic Messages
  API that reserves the worst-case cost of each call before it is sent and settles the actual
  cost afterwards, in one Redis Lua script per call.
- Budgets per run, agent per day, project per month and org per month, each in `skip`,
  `check` (shadow) or `enforce` mode; sub-agent spend counts against every ancestor run.
- Pause, resume and kill runs; per-org fail mode (`open` or `closed`) when Redis is unreachable.
- Usage reporting, budget checks, Claude Code telemetry and Claude Code hooks for agents that
  call providers directly.
- Dashboard and admin API: orgs, projects, virtual keys, budgets, guardrail packs, provider keys
  (encrypted with `TOKENREIN_SECRETS_KEY`), model prices, runs, agents, spend and activity.
- Optional Postgres for every version of the managed config and an append-only audit log.

### Deployment and operations

- Container image (`ghcr.io/geektuhin123/tokenrein`): distroless, non-root, linux/amd64 and
  linux/arm64, with a default config so no file needs to be mounted.
- Docker Compose stack with Redis and Postgres 17 (`deploy/docker-compose.yml`), and a Helm
  chart (`deploy/helm/tokenrein`) with probes, autoscaling, a disruption budget, and either
  bundled or external Redis and Postgres.
- Settings from the environment: `TOKENREIN_LISTEN`, `TOKENREIN_ADMIN_LISTEN`, `TOKENREIN_REDIS_URL`,
  `TOKENREIN_DATABASE_MAX_CONNECTIONS`. Any variable can be read from a file named by `<NAME>_FILE`
  (Docker and Kubernetes secrets).
- `GET /metrics` on the admin listener (Prometheus): decisions, pre-upstream overhead, upstream
  latency, ledger errors, dropped activity events, audit write failures, config version.
- Security headers (`nosniff`, `no-referrer`, `frame-ancestors 'none'`) on every admin response.
- `tokenrein rotate-secrets-key` and `TOKENREIN_SECRETS_KEY_PREVIOUS` for rotating the key that
  encrypts saved provider keys without downtime.
- `tokenrein probe URL...` for container healthchecks.
- Redis calls time out after `[redis] timeout_ms` (default 1000) and are not retried inside a
  call, so a Redis outage applies the org's fail mode at once instead of holding requests for
  minutes.
- CI: SDK test suites, `cargo audit`, `npm audit`, `pip-audit` and a CycloneDX SBOM. Release
  workflow for tags `v*`.

### SDKs

- Python (`tokenrein`) and TypeScript (`@tokenrein/sdk`) SDKs that wrap an OpenAI or Anthropic
  client in one line, through the proxy or in direct mode.

### Removed

- The legacy root `docker-compose.yml` and `.env.example`; `deploy/` replaces them.
