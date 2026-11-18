// Tokenrein dashboard: Spend page.
"use strict";

// What the page shows: a window (a preset or a custom range), how it is grouped, and filters.
// The window and grouping are remembered per browser; filters are not.
const SPEND_KEY = "tokenrein.spend";
const SPEND_WINDOWS = { "15m": 900, "1h": 3600, "6h": 21600, "24h": 86400, "7d": 7, "30d": 30, "90d": 90, "180d": 180, "365d": 365 };
const SPEND_BUCKET_SECS = { minute: 60, hour: 3600, day: 86400 };
// The server's limit on buckets in one report (a day of minutes, two months of hours).
const SPEND_MAX_BUCKETS = 1500;
const SPEND_DIMS = [["project", "Project"], ["agent", "Agent"], ["user", "User"], ["model", "Model"]];
const SPEND = (() => {
  const st = { window: "30d", bucket: "auto", from: "", to: "", f: { project: "", agent: "", user: "", model: "", status: "" } };
  try {
    const saved = JSON.parse(store("local", SPEND_KEY) || "{}");
    if (saved.window in SPEND_WINDOWS || saved.window === "custom") st.window = saved.window;
    if (["auto", "minute", "hour", "day"].includes(saved.bucket)) st.bucket = saved.bucket;
    if (st.window === "custom") { st.from = saved.from || ""; st.to = saved.to || ""; if (!st.from) st.window = "30d"; }
  } catch (_) { /* first visit or storage blocked */ }
  return st;
})();
// Whether the server keeps the usage history (minutes, hours and filters need it); unknown at first.
let spendHistory = null;

function saveSpend() {
  store("local", SPEND_KEY, JSON.stringify({ window: SPEND.window, bucket: SPEND.bucket, from: SPEND.from, to: SPEND.to }));
}

// The window as [from, to] Dates. Day windows start at midnight UTC so each bar is a whole day.
function spendWindow() {
  const now = new Date();
  if (SPEND.window === "custom") {
    const from = new Date(SPEND.from), to = SPEND.to ? new Date(SPEND.to) : now;
    return [from, to > now ? now : to];
  }
  const v = SPEND_WINDOWS[SPEND.window];
  if (SPEND.window.endsWith("d")) {
    const from = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - (v - 1)));
    return [from, now];
  }
  return [new Date(now - v * 1000), now];
}

// The grouping the server will use: the chosen one, or the one it picks from the window's length.
function spendAutoBucket(secs) { return secs <= 3 * 3600 ? "minute" : secs <= 4 * 86400 ? "hour" : "day"; }
function spendBucketFits(bucket, secs) { return Math.ceil(secs / SPEND_BUCKET_SECS[bucket]) + 1 <= SPEND_MAX_BUCKETS; }
function spendEffectiveBucket() {
  const [from, to] = spendWindow();
  const secs = (to - from) / 1000;
  if (spendHistory === false) return "day";
  if (SPEND.bucket !== "auto" && spendBucketFits(SPEND.bucket, secs)) return SPEND.bucket;
  return spendAutoBucket(secs);
}
function spendFiltered() { return Object.values(SPEND.f).some(Boolean); }

function spendParams(extra = {}) {
  const [from, to] = spendWindow();
  const p = new URLSearchParams({ from: from.toISOString(), bucket: spendEffectiveBucket() });
  if (SPEND.window === "custom" && SPEND.to) p.set("to", to.toISOString());
  if (spendHistory !== false) for (const [k, v] of Object.entries(SPEND.f)) if (v) p.set(k, v);
  for (const [k, v] of Object.entries(extra)) p.set(k, v);
  return p;
}

// Called by refresh() while the Spend page is open.
async function loadSpend(o) {
  try {
    S.spend = await api(`/admin/orgs/${o}/spend?${spendParams()}`);
  } catch (err) {
    // Minutes, hours and filters need the usage history: fall back to days from Redis.
    if (!/usage history/.test(err.message) || spendHistory === false) throw err;
    spendHistory = false;
    S.spend = await api(`/admin/orgs/${o}/spend?${spendParams()}`);
  }
  spendHistory = S.spend.history !== false;
}

// --- formatting ---
function niceMax(v) {
  if (v <= 0) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v)));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}
// Minutes and hours in the viewer's time; days in UTC, as they are counted.
function spendLabel(t, bucket, long) {
  const d = new Date(t);
  if (bucket === "day") return d.toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC", ...(long ? { year: "numeric" } : {}) });
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return long || bucket === "hour" && S.spend.buckets.length > 24
    ? `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}` : time;
}
function spendTitle(sp) {
  const per = `Cost per ${sp.bucket}`;
  if (SPEND.window === "custom") {
    const fmt = d => new Date(d).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
    return `${per}, ${fmt(sp.from)} – ${fmt(sp.to)}`;
  }
  const name = $(`#spend-window option[value="${SPEND.window}"]`)?.textContent.toLowerCase() || "";
  return `${per}, ${name}${sp.bucket === "day" ? " (UTC)" : ""}`;
}

// --- controls ---
function renderSpendControls() {
  const sp = S.spend;
  const kept = sp?.history_days || 0;
  const history = spendHistory !== false;
  $$("#spend-window option").forEach(o => {
    if ("history" in o.dataset) {
      // Past 40 days needs the usage history; past 90 days also a license (the option stays
      // visible so people know it exists).
      const days = SPEND_WINDOWS[o.value];
      const paid = days > 90 && S.license && !licensed("long_history");
      o.hidden = !history || kept < days;
      o.disabled = o.hidden || paid;
      o.textContent = o.textContent.replace(/ \(Team license\)$/, "") + (paid ? " (Team license)" : "");
    }
    if ("short" in o.dataset) o.disabled = !history;
  });
  $("#spend-window").value = SPEND.window;
  const [from, to] = spendWindow();
  const secs = (to - from) / 1000;
  const used = spendEffectiveBucket();
  $$("#spend-bucket button").forEach(b => {
    const k = b.dataset.bucket;
    const ok = k === "auto" || k === "day" || history && spendBucketFits(k, secs);
    b.disabled = !ok;
    b.title = ok ? "" : history ? `Too many ${k}s for this window` : "Needs the usage history";
    // A chosen grouping that doesn't fit this window falls back to Auto.
    const fits = SPEND.bucket === "auto" || SPEND.bucket === "day" || history && spendBucketFits(SPEND.bucket, secs);
    b.setAttribute("aria-pressed", String(fits ? SPEND.bucket === k : k === "auto"));
    if (k === "auto") b.textContent = !fits || SPEND.bucket === "auto" ? `Auto${sp ? ` (${used})` : ""}` : "Auto";
  });
  $("#spend-custom").classList.toggle("hide", SPEND.window !== "custom");
  if (SPEND.window === "custom" && !$("#spend-custom").contains(document.activeElement)) {
    $("#spend-from").value = SPEND.from; $("#spend-to").value = SPEND.to;
  }

  const bar = $("#spend-filters");
  if (!history) {
    bar.innerHTML = `<span class="muted">Minute and hour detail and filters need the usage history: set <span class="mono">TOKENREIN_HISTORY_DIR</span>, or <span class="mono">history.enabled</span> in the Helm chart.</span>`;
    return;
  }
  if (bar.contains(document.activeElement)) return; // don't rebuild a menu someone has open
  const opts = sp?.options || {};
  const select = (dim, label) => {
    const cur = SPEND.f[dim];
    const values = [...(opts[dim] || [])];
    if (cur && !values.includes(cur)) values.unshift(cur);
    const shown = v => v === "-" ? "(not set)" : v;
    return `<label class="inline status">${label}
      <select data-spend-filter="${dim}" aria-label="Filter by ${label.toLowerCase()}"><option value="">All</option>${values.map(v => `<option value="${esc(v)}" ${v === cur ? "selected" : ""}>${esc(shown(v))}</option>`).join("")}</select></label>`;
  };
  bar.innerHTML = SPEND_DIMS.map(([d, l]) => select(d, l)).join("") +
    `<label class="inline status">Status <select data-spend-filter="status" aria-label="Filter by status">
      <option value="">All</option><option value="allowed" ${SPEND.f.status === "allowed" ? "selected" : ""}>Allowed</option><option value="blocked" ${SPEND.f.status === "blocked" ? "selected" : ""}>Blocked</option></select></label>` +
    (spendFiltered() ? `<button class="btn sm" type="button" data-spend-clear>Clear filters</button>` : "");
}

// --- page ---
function renderSpend() {
  renderSpendControls();
  const sp = S.spend;
  if (!sp) { $("#spend-chart").innerHTML = '<p class="muted">Loading…</p>'; return; }
  const buckets = sp.buckets || [];
  const unit = sp.bucket;
  const total = buckets.reduce((a, d) => a + d.cost_micros, 0);
  const reqs = buckets.reduce((a, d) => a + d.requests, 0);
  const blocked = buckets.reduce((a, d) => a + d.blocked, 0);
  const peak = buckets.reduce((m, d, i) => d.cost_micros > (buckets[m]?.cost_micros ?? -1) ? i : m, 0);
  $("#spend-title").textContent = spendTitle(sp);
  $("#spend-kpis").innerHTML = [
    kpi("Total", usd(total), `${reqs.toLocaleString()} requests${spendFiltered() ? " · filtered" : ""}`),
    kpi(`Average per ${unit}`, usd(Math.round(total / Math.max(1, buckets.length))), `over ${buckets.length.toLocaleString()} ${unit}s`),
    kpi(`Busiest ${unit}`, total > 0 ? usd(buckets[peak].cost_micros) : "–", total > 0 ? esc(spendLabel(buckets[peak].t, unit, true)) : "no spend yet"),
    kpi("Blocked requests", blocked.toLocaleString(), "over an enforced limit or stopped run"),
  ].join("");

  // Columns: one series, so no legend; the peak is labelled; every value is in the table view.
  const W = 760, H = 220, L = 52, R = 8, T = 18, B = 26;
  const max = niceMax(Math.max(...buckets.map(d => d.cost_micros), 0));
  const slot = (W - L - R) / Math.max(1, buckets.length);
  const bw = slot >= 4 ? Math.min(24, slot - 2) : Math.max(0.6, slot * 0.8);
  const y = v => T + (H - T - B) * (1 - v / max);
  const ticks = [0, 0.25, 0.5, 0.75, 1].map(f => f * max);
  const every = Math.ceil(buckets.length / 8);
  let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Cost per ${unit}; values in the table view"><g class="grid">`;
  for (const t of ticks) svg += `<line x1="${L}" x2="${W - R}" y1="${y(t)}" y2="${y(t)}"/>`;
  svg += `</g><g class="axis">`;
  // Tick labels share one precision so neighbours never round to the same text.
  const step = max / 4 / 1e6;
  const decimals = step >= 1 ? 0 : Math.min(6, Math.max(2, Math.ceil(-Math.log10(step))));
  const tickLabel = v => v === 0 ? "$0" : v / 1e6 >= 1000 ? "$" + (v / 1e9).toFixed(1).replace(/\.0$/, "") + "K" : "$" + (v / 1e6).toFixed(decimals);
  for (const t of ticks) svg += `<text x="${L - 6}" y="${y(t) + 4}" text-anchor="end">${esc(tickLabel(t))}</text>`;
  // Labels counted back from the newest bucket, so it is always labelled and spacing stays even.
  buckets.forEach((d, i) => { if ((buckets.length - 1 - i) % every === 0) svg += `<text x="${L + slot * i + slot / 2}" y="${H - 6}" text-anchor="middle">${esc(spendLabel(d.t, unit))}</text>`; });
  svg += `</g>`;
  buckets.forEach((d, i) => {
    const x = L + slot * i + (slot - bw) / 2;
    const h = Math.max(0, y(0) - y(d.cost_micros));
    if (h > 0) {
      const r = bw >= 6 ? Math.min(4, h, bw / 2) : 0;
      svg += `<path class="col" data-i="${i}" d="M${x},${y(0)} V${y(0) - h + r} Q${x},${y(0) - h} ${x + r},${y(0) - h} H${x + bw - r} Q${x + bw},${y(0) - h} ${x + bw},${y(0) - h + r} V${y(0)} Z"/>`;
    }
    svg += `<rect data-i="${i}" x="${L + slot * i}" y="${T}" width="${slot}" height="${H - T - B}" fill="transparent"/>`;
  });
  if (total > 0) svg += `<text class="peak" x="${Math.min(W - R - 24, Math.max(L + 24, L + slot * peak + slot / 2))}" y="${y(buckets[peak].cost_micros) - 6}" text-anchor="middle">${esc(usd(buckets[peak].cost_micros))}</text>`;
  svg += `</svg>`;
  const chart = $("#spend-chart");
  chart.innerHTML = total > 0 ? svg + '<div class="tip hide"></div>'
    : `<div class="empty">No spend in this window${spendFiltered() ? " for these filters" : ""}.</div>`;
  const tip = $(".tip", chart);
  if (tip) {
    const svgEl = $("svg", chart);
    svgEl.addEventListener("pointermove", ev => {
      const i = ev.target.dataset?.i;
      $$(".col", svgEl).forEach(c => c.classList.toggle("hover", c.dataset.i === i));
      if (i == null) { tip.classList.add("hide"); return; }
      const d = buckets[+i];
      tip.replaceChildren();
      const b = document.createElement("b"); b.textContent = usd(d.cost_micros);
      const line = document.createElement("div"); line.className = "muted";
      line.textContent = `${spendLabel(d.t, unit, true)}${unit === "day" ? " (UTC)" : ""} · ${d.requests.toLocaleString()} requests${d.blocked ? ` · ${d.blocked} blocked` : ""}`;
      tip.append(b, line);
      const box = chart.getBoundingClientRect();
      tip.classList.remove("hide");
      const left = Math.min(box.width - tip.offsetWidth, Math.max(0, ev.clientX - box.left + 12));
      tip.style.left = left + "px"; tip.style.top = Math.max(0, ev.clientY - box.top - 50) + "px";
    });
    svgEl.addEventListener("pointerleave", () => { tip.classList.add("hide"); $$(".col", svgEl).forEach(c => c.classList.remove("hover")); });
  }
  const showTable = $("#spend-table-toggle").checked;
  $("#spend-daily").classList.toggle("hide", !showTable);
  chart.classList.toggle("hide", showTable);
  if (showTable) {
    const head = unit === "day" ? "Day (UTC)" : unit === "hour" ? "Hour" : "Minute";
    $("#spend-daily").innerHTML = `<table><thead><tr><th>${head}</th><th class="num">Cost</th><th class="num">Requests</th><th class="num">Blocked</th></tr></thead><tbody>${buckets.slice().reverse().map(d => `<tr><td>${esc(spendLabel(d.t, unit, true))}</td><td class="num">${usd(d.cost_micros)}</td><td class="num">${d.requests.toLocaleString()}</td><td class="num">${d.blocked}</td></tr>`).join("")}</tbody></table>`;
  }

  // Breakdowns follow the filters; with the usage history a row click filters by it.
  const pick = spendHistory !== false;
  const breakdown = (dim, title, label) => {
    const rows = sp.by[dim] || [];
    const top = Math.max(1, ...rows.map(r => r.cost_micros));
    const active = SPEND.f[dim];
    return `<div class="card"><div class="card-h"><h2>${esc(title)}</h2>${active ? `<span class="muted">filtered</span>` : ""}</div><div class="table-wrap"><table>
      ${rows.length ? `<thead><tr><th>${esc(label)}</th><th>Cost</th><th class="num">Requests</th></tr></thead><tbody>${rows.slice(0, 15).map(r => `<tr${pick ? ` class="pick" data-spend-pick="${dim}" data-key="${esc(r.key)}" tabindex="0" title="${active === r.key ? "Remove this filter" : "Show only this " + esc(label.toLowerCase())}"` : ""}${active === r.key ? ' aria-current="true"' : ""}>
        <td class="${dim === "model" || dim === "user" ? "mono" : ""}">${r.key === "-" ? '<span class="muted">(not set)</span>' : esc(r.key)}</td>
        <td><span class="share"><span class="bar"><span style="width:${(r.cost_micros / top) * 100}%"></span></span><span class="nowrap">${usd(r.cost_micros)}</span></span></td>
        <td class="num">${r.requests.toLocaleString()}${r.blocked ? ` <span class="badge b-bad">${r.blocked}</span>` : ""}</td></tr>`).join("")}</tbody>` : `<tr><td class="empty">No data in this window.</td></tr>`}
      </table></div></div>`;
  };
  $("#spend-split").innerHTML = breakdown("model", "By model", "Model") + breakdown("user", "By user", "User") + breakdown("project", "By project", "Project") + breakdown("agent", "By agent", "Project / agent");
}

function spendChanged() { saveSpend(); S.spend = null; renderSpend(); refresh(); }

$("#spend-window").addEventListener("change", e => {
  if (e.target.value === "custom") {
    // Start the custom range from the window that was showing.
    const pad = n => String(n).padStart(2, "0");
    const local = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
    const [from] = spendWindow();
    SPEND.window = "custom"; SPEND.from = local(from); SPEND.to = "";
    saveSpend(); renderSpendControls(); $("#spend-from").focus();
    return;
  }
  SPEND.window = e.target.value;
  spendChanged();
});
$("#spend-custom").addEventListener("submit", e => {
  e.preventDefault();
  const from = $("#spend-from").value, to = $("#spend-to").value;
  if (!from) return;
  if (to && new Date(to) <= new Date(from)) { toast("The end must be after the start.", true); return; }
  SPEND.from = from; SPEND.to = to;
  spendChanged();
});
$("#spend-bucket").addEventListener("click", e => {
  const b = e.target.closest("[data-bucket]");
  if (!b || b.disabled) return;
  SPEND.bucket = b.dataset.bucket;
  spendChanged();
});
$("#spend-filters").addEventListener("change", e => {
  const s = e.target.closest("[data-spend-filter]");
  if (!s) return;
  SPEND.f[s.dataset.spendFilter] = s.value;
  spendChanged();
});
document.addEventListener("click", e => {
  if (e.target.closest("[data-spend-clear]")) {
    for (const k of Object.keys(SPEND.f)) SPEND.f[k] = "";
    spendChanged();
    return;
  }
  const row = e.target.closest("[data-spend-pick]");
  if (row) {
    const dim = row.dataset.spendPick;
    SPEND.f[dim] = SPEND.f[dim] === row.dataset.key ? "" : row.dataset.key;
    spendChanged();
  }
});
document.addEventListener("keydown", e => {
  const row = e.target.closest?.("[data-spend-pick]");
  if (row && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); row.click(); }
});

// Download: the same window, grouping and filters; with the usage history one row per bucket,
// project, agent, user and model, without it daily totals.
document.addEventListener("click", async e => {
  if (!e.target.closest("[data-spend-csv]") || !S.org) return;
  try {
    const res = await fetch(`/admin/orgs/${encodeURIComponent(S.org)}/spend?${spendParams({ format: "csv" })}`, { headers: S.token ? { Authorization: "Bearer " + S.token } : {}, credentials: "same-origin" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const name = /filename="([^"]+)"/.exec(res.headers.get("content-disposition") || "")?.[1] || `tokenrein-spend-${S.org}.csv`;
    const url = URL.createObjectURL(await res.blob());
    const a = document.createElement("a");
    a.href = url; a.download = name;
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  } catch (err) { toast("Download failed: " + err.message, true); }
});
