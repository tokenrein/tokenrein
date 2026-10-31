// Tokenrein dashboard: the license (Keys & limits page), its reminders and the paid-feature tags.
"use strict";

const PAID = {
  sso: "Single sign-on",
  audit_export: "Audit log export",
  long_history: "Usage history past 90 days",
  pagerduty: "PagerDuty alerts",
  cursor_import: "Cursor usage import",
  multiple_orgs: "More than one org",
};
const PLAN_NAMES = { team: "Team", enterprise: "Enterprise" };

async function loadLicense(force) {
  if (!force && S.license && Date.now() - S.licenseAt < 60000) return;
  try { S.license = await api("/admin/license", { quiet: true }); S.licenseAt = Date.now(); }
  catch (_) { /* reminders are best effort */ }
}

function licensed(feature) { return !!S.license?.features?.[feature]?.allowed; }

// A tag after a paid control while the install's key does not include it.
function paidTag(feature) {
  if (!S.license || licensed(feature)) return "";
  const plan = PLAN_NAMES[S.license.features?.[feature]?.plan] || "Team";
  return ` <a class="badge b-muted" href="${esc(S.license.pricing_url)}" target="_blank" rel="noopener" title="Needs a ${plan} license">${plan}</a>`;
}

// Above every page, for admins: over the free tier or the license, or the key running out.
function renderLicenseBanner() {
  const el = $("#license-banner");
  const l = S.license;
  let msg = "";
  if (l && (S.me?.instance_admin || can("admin"))) {
    const u = l.usage || {};
    const day = t => new Date(t).toLocaleDateString();
    if (l.invalid) msg = `The license key from the ${esc(l.source)} is not valid, so Tokenrein runs as the free tier.`;
    // The free tier has a spend limit; a license only counts developers.
    else if (l.over_limit && l.limits.spend_micros != null) msg = `Tokenrein has seen <b>${u.developers}</b> developers in the last 30 days and <b>${usd(u.spend_micros)}</b> of AI spend this month; the free tier covers ${l.limits.developers} developers and ${usd(l.limits.spend_micros)}. Agents keep working.`;
    else if (l.over_limit) msg = `Tokenrein has seen <b>${u.developers}</b> developers in the last 30 days; the license covers ${l.limits.developers}. Agents keep working.`;
    else if (l.standing === "expiring") msg = `The ${PLAN_NAMES[l.plan]} license expires on <b>${day(l.expires_at)}</b>.`;
    else if (l.standing === "grace") msg = `The license expired on ${day(l.expires_at)}. Paid features stop on <b>${day(l.grace_ends_at)}</b>.`;
    else if (l.standing === "expired") msg = "The license expired, so paid features are off. Settings and data are kept.";
  }
  el.classList.toggle("hide", !msg);
  el.innerHTML = msg ? `${msg} <a href="${esc(l.pricing_url)}" target="_blank" rel="noopener">See plans</a>${S.me?.instance_admin ? ` · <a href="#" data-goto="settings">Enter a key</a>` : ""}` : "";
}

const STANDING = {
  free: '<span class="badge b-muted">Free tier</span>',
  licensed: '<span class="badge b-ok">Licensed</span>',
  expiring: '<span class="badge b-warn">Expires soon</span>',
  grace: '<span class="badge b-warn">Expired: grace period</span>',
  expired: '<span class="badge b-bad">Expired</span>',
};

// Instance admins only: the key, usage against its limits, and what it unlocks.
function licenseCard() {
  const l = S.license;
  if (!l || !S.me?.instance_admin) return "";
  const u = l.usage || {};
  const plan = l.plan
    ? `${PLAN_NAMES[l.plan]} license for <b>${esc(l.customer)}</b>, until ${new Date(l.expires_at).toLocaleDateString()}`
    : "Free tier: every guardrail, up to the limits below.";
  const limit = (used, max, fmt) => max == null ? `${fmt(used)} <span class="muted">(no limit)</span>`
    : `<span class="${used > max ? "err" : ""}">${fmt(used)}</span> <span class="muted">of ${fmt(max)}</span>`;
  const features = Object.entries(PAID).map(([id, label]) => {
    const f = l.features[id];
    const tag = f.allowed ? '<span class="badge b-ok">On</span>' : `<span class="badge b-muted">${PLAN_NAMES[f.plan]}</span>`;
    return `<tr><td>${esc(label)}</td><td>${tag}</td></tr>`;
  }).join("");
  return `<div class="card" data-license-card>
    <div class="card-h"><h2>License</h2>${STANDING[l.standing] || ""}
      <div class="toolbar"><a class="btn sm" href="${esc(l.pricing_url)}" target="_blank" rel="noopener">See plans</a></div></div>
    <div class="card-b">
      <p style="margin-top:0">${plan}${l.source ? ` <span class="muted">Key from the ${esc(l.source)}.</span>` : ""}</p>
      ${l.invalid ? `<p class="err">The key from the ${esc(l.source)} is not valid: ${esc(l.invalid)}</p>` : ""}
      <div class="table-wrap" style="border:1px solid var(--line);border-radius:10px;margin-bottom:10px"><table><tbody>
        <tr><td>Developers in the last 30 days</td><td>${limit(u.developers, l.limits.developers, String)}</td></tr>
        <tr><td>AI spend this month</td><td>${limit(u.spend_micros, l.limits.spend_micros, usd)}</td></tr>
        ${features}
      </tbody></table></div>
      <p class="help">Developers are the people on runs: the <span class="mono">user</span> the connectors and SDKs send. Going over never blocks agents.</p>
      <form class="inline" data-license-key>
        <input type="password" required autocomplete="off" placeholder="Paste a license key" aria-label="License key" style="min-width:320px">
        <button class="btn primary sm" type="submit">Save key</button>
        ${l.source === "dashboard" ? '<button class="btn sm danger" type="button" data-license-remove>Remove key</button>' : ""}
      </form>
    </div></div>`;
}

// Capture phase, before the settings page's own form handler.
$("#settings").addEventListener("submit", async e => {
  const f = e.target.closest("[data-license-key]");
  if (!f) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  const input = $("input", f);
  const r = await mutate(() => api("/admin/license", { method: "PUT", body: { key: input.value.trim() } }));
  input.value = "";
  if (r) { S.license = r; S.licenseAt = Date.now(); toast(`${PLAN_NAMES[r.plan]} license saved`); render(); }
}, true);
$("#settings").addEventListener("click", async e => {
  if (!e.target.closest("[data-license-remove]")) return;
  if (!confirm("Remove the license key? Paid features turn off; settings and data are kept.")) return;
  await mutate(() => api("/admin/license", { method: "DELETE" }), "License key removed");
  await loadLicense(true);
  render();
});
