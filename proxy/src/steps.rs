//! Agent logs: what an agent did between model calls (prompts, tool calls and their results,
//! sub-agents, the end of a turn or session), recorded as `"event": "step"` activity next to the
//! request events, and kept as long as run detail (7 days).
//!
//! Steps come from coding agents' hooks and from the SDKs (`POST /v1/steps`). By default a step
//! holds no prompt or tool-input text: only its kind, tool name, size, outcome and timing. A
//! project with `capture_content` on also keeps a redacted, shortened copy of that text.

use serde_json::{Value, json};

/// The longest content kept for one step, in characters.
pub const CONTENT_MAX_CHARS: usize = 2000;
/// The longest step or tool name kept.
const NAME_MAX_CHARS: usize = 120;
/// Most attributes kept on an SDK step, and the longest value.
const ATTRS_MAX: usize = 20;
const ATTR_VALUE_MAX_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Prompt,
    ToolCall,
    ToolResult,
    Subagent,
    TurnEnd,
    SessionEnd,
    /// Logged by an SDK.
    Log,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Prompt => "prompt",
            Kind::ToolCall => "tool_call",
            Kind::ToolResult => "tool_result",
            Kind::Subagent => "subagent",
            Kind::TurnEnd => "turn_end",
            Kind::SessionEnd => "session_end",
            Kind::Log => "log",
        }
    }
}

/// One step, before it is tied to an org, project and run.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub kind: Kind,
    /// Tool, sub-agent or log name.
    pub name: Option<String>,
    /// Pairs a tool call with its result.
    pub call_id: Option<String>,
    /// "ok", "error" or "blocked"; none while a tool call is still running.
    pub status: Option<&'static str>,
    pub level: Option<&'static str>,
    /// Bytes of prompt or tool input (or result), whether or not the text is kept.
    pub size: Option<u64>,
    /// Hash of the tool call (name and input): repeats show up without keeping the input.
    pub fingerprint: Option<String>,
    pub duration_ms: Option<u64>,
    /// Why a session ended, and similar short, non-content details.
    pub detail: Option<String>,
    /// Redacted and shortened; only with capture on.
    pub content: Option<String>,
    pub attrs: Option<Value>,
}

impl Step {
    fn new(kind: Kind) -> Step {
        Step {
            kind,
            name: None,
            call_id: None,
            status: None,
            level: None,
            size: None,
            fingerprint: None,
            duration_ms: None,
            detail: None,
            content: None,
            attrs: None,
        }
    }
}

fn first_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .filter(|v| !v.is_empty())
}

fn first_value<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|k| input.get(*k))
        .filter(|v| !v.is_null())
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Did a tool result fail? Each agent reports it differently; anything unrecognised is "ok".
fn result_failed(input: &Value) -> bool {
    let response = first_value(
        input,
        &["tool_response", "toolResult", "tool_output", "result"],
    );
    let flagged = |v: &Value| {
        v.get("is_error").and_then(Value::as_bool) == Some(true)
            || v.get("isError").and_then(Value::as_bool) == Some(true)
            || v.get("success").and_then(Value::as_bool) == Some(false)
            || v.get("error")
                .is_some_and(|e| !e.is_null() && e != "" && e != false)
            || v.get("resultType")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "success")
            || ["exit_code", "exitCode"]
                .iter()
                .any(|k| v.get(*k).and_then(Value::as_i64).is_some_and(|c| c != 0))
            // Copilot's shell tool reports "success" and puts the command's exit code in the text.
            || v.get("textResultForLlm")
                .and_then(Value::as_str)
                .and_then(shell_exit_code)
                .is_some_and(|c| c != 0)
    };
    // Codex reports only the output; Tokenrein's hook adds the exit code from its transcript.
    let codex = input
        .get(CODEX_RESULT)
        .is_some_and(|r| flagged(r) || r.get("status").and_then(Value::as_str) == Some("failed"));
    flagged(input) || response.is_some_and(flagged) || codex
}

/// The field Tokenrein's hook adds to a Codex PostToolUse event: `{exit_code, status,
/// duration_ms}` from the session transcript (see connect_hook.py).
const CODEX_RESULT: &str = "tokenrein_result";

/// The exit code in Copilot's shell trailer, `<shellId: 1 completed with exit code 1>`.
fn shell_exit_code(text: &str) -> Option<i64> {
    const MARK: &str = "completed with exit code ";
    let rest = &text[text.rfind(MARK)? + MARK.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The step a coding agent's hook event describes, if it is one Tokenrein logs. `capture` keeps
/// a redacted copy of the prompt or tool input.
pub fn from_hook(event: &str, input: &Value, capture: bool) -> Option<Step> {
    let kind = match event {
        "UserPromptSubmit" | "BeforeAgent" | "beforeSubmitPrompt" | "userPromptSubmitted" => {
            Kind::Prompt
        }
        "PreToolUse"
        | "BeforeTool"
        | "preToolUse"
        | "beforeShellExecution"
        | "beforeMCPExecution" => Kind::ToolCall,
        "PostToolUse"
        | "AfterTool"
        | "postToolUse"
        | "afterShellExecution"
        | "afterMCPExecution"
        | "PostToolUseFailure"
        | "postToolUseFailure" => Kind::ToolResult,
        "SubagentStart" | "subagentStart" => Kind::Subagent,
        "Stop" | "stop" => Kind::TurnEnd,
        "SessionEnd" | "sessionEnd" => Kind::SessionEnd,
        _ => return None,
    };
    let mut step = Step::new(kind);
    match kind {
        Kind::Prompt => {
            let prompt = first_str(input, &["prompt", "user_prompt", "userPrompt"]).unwrap_or("");
            step.size = Some(prompt.len() as u64);
            if capture && !prompt.is_empty() {
                step.content = Some(redact(prompt));
            }
        }
        Kind::ToolCall | Kind::ToolResult => {
            let name = first_str(input, &["tool_name", "toolName"])
                .or(match event {
                    "beforeShellExecution" | "afterShellExecution" => Some("Shell"),
                    "beforeMCPExecution" | "afterMCPExecution" => Some("MCP"),
                    _ => None,
                })
                .unwrap_or("tool");
            step.name = Some(clip(name, NAME_MAX_CHARS));
            step.call_id = first_str(
                input,
                &[
                    "tool_use_id",
                    "toolUseId",
                    "tool_call_id",
                    "toolCallId",
                    "call_id",
                ],
            )
            .map(|id| clip(id, NAME_MAX_CHARS));
            let tool_input = first_value(input, &["tool_input", "toolArgs", "command"]);
            if kind == Kind::ToolCall {
                let text = tool_input.map(text_of).unwrap_or_default();
                step.size = Some(text.len() as u64);
                let args = tool_input.map(Value::to_string).unwrap_or_default();
                step.fingerprint = Some(crate::loops::fingerprint(&[name, &args]));
                if capture && !text.is_empty() {
                    step.content = Some(redact(&text));
                }
            } else {
                // Claude Code, Cursor and Copilot report a failed tool on its own event.
                let failed = event.ends_with("ToolUseFailure") || result_failed(input);
                step.status = Some(if failed { "error" } else { "ok" });
                step.duration_ms = input
                    .get(CODEX_RESULT)
                    .and_then(|r| r.get("duration_ms"))
                    .and_then(Value::as_u64);
                let output = first_value(
                    input,
                    &[
                        "tool_response",
                        "toolResult",
                        "tool_output",
                        "output",
                        "result",
                    ],
                )
                .map(text_of)
                .unwrap_or_default();
                step.size = Some(output.len() as u64);
                if capture && !output.is_empty() {
                    step.content = Some(redact(&output));
                }
            }
        }
        Kind::Subagent => {
            step.name = first_str(input, &["agent_type", "subagent_type", "agentType", "name"])
                .map(|n| clip(n, NAME_MAX_CHARS));
        }
        Kind::SessionEnd => {
            step.detail = first_str(input, &["reason"]).map(|r| clip(r, NAME_MAX_CHARS));
        }
        Kind::TurnEnd | Kind::Log => {}
    }
    Some(step)
}

/// A step an SDK logged with `guard.log()`. Returns an error message for invalid input.
pub fn from_sdk(body: &Value, capture: bool) -> Result<Step, String> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or("name is required")?;
    let kind = match body.get("kind").and_then(Value::as_str).unwrap_or("log") {
        "log" => Kind::Log,
        "tool_call" => Kind::ToolCall,
        "tool_result" => Kind::ToolResult,
        "prompt" => Kind::Prompt,
        "subagent" => Kind::Subagent,
        other => return Err(format!("unknown kind {other:?}")),
    };
    let mut step = Step::new(kind);
    step.name = Some(clip(name, NAME_MAX_CHARS));
    step.level = Some(
        match body.get("level").and_then(Value::as_str).unwrap_or("info") {
            "debug" => "debug",
            "info" => "info",
            "warn" | "warning" => "warn",
            "error" => "error",
            other => return Err(format!("unknown level {other:?}")),
        },
    );
    step.status = match body.get("status").and_then(Value::as_str) {
        None => (step.level == Some("error")).then_some("error"),
        Some("ok") => Some("ok"),
        Some("error") => Some("error"),
        Some(other) => return Err(format!("status must be \"ok\" or \"error\", not {other:?}")),
    };
    step.call_id = body
        .get("call_id")
        .and_then(Value::as_str)
        .map(|c| clip(c, NAME_MAX_CHARS));
    step.duration_ms = body.get("duration_ms").and_then(Value::as_u64);
    if let Some(content) = body.get("content").filter(|c| !c.is_null()) {
        let text = text_of(content);
        step.size = Some(text.len() as u64);
        // Same tool, same input: what the trace counts as a repeated call. Only a hash is kept,
        // so it works with content capture off.
        if kind == Kind::ToolCall {
            step.fingerprint = Some(crate::loops::fingerprint(&[name, &content.to_string()]));
        }
        if capture && !text.is_empty() {
            step.content = Some(redact(&text));
        }
    }
    if let Some(attrs) = body.get("attrs").filter(|a| !a.is_null()) {
        let map = attrs.as_object().ok_or("attrs must be an object")?;
        let mut kept = serde_json::Map::new();
        for (k, v) in map.iter().take(ATTRS_MAX) {
            let v = match v {
                Value::Bool(_) | Value::Number(_) | Value::Null => v.clone(),
                Value::String(s) => json!(redact_short(s)),
                other => json!(redact_short(&other.to_string())),
            };
            kept.insert(clip(k, 64), v);
        }
        step.attrs = Some(Value::Object(kept));
    }
    Ok(step)
}

/// Where a step happened, for [`event`].
pub struct Place<'a> {
    pub org: &'a str,
    pub project: &'a str,
    pub agent: &'a str,
    pub user: Option<&'a str>,
    pub run: &'a str,
    pub parent: Option<&'a str>,
    pub source: &'a str,
}

/// The activity event for a step.
pub fn event(p: &Place, step: &Step) -> Value {
    json!({
        "event": "step",
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "step_id": format!("st_{}", uuid::Uuid::new_v4().simple()),
        "org": p.org,
        "project": p.project,
        "agent": p.agent,
        "user": p.user,
        "run_id": p.run,
        "parent_run_id": p.parent,
        "source": p.source,
        "kind": step.kind.name(),
        "name": step.name,
        "call_id": step.call_id,
        "status": step.status,
        "level": step.level,
        "size": step.size,
        "fingerprint": step.fingerprint,
        "duration_ms": step.duration_ms,
        "detail": step.detail,
        "content": step.content,
        "attrs": step.attrs,
    })
}

/// A request event, shortened for the org's log stream next to the steps.
pub fn model_call_entry(e: &Value) -> Value {
    let decision = e.get("decision").and_then(Value::as_str).unwrap_or("");
    let status = if decision.starts_with("blocked") {
        "blocked"
    } else if decision == "allowed" {
        "ok"
    } else {
        "error"
    };
    let detail = match e.get("blocked_scope") {
        Some(s) if !s.is_null() => s.clone(),
        _ => json!(decision),
    };
    let usage = e.get("usage");
    let tokens = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    json!({
        "event": "step",
        "ts": e.get("ts"),
        "step_id": e.get("request_id"),
        "org": e.get("org"),
        "project": e.get("project"),
        "agent": e.get("agent"),
        "user": e.get("user"),
        "run_id": e.get("run_id"),
        "parent_run_id": e.get("parent_run_id"),
        "source": e.get("source"),
        "kind": "model_call",
        "name": e.get("model"),
        "status": status,
        "detail": detail,
        "duration_ms": e.get("upstream_ms"),
        "cost_micros": e.get("actual_micros"),
        "tokens": tokens("input") + tokens("cached_input") + tokens("cache_write") + tokens("output"),
    })
}

fn clip(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn redact_short(s: &str) -> String {
    clip(&redact_all(s), ATTR_VALUE_MAX_CHARS)
}

/// `text` with secrets and email addresses replaced, cut to [`CONTENT_MAX_CHARS`].
pub fn redact(text: &str) -> String {
    clip(&redact_all(text), CONTENT_MAX_CHARS)
}

const REDACTED: &str = "[redacted]";

/// Prefixes of API keys and tokens (providers, GitHub, Slack, cloud, Tokenrein's own keys).
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "rk_",
    "pk_live_",
    "tr_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "glpat-",
    "npm_",
    "hf_",
    "whsec_",
    "shpat_",
];

/// Names whose value is a secret: `password=…`, `"api_key": "…"`, `Authorization: …`.
fn sensitive_name(name: &str) -> bool {
    let n: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "apikey",
        "authorization",
        "credential",
        "privatekey",
        "accesskey",
        "clientsecret",
        "cookie",
        "session",
    ]
    .iter()
    .any(|s| n.ends_with(s) || n == *s)
}

fn looks_secret(t: &str) -> bool {
    if KEY_PREFIXES
        .iter()
        .any(|p| t.starts_with(p) && t.len() >= p.len() + 12)
    {
        return true;
    }
    // A JSON web token.
    if t.starts_with("eyJ") && t.matches('.').count() == 2 && t.len() > 30 {
        return true;
    }
    // A long opaque string of letters and digits (but not a git commit id).
    let opaque = t.len() >= 32
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '='))
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().any(|c| c.is_ascii_alphabetic());
    let commit = t.len() == 40
        && t.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    opaque && !commit
}

fn looks_email(t: &str) -> bool {
    let Some((local, domain)) = t.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !t.contains("://")
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

fn is_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | '{' | '}' | '[' | ']' | '(' | ')' | '<' | '>'
        )
}

/// Replaces private key blocks, then scans word by word.
fn redact_all(text: &str) -> String {
    let mut text = text.to_string();
    while let Some(start) = text.find("-----BEGIN") {
        let Some(rel) = text[start..].find("PRIVATE KEY-----") else {
            break;
        };
        let after_begin = start + rel + "PRIVATE KEY-----".len();
        let end = text[after_begin..]
            .find("-----END")
            .and_then(|e| {
                let from = after_begin + e;
                text[from..]
                    .find("KEY-----")
                    .map(|k| from + k + "KEY-----".len())
            })
            .unwrap_or(text.len());
        text.replace_range(start..end, "[redacted private key]");
    }

    let mut out = String::with_capacity(text.len());
    let mut pending = false;
    let mut chars = text.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if is_delimiter(c) {
            out.push(c);
            chars.next();
            continue;
        }
        let mut end = text.len();
        while let Some(&(j, d)) = chars.peek() {
            if is_delimiter(d) {
                end = j;
                break;
            }
            chars.next();
        }
        let word = &text[i..end];
        let next_is_assign = text[end..]
            .trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .starts_with([':', '=']);
        if pending {
            let scheme = word.eq_ignore_ascii_case("bearer") || word.eq_ignore_ascii_case("basic");
            if scheme || matches!(word, ":" | "=" | ":=") {
                out.push_str(word);
                continue;
            }
            out.push_str(REDACTED);
            pending = false;
            continue;
        }
        if word.eq_ignore_ascii_case("bearer") || word.eq_ignore_ascii_case("basic") {
            out.push_str(word);
            pending = true;
            continue;
        }
        if let Some(pos) = word.find(['=', ':'])
            && pos > 0
            && sensitive_name(&word[..pos])
        {
            let (name, rest) = word.split_at(pos + 1);
            out.push_str(name);
            if rest.trim_start_matches([':', '=']).is_empty() {
                pending = true;
            } else {
                out.push_str(REDACTED);
            }
            continue;
        }
        if sensitive_name(word) && next_is_assign {
            out.push_str(word);
            pending = true;
            continue;
        }
        if let Some((scheme, rest)) = word.split_once("://")
            && let Some(at) = rest.find('@')
            && rest[..at].contains(':')
            && !rest[..at].contains('/')
        {
            out.push_str(scheme);
            out.push_str("://");
            out.push_str(REDACTED);
            out.push_str(&rest[at..]);
            continue;
        }
        if looks_secret(word) {
            out.push_str(REDACTED);
        } else if looks_email(word.trim_end_matches(['.', ':'])) {
            out.push_str("[email]");
            out.push_str(&word[word.trim_end_matches(['.', ':']).len()..]);
        } else {
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_keys_tokens_and_emails() {
        let cases = [
            (
                "export OPENAI_API_KEY=sk-proj-abcdefghijklmnop1234",
                "export OPENAI_API_KEY=[redacted]",
            ),
            (
                r#"{"password": "hunter2", "user": "ana"}"#,
                r#"{"password": "[redacted]", "user": "ana"}"#,
            ),
            (
                "curl -H 'Authorization: Bearer abc.def.ghi'",
                "curl -H 'Authorization: Bearer [redacted]'",
            ),
            ("token = 1234", "token = [redacted]"),
            ("mail ana@example.com now.", "mail [email] now."),
            (
                "git push https://bob:s3cret@github.com/x/y",
                "git push https://[redacted]@github.com/x/y",
            ),
            ("key ghp_abcdefghijklmnopqrstuvwxyz0123", "key [redacted]"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJlMTIz",
                "jwt [redacted]",
            ),
            (
                "hex 3f1c9e2b7a4d6f8e0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a",
                "hex [redacted]",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "input: {input}");
        }
    }

    #[test]
    fn keeps_ordinary_text() {
        for text in [
            "ls -la /Users/ana/project/src && cargo test --release",
            "commit 2f9d1c3b4a5e6f708192a3b4c5d6e7f809a1b2c3 fixed the token counter",
            "the session ended because the run hit its budget",
            "fn main() { println!(\"hello\"); }",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn redacts_private_keys_and_clips() {
        let pem =
            "before -----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY----- after";
        assert_eq!(redact(pem), "before [redacted private key] after");
        let long = "a ".repeat(CONTENT_MAX_CHARS);
        let r = redact(&long);
        assert_eq!(r.chars().count(), CONTENT_MAX_CHARS + 1);
        assert!(r.ends_with('…'));
    }

    #[test]
    fn hook_steps_keep_content_only_with_capture() {
        let input = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_use_id": "toolu_1", "tool_input": {"command": "echo $GITHUB_TOKEN"}});
        let off = from_hook("PreToolUse", &input, false).unwrap();
        assert_eq!(off.kind, Kind::ToolCall);
        assert_eq!(off.name.as_deref(), Some("Bash"));
        assert_eq!(off.call_id.as_deref(), Some("toolu_1"));
        assert!(off.content.is_none());
        assert!(off.size.unwrap() > 0);
        assert!(off.fingerprint.is_some());
        let on = from_hook("PreToolUse", &input, true).unwrap();
        assert_eq!(
            on.content.as_deref(),
            Some(r#"{"command":"echo $GITHUB_TOKEN"}"#)
        );

        let prompt = json!({"prompt": "deploy with password=abc"});
        let p = from_hook("UserPromptSubmit", &prompt, true).unwrap();
        assert_eq!(
            p.content.as_deref(),
            Some("deploy with password=[redacted]")
        );
        assert_eq!(
            from_hook("UserPromptSubmit", &prompt, false)
                .unwrap()
                .content,
            None
        );
        assert!(from_hook("Notification", &prompt, true).is_none());
    }

    #[test]
    fn tool_results_report_failures() {
        let ok = json!({"tool_name": "Read", "tool_response": {"file": "x"}});
        let copilot_ok = json!({"toolName": "bash", "toolResult": {"resultType": "success",
            "textResultForLlm": "tokenrein-ok\n<shellId: 0 completed with exit code 0>"}});
        assert_eq!(
            from_hook("postToolUse", &copilot_ok, false).unwrap().status,
            Some("ok")
        );
        assert_eq!(
            from_hook("PostToolUse", &ok, false).unwrap().status,
            Some("ok")
        );
        for failed in [
            json!({"tool_name": "Bash", "tool_response": {"is_error": true}}),
            json!({"toolName": "bash", "toolResult": {"resultType": "failure"}}),
            json!({"tool_name": "run", "tool_response": {"exit_code": 2}}),
            json!({"tool_name": "x", "error": "boom"}),
            // Copilot CLI's real payload for a failed shell command.
            json!({"toolName": "bash", "toolCallId": "call_1", "toolResult": {"resultType": "success",
                "textResultForLlm": "ls: /missing: No such file or directory\n<shellId: 1 completed with exit code 1>"}}),
        ] {
            assert_eq!(
                from_hook("postToolUse", &failed, false).unwrap().status,
                Some("error")
            );
        }
        // Codex CLI's real PostToolUse payload: plain output text, plus the result Tokenrein's
        // hook reads from the session transcript.
        let codex = |result: Value| {
            json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
                "tool_use_id": "exec-e5857aa1", "tool_input": {"command": "cat missing.txt"},
                "tool_response": "cat: missing.txt: No such file or directory\n",
                "tokenrein_result": result})
        };
        let failed = from_hook(
            "PostToolUse",
            &codex(json!({"exit_code": 1, "status": "failed", "duration_ms": 48})),
            false,
        )
        .unwrap();
        assert_eq!(failed.status, Some("error"));
        assert_eq!(failed.duration_ms, Some(48));
        assert_eq!(failed.call_id.as_deref(), Some("exec-e5857aa1"));
        let ok = from_hook(
            "PostToolUse",
            &codex(json!({"exit_code": 0, "status": "completed", "duration_ms": 3})),
            false,
        )
        .unwrap();
        assert_eq!((ok.status, ok.duration_ms), (Some("ok"), Some(3)));
        let no_transcript = from_hook("PostToolUse", &codex(Value::Null), false).unwrap();
        assert_eq!(
            (no_transcript.status, no_transcript.duration_ms),
            (Some("ok"), None)
        );

        let failure = json!({"tool_name": "Bash", "tool_use_id": "toolu_9", "error": "exit 1"});
        for event in ["PostToolUseFailure", "postToolUseFailure"] {
            let r = from_hook(event, &failure, false).unwrap();
            assert_eq!(
                (r.kind, r.status, r.call_id.as_deref()),
                (Kind::ToolResult, Some("error"), Some("toolu_9"))
            );
        }
    }

    #[test]
    fn sdk_steps_are_validated() {
        let s = from_sdk(
            &json!({"name": "retrieve", "level": "warn", "duration_ms": 120,
            "content": "query for ana@example.com", "attrs": {"hits": 3, "index": "docs"}}),
            false,
        )
        .unwrap();
        assert_eq!(
            (s.kind, s.level, s.duration_ms),
            (Kind::Log, Some("warn"), Some(120))
        );
        assert!(s.content.is_none());
        assert_eq!(s.attrs, Some(json!({"hits": 3, "index": "docs"})));
        let s = from_sdk(&json!({"name": "x", "level": "error"}), true).unwrap();
        assert_eq!(s.status, Some("error"));
        assert!(from_sdk(&json!({"level": "info"}), false).is_err());
        assert!(from_sdk(&json!({"name": "x", "kind": "nope"}), false).is_err());
    }

    #[test]
    fn sdk_tool_calls_with_the_same_input_share_a_fingerprint() {
        let call = |q: &str| {
            from_sdk(
                &json!({"name": "web_search", "kind": "tool_call", "content": {"q": q}}),
                false,
            )
            .unwrap()
            .fingerprint
        };
        assert!(call("pricing").is_some());
        assert_eq!(call("pricing"), call("pricing"));
        assert_ne!(call("pricing"), call("docs"));
        let no_input =
            from_sdk(&json!({"name": "web_search", "kind": "tool_call"}), false).unwrap();
        assert!(no_input.fingerprint.is_none());
        let log = from_sdk(&json!({"name": "web_search", "content": "pricing"}), false).unwrap();
        assert!(log.fingerprint.is_none());
    }

    #[test]
    fn model_calls_are_shortened() {
        let e = json!({"event": "request", "ts": "t", "request_id": "r1", "org": "o",
            "run_id": "run", "model": "gpt-4o", "decision": "blocked_budget",
            "blocked_scope": "run:run", "actual_micros": 0, "upstream_ms": 0,
            "usage": {"input": 10, "output": 5}});
        let m = model_call_entry(&e);
        assert_eq!(m["kind"], "model_call");
        assert_eq!(m["status"], "blocked");
        assert_eq!(m["detail"], "run:run");
        assert_eq!(m["tokens"], 15);
    }
}
