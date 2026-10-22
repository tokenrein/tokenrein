// TokenLens dashboard: sign-in, navigation, data refresh and start-up (loaded last).
"use strict";

// --- auth ---
function signOut(message) {
  S.token = null;
  store("session", TOKEN_KEY, null);
  clearInterval(S.timer);
  $("#app").classList.add("hide");
  $("#login").classList.remove("hide");
  $("#login-err").textContent = message || "";
  $("#token").value = "";
  $("#token").focus();
}
$("#login-form").addEventListener("submit", async e => {
  e.preventDefault();
  S.token = $("#token").value.trim();
  $("#login-err").textContent = "";
  try {
    await api("/admin/overview");
    store("session", TOKEN_KEY, S.token);
    start();
  } catch (err) {
    if (err.message !== "unauthorized") $("#login-err").textContent = err.message;
  }
});
$("#signout").addEventListener("click", () => signOut());

// --- navigation ---
function showTab(tab) {
  const changed = S.tab !== tab;
  S.tab = tab;
  if (tab !== "run") store("local", TAB_KEY, tab);
  $$("nav.tabs button").forEach(b => b.setAttribute("aria-selected", String(b.dataset.tab === tab)));
  $$("[data-view]").forEach(v => v.classList.toggle("hide", v.dataset.view !== tab));
  render();
  if (changed && ["spend", "run", "overview", "models", "connect", "audit"].includes(tab) && S.token) refresh();
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
      if (S.tab === "audit") S.audit = await api(auditPath());
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
    if (["settings", "connect", "models", "audit"].includes(S.tab) || !$("#autorefresh").checked && S.tab === "runs") return;
    refresh();
  }, 5000);
}
S.token = store("session", TOKEN_KEY);
if (S.token) start(); else signOut();
