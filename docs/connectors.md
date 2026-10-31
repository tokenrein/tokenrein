# Coding-agent connectors

Tokenrein tracks and stops coding agents without sitting in their request path. Each connector
uses what the agent already has: its OpenTelemetry export for usage (model, tokens, user) and
its hooks to refuse the next prompt or tool call when a run is paused or killed, or an enforced
budget is used up. This page lists the exact fields Tokenrein relies on, and where each is
documented, so the mappers can be checked when the agents change.

Checked against the agents' docs and source on 2026-10-22 (GitHub Copilot: 2026-10-31).

| Agent | Usage from | Blocks with | Run id | Connect script |
|---|---|---|---|---|
| Claude Code | OTLP logs, `claude_code.api_request` | HTTP hooks | `cc-<session.id>` (`-sub` for sub-agents) | `/connect/claude-code.py` |
| OpenAI Codex CLI | OTLP logs, `codex.sse_event` (`response.completed`) | command hooks | `codex-<conversation.id>` | `/connect/codex.py` |
| Gemini CLI | OTLP logs, `gemini_cli.api_response` | command hooks | `gemini-<session.id>` (`-sub` for sub-agents) | `/connect/gemini.py` |
| Cursor | Admin API usage events (team plans) | command hooks | `cursor-<conversation_id>` | `/connect/cursor.py` |
| GitHub Copilot CLI | OTLP traces, `chat` spans | command hooks (tool calls only) | `copilot-<sessionId>` | `/connect/copilot.py` |

All endpoints are on the proxy listener (`:8787`) and take a Tokenrein key
(`Authorization: Bearer tr_…`). `x-tr-agent` and `x-tr-user` headers override the agent name
(default: `claude-code`, `codex`, `gemini-cli`, `cursor`, `copilot`) and the user.

Codex CLI, Gemini CLI, Cursor and Copilot CLI get *command* hooks, so their connect
scripts install `~/.tokenrein/hook.py` (Python standard library). The agent runs
`python3 ~/.tokenrein/hook.py <tool>` with the hook event on stdin; it posts the event to
`/v1/hooks/<tool>` with the key from `<tool>.json` next to it (`~/.tokenrein`, mode 0600) and prints
Tokenrein's answer. If Tokenrein can't be reached in 4 seconds it prints the agent's "allow"
answer: a Tokenrein outage never stops anyone's agent. The server also fails open when its
ledger is down.

## OpenAI Codex CLI

Docs: [Advanced configuration: observability and telemetry](https://developers.openai.com/codex/config-advanced),
[configuration reference](https://developers.openai.com/codex/config-reference),
[hooks](https://developers.openai.com/codex/hooks) (these now redirect to learn.chatgpt.com).
Event fields are from the source: [`codex-rs/otel/src/events/session_telemetry.rs`](https://github.com/openai/codex/blob/main/codex-rs/otel/src/events/session_telemetry.rs)
and [`events/shared.rs`](https://github.com/openai/codex/blob/main/codex-rs/otel/src/events/shared.rs).

**Telemetry.** `~/.codex/config.toml`:

```toml
[otel]
environment = "tokenrein"
log_user_prompt = false
exporter = { otlp-http = { endpoint = "http://127.0.0.1:8787/v1/otel/v1/logs", protocol = "json", headers = { "Authorization" = "Bearer tr_…", "x-tr-agent" = "codex" } } }
```

- Resource `service.name` is Codex's originator (e.g. `codex_cli_rs`); records are recognised by
  the `codex.` event-name prefix, or a `service.name` starting with `codex`.
- Every event carries `conversation.id` (the run), `model`, `slug`, `user.email`,
  `user.account_id`, `auth_mode`, `app.version`.
- `event.name = "codex.sse_event"` with `event.kind = "response.completed"` carries the usage of
  one model response: `input_token_count`, `output_token_count`, `cached_token_count`,
  `cache_write_token_count`, `reasoning_token_count`, `tool_token_count` (the total). Some are
  formatted as strings; both are accepted.
- OpenAI counts cached input inside `input_token_count`, so Tokenrein charges
  `input_token_count − cached_token_count` at the input price and `cached_token_count` at the
  cached price. Reasoning tokens are already inside `output_token_count`.
- Codex sends no cost; Tokenrein prices the tokens (also for ChatGPT sign-ins, as an estimate).
  `codex.turn_cost` (`usage.estimated_usd`, per turn) is not used, so calls are not counted twice.
- `codex.conversation_starts`, `codex.user_prompt`, `codex.tool_decision` and
  `codex.tool_result` only mark the session as active. Failed responses (no token counts) and
  other stream events are ignored.
- Sub-agents run in their own thread with their own `conversation.id`; the parent is not on
  usage events, so they show up as separate `codex-…` runs.

**Hooks.** `~/.codex/hooks.json` (`$CODEX_HOME` if set), command handlers. Codex skips new hooks
until they are reviewed and trusted with `/hooks`.

| Event | Input used | Blocking answer |
|---|---|---|
| `UserPromptSubmit` | `session_id` | `{"decision": "block", "reason": …}` |
| `PreToolUse` (matcher `*`) | `session_id` | `{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": …}}` |
| `SessionEnd` | `session_id` | — (marks the run ended; 3 s max timeout) |

Codex's `session_id` is the thread id, the same value as `conversation.id`; sub-agent hooks carry
the parent session's id. `SubagentStart` can't stop a sub-agent (`continue: false` is ignored), so
it is not used. Allowing is `{}`.

## Gemini CLI

Docs: [Observability with OpenTelemetry](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/telemetry.md),
[configuration reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md),
[hooks](https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/index.md),
[hooks reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md).
Source: [`telemetry/types.ts`](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/telemetry/types.ts)
(`ApiResponseEvent`), [`telemetry/sdk.ts`](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/telemetry/sdk.ts),
[`telemetryAttributes.ts`](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/telemetry/telemetryAttributes.ts).

**Telemetry.** `~/.gemini/settings.json`:

```json
{"telemetry": {"enabled": true, "target": "local", "otlpEndpoint": "http://127.0.0.1:8787/v1/otel",
  "otlpProtocol": "http", "otlpHeaders": {"Authorization": "Bearer tr_…", "x-tr-agent": "gemini-cli"},
  "logPrompts": false}}
```

- With `otlpProtocol: "http"` Gemini CLI appends `/v1/logs`, `/v1/traces` and `/v1/metrics` to
  the endpoint and sends OTLP/JSON. Tokenrein reads the logs; traces and metrics are accepted and
  dropped.
- Resource `service.name = "gemini-cli"`; events are named `gemini_cli.*`.
- Every event carries `session.id` (the run), `installation.id`, `user.email` (when signed in),
  `auth_type`.
- `event.name = "gemini_cli.api_response"`: `model`, `duration_ms`, `status_code`,
  `input_token_count` (Gemini's `promptTokenCount`), `output_token_count`
  (`candidatesTokenCount`), `cached_content_token_count`, `thoughts_token_count`,
  `tool_token_count`, `total_token_count`, `prompt_id`, `role` (`main`, `subagent`,
  `utility_…`).
- Cached content is inside the prompt count, so Tokenrein charges
  `input_token_count − cached_content_token_count` as input and the cached part at the cached
  price. Thinking tokens are billed as output: output = `output_token_count +
  thoughts_token_count`. Gemini CLI sends no cost; Tokenrein prices the tokens.
- Each response is also logged as `gen_ai.client.inference.operation.details` (with
  `gen_ai.usage.*`); that copy is ignored so nothing is counted twice.
- `role = "subagent"` calls go to the child run `gemini-<session>-sub`.
- `gemini_cli.user_prompt`, `gemini_cli.tool_call` and `gemini_cli.agent.start` only mark the
  session as active.

**Hooks.** `hooks` in the same settings file; command handlers, timeout in milliseconds. Hooks
run with a sanitised environment, which is why the hook reads its key from a file. On by default
(`hooksConfig.enabled`).

| Event | Input used | Blocking answer |
|---|---|---|
| `BeforeAgent` (after a prompt is submitted) | `session_id` | `{"decision": "deny", "reason": …}` |
| `BeforeTool` (matcher `*`) | `session_id` | `{"decision": "deny", "reason": …}` |
| `BeforeModel` (only with `--every-model-call`) | `session_id` | `{"decision": "deny", "reason": …}` |
| `SessionEnd` | `session_id` | — (marks the run ended) |

Allowing is `{}`.

## Cursor

Docs: [hooks](https://cursor.com/docs/agent/hooks),
[Admin API](https://cursor.com/docs/account/teams/admin-api) (`/teams/filtered-usage-events`).

**Hooks.** `~/.cursor/hooks.json`, `{"version": 1, "hooks": {…}}`, command entries, timeout in
seconds. Every hook gets `conversation_id`, `generation_id`, `model`, `hook_event_name`,
`cursor_version`, `workspace_roots`, `user_email`, `transcript_path`. For permission hooks Cursor
blocks on invalid JSON, so the hook always prints a valid answer.

| Event | Input used | Allowing answer | Blocking answer |
|---|---|---|---|
| `beforeSubmitPrompt` | `conversation_id`, `user_email` | `{"continue": true}` | `{"continue": false, "user_message": …}` |
| `preToolUse` (all tools: shell, MCP, file edits) | `conversation_id` | `{"permission": "allow"}` | `{"permission": "deny", "user_message": …, "agent_message": …}` |
| `subagentStart` | `parent_conversation_id` | `{"permission": "allow"}` | `{"permission": "deny", "user_message": …}` |
| `sessionEnd` | `session_id` (same as `conversation_id`) | — | — (marks the run ended) |

`beforeShellExecution` and `beforeMCPExecution` are answered the same way as `preToolUse` if you
wire them yourself; the connect script doesn't, because `preToolUse` already covers them.

**Usage.** Cursor has no local telemetry export. Teams can import usage from the Admin API: save
an Admin API key on the Connect page (or `PUT /admin/orgs/{org}/connectors/cursor
{"project", "api_key"}` on the admin listener). It is encrypted with `TOKENREIN_SECRETS_KEY` like
provider keys, bound to the org, and never returned.

- `POST https://api.cursor.com/teams/filtered-usage-events`, Basic auth with the key as the user
  name, body `{"startDate", "endDate", "page", "pageSize"}` (epoch ms, inclusive), paged until
  `pagination.hasNextPage` is false.
- Per event: `timestamp` (epoch ms, as a string), `userEmail`, `conversationId` (the same id as
  the hooks' `conversation_id`), `cloudAgentId`, `model`, `tokenUsage.inputTokens`,
  `outputTokens`, `cacheReadTokens`, `cacheWriteTokens`, `totalCents`, and `chargedCents`.
- The cost charged is `chargedCents / 100` (model cost plus Cursor's token fee; this is what
  Cursor's spend totals add up), falling back to `tokenUsage.totalCents`. Events without a
  conversation go to `cursor-<user>-<YYYYMMDD>`.
- It runs hourly (Cursor aggregates usage hourly) and on demand
  (`POST /admin/orgs/{org}/connectors/cursor/sync`). The first run reads from the start of the
  current UTC day; later runs re-read the last 3 hours for late events. Each event is charged
  once (a hash of it is remembered for 10 days).
- Imported spend is charged to today's budgets when it arrives, up to an hour after the calls.
  The hooks then stop the next prompt once an enforced budget is used up.

## GitHub Copilot CLI

Docs: [hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference),
[Copilot CLI OpenTelemetry monitoring](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference#opentelemetry-monitoring),
[OpenTelemetry for agent monitoring](https://docs.github.com/en/copilot/concepts/enterprise/opentelemetry),
[enterprise managed settings](https://docs.github.com/en/copilot/reference/enterprise-administrators/enterprise-managed-settings)
(`telemetry`), [CLI configuration directory](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference)
(managed settings locations), and VS Code's [Monitor agent usage with OpenTelemetry](https://code.visualstudio.com/docs/agents/guides/monitoring-agents).
The payloads and spans below are from those docs; the mappers and tests were not checked against
a captured export from a real Copilot client.

**Telemetry.** Copilot CLI exports OpenTelemetry *traces and metrics* (no logs) when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set; the default protocol is `http/json`. The connect script
writes `~/.tokenrein/copilot.sh` (mode 0600), sourced from `~/.zshrc` / `~/.bashrc`, which runs
the `copilot` command with:

```sh
COPILOT_OTEL_ENABLED=true
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:8787/v1/otel     # Copilot appends /v1/traces, /v1/metrics
OTEL_EXPORTER_OTLP_PROTOCOL=http/json
OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer tr_…,x-tr-agent=copilot'
```

Only the `copilot` command gets them (a shell function), so other OpenTelemetry programs keep their
own setup. `--no-profile` leaves the shell profiles alone and prints the line to add.

- Each model call is a `chat` span (`gen_ai.operation.name = "chat"`, kind CLIENT) carrying
  `gen_ai.conversation.id` (the session; the run), `gen_ai.provider.name`,
  `gen_ai.request.model` / `gen_ai.response.model`, `gen_ai.usage.input_tokens`,
  `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`,
  `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.response.id`, `github.copilot.cost` (the
  premium-request multiplier, not money) and `github.copilot.nano_aiu`.
- The GenAI conventions count cached input inside `input_tokens`, so Tokenrein charges
  `input − cache_read − cache_creation` at the input price, `cache_read` at the cached price and
  `cache_creation` at the cache-write price. Reasoning is inside `output_tokens`.
- Copilot reports its own API (`github`) as the provider; Tokenrein prices the model at its
  maker's API price (`claude-*` → Anthropic, `gpt-*`/`o<N>` → OpenAI, `gemini-*` → Google), and
  writes Copilot's Claude ids with dashes (`claude-sonnet-4.5` → `claude-sonnet-4-5`). Spend is an
  estimate: GitHub bills premium requests (and AI units), not tokens.
- The `invoke_agent` span carries the totals of one agent invocation; it only marks the session as
  active, so nothing is counted twice. `execute_tool` spans and metrics are dropped.
- Spans count as Copilot's if the resource's `service.name` or the scope name starts with
  `github-copilot`, `github.copilot` or `copilot`, or the span has `github.copilot.*` /
  `copilot_chat.*` attributes. Other agents' spans are dropped.
- `user.name` (the GitHub login) is only sent when identity capture is on (managed settings
  `telemetry.capture.identity`, which `--policy` sets); otherwise pass `--user`.
- Exporters retry failed batches, so each span (`traceId` + `spanId`) is charged once (remembered
  for two days).
- VS Code's Copilot Chat puts the session only on the `invoke_agent` span; Tokenrein takes it from
  the span with the same `traceId` in the same batch, else the resource's `session.id` (one VS Code
  window), else the trace.

**Hooks.** `~/.copilot/hooks/tokenrein.json` (`$COPILOT_HOME/hooks` if set), `{"version": 1,
"hooks": {…}}`, command entries (`bash`, `timeoutSec`). The native camelCase events send camelCase
fields and don't name the event, so the command passes it: `python3 ~/.tokenrein/hook.py copilot
preToolUse`, and the hook adds `hook_event_name`. VS Code-compatible PascalCase events
(`PreToolUse`, snake_case fields) are read too.

| Event | Input used | Blocking answer |
|---|---|---|
| `preToolUse` | `sessionId`, `toolName`, `toolArgs` (loop guard) | `{"permissionDecision": "deny", "permissionDecisionReason": …}` |
| `userPromptSubmitted` | `sessionId` | — (Copilot drops command hooks' answers to prompts; marks the session active) |
| `sessionEnd` | `sessionId` | — (marks the run ended) |

Allowing is `{}` (Copilot's default permission flow). `subagentStart` can't stop a sub-agent, and
sub-agents' hooks carry the session's id, so their tool calls are checked against the session's
run. Command `preToolUse` hooks fail *closed* when the command crashes or exits non-zero, but open
on a timeout; the hook always exits 0 and prints `{}` when Tokenrein can't be reached.

**Every user of a machine (`--policy`, as root).** For MDM or machine images:

- `/etc/github-copilot/policy.d/tokenrein.json`: Copilot *policy hooks*, loaded before all others,
  not turned off by `disableAllHooks`, and run on the host even in a sandboxed session. Root-owned,
  mode 0644 (Copilot ignores group- or world-writable policy files).
- `/etc/tokenrein/hook.py` and `/etc/tokenrein/copilot.json` (mode 0644: every user's hooks read
  the key there).
- Copilot's managed settings, `/Library/Application Support/GitHubCopilot/managed-settings.json`
  on macOS or `/etc/github-copilot/managed-settings.json` on Linux: `telemetry` with `enabled`,
  `endpoint` (`…/v1/otel`), `protocol: "http/json"`, `headers` and `capture: {"identity": true}`.
  Managed values override users' settings and environment. Other keys in the file are kept; an
  existing `telemetry` block that isn't Tokenrein's is left alone and the script stops.

Undo with `sudo python3 copilot.py --policy --remove`.

## Not covered

- Codex and Gemini report tokens, not cost: unknown models are charged at the provider's most
  expensive price until you add one under Models & prices.
- Cursor without a team Admin API key: conversations are tracked and can be paused, but their
  spend is not known to Tokenrein.
- Cloud agents (Cursor background agents, Codex cloud, Copilot cloud agent) don't run user-level
  hooks. Copilot's cloud agent only runs `.github/hooks/*.json` from the repository, behind its
  firewall.
- Copilot can only be stopped before a tool call: its prompt hooks can't refuse a prompt, so a
  paused run still gets one model answer to the next prompt.
- Copilot in VS Code: managed settings (`--policy`) also turn on VS Code's export, but VS Code may
  send OTLP as protobuf, which Tokenrein refuses (415); it reads OTLP/JSON only. VS Code's own
  agent hooks are not installed by the script.
- Copilot spend is priced from tokens; `github.copilot.nano_aiu` and premium-request multipliers
  are not converted to dollars.
