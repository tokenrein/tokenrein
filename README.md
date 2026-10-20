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

```bash
OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-... TL_ADMIN_TOKEN=change-me-16chars-min cargo run -p tokenlens-proxy --release
```

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

## Admin API (`:8788`, `Authorization: Bearer $TL_ADMIN_TOKEN`)

| Method | Path | Effect |
|---|---|---|
| `GET` | `/admin/orgs/{org}/runs/{run}` | State, parent, limit / spent / reserved |
| `POST` | `/admin/orgs/{org}/runs/{run}/pause` | Block the run and its children |
| `POST` | `/admin/orgs/{org}/runs/{run}/resume` | Unblock (not allowed after kill) |
| `POST` | `/admin/orgs/{org}/runs/{run}/kill` | Block permanently |
| `GET` | `/healthz`, `/readyz` | Liveness; Redis reachability |

Logs go to stderr. Stdout carries one JSON event per request (decision, estimate,
actual cost, usage, `pre_upstream_us` overhead) for the analytics pipeline.

## Tests

```bash
cargo test
```

37 tests: unit tests plus integration tests that start a real `redis-server` and a
fake OpenAI/Anthropic upstream. They include a concurrency test (500 parallel
reservations against a budget that fits exactly 100 — exactly 100 succeed),
streaming settlement for both providers, child-run spend counting against the
parent, shadow mode, and pause/resume/kill. Integration tests skip if
`redis-server` is missing unless `TL_REQUIRE_REDIS=1` (set in CI).

Local smoke run of the release binary (Apple Silicon laptop, Redis on the same
machine, sequential requests to a stub upstream): proxy overhead before the
upstream call p50 75 µs, p99 92 µs over 1,000 requests; recorded spend matched
expected cost exactly. This is not a production load test.

## Not built yet (rest of Phase 1)

- Loop detectors (identical-call repeats, spend velocity, run structure, retry storms)
- Slack / email alerts with Resume / Raise / Kill
- Control plane: signup, orgs, keys and policies in Postgres (config file today),
  KMS-encrypted provider keys (environment variables today), dashboard, Stripe
- Event pipeline to ClickHouse (stdout today), replay simulation, shadow-mode report
- Python / TypeScript SDKs
- Responses API: context size of a `previous_response_id` the proxy never saw is
  assumed to be 32k tokens
