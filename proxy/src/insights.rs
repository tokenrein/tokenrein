//! Insights: where agents waste tokens, worked out from the usage and step history.
//!
//! Everything here is a rule over data Tokenrein already keeps; no model is asked. Two reports:
//!
//! - **Cache health.** Agents re-send their whole context on every call, so most of their bill is
//!   input that a provider prompt cache could have served at a tenth of the price or less. Each
//!   call is compared with the previous call of its run: the prompt they share could have come
//!   from the cache, and when it didn't, the gap is put down to a cause (caching not on, the start
//!   of the prompt changed, the cache expired, a model switch, calls sent in parallel) and priced.
//! - **Tool health.** Per tool: how often it fails, how often an agent repeats the same call (same
//!   input fingerprint) or retries a failing input unchanged, and outputs too big for the context.
//!
//! Estimates are labelled as such: Tokenrein sees token counts, not prompts, so the shared prefix
//! of two calls is taken as the smaller of their prompts.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::history::{History, StepHistory};
use crate::pricing::{Catalog, Price, tokens_cost};
use crate::providers::Usage;

/// Rows read per report; beyond this the report says it is a sample.
const MAX_ROWS: usize = 1_000_000;
/// A cache entry lives this long without a read (Anthropic's default, OpenAI's in-memory cache).
const CACHE_TTL_US: i64 = 5 * 60 * 1_000_000;
/// Shared prompt smaller than this is not worth a finding (OpenAI's minimum cacheable prompt).
const MIN_CACHEABLE: u64 = 1_024;
/// Tool output over this many tokens crowds the context (Claude Code's own cap).
const OVERSIZED_TOKENS: u64 = 25_000;
/// Agents that report through these sources are people at a keyboard, not batch jobs.
const INTERACTIVE_SOURCES: &[&str] = &["claude-code", "codex", "gemini-cli", "cursor", "copilot"];

/// One model call, as the cache report needs it.
#[derive(Debug, Clone, Default)]
pub struct Call {
    pub run: String,
    pub ts_us: i64,
    pub provider: String,
    pub model: String,
    pub agent: String,
    pub user: String,
    pub source: String,
    pub usage: Usage,
    pub upstream_ms: i64,
    pub cost_micros: i64,
}

/// One tool call or result, as the tool report needs it.
#[derive(Debug, Clone, Default)]
pub struct ToolStep {
    pub run: String,
    pub ts_us: i64,
    pub agent: String,
    /// `true` for a tool call, `false` for its result.
    pub is_call: bool,
    pub name: String,
    pub call_id: String,
    pub status: String,
    pub fingerprint: String,
    pub size: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// Why a call paid full price for a prompt it shared with the call before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Cause {
    /// No cache read or write at all, on a provider where caching must be asked for.
    NotCached,
    /// The cache was written or should have hit, but the start of the prompt changed.
    PrefixChanged,
    /// More than 5 minutes passed since the call before.
    Expired,
    /// The run switched model; caches are per model.
    ModelSwitch,
    /// Sent before the call before it had answered, so its cache entry did not exist yet.
    Parallel,
}

impl Cause {
    const ALL: [Cause; 5] = [
        Cause::NotCached,
        Cause::PrefixChanged,
        Cause::Expired,
        Cause::ModelSwitch,
        Cause::Parallel,
    ];

    fn id(self) -> &'static str {
        match self {
            Cause::NotCached => "not_cached",
            Cause::PrefixChanged => "prefix_changed",
            Cause::Expired => "expired",
            Cause::ModelSwitch => "model_switch",
            Cause::Parallel => "parallel",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Cause::NotCached => "Prompt caching not turned on",
            Cause::PrefixChanged => "The start of the prompt changes between calls",
            Cause::Expired => "The cache expired between calls",
            Cause::ModelSwitch => "Runs switch model",
            Cause::Parallel => "Calls sent in parallel before the cache existed",
        }
    }

    fn fix(self, family: &str) -> &'static str {
        match (self, family) {
            (Cause::NotCached, "anthropic") => {
                "Add cache_control to the request: one top-level cache_control turns on automatic caching of the growing conversation."
            }
            (Cause::NotCached, _) => {
                "Turn on the provider's prompt caching, and keep prompts above its minimum cacheable length."
            }
            (Cause::PrefixChanged, _) => {
                "Keep the system prompt and tool list identical from call to call: no timestamps or IDs near the start, a fixed tool order, JSON with stable key order, and new content appended at the end instead of edited in."
            }
            (Cause::Expired, "openai") => {
                "Calls are often more than 5 minutes apart: ask for extended retention (prompt_cache_retention \"24h\", or a 12-hour cache write on GPT-5.6 and later)."
            }
            (Cause::Expired, _) => {
                "Calls are often more than 5 minutes apart: a 1-hour cache write (ttl \"1h\") costs 2x input instead of 1.25x and pays off after two reads."
            }
            (Cause::ModelSwitch, _) => {
                "Each model keeps its own cache, so a switch mid-run re-sends the whole context at full price: switch at the start of a task, or hand a short summary to the other model."
            }
            (Cause::Parallel, _) => {
                "A cache entry exists only once the first response has started: send one request on the shared prefix first, then fan out."
            }
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CacheTotals {
    pub calls: u64,
    /// Every prompt token: fresh input, cache reads and cache writes.
    pub prompt_tokens: u64,
    pub fresh_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_long_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_micros: i64,
    /// What cache reads saved against paying for them as fresh input.
    pub saved_micros: u64,
    /// Estimated: what reading the shared prompt from the cache would have saved on top.
    pub missed_micros: u64,
    /// Write premium paid for cache entries that were never read.
    pub unread_write_micros: u64,
}

impl CacheTotals {
    fn hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / self.prompt_tokens as f64
        }
    }

    fn add(&mut self, c: &Call, saved: u64) {
        self.calls += 1;
        self.prompt_tokens += c.usage.prompt();
        self.fresh_tokens += c.usage.input;
        self.cache_read_tokens += c.usage.cached_input;
        self.cache_write_tokens += c.usage.cache_write;
        self.cache_write_long_tokens += c.usage.cache_write_long;
        self.output_tokens += c.usage.output;
        self.reasoning_tokens += c.usage.reasoning;
        self.cost_micros += c.cost_micros;
        self.saved_micros += saved;
    }

    fn merge(&mut self, t: &CacheTotals) {
        self.calls += t.calls;
        self.prompt_tokens += t.prompt_tokens;
        self.fresh_tokens += t.fresh_tokens;
        self.cache_read_tokens += t.cache_read_tokens;
        self.cache_write_tokens += t.cache_write_tokens;
        self.cache_write_long_tokens += t.cache_write_long_tokens;
        self.output_tokens += t.output_tokens;
        self.reasoning_tokens += t.reasoning_tokens;
        self.cost_micros += t.cost_micros;
        self.saved_micros += t.saved_micros;
        self.missed_micros += t.missed_micros;
        self.unread_write_micros += t.unread_write_micros;
    }

    fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["hit_rate"] = json!(round(self.hit_rate()));
        v
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct CauseTotals {
    calls: u64,
    tokens: u64,
    missed_micros: u64,
}

/// A thing worth fixing, with what it costs and how to fix it.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable id of the rule, e.g. `cache.prefix_changed`, `tool.error_rate`.
    pub rule: String,
    /// "warning" or "info".
    pub severity: &'static str,
    pub title: String,
    pub detail: String,
    pub fix: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    /// Estimated dollars at stake over the window, when it can be priced.
    pub impact_micros: u64,
    /// Estimated tokens at stake.
    pub impact_tokens: u64,
}

/// One agent's calls to one model.
#[derive(Default)]
struct Group {
    family: String,
    price: Option<Price>,
    totals: CacheTotals,
    follow_ups: u64,
    causes: BTreeMap<Cause, CauseTotals>,
}

/// Cache health over `calls`, which must be ordered by run, then time.
pub fn cache_report(calls: &[Call], prices: &Catalog) -> Value {
    let mut totals = CacheTotals::default();
    let mut causes: BTreeMap<Cause, CauseTotals> = BTreeMap::new();
    // (agent, provider, model)
    let mut groups: BTreeMap<(String, String, String), Group> = BTreeMap::new();
    let mut by_model: BTreeMap<(String, String), CacheTotals> = BTreeMap::new();
    // agent -> (calls, cost, someone at a keyboard)
    let mut batch: BTreeMap<String, (u64, i64, bool)> = BTreeMap::new();
    let mut price_of: HashMap<(String, String), Price> = HashMap::new();

    for (i, c) in calls.iter().enumerate() {
        let price = *price_of
            .entry((c.provider.clone(), c.model.clone()))
            .or_insert_with(|| prices.lookup_any(&c.provider, &c.model).0);
        let saved = price.cache_savings(&c.usage);
        totals.add(c, saved);
        let model_totals = by_model
            .entry((c.provider.clone(), c.model.clone()))
            .or_default();
        model_totals.add(c, saved);
        let g = groups
            .entry((c.agent.clone(), c.provider.clone(), c.model.clone()))
            .or_default();
        g.family = provider_family(&c.provider, &c.model).to_string();
        g.price = Some(price);
        g.totals.add(c, saved);
        let b = batch.entry(c.agent.clone()).or_insert((0, 0, false));
        b.0 += 1;
        b.1 += c.cost_micros;
        b.2 |= !c.user.is_empty() || INTERACTIVE_SOURCES.contains(&c.source.as_str());

        let Some(prev) = i
            .checked_sub(1)
            .map(|p| &calls[p])
            .filter(|p| p.run == c.run)
        else {
            continue;
        };
        g.follow_ups += 1;
        let Some((cause, tokens, missed)) = miss(prev, c, &price, &g.family) else {
            continue;
        };
        for t in [
            causes.entry(cause).or_default(),
            g.causes.entry(cause).or_default(),
        ] {
            t.calls += 1;
            t.tokens += tokens;
            t.missed_micros += missed;
        }
        totals.missed_micros += missed;
        g.totals.missed_micros += missed;
        model_totals.missed_micros += missed;
    }

    let mut findings = Vec::new();
    for ((agent, _, model), g) in &mut groups {
        // Cache entries written and hardly ever read: the write premium bought nothing.
        let t = &g.totals;
        if t.cache_write_tokens > 0
            && t.cache_read_tokens * 5 < t.cache_write_tokens
            && t.calls >= 5
        {
            let waste = g.price.map_or(0, |p| {
                let short = t.cache_write_tokens - t.cache_write_long_tokens;
                tokens_cost(short, p.cache_write.saturating_sub(p.input))
                    + tokens_cost(
                        t.cache_write_long_tokens,
                        p.cache_write_long.saturating_sub(p.input),
                    )
            });
            totals.unread_write_micros += waste;
            g.totals.unread_write_micros += waste;
            findings.push(Finding {
                rule: "cache.unread_writes".into(),
                severity: "warning",
                title: format!("{agent} writes the cache but rarely reads it"),
                detail: format!(
                    "{} tokens written to the cache and {} read back, on {model}. A cache write costs more than plain input.",
                    fmt_n(g.totals.cache_write_tokens),
                    fmt_n(g.totals.cache_read_tokens)
                ),
                fix: Cause::PrefixChanged.fix(&g.family).into(),
                agent: Some(agent.clone()),
                model: Some(model.clone()),
                tool: None,
                impact_micros: waste,
                impact_tokens: g.totals.cache_write_tokens,
            });
        }
        for cause in Cause::ALL {
            let Some(ct) = g.causes.get(&cause) else {
                continue;
            };
            // Enough calls to be a pattern, not a one-off.
            if ct.calls < 5 || ct.calls * 10 < g.follow_ups || ct.tokens < MIN_CACHEABLE * 5 {
                continue;
            }
            findings.push(Finding {
                rule: format!("cache.{}", cause.id()),
                severity: if ct.missed_micros >= 1_000_000 {
                    "warning"
                } else {
                    "info"
                },
                title: format!("{}: {agent} on {model}", cause.label()),
                detail: format!(
                    "{} of {} follow-up calls paid full price for about {} tokens of prompt they shared with the call before.",
                    fmt_n(ct.calls),
                    fmt_n(g.follow_ups),
                    fmt_n(ct.tokens)
                ),
                fix: cause.fix(&g.family).into(),
                agent: Some(agent.clone()),
                model: Some(model.clone()),
                tool: None,
                impact_micros: ct.missed_micros,
                impact_tokens: ct.tokens,
            });
        }
    }
    // Agents with no person behind them and no coding-agent source may not need an answer now.
    for (agent, (n, cost, interactive)) in &batch {
        if *interactive || *n < 50 || *cost < 1_000_000 {
            continue;
        }
        findings.push(Finding {
            rule: "cost.batch_candidate".into(),
            severity: "info",
            title: format!("{agent} could use the Batch API"),
            detail: format!(
                "{} calls with no person attached. If they don't need an answer within seconds, the Anthropic, OpenAI and Gemini batch APIs charge half, cache discounts included.",
                fmt_n(*n)
            ),
            fix: "Send non-interactive work (evals, nightly jobs, back-fills) through the provider's Batch API.".into(),
            agent: Some(agent.clone()),
            model: None,
            tool: None,
            impact_micros: (*cost / 2).max(0) as u64,
            impact_tokens: 0,
        });
    }

    let mut model_rows: Vec<(&(String, String), &CacheTotals)> = by_model.iter().collect();
    model_rows.sort_by_key(|(_, t)| std::cmp::Reverse((t.missed_micros, t.cost_micros)));
    let mut by_agent: BTreeMap<&str, CacheTotals> = BTreeMap::new();
    for ((agent, _, _), g) in &groups {
        by_agent.entry(agent).or_default().merge(&g.totals);
    }
    let mut agent_rows: Vec<(&&str, &CacheTotals)> = by_agent.iter().collect();
    agent_rows.sort_by_key(|(_, t)| std::cmp::Reverse((t.missed_micros, t.cost_micros)));
    json!({
        "totals": totals.to_json(),
        "causes": Cause::ALL.iter().filter_map(|c| causes.get(c).map(|t| json!({
            "cause": c.id(), "label": c.label(), "calls": t.calls, "tokens": t.tokens,
            "missed_micros": t.missed_micros,
        }))).collect::<Vec<_>>(),
        "by_model": model_rows.into_iter().take(50).map(|((p, m), t)| {
            let mut j = t.to_json();
            j["provider"] = json!(p);
            j["model"] = json!(m);
            j
        }).collect::<Vec<_>>(),
        "by_agent": agent_rows.into_iter().take(50).map(|(a, t)| {
            let mut j = t.to_json();
            j["agent"] = json!(a);
            j
        }).collect::<Vec<_>>(),
        "findings": findings,
    })
}

/// Whose caching rules apply: caching is opt-in at Anthropic (and for Claude on Bedrock or
/// Vertex), automatic elsewhere.
fn provider_family(provider: &str, model: &str) -> &'static str {
    let p = provider.to_ascii_lowercase();
    if p.contains("anthropic") || p == "bedrock" || p == "claude" || model.contains("claude") {
        "anthropic"
    } else if p.contains("openai") || p.contains("azure") {
        "openai"
    } else {
        "other"
    }
}

/// The prompt `c` shared with `prev` but paid full price for, why, and what that cost; `None`
/// when the cache did its job or the provider has no cache discount.
fn miss(prev: &Call, c: &Call, price: &Price, family: &str) -> Option<(Cause, u64, u64)> {
    let p = price.for_prompt(c.usage.prompt());
    if p.cached_input >= p.input {
        return None;
    }
    let shared = prev.usage.prompt().min(c.usage.prompt());
    let missed = shared.saturating_sub(c.usage.cached_input);
    // Some of the shared prompt is always new (the last turn), so small gaps are noise.
    if shared < MIN_CACHEABLE || missed * 10 < shared {
        return None;
    }
    // Events are stamped when a call finishes: `c` started `upstream_ms` before its stamp, and
    // `prev`'s cache entry existed from (about) its own stamp.
    let started = c.ts_us - c.upstream_ms.max(0) * 1_000;
    let cause = if prev.model != c.model {
        Cause::ModelSwitch
    } else if started < prev.ts_us && c.usage.cached_input == 0 {
        Cause::Parallel
    } else if started - prev.ts_us > CACHE_TTL_US && prev.usage.cache_write_long == 0 {
        Cause::Expired
    } else if family == "anthropic" && c.usage.cached_input == 0 && c.usage.cache_write == 0 {
        Cause::NotCached
    } else {
        Cause::PrefixChanged
    };
    Some((cause, missed, tokens_cost(missed, p.input - p.cached_input)))
}

#[derive(Debug, Default, Clone)]
struct ToolStats {
    calls: u64,
    errors: u64,
    repeats: u64,
    fail_retries: u64,
    oversized: u64,
    /// Estimated tokens of repeated calls' output.
    repeat_tokens: u64,
    /// Estimated tokens of output above the oversized cap.
    excess_tokens: u64,
    output_tokens: u64,
    durations: Vec<i64>,
    sizes: Vec<u64>,
}

#[derive(Debug, Default, Clone, Serialize)]
struct RunStats {
    run: String,
    agent: String,
    calls: u64,
    errors: u64,
    repeats: u64,
    fail_retries: u64,
}

/// A tool call with its outcome, after pairing calls with results.
struct Paired<'a> {
    step: &'a ToolStep,
    status: &'a str,
    out_bytes: Option<u64>,
    duration_ms: Option<i64>,
}

/// Pairs each call with its result: by call id, else the next result of the same tool in the run.
fn pair(steps: &[ToolStep]) -> Vec<Paired<'_>> {
    let mut out: Vec<Paired> = Vec::new();
    let mut by_id: HashMap<(&str, &str), usize> = HashMap::new();
    let mut open: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for s in steps {
        if s.is_call {
            let i = out.len();
            out.push(Paired {
                step: s,
                status: s.status.as_str(),
                out_bytes: None,
                duration_ms: s.duration_ms,
            });
            if s.call_id.is_empty() {
                open.entry((s.run.as_str(), s.name.as_str()))
                    .or_default()
                    .push(i);
            } else {
                by_id.insert((s.run.as_str(), s.call_id.as_str()), i);
            }
            continue;
        }
        let found = if s.call_id.is_empty() {
            open.get_mut(&(s.run.as_str(), s.name.as_str()))
                .and_then(|q| (!q.is_empty()).then(|| q.remove(0)))
        } else {
            by_id.remove(&(s.run.as_str(), s.call_id.as_str()))
        };
        let Some(i) = found else { continue };
        let p = &mut out[i];
        if !matches!(p.status, "error" | "blocked") && !s.status.is_empty() {
            p.status = s.status.as_str();
        }
        p.out_bytes = s.size.map(|b| b.max(0) as u64);
        p.duration_ms = p
            .duration_ms
            .or(s.duration_ms)
            .or(Some((s.ts_us - p.step.ts_us).max(0) / 1_000));
    }
    out
}

/// Tool health over `steps`, which must be ordered by run, then time.
pub fn tool_report(steps: &[ToolStep]) -> Value {
    let calls = pair(steps);
    let mut tools: BTreeMap<&str, ToolStats> = BTreeMap::new();
    let mut runs: BTreeMap<&str, RunStats> = BTreeMap::new();
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    // (run, tool) -> fingerprint of its last call, when that call failed.
    let mut last_failed: HashMap<(&str, &str), &str> = HashMap::new();
    for p in &calls {
        let s = p.step;
        let name = if s.name.is_empty() {
            "(unnamed)"
        } else {
            s.name.as_str()
        };
        let t = tools.entry(name).or_default();
        let r = runs.entry(s.run.as_str()).or_insert_with(|| RunStats {
            run: s.run.clone(),
            agent: s.agent.clone(),
            ..RunStats::default()
        });
        t.calls += 1;
        r.calls += 1;
        let failed = p.status == "error";
        if failed {
            t.errors += 1;
            r.errors += 1;
        }
        let tokens = p
            .out_bytes
            .map(crate::providers::estimate_tokens_from_bytes);
        if let Some(tok) = tokens {
            t.output_tokens += tok;
            t.sizes.push(tok);
            if tok > OVERSIZED_TOKENS {
                t.oversized += 1;
                t.excess_tokens += tok - OVERSIZED_TOKENS;
            }
        }
        if let Some(d) = p.duration_ms {
            t.durations.push(d);
        }
        if !s.fingerprint.is_empty() {
            if !seen.insert((s.run.as_str(), s.fingerprint.as_str())) {
                t.repeats += 1;
                r.repeats += 1;
                t.repeat_tokens += tokens.unwrap_or(0);
            }
            let key = (s.run.as_str(), name);
            if last_failed.get(&key) == Some(&s.fingerprint.as_str()) {
                t.fail_retries += 1;
                r.fail_retries += 1;
            }
            if failed {
                last_failed.insert(key, s.fingerprint.as_str());
            } else {
                last_failed.remove(&key);
            }
        }
    }

    let mut findings = Vec::new();
    let mut total = ToolStats::default();
    let mut rows = Vec::new();
    for (name, t) in &tools {
        total.calls += t.calls;
        total.errors += t.errors;
        total.repeats += t.repeats;
        total.fail_retries += t.fail_retries;
        total.oversized += t.oversized;
        total.repeat_tokens += t.repeat_tokens;
        total.excess_tokens += t.excess_tokens;
        total.output_tokens += t.output_tokens;
        let error_rate = t.errors as f64 / t.calls.max(1) as f64;
        let p50 = percentile(&t.durations, 50);
        let p95 = percentile(&t.durations, 95);
        rows.push(json!({
            "tool": name, "calls": t.calls, "errors": t.errors, "error_rate": round(error_rate),
            "repeats": t.repeats, "fail_retries": t.fail_retries, "oversized": t.oversized,
            "p50_ms": p50, "p95_ms": p95,
            "p95_output_tokens": percentile(&t.sizes, 95), "output_tokens": t.output_tokens,
            "wasted_tokens": t.repeat_tokens + t.excess_tokens,
        }));
        let tool =
            |rule: &str, severity, title: String, detail: String, fix: &str, tokens| Finding {
                rule: rule.into(),
                severity,
                title,
                detail,
                fix: fix.into(),
                agent: None,
                model: None,
                tool: Some(name.to_string()),
                impact_micros: 0,
                impact_tokens: tokens,
            };
        if t.calls >= 10 && error_rate >= 0.2 {
            findings.push(tool(
                "tool.error_rate",
                "warning",
                format!("{name} fails {}% of the time", (error_rate * 100.0).round()),
                format!("{} of {} calls returned an error.", fmt_n(t.errors), fmt_n(t.calls)),
                "Its description or input schema probably misleads the agent: say what valid input looks like, with an example, and return errors that tell the agent how to fix the call.",
                0,
            ));
        }
        if t.fail_retries >= 2 {
            findings.push(tool(
                "tool.retries_failing_input",
                "warning",
                format!("Agents retry {name} with the same failing input"),
                format!(
                    "{} times a call failed and the next call was identical.",
                    fmt_n(t.fail_retries)
                ),
                "Return an error message that says what to change, so the agent changes its input instead of repeating it.",
                0,
            ));
        }
        if t.repeats >= 5 && t.repeats * 5 >= t.calls {
            findings.push(tool(
                "tool.repeated_calls",
                if t.repeat_tokens >= 100_000 { "warning" } else { "info" },
                format!("{name} is called again with the same input"),
                format!(
                    "{} of {} calls repeated an earlier call of the same run{}.",
                    fmt_n(t.repeats),
                    fmt_n(t.calls),
                    if t.repeat_tokens > 0 {
                        format!(", adding about {} tokens of output the agent already had", fmt_n(t.repeat_tokens))
                    } else {
                        String::new()
                    }
                ),
                "The agent asks for something it already has: write large results to a file it can re-read, cache the tool's result, or add pagination so it fetches only the part it needs.",
                t.repeat_tokens,
            ));
        }
        if t.oversized >= 1 {
            findings.push(tool(
                "tool.oversized_output",
                if t.excess_tokens >= 100_000 { "warning" } else { "info" },
                format!("{name} returns outputs too big for the context"),
                format!(
                    "{} outputs over {} tokens, {} tokens over in all; every later call in the run re-reads them.",
                    fmt_n(t.oversized),
                    fmt_n(OVERSIZED_TOKENS),
                    fmt_n(t.excess_tokens)
                ),
                "Return less: filter, paginate or truncate (keep the last lines of logs and test output), or write the full output to a file and return its path.",
                t.excess_tokens,
            ));
        }
        if t.calls >= 5 && p95.is_some_and(|d| d >= 30_000) {
            findings.push(tool(
                "tool.slow",
                "info",
                format!("{name} is slow"),
                format!(
                    "Half its calls take over {}, the slowest 5% over {}.",
                    fmt_ms(p50.unwrap_or(0)),
                    fmt_ms(p95.unwrap_or(0))
                ),
                "A slow tool keeps the run, and its context, open: add a timeout, or return early with partial results.",
                0,
            ));
        }
    }
    rows.sort_by_key(|v| {
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        std::cmp::Reverse((n("wasted_tokens") + n("errors") * 1_000, n("calls")))
    });
    let mut top_runs: Vec<&RunStats> = runs
        .values()
        .filter(|r| r.errors + r.repeats + r.fail_retries > 0)
        .collect();
    top_runs.sort_by_key(|r| std::cmp::Reverse(r.repeats + r.fail_retries * 2 + r.errors));
    json!({
        "totals": {
            "calls": total.calls, "errors": total.errors,
            "error_rate": round(total.errors as f64 / total.calls.max(1) as f64),
            "repeats": total.repeats, "fail_retries": total.fail_retries,
            "oversized": total.oversized, "output_tokens": total.output_tokens,
            "wasted_tokens": total.repeat_tokens + total.excess_tokens, "runs": runs.len(),
        },
        "by_tool": rows.into_iter().take(100).collect::<Vec<_>>(),
        "runs": top_runs.into_iter().take(20).collect::<Vec<_>>(),
        "findings": findings,
    })
}

fn percentile<T: Copy + Ord>(v: &[T], pct: usize) -> Option<T> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    Some(s[((s.len() - 1) * pct).div_ceil(100).min(s.len() - 1)])
}

fn round(f: f64) -> f64 {
    (f * 1000.0).round() / 1000.0
}

fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_ms(ms: i64) -> String {
    if ms >= 60_000 {
        format!("{:.1} min", ms as f64 / 60_000.0)
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

/// Blocking: the model calls of `org` in `[from_us, to_us)`, ordered by run and time, and
/// whether there were more than [`MAX_ROWS`].
pub fn read_calls(h: &History, org: &str, from_us: i64, to_us: i64) -> Result<(Vec<Call>, bool)> {
    let Some(conn) = h.open_window(org, from_us, to_us)? else {
        return Ok((Vec::new(), false));
    };
    let mut stmt = conn.prepare(
        r#"SELECT run_id, ts_us, COALESCE(provider, ''), model, agent, COALESCE("user", ''),
                  COALESCE(source, ''), input_tokens, cached_input_tokens, cache_write_tokens,
                  COALESCE(cache_write_long_tokens, 0), output_tokens,
                  COALESCE(reasoning_tokens, 0), upstream_ms, cost_micros
           FROM e
           WHERE ts_us >= ? AND ts_us < ? AND decision LIKE 'allowed%'
             AND input_tokens + cached_input_tokens + cache_write_tokens + output_tokens > 0
           ORDER BY run_id, ts_us LIMIT ?"#,
    )?;
    let n = |v: i64| v.max(0) as u64;
    let rows = stmt.query_map(duckdb::params![from_us, to_us, MAX_ROWS as i64 + 1], |r| {
        Ok(Call {
            run: r.get(0)?,
            ts_us: r.get(1)?,
            provider: r.get(2)?,
            model: r.get(3)?,
            agent: r.get(4)?,
            user: r.get(5)?,
            source: r.get(6)?,
            usage: Usage {
                input: n(r.get(7)?),
                cached_input: n(r.get(8)?),
                cache_write: n(r.get(9)?),
                cache_write_long: n(r.get(10)?),
                output: n(r.get(11)?),
                reasoning: n(r.get(12)?),
            },
            upstream_ms: r.get(13)?,
            cost_micros: r.get(14)?,
        })
    })?;
    let mut calls: Vec<Call> = rows.collect::<duckdb::Result<_>>()?;
    let sampled = calls.len() > MAX_ROWS;
    calls.truncate(MAX_ROWS);
    Ok((calls, sampled))
}

/// Blocking: the tool calls and results of `org` in `[from_us, to_us)`, ordered by run and time.
pub fn read_tool_steps(
    h: &StepHistory,
    org: &str,
    from_us: i64,
    to_us: i64,
) -> Result<(Vec<ToolStep>, bool)> {
    let Some(conn) = h.open_window(org, from_us, to_us)? else {
        return Ok((Vec::new(), false));
    };
    let mut stmt = conn.prepare(
        r#"SELECT run_id, ts_us, agent, kind = 'tool_call', COALESCE(name, ''),
                  COALESCE(call_id, ''), COALESCE(status, ''), COALESCE(fingerprint, ''),
                  size, duration_ms
           FROM e
           WHERE ts_us >= ? AND ts_us < ? AND kind IN ('tool_call', 'tool_result')
           ORDER BY run_id, ts_us LIMIT ?"#,
    )?;
    let rows = stmt.query_map(duckdb::params![from_us, to_us, MAX_ROWS as i64 + 1], |r| {
        Ok(ToolStep {
            run: r.get(0)?,
            ts_us: r.get(1)?,
            agent: r.get(2)?,
            is_call: r.get(3)?,
            name: r.get(4)?,
            call_id: r.get(5)?,
            status: r.get(6)?,
            fingerprint: r.get(7)?,
            size: r.get(8)?,
            duration_ms: r.get(9)?,
        })
    })?;
    let mut steps: Vec<ToolStep> = rows.collect::<duckdb::Result<_>>()?;
    let sampled = steps.len() > MAX_ROWS;
    steps.truncate(MAX_ROWS);
    Ok((steps, sampled))
}

/// Blocking: the whole report for `org` over `[from_us, to_us)`.
pub fn report(
    usage: &History,
    steps: Option<&StepHistory>,
    prices: &Catalog,
    org: &str,
    from_us: i64,
    to_us: i64,
) -> Result<Value> {
    let (calls, calls_sampled) = read_calls(usage, org, from_us, to_us)?;
    let (tool_steps, steps_sampled) = match steps {
        Some(s) => read_tool_steps(s, org, from_us, to_us)?,
        None => (Vec::new(), false),
    };
    let cache = cache_report(&calls, prices);
    let tools = tool_report(&tool_steps);
    let mut findings: Vec<Value> = cache["findings"]
        .as_array()
        .into_iter()
        .chain(tools["findings"].as_array())
        .flatten()
        .cloned()
        .collect();
    // Warnings first, then by dollars, then by tokens.
    findings.sort_by_key(|v| {
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        std::cmp::Reverse((
            v["severity"] == "warning",
            n("impact_micros"),
            n("impact_tokens"),
        ))
    });
    Ok(json!({
        "findings": findings,
        "cache": cache,
        "tools": tools,
        "sampled": calls_sampled || steps_sampled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(run: &str, t_s: i64, model: &str, input: u64, cached: u64, write: u64) -> Call {
        Call {
            run: run.into(),
            ts_us: t_s * 1_000_000,
            provider: "anthropic".into(),
            model: model.into(),
            agent: "coder".into(),
            user: "ana@example.com".into(),
            source: "sdk".into(),
            usage: Usage {
                input,
                cached_input: cached,
                cache_write: write,
                output: 500,
                ..Usage::default()
            },
            upstream_ms: 2_000,
            cost_micros: 1_000,
        }
    }

    fn rules(v: &Value) -> Vec<String> {
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["rule"].as_str().unwrap().to_string())
            .collect()
    }

    fn has_cause(v: &Value, cause: &str) -> bool {
        v["causes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["cause"] == cause)
    }

    #[test]
    fn a_well_cached_run_has_nothing_to_report() {
        let calls: Vec<Call> = (0..20)
            .map(|i| {
                call(
                    "r1",
                    i * 10,
                    "claude-opus-5-5",
                    500,
                    50_000 + i as u64 * 500,
                    500,
                )
            })
            .collect();
        let r = cache_report(&calls, &Catalog::builtin());
        assert!(rules(&r).is_empty(), "{r}");
        assert!(r["totals"]["hit_rate"].as_f64().unwrap() > 0.9);
        assert!(r["totals"]["saved_micros"].as_u64().unwrap() > 0);
        assert_eq!(r["totals"]["missed_micros"], 0);
    }

    #[test]
    fn misses_are_put_down_to_their_cause() {
        let catalog = Catalog::builtin();
        // Never cached at all: opt-in caching is off.
        let off: Vec<Call> = (0..10)
            .map(|i| call("r1", i * 10, "claude-opus-5-5", 40_000, 0, 0))
            .collect();
        let r = cache_report(&off, &catalog);
        assert_eq!(rules(&r), ["cache.not_cached"]);
        assert_eq!(r["causes"][0]["calls"], 9);
        // 9 follow-ups x 40k shared tokens x ($4 - $0.20) per million.
        assert_eq!(r["totals"]["missed_micros"], 9 * 152_000);
        // Written every call, never read: the start of the prompt keeps changing.
        let churn: Vec<Call> = (0..10)
            .map(|i| call("r1", i * 10, "claude-opus-5-5", 500, 0, 40_000))
            .collect();
        let found = rules(&cache_report(&churn, &catalog));
        assert!(found.contains(&"cache.unread_writes".into()), "{found:?}");
        assert!(found.contains(&"cache.prefix_changed".into()), "{found:?}");
        // Ten minutes between calls: the cache expired.
        let slow: Vec<Call> = (0..10)
            .map(|i| call("r1", i * 600, "claude-opus-5-5", 500, 0, 40_000))
            .collect();
        assert!(has_cause(&cache_report(&slow, &catalog), "expired"));
        // Alternating models.
        let switch: Vec<Call> = (0..10)
            .map(|i| {
                let m = if i % 2 == 0 {
                    "claude-opus-5-5"
                } else {
                    "claude-sonnet-5-5"
                };
                call("r1", i * 10, m, 40_000, 0, 0)
            })
            .collect();
        assert!(has_cause(&cache_report(&switch, &catalog), "model_switch"));
        // Fired together before the first answer.
        let fan: Vec<Call> = (0..10)
            .map(|_| call("r1", 100, "claude-opus-5-5", 500, 0, 40_000))
            .collect();
        assert!(has_cause(&cache_report(&fan, &catalog), "parallel"));
        // Calls of different runs are never compared.
        let runs: Vec<Call> = (0..10)
            .map(|i| call(&format!("r{i}"), i, "claude-opus-5-5", 40_000, 0, 0))
            .collect();
        assert_eq!(cache_report(&runs, &catalog)["totals"]["missed_micros"], 0);
    }

    #[test]
    fn providers_without_a_cache_discount_are_not_flagged() {
        let calls: Vec<Call> = (0..10)
            .map(|i| Call {
                provider: "mistral".into(),
                ..call("r1", i * 10, "mistral-large", 40_000, 0, 0)
            })
            .collect();
        assert!(rules(&cache_report(&calls, &Catalog::builtin())).is_empty());
    }

    #[test]
    fn unattended_agents_are_batch_candidates() {
        let calls: Vec<Call> = (0..60)
            .map(|i| Call {
                user: String::new(),
                cost_micros: 100_000,
                ..call(&format!("job{i}"), i, "claude-opus-5-5", 500, 0, 0)
            })
            .collect();
        let r = cache_report(&calls, &Catalog::builtin());
        assert_eq!(rules(&r), ["cost.batch_candidate"]);
        assert_eq!(r["findings"][0]["impact_micros"], 3_000_000);
    }

    #[allow(clippy::too_many_arguments)]
    fn step(
        run: &str,
        t: i64,
        is_call: bool,
        name: &str,
        id: &str,
        status: &str,
        fp: &str,
        size: Option<i64>,
    ) -> ToolStep {
        ToolStep {
            run: run.into(),
            ts_us: t * 1_000_000,
            agent: "coder".into(),
            is_call,
            name: name.into(),
            call_id: id.into(),
            status: status.into(),
            fingerprint: fp.into(),
            size,
            duration_ms: None,
        }
    }

    #[test]
    fn tool_errors_retries_repeats_and_big_outputs() {
        let mut s = Vec::new();
        let mut t = 0;
        // Bash fails 6 of 10 times; the first three calls are identical.
        for i in 0..10 {
            let fp = if i < 3 {
                "same".to_string()
            } else {
                format!("fp{i}")
            };
            let status = if i < 6 { "error" } else { "ok" };
            let id = format!("b{i}");
            s.push(step("r1", t, true, "Bash", &id, "", &fp, Some(40)));
            s.push(step("r1", t + 1, false, "Bash", &id, status, "", Some(200)));
            t += 2;
        }
        // Read: the same file 6 times, 100 KB each; results without call ids pair in order.
        for _ in 0..6 {
            s.push(step("r1", t, true, "Read", "", "", "readme", Some(30)));
            s.push(step(
                "r1",
                t + 3,
                false,
                "Read",
                "",
                "ok",
                "",
                Some(100_000),
            ));
            t += 4;
        }
        let r = tool_report(&s);
        let found = rules(&r);
        for want in [
            "tool.error_rate",
            "tool.retries_failing_input",
            "tool.repeated_calls",
            "tool.oversized_output",
        ] {
            assert!(
                found.contains(&want.to_string()),
                "{want} missing from {found:?}"
            );
        }
        let tool = |name: &str| {
            r["by_tool"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["tool"] == name)
                .unwrap()
                .clone()
        };
        let bash = tool("Bash");
        assert_eq!(
            (
                bash["errors"].as_u64(),
                bash["fail_retries"].as_u64(),
                bash["repeats"].as_u64()
            ),
            (Some(6), Some(2), Some(2))
        );
        let read = tool("Read");
        assert_eq!(
            (read["repeats"].as_u64(), read["oversized"].as_u64()),
            (Some(5), Some(6))
        );
        // Durations come from the gap between call and result.
        assert_eq!(read["p50_ms"], 3_000);
        assert_eq!(r["totals"]["calls"], 16);
        assert_eq!(r["runs"][0]["run"], "r1");
    }
}
