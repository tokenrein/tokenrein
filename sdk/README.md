# TokenLens SDKs

Thin clients for the TokenLens proxy (`proxy/`). They point your existing OpenAI or
Anthropic client at the proxy with your TokenLens virtual key, attach the run headers from
the [README's "Request headers" table](../README.md#request-headers) to every call, and turn
the proxy's 402/403 answers into typed errors. Apache-2.0.

| Package | Path | Wraps |
|---|---|---|
| `tokenlens` (Python ≥ 3.9) | [`python/`](python) | `openai` ≥ 1.0, `anthropic` ≥ 0.30 (sync and async clients) |
| `@tokenlens/sdk` (Node ≥ 18) | [`typescript/`](typescript) | `openai` ≥ 4.0, `@anthropic-ai/sdk` ≥ 0.30 |

Configuration: `TOKENLENS_API_KEY` (virtual key, `tl_…`) and `TOKENLENS_BASE_URL` (default
`http://127.0.0.1:8787`), or pass `api_key` / `base_url` (`apiKey` / `baseURL`).

## Quickstart

Python:

```python
from tokenlens import guard
client = guard.openai(OpenAI(), agent="support-bot")      # or guard.anthropic(Anthropic(), ...)
with guard.run("ticket-4812", budget_usd=5): client.chat.completions.create(...)
```

TypeScript:

```ts
import { guard } from '@tokenlens/sdk';
const client = guard.openai(new OpenAI(), { agent: 'support-bot' });  // or guard.anthropic(new Anthropic(), ...)
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, () => client.chat.completions.create(...));
```

## What a run sends

Inside `guard.run(...)` every request from a guarded client carries:

| Header | Value |
|---|---|
| `x-tl-run-id` | the run id you pass, or a generated `run_<32 hex>` |
| `x-tl-run-budget-usd` | `budget_usd` / `budgetUsd`, if set (the proxy fixes it on first use) |
| `x-tl-parent-run-id` | the enclosing run's id, when runs are nested — sub-agent spend counts against every ancestor |
| `x-tl-agent` | the client's `agent`, else the run's `agent` |
| `x-tl-user` | the client's `user`, else the run's `user` (inherited by sub-agent runs) — for spend reports by user |

Outside a run only `x-tl-agent` is sent and the proxy treats each request as its own run.
Headers you set yourself on a request (`extra_headers` / `headers`) take precedence.

Run state lives in a `contextvars.ContextVar` (Python) or `AsyncLocalStorage` (Node), so
concurrent asyncio tasks, threads started with `asyncio.to_thread`, and concurrent promises
each see their own run.

```python
with guard.run("ticket-4812", budget_usd=5):           # root run
    planner()
    with guard.run(budget_usd=1):                       # sub-agent: parent = ticket-4812
        researcher()
```

```ts
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, async () => {
  await planner();
  await guard.run({ budgetUsd: 1 }, () => researcher());   // parent = ticket-4812
});
```

## Errors

| Proxy response | Python | TypeScript |
|---|---|---|
| 402 `tokenlens_budget_exceeded` | `BudgetExceeded` | `BudgetExceeded` |
| 403 `tokenlens_run_paused` | `RunPaused` (a `RunStopped`) | `RunPaused` (a `RunStopped`) |
| 403 `tokenlens_run_killed` | `RunKilled` (a `RunStopped`) | `RunKilled` (a `RunStopped`) |
| local `max_calls` / `maxCalls` hit | `LocalCapExceeded` (a `BudgetExceeded`) | `LocalCapExceeded` (a `BudgetExceeded`) |

All are `TokenLensError`s with `run_id` / `runId` (from the `x-tl-run-id` response header),
`error_type` / `errorType`, `message`, and `status_code` / `status`. Errors from the proxy are
also the provider SDK's own error class (`openai.APIStatusError`, `PermissionDeniedError`, …),
so existing handlers keep catching them. Both error body shapes are understood: OpenAI
`{"error": {"type": …}}` and Anthropic `{"type": "error", "error": {"type": …}}`. The SDKs do
not retry 402/403.

```python
try:
    with guard.run("ticket-4812", budget_usd=5):
        agent.invoke(...)
except BudgetExceeded as e:
    log.warning("run %s stopped: %s", e.run_id, e.message)
```

## Local fallback cap

`guard.run(max_calls=50)` / `guard.run({ maxCalls: 50 }, fn)` caps the number of requests in a
run, counted in this process, including requests made in its child runs. It is checked before
each request is sent, so it still holds if the proxy is unreachable and the org is fail-open.
SDK retries of the same request are not counted (on very old Python SDKs such as openai 1.0,
which do not report retries, they are, so the cap errs on the strict side).

## How the wrapping works

`guard.openai(client)` returns a copy of your client (`with_options` / `withOptions`); the
original is not modified. The copy's class is a subclass of the SDK's own client class that
adds the headers when the SDK builds each request and re-labels proxy errors where the SDK
creates them (`_build_request` / `_make_status_error` in Python, `buildRequest` /
`makeStatusError` in Node). Calling `with_options` / `withOptions` on the guarded client keeps
the guard. For Anthropic, any provider `Authorization: Bearer` token is dropped so the proxy
sees only the TokenLens key.

## Develop

```bash
cd sdk/python && python3 -m venv .venv && .venv/bin/pip install -e ".[test]" && .venv/bin/pytest
```

```bash
cd sdk/typescript && npm install && npm run typecheck && npm test && npm run build
```

Both test suites run against a small fake proxy (a local HTTP server) that records request
headers and can answer with the proxy's 402/403 bodies; no network or real proxy is needed.
