// Tokenrein site: theme toggle, code tabs, and the hero run meter.
"use strict";

(function theme() {
  const root = document.documentElement;
  let saved = null;
  try { saved = localStorage.getItem("tokenrein.theme"); } catch (_) { /* private mode */ }
  if (saved) root.dataset.theme = saved;
  document.querySelector("[data-theme-toggle]")?.addEventListener("click", () => {
    const dark = (root.dataset.theme || (matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark")) === "dark";
    root.dataset.theme = dark ? "light" : "dark";
    try { localStorage.setItem("tokenrein.theme", root.dataset.theme); } catch (_) { /* ignore */ }
  });
})();

for (const box of document.querySelectorAll("[data-tabs]")) {
  const tabs = [...box.querySelectorAll("[role=tab]")];
  tabs.forEach((tab) => tab.addEventListener("click", () => {
    tabs.forEach((t) => t.setAttribute("aria-selected", String(t === tab)));
    box.querySelectorAll("[data-panel]").forEach((p) => { p.hidden = p.dataset.panel !== tab.dataset.tab; });
  }));
}

document.querySelectorAll("[data-year]").forEach((el) => { el.textContent = String(new Date().getFullYear()); });

// The hero meter fills to the run's budget once, then the run pauses for approval.
(function meter() {
  const fill = document.querySelector("[data-fill]");
  const spent = document.querySelector("[data-spent]");
  const state = document.querySelector("[data-state]");
  if (!fill || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  fill.style.width = "0%";
  let v = 0;
  const target = 5;
  const step = () => {
    v = Math.min(target, v + 0.17 + Math.random() * 0.12);
    const pct = (v / target) * 100;
    fill.style.width = pct.toFixed(1) + "%";
    spent.textContent = "$" + v.toFixed(2);
    if (v >= target) {
      fill.parentElement.classList.add("full");
      state.textContent = "paused · waiting for approval";
      state.className = "pill stop";
      return;
    }
    state.textContent = Math.round(pct) + "% used";
    setTimeout(step, 320);
  };
  setTimeout(step, 600);
})();
