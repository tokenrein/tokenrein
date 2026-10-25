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
  if (!root || (!t.events.length && !root.requests)) { el.innerHTML = `<h1 style="font-family:var(--mono)">${esc(S.runId)}</h1><p class="sub">No requests recorded for this run in the last 7 days.</p>`; return; }
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
  const evRows = t.events.map(e => {
    const d = depth.get(e.run_id) || 0;
    const u = e.usage;
    return `<tr>
      <td class="muted nowrap">${when(e.ts)}</td>
      <td class="nowrap">${d ? `<span class="tree-indent" style="width:${d * 16}px">↳</span>` : ""}<span class="mono">${esc(e.run_id)}</span></td>
      <td class="nowrap">${esc(e.agent)}${e.user ? `<div class="muted">${esc(e.user)}</div>` : ""}</td>
      <td class="mono nowrap">${esc(e.model)}</td>
      <td>${decisionBadge(e.decision)}${e.blocked_scope ? `<div class="muted mono">${esc(e.blocked_scope)}</div>` : ""}${(e.check_exceeded || []).length ? `<div><span class="badge b-warn">would block: ${esc(e.check_exceeded.join(", "))}</span></div>` : ""}</td>
      <td class="muted nowrap">${u ? `${(u.input || 0).toLocaleString()} in · ${(u.output || 0).toLocaleString()} out` : "–"}</td>
      <td class="num">${e.decision?.startsWith("allowed") ? usd(e.actual_micros) : "–"}</td>
      <td class="num muted">${e.upstream_ms ? e.upstream_ms + " ms" : "–"}</td></tr>`;
  }).join("");
  el.innerHTML = `
    <div class="inline" style="justify-content:space-between;align-items:flex-start;margin-bottom:12px">
      <div><h1 style="font-family:var(--mono);word-break:break-all">${esc(root.run_id)}</h1>
        <p class="sub" style="margin:0">${stateBadge(root.state)} ${liveness(root.last_seen)} · ${esc(root.project || "–")} / ${esc(root.agent || "–")}${root.parent_run_id ? ` · parent <a href="#" class="mono runlink" data-open-run="${esc(root.parent_run_id)}">${esc(root.parent_run_id)}</a>` : ""}</p></div>
      <div class="row-actions">${actions}</div></div>
    <div class="grid kpis">
      ${kpi("Spend", usd(cost), `incl. ${t.runs.length - 1} sub-agent run${t.runs.length === 2 ? "" : "s"}`)}
      ${kpi("Run limit", root.budget.limit_micros != null && root.budget.mode !== "skip" ? usd(root.budget.limit_micros) : "–", root.budget.mode ? esc(root.budget.mode) + " mode" : "no run limit")}
      ${kpi("Requests", String(t.events.length), blocked ? `${blocked} blocked` : "none blocked")}
      ${kpi("Tokens", tokens.toLocaleString(), dur >= 1 ? `over ${dur < 120 ? Math.round(dur) + "s" : Math.round(dur / 60) + " min"}` : "")}
    </div>
    <div class="card" style="margin-bottom:16px"><div class="card-h"><h2>Budget</h2></div><div class="card-b">${meter(root.budget)}</div></div>
    ${t.runs.length > 1 ? `<div class="card" style="margin-bottom:16px"><div class="card-h"><h2>Sub-agent runs</h2></div>
      <div class="table-wrap"><table class="wide"><thead><tr><th>Run</th><th>State</th><th>Agent</th><th class="num">Requests</th><th>Spend / run limit</th><th>Last seen</th></tr></thead><tbody>${subRows}</tbody></table></div></div>` : ""}
    <div class="card"><div class="card-h"><h2>Timeline</h2><span class="muted">every request in this run and its sub-agent runs, oldest first</span></div>
      <div class="table-wrap"><table class="wide"><thead><tr><th>Time</th><th>Run</th><th>Agent / user</th><th>Model</th><th>Decision</th><th>Tokens</th><th class="num">Cost</th><th class="num">Upstream</th></tr></thead><tbody>${evRows}</tbody></table></div></div>`;
}
