// Tokenrein dashboard: agent logs (the Logs page) and the step helpers the run trace shares.
"use strict";

const KIND_LABELS = {
  model_call: "Model call", prompt: "Prompt", tool_call: "Tool call", tool_result: "Tool result",
  subagent: "Sub-agent", turn_end: "Turn ended", session_end: "Session ended", log: "Log",
};
const KIND_ICONS = {
  model_call: '<path d="M12 3l8 4.5v9L12 21l-8-4.5v-9z"/><path d="M12 12l8-4.5M12 12v9M12 12L4 7.5"/>',
  prompt: '<path d="M4 5h16v11H8l-4 4z"/>',
  tool_call: '<path d="M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2.4-.6-.6-2.4z"/>',
  tool_result: '<path d="M5 12l4 4 10-10"/>',
  subagent: '<circle cx="6" cy="6" r="2"/><circle cx="18" cy="18" r="2"/><path d="M6 8v4a6 6 0 0 0 6 6h4"/>',
  turn_end: '<path d="M6 6h12v12H6z"/>',
  session_end: '<path d="M10 5H5v14h5M15 8l4 4-4 4M19 12H9"/>',
  log: '<path d="M5 6h14M5 12h14M5 18h9"/>',
};
function kindChip(kind) {
  const k = KIND_ICONS[kind] ? kind : "log";
  return `<span class="kind k-${esc(k)}"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${KIND_ICONS[k]}</svg>${esc(KIND_LABELS[kind] || kind)}</span>`;
}
function statusBadge(s, level) {
  if (s === "error") return `<span class="badge b-bad">error</span>`;
  if (s === "blocked") return `<span class="badge b-bad">blocked</span>`;
  if (level === "warn") return `<span class="badge b-warn">warn</span>`;
  if (s === "ok") return `<span class="badge b-ok">ok</span>`;
  if (s === "running") return `<span class="badge b-info">running</span>`;
  return '<span class="muted">–</span>';
}
function ms(v) {
  if (v == null) return "–";
  if (v < 1000) return Math.round(v) + " ms";
  if (v < 60000) return (v / 1000).toFixed(v < 10000 ? 2 : 1) + " s";
  return Math.floor(v / 60000) + "m " + Math.round((v % 60000) / 1000) + "s";
}
function bytes(n) {
  if (n == null) return "";
  if (n < 1024) return n + " B";
  return (n / 1024).toFixed(n < 10240 ? 1 : 0) + " KB";
}

// A request event (from the run timeline) in the same shape as a step.
function stepFromRequest(e) {
  const d = e.decision || "";
  const u = e.usage || {};
  return {
    ts: e.ts, step_id: e.request_id, kind: "model_call", name: e.model, run_id: e.run_id, agent: e.agent, user: e.user,
    status: d.startsWith("blocked") ? "blocked" : d === "allowed" ? "ok" : "error",
    detail: e.blocked_scope || (d === "allowed" ? null : d), duration_ms: e.upstream_ms || null,
    cost_micros: d.startsWith("allowed") ? e.actual_micros : 0,
    tokens: (u.input || 0) + (u.cached_input || 0) + (u.cache_write || 0) + (u.output || 0),
    usage: e.usage, check_exceeded: e.check_exceeded, source: e.source,
  };
}

// Pairs each tool call with its result (by call id, else the next result of the same tool in the
// same run): the call gets the duration and outcome, and the result row is folded into it.
function pairTools(steps) {
  const open = new Map();
  const out = [];
  for (const s of steps) {
    if (s.kind === "tool_call") {
      const row = { ...s, status: s.status === "blocked" ? "blocked" : "running" };
      out.push(row);
      if (row.status !== "blocked") {
        const key = s.call_id || `${s.run_id}\u0000${s.name}`;
        if (!open.has(key)) open.set(key, []);
        open.get(key).push(row);
      }
    } else if (s.kind === "tool_result") {
      const key = s.call_id || `${s.run_id}\u0000${s.name}`;
      const call = open.get(key)?.shift();
      if (call) {
        call.status = s.status || "ok";
        // The duration the agent measured, else the time between the call and its result.
        call.duration_ms = s.duration_ms ?? Math.max(0, new Date(s.ts) - new Date(call.ts));
        call.result = s;
      } else out.push(s);
    } else out.push(s);
  }
  return out;
}

// Tool calls made with the same name and input three or more times in the list.
function repeatedCalls(steps) {
  const counts = new Map();
  for (const s of steps) if (s.kind === "tool_call" && s.fingerprint) {
    const c = counts.get(s.fingerprint) || { name: s.name, n: 0 };
    c.n += 1;
    counts.set(s.fingerprint, c);
  }
  return [...counts.entries()].filter(([, c]) => c.n >= 3).sort((a, b) => b[1].n - a[1].n);
}

// The expandable detail under a step: content (when kept), result, attributes and ids.
function stepDetail(s, captureOn) {
  const rows = [];
  const add = (k, v) => { if (v != null && v !== "") rows.push(`<dt>${esc(k)}</dt><dd>${v}</dd>`); };
  if (s.content) add(s.kind === "prompt" ? "Prompt" : s.kind === "tool_result" ? "Output" : "Input", `<pre class="snip">${esc(s.content)}</pre>`);
  else if (["prompt", "tool_call", "tool_result", "log"].includes(s.kind) && s.size) {
    add("Content", `<span class="muted">${bytes(s.size)}, not kept. ${captureOn === false ? "Turn on content capture for this project under Keys &amp; limits to keep a redacted copy." : "Content capture is off for this project."}</span>`);
  }
  if (s.result?.content) add("Output", `<pre class="snip">${esc(s.result.content)}</pre>`);
  else if (s.result?.size) add("Output", `<span class="muted">${bytes(s.result.size)}</span>`);
  if (s.attrs && Object.keys(s.attrs).length) add("Attributes", `<span class="mono">${Object.entries(s.attrs).map(([k, v]) => `${esc(k)}=${esc(typeof v === "string" ? v : JSON.stringify(v))}`).join(" · ")}</span>`);
  if (s.usage) add("Tokens", `${(s.usage.input || 0).toLocaleString()} in · ${(s.usage.cached_input || 0).toLocaleString()} cached · ${(s.usage.output || 0).toLocaleString()} out`);
  if ((s.check_exceeded || []).length) add("Would block", `<span class="badge b-warn">${esc(s.check_exceeded.join(", "))}</span>`);
  add("Detail", s.detail ? `<span class="mono">${esc(s.detail)}</span>` : "");
  add("Run", `<a href="#" class="mono runlink" data-open-run="${esc(s.run_id)}">${esc(s.run_id)}</a>`);
  add("Agent", s.agent ? esc(s.agent) + (s.user ? ` <span class="muted">for ${esc(s.user)}</span>` : "") : "");
  add("Call id", s.call_id ? `<span class="mono">${esc(s.call_id)}</span>` : "");
  add("Fingerprint", s.fingerprint ? `<span class="mono muted">${esc(s.fingerprint.slice(0, 12))}</span>` : "");
  add("Source", s.source ? esc(s.source) : "");
  return `<dl class="kv">${rows.join("")}</dl>`;
}

function captureFor(project) {
  const p = currentOrg()?.projects.find(x => x.id === project);
  return p ? !!p.capture_content : undefined;
}

// --- Logs page ---
const LOGS = { entries: [], next: null, loading: false, open: new Set(), key: "" };

function logsQuery() {
  const p = new URLSearchParams({ limit: "200" });
  const kind = $("#logs-kind").value, status = $("#logs-status").value, agent = $("#logs-agent").value, q = $("#logs-q").value.trim();
  if (kind) p.set("kind", kind === "tool_call" ? "tool_call,tool_result" : kind);
  if (status) p.set("status", status);
  if (agent) { const i = agent.indexOf("/"); p.set("project", agent.slice(0, i)); p.set("agent", agent.slice(i + 1)); }
  if (q) p.set("q", q);
  return p;
}
async function loadLogs(more = false) {
  if (!S.org || LOGS.loading) return;
  const p = logsQuery();
  const key = S.org + "?" + p;
  if (more && LOGS.next) p.set("before", LOGS.next);
  LOGS.loading = true;
  try {
    const r = await api(`/admin/orgs/${encodeURIComponent(S.org)}/logs?${p}`);
    if (more && key === LOGS.key) LOGS.entries = LOGS.entries.concat(r.entries);
    else if (key === LOGS.key && !more) {
      // A refresh: keep older pages already loaded below the newest entries.
      const seen = new Set(r.entries.map(e => e.step_id));
      LOGS.entries = r.entries.concat(LOGS.entries.slice(r.entries.length).filter(e => !seen.has(e.step_id)));
      if (LOGS.entries.length <= r.entries.length) LOGS.next = r.next;
    } else { LOGS.entries = r.entries; LOGS.open.clear(); LOGS.next = r.next; }
    if (more) LOGS.next = r.next;
    LOGS.key = key;
  } catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
  finally { LOGS.loading = false; }
  renderLogs();
}

function renderLogs() {
  const agents = $("#logs-agent");
  const want = agents.value;
  const opts = S.agents.map(a => `${a.project}/${a.agent}`);
  if (agents.dataset.opts !== opts.join("|")) {
    agents.innerHTML = `<option value="">All agents</option>` + opts.map(o => `<option ${o === want ? "selected" : ""}>${esc(o)}</option>`).join("");
    agents.dataset.opts = opts.join("|");
  }
  // Newest first; pairing runs oldest first, folding each result into its call.
  const entries = pairTools([...LOGS.entries].reverse()).reverse();
  const problems = entries.filter(e => e.status === "error" || e.status === "blocked").length;
  const capturing = (currentOrg()?.projects || []).filter(p => p.capture_content).map(p => p.id);
  $("#logs-note").innerHTML = capturing.length
    ? `Content capture is on for <b>${capturing.map(esc).join(", ")}</b>: prompts and tool inputs are kept, redacted and shortened.`
    : `Content capture is off: steps show what happened (tool, size, outcome, timing), not prompt or tool-input text. Turn it on per project under <a href="#" data-goto="settings">Keys &amp; limits</a>.`;
  $("#logs-count").textContent = entries.length ? `${entries.length} entries${problems ? ` · ${problems} with problems` : ""}` : "";
  if (!entries.length) {
    $("#logs-table").innerHTML = `<tr><td class="empty">${LOGS.loading ? "Loading…" : "No log entries match. Agent steps arrive from the coding-agent connectors' hooks and from <span class=\"mono\">guard.log()</span> in the SDKs; model calls are listed too."}</td></tr>`;
    $("#logs-more").classList.add("hide");
    return;
  }
  // Refused retries (a paused run's agent trying again) are folded into the newest one.
  const groups = [], blocked = new Map();
  for (const e of entries) {
    const key = e.status === "blocked" ? [e.run_id, e.kind, e.name, e.detail].join("\u0000") : null;
    const g = key && blocked.get(key);
    if (g) { g.n += 1; g.oldest = e.ts; continue; }
    const ng = { e, n: 1, oldest: e.ts };
    groups.push(ng);
    if (key) blocked.set(key, ng);
  }
  const rows = groups.map(({ e, n, oldest }, i) => {
    const id = e.step_id || String(i);
    const open = LOGS.open.has(id);
    const cost = e.kind === "model_call" ? `${usd(e.cost_micros || 0)}<div class="muted">${(e.tokens || 0).toLocaleString()} tok</div>` : (e.size ? `<span class="muted">${bytes(e.size)}</span>` : "");
    return `<tr class="logrow ${open ? "open" : ""} ${e.status === "error" || e.status === "blocked" ? "problem" : ""}" data-log="${esc(id)}" tabindex="0" aria-expanded="${open}">
      <td class="muted nowrap">${when(e.ts)}</td>
      <td class="nowrap">${kindChip(e.kind)}</td>
      <td><span class="mono">${esc(e.name || "–")}</span>${e.detail && e.kind !== "model_call" ? ` <span class="muted">${esc(e.detail)}</span>` : ""}${e.content ? `<div class="muted clip">${esc(e.content)}</div>` : ""}</td>
      <td class="nowrap"><a href="#" class="mono runlink" data-open-run="${esc(e.run_id)}">${esc(e.run_id)}</a><div class="muted">${esc(e.project)} / ${esc(e.agent)}</div></td>
      <td>${statusBadge(e.status, e.level)}${n > 1 ? ` <span class="count" title="${n} refused retries since ${esc(when(oldest))}">×${n}</span>` : ""}</td>
      <td class="num muted">${ms(e.duration_ms || null)}</td>
      <td class="num">${cost}</td></tr>
      ${open ? `<tr class="logdetail"><td colspan="7">${stepDetail(e, captureFor(e.project))}</td></tr>` : ""}`;
  }).join("");
  $("#logs-table").innerHTML = `<thead><tr><th>Time</th><th>Kind</th><th>What</th><th>Run</th><th>Status</th><th class="num">Duration</th><th class="num">Cost / size</th></tr></thead><tbody>${rows}</tbody>`;
  $("#logs-more").classList.toggle("hide", !LOGS.next);
}

document.addEventListener("click", e => {
  if (e.target.closest("a, button, pre")) return;
  const row = e.target.closest("[data-log]");
  if (!row) return;
  const id = row.dataset.log;
  LOGS.open.has(id) ? LOGS.open.delete(id) : LOGS.open.add(id);
  renderLogs();
});
document.addEventListener("keydown", e => {
  const row = e.target.closest?.("[data-log], [data-trace]");
  if (!row || (e.key !== "Enter" && e.key !== " ")) return;
  e.preventDefault();
  row.click();
});
["#logs-kind", "#logs-status", "#logs-agent"].forEach(sel => $(sel).addEventListener("change", () => loadLogs()));
let logsTyping;
$("#logs-q").addEventListener("input", () => { clearTimeout(logsTyping); logsTyping = setTimeout(() => loadLogs(), 300); });
$("#logs-more").addEventListener("click", () => loadLogs(true));
