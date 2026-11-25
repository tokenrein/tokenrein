//! Request changes the proxy makes to save money, each opt-in per project (`optimize`):
//!
//! - **Prompt caching a client forgot.** An Anthropic request with no `cache_control` anywhere
//!   pays full price for its whole context on every call; one top-level `cache_control` turns on
//!   automatic caching of the growing conversation. OpenAI caches on its own, and a stable
//!   `prompt_cache_key` sends a run's calls to the same cache.
//! - **Cheaper models for simple steps.** A call with no tools, a short prompt and a short answer
//!   (titles, summaries, classifications) goes to the model a project route names instead.
//!
//! And, for the loop guard, a nudge: a short note appended at the end of the conversation, where
//! it leaves the cached prefix alone.

use serde_json::{Map, Value, json};

use crate::config::Optimize;
use crate::providers::Api;

/// Prompts smaller than this are not worth caching (the providers' minimum is 512 to 4,096).
const MIN_CACHE_TOKENS: u64 = 1_024;
/// A step is simple when its prompt is at most this big...
const SIMPLE_MAX_PROMPT: u64 = 2_000;
/// ...and its answer at most this long.
const SIMPLE_MAX_OUTPUT: u64 = 1_024;

/// What the proxy changed in a request.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Applied {
    pub cache: bool,
    pub routed_from: Option<String>,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        !self.cache && self.routed_from.is_none()
    }

    pub fn to_json(&self) -> Value {
        json!({ "cache": self.cache, "routed_from": self.routed_from })
    }
}

fn has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(m) => m.contains_key(key) || m.values().any(|x| has_key(x, key)),
        Value::Array(a) => a.iter().any(|x| has_key(x, key)),
        _ => false,
    }
}

/// `model` is `prefix` or a dated or tagged version of it.
fn model_matches(model: &str, prefix: &str) -> bool {
    model
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', ':', '@']))
}

/// No tools, a short prompt, a short answer, and nothing a cheaper model may not support.
fn simple(body: &Map<String, Value>, prompt_tokens: u64, max_output: u64) -> bool {
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    let special = [
        "thinking",
        "reasoning",
        "reasoning_effort",
        "previous_response_id",
    ]
    .iter()
    .any(|k| body.get(*k).is_some_and(|v| !v.is_null()));
    !tools && !special && prompt_tokens <= SIMPLE_MAX_PROMPT && max_output <= SIMPLE_MAX_OUTPUT
}

/// Apply a project's optimisations to a parsed request body. `cache_key` names the run's agent,
/// so its calls share OpenAI's cache.
pub fn apply(
    api: Api,
    body: &mut Map<String, Value>,
    opt: &Optimize,
    prompt_tokens: u64,
    max_output: u64,
    cache_key: &str,
) -> Applied {
    let mut done = Applied::default();
    if opt.cache && prompt_tokens >= MIN_CACHE_TOKENS {
        match api {
            Api::AnthropicMessages => {
                if !body.values().any(|v| has_key(v, "cache_control"))
                    && !body.contains_key("cache_control")
                {
                    body.insert("cache_control".into(), json!({"type": "ephemeral"}));
                    done.cache = true;
                }
            }
            Api::OpenAIChat | Api::OpenAIResponses => {
                if !body.contains_key("prompt_cache_key") {
                    body.insert("prompt_cache_key".into(), json!(cache_key));
                    done.cache = true;
                }
            }
        }
    }
    if !opt.routes.is_empty() && simple(body, prompt_tokens, max_output) {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(r) = opt.routes.iter().find(|r| model_matches(&model, &r.from)) {
            body.insert("model".into(), json!(r.to));
            done.routed_from = Some(model);
        }
    }
    done
}

/// The request with `note` added as the latest user text, or `None` when it has no conversation
/// to add to.
pub fn with_note(api: Api, raw: &[u8], note: &str) -> Option<Vec<u8>> {
    let mut body: Map<String, Value> = serde_json::from_slice(raw).ok()?;
    match api {
        Api::AnthropicMessages => {
            let messages = body.get_mut("messages")?.as_array_mut()?;
            let last_is_user = messages.last()?.get("role").and_then(Value::as_str) == Some("user");
            if !last_is_user {
                messages.push(json!({"role": "user", "content": note}));
            } else {
                let last = messages.last_mut()?;
                let text = json!({"type": "text", "text": note});
                match last.get_mut("content")? {
                    Value::Array(blocks) => blocks.push(text),
                    Value::String(s) => {
                        let first = json!({"type": "text", "text": s.clone()});
                        last["content"] = json!([first, text]);
                    }
                    _ => return None,
                }
            }
        }
        Api::OpenAIChat => {
            let messages = body.get_mut("messages")?.as_array_mut()?;
            messages.push(json!({"role": "user", "content": note}));
        }
        Api::OpenAIResponses => {
            let item = json!({"role": "user", "content": note});
            match body.get_mut("input") {
                Some(Value::Array(items)) => items.push(item),
                Some(Value::String(s)) => {
                    let first = json!({"role": "user", "content": s.clone()});
                    body.insert("input".into(), json!([first, item]));
                }
                _ => return None,
            }
        }
    }
    serde_json::to_vec(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelRoute;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn opt(cache: bool, routes: &[(&str, &str)]) -> Optimize {
        Optimize {
            cache,
            routes: routes
                .iter()
                .map(|(f, t)| ModelRoute {
                    from: f.to_string(),
                    to: t.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn caching_is_turned_on_only_when_missing() {
        let o = opt(true, &[]);
        let mut b = body(json!({"model": "claude-opus-5-5", "messages": []}));
        let done = apply(Api::AnthropicMessages, &mut b, &o, 30_000, 1_000, "k");
        assert!(done.cache);
        assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
        // A client that set its own breakpoint is left alone.
        let mut b = body(json!({"model": "claude-opus-5-5", "system": [
            {"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}]}));
        assert!(!apply(Api::AnthropicMessages, &mut b, &o, 30_000, 1_000, "k").cache);
        assert!(b.get("cache_control").is_none());
        // Too small to cache.
        let mut b = body(json!({"model": "claude-opus-5-5"}));
        assert!(!apply(Api::AnthropicMessages, &mut b, &o, 500, 1_000, "k").cache);
        // OpenAI: a routing key, unless the client sent one.
        let mut b = body(json!({"model": "gpt-6.2-sol"}));
        assert!(
            apply(
                Api::OpenAIResponses,
                &mut b,
                &o,
                30_000,
                1_000,
                "tr-acme-coder"
            )
            .cache
        );
        assert_eq!(b["prompt_cache_key"], "tr-acme-coder");
        let mut b = body(json!({"model": "gpt-6.2-sol", "prompt_cache_key": "mine"}));
        assert!(!apply(Api::OpenAIChat, &mut b, &o, 30_000, 1_000, "k").cache);
        // Off unless asked for.
        let mut b = body(json!({"model": "claude-opus-5-5"}));
        let off = opt(false, &[]);
        assert!(apply(Api::AnthropicMessages, &mut b, &off, 30_000, 1_000, "k").is_empty());
    }

    #[test]
    fn only_simple_steps_are_routed() {
        let o = opt(false, &[("claude-opus-5-5", "claude-haiku-5-5")]);
        let mut b = body(json!({"model": "claude-opus-5-5-20261101", "messages": []}));
        let done = apply(Api::AnthropicMessages, &mut b, &o, 800, 300, "k");
        assert_eq!(
            done.routed_from.as_deref(),
            Some("claude-opus-5-5-20261101")
        );
        assert_eq!(b["model"], "claude-haiku-5-5");
        // Tools, a long prompt, a long answer, thinking, or another model: untouched.
        for (extra, prompt, out) in [
            (json!({"tools": [{"name": "Bash"}]}), 800, 300),
            (json!({}), 9_000, 300),
            (json!({}), 800, 4_000),
            (json!({"thinking": {"type": "enabled"}}), 800, 300),
        ] {
            let mut b = body(json!({"model": "claude-opus-5-5"}));
            b.extend(extra.as_object().unwrap().clone());
            assert!(apply(Api::AnthropicMessages, &mut b, &o, prompt, out, "k").is_empty());
            assert_eq!(b["model"], "claude-opus-5-5");
        }
        let mut b = body(json!({"model": "claude-opus-5-55"}));
        assert!(apply(Api::AnthropicMessages, &mut b, &o, 800, 300, "k").is_empty());
    }

    #[test]
    fn notes_go_at_the_end_of_the_conversation() {
        let note = "[Tokenrein] try another approach";
        let parse = |v: Vec<u8>| serde_json::from_slice::<Value>(&v).unwrap();
        let with = |api: Api, raw: &Value| with_note(api, raw.to_string().as_bytes(), note);
        // Anthropic, after tool results: one more text block in the same user turn.
        let raw = json!({"model": "m", "messages": [
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "fail"}]},
        ]});
        let out = parse(with(Api::AnthropicMessages, &raw).unwrap());
        let last = &out["messages"][2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1], json!({"type": "text", "text": note}));
        // Earlier messages are untouched, so the cached prefix still matches.
        assert_eq!(out["messages"][0], raw["messages"][0]);
        // A plain-text user turn becomes two text blocks.
        let raw = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let out = parse(with(Api::AnthropicMessages, &raw).unwrap());
        assert_eq!(out["messages"][0]["content"][1]["text"], note);
        // OpenAI chat and Responses: a user message at the end.
        let raw = json!({"model": "m", "messages": [{"role": "tool", "tool_call_id": "c", "content": "x"}]});
        let out = parse(with(Api::OpenAIChat, &raw).unwrap());
        assert_eq!(out["messages"][1], json!({"role": "user", "content": note}));
        let raw = json!({"model": "m", "input": "hello"});
        let out = parse(with(Api::OpenAIResponses, &raw).unwrap());
        assert_eq!(out["input"][1]["content"], note);
        assert!(with(Api::OpenAIChat, &json!({"model": "m"})).is_none());
    }
}
