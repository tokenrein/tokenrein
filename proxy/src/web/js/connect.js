// TokenLens dashboard: Connect page.
"use strict";

// --- Connect ---
function adminUrl() { return location.origin; }
function claudeCodeCommand(key) {
  return `curl -fsSL ${adminUrl()}/connect/claude-code.py | python3 - --key ${key} --url ${proxyUrl()}`;
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
function renderConnect() {
  const org = currentOrg();
  if (!org) { $("#connect").innerHTML = `<div class="card"><div class="card-b">Create an org under Keys &amp; limits first.</div></div>`; return; }
  const projects = org.projects.map(p => p.id);
  if (!S.connectProject || !projects.includes(S.connectProject)) S.connectProject = projects.includes("claude-code") ? "claude-code" : projects[0] || null;
  const now = Date.now() / 1000;
  const sessions = S.runs.filter(r => r.run_id.startsWith("cc-") && !r.parent_run_id);
  const live = sessions.filter(r => now - r.last_seen <= LIVE_SECS).length;
  const seen = sessions.length
    ? `<div class="seen">${live ? liveness(sessions[0].last_seen) : `<span class="live idle"><span class="dot"></span>Connected</span>`}
        <span>${sessions.length} session${sessions.length === 1 ? "" : "s"} in the last 7 days · last ${ago(sessions[0].last_seen)}</span>
        <a href="#" data-goto="runs">View runs</a></div>`
    : `<div class="seen"><span class="live off"><span class="dot"></span>Not connected yet</span></div>`;
  const keyStep = S.connectKey
    ? `<div class="step-h"><span class="n">2</span>Run this once in a terminal on the machine that runs Claude Code</div>
       ${cmdBlock(claudeCodeCommand(S.connectKey))}
       <p class="help" style="margin:0">It contains your new key, shown only now. It edits <span class="mono">~/.claude/settings.json</span> (with a backup) and checks the key first. Then start a new Claude Code session.</p>
       <details class="more"><summary>Prefer to edit the settings yourself?</summary>
         <p class="help">Merge this into <span class="mono">~/.claude/settings.json</span>, keeping your existing hooks:</p>${cmdBlock(claudeCodeSettings(S.connectKey))}</details>`
    : `<div class="step-h"><span class="n">2</span>Create a key and get the command</div>
       <div><button class="btn primary" data-cc-key ${S.connectProject ? "" : "disabled"}>Create key &amp; show command</button></div>`;
  $("#connect").innerHTML = `<div class="connect-grid">
    <div class="card connect-card">
      <div class="card-h"><h2>Claude Code</h2><span class="tag">Recommended · no code</span></div>
      <div class="card-b">
        ${seen}
        <ul class="checks">
          <li>Every model call: model, tokens, cost, user, sub-agents</li>
          <li>Stops the next prompt or tool call when a run is paused or an enforced limit is used up</li>
          <li>Works with API keys and Claude subscriptions; nothing to install</li>
        </ul>
        <div class="step-h"><span class="n">1</span>Choose the project its spend counts against</div>
        <div class="inline">
          <select data-cc-project aria-label="Project for Claude Code">${projects.map(p => `<option ${p === S.connectProject ? "selected" : ""}>${esc(p)}</option>`).join("")}</select>
          ${projects.includes("claude-code") ? "" : `<button class="btn sm" data-cc-newproject>New project “claude-code”</button>`}
        </div>
        ${keyStep}
        <details class="more"><summary>Disconnect later</summary>${cmdBlock(`curl -fsSL ${adminUrl()}/connect/claude-code.py | python3 - --remove`)}</details>
      </div>
    </div>

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

    <div class="card connect-card">
      <div class="card-h"><h2>More coding agents</h2><span class="tag muted">Coming soon</span></div>
      <div class="card-b">
        <div class="soon"><span class="badge b-muted">OpenAI Codex CLI</span><span class="badge b-muted">Gemini CLI</span><span class="badge b-muted">Cursor</span><span class="badge b-muted">GitHub Copilot</span></div>
        <p class="help" style="margin:0">Same idea as Claude Code: their own telemetry and hooks, no proxy. Until then, tools that let you set a base URL work through strict mode.</p>
      </div>
    </div>
  </div>`;
}
document.addEventListener("change", e => {
  if (e.target.matches("[data-cc-project]")) { S.connectProject = e.target.value; S.connectKey = null; renderConnect(); }
});
document.addEventListener("click", async e => {
  const t = e.target;
  if (t.closest("[data-copy]")) {
    try { await navigator.clipboard.writeText(t.closest("[data-copy]").dataset.copy); toast("Copied"); }
    catch (_) { toast("Copy failed: select the text and copy it", true); }
  } else if (t.closest("[data-sdk-snip]")) {
    S.sdkSnip = t.closest("[data-sdk-snip]").dataset.sdkSnip; renderConnect();
  } else if (t.closest("[data-cc-newproject]")) {
    const r = await mutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/projects`, { method: "POST", body: { id: "claude-code" } }), "Project claude-code created");
    if (r) { S.connectProject = "claude-code"; renderConnect(); }
  } else if (t.closest("[data-cc-key]")) {
    const name = `Claude Code (${new Date().toLocaleDateString()})`;
    const r = await mutate(() => api(`/admin/orgs/${encodeURIComponent(S.org)}/projects/${encodeURIComponent(S.connectProject)}/keys`, { method: "POST", body: { name } }), "Key created");
    if (r?.key) { S.connectKey = r.key; renderConnect(); }
  }
});
