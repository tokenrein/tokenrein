// Tokenrein dashboard: Insights page (where agents waste tokens, and what to fix first).
"use strict";

const INSIGHT_DAYS_KEY = "tokenrein.insights.days";
const INSIGHT_DAYS = [1, 7, 30, 90];

function insightDays() {
  const d = Number(store("local", INSIGHT_DAYS_KEY));
  return INSIGHT_DAYS.includes(d) ? d : 7;
}

// The report is heavier than other pages: fetched when the page opens or the window changes, and
// at most once a minute while it stays open.
async function loadInsights(o, force = false) {
  const days = insightDays();
  const cur = S.insights;
  if (!force && cur && cur.days === days && cur.org === S.org && Date.now() - cur.at < 60_000) return;
  try { S.insights = { ...(await api(`/admin/orgs/${o}/insights?days=${days}`)), org: S.org, at: Date.now() }; }
  catch (err) { S.insights = { error: err.message, days, org: S.org, at: Date.now() }; }
}

const tok = n => Number(n || 0).toLocaleString();
const pct = f => `${Math.round((f || 0) * 100)}%`;
const msShort = ms => ms == null ? "–" : ms >= 60_000 ? `${(ms / 60_000).toFixed(1)} min` : ms >= 1000 ? `${(ms / 1000).toFixed(1)} s` : `${ms} ms`;

function findingImpact(f) {
  if (f.impact_micros) return `<b>${usd(f.impact_micros)}</b>`;
  if (f.impact_tokens) return `<b>${tok(f.impact_tokens)}</b> tokens`;
  return "";
}

function findingCard(f) {
  const scope = [f.agent && `agent <span class="mono">${esc(f.agent)}</span>`, f.model && `model <span class="mono">${esc(f.model)}</span>`,
    f.tool && `tool <span class="mono">${esc(f.tool)}</span>`].filter(Boolean).join(" · ");
  const area = f.rule.startsWith("tool.") ? "tool" : f.rule.startsWith("cache.") ? "cache" : "cost";
  return `<div class="insight ${f.severity === "warning" ? "warn" : ""}">
    <div class="insight-h"><span class="badge ${f.severity === "warning" ? "b-warn" : "b-info"}">${area}</span>
      <b>${esc(f.title)}</b><span class="insight-impact">${findingImpact(f)}</span></div>
    <p>${esc(f.detail)}</p>
    <p class="insight-fix"><b>Fix:</b> ${esc(f.fix)}</p>
    ${scope ? `<p class="muted">${scope}</p>` : ""}
  </div>`;
}

function renderInsights() {
  const el = $("#insights");
  const days = insightDays();
  $("#insights-days").innerHTML = INSIGHT_DAYS.map(d => `<option value="${d}" ${d === days ? "selected" : ""}>Last ${d === 1 ? "24 hours" : d + " days"}</option>`).join("");
  const r = S.insights;
  if (!r || r.org !== S.org) { el.innerHTML = '<p class="muted">Working it out…</p>'; return; }
  if (r.error) { el.innerHTML = `<div class="card"><div class="card-b">${esc(r.error)}</div></div>`; return; }
  const c = r.cache.totals, t = r.tools.totals;
  const kpis = `<div class="grid kpis">
    ${kpi("Input from cache", c.prompt_tokens ? pct(c.hit_rate) : "–", c.prompt_tokens ? `of ${tok(c.prompt_tokens)} tokens` : "no model calls")}
    ${kpi("Saved by caching", usd(c.saved_micros), "vs full price")}
    ${kpi("Missed savings", usd(c.missed_micros), "estimate")}
    ${kpi("Unread cache writes", usd(c.unread_write_micros), "write premium")}
    ${kpi("Tool calls failing", t.calls ? pct(t.error_rate) : "–", t.calls ? `${tok(t.errors)} of ${tok(t.calls)}` : "none logged")}
    ${kpi("Wasted tool output", t.calls ? tok(t.wasted_tokens) : "–", "tokens")}
  </div>`;
  const findings = r.findings.length
    ? r.findings.map(findingCard).join("")
    : `<div class="card-b"><b>Nothing to fix.</b> <span class="muted">No cache misses or tool problems worth your time in this window.</span></div>`;
  const causes = r.cache.causes.length ? `<div class="card"><div class="card-h"><h2>Why prompts missed the cache</h2></div>
    <div class="table-wrap"><table><thead><tr><th>Cause</th><th class="num">Calls</th><th class="num">Tokens at full price</th><th class="num">Missed (estimate)</th></tr></thead><tbody>
    ${r.cache.causes.map(x => `<tr><td>${esc(x.label)}</td><td class="num">${tok(x.calls)}</td><td class="num">${tok(x.tokens)}</td><td class="num">${usd(x.missed_micros)}</td></tr>`).join("")}
    </tbody></table></div></div>` : "";
  const cacheRows = (rows, key) => rows.map(x => `<tr><td class="mono">${esc(key(x))}</td><td class="num">${tok(x.calls)}</td><td class="num">${tok(x.prompt_tokens)}</td>
    <td class="num">${pct(x.hit_rate)}</td><td class="num">${usd(x.cost_micros)}</td><td class="num">${usd(x.saved_micros)}</td><td class="num">${usd(x.missed_micros)}</td></tr>`).join("");
  const cacheHead = first => `<thead><tr><th>${first}</th><th class="num">Calls</th><th class="num">Prompt tokens</th><th class="num">From cache</th><th class="num">Cost</th><th class="num">Saved</th><th class="num">Missed</th></tr></thead>`;
  const byModel = r.cache.by_model.length ? `<div class="card"><div class="card-h"><h2>Cache by model</h2></div>
    <div class="table-wrap"><table>${cacheHead("Model")}<tbody>${cacheRows(r.cache.by_model, x => (x.provider ? x.provider + "/" : "") + x.model)}</tbody></table></div></div>` : "";
  const byAgent = r.cache.by_agent.length ? `<div class="card"><div class="card-h"><h2>Cache by agent</h2></div>
    <div class="table-wrap"><table>${cacheHead("Agent")}<tbody>${cacheRows(r.cache.by_agent, x => x.agent)}</tbody></table></div></div>` : "";
  const tools = r.tools.by_tool.length ? `<div class="card"><div class="card-h"><h2>Tool health</h2></div>
    <div class="table-wrap"><table><thead><tr><th>Tool</th><th class="num">Calls</th><th class="num">Failing</th><th class="num">Repeats</th><th class="num" title="A failed call followed by the identical call">Retried unchanged</th>
      <th class="num" title="Outputs over 25,000 tokens">Oversized</th><th class="num">p50 / p95</th><th class="num">p95 output</th><th class="num">Wasted tokens</th></tr></thead><tbody>
    ${r.tools.by_tool.map(x => `<tr><td class="mono">${esc(x.tool)}</td><td class="num">${tok(x.calls)}</td>
      <td class="num ${x.error_rate >= 0.2 && x.calls >= 10 ? "err" : ""}">${x.errors ? `${tok(x.errors)} (${pct(x.error_rate)})` : "0"}</td>
      <td class="num">${tok(x.repeats)}</td><td class="num">${tok(x.fail_retries)}</td><td class="num">${tok(x.oversized)}</td>
      <td class="num nowrap">${msShort(x.p50_ms)} / ${msShort(x.p95_ms)}</td><td class="num">${x.p95_output_tokens == null ? "–" : tok(x.p95_output_tokens)}</td><td class="num">${tok(x.wasted_tokens)}</td></tr>`).join("")}
    </tbody></table></div></div>` : "";
  const runs = r.tools.runs.length ? `<div class="card"><div class="card-h"><h2>Runs with tool problems</h2></div>
    <div class="table-wrap"><table><thead><tr><th>Run</th><th>Agent</th><th class="num">Tool calls</th><th class="num">Failing</th><th class="num">Repeats</th><th class="num">Retried unchanged</th></tr></thead><tbody>
    ${r.tools.runs.map(x => `<tr><td><a href="#" class="mono runlink" data-open-run="${esc(x.run)}">${esc(x.run)}</a></td><td>${esc(x.agent || "–")}</td>
      <td class="num">${tok(x.calls)}</td><td class="num">${tok(x.errors)}</td><td class="num">${tok(x.repeats)}</td><td class="num">${tok(x.fail_retries)}</td></tr>`).join("")}
    </tbody></table></div></div>` : "";
  const noTools = !t.calls ? `<p class="help">No tool calls logged in this window. Tool health comes from agent logs: the connect scripts' hooks, or <span class="mono">guard.log</span> / <span class="mono">POST /v1/steps</span> from the SDKs.</p>` : "";
  el.innerHTML = `${kpis}
    <div class="card"><div class="card-h"><h2>What to fix first</h2><span class="status">${r.findings.length} finding${r.findings.length === 1 ? "" : "s"}</span></div>${findings}</div>
    ${causes}${byModel}${byAgent}${tools}${runs}${noTools}
    <p class="help" style="margin-top:16px">Estimates: Tokenrein sees token counts, not prompts, so the prompt two calls of a run share is taken as the smaller of the two. Steps are kept as metadata only (names, outcomes, sizes, timings, input fingerprints).${r.sampled ? " This window has over a million rows, so the report covers the first million." : ""}</p>`;
}

$("#insights-days").addEventListener("change", e => {
  store("local", INSIGHT_DAYS_KEY, e.target.value);
  if (S.org) loadInsights(encodeURIComponent(S.org), true).then(renderInsights);
  renderInsights();
});
