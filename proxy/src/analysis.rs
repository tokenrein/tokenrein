//! Fix suggestions: one Insights finding, explained and turned into a concrete plan by a model the
//! operator chooses.
//!
//! The model sits behind any OpenAI-compatible endpoint (`[analysis]`: LiteLLM, a vLLM or Ollama
//! server, OpenAI, a company gateway), called with its own key, never an agent's. Tokenrein builds
//! what is sent itself, from its own metrics: counts, rates, token totals and costs, never prompts
//! or tool text, and returns it with the answer so people can see exactly what left. Spend is
//! capped per calendar month. A suggestion is advice: nothing is changed automatically.

use std::time::Duration;

use chrono::Utc;
use redis::AsyncCommands;
use serde_json::{Value, json};

use crate::config::{Resolved, usd_to_micros};
use crate::providers::Usage;
use crate::proxy::AppState;

const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_OUTPUT_TOKENS: u64 = 1_000;

const SYSTEM: &str = "You are an expert in AI agent tooling, prompt design and LLM cost. You get one \
finding from Tokenrein, a spend guardrail for AI agents: the rule that fired, metrics computed from \
agent traces (counts, rates, token totals and costs; no prompt or tool text) and the fix Tokenrein \
suggests. Write a plan the team can act on: (1) the most likely root causes, given the metrics; \
(2) specific changes, such as wording to add to a tool description or input schema, an error \
message format, a prompt layout or a cache setting, with a short example each; (3) how to tell it \
worked, naming the Tokenrein metric that should move. Under 250 words, Markdown bullet lists. Use \
only numbers from the input, and say so when the metrics can't tell.";

/// Where suggestions come from, when `[analysis]` is set.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub budget_micros: u64,
}

impl Endpoint {
    /// The host only, for the dashboard and the audit log.
    pub fn host(&self) -> &str {
        let rest = self
            .base_url
            .split_once("://")
            .map_or(self.base_url.as_str(), |(_, r)| r);
        rest.split(['/', '?']).next().unwrap_or(rest)
    }
}

pub fn endpoint(rt: &Resolved) -> Option<Endpoint> {
    let a = &rt.cfg.analysis;
    let base_url = a
        .base_url
        .as_deref()
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| !u.is_empty())?;
    let model = a
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())?;
    Some(Endpoint {
        base_url: base_url.to_string(),
        model: model.to_string(),
        api_key: rt.env_var(&a.api_key_env).filter(|k| !k.trim().is_empty()),
        budget_micros: usd_to_micros(a.monthly_budget_usd),
    })
}

#[derive(Debug)]
pub enum Error {
    /// This month's budget is used up.
    Budget { spent: u64, budget: u64 },
    /// The endpoint failed or answered something unusable.
    Endpoint(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Budget { spent, budget } => write!(
                f,
                "this month's suggestion budget is used up (${:.2} of ${:.2}); raise \
                 analysis.monthly_budget_usd to allow more",
                *spent as f64 / 1e6,
                *budget as f64 / 1e6
            ),
            Error::Endpoint(m) => write!(f, "the analysis endpoint failed: {m}"),
        }
    }
}

/// One answer.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub text: String,
    pub cost_micros: u64,
    pub spent_micros: u64,
}

fn month_key() -> String {
    format!("tl:analysis:spent:{}", Utc::now().format("%Y%m"))
}

/// Spent on suggestions this calendar month, all orgs together.
pub async fn spent(st: &AppState) -> u64 {
    let mut conn = st.ledger.conn();
    conn.get::<_, Option<u64>>(month_key())
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// What is sent for `finding`: the finding and the report rows about the same tool, agent and
/// model. Built from Tokenrein's own metrics only.
pub fn package(report: &Value, finding: &Value) -> Value {
    let pick = |list: &Value, key: &str, want: &Value| -> Value {
        if want.is_null() {
            return Value::Null;
        }
        list.as_array()
            .and_then(|rows| rows.iter().find(|r| &r[key] == want))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let rule = finding["rule"].as_str().unwrap_or("");
    json!({
        "finding": {
            "rule": finding["rule"], "title": finding["title"], "detail": finding["detail"],
            "tokenrein_fix": finding["fix"], "agent": finding["agent"], "model": finding["model"],
            "tool": finding["tool"],
            "estimated_cost_usd": finding["impact_micros"].as_u64().map(|m| m as f64 / 1e6),
            "estimated_tokens": finding["impact_tokens"],
        },
        "window_days": report["days"],
        "tool_metrics": pick(&report["tools"]["by_tool"], "tool", &finding["tool"]),
        "agent_cache_metrics": pick(&report["cache"]["by_agent"], "agent", &finding["agent"]),
        "model_cache_metrics": pick(&report["cache"]["by_model"], "model", &finding["model"]),
        "cache_miss_causes": if rule.starts_with("cache.") {
            report["cache"]["causes"].clone()
        } else {
            Value::Null
        },
        "units": "Token counts are tokens; *_micros are millionths of a US dollar; rates are 0-1.",
    })
}

/// Ask the endpoint about one finding. Charges the month's budget with what it cost.
pub async fn suggest(st: &AppState, ep: &Endpoint, package: &Value) -> Result<Suggestion, Error> {
    let already = spent(st).await;
    if ep.budget_micros == 0 || already >= ep.budget_micros {
        return Err(Error::Budget {
            spent: already,
            budget: ep.budget_micros,
        });
    }
    let body = json!({
        "model": ep.model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": serde_json::to_string_pretty(package).unwrap_or_default()},
        ],
        "max_tokens": MAX_OUTPUT_TOKENS,
        "temperature": 0.2,
    });
    let mut req = st
        .http
        .post(format!("{}/chat/completions", ep.base_url))
        .timeout(TIMEOUT)
        .json(&body);
    if let Some(k) = &ep.api_key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Error::Endpoint(e.without_url().to_string()))?;
    let status = resp.status();
    // LiteLLM reports what the call cost; otherwise Tokenrein prices the tokens itself.
    let reported_cost = resp
        .headers()
        .get("x-litellm-response-cost")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(usd_to_micros);
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Endpoint(format!("not JSON ({status}): {}", e.without_url())))?;
    if !status.is_success() {
        let msg = v["error"]["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("no message");
        return Err(Error::Endpoint(format!("{status}: {}", clip(msg, 300))));
    }
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::Endpoint("the answer had no text".into()))?
        .to_string();
    let usage =
        crate::providers::usage_from_body(crate::providers::Api::OpenAIChat, &v).unwrap_or(Usage {
            output: MAX_OUTPUT_TOKENS,
            ..Usage::default()
        });
    let cost = reported_cost.unwrap_or_else(|| {
        st.rt()
            .prices
            .lookup_any("", &ep.model)
            .0
            .usage_cost(&usage)
    });
    let key = month_key();
    let mut conn = st.ledger.conn();
    let spent_now: u64 = redis::pipe()
        .incr(&key, cost)
        .expire(&key, 40 * 86_400)
        .ignore()
        .query_async::<(u64,)>(&mut conn)
        .await
        .map(|(n,)| n)
        .unwrap_or(already + cost);
    Ok(Suggestion {
        text,
        cost_micros: cost,
        spent_micros: spent_now,
    })
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_holds_metrics_about_the_finding_only() {
        let report = json!({
            "days": 7,
            "tools": {"by_tool": [
                {"tool": "Bash", "calls": 40, "errors": 12, "error_rate": 0.3},
                {"tool": "Read", "calls": 10, "errors": 0, "error_rate": 0.0},
            ]},
            "cache": {"by_agent": [{"agent": "coder", "hit_rate": 0.4}], "by_model": [],
                "causes": [{"cause": "expired"}]},
        });
        let finding = json!({"rule": "tool.error_rate", "title": "Bash fails 30% of the time",
            "detail": "12 of 40 calls returned an error.", "fix": "Say what valid input looks like.",
            "agent": null, "model": null, "tool": "Bash", "impact_micros": 0, "impact_tokens": 0});
        let p = package(&report, &finding);
        assert_eq!(p["tool_metrics"]["calls"], 40);
        assert!(p["agent_cache_metrics"].is_null());
        assert!(p["cache_miss_causes"].is_null());
        assert_eq!(p["window_days"], 7);
        assert!(!p.to_string().contains("Read"), "{p}");
    }

    #[test]
    fn hosts_are_shown_without_paths() {
        let ep = Endpoint {
            base_url: "https://litellm.internal:4000/v1".into(),
            model: "m".into(),
            api_key: None,
            budget_micros: 1,
        };
        assert_eq!(ep.host(), "litellm.internal:4000");
    }
}
