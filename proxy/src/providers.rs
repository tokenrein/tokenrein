use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    OpenAI,
    Anthropic,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenAI => "openai",
            Provider::Anthropic => "anthropic",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Provider::OpenAI),
            "anthropic" => Some(Provider::Anthropic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    OpenAIChat,
    OpenAIResponses,
    AnthropicMessages,
}

impl Api {
    pub fn provider(self) -> Provider {
        match self {
            Api::OpenAIChat | Api::OpenAIResponses => Provider::OpenAI,
            Api::AnthropicMessages => Provider::Anthropic,
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Api::OpenAIChat => "/v1/chat/completions",
            Api::OpenAIResponses => "/v1/responses",
            Api::AnthropicMessages => "/v1/messages",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input: u64,
    pub cached_input: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Debug)]
pub struct Prepared {
    pub model: String,
    pub stream: bool,
    pub body: Vec<u8>,
    pub input_tokens_est: u64,
    pub max_output_tokens: u64,
    pub previous_response_id: Option<String>,
}

const IMAGE_TOKENS: u64 = 1_600;
const IMAGE_TYPES: [&str; 3] = ["image_url", "image", "input_image"];
const BASE64_MIN_LEN: usize = 20_000;
const REQUEST_OVERHEAD_TOKENS: u64 = 50;

/// Deliberately pessimistic: ~2.5 bytes per token over-counts English (~4) and roughly matches CJK.
pub fn estimate_tokens_from_bytes(bytes: u64) -> u64 {
    (bytes * 2).div_ceil(5)
}

pub fn prepare(api: Api, raw: &[u8], default_max_out: u64) -> Result<Prepared, String> {
    let mut body: Map<String, Value> = serde_json::from_slice(raw)
        .map_err(|e| format!("request body must be a JSON object: {e}"))?;

    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .ok_or("missing \"model\"")?
        .to_string();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let out_fields: &[&str] = match api {
        Api::OpenAIChat => &["max_completion_tokens", "max_tokens"],
        Api::OpenAIResponses => &["max_output_tokens"],
        Api::AnthropicMessages => &["max_tokens"],
    };
    let mut max_out = None;
    for field in out_fields {
        match body.get(*field) {
            None | Some(Value::Null) => continue,
            Some(v) => {
                let n = v
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("\"{field}\" must be a positive integer"))?;
                max_out = Some(n);
                break;
            }
        }
    }
    // Without an output cap the worst-case cost is unbounded, so the proxy always sets one.
    let max_out = match max_out {
        Some(n) => n,
        None => {
            body.insert(out_fields[0].to_string(), json!(default_max_out));
            default_max_out
        }
    };

    let choices = match api {
        Api::OpenAIChat => body.get("n").and_then(Value::as_u64).unwrap_or(1).max(1),
        _ => 1,
    };

    if api == Api::OpenAIChat && stream {
        match body.entry("stream_options").or_insert_with(|| json!({})) {
            Value::Object(opts) => {
                opts.insert("include_usage".into(), json!(true));
            }
            _ => return Err("\"stream_options\" must be an object".into()),
        }
    }

    let previous_response_id = match api {
        Api::OpenAIResponses => body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    };

    let mut bytes = 0u64;
    let mut extra_tokens = 0u64;
    for (key, value) in &body {
        if key != "model" {
            measure(value, &mut bytes, &mut extra_tokens);
        }
    }
    let input_tokens_est =
        estimate_tokens_from_bytes(bytes) + extra_tokens + REQUEST_OVERHEAD_TOKENS;

    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    Ok(Prepared {
        model,
        stream,
        body,
        input_tokens_est,
        max_output_tokens: max_out.saturating_mul(choices),
        previous_response_id,
    })
}

fn measure(v: &Value, bytes: &mut u64, extra_tokens: &mut u64) {
    match v {
        Value::String(s) => match base64_payload(s) {
            Some(payload) => *extra_tokens += (payload.len() / 6) as u64,
            None => *bytes += s.len() as u64,
        },
        Value::Array(items) => items.iter().for_each(|x| measure(x, bytes, extra_tokens)),
        Value::Object(obj) => {
            let is_image = obj
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| IMAGE_TYPES.contains(&t));
            if is_image {
                *extra_tokens += IMAGE_TOKENS;
                return;
            }
            obj.values().for_each(|x| measure(x, bytes, extra_tokens));
        }
        Value::Number(_) => *bytes += 4,
        Value::Bool(_) | Value::Null => {}
    }
}

fn base64_payload(s: &str) -> Option<&str> {
    if s.len() < BASE64_MIN_LEN {
        return None;
    }
    let payload = match s.strip_prefix("data:") {
        Some(rest) => rest.split_once(',')?.1,
        None => s,
    };
    payload
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n' | b'\r'))
        .then_some(payload)
}

fn num(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for key in path {
        match cur.get(key) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0)
}

fn openai_chat_usage(us: &Value) -> Option<Usage> {
    if !us.is_object() {
        return None;
    }
    let prompt = num(us, &["prompt_tokens"]);
    let cached = num(us, &["prompt_tokens_details", "cached_tokens"]).min(prompt);
    Some(Usage {
        input: prompt - cached,
        cached_input: cached,
        cache_write: 0,
        output: num(us, &["completion_tokens"]),
    })
}

fn responses_usage(us: &Value) -> Option<Usage> {
    if !us.is_object() {
        return None;
    }
    let input = num(us, &["input_tokens"]);
    let cached = num(us, &["input_tokens_details", "cached_tokens"]).min(input);
    Some(Usage {
        input: input - cached,
        cached_input: cached,
        cache_write: 0,
        output: num(us, &["output_tokens"]),
    })
}

fn anthropic_usage(us: &Value) -> Option<Usage> {
    if !us.is_object() {
        return None;
    }
    Some(Usage {
        input: num(us, &["input_tokens"]),
        cached_input: num(us, &["cache_read_input_tokens"]),
        cache_write: num(us, &["cache_creation_input_tokens"]),
        output: num(us, &["output_tokens"]),
    })
}

pub fn usage_from_body(api: Api, v: &Value) -> Option<Usage> {
    let us = v.get("usage")?;
    match api {
        Api::OpenAIChat => openai_chat_usage(us),
        Api::OpenAIResponses => responses_usage(us),
        Api::AnthropicMessages => anthropic_usage(us),
    }
}

pub fn response_id(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

#[derive(Debug, Default)]
pub struct StreamResult {
    pub usage: Option<Usage>,
    pub complete: bool,
    pub approx_output_tokens: u64,
    pub response_id: Option<String>,
}

const MAX_PENDING_LINE: usize = 4 * 1024 * 1024;

pub struct StreamTracker {
    api: Api,
    pending: Vec<u8>,
    out_bytes: u64,
    result: StreamResult,
}

impl StreamTracker {
    pub fn new(api: Api) -> Self {
        Self {
            api,
            pending: Vec::new(),
            out_bytes: 0,
            result: StreamResult::default(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(pos) = data[start..].iter().position(|&b| b == b'\n') {
            self.handle_line(&data[start..start + pos]);
            start += pos + 1;
        }
        data.drain(..start);
        if data.len() > MAX_PENDING_LINE {
            data.clear();
        }
        self.pending = data;
    }

    fn handle_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = payload.trim_ascii();
        if payload.is_empty() || payload == b"[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(payload) else {
            return;
        };
        match self.api {
            Api::OpenAIChat => {
                if let Some(choices) = v.get("choices").and_then(Value::as_array) {
                    for choice in choices {
                        if let Some(delta) = choice.get("delta") {
                            self.out_bytes += string_bytes(delta);
                        }
                    }
                }
                if let Some(u) = v.get("usage").and_then(openai_chat_usage) {
                    self.result.usage = Some(u);
                    self.result.complete = true;
                }
            }
            Api::OpenAIResponses => {
                let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                if kind.ends_with(".delta")
                    && let Some(delta) = v.get("delta")
                {
                    self.out_bytes += string_bytes(delta);
                }
                if let Some(resp) = v.get("response") {
                    if let Some(id) = resp.get("id").and_then(Value::as_str) {
                        self.result.response_id = Some(id.to_string());
                    }
                    if let Some(u) = resp.get("usage").and_then(responses_usage) {
                        self.result.usage = Some(u);
                        if matches!(
                            kind,
                            "response.completed" | "response.incomplete" | "response.failed"
                        ) {
                            self.result.complete = true;
                        }
                    }
                }
            }
            Api::AnthropicMessages => match v.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    if let Some(u) = v.pointer("/message/usage").and_then(anthropic_usage) {
                        self.result.usage = Some(u);
                    }
                }
                Some("content_block_delta") => {
                    if let Some(delta) = v.get("delta") {
                        self.out_bytes += string_bytes(delta);
                    }
                }
                Some("message_delta") => {
                    if let Some(us) = v.get("usage") {
                        let mut cur = self.result.usage.unwrap_or_default();
                        cur.output = cur.output.max(num(us, &["output_tokens"]));
                        cur.input = cur.input.max(num(us, &["input_tokens"]));
                        cur.cached_input =
                            cur.cached_input.max(num(us, &["cache_read_input_tokens"]));
                        cur.cache_write = cur
                            .cache_write
                            .max(num(us, &["cache_creation_input_tokens"]));
                        self.result.usage = Some(cur);
                        self.result.complete = true;
                    }
                }
                _ => {}
            },
        }
    }

    pub fn finish(mut self) -> StreamResult {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.handle_line(&rest);
        }
        self.result.approx_output_tokens = estimate_tokens_from_bytes(self.out_bytes);
        self.result
    }
}

fn string_bytes(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.len() as u64,
        Value::Array(items) => items.iter().map(string_bytes).sum(),
        Value::Object(obj) => obj.values().map(string_bytes).sum(),
        _ => 0,
    }
}

pub fn error_body(api: Api, kind: &str, message: &str) -> Value {
    match api.provider() {
        Provider::OpenAI => json!({
            "error": { "message": message, "type": kind, "code": kind, "param": null }
        }),
        Provider::Anthropic => json!({
            "type": "error",
            "error": { "type": kind, "message": message }
        }),
    }
}

const PASSTHROUGH_HEADERS: [&str; 5] = [
    "anthropic-version",
    "anthropic-beta",
    "anthropic-workspace-id",
    "openai-beta",
    "idempotency-key",
];

pub fn upstream_headers(api: Api, client: &HeaderMap, api_key: &str) -> Result<HeaderMap, String> {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for name in PASSTHROUGH_HEADERS {
        if let Some(v) = client.get(name) {
            h.insert(HeaderName::from_static(name), v.clone());
        }
    }
    let invalid = |_| "provider API key contains invalid characters".to_string();
    match api.provider() {
        Provider::OpenAI => {
            let mut v = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(invalid)?;
            v.set_sensitive(true);
            h.insert(AUTHORIZATION, v);
        }
        Provider::Anthropic => {
            let mut v = HeaderValue::from_str(api_key).map_err(invalid)?;
            v.set_sensitive(true);
            h.insert(HeaderName::from_static("x-api-key"), v);
            if !h.contains_key("anthropic-version") {
                h.insert(
                    HeaderName::from_static("anthropic-version"),
                    HeaderValue::from_static("2023-06-01"),
                );
            }
        }
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    #[test]
    fn injects_output_cap_and_stream_usage_for_chat() {
        let raw =
            br#"{"model":"gpt-4o-mini","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let p = prepare(Api::OpenAIChat, raw, 1000).unwrap();
        let body = parse(&p.body);
        assert_eq!(body["max_completion_tokens"], 1000);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(p.max_output_tokens, 1000);
        assert!(p.stream);
    }

    #[test]
    fn respects_client_cap_and_multiplies_by_n() {
        let raw = br#"{"model":"gpt-4o","n":3,"max_tokens":200,"messages":[]}"#;
        let p = prepare(Api::OpenAIChat, raw, 1000).unwrap();
        assert_eq!(p.max_output_tokens, 600);
        assert!(parse(&p.body).get("max_completion_tokens").is_none());
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(prepare(Api::OpenAIChat, b"[]", 10).is_err());
        assert!(prepare(Api::OpenAIChat, br#"{"messages":[]}"#, 10).is_err());
        assert!(
            prepare(
                Api::AnthropicMessages,
                br#"{"model":"m","max_tokens":0}"#,
                10
            )
            .is_err()
        );
    }

    #[test]
    fn images_count_as_fixed_tokens_not_base64_length() {
        let blob = "A".repeat(500_000);
        let raw = serde_json::to_vec(&json!({
            "model": "claude-sonnet-4",
            "max_tokens": 10,
            "messages": [{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": blob}},
                {"type":"text","text":"what is this?"}
            ]}]
        }))
        .unwrap();
        let p = prepare(Api::AnthropicMessages, &raw, 10).unwrap();
        assert!(p.input_tokens_est < 2_000, "{}", p.input_tokens_est);
    }

    #[test]
    fn estimate_is_pessimistic_for_text() {
        let text = "word ".repeat(4_000);
        let raw = serde_json::to_vec(
            &json!({"model":"gpt-4o","messages":[{"role":"user","content":text}]}),
        )
        .unwrap();
        let p = prepare(Api::OpenAIChat, &raw, 10).unwrap();
        assert!(p.input_tokens_est >= 8_000, "{}", p.input_tokens_est);
    }

    #[test]
    fn openai_stream_usage_and_split_lines() {
        let mut t = StreamTracker::new(Api::OpenAIChat);
        t.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel");
        t.feed(b"lo\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"prompt_tokens_details\":{\"cached_tokens\":20}}}\n\ndata: [DONE]\n\n");
        let r = t.finish();
        assert!(r.complete);
        assert_eq!(
            r.usage,
            Some(Usage {
                input: 80,
                cached_input: 20,
                cache_write: 0,
                output: 50
            })
        );
        assert!(r.approx_output_tokens > 0);
    }

    #[test]
    fn anthropic_stream_usage() {
        let mut t = StreamTracker::new(Api::AnthropicMessages);
        t.feed(b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":200,\"output_tokens\":1,\"cache_read_input_tokens\":30}}}\r\n\r\n");
        let partial = StreamTracker::new(Api::AnthropicMessages).finish();
        assert!(!partial.complete);
        t.feed(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n");
        t.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":80}}\n\n");
        let r = t.finish();
        assert!(r.complete);
        assert_eq!(
            r.usage,
            Some(Usage {
                input: 200,
                cached_input: 30,
                cache_write: 0,
                output: 80
            })
        );
    }

    #[test]
    fn responses_stream_usage_and_id() {
        let mut t = StreamTracker::new(Api::OpenAIResponses);
        t.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n");
        t.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n");
        let r = t.finish();
        assert!(r.complete);
        assert_eq!(r.response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            r.usage,
            Some(Usage {
                input: 6,
                cached_input: 4,
                cache_write: 0,
                output: 5
            })
        );
    }

    #[test]
    fn error_bodies_match_provider_shapes() {
        let o = error_body(Api::OpenAIChat, "tokenlens_budget_exceeded", "x");
        assert_eq!(o["error"]["type"], "tokenlens_budget_exceeded");
        let a = error_body(Api::AnthropicMessages, "tokenlens_budget_exceeded", "x");
        assert_eq!(a["type"], "error");
        assert_eq!(a["error"]["type"], "tokenlens_budget_exceeded");
    }
}
