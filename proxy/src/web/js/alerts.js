// Tokenrein dashboard: alert channels (Keys & limits page).
"use strict";

const ALERT_KINDS = {
  slack: ["Slack", "https://hooks.slack.com/services/…", "Incoming webhook URL from a Slack app."],
  teams: ["Microsoft Teams", "https://…webhook.office.com/… or a Workflows URL", "A Teams channel's incoming webhook or Workflows “post to a channel” URL."],
  webhook: ["Webhook", "https://example.com/tokenrein-alerts", "Any HTTPS endpoint. Requests are signed with HMAC-SHA256 (x-tokenrein-signature)."],
  pagerduty: ["PagerDuty", "Integration (routing) key", "An Events API v2 integration key from a PagerDuty service."],
};
const ALERT_EVENTS = {
  budget_warning: "Budget 80% used",
  budget_blocked: "Budget used up (requests refused)",
  approval_requested: "Run waiting for approval",
  loop_detected: "Runaway loop detected",
};

async function loadAlerts() {
  if (!S.org || !can("admin")) { S.alerts = null; return; }
  try { S.alerts = await api(`/admin/orgs/${encodeURIComponent(S.org)}/alerts`); }
  catch (err) { S.alerts = { error: err.message }; }
}

function alertsCard() {
  const a = S.alerts;
  if (!a) return `<div class="card" data-alerts-card><div class="card-h"><h2>Alerts</h2></div><div class="card-b muted">Loading…</div></div>`;
  if (a.error) return `<div class="card" data-alerts-card><div class="card-h"><h2>Alerts</h2></div><div class="card-b err">${esc(a.error)}</div></div>`;
  const rows = a.channels.map(c => {
    const d = c.last_delivery;
    const status = !d ? '<span class="muted">nothing sent yet</span>'
      : d.ok ? `<span class="badge b-ok">delivered</span> <span class="muted">${esc(new Date(d.at).toLocaleString())}</span>`
      : `<span class="badge b-bad">failed</span> <span class="muted">${esc(d.error || "")} · ${esc(new Date(d.at).toLocaleString())}</span>`;
    const events = c.events.length ? c.events.map(e => esc(ALERT_EVENTS[e] || e)).join(", ") : "All events";
    return `<tr><td><b>${esc(c.name)}</b><div class="muted">${esc(ALERT_KINDS[c.kind]?.[0] || c.kind)} · …${esc(c.last4)}${c.signed ? " · signed" : ""}</div></td>
      <td class="muted">${events}</td><td>${status}</td>
      <td><div class="row-actions"><button class="btn sm" type="button" data-alert-test="${esc(c.id)}">Test</button>
        <button class="btn sm danger" type="button" data-alert-remove="${esc(c.id)}" data-name="${esc(c.name)}">Remove</button></div></td></tr>`;
  }).join("");
  const kind = S.alertKind || "slack";
  return `<div class="card" data-alerts-card>
    <div class="card-h"><h2>Alerts for <span class="mono">${esc(S.org)}</span></h2></div>
    <div class="card-b" style="padding-bottom:0"><p class="help">Where Tokenrein tells people about budgets running out, refused requests, runs waiting for approval and runaway loops. ${a.links ? "Alerts link back to this dashboard." : "Set <span class=\"mono\">auth.public_url</span> in the proxy config so alerts link back to this dashboard."}${a.secrets_enabled ? "" : " <b>Saving channels needs the proxy's secrets key</b> (URLs and keys are stored encrypted)."}</p></div>
    <div class="table-wrap"><table>${rows ? `<thead><tr><th>Channel</th><th>Events</th><th>Last delivery</th><th></th></tr></thead><tbody>${rows}</tbody>` : `<tr><td class="empty">No channels yet.</td></tr>`}</table></div>
    <form class="card-b alert-form" data-alert-new>
      <div class="inline">
        <select name="kind" aria-label="Channel type">${Object.entries(ALERT_KINDS).map(([k, [label]]) => `<option value="${k}" ${k === kind ? "selected" : ""}>${esc(label)}</option>`).join("")}</select>
        <input type="text" name="name" required maxlength="100" placeholder="Name, e.g. #ai-spend" aria-label="Channel name" style="min-width:180px">
        <input type="${kind === "pagerduty" ? "password" : "url"}" name="target" required autocomplete="off" placeholder="${esc(ALERT_KINDS[kind][1])}" aria-label="${kind === "pagerduty" ? "Routing key" : "Webhook URL"}" style="flex:1;min-width:260px">
      </div>
      <p class="help" style="margin:0">${esc(ALERT_KINDS[kind][2])}</p>
      <div class="inline">${Object.entries(ALERT_EVENTS).map(([k, label]) => `<label class="inline status"><input type="checkbox" name="events" value="${k}" checked> ${esc(label)}</label>`).join("")}</div>
      <div><button class="btn" type="submit" ${a.secrets_enabled ? "" : "disabled"}>Add channel</button></div>
    </form>
  </div>`;
}

async function alertMutate(fn, ok) {
  try { const r = await fn(); if (ok) toast(ok); await loadAlerts(); S.editing = false; renderSettings(); return r; }
  catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
}

$("#settings").addEventListener("change", e => {
  if (e.target.matches("[data-alert-new] [name=kind]")) { S.alertKind = e.target.value; S.editing = false; renderSettings(); }
});
$("#settings").addEventListener("click", async e => {
  const b = e.target.closest("button");
  if (!b) return;
  const o = encodeURIComponent(S.org);
  if (b.matches("[data-alert-test]")) {
    b.disabled = true; b.textContent = "Sending…";
    try { const r = await api(`/admin/orgs/${o}/alerts/${b.dataset.alertTest}/test`, { method: "POST" }); toast(r.message, !r.ok); }
    catch (err) { toast(err.message, true); }
    await loadAlerts(); S.editing = false; renderSettings();
  } else if (b.matches("[data-alert-remove]")) {
    if (!confirm(`Remove the alert channel "${b.dataset.name}"?`)) return;
    await alertMutate(() => api(`/admin/orgs/${o}/alerts/${b.dataset.alertRemove}`, { method: "DELETE" }), "Channel removed");
  }
});
$("#settings").addEventListener("submit", async e => {
  const f = e.target.closest("[data-alert-new]");
  if (!f) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  const events = $$("[name=events]", f).filter(x => x.checked).map(x => x.value);
  const body = { kind: f.elements.kind.value, name: f.elements.name.value.trim(), target: f.elements.target.value.trim(),
    events: events.length === Object.keys(ALERT_EVENTS).length ? [] : events };
  const r = await alertMutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/alerts`, { method: "POST", body }), `${body.name} added`);
  if (r?.signing_secret) {
    S.alertsNotice = oneTime("Webhook signing secret", r.signing_secret,
      "Copy it now: it is shown only once. Check each request's x-tokenrein-signature header: sha256=HMAC-SHA256(secret, timestamp + \".\" + body), with the timestamp from x-tokenrein-timestamp.");
    S.editing = false; renderSettings();
  }
}, true);
