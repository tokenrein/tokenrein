// Tokenrein dashboard: state, formatting helpers and the admin API client.
"use strict";

const TOKEN_KEY = "tokenrein.admin_token";
const ORG_KEY = "tokenrein.org";
const TAB_KEY = "tokenrein.tab";
const MODES = { enforce: "Enforce: block the call", check: "Check: allow and warn", skip: "Skip: track spend only" };
const POLICIES = [
  ["org_monthly", "Org per month", "All spend in this org, per calendar month (UTC)."],
  ["project_monthly", "Project per month", "Each project, per calendar month (UTC)."],
  ["agent_daily", "Agent per day", "Each agent name within a project, per day (UTC)."],
  ["run", "Run (default)", "Each run, unless the caller sends x-tr-run-budget-usd. Fixed when a run starts."],
];

const S = { token: null, org: null, overview: null, runs: [], agents: [], events: [], tab: "overview", timer: null, editing: false,
  packs: [], review: null, spend: null, runId: null, run: null, snip: "python",
  models: null, modelFilter: "", modelProvider: "", connectProject: null, connectKey: null, sdkSnip: "python",
  audit: null, auditQuery: "", auditOrgOnly: false,
  me: null, csrf: null, authConfig: null, people: null, peopleNotice: "",
  approvals: null, alerts: null, alertKind: "slack", alertsNotice: "", license: null, licenseAt: 0 };

// Roles, lowest first; each can do everything the ones before it can.
const RANK = { viewer: 1, approver: 2, admin: 3, owner: 4 };
function roleIn(org = S.org) { return S.me?.instance_admin ? "owner" : S.me?.roles?.[org]; }
function can(role, org = S.org) { return (RANK[roleIn(org)] || 0) >= RANK[role]; }
const LIVE_SECS = 60, IDLE_SECS = 15 * 60;
function liveness(ts) {
  if (!ts) return `<span class="live off"><span class="dot"></span>Never</span>`;
  const age = Date.now() / 1000 - ts;
  if (age <= LIVE_SECS) return `<span class="live on"><span class="dot"></span>Live</span>`;
  if (age <= IDLE_SECS) return `<span class="live idle"><span class="dot"></span>Idle</span>`;
  return `<span class="live off"><span class="dot"></span>Offline</span>`;
}
function proxyUrl() { return `${location.protocol}//${location.hostname}:8787`; }

function store(kind, key, value) {
  try {
    const s = kind === "session" ? sessionStorage : localStorage;
    if (value === undefined) return s.getItem(key);
    if (value === null) s.removeItem(key); else s.setItem(key, value);
  } catch (_) { return null; }
}

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
function esc(v) {
  return String(v ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function usd(micros) {
  if (micros == null) return "–";
  const v = micros / 1e6;
  if (v === 0) return "$0";
  if (Math.abs(v) < 0.01) return "$" + v.toFixed(4);
  if (Math.abs(v) < 100) return "$" + v.toFixed(2);
  return "$" + v.toLocaleString(undefined, { maximumFractionDigits: 0 });
}
function ago(ts) {
  if (!ts) return "–";
  const s = Math.max(0, Math.round(Date.now() / 1000 - ts));
  if (s < 60) return s + "s ago";
  if (s < 3600) return Math.round(s / 60) + "m ago";
  if (s < 86400) return Math.round(s / 3600) + "h ago";
  return Math.round(s / 86400) + "d ago";
}
function when(iso) {
  if (!iso) return "–";
  const d = new Date(iso);
  return isNaN(d) ? esc(iso) : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
function meter(b) {
  if (!b) return '<span class="muted">–</span>';
  const spent = (b.spent_micros || 0) + (b.reserved_micros || 0);
  const limit = b.limit_micros;
  if (limit == null || b.mode === "skip") {
    return `<span class="meter"><span class="txt">${usd(b.spent_micros)}</span><span class="muted" style="font-size:12px">no limit</span></span>`;
  }
  const pct = limit > 0 ? Math.min(100, (spent / limit) * 100) : 100;
  const cls = pct >= 100 ? "bad" : pct >= 80 ? "warn" : "";
  const inflight = b.reserved_micros ? ` <span class="muted">(+${usd(b.reserved_micros)} in flight)</span>` : "";
  return `<span class="meter" title="${esc(b.mode)} mode"><span class="bar"><span class="fill ${cls}" style="width:${pct.toFixed(1)}%"></span></span><span class="txt">${usd(b.spent_micros)} / ${usd(limit)}${inflight}</span></span>`;
}
function stateBadge(state) {
  const cls = { running: "b-ok", paused: "b-warn", killed: "b-bad" }[state] || "b-muted";
  return `<span class="badge ${cls}">${esc(state)}</span>`;
}
function decisionBadge(d) {
  if (!d) return "";
  const cls = d.startsWith("allowed") ? "b-ok" : d.startsWith("blocked") ? "b-bad" : "b-warn";
  return `<span class="badge ${cls}">${esc(d.replace(/_/g, " "))}</span>`;
}
function modeBadge(m) {
  const cls = { enforce: "b-bad", check: "b-warn", skip: "b-muted" }[m] || "b-muted";
  return m ? `<span class="badge ${cls}">${esc(m)}</span>` : "";
}
function toast(msg, bad = false) {
  const t = document.createElement("div");
  t.className = "toast" + (bad ? " bad" : "");
  t.setAttribute("role", "status");
  t.textContent = msg;
  document.body.appendChild(t);
  setTimeout(() => t.remove(), bad ? 6000 : 2500);
}

// Signed in either with a session cookie (sent automatically; changes also need the CSRF token)
// or with the admin token kept in this tab.
async function api(path, opts = {}) {
  const method = opts.method || "GET";
  const headers = { ...(opts.body ? { "Content-Type": "application/json" } : {}) };
  if (S.token) headers.Authorization = "Bearer " + S.token;
  if (S.csrf && method !== "GET") headers["x-csrf-token"] = S.csrf;
  const res = await fetch(path, { method, headers, credentials: "same-origin",
    body: opts.body ? JSON.stringify(opts.body) : undefined });
  if (res.status === 401 && !opts.quiet) { signOut("Your session ended. Sign in again."); throw new Error("unauthorized"); }
  if (res.status === 204) return null;
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

function currentOrg() { return S.overview?.orgs.find(o => o.id === S.org); }

function kpi(label, value, hint) {
  return `<div class="card kpi"><div class="label">${esc(label)}</div><div class="value">${value}</div><div class="hint">${hint}</div></div>`;
}

async function mutate(fn, ok) {
  try { const r = await fn(); if (ok) toast(ok); S.editing = false; await refresh(); return r; }
  catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
}
