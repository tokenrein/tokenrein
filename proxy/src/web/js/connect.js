// TokenLens dashboard: Connect page.
"use strict";

// --- Connect ---
function adminUrl() { return location.origin; }

// Coding agents that report through their own telemetry and hooks. `prefix` is how their runs
// are named, which is how the page knows usage has arrived.
const AGENTS = [
  { id: "claude-code", name: "Claude Code", script: "claude-code", prefix: "cc-", tag: "Recommended · no code",
    files: "~/.claude/settings.json", after: "Then start a new Claude Code session.",
    checks: ["Every model call: model, tokens, cost, user, sub-agents",
      "Stops the next prompt or tool call when a run is paused or an enforced limit is used up",
      "Works with API keys and Claude subscriptions; nothing to install"] },
  { id: "codex", name: "OpenAI Codex CLI", script: "codex", prefix: "codex-", tag: "No code",
    files: "~/.codex/config.toml and ~/.codex/hooks.json",
    after: "Then start Codex and run <span class=\"mono\">/hooks</span> once to trust the TokenLens hooks; Codex skips new hooks until you do.",
    checks: ["Every model call from its OpenTelemetry logs: model, tokens (cached input priced as such), user",
      "Stops the next prompt or tool call when a run is paused or an enforced limit is used up",
      "Priced at OpenAI's API rates, also for ChatGPT sign-ins"] },
  { id: "gemini-cli", name: "Gemini CLI", script: "gemini", prefix: "gemini-", tag: "No code",
    files: "~/.gemini/settings.json", after: "Then start a new Gemini CLI session.",
    checks: ["Every model call from its OpenTelemetry logs: model, tokens (thinking counted as output), user, sub-agents",
      "Stops the next prompt or tool call when a run is paused or an enforced limit is used up",
      "Works with Gemini API keys, Vertex AI and Google sign-in"] },
  { id: "cursor", name: "Cursor", script: "cursor", prefix: "cursor-", tag: "Hooks · team usage import",
    files: "~/.cursor/hooks.json", after: "Cursor picks up the hooks when the file is saved.",
    checks: ["Each agent conversation shows up as a run, with the user's email",
      "Stops the next prompt, tool call or sub-agent when a run is paused or an enforced limit is used up",
      "Spend comes from Cursor's Admin API (team plans): add the key below"] },
];

function agentState(id) {
  S.connectAgents ??= {};
  return (S.connectAgents[id] ??= { project: null, key: null });
}
function connectCommand(a, key) {
  return `curl -fsSL ${adminUrl()}/connect/${a.script}.py | python3 - --key ${key} --url ${proxyUrl()}`;
}
function claudeCodeSettings(key) {
  const hook = { type: "http", url: `${proxyUrl()}/v1/hooks/claude-code`, timeout: 5,
    headers: { Authorization: "Bearer $TOKENLENS_API_KEY", "x-tl-agent": "claude-code" }, allowedEnvVars: ["TOKENLENS_API_KEY"] };
  return JSON.stringify({
    env: {
      CLAUDE_CODE_ENABLE_TELEMETRY: "1", OTEL_LOGS_EXPORTER: "otlp", OTEL_EXPORTER_OTLP_LOGS_PROTOCOL: "http/json",
      OTEL_EXPORTER_OTLP_LOGS_ENDPOINT: `${proxyUrl()}/v1/otel/v1/logs`,
      OTEL_EXPORTER_OTLP_LOGS_HEADERS: `Authorization=Bearer ${key},x-tl-agent=claude-code`,
      OTEL_LOGS_EXPORT_INTERVAL: "2000", TOKENLENS_API_KEY: key,
    },
    hooks: Object.fromEntries(["UserPromptSubmit", "PreToolUse", "SubagentStart", "Stop", "SessionEnd"].map(e =>
      [e, [e === "PreToolUse" ? { matcher: "*", hooks: [hook] } : { hooks: [hook] }]])),
  }, null, 2);
}
function sdkSnippet(kind) {
  if (kind === "python") return `pip install "tokenlens[anthropic]"
export TOKENLENS_API_KEY=tl_…        # a key for your project
export TOKENLENS_BASE_URL=${proxyUrl()}

from anthropic import Anthropic
from tokenlens import guard

client = guard.anthropic(Anthropic(), agent="support-bot")   # still calls Anthropic directly
with guard.run("ticket-4812", budget_usd=5):                 # one task = one run
    client.messages.create(model="claude-sonnet-5-5", max_tokens=1024, messages=[...])`;
  return `npm install @tokenlens/sdk @anthropic-ai/sdk
export TOKENLENS_API_KEY=tl_…        # a key for your project
export TOKENLENS_BASE_URL=${proxyUrl()}

import Anthropic from '@anthropic-ai/sdk';
import { guard } from '@tokenlens/sdk';

const client = guard.anthropic(new Anthropic(), { agent: 'support-bot' });  // calls Anthropic directly
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, () =>
  client.messages.create({ model: 'claude-sonnet-5-5', max_tokens: 1024, messages: [...] }));`;
}
function cmdBlock(text, label = "Copy") {
  return `<div class="cmd"><pre class="snip">${esc(text)}</pre><button class="btn sm" data-copy="${esc(text)}">${label}</button></div>`;
}

function seenLine(a) {
  const now = Date.now() / 1000;
  const sessions = S.runs.filter(r => r.run_id.startsWith(a.prefix) && !r.parent_run_id);
  if (!sessions.length) return `<div class="seen"><span class="live off"><span class="dot"></span>Not connected yet</span></div>`;
  const live = sessions.some(r => now - r.last_seen <= LIVE_SECS);
  return `<div class="seen">${live ? liveness(sessions[0].last_seen) : `<span class="live idle"><span class="dot"></span>Connected</span>`}
    <span>${sessions.length} session${sessions.length === 1 ? "" : "s"} in the last 7 days · last ${ago(sessions[0].last_seen)}</span>
    <a href="#" data-goto="runs">View runs</a></div>`;
}

function agentCard(a, projects) {
  const st = agentState(a.id);
  if (!st.project || !projects.includes(st.project)) st.project = projects.includes(a.id) ? a.id : projects[0] || null;
  const keyStep = st.key
    ? `<div class="step-h"><span class="n">2</span>Run this once in a terminal on the machine that runs ${esc(a.name)}</div>
       ${cmdBlock(connectCommand(a, st.key))}
       <p class="help" style="margin:0">It contains your new key, shown only now. It edits <span class="mono">${esc(a.files)}</span> (with a backup) and checks the key first. ${a.after}</p>
       ${a.id === "claude-code" ? `<details class="more"><summary>Prefer to edit the settings yourself?</summary>
         <p class="help">Merge this into <span class="mono">~/.claude/settings.json</span>, keeping your existing hooks:</p>${cmdBlock(claudeCodeSettings(st.key))}</details>` : ""}`
    : `<div class="step-h"><span class="n">2</span>Create a key and get the command</div>
       <div><button class="btn primary" data-agent-key="${a.id}" ${st.project ? "" : "disabled"}>Create key &amp; show command</button></div>`;
  return `<div class="card connect-card">
    <div class="card-h"><h2>${esc(a.name)}</h2><span class="tag">${esc(a.tag)}</span></div>
    <div class="card-b">
      ${seenLine(a)}
      <ul class="checks">${a.checks.map(c => `<li>${esc(c)}</li>`).join("")}</ul>
      <div class="step-h"><span class="n">1</span>Choose the project its spend counts against</div>
      <div class="inline">
        <select data-agent-project="${a.id}" aria-label="Project for ${esc(a.name)}">${projects.map(p => `<option ${p === st.project ? "selected" : ""}>${esc(p)}</option>`).join("")}</select>
        ${projects.includes(a.id) ? "" : `<button class="btn sm" data-agent-newproject="${a.id}">New project “${esc(a.id)}”</button>`}
      </div>
      ${keyStep}
      ${a.id === "cursor" ? cursorImport(projects) : ""}
      <details class="more"><summary>Disconnect later</summary>${cmdBlock(`curl -fsSL ${adminUrl()}/connect/${a.script}.py | python3 - --remove`)}</details>
    </div>
  </div>`;
}

// The optional Cursor usage import (team Admin API key, stored encrypted).
function cursorImport(projects) {
  const ci = S.cursorImport;
  if (!ci || ci.org !== S.org) return `<div class="help">Loading the usage import…</div>`;
  if (ci.error) return `<div class="help">Usage import unavailable: ${esc(ci.error)}</div>`;
  const s = ci.status;
  const project = s.project && projects.includes(s.project) ? s.project : agentState("cursor").project;
  const sync = s.sync || {};
  const status = !s.configured ? "" : sync.error
    ? `<span class="badge b-bad">Last import failed</span> <span class="help">${esc(sync.error)}</span>`
    : sync.last_run
      ? `<span class="badge b-ok">Importing</span> <span class="help">last run ${when(sync.last_run)}, ${sync.imported} new call${sync.imported === 1 ? "" : "s"}; runs hourly</span>`
      : `<span class="badge b-muted">Saved</span> <span class="help">first import within the hour, or import now</span>`;
  const canSave = s.secrets_enabled;
  return `<div class="step-h"><span class="n">3</span>Import spend from Cursor (team plans)</div>
    <p class="help" style="margin:0">Cursor reports usage per call (model, tokens, cost) through its Admin API, about hourly. Create an Admin API key in Cursor's team settings; it is stored encrypted and never shown again.</p>
    ${status ? `<div class="inline">${status}</div>` : ""}
    <form class="inline" data-cursor-import>
      <select name="project" aria-label="Project for imported Cursor spend">${projects.map(p => `<option ${p === project ? "selected" : ""}>${esc(p)}</option>`).join("")}</select>
      <input type="password" name="api_key" autocomplete="off" spellcheck="false" placeholder="${s.configured ? `key …${esc(s.last4)} (leave empty to keep)` : "Cursor Admin API key"}" aria-label="Cursor Admin API key" ${canSave ? "" : "disabled"}>
      <button class="btn sm primary" ${canSave ? "" : "disabled"}>${s.configured ? "Update" : "Save"}</button>
      ${s.configured ? `<button type="button" class="btn sm" data-cursor-sync>Import now</button><button type="button" class="btn sm" data-cursor-remove>Remove</button>` : ""}
    </form>
    ${canSave ? "" : `<p class="help" style="margin:0">Set <span class="mono">${esc(s.secrets_key_env)}</span> on the proxy (e.g. <span class="mono">openssl rand -hex 32</span>) and restart it to save keys here.</p>`}`;
}

async function loadCursorImport() {
  const org = S.org;
  try {
    const status = await api(`/admin/orgs/${encodeURIComponent(org)}/connectors/cursor`);
    S.cursorImport = { org, status };
  } catch (err) {
    S.cursorImport = { org, error: err.message };
  }
  if (S.tab === "connect" && S.org === org) renderConnect();
}

function renderConnect() {
  const org = currentOrg();
  if (!org) { $("#connect").innerHTML = `<div class="card"><div class="card-b">Create an org under Keys &amp; limits first.</div></div>`; return; }
  const projects = org.projects.map(p => p.id);
  if (S.cursorImport?.org !== S.org && !S.cursorImportLoading) {
    S.cursorImportLoading = true;
    loadCursorImport().finally(() => { S.cursorImportLoading = false; });
  }
  $("#connect").innerHTML = `<div class="connect-grid">
    ${AGENTS.map(a => agentCard(a, projects)).join("")}

    <div class="card connect-card">
      <div class="card-h"><h2>Your app or agent</h2><span class="tag">Python · TypeScript</span></div>
      <div class="card-b">
        <ul class="checks">
          <li>Wrap your existing OpenAI or Anthropic client in one line</li>
          <li>Calls go straight to the provider; TokenLens only receives usage, never prompts</li>
          <li>Works with any OpenAI-compatible provider: Gemini, Mistral, Groq, DeepSeek, OpenRouter, Ollama…</li>
        </ul>
        <div class="snip-tabs" role="tablist">${[["python", "Python"], ["typescript", "TypeScript"]].map(([k, l]) => `<button role="tab" data-sdk-snip="${k}" aria-selected="${S.sdkSnip === k}">${l}</button>`).join("")}</div>
        ${cmdBlock(sdkSnippet(S.sdkSnip))}
        <p class="help" style="margin:0">Need a key? Create one for your project under <a href="#" data-goto="settings">Keys &amp; limits</a>.</p>
      </div>
    </div>

    <div class="card connect-card">
      <div class="card-h"><h2>Any tool with a base URL</h2><span class="tag muted">Strict mode</span></div>
      <div class="card-b">
        <ul class="checks">
          <li>Cline, Aider, Continue, OpenCode, LibreChat, n8n, or your own HTTP calls</li>
          <li>Every call is checked against budgets <b>before</b> it is sent, exact even under bursts</li>
        </ul>
        ${cmdBlock(`Base URL:  ${proxyUrl()}      (OpenAI-style tools: ${proxyUrl()}/v1)\nAPI key:   tl_…  (a TokenLens key, not your provider key)`)}
        <p class="help" style="margin:0">TokenLens forwards to OpenAI or Anthropic with the provider keys saved under Keys &amp; limits.</p>
      </div>
    </div>
  </div>`;
}

function cursorPath() { return `/admin/orgs/${encodeURIComponent(S.org)}/connectors/cursor`; }

document.addEventListener("change", e => {
  const sel = e.target.closest("[data-agent-project]");
  if (sel) { const st = agentState(sel.dataset.agentProject); st.project = sel.value; st.key = null; renderConnect(); }
});
document.addEventListener("submit", async e => {
  const form = e.target.closest("[data-cursor-import]");
  if (!form) return;
  e.preventDefault();
  const body = { project: form.project.value };
  if (form.api_key.value.trim()) body.api_key = form.api_key.value.trim();
  const r = await mutate(() => api(cursorPath(), { method: "PUT", body }), "Cursor import saved");
  if (r) {
    S.cursorImport = { org: S.org, status: r };
    const synced = await mutate(() => api(`${cursorPath()}/sync`, { method: "POST" }));
    if (synced) toast(`Imported ${synced.imported} Cursor call${synced.imported === 1 ? "" : "s"}`);
    await loadCursorImport();
  }
});
document.addEventListener("click", async e => {
  const t = e.target;
  if (t.closest("[data-copy]")) {
    try { await navigator.clipboard.writeText(t.closest("[data-copy]").dataset.copy); toast("Copied"); }
    catch (_) { toast("Copy failed: select the text and copy it", true); }
  } else if (t.closest("[data-sdk-snip]")) {
    S.sdkSnip = t.closest("[data-sdk-snip]").dataset.sdkSnip; renderConnect();
  } else if (t.closest("[data-agent-newproject]")) {
    const id = t.closest("[data-agent-newproject]").dataset.agentNewproject;
    const r = await mutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/projects`, { method: "POST", body: { id } }), `Project ${id} created`);
    if (r) { agentState(id).project = id; renderConnect(); }
  } else if (t.closest("[data-agent-key]")) {
    const a = AGENTS.find(x => x.id === t.closest("[data-agent-key]").dataset.agentKey);
    const st = agentState(a.id);
    const name = `${a.name} (${new Date().toLocaleDateString()})`;
    const r = await mutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/projects/${encodeURIComponent(st.project)}/keys`, { method: "POST", body: { name } }), "Key created");
    if (r?.key) { st.key = r.key; renderConnect(); }
  } else if (t.closest("[data-cursor-sync]")) {
    const r = await mutate(() => api(`${cursorPath()}/sync`, { method: "POST" }));
    if (r) toast(`Imported ${r.imported} Cursor call${r.imported === 1 ? "" : "s"}`);
    await loadCursorImport();
  } else if (t.closest("[data-cursor-remove]")) {
    if (!confirm("Stop importing Cursor usage and delete the saved key?")) return;
    await mutate(() => api(cursorPath(), { method: "DELETE" }), "Cursor import removed");
    await loadCursorImport();
  }
});
