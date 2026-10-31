# Changelog

All notable changes to Tokenrein are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). One version number covers the proxy image and both
SDKs; the release workflow refuses a tag that does not match all three.

## [Unreleased]

### Added
- GitHub Copilot CLI connector (`/connect/copilot.py`): usage (model and tokens of each call)
  from Copilot's OpenTelemetry traces, read at `POST /v1/otel/v1/traces`, and command hooks at
  `POST /v1/hooks/copilot` that refuse the next tool call when a run is paused, over budget or
  looping. `--policy` sets up every user of a machine with Copilot policy hooks and managed
  telemetry settings. Spend is an estimate at API prices; GitHub bills premium requests.

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
