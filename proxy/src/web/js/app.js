// Tokenrein dashboard: sign-in, navigation, data refresh and start-up (loaded last).
"use strict";

// --- auth ---
// Show the sign-in screen with the ways this install allows (single sign-on, password, admin token).
async function showLogin(message) {
  clearInterval(S.timer);
  S.token = null; S.csrf = null; S.me = null;
  store("session", TOKEN_KEY, null);
  $("#app").classList.add("hide");
  $("#login").classList.remove("hide");
  $("#login-err").textContent = message || "";
  let cfg = S.authConfig;
  try { cfg = S.authConfig = await (await fetch("/auth/config")).json(); } catch (_) { /* keep the last one */ }
  cfg = cfg || { admin_token: true };
  const setup = !!S.setupToken;
  $("#setup-form").classList.toggle("hide", !setup);
  $("#sso-btn").classList.toggle("hide", setup || !cfg.sso);
  if (cfg.sso) $("#sso-btn").textContent = `Sign in with ${cfg.sso.label}`;
  $("#password-form").classList.toggle("hide", setup || !cfg.password_login);
  $("#login-or").classList.toggle("hide", setup || !(cfg.sso && cfg.password_login));
  const tokenOnly = !cfg.sso && !cfg.password_login;
  $("#token-login").classList.toggle("hide", setup || !cfg.admin_token);
  $("#token-login").open = tokenOnly;
  $("#login-title").textContent = setup ? "Set your password" : "Sign in to Tokenrein";
  const help = $("#login-help");
  help.classList.toggle("hide", !(cfg.database && cfg.users === 0 && !setup));
  help.innerHTML = cfg.database && cfg.users === 0
    ? `No accounts yet. Run <span class="mono">tokenrein create-owner you@company.com</span> on the server for a link to set the first password, or sign in with the admin token and add people under People &amp; access.`
    : "";
  const focus = setup ? "#setup-password" : cfg.password_login ? "#email" : "#token";
  $(focus)?.focus();
}
function signOut(message) { showLogin(message); }

async function signedIn(csrf) {
  S.csrf = csrf || null;
  S.me = await api("/auth/me", { quiet: true });
  S.csrf = S.me.csrf || S.csrf;
  start();
}

$("#password-form").addEventListener("submit", async e => {
  e.preventDefault();
  $("#login-err").textContent = "";
  const res = await fetch("/auth/login", { method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email: $("#email").value.trim(), password: $("#password").value }) });
  const data = await res.json().catch(() => ({}));
  $("#password").value = "";
  if (!res.ok) { $("#login-err").textContent = data.error || `HTTP ${res.status}`; return; }
  try { await signedIn(data.csrf); } catch (err) { $("#login-err").textContent = err.message; }
});
$("#setup-form").addEventListener("submit", async e => {
  e.preventDefault();
  $("#login-err").textContent = "";
  const res = await fetch("/auth/setup", { method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token: S.setupToken, password: $("#setup-password").value, name: $("#setup-name").value.trim() || null }) });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) { $("#login-err").textContent = data.error || `HTTP ${res.status}`; return; }
  S.setupToken = null;
  $("#setup-password").value = "";
  try { await signedIn(data.csrf); } catch (err) { $("#login-err").textContent = err.message; }
});
$("#login-form").addEventListener("submit", async e => {
  e.preventDefault();
  S.token = $("#token").value.trim();
  $("#login-err").textContent = "";
  try {
    await signedIn();
    store("session", TOKEN_KEY, S.token);
    $("#token").value = "";
  } catch (err) {
    S.token = null;
    $("#login-err").textContent = err.message === "HTTP 401" || /token/.test(err.message) ? "That admin token was not accepted." : err.message;
  }
});
$("#signout").addEventListener("click", async () => {
  if (S.csrf) { try { await api("/auth/logout", { method: "POST", quiet: true }); } catch (_) { /* signed out anyway */ } }
  showLogin();
});

function renderWhoami() {
  const who = S.me?.via === "admin_token" ? "Admin token" : S.me?.email || "";
  const role = S.me?.instance_admin ? "instance admin" : roleIn() || "no role";
  $("#whoami").innerHTML = `<b>${esc(who)}</b><br>${esc(role)}${S.org && !S.me?.instance_admin ? ` in ${esc(S.org)}` : ""}`;
  $("#whoami").title = who;
}

// --- navigation ---
function showTab(tab) {
  const changed = S.tab !== tab;
  S.tab = tab;
  if (tab !== "run") store("local", TAB_KEY, tab);
  if (changed) { S.peopleNotice = ""; S.alertsNotice = ""; } // one-time secrets are shown once
  $$("nav.tabs button").forEach(b => b.setAttribute("aria-selected", String(b.dataset.tab === tab)));
  $$("[data-view]").forEach(v => v.classList.toggle("hide", v.dataset.view !== tab));
  render();
  if (changed && ["spend", "run", "overview", "models", "connect", "audit", "people", "approvals", "settings"].includes(tab) && S.me) refresh();
}
function openRun(id) {
  S.runId = id; S.run = null;
  $("#run-view").innerHTML = `<p class="muted">Loading run ${esc(id)}…</p>`;
  showTab("run");
  window.scrollTo(0, 0);
}
document.addEventListener("click", e => {
  const a = e.target.closest("[data-open-run]");
  if (a) { e.preventDefault(); openRun(a.dataset.openRun); }
});
$$("nav.tabs button").forEach(b => b.addEventListener("click", () => showTab(b.dataset.tab)));
document.addEventListener("click", e => {
  const g = e.target.closest("[data-goto]");
  if (g) { e.preventDefault(); showTab(g.dataset.goto); }
});
$("#org").addEventListener("change", () => { S.org = $("#org").value; store("local", ORG_KEY, S.org); refresh(); });
["#run-filter", "#run-flat", "#ev-filter", "#spend-table-toggle"].forEach(sel => $(sel).addEventListener("input", render));
$("#spend-days").addEventListener("change", refresh);

// --- data ---
async function refresh() {
  try {
    S.overview = await api("/admin/overview");
    await loadLicense(S.tab === "settings");
    const orgs = S.overview.orgs.map(o => o.id);
    if (!orgs.includes(S.org)) S.org = orgs[0] || null;
    const sel = $("#org");
    sel.innerHTML = orgs.map(id => `<option ${id === S.org ? "selected" : ""}>${esc(id)}</option>`).join("");
    if (S.org) {
      const o = encodeURIComponent(S.org);
      const [runs, agents, events, review] = await Promise.all([
        api(`/admin/orgs/${o}/runs?limit=300`),
        api(`/admin/orgs/${o}/agents?limit=200`),
        api(`/admin/orgs/${o}/events?limit=200`),
        S.tab === "overview" ? api(`/admin/orgs/${o}/review`) : Promise.resolve(S.review),
      ]);
      S.runs = runs.runs; S.agents = agents.agents; S.events = events.events; S.review = review;
      if (!S.packs.length) S.packs = (await api("/admin/packs")).packs;
      if (S.tab === "spend") S.spend = await api(`/admin/orgs/${o}/spend?days=${$("#spend-days").value}`);
      if (S.tab === "models") S.models = await api(`/admin/models?org=${o}`);
      if (S.tab === "run" && S.runId) S.run = await api(`/admin/orgs/${o}/runs/${encodeURIComponent(S.runId)}/timeline`);
      if (S.tab === "audit") S.audit = can("admin") ? await api(auditPath()) : null;
      if (S.tab === "people") await loadPeople();
      if (S.tab === "settings") await loadAlerts();
      S.approvals = await api(`/admin/orgs/${o}/approvals`).catch(err => ({ error: err.message, approvals: [], pending: 0 }));
    } else {
      S.runs = []; S.agents = []; S.events = []; S.review = null;
    }
    $("#updated").textContent = "Updated " + new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
    render();
  } catch (err) {
    if (err.message !== "unauthorized") $("#updated").textContent = "Update failed: " + err.message;
  }

}

// --- rendering ---
function render() {
  if (!S.overview) return;
  if (S.tab === "overview") renderOverview();
  if (S.tab === "runs") renderRuns();
  if (S.tab === "agents") renderAgents();
  if (S.tab === "spend") renderSpend();
  if (S.tab === "connect") renderConnect();
  if (S.tab === "models") renderModels();
  if (S.tab === "run") renderRun();
  if (S.tab === "activity") renderEvents();
  if (S.tab === "settings" && !S.editing) renderSettings();
  if (S.tab === "audit") renderAudit();
  if (S.tab === "people") renderPeople();
  if (S.tab === "approvals") renderApprovals();
  renderApprovalBadge();
  renderLicenseBanner();
  renderWhoami();
  // Org admins see their org's audit log; instance admins see everything.
  $("nav.tabs button[data-tab=audit]").classList.toggle("hide", !can("admin"));
}

// --- start ---
function start() {
  $("#login").classList.add("hide");
  $("#app").classList.remove("hide");
  S.org = store("local", ORG_KEY) || null;
  showTab(store("local", TAB_KEY) || "overview");
  refresh();
  clearInterval(S.timer);
  S.timer = setInterval(() => {
    if (document.hidden) return;
    if (["settings", "connect", "models", "audit", "people"].includes(S.tab) || !$("#autorefresh").checked && S.tab === "runs") return;
    refresh();
  }, 5000);
}
// Start: a setup link or a single sign-on error in the address, else the session (cookie) or the
// admin token kept in this tab.
(async function boot() {
  const hash = location.hash;
  let message = "";
  if (hash.startsWith("#setup=")) S.setupToken = decodeURIComponent(hash.slice(7));
  if (hash.startsWith("#login-error=")) message = decodeURIComponent(hash.slice(13).replace(/\+/g, " "));
  // Links from alerts open a page: #tab=approvals (kept through sign-in).
  const tab = hash.startsWith("#tab=") ? hash.slice(5) : "";
  if ($(`nav.tabs button[data-tab="${CSS.escape(tab)}"]`)) store("local", TAB_KEY, tab);
  if (hash) history.replaceState(null, "", location.pathname);
  if (S.setupToken || message) return showLogin(message);
  S.token = store("session", TOKEN_KEY);
  try { await signedIn(); } catch (_) { showLogin(); }
})();
// An alert link opened in a tab that already shows the dashboard.
window.addEventListener("hashchange", () => {
  const tab = location.hash.startsWith("#tab=") ? location.hash.slice(5) : "";
  if (!S.me || !$(`nav.tabs button[data-tab="${CSS.escape(tab)}"]`)) return;
  history.replaceState(null, "", location.pathname);
  showTab(tab);
});
