// Tokenrein dashboard: Overview page, Get started checklist and the Check-mode review.
"use strict";

function renderOverview() {
  const org = currentOrg();
  if (!org) { $("#kpis").innerHTML = ""; $("#projects-table").innerHTML = `<tr><td class="empty">No orgs yet. Create one under Keys &amp; limits.</td></tr>`; return; }
  const m = S.overview.month;
  $("#ov-sub").textContent = `Org ${org.id} · ${m.slice(0, 4)}-${m.slice(4)} (UTC) · ${S.overview.managed ? "managed from this dashboard" : "read from the config file"}`;
  const om = org.spend.org_monthly;
  const hour = Date.now() / 1000 - 3600;
  const activeRuns = S.runs.filter(r => r.last_seen >= hour).length;
  const recent = S.events.length;
  const blocked = S.events.filter(e => (e.decision || "").startsWith("blocked")).length;
  const spentToday = S.agents.reduce((a, x) => a + (x.today?.spent_micros || 0), 0);
  const now = Date.now() / 1000;
  const liveAgents = S.agents.filter(a => now - a.last_seen <= LIVE_SECS).length;
  $("#kpis").innerHTML = [
    kpi("Spend this month", usd(om?.spent_micros || 0), om?.limit_micros != null && om?.mode !== "skip" ? `of ${usd(om.limit_micros)} (${esc(om.mode)})` : "no org limit"),
    kpi("Spend today", usd(spentToday), "across all agents"),
    kpi("Live agents", `${liveAgents ? '<span class="live on" style="font-size:inherit"><span class="dot"></span></span> ' : ""}${liveAgents}`, `of ${S.agents.length} seen · <a href="#" data-goto="agents">view agents</a>`),
    kpi("Active runs", String(activeRuns), "seen in the last hour"),
    kpi("Blocked requests", String(blocked), `of the last ${recent} requests`),
    kpi("Waiting for approval", String(S.approvals?.pending || 0), S.approvals?.pending ? `<a href="#" data-goto="approvals">open approvals</a>` : "no runs waiting"),
  ].join("");
  renderGetStarted(org);
  renderReview(org);
  const rows = org.projects.map(p => `
    <tr><td><b>${esc(p.id)}</b></td><td>${meter(p.spend.project_monthly)}</td>
    <td>${modeBadge(p.effective.project_monthly?.mode)}</td>
    <td class="num">${p.keys.length}</td></tr>`).join("");
  $("#projects-table").innerHTML = org.projects.length
    ? `<thead><tr><th>Project</th><th>Spend / monthly limit</th><th>Mode</th><th class="num">Keys</th></tr></thead><tbody>${rows}</tbody>`
    : `<tr><td class="empty">No projects. Create one under Keys &amp; limits.</td></tr>`;
  $("#ov-runs").innerHTML = runTable(S.runs.slice(0, 8), true);
}

// --- Get started ---
const GS_HIDE = "tokenrein.getstarted.hidden.";
function snippet(kind, project) {
  const base = proxyUrl();
  const key = "tr_…  # the key you created for " + project;
  if (kind === "python") return `pip install "tokenrein[anthropic]"
export TOKENREIN_BASE_URL=${base}
export TOKENREIN_API_KEY=tr_…   # the key you created for ${project}

from anthropic import Anthropic
from tokenrein import guard
client = guard.anthropic(Anthropic(), agent="my-agent", user="you")
with guard.run("first-run"):
    client.messages.create(model="claude-haiku-4-5", max_tokens=50,
                           messages=[{"role": "user", "content": "Say hi"}])`;
  if (kind === "typescript") return `npm install @tokenrein/sdk @anthropic-ai/sdk
export TOKENREIN_BASE_URL=${base}
export TOKENREIN_API_KEY=tr_…   # the key you created for ${project}

import Anthropic from '@anthropic-ai/sdk';
import { guard } from '@tokenrein/sdk';
const client = guard.anthropic(new Anthropic(), { agent: 'my-agent', user: 'you' });
await guard.run('first-run', () =>
  client.messages.create({ model: 'claude-haiku-4-5', max_tokens: 50,
    messages: [{ role: 'user', content: 'Say hi' }] }));`;
  if (kind === "any") return `# Any OpenAI / Anthropic SDK or HTTP client: change two things.
base URL:  ${base}        (OpenAI SDKs: ${base}/v1)
API key:   tr_…           (the key you created for ${project})

# Optional headers
x-tr-agent: my-agent      x-tr-user: you
x-tr-run-id: first-run    x-tr-run-budget-usd: 1`;
  return `curl ${base}/v1/messages \\
  -H "x-api-key: tr_…" -H "anthropic-version: 2023-06-01" \\
  -H "content-type: application/json" \\
  -H "x-tr-agent: my-agent" -H "x-tr-run-id: first-run" \\
  -d '{"model":"claude-haiku-4-5","max_tokens":50,"messages":[{"role":"user","content":"Say hi"}]}'`;
}
function gsSteps(org) {
  const providerOk = Object.values(org.providers || {}).some(p => p && p.source);
  const keyProject = org.projects.find(p => p.keys.length) || org.projects[0];
  const hasKey = org.projects.some(p => p.keys.length);
  const first = S.events[S.events.length - 1];
  const traffic = S.events.length > 0 || S.agents.length > 0;
  const packed = org.projects.find(p => p.pack);
  const enforced = packed && ["run", "agent_daily"].some(k => packed.effective[k]?.mode === "enforce");
  return [
    { done: providerOk, title: "Connect a provider",
      doneText: "Provider key saved.",
      body: `<p class="help" style="margin:0">Save your Anthropic or OpenAI API key. Tokenrein uses it to call the provider; your agents never see it.</p>
        <div><button class="btn primary sm" data-goto-settings="providers">Add a provider key</button></div>` },
    { done: hasKey, title: "Create a project key",
      body: `<p class="help" style="margin:0">Agents authenticate with a Tokenrein key per project, so you can see and limit each project separately.</p>
        <div><button class="btn primary sm" data-goto-settings="keys">Create a key</button></div>` },
    { done: traffic, title: "Connect your agent",
      doneText: first ? `First request received from <b>${esc(first.agent)}</b> (${esc(first.model)}).` : "Requests are arriving.",
      body: `<p class="help" style="margin:0">Connect Claude Code with one command, or wrap your app's OpenAI or Anthropic client in one line. Calls keep going straight to the provider.</p>
        <div class="inline"><button class="btn primary sm" data-goto="connect">Open Connect</button><span class="waiting"><span class="live on"><span class="dot"></span></span>Waiting for your first request…</span></div>` },
    { done: !!packed, title: "Pick a guardrail pack",
      doneText: packed ? `<b>${esc(packed.id)}</b> uses the ${esc(packed.pack)} pack.` : "",
      body: `<p class="help" style="margin:0">A pack sets sensible limits for a kind of agent, all in <b>Check</b> mode: nothing is blocked yet, you see what would be.</p>
        <form class="inline" data-gs-pack>
          <select aria-label="Project">${org.projects.map(p => `<option>${esc(p.id)}</option>`).join("")}</select>
          <select aria-label="Pack">${S.packs.map(p => `<option value="${esc(p.id)}">${esc(p.title)}</option>`).join("")}</select>
          <button class="btn primary sm" type="submit">Apply in Check mode</button>
        </form>
        <div class="help" style="margin:0">${S.packs.map(p => `<div><b>${esc(p.title)}</b>: ${esc(p.description)}</div>`).join("")}</div>` },
    { done: !!enforced, title: "Review and enforce",
      body: `<p class="help" style="margin:0">Let real traffic run in Check mode, then look at what <b>would have been blocked</b> below and switch the limits you trust to <b>Enforce</b>.</p>` },
  ];
}
function renderGetStarted(org) {
  const el = $("#get-started");
  const steps = gsSteps(org);
  const done = steps.filter(s => s.done).length;
  if (done === steps.length || !can("admin") || store("local", GS_HIDE + org.id) === "1") { el.innerHTML = ""; return; }
  const current = steps.findIndex(s => !s.done);
  el.innerHTML = `<div class="card" style="margin-bottom:16px">
    <div class="card-h"><h2>Get started</h2>
      <div class="progress" aria-label="${done} of ${steps.length} steps done"><span style="width:${(done / steps.length) * 100}%"></span></div>
      <span class="muted">${done} of ${steps.length}</span>
      <div class="toolbar"><button class="btn sm" data-gs-hide>Hide</button></div></div>
    <ol class="steps">${steps.map((s, i) => `<li class="step ${s.done ? "done" : i === current ? "current" : ""}">
      <span class="num">${s.done ? "✓" : i + 1}</span>
      <div><div class="t">${esc(s.title)}</div>${s.done && s.doneText ? `<div class="muted" style="font-size:13px">${s.doneText}</div>` : ""}</div>
      ${i === current ? `<div class="body">${s.body}</div>` : ""}
    </li>`).join("")}</ol></div>`;
}
document.addEventListener("click", async e => {
  const t = e.target;
  if (t.closest("[data-gs-hide]")) { store("local", GS_HIDE + S.org, "1"); render(); }
  else if (t.closest("[data-snip]")) { S.snip = t.closest("[data-snip]").dataset.snip; render(); }
  else if (t.closest("[data-copy-snip]")) {
    try { await navigator.clipboard.writeText($("pre.snip").textContent); toast("Copied"); } catch (_) { toast("Copy failed: select the text and copy it", true); }
  } else if (t.closest("[data-goto-settings]")) {
    const where = t.closest("[data-goto-settings]").dataset.gotoSettings;
    showTab("settings");
    const target = where === "providers" ? $("[data-providers-card]") : $("[data-new-key]");
    target?.scrollIntoView({ behavior: "smooth", block: "center" });
    if (where === "keys") $("input", target)?.focus();
  }
});
document.addEventListener("submit", async e => {
  const f = e.target.closest("[data-gs-pack]");
  if (!f) return;
  e.preventDefault();
  const [project, pack] = $$("select", f).map(x => x.value);
  await applyPack(project, pack);
});
async function applyPack(project, pack) {
  const title = S.packs.find(p => p.id === pack)?.title || pack;
  if (!confirm(`Apply the "${title}" pack to ${project}? It replaces this project's limits with the pack's, all in Check mode (nothing is blocked).`)) return;
  await mutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/projects/${encodeURIComponent(project)}/pack`, { method: "POST", body: { pack } }), `${title} applied to ${project} in Check mode`);
}

// --- Check-mode review ---
const POLICY_NAMES = { run: "Run budget", agent_daily: "Agent per day", project_monthly: "Project per month", org_monthly: "Org per month" };
function renderReview(org) {
  const el = $("#review");
  const r = S.review;
  if (!r || !r.events_scanned) { el.innerHTML = ""; return; }
  const items = r.items.filter(i => i.current && i.current.mode === "check");
  const anyCheck = org.projects.some(p => ["run", "agent_daily", "project_monthly"].some(k => p.effective[k]?.mode === "check"));
  if (!items.length && !anyCheck) { el.innerHTML = ""; return; }
  const since = r.oldest_ts ? new Date(r.oldest_ts).toLocaleString() : "";
  const total = r.check_requests, cost = r.check_cost_micros;
  const rows = items.map(i => `<tr>
    <td><b>${esc(POLICY_NAMES[i.policy] || i.policy)}</b><div class="muted">${i.project ? "project " + esc(i.project) : "whole org"} · limit ${usd(i.current.limit_usd * 1e6)}</div></td>
    <td class="num">${i.requests}</td>
    <td class="num">${usd(i.cost_micros)}</td>
    <td>${i.run_count} run${i.run_count === 1 ? "" : "s"}<div class="muted mono">${i.runs.map(x => `<a href="#" class="runlink" data-open-run="${esc(x)}">${esc(x)}</a>`).join(", ")}</div></td>
    <td><div class="row-actions">${can("admin") ? `<button class="btn primary sm" data-enforce="${esc(i.policy)}" data-project="${esc(i.project || "")}">Switch to Enforce</button>` : ""}</div></td></tr>`).join("");
  el.innerHTML = `<div class="card" style="margin-bottom:16px">
    <div class="card-h"><h2>Check mode: what Enforce would have blocked</h2><span class="muted">last ${r.events_scanned} requests${since ? " · since " + esc(since) : ""}</span></div>
    ${items.length ? `<div class="card-b" style="padding-bottom:0"><p class="help">With Enforce on, <b>${total} request${total === 1 ? "" : "s"}</b> (${usd(cost)} of spend) would have been blocked with HTTP 402. Nothing was blocked: these limits are in Check mode.</p></div>
      <div class="table-wrap"><table class="wide"><thead><tr><th>Limit</th><th class="num">Requests over</th><th class="num">Their cost</th><th>Runs</th><th></th></tr></thead><tbody>${rows}</tbody></table></div>`
      : `<div class="card-b"><p class="help" style="margin:0">Nothing would have been blocked: every request stayed within the limits that are in Check mode. Once you're confident, switch them to Enforce under Keys &amp; limits.</p></div>`}
  </div>`;
}
document.addEventListener("click", async e => {
  const b = e.target.closest("[data-enforce]");
  if (!b) return;
  const policy = b.dataset.enforce, projectId = b.dataset.project;
  const org = currentOrg();
  const orgId = encodeURIComponent(S.org);
  if (policy === "org_monthly") {
    const budgets = { ...org.budgets, org_monthly: { ...org.budgets.org_monthly, mode: "enforce" } };
    if (!confirm("Enforce the org monthly budget? Requests over it get HTTP 402.")) return;
    await mutate(() => api(`/admin/orgs/${orgId}`, { method: "PUT", body: { budgets } }), "Org monthly budget enforced");
    return;
  }
  const p = org.projects.find(x => x.id === projectId);
  const current = p.effective[policy];
  const budgets = { ...p.budgets, [policy]: { limit_usd: current.limit_usd, mode: "enforce" } };
  delete budgets.org_monthly;
  const note = policy === "run" ? " Runs that already started keep their current mode; new runs are enforced." : "";
  if (!confirm(`Enforce "${POLICY_NAMES[policy]}" (${usd(current.limit_usd * 1e6)}) for ${projectId}? Requests over it get HTTP 402.${note}`)) return;
  await mutate(() => api(`/admin/orgs/${orgId}/projects/${encodeURIComponent(projectId)}`, { method: "PUT", body: { budgets } }), `${POLICY_NAMES[policy]} enforced for ${projectId}`);
});
