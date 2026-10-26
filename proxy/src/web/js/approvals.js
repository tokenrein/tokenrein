// Tokenrein dashboard: Approvals page (runs waiting for an approver).
"use strict";

function renderApprovals() {
  const el = $("#approvals");
  const a = S.approvals;
  if (!a) { el.innerHTML = '<p class="muted">Loading…</p>'; return; }
  if (a.error) { el.innerHTML = `<div class="card"><div class="card-b err">${esc(a.error)}</div></div>`; return; }
  const approver = can("approver");
  const pending = a.approvals.filter(x => x.status === "pending");
  const done = a.approvals.filter(x => x.status !== "pending");
  const why = x => x.kind === "loop"
    ? `<span class="badge b-warn">loop</span> ${esc(x.detail || "")}`
    : `<span class="badge b-info">budget</span> ${esc(x.detail || "")} <span class="muted mono">${esc(x.scope || "")}</span>`;
  const card = x => `<div class="card approval" data-approval="${esc(x.run_id)}">
    <div class="card-b approval-b">
      <div>
        <div class="approval-title"><a href="#" class="mono runlink" data-open-run="${esc(x.run_id)}">${esc(x.run_id)}</a></div>
        <div class="muted">${esc(x.project || "–")} / ${esc(x.agent || "–")} · asked ${esc(new Date(x.requested_at).toLocaleString())}</div>
        <div style="margin-top:6px">${why(x)}</div>
      </div>
      ${approver ? `<form class="inline approval-actions" data-approve="${esc(x.run_id)}">
        ${x.kind === "budget" ? `<label class="inline status">Add $<input type="number" min="0" step="any" value="${Math.max((x.limit_micros || 0) / 1e6, 1).toFixed(2)}" aria-label="Budget to add in USD" style="width:90px"></label>` : ""}
        <button class="btn primary sm" type="submit">${x.kind === "budget" ? "Approve" : "Let it continue"}</button>
        <button class="btn sm danger" type="button" data-deny="${esc(x.run_id)}">Stop the run</button>
      </form>` : `<span class="muted">An approver of ${esc(S.org)} decides.</span>`}
    </div></div>`;
  const rows = done.slice(0, 50).map(x => `<tr>
    <td class="mono">${esc(x.run_id)}</td><td>${why(x)}</td>
    <td>${x.status === "approved" ? '<span class="badge b-ok">approved</span>' : '<span class="badge b-bad">stopped</span>'}${x.added_micros ? ` <span class="muted">+${usd(x.added_micros)} budget</span>` : ""}</td>
    <td class="muted nowrap">${esc(x.decided_by || "")}<div>${x.decided_at ? esc(new Date(x.decided_at).toLocaleString()) : ""}</div></td></tr>`).join("");
  el.innerHTML = `${pending.length ? pending.map(card).join("") : `<div class="card"><div class="card-b"><b>Nothing is waiting.</b> <span class="muted">Runs land here when a run budget in <b>Ask</b> mode runs out, or when the loop guard pauses a runaway run. Set either under Keys &amp; limits.</span></div></div>`}
    <div class="card"><div class="card-h"><h2>Recent decisions</h2></div>
      <div class="table-wrap"><table>${rows ? `<thead><tr><th>Run</th><th>Why</th><th>Decision</th><th>By</th></tr></thead><tbody>${rows}</tbody>` : `<tr><td class="empty">No decisions yet.</td></tr>`}</table></div></div>`;
}

function renderApprovalBadge() {
  const n = S.approvals?.pending || 0;
  const b = $("#approvals-count");
  b.textContent = n ? String(n) : "";
  b.classList.toggle("hide", !n);
}

async function decideApproval(run, approve, body) {
  const o = encodeURIComponent(S.org);
  try {
    await api(`/admin/orgs/${o}/approvals/${encodeURIComponent(run)}/${approve ? "approve" : "deny"}`, { method: "POST", body });
    toast(approve ? `Run ${run} continues` : `Run ${run} stopped`);
    await refresh();
  } catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
}

$("#approvals").addEventListener("submit", async e => {
  const f = e.target.closest("[data-approve]");
  if (!f) return;
  e.preventDefault();
  const input = $("input", f);
  const add = input ? Number(input.value) : undefined;
  await decideApproval(f.dataset.approve, true, add !== undefined && add > 0 ? { add_usd: add } : {});
});
$("#approvals").addEventListener("click", async e => {
  const b = e.target.closest("[data-deny]");
  if (!b) return;
  if (!confirm(`Stop run ${b.dataset.deny} for good? It and its sub-agent runs are blocked.`)) return;
  await decideApproval(b.dataset.deny, false, {});
});
