//! What a proxied request is made of, without keeping any of it.
//!
//! In proxy mode Tokenrein sees the whole request. For each one it hashes and sizes the parts a
//! provider's prompt cache depends on, in the order the cache reads them: tool definitions, the
//! system prompt, cache-relevant settings, then the conversation. Comparing those hashes with the
//! run's previous request names what broke the cache ("the tool list changed", "an earlier
//! message was edited"). The same pass records which tools are defined and which get called, so
//! unused definitions show up, and turns the tool results in the conversation into agent steps for
//! runs that send no steps of their own. Only hashes, counts, sizes and tool names are kept.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::Utc;
use redis::AsyncCommands;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::providers::Api;
use crate::proxy::AppState;

/// Settings that change what a provider caches (a different value starts a new cache).
const CACHE_PARAMS: &[&str] = &[
    "tool_choice",
    "thinking",
    "reasoning",
    "reasoning_effort",
    "output_config",
    "response_format",
    "text",
    "parallel_tool_calls",
    "speed",
];
/// Tool names kept per request, at most.
const MAX_NAMES: usize = 200;
/// Tool results turned into steps per request, at most.
const MAX_RESULTS: usize = 50;
const DAY_SECS: u64 = 86_400;
/// Tool names are kept per day for this long, a little over the Insights page's week.
const TOOL_SET_DAYS: u64 = 8;

/// Hashes and sizes of one request's parts.
#[derive(Debug, Clone, Default)]
pub struct Anatomy {
    pub tools_hash: String,
    /// Tool definitions sent in full with every request.
    pub tool_defs: u64,
    /// Tool definitions loaded on demand (`defer_loading`), which cost nothing until used.
    pub deferred_tools: u64,
    /// About how many tokens the full definitions take.
    pub tool_def_tokens: u64,
    pub system_hash: String,
    pub system_tokens: u64,
    pub params_hash: String,
    /// `message_hashes[i]` covers messages `0..=i`, so a prefix is checked in one comparison.
    pub message_hashes: Vec<String>,
    /// The conversation continues a stored response (`previous_response_id`): earlier turns are
    /// not in the request, so they can't be compared.
    pub chained: bool,
    /// Names of every tool defined (full or deferred).
    pub defined_tools: Vec<String>,
    /// Names of the tools the model called in its latest turn.
    pub called_tools: Vec<String>,
    /// Results of those calls that arrive with this request.
    pub results: Vec<ToolOutcome>,
}

/// A tool call and its result, read from the conversation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutcome {
    pub call_id: String,
    pub name: String,
    pub fingerprint: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub error: bool,
}

/// About four bytes of JSON or English to a token.
fn approx_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn hash(v: &Value) -> String {
    // serde_json keeps object keys sorted, so key order in the request does not change the hash.
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    hex::encode(&Sha256::digest(&bytes)[..8])
}

fn chain(prev: &str, v: &Value) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(serde_json::to_vec(v).unwrap_or_default());
    hex::encode(&h.finalize()[..8])
}

fn text_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Null => 0,
        other => other.to_string().len(),
    }
}

fn tool_name(t: &Value) -> Option<String> {
    t.get("name")
        .or_else(|| t.pointer("/function/name"))
        .or_else(|| t.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn role(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

fn blocks(m: &Value) -> impl Iterator<Item = &Value> {
    m.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn is_item(m: &Value, types: &[&str]) -> bool {
    m.get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| types.contains(&t))
}

/// Read a request body's parts. `body` is the parsed request, before Tokenrein changes it.
pub fn anatomy(api: Api, body: &Map<String, Value>) -> Anatomy {
    let mut a = Anatomy::default();
    let tools: &[Value] = body
        .get("tools")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let mut full = Vec::new();
    for t in tools {
        if t.get("defer_loading").and_then(Value::as_bool) == Some(true) {
            a.deferred_tools += 1;
        } else {
            a.tool_defs += 1;
            full.push(t.clone());
        }
        if a.defined_tools.len() < MAX_NAMES
            && let Some(n) = tool_name(t)
        {
            a.defined_tools.push(n);
        }
    }
    a.tools_hash = hash(&Value::Array(tools.to_vec()));
    a.tool_def_tokens = if full.is_empty() {
        0
    } else {
        approx_tokens(Value::Array(full).to_string().len())
    };
    let params: Map<String, Value> = CACHE_PARAMS
        .iter()
        .filter_map(|k| body.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    a.params_hash = hash(&Value::Object(params));

    // The system prompt, and the conversation after it.
    let (system, messages): (Value, Vec<Value>) = match api {
        Api::AnthropicMessages => (
            body.get("system").cloned().unwrap_or(Value::Null),
            body.get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        Api::OpenAIChat => {
            let all = body
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let lead = all
                .iter()
                .take_while(|m| matches!(role(m), "system" | "developer"))
                .count();
            (Value::Array(all[..lead].to_vec()), all[lead..].to_vec())
        }
        Api::OpenAIResponses => {
            a.chained = body
                .get("previous_response_id")
                .is_some_and(|v| !v.is_null());
            let input = match body.get("input") {
                Some(Value::Array(items)) => items.clone(),
                Some(Value::Null) | None => Vec::new(),
                Some(other) => vec![other.clone()],
            };
            (
                body.get("instructions").cloned().unwrap_or(Value::Null),
                input,
            )
        }
    };
    a.system_hash = hash(&system);
    a.system_tokens = match &system {
        Value::Array(parts) => parts
            .iter()
            .map(|p| p.get("content").or_else(|| p.get("text")).unwrap_or(p))
            .map(|v| approx_tokens(text_len(v)))
            .sum(),
        other => approx_tokens(text_len(other)),
    };
    let mut prev = String::new();
    for m in &messages {
        prev = chain(&prev, m);
        a.message_hashes.push(prev.clone());
    }
    let (called, results) = latest_turn(api, &messages);
    a.called_tools = called;
    a.results = results;
    a
}

/// The tools the model called in its latest turn, and the results that came back for them.
fn latest_turn(api: Api, messages: &[Value]) -> (Vec<String>, Vec<ToolOutcome>) {
    // (call id, name, input)
    let mut calls: Vec<(String, String, Value)> = Vec::new();
    // (call id, output, failed)
    let mut outputs: Vec<(String, Value, bool)> = Vec::new();
    match api {
        Api::AnthropicMessages => {
            let Some(last) = messages.iter().rposition(|m| role(m) == "assistant") else {
                return (Vec::new(), Vec::new());
            };
            for b in blocks(&messages[last]) {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    calls.push((
                        str_of(b, "id"),
                        str_of(b, "name"),
                        b.get("input").cloned().unwrap_or(Value::Null),
                    ));
                }
            }
            for b in messages[last + 1..].iter().flat_map(blocks) {
                if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                    outputs.push((
                        str_of(b, "tool_use_id"),
                        b.get("content").cloned().unwrap_or(Value::Null),
                        b.get("is_error").and_then(Value::as_bool) == Some(true),
                    ));
                }
            }
        }
        Api::OpenAIChat => {
            let Some(last) = messages.iter().rposition(|m| role(m) == "assistant") else {
                return (Vec::new(), Vec::new());
            };
            let tool_calls = messages[last].get("tool_calls").and_then(Value::as_array);
            for c in tool_calls.into_iter().flatten() {
                calls.push((
                    str_of(c, "id"),
                    c.pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    c.pointer("/function/arguments")
                        .cloned()
                        .unwrap_or(Value::Null),
                ));
            }
            for m in messages[last + 1..].iter().filter(|m| role(m) == "tool") {
                outputs.push((
                    str_of(m, "tool_call_id"),
                    m.get("content").cloned().unwrap_or(Value::Null),
                    false,
                ));
            }
        }
        Api::OpenAIResponses => {
            // Calls and their outputs are items of their own; with `previous_response_id` the
            // calls live in the stored response and only the outputs are sent.
            const OUTPUTS: &[&str] = &["function_call_output", "custom_tool_call_output"];
            let start = messages
                .iter()
                .rposition(|m| !is_item(m, OUTPUTS))
                .map_or(0, |i| i + 1);
            for m in messages[..start]
                .iter()
                .filter(|m| is_item(m, &["function_call", "custom_tool_call"]))
            {
                calls.push((
                    str_of(m, "call_id"),
                    str_of(m, "name"),
                    m.get("arguments")
                        .or_else(|| m.get("input"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ));
            }
            for m in &messages[start..] {
                outputs.push((
                    str_of(m, "call_id"),
                    m.get("output").cloned().unwrap_or(Value::Null),
                    false,
                ));
            }
        }
    }
    let by_id: HashMap<&str, (&str, &Value)> = calls
        .iter()
        .map(|(id, name, input)| (id.as_str(), (name.as_str(), input)))
        .collect();
    let results = outputs
        .iter()
        .filter_map(|(id, out, error)| {
            let (name, input) = by_id.get(id.as_str())?;
            Some(ToolOutcome {
                call_id: id.clone(),
                name: name.to_string(),
                fingerprint: crate::loops::fingerprint(&[name, &input.to_string()]),
                input_bytes: text_len(input) as u64,
                output_bytes: text_len(out) as u64,
                error: *error,
            })
        })
        .take(MAX_RESULTS)
        .collect();
    let called: BTreeSet<String> = calls
        .into_iter()
        .map(|(_, name, _)| name)
        .filter(|n| !n.is_empty())
        .collect();
    (called.into_iter().collect(), results)
}

/// What changed since the run's previous request, in the order a prompt cache reads a request;
/// `None` for the first request of a run or when nothing before the newest messages changed.
pub fn change(prev: &HashMap<String, String>, model: &str, a: &Anatomy) -> Option<&'static str> {
    if prev.is_empty() {
        return None;
    }
    let differs = |k: &str, v: &str| prev.get(k).map(String::as_str) != Some(v);
    if differs("model", model) {
        return Some("model");
    }
    if differs("tools", &a.tools_hash) {
        return Some("tools");
    }
    if differs("system", &a.system_hash) {
        return Some("system");
    }
    if differs("params", &a.params_hash) {
        return Some("params");
    }
    if a.chained {
        return None;
    }
    let n: usize = prev.get("n").and_then(|n| n.parse().ok()).unwrap_or(0);
    if n == 0 {
        return None;
    }
    match a.message_hashes.get(n - 1) {
        Some(h) if prev.get("mh") == Some(h) => None,
        // Fewer messages than before (compacted or trimmed), or an earlier one edited.
        _ => Some("history"),
    }
}

/// Where a request happened, for [`observe`].
pub struct Seen<'a> {
    pub org: &'a str,
    pub project: &'a str,
    pub agent: &'a str,
    pub user: Option<&'a str>,
    pub run: &'a str,
    pub parent: Option<&'a str>,
    pub model: &'a str,
}

/// After a proxied call: compare with the run's previous request, remember this one, record the
/// tool names, and log the tool results it carried as steps. Returns the request event's
/// `prompt` field. Best effort: a Redis error only loses the comparison.
pub async fn observe(st: &AppState, s: &Seen<'_>, a: &Anatomy) -> Value {
    let key = format!("tl:prompt:{}:{}", s.org, s.run);
    let mut conn = st.ledger.conn();
    let prev: HashMap<String, String> = conn.hgetall(&key).await.unwrap_or_default();
    let change = change(&prev, s.model, a);
    let day = Utc::now().format("%Y%m%d").to_string();
    let n = if a.chained { 0 } else { a.message_hashes.len() };
    let mut p = redis::pipe();
    p.hset_multiple(
        &key,
        &[
            ("model", s.model.to_string()),
            ("tools", a.tools_hash.clone()),
            ("system", a.system_hash.clone()),
            ("params", a.params_hash.clone()),
            ("n", n.to_string()),
            ("mh", a.message_hashes.last().cloned().unwrap_or_default()),
        ],
    )
    .ignore()
    .expire(&key, DAY_SECS as i64)
    .ignore();
    let ttl = (TOOL_SET_DAYS * DAY_SECS) as i64;
    let agents = format!("tl:tools:agents:{}:{day}", s.org);
    p.sadd(&agents, s.agent)
        .ignore()
        .expire(&agents, ttl)
        .ignore();
    for (kind, names) in [("def", &a.defined_tools), ("called", &a.called_tools)] {
        if !names.is_empty() {
            let k = format!("tl:tools:{kind}:{}:{}:{day}", s.org, s.agent);
            p.sadd(&k, names).ignore().expire(&k, ttl).ignore();
        }
    }
    if let Err(e) = p.query_async::<()>(&mut conn).await {
        tracing::warn!(error = %e, "could not record the prompt's shape");
    }
    publish_steps(st, s, a);
    json!({
        "tool_defs": a.tool_defs,
        "deferred_tools": a.deferred_tools,
        "tool_def_tokens": a.tool_def_tokens,
        "system_tokens": a.system_tokens,
        "messages": a.message_hashes.len(),
        "change": change,
    })
}

/// The tool calls answered in this request, as a call and a result step each. Views drop them
/// for runs whose agent logs its own steps (hooks, SDK), so nothing is counted twice.
fn publish_steps(st: &AppState, s: &Seen<'_>, a: &Anatomy) {
    let place = crate::steps::Place {
        org: s.org,
        project: s.project,
        agent: s.agent,
        user: s.user,
        run: s.run,
        parent: s.parent,
        source: "proxy",
    };
    for r in &a.results {
        let mut call = crate::steps::Step::new(crate::steps::Kind::ToolCall);
        call.name = Some(r.name.clone());
        call.call_id = Some(r.call_id.clone()).filter(|c| !c.is_empty());
        call.fingerprint = Some(r.fingerprint.clone());
        call.size = Some(r.input_bytes);
        let mut result = crate::steps::Step::new(crate::steps::Kind::ToolResult);
        result.name = call.name.clone();
        result.call_id = call.call_id.clone();
        result.status = Some(if r.error { "error" } else { "ok" });
        result.size = Some(r.output_bytes);
        for step in [call, result] {
            crate::metrics::publish(
                &st.activity,
                crate::metrics::Path::Proxy,
                crate::steps::event(&place, &step),
            );
        }
    }
}

/// Per agent, the tools defined and the tools called over the last `days` days (at most a week).
pub async fn tool_usage(
    st: &AppState,
    org: &str,
    days: u32,
) -> BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> {
    let mut out = BTreeMap::new();
    let mut conn = st.ledger.conn();
    let today = Utc::now().date_naive();
    let days: Vec<String> = (0..days.clamp(1, 7))
        .map(|d| {
            (today - chrono::Duration::days(i64::from(d)))
                .format("%Y%m%d")
                .to_string()
        })
        .collect();
    let mut agents = BTreeSet::new();
    for d in &days {
        let found: Vec<String> = conn
            .smembers(format!("tl:tools:agents:{org}:{d}"))
            .await
            .unwrap_or_default();
        agents.extend(found);
    }
    for agent in agents {
        let keys = |kind: &str| -> Vec<String> {
            days.iter()
                .map(|d| format!("tl:tools:{kind}:{org}:{agent}:{d}"))
                .collect()
        };
        let defined: BTreeSet<String> = conn.sunion(keys("def")).await.unwrap_or_default();
        let called: BTreeSet<String> = conn.sunion(keys("called")).await.unwrap_or_default();
        if !defined.is_empty() {
            out.insert(agent, (defined, called));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn anthropic_parts_calls_and_results() {
        let b = body(json!({
            "model": "claude-opus-5-5",
            "system": "You are a careful engineer.",
            "tools": [
                {"name": "Bash", "description": "Run a command", "input_schema": {"type": "object"}},
                {"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}},
                {"name": "Rare", "defer_loading": true, "input_schema": {"type": "object"}},
            ],
            "messages": [
                {"role": "user", "content": "fix the tests"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Running them."},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "make test"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "1 failed"},
                ]},
            ],
        }));
        let a = anatomy(Api::AnthropicMessages, &b);
        assert_eq!((a.tool_defs, a.deferred_tools), (2, 1));
        assert!(a.tool_def_tokens > 10);
        assert_eq!(a.defined_tools, ["Bash", "Read", "Rare"]);
        assert_eq!(a.called_tools, ["Bash"]);
        assert_eq!(a.message_hashes.len(), 3);
        assert_eq!(a.results.len(), 1);
        let r = &a.results[0];
        assert_eq!(
            (r.name.as_str(), r.call_id.as_str(), r.error),
            ("Bash", "t1", true)
        );
        assert_eq!(r.output_bytes, 8);
        // Same tool and input as an SDK-logged call: the same fingerprint.
        assert_eq!(
            r.fingerprint,
            crate::loops::fingerprint(&["Bash", r#"{"cmd":"make test"}"#])
        );
    }

    #[test]
    fn openai_chat_and_responses_tool_turns() {
        let chat = body(json!({
            "model": "gpt-6.2-sol",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Pune\"}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "31C"},
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {}}}],
        }));
        let a = anatomy(Api::OpenAIChat, &chat);
        assert_eq!(a.system_tokens, 3);
        assert_eq!(a.message_hashes.len(), 3);
        assert_eq!(a.called_tools, ["get_weather"]);
        assert_eq!(a.results[0].output_bytes, 3);
        let responses = body(json!({
            "model": "gpt-6.2-sol",
            "instructions": "Be brief.",
            "input": [
                {"role": "user", "content": "weather?"},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "31C"},
            ],
        }));
        let a = anatomy(Api::OpenAIResponses, &responses);
        assert!(!a.chained);
        assert_eq!(a.results.len(), 1);
        assert_eq!(a.results[0].name, "get_weather");
        // Chained to a stored response: the call isn't in the request, so no step.
        let chained = body(
            json!({"model": "gpt-6.2-sol", "previous_response_id": "resp_1",
            "input": [{"type": "function_call_output", "call_id": "c9", "output": "ok"}]}),
        );
        let a = anatomy(Api::OpenAIResponses, &chained);
        assert!(a.chained && a.results.is_empty());
    }

    #[test]
    fn changes_are_named_in_cache_order() {
        let first = body(json!({
            "model": "claude-opus-5-5", "system": "rules",
            "tools": [{"name": "Bash"}],
            "messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}],
        }));
        let a = anatomy(Api::AnthropicMessages, &first);
        let prev: HashMap<String, String> = HashMap::from([
            ("model".into(), "claude-opus-5-5".into()),
            ("tools".into(), a.tools_hash.clone()),
            ("system".into(), a.system_hash.clone()),
            ("params".into(), a.params_hash.clone()),
            ("n".into(), a.message_hashes.len().to_string()),
            ("mh".into(), a.message_hashes.last().cloned().unwrap()),
        ]);
        let m = "claude-opus-5-5";
        assert_eq!(change(&HashMap::new(), m, &a), None);
        // Appending a turn changes nothing a cache depends on.
        let mut next = first.clone();
        next["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "user", "content": "c"}));
        assert_eq!(
            change(&prev, m, &anatomy(Api::AnthropicMessages, &next)),
            None
        );
        // Key order alone changes nothing either.
        let reordered: Map<String, Value> = serde_json::from_str(
            r#"{"tools":[{"name":"Bash"}],"messages":[{"content":"a","role":"user"},
                {"content":"b","role":"assistant"}],"system":"rules","model":"claude-opus-5-5"}"#,
        )
        .unwrap();
        assert_eq!(
            change(&prev, m, &anatomy(Api::AnthropicMessages, &reordered)),
            None
        );
        let with = |k: &str, v: Value| {
            let mut b = first.clone();
            b.insert(k.into(), v);
            anatomy(Api::AnthropicMessages, &b)
        };
        let tools = with("tools", json!([{"name": "Bash"}, {"name": "Read"}]));
        assert_eq!(change(&prev, m, &tools), Some("tools"));
        let system = with("system", json!("rules, 10:42"));
        assert_eq!(change(&prev, m, &system), Some("system"));
        let thinking = with("thinking", json!({"type": "enabled"}));
        assert_eq!(change(&prev, m, &thinking), Some("params"));
        assert_eq!(change(&prev, "claude-sonnet-5-5", &a), Some("model"));
        // An earlier message edited, or the conversation compacted.
        let edited = with(
            "messages",
            json!([{"role": "user", "content": "A"}, {"role": "assistant", "content": "b"},
                {"role": "user", "content": "c"}]),
        );
        assert_eq!(change(&prev, m, &edited), Some("history"));
        let shorter = with("messages", json!([{"role": "user", "content": "summary"}]));
        assert_eq!(change(&prev, m, &shorter), Some("history"));
    }
}
