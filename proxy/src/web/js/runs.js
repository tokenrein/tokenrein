// Tokenrein dashboard: Runs, Agents and the single-run page.
"use strict";

function runTable(runs, flat) {
  if (!runs.length) return `<tr><td class="empty">No runs yet. Requests through the proxy show up here within a second.</td></tr>`;
  let ordered = runs.map(r => [r, 0]);
  if (!flat) {
    const byId = new Map(runs.map(r => [r.run_id, r]));
    const kids = new Map();
    const roots = [];
    for (const r of runs) {
      if (r.parent_run_id && byId.has(r.parent_run_id) && r.parent_run_id !== r.run_id) {
        if (!kids.has(r.parent_run_id)) kids.set(r.parent_run_id, []);
        kids.get(r.parent_run_id).push(r);
      } else roots.push(r);
    }
    ordered = [];
    const seen = new Set();
    const walk = (r, depth) => {
      if (seen.has(r.run_id) || depth > 16) return;
      seen.add(r.run_id);
      ordered.push([r, depth]);
      for (const k of kids.get(r.run_id) || []) walk(k, depth + 1);
    };
    roots.forEach(r => walk(r, 0));
  }
  const rows = ordered.map(([r, depth]) => {
    const indent = depth ? `<span class="tree-indent" style="width:${depth * 16}px">↳</span>` : "";
    const actions = r.state === "killed" || !can("approver") ? '<span class="muted">–</span>' : `
      ${r.state === "paused"
        ? `<button class="btn sm" data-run-action="resume" data-run="${esc(r.run_id)}">Resume</button>`
        : `<button class="btn sm" data-run-action="pause" data-run="${esc(r.run_id)}">Pause</button>`}
      <button class="btn sm danger" data-run-action="kill" data-run="${esc(r.run_id)}">Kill</button>`;
    return `<tr>
      <td>${indent}<a href="#" class="mono runlink" data-open-run="${esc(r.run_id)}">${esc(r.run_id)}</a>${r.parent_run_id && flat ? `<div class="muted" style="font-size:12px">parent <span class="mono">${esc(r.parent_run_id)}</span></div>` : ""}</td>
      <td>${stateBadge(r.state)}</td>
      <td class="nowrap">${esc(r.project || "–")} / ${esc(r.agent || "–")}<div class="muted mono">${esc(r.model || "")}</div></td>
      <td class="num">${r.requests}${r.blocked ? ` <span class="badge b-bad">${r.blocked} blocked</span>` : ""}</td>
      <td>${meter(r.budget)}</td>
      <td class="muted nowrap">${ago(r.last_seen)}</td>
      <td><div class="row-actions">${actions}</div></td></tr>`;
  }).join("");
  return `<thead><tr><th>Run</th><th>State</th><th>Project / agent</th><th class="num">Requests</th><th>Spend / run limit</th><th>Last seen</th><th class="num">Actions</th></tr></thead><tbody>${rows}</tbody>`;
}
function renderRuns() {
  const q = $("#run-filter").value.trim().toLowerCase();
  const runs = q ? S.runs.filter(r => [r.run_id, r.project, r.agent, r.parent_run_id].some(v => (v || "").toLowerCase().includes(q))) : S.runs;
  $("#runs-table").innerHTML = runTable(runs, $("#run-flat").checked || !!q);
}
document.addEventListener("click", async e => {
  const b = e.target.closest("[data-run-action]");
  if (!b) return;
  const { runAction: action, run } = b.dataset;
  if (action === "kill" && !confirm(`Kill run ${run}? It and its sub-agent runs are blocked permanently.`)) return;
  b.disabled = true;
  try {
    await api(`/admin/orgs/${encodeURIComponent(S.org)}/runs/${encodeURIComponent(run)}/${action}`, { method: "POST" });
    toast(`Run ${run}: ${action === "resume" ? "resumed" : action === "pause" ? "paused" : "killed"}`);
    await refresh();
  } catch (err) { toast(err.message, true); b.disabled = false; }
});

function renderAgents() {
  const org = currentOrg();
  $("#agents-sub").textContent = `Spend per agent today (${S.overview.day.slice(0, 4)}-${S.overview.day.slice(4, 6)}-${S.overview.day.slice(6)} UTC), against the agent daily budget.`;
  const rows = S.agents.map(a => `<tr>
    <td>${liveness(a.last_seen)}</td>
    <td class="nowrap">${esc(a.project)}</td><td class="nowrap"><b>${esc(a.agent)}</b></td>
    <td>${meter(a.today)}</td><td>${modeBadge(a.today?.mode)}</td>
    <td class="muted nowrap">${ago(a.last_seen)}</td></tr>`).join("");
  $("#agents-table").innerHTML = S.agents.length
    ? `<thead><tr><th>Status</th><th>Project</th><th>Agent</th><th>Spend today / daily limit</th><th>Mode</th><th>Last seen</th></tr></thead><tbody>${rows}</tbody>`
    : `<tr><td class="empty">No agents seen yet${org ? "" : " (no org selected)"}.</td></tr>`;
}

// --- Run detail ---
function renderRun() {
  const el = $("#run-view");
  const t = S.run;
  if (!t || t.run_id !== S.runId) return;
  const root = t.runs[0];
  if (!root || (!t.events.length && !(t.steps || []).length && !root.requests)) { el.innerHTML = `<h1 style="font-family:var(--mono)">${esc(S.runId)}</h1><p class="sub">No requests recorded for this run in the last 7 days.</p>`; return; }
  const depth = new Map([[root.run_id, 0]]);
  for (const r of t.runs.slice(1)) depth.set(r.run_id, (depth.get(r.parent_run_id) ?? 0) + 1);
  const cost = t.events.reduce((a, e) => a + (e.decision?.startsWith("allowed") ? e.actual_micros || 0 : 0), 0);
  const blocked = t.events.filter(e => e.decision?.startsWith("blocked")).length;
  const tokens = t.events.reduce((a, e) => a + (e.usage ? (e.usage.input || 0) + (e.usage.output || 0) : 0), 0);
  const startTs = t.events[0]?.ts, endTs = t.events[t.events.length - 1]?.ts;
  const dur = startTs && endTs ? Math.max(0, (new Date(endTs) - new Date(startTs)) / 1000) : 0;
  const actions = root.state === "killed" || !can("approver") ? "" : `${root.state === "paused"
      ? `<button class="btn sm" data-run-action="resume" data-run="${esc(root.run_id)}">Resume</button>`
      : `<button class="btn sm" data-run-action="pause" data-run="${esc(root.run_id)}">Pause</button>`}
    <button class="btn sm danger" data-run-action="kill" data-run="${esc(root.run_id)}">Kill</button>`;
  const subRows = t.runs.slice(1).map(r => `<tr>
    <td><span class="tree-indent" style="width:${(depth.get(r.run_id) || 1) * 16}px">↳</span><a href="#" class="mono runlink" data-open-run="${esc(r.run_id)}">${esc(r.run_id)}</a></td>
    <td>${stateBadge(r.state)}</td><td class="nowrap">${esc(r.agent || "–")}</td>
    <td class="num">${r.requests}</td><td>${meter(r.budget)}</td><td class="muted nowrap">${ago(r.last_seen)}</td></tr>`).join("");
  const steps = t.steps || [];
  const trace = pairTools([...t.events.map(stepFromRequest), ...steps].sort((a, b) => traceStart(a) - traceStart(b)));
  const toolCalls = trace.filter(x => x.kind === "tool_call").length;
  const problems = trace.filter(x => x.status === "error" || x.status === "blocked");
  const repeats = repeatedCalls(steps);
  el.innerHTML = `
    <div class="inline" style="justify-content:space-between;align-items:flex-start;margin-bottom:12px">
      <div><h1 style="font-family:var(--mono);word-break:break-all">${esc(root.run_id)}</h1>
        <p class="sub" style="margin:0">${stateBadge(root.state)}${root.state === "killed" ? "" : `<span style="margin-left:8px">${liveness(root.last_seen)}</span>`} · ${esc(root.project || "–")} / ${esc(root.agent || "–")}${root.parent_run_id ? ` · parent <a href="#" class="mono runlink" data-open-run="${esc(root.parent_run_id)}">${esc(root.parent_run_id)}</a>` : ""}</p></div>
      <div class="run-head-r"><div class="row-actions">${actions}</div>${root.budget?.limit_micros != null && root.budget.mode !== "skip" ? `<div class="run-budget">${meter(root.budget)}</div>` : ""}</div></div>
    <div class="grid kpis">
      ${kpi("Spend", usd(cost), `incl. ${t.runs.length - 1} sub-agent run${t.runs.length === 2 ? "" : "s"}`)}
      ${kpi("Run limit", root.budget.limit_micros != null && root.budget.mode !== "skip" ? usd(root.budget.limit_micros) : "–", root.budget.mode ? esc(root.budget.mode) + " mode" : "no run limit")}
      ${kpi("Requests", String(t.events.length), blocked ? `${blocked} blocked` : "none blocked")}
      ${kpi("Tokens", tokens.toLocaleString(), dur >= 1 ? `over ${dur < 120 ? Math.round(dur) + "s" : Math.round(dur / 60) + " min"}` : "")}
      ${kpi("Tool calls", String(toolCalls), steps.length ? `${steps.length} agent steps logged` : "no agent steps logged")}
      ${kpi("Problems", String(problems.length), problems.length ? `<a href="#" data-trace-filter="problems">show them</a>` : "no errors or blocks")}
    </div>
    ${t.runs.length > 1 ? `<div class="card" style="margin-bottom:16px"><div class="card-h"><h2>Sub-agent runs</h2></div>
      <div class="table-wrap"><table class="wide"><thead><tr><th>Run</th><th>State</th><th>Agent</th><th class="num">Requests</th><th>Spend / run limit</th><th>Last seen</th></tr></thead><tbody>${subRows}</tbody></table></div></div>` : ""}
    ${repeats.length ? `<div class="alert"><b>Repeated tool calls.</b> ${repeats.slice(0, 3).map(([, c]) => `<span class="mono">${esc(c.name)}</span> with the same input ${c.n}×`).join(" · ")}. A loop like this is what the loop guard pauses.</div>` : ""}
    <div class="card" id="trace-card">${traceCard(trace, depth)}</div>`;
}

// --- Run trace: model calls and agent steps on one timeline ---
// Request events are written when a call ends; steps when they start.
function traceStart(x) { return new Date(x.ts).getTime() - (x.kind === "model_call" ? x.duration_ms || 0 : 0); }
const TRACE = { filter: "all", open: new Set(), run: null };
const TRACE_FILTERS = [["all", "All"], ["model_call", "Model calls"], ["tools", "Tools"], ["problems", "Problems"]];

function traceCard(trace, depth) {
  if (TRACE.run !== S.runId) { TRACE.run = S.runId; TRACE.open.clear(); TRACE.filter = "all"; }
  const shown = trace.filter(x => TRACE.filter === "all"
    || (TRACE.filter === "model_call" && x.kind === "model_call")
    || (TRACE.filter === "tools" && (x.kind === "tool_call" || x.kind === "tool_result"))
    || (TRACE.filter === "problems" && (x.status === "error" || x.status === "blocked")));
  const startOf = traceStart;
  const t0 = trace.reduce((m, x) => Math.min(m, startOf(x)), trace.length ? startOf(trace[0]) : 0);
  const tEnd = trace.reduce((m, x) => Math.max(m, startOf(x) + (x.duration_ms || 0)), t0);
  const span = Math.max(1, tEnd - t0);
  const capture = captureFor(S.run?.runs?.[0]?.project);
  const chips = TRACE_FILTERS.map(([k, label]) => `<button type="button" data-trace-filter="${k}" aria-pressed="${TRACE.filter === k}">${label}</button>`).join("");
  const rows = shown.map((x, i) => {
    const id = x.step_id || `${x.ts}:${x.kind}:${i}`;
    const open = TRACE.open.has(id);
    const d = depth.get(x.run_id) || 0;
    const start = startOf(x) - t0;
    const left = (start / span) * 100;
    const width = Math.max(0.6, ((x.duration_ms || 0) / span) * 100);
    const bad = x.status === "error" || x.status === "blocked";
    const cls = bad ? "bad" : x.kind === "model_call" ? "model" : x.kind.startsWith("tool") ? "tool" : "other";
    const rel = start < 1000 ? `+${start} ms` : `+${(start / 1000).toFixed(start < 10000 ? 2 : 1)} s`;
    return `<tr class="logrow ${open ? "open" : ""} ${bad ? "problem" : ""}" data-trace="${esc(id)}" tabindex="0" aria-expanded="${open}">
      <td class="muted nowrap" title="${esc(new Date(x.ts).toLocaleString())}">${rel}</td>
      <td class="nowrap">${kindChip(x.kind)}</td>
      <td>${d ? `<span class="tree-indent" style="width:${d * 16}px">↳</span>` : ""}${x.name ? `<span class="mono">${esc(x.name)}</span>` : `<span class="muted">${x.size ? bytes(x.size) : "–"}</span>`}${d ? ` <span class="muted mono">${esc(x.run_id)}</span>` : ""}${x.content ? `<div class="muted clip">${esc(x.content)}</div>` : ""}</td>
      <td>${statusBadge(x.status, x.level)}</td>
      <td class="wf-cell"><div class="wf"><span class="wf-bar ${cls}" style="left:${Math.min(99.4, left).toFixed(2)}%;width:${Math.min(100 - left, width).toFixed(2)}%"></span></div></td>
      <td class="num muted">${ms(x.duration_ms || null)}</td>
      <td class="num">${x.kind === "model_call" ? usd(x.cost_micros || 0) : ""}</td></tr>
      ${open ? `<tr class="logdetail"><td colspan="7">${stepDetail(x, capture)}</td></tr>` : ""}`;
  }).join("");
  const hint = trace.some(x => x.kind !== "model_call") ? "" : `<div class="card-b help" style="margin:0;border-top:1px solid var(--line)">Only model calls so far. Tool calls, prompts and sub-agents appear here when the agent is connected with hooks (Claude Code, Codex, Gemini CLI, Cursor, Copilot) or logs steps with <span class="mono">guard.log()</span>.</div>`;
  return `<div class="card-h"><h2>Trace</h2><span class="muted">${trace.length} steps over ${ms(span)}</span>
      <div class="toolbar seg" role="group" aria-label="Show">${chips}</div></div>
    <div class="table-wrap"><table class="wide trace"><thead><tr><th>Time</th><th>Kind</th><th>What</th><th>Status</th><th class="wf-h">Timeline</th><th class="num">Duration</th><th class="num">Cost</th></tr></thead>
    <tbody>${rows || `<tr><td colspan="7" class="empty">Nothing matches this filter.</td></tr>`}</tbody></table></div>${hint}`;
}
document.addEventListener("click", e => {
  const f = e.target.closest("[data-trace-filter]");
  if (f) { e.preventDefault(); TRACE.filter = f.dataset.traceFilter; renderRun(); $("#trace-card")?.scrollIntoView({ block: "nearest" }); return; }
  if (e.target.closest("a, button, pre")) return;
  const row = e.target.closest("[data-trace]");
  if (!row) return;
  const id = row.dataset.trace;
  TRACE.open.has(id) ? TRACE.open.delete(id) : TRACE.open.add(id);
  renderRun();
});
