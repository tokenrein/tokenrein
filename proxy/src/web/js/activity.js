// Tokenrein dashboard: Activity page.
"use strict";

function renderEvents() {
  const f = $("#ev-filter").value;
  const events = S.events.filter(e => {
    const d = e.decision || "";
    if (f === "allowed") return d.startsWith("allowed");
    if (f === "blocked") return d.startsWith("blocked");
    if (f === "error") return !d.startsWith("allowed") && !d.startsWith("blocked");
    return true;
  });
  const tokens = u => u ? `${(u.input || 0).toLocaleString()} in · ${(u.output || 0).toLocaleString()} out` : "–";
  const rows = events.map(e => `<tr>
    <td class="muted nowrap">${when(e.ts)}</td>
    <td>${decisionBadge(e.decision)}${e.blocked_scope ? `<div class="muted mono">${esc(e.blocked_scope)}</div>` : ""}${(e.check_exceeded || []).length ? `<div><span class="badge b-warn">over: ${esc(e.check_exceeded.join(", "))}</span></div>` : ""}</td>
    <td class="mono">${esc(e.run_id)}${e.parent_run_id ? `<div class="muted">↳ ${esc(e.parent_run_id)}</div>` : ""}</td>
    <td class="nowrap">${esc(e.project)} / ${esc(e.agent)}</td>
    <td class="mono nowrap">${esc(e.provider)}<div class="muted">${esc(e.model)}</div></td>
    <td class="num">${usd(e.estimate_micros)}</td>
    <td class="num">${e.decision?.startsWith("allowed") ? usd(e.actual_micros) : "–"}</td>
    <td class="muted nowrap">${tokens(e.usage)}</td>
    <td class="num muted">${e.upstream_ms ? e.upstream_ms + " ms" : "–"}</td></tr>`).join("");
  $("#events-table").innerHTML = events.length
    ? `<thead><tr><th>Time</th><th>Decision</th><th>Run</th><th>Project / agent</th><th>Model</th><th class="num">Reserved</th><th class="num">Cost</th><th>Tokens</th><th class="num">Upstream</th></tr></thead><tbody>${rows}</tbody>`
    : `<tr><td class="empty">No requests yet.</td></tr>`;
}
