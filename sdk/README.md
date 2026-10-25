# Tokenrein SDKs

Wrap your existing OpenAI or Anthropic client in one line and every call is budgeted, traced
per run and agent, and shown in the Tokenrein dashboard. Apache-2.0.

Two modes:

- **Direct (default):** the client keeps calling its provider with your provider key. Before
  each call the SDK asks Tokenrein whether the run may continue; after each response it reads
  the token usage (streams too) and reports it in the background. Prompts and responses never
  leave your process, and if Tokenrein is unreachable calls still go through.
- **Proxy (`mode="proxy"`):** calls go through the Tokenrein proxy, which checks every call
  before sending it and is exact under heavy concurrency. Use it when you need hard guarantees.

Direct mode works with any provider the `openai` SDK can reach through `base_url`: OpenAI, Azure
OpenAI, Google Gemini, Groq, Mistral, DeepSeek, xAI, OpenRouter, Together, Fireworks, and local
Ollama / vLLM / LM Studio (priced at $0). The provider is recognised from the base URL.

| Package | Path | Wraps |
|---|---|---|
| `tokenrein` (Python ≥ 3.9) | [`python/`](python) | `openai` ≥ 1.0, `anthropic` ≥ 0.30 (sync and async clients) |
| `@tokenrein/sdk` (Node ≥ 20) | [`typescript/`](typescript) | `openai` ≥ 4.0, `@anthropic-ai/sdk` ≥ 0.30 |

Configuration: `TOKENREIN_API_KEY` (a Tokenrein key, `tr_…`), `TOKENREIN_BASE_URL` (default
`http://127.0.0.1:8787`) and optionally `TOKENREIN_MODE` (`direct` or `proxy`); or pass
`api_key` / `base_url` / `mode` (`apiKey` / `baseURL` / `mode`). In direct mode your provider
key stays with your client as usual.

Options: `agent`, `user` (a person or service, emails allowed; inherited by sub-agent runs),
`mode`, and `check` (direct mode: `False` skips the per-call check and only stops runs that
recent reports showed are over an enforced limit). Short-lived scripts can call
`tokenrein.flush()` / `await flush()` to send queued usage before exiting (Python also flushes at
exit).

## Quickstart

Python:

```python
from tokenrein import guard
client = guard.openai(OpenAI(), agent="support-bot")      # or guard.anthropic(Anthropic(), ...)
with guard.run("ticket-4812", budget_usd=5): client.chat.completions.create(...)
```

TypeScript:

```ts
import { guard } from '@tokenrein/sdk';
const client = guard.openai(new OpenAI(), { agent: 'support-bot' });  // or guard.anthropic(new Anthropic(), ...)
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, () => client.chat.completions.create(...));
```

## Using other providers

```python
gemini = guard.openai(OpenAI(base_url="https://generativelanguage.googleapis.com/v1beta/openai/",
                             api_key=os.environ["GEMINI_API_KEY"]), agent="researcher")
local = guard.openai(OpenAI(base_url="http://localhost:11434/v1", api_key="ollama"), agent="drafts")
```

## What a run sends

Inside `guard.run(...)` every call from a guarded client is attributed to the run. In proxy mode
these travel as headers; in direct mode the same fields go to Tokenrein's check and usage
reports instead, and nothing extra is sent to your provider:

| Header | Value |
|---|---|
| `x-tr-run-id` | the run id you pass, or a generated `run_<32 hex>` |
| `x-tr-run-budget-usd` | `budget_usd` / `budgetUsd`, if set (the proxy fixes it on first use) |
| `x-tr-parent-run-id` | the enclosing run's id, when runs are nested — sub-agent spend counts against every ancestor |
| `x-tr-agent` | the client's `agent`, else the run's `agent` |
| `x-tr-user` | the client's `user`, else the run's `user` (inherited by sub-agent runs) — for spend reports by user |

Outside a run only `x-tr-agent` is sent and the proxy treats each request as its own run.
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
| 402 `tokenrein_budget_exceeded` | `BudgetExceeded` | `BudgetExceeded` |
| 403 `tokenrein_run_paused` | `RunPaused` (a `RunStopped`) | `RunPaused` (a `RunStopped`) |
| 403 `tokenrein_run_killed` | `RunKilled` (a `RunStopped`) | `RunKilled` (a `RunStopped`) |
| local `max_calls` / `maxCalls` hit | `LocalCapExceeded` (a `BudgetExceeded`) | `LocalCapExceeded` (a `BudgetExceeded`) |

All are `TokenreinError`s with `run_id` / `runId` (from the `x-tr-run-id` response header),
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
sees only the Tokenrein key.

## Develop

```bash
cd sdk/python && python3 -m venv .venv && .venv/bin/pip install -e ".[test]" && .venv/bin/pytest
```

```bash
cd sdk/typescript && npm install && npm run typecheck && npm test && npm run build
```

Both test suites run against a small fake proxy (a local HTTP server) that records request
headers and can answer with the proxy's 402/403 bodies; no network or real proxy is needed.
