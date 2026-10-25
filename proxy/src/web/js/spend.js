// Tokenrein dashboard: Spend page.
"use strict";

// --- Spend ---
function fmtDay(d) { return new Date(`${d.slice(0, 4)}-${d.slice(4, 6)}-${d.slice(6)}T00:00:00Z`).toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC" }); }
function niceMax(v) {
  if (v <= 0) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v)));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}
function renderSpend() {
  const sp = S.spend;
  if (!sp) { $("#spend-chart").innerHTML = '<p class="muted">Loading…</p>'; return; }
  const days = sp.days;
  const total = days.reduce((a, d) => a + d.cost_micros, 0);
  const reqs = days.reduce((a, d) => a + d.requests, 0);
  const blocked = days.reduce((a, d) => a + d.blocked, 0);
  $("#spend-title").textContent = `Cost per day, last ${days.length} days (UTC)`;
  $("#spend-kpis").innerHTML = [
    kpi("Total", usd(total), `${reqs.toLocaleString()} requests`),
    kpi("Average per day", usd(Math.round(total / Math.max(1, days.length))), ""),
    kpi("Blocked requests", blocked.toLocaleString(), "over an enforced limit or stopped run"),
  ].join("");
  // Columns: one series, so no legend; the peak is labelled; every value is in the table view.
  const W = 760, H = 220, L = 52, R = 8, T = 18, B = 26;
  const max = niceMax(Math.max(...days.map(d => d.cost_micros), 0));
  const slot = (W - L - R) / days.length;
  const bw = Math.min(24, Math.max(2, slot - 2));
  const y = v => T + (H - T - B) * (1 - v / max);
  const ticks = [0, 0.25, 0.5, 0.75, 1].map(f => f * max);
  const peak = days.reduce((m, d, i) => d.cost_micros > (days[m]?.cost_micros ?? -1) ? i : m, 0);
  const every = Math.ceil(days.length / 8);
  let svg = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Cost per day; values in the table view"><g class="grid">`;
  for (const t of ticks) svg += `<line x1="${L}" x2="${W - R}" y1="${y(t)}" y2="${y(t)}"/>`;
  svg += `</g><g class="axis">`;
  // Tick labels share one precision so neighbours never round to the same text.
  const step = max / 4 / 1e6;
  const decimals = step >= 1 ? 0 : Math.min(6, Math.max(2, Math.ceil(-Math.log10(step))));
  const tickLabel = v => v === 0 ? "$0" : v / 1e6 >= 1000 ? "$" + (v / 1e9).toFixed(1).replace(/\.0$/, "") + "K" : "$" + (v / 1e6).toFixed(decimals);
  for (const t of ticks) svg += `<text x="${L - 6}" y="${y(t) + 4}" text-anchor="end">${esc(tickLabel(t))}</text>`;
  // Day labels counted back from today, so the last day is always labelled and spacing stays even.
  days.forEach((d, i) => { if ((days.length - 1 - i) % every === 0) svg += `<text x="${L + slot * i + slot / 2}" y="${H - 6}" text-anchor="middle">${esc(fmtDay(d.day))}</text>`; });
  svg += `</g>`;
  days.forEach((d, i) => {
    const x = L + slot * i + (slot - bw) / 2;
    const h = Math.max(0, y(0) - y(d.cost_micros));
    if (h > 0) {
      const r = Math.min(4, h, bw / 2);
      svg += `<path class="col" data-i="${i}" d="M${x},${y(0)} V${y(0) - h + r} Q${x},${y(0) - h} ${x + r},${y(0) - h} H${x + bw - r} Q${x + bw},${y(0) - h} ${x + bw},${y(0) - h + r} V${y(0)} Z"/>`;
    }
    svg += `<rect data-i="${i}" x="${L + slot * i}" y="${T}" width="${slot}" height="${H - T - B}" fill="transparent"/>`;
  });
  if (total > 0) svg += `<text class="peak" x="${L + slot * peak + slot / 2}" y="${y(days[peak].cost_micros) - 6}" text-anchor="middle">${esc(usd(days[peak].cost_micros))}</text>`;
  svg += `</svg>`;
  const chart = $("#spend-chart");
  chart.innerHTML = total > 0 ? svg + '<div class="tip hide"></div>' : `<div class="empty">No spend recorded in this range yet.</div>`;
  const tip = $(".tip", chart);
  if (tip) {
    const svgEl = $("svg", chart);
    svgEl.addEventListener("pointermove", ev => {
      const i = ev.target.dataset?.i;
      $$(".col", svgEl).forEach(c => c.classList.toggle("hover", c.dataset.i === i));
      if (i == null) { tip.classList.add("hide"); return; }
      const d = days[+i];
      tip.replaceChildren();
      const b = document.createElement("b"); b.textContent = usd(d.cost_micros);
      const line = document.createElement("div"); line.className = "muted";
      line.textContent = `${fmtDay(d.day)} · ${d.requests.toLocaleString()} requests${d.blocked ? ` · ${d.blocked} blocked` : ""}`;
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
  if (showTable) $("#spend-daily").innerHTML = `<table><thead><tr><th>Day (UTC)</th><th class="num">Cost</th><th class="num">Requests</th><th class="num">Blocked</th></tr></thead><tbody>${days.slice().reverse().map(d => `<tr><td>${esc(fmtDay(d.day))}</td><td class="num">${usd(d.cost_micros)}</td><td class="num">${d.requests}</td><td class="num">${d.blocked}</td></tr>`).join("")}</tbody></table>`;
  const breakdown = (dim, title, label) => {
    const rows = sp.by[dim] || [];
    const top = Math.max(1, ...rows.map(r => r.cost_micros));
    return `<div class="card"><div class="card-h"><h2>${esc(title)}</h2></div><div class="table-wrap"><table>
      ${rows.length ? `<thead><tr><th>${esc(label)}</th><th>Cost</th><th class="num">Requests</th></tr></thead><tbody>${rows.slice(0, 15).map(r => `<tr>
        <td class="${dim === "model" || dim === "user" ? "mono" : ""}">${r.key === "-" ? '<span class="muted">(not set)</span>' : esc(r.key)}</td>
        <td><span class="share"><span class="bar"><span style="width:${(r.cost_micros / top) * 100}%"></span></span><span class="nowrap">${usd(r.cost_micros)}</span></span></td>
        <td class="num">${r.requests.toLocaleString()}${r.blocked ? ` <span class="badge b-bad">${r.blocked}</span>` : ""}</td></tr>`).join("")}</tbody>` : `<tr><td class="empty">No data yet.</td></tr>`}
      </table></div></div>`;
  };
  $("#spend-split").innerHTML = breakdown("model", "By model", "Model") + breakdown("user", "By user", "User") + breakdown("project", "By project", "Project") + breakdown("agent", "By agent", "Project / agent");
}
