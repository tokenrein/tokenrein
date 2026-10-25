// TokenLens dashboard: Audit log page.
"use strict";

const AUDIT_ACTIONS = {
  "org.create": "Created org", "org.update": "Changed org settings", "org.delete": "Deleted org",
  "project.create": "Created project", "project.update": "Changed project limits", "project.delete": "Deleted project",
  "key.create": "Created key", "key.revoke": "Revoked key",
  "provider_key.set": "Saved provider key", "provider_key.remove": "Removed provider key", "provider_key.test": "Tested provider key",
  "pack.apply": "Applied guardrail pack", "pricing.update": "Changed model prices",
  "run.pause": "Paused run", "run.resume": "Resumed run", "run.kill": "Killed run",
};
const AUDIT_DANGER = new Set(["org.delete", "project.delete", "key.revoke", "provider_key.remove", "run.kill"]);

function auditPath(before) {
  const q = new URLSearchParams({ limit: "100" });
  // Only instance admins see the whole log; everyone else sees their org's.
  if ((S.auditOrgOnly || !S.me?.instance_admin) && S.org) q.set("org", S.org);
  if (before) q.set("before", before);
  return `/admin/audit?${q}`;
}

// Long internal fields that say nothing to a reader.
const AUDIT_HIDDEN = new Set(["hash", "sealed", "created_at", "updated_at", "virtual_key_hashes"]);
function auditValue(v) {
  if (v === null || v === undefined) return '<span class="muted">none</span>';
  let text;
  if (typeof v === "string") text = v;
  else if (v && typeof v === "object" && !Array.isArray(v)) {
    text = Object.entries(v).filter(([k]) => !AUDIT_HIDDEN.has(k))
      .map(([k, x]) => `${k}: ${typeof x === "string" ? x : JSON.stringify(x)}`).join(", ") || "{}";
  } else text = JSON.stringify(v);
  return `<span class="mono">${esc(text.length > 160 ? text.slice(0, 160) + "…" : text)}</span>`;
}

function auditChanges(e) {
  const list = e.detail?.changes || [];
  if (!list.length) return '<span class="muted">–</span>';
  // Paths inside the entry's own org read better without the org prefix.
  const prefix = e.org ? `orgs[${e.org}].` : "";
  const rows = list.map(c => {
    const path = c.path.startsWith(prefix) ? c.path.slice(prefix.length) : c.path;
    if (c.note) return `<li class="muted">${esc(c.note)}</li>`;
    const label = `<span class="mono">${esc(path)}</span>`;
    if (c.before === null) return `<li><span class="badge b-ok">added</span> ${label}: ${auditValue(c.after)}</li>`;
    if (c.after === null) return `<li><span class="badge b-bad">removed</span> ${label}: ${auditValue(c.before)}</li>`;
    return `<li>${label}: ${auditValue(c.before)} → ${auditValue(c.after)}</li>`;
  }).join("");
  return `<details class="changes"><summary>${list.length} change${list.length === 1 ? "" : "s"}</summary><ul>${rows}</ul></details>`;
}

function renderAudit() {
  const el = $("#audit");
  const a = S.audit;
  if (!can("admin")) { el.innerHTML = `<div class="card"><div class="card-b">Only admins and owners of ${esc(S.org || "this org")} can see its audit log.</div></div>`; return; }
  if (!a) { el.innerHTML = '<p class="muted">Loading…</p>'; return; }
  const q = S.auditQuery.trim().toLowerCase();
  const entries = q ? a.entries.filter(e => [e.actor, e.action, AUDIT_ACTIONS[e.action], e.target, e.org, e.ip]
    .some(v => (v || "").toLowerCase().includes(q))) : a.entries;
  const rows = entries.map(e => `<tr>
    <td class="nowrap">${esc(new Date(e.at).toLocaleString())}<div class="muted">${esc(e.actor)}${e.ip ? ` · ${esc(e.ip)}` : ""}</div></td>
    <td><span class="badge ${AUDIT_DANGER.has(e.action) ? "b-bad" : "b-muted"}">${esc(AUDIT_ACTIONS[e.action] || e.action)}</span>
      <div class="mono" style="word-break:break-all;margin-top:4px">${esc(e.target || "–")}</div>
      <div class="muted">${e.org ? `org ${esc(e.org)}` : "all orgs"}</div></td>
    <td style="min-width:240px">${auditChanges(e)}</td></tr>`).join("");
  const note = a.durable ? "" : `<div class="alert">This proxy has no database, so it keeps only the last 10,000 entries (in Redis). Set <span class="mono">TL_DATABASE_URL</span> to a Postgres database to keep the full history.</div>`;
  el.innerHTML = `${note}<div class="card">
    <div class="card-h">
      <input type="search" data-audit-q placeholder="Filter by person, action or target" aria-label="Filter audit log" value="${esc(S.auditQuery)}" style="min-width:240px">
      ${S.me?.instance_admin ? `<label class="inline status"><input type="checkbox" data-audit-org ${S.auditOrgOnly ? "checked" : ""}> Only org ${esc(S.org || "")}</label>` : `<span class="status">Org ${esc(S.org || "")}</span>`}
      <div class="toolbar"><button class="btn sm" data-audit-csv>Download CSV</button></div>
    </div>
    <div class="table-wrap"><table>${entries.length
      ? `<thead><tr><th>When / who</th><th>Action</th><th>What changed</th></tr></thead><tbody>${rows}</tbody>`
      : `<tr><td class="empty">${a.entries.length ? "No entries match." : "Nothing recorded yet. Changes made from now on show up here."}</td></tr>`}</table></div>
    ${a.next ? `<div class="card-b" style="text-align:center"><button class="btn sm" data-audit-more>Load older entries</button></div>` : ""}
  </div>`;
}

document.addEventListener("input", e => {
  if (e.target.matches("[data-audit-q]")) {
    S.auditQuery = e.target.value;
    const pos = e.target.selectionStart;
    renderAudit();
    const f = $("[data-audit-q]"); f.focus(); f.setSelectionRange(pos, pos);
  }
});
document.addEventListener("change", async e => {
  if (!e.target.matches("[data-audit-org]")) return;
  S.auditOrgOnly = e.target.checked;
  try { S.audit = await api(auditPath()); renderAudit(); }
  catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
});
document.addEventListener("click", async e => {
  const t = e.target;
  if (t.closest("[data-audit-more]")) {
    const b = t.closest("[data-audit-more]");
    b.disabled = true;
    try {
      const page = await api(auditPath(S.audit.next));
      S.audit = { ...page, entries: S.audit.entries.concat(page.entries) };
      renderAudit();
    } catch (err) { if (err.message !== "unauthorized") toast(err.message, true); b.disabled = false; }
  } else if (t.closest("[data-audit-csv]")) {
    const q = new URLSearchParams({ format: "csv" });
    if ((S.auditOrgOnly || !S.me?.instance_admin) && S.org) q.set("org", S.org);
    try {
      const res = await fetch(`/admin/audit?${q}`, { headers: S.token ? { Authorization: "Bearer " + S.token } : {} });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const url = URL.createObjectURL(await res.blob());
      const a = document.createElement("a");
      a.href = url; a.download = `tokenlens-audit-${new Date().toISOString().slice(0, 10)}.csv`;
      document.body.appendChild(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (err) { toast("Download failed: " + err.message, true); }
  }
});
