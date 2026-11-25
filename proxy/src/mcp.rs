//! A budget the agent can see: a small MCP server (Streamable HTTP, JSON replies) on the proxy.
//!
//! An agent that knows how much budget its run has left can wrap up cleanly instead of being cut
//! off mid-task. Connect it like any remote MCP server, with a Tokenrein key:
//!
//! ```text
//! claude mcp add --transport http tokenrein http://localhost:8787/mcp \
//!   --header "Authorization: Bearer tr_..."
//! ```
//!
//! One tool, `tokenrein_budget`: what the run has spent and has left, where its cost is heading,
//! and the project, agent and org budgets around it. The run is the one named in the call (or the
//! `x-tr-run-id` header), else the agent's most recently active run in the key's project.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use chrono::Utc;
use serde_json::{Value, json};

use crate::config::BudgetPolicy;
use crate::ingest::Caller;
use crate::ledger::Keys;
use crate::proxy::AppState;

const PROTOCOL: &str = "2025-06-18";
const TOOL: &str = "tokenrein_budget";

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_spec() -> Value {
    json!({
        "name": TOOL,
        "title": "Budget left",
        "description": "How much budget this run has left, how fast it is spending and where it \
            is heading, and the project, agent and org budgets around it. Call it before long or \
            expensive work, and wrap up (finish the step, report what is done and what is left) \
            when little is left.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "The run to check; by default, this agent's current run." },
                "agent": { "type": "string", "description": "The agent's name, when the run is not given." }
            }
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false }
    })
}

/// `POST /mcp`: JSON-RPC requests from an MCP client.
pub async fn handle(State(st): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let c = match crate::ingest::caller(&st, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(msg) = serde_json::from_str::<Value>(&body) else {
        return Json(rpc_error(&Value::Null, -32700, "not JSON")).into_response();
    };
    let batch = msg.is_array();
    let mut out = Vec::new();
    for m in msg.as_array().cloned().unwrap_or_else(|| vec![msg]) {
        if let Some(reply) = answer(&st, &c, &headers, &m).await {
            out.push(reply);
        }
    }
    match (batch, out.len()) {
        // Only notifications: nothing to say.
        (_, 0) => StatusCode::ACCEPTED.into_response(),
        (false, _) => Json(out.remove(0)).into_response(),
        (true, _) => Json(Value::Array(out)).into_response(),
    }
}

/// `GET /mcp`: no server-sent stream; everything is request and response.
pub async fn no_stream() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn answer(st: &AppState, c: &Caller, headers: &HeaderMap, m: &Value) -> Option<Value> {
    let id = m.get("id")?.clone();
    let method = m.get("method").and_then(Value::as_str).unwrap_or("");
    let params = m.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => rpc_result(
            &id,
            json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "tokenrein", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Check tokenrein_budget before long or expensive work, and wrap up \
                    cleanly when the run's budget is nearly used up.",
            }),
        ),
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({ "tools": [tool_spec()] })),
        "tools/call" => {
            if params.get("name").and_then(Value::as_str) != Some(TOOL) {
                return Some(rpc_error(&id, -32602, "unknown tool"));
            }
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            match budget(st, c, headers, &args).await {
                Ok(report) => rpc_result(
                    &id,
                    json!({
                        "content": [{ "type": "text", "text": report["summary"] }],
                        "structuredContent": report,
                        "isError": false,
                    }),
                ),
                Err(e) => rpc_result(
                    &id,
                    json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                ),
            }
        }
        _ => rpc_error(&id, -32601, "method not found"),
    })
}

fn usd(m: f64) -> String {
    format!("${:.2}", m / 1e6)
}

/// One budget: spent and limit in micro-USD, from its ledger hash and the configured policy.
fn scope(b: &std::collections::HashMap<String, String>, policy: Option<&BudgetPolicy>) -> Value {
    let num = |k: &str| b.get(k).and_then(|v| v.parse::<f64>().ok());
    let spent = num("spent").unwrap_or(0.0);
    let limit = num("limit")
        .filter(|l| *l > 0.0)
        .or(policy.map(|p| p.limit_usd * 1e6));
    let mode = b
        .get("mode")
        .cloned()
        .or(policy.map(|p| format!("{:?}", p.mode).to_lowercase()));
    json!({
        "spent_usd": spent / 1e6,
        "limit_usd": limit.map(|l| l / 1e6),
        "left_usd": limit.map(|l| ((l - spent) / 1e6).max(0.0)),
        "mode": mode,
    })
}

/// The run to report on: named, from the header, or the agent's latest run in the project.
async fn which_run(
    st: &AppState,
    c: &Caller,
    headers: &HeaderMap,
    args: &Value,
) -> Option<(String, String)> {
    let named = args
        .get("run_id")
        .and_then(Value::as_str)
        .or_else(|| headers.get("x-tr-run-id").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|r| crate::config::valid_id(r));
    let agent = args
        .get("agent")
        .and_then(Value::as_str)
        .or_else(|| headers.get("x-tr-agent").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|a| !a.is_empty());
    let org = &c.org().id;
    let runs: Vec<String> = match named {
        Some(r) => vec![r.to_string()],
        None => st
            .ledger
            .recent_runs(org, 200)
            .await
            .ok()?
            .into_iter()
            .map(|(r, _)| r)
            .collect(),
    };
    let details = st.ledger.run_details(org, &runs).await.ok()?;
    runs.into_iter().zip(details).find_map(|(run, d)| {
        let project_ok = d.info.get("project") == Some(&c.project().id);
        let agent_ok = agent.is_none_or(|a| d.info.get("agent").map(String::as_str) == Some(a));
        let root = d.parent.is_none() || named.is_some();
        (project_ok && agent_ok && root).then(|| {
            let a = d.info.get("agent").cloned().unwrap_or_default();
            (run, a)
        })
    })
}

async fn budget(
    st: &AppState,
    c: &Caller,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, String> {
    let Some((run, agent)) = which_run(st, c, headers, args).await else {
        return Err(format!(
            "No active run found in project {} yet. Pass run_id, or check again after the run's \
             first model call.",
            c.project().id
        ));
    };
    let org = &c.org().id;
    let project = &c.project().id;
    let keys = Keys::new(org);
    let now = Utc::now();
    let (ymd, ym) = (
        now.format("%Y%m%d").to_string(),
        now.format("%Y%m").to_string(),
    );
    let hashes = st
        .ledger
        .budgets(&[
            keys.run_budget(&run),
            keys.agent_daily(project, &agent, &ymd),
            keys.project_monthly(project, &ym),
            keys.org_monthly(&ym),
        ])
        .await
        .map_err(|e| format!("Tokenrein could not read the budgets: {e}"))?;
    let b = c.budgets();
    let run_b = scope(&hashes[0], b.run.as_ref());
    let forecast = crate::forecast::get(st, org, &run).await;
    let left = run_b["left_usd"].as_f64();
    let spent = run_b["spent_usd"].as_f64().unwrap_or(0.0);
    let mut summary = match (left, run_b["limit_usd"].as_f64()) {
        (Some(left), Some(limit)) => format!(
            "Run {run}: {} spent of its {} budget, {} left.",
            usd(spent * 1e6),
            usd(limit * 1e6),
            usd(left * 1e6)
        ),
        _ => format!("Run {run}: {} spent, no run budget.", usd(spent * 1e6)),
    };
    if let Some(f) = &forecast {
        summary.push_str(&format!(
            " The latest call cost about {}",
            usd(f.per_call_micros as f64)
        ));
        if f.growth_per_call_micros > 0 {
            summary.push_str(&format!(
                ", and each call costs about {} more than the last as the context grows",
                usd(f.growth_per_call_micros as f64)
            ));
        }
        summary.push('.');
        if let Some(m) = f.calls_to_limit {
            summary.push_str(&format!(
                " At this rate the budget runs out in about {m} more calls."
            ));
        }
    }
    let tight = left.is_some_and(|l| l <= spent.max(0.01) * 0.25)
        || forecast
            .as_ref()
            .and_then(|f| f.calls_to_limit)
            .is_some_and(|m| m <= 5);
    summary.push_str(if tight {
        " Little is left: finish the current step, then report what is done and what is left."
    } else {
        " There is room to continue."
    });
    Ok(json!({
        "summary": summary,
        "run": run,
        "agent": agent,
        "project": project,
        "run_budget": run_b,
        "agent_daily": scope(&hashes[1], b.agent_daily.as_ref()),
        "project_monthly": scope(&hashes[2], b.project_monthly.as_ref()),
        "org_monthly": scope(&hashes[3], b.org_monthly.as_ref()),
        "forecast": forecast,
        "wrap_up": tight,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_read_from_the_ledger_or_the_policy() {
        let hash = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let s = scope(
            &hash(&[
                ("spent", "3500000"),
                ("limit", "10000000"),
                ("mode", "enforce"),
            ]),
            None,
        );
        assert_eq!(
            (s["spent_usd"].as_f64(), s["left_usd"].as_f64()),
            (Some(3.5), Some(6.5))
        );
        assert_eq!(s["mode"], "enforce");
        // Nothing spent yet: the configured limit.
        let p = BudgetPolicy {
            limit_usd: 2.0,
            mode: crate::config::Mode::Check,
        };
        let s = scope(&hash(&[]), Some(&p));
        assert_eq!(
            (s["limit_usd"].as_f64(), s["mode"].as_str()),
            (Some(2.0), Some("check"))
        );
        // No limit at all.
        assert!(scope(&hash(&[("spent", "100")]), None)["left_usd"].is_null());
    }

    #[test]
    fn the_tool_describes_itself() {
        let t = tool_spec();
        assert_eq!(t["name"], TOOL);
        assert_eq!(t["inputSchema"]["type"], "object");
        assert_eq!(rpc_error(&json!(1), -32601, "x")["error"]["code"], -32601);
    }
}
