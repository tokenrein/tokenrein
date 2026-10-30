// Tokenrein site: the live run in the hero, the run stops index, the rollout slider, the audit log
// days, the plans, the deploy commands and the reveals. Everything is readable without it.
"use strict";

const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
const money = (v) => "$" + v.toFixed(2);
document.querySelectorAll("[data-year]").forEach((el) => { el.textContent = String(new Date().getFullYear()); });

// A run that spends until it reaches its budget, then waits for a person. Approving lets out
// another $5 of rein; the run finishes at $7.40.
(function liveRun() {
  const root = document.querySelector("[data-live]");
  if (!root) return;
  const meter = root.querySelector("[data-meter]");
  const spentEl = root.querySelector("[data-spent]");
  const limitEl = root.querySelector("[data-limit]");
  const state = root.querySelector("[data-state]");
  const approve = root.querySelector("[data-approve]");
  const stop = root.querySelector("[data-stop]");
  const budgets = root.querySelectorAll("[data-budget]");
  const SCALE = 15, TOTAL = 7.4;
  let spent = 0, limit = 5, timer = 0;

  const draw = () => {
    meter.style.setProperty("--fill", String(Math.min(spent, SCALE) / SCALE));
    meter.style.setProperty("--limit", String(Math.min(limit, SCALE) / SCALE));
    spentEl.textContent = money(spent);
    limitEl.textContent = money(limit);
  };
  const setState = (text, paused) => {
    state.textContent = text;
    meter.classList.toggle("is-paused", paused);
    approve.hidden = stop.hidden = !paused;
  };
  const step = () => {
    if (spent >= TOTAL) { setState("Finished", false); return; }
    const next = Math.min(TOTAL, spent + 0.12 + Math.random() * 0.3);
    if (next > limit) { spent = limit; draw(); setState("Paused, waiting for approval", true); return; }
    spent = next;
    draw();
    setState("Running · call " + Math.round(spent * 4), false);
    timer = setTimeout(step, reduced ? 0 : 260);
  };
  const run = () => {
    clearTimeout(timer);
    spent = 0;
    draw();
    if (reduced) { while (spent < TOTAL && spent < limit) spent = Math.min(TOTAL, limit, spent + 0.3); draw(); step(); return; }
    timer = setTimeout(step, 300);
  };
  budgets.forEach((b) => b.addEventListener("click", () => {
    budgets.forEach((x) => x.setAttribute("aria-checked", String(x === b)));
    limit = Number(b.dataset.budget);
    run();
  }));
  approve.addEventListener("click", () => { limit += 5; draw(); setState("Approved +$5.00", false); timer = setTimeout(step, reduced ? 0 : 400); });
  stop.addEventListener("click", () => { clearTimeout(timer); setState("Stopped by an approver", false); approve.hidden = stop.hidden = true; });
  run();
})();

// What stops a run: pointing at or choosing a row shows its page of the product.
(function guards() {
  const rows = document.querySelectorAll("[data-pick]");
  const capsule = document.querySelector("[data-capsule]");
  const fact = document.querySelector("[data-fact-out]");
  const pick = (row) => {
    rows.forEach((r) => r.setAttribute("aria-pressed", String(r === row)));
    document.querySelectorAll("[data-guard]").forEach((f) => f.classList.toggle("is-on", f.dataset.guard === row.dataset.pick));
    capsule.classList.toggle("is-mint", row.dataset.tone === "mint");
    fact.textContent = row.dataset.fact;
  };
  const fine = matchMedia("(hover: hover) and (pointer: fine)");
  rows.forEach((row) => {
    row.addEventListener("click", () => pick(row));
    row.addEventListener("focus", () => pick(row));
    row.addEventListener("pointerenter", () => { if (fine.matches) pick(row); });
  });
})();

// Rollout: four stages on one slider.
(function rollout() {
  const input = document.querySelector("[data-stage-input]");
  if (!input) return;
  const stages = [
    ["First, watch every run", "Connect a team's agents with every limit in Check mode. Nothing is blocked, and from the first day you see spend per person, per agent and per run.", "Watch"],
    ["Then, hear about overruns", "Add a Slack, Teams, PagerDuty or webhook channel. Budgets at 80%, runs waiting for approval and runaway loops arrive where the team already works.", "Alert"],
    ["Then, enforce the limits", "Switch the limits you trust to Enforce, set run budgets to Ask and the loop guard to Pause, and make team leads approvers.", "Enforce"],
    ["Finally, every team", "Add teams and production agents, with an org per business unit, and charge spend back by project, agent and person.", "Expand"],
  ];
  const title = document.querySelector("[data-stage-title]");
  const note = document.querySelector("[data-stage-note]");
  const names = document.querySelectorAll(".rollout__names li");
  input.addEventListener("input", () => {
    const i = Number(input.value);
    title.textContent = stages[i][0];
    note.textContent = stages[i][1];
    input.setAttribute("aria-valuetext", stages[i][2]);
    names.forEach((n, k) => n.classList.toggle("is-on", k === i));
    document.querySelectorAll("[data-stage]").forEach((f) => f.classList.toggle("is-on", Number(f.dataset.stage) === i));
  });
})();

// Audit log days: tabs with arrow keys.
(function days() {
  const tabs = [...document.querySelectorAll(".days [role=tab]")];
  const select = (tab) => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
  };
  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (e) => {
      const d = e.key === "ArrowDown" || e.key === "ArrowRight" ? 1 : e.key === "ArrowUp" || e.key === "ArrowLeft" ? -1 : 0;
      if (!d) return;
      e.preventDefault();
      const next = tabs[(i + d + tabs.length) % tabs.length];
      select(next);
      next.focus();
    });
  });
})();

// Plans: the figure belongs to the chosen name.
(function plans() {
  const names = document.querySelectorAll("[data-plan]");
  const pick = (btn) => {
    names.forEach((n) => n.setAttribute("aria-pressed", String(n === btn)));
    document.querySelectorAll("[data-plan-card]").forEach((c) => { c.hidden = c.dataset.planCard !== btn.dataset.plan; });
  };
  names.forEach((n) => { n.addEventListener("click", () => pick(n)); n.addEventListener("focus", () => pick(n)); });
})();

// Deploy: the commands for the address and the target, copied by the button.
(function deploy() {
  const form = document.querySelector("[data-deploy]");
  if (!form) return;
  const field = form.querySelector("#addr");
  const out = form.querySelector("[data-cmd]");
  const go = form.querySelector("[data-copy]");
  const seg = form.querySelectorAll("[data-target]");
  let target = "compose";
  const commands = () => {
    const url = (field.value.trim() || "https://tokenrein.example.com").replace(/[\s"'`$\\]/g, "");
    return target === "compose"
      ? [
          "git clone https://github.com/tokenrein/tokenrein",
          "cd tokenrein/deploy && cp .env.example .env",
          `echo "TOKENREIN_PUBLIC_URL=${url}" >> .env`,
          "docker compose up -d",
          "docker compose exec tokenrein tokenrein create-owner you@company.com",
        ].join("\n")
      : [
          "git clone https://github.com/tokenrein/tokenrein && cd tokenrein",
          "helm install tokenrein deploy/helm/tokenrein -n tokenrein --create-namespace \\",
          `  --set publicUrl=${url}`,
          "kubectl -n tokenrein exec deploy/tokenrein -- tokenrein create-owner you@company.com",
        ].join("\n");
  };
  const draw = () => { out.textContent = commands(); };
  field.addEventListener("input", draw);
  seg.forEach((b) => b.addEventListener("click", () => {
    target = b.dataset.target;
    seg.forEach((x) => x.setAttribute("aria-checked", String(x === b)));
    draw();
  }));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await navigator.clipboard.writeText(commands());
      go.textContent = "Copied. Paste them in a terminal";
    } catch (_) {
      go.textContent = "Select the commands above to copy them";
    }
    setTimeout(() => { go.textContent = "Copy the commands"; }, 2600);
  });
  draw();
})();

// Reveals: the parts of each section rise once as it comes into view.
(function reveals() {
  if (reduced || !("IntersectionObserver" in window)) return;
  const groups = [
    ".showcase__index, .showcase__fact",
    ".rollout__text > *",
    ".agents__intro > *, .tile",
    ".record__head, .record__lead, .record__log",
    ".stack__board > li",
    ".plans > *, .plan-stage",
    ".faq__title, .faq__list > details",
    ".deploy__close > *, .dialog",
    ".foot__cols > *",
  ];
  const targets = [];
  for (const sel of groups) {
    document.querySelectorAll(sel).forEach((el, i) => {
      if (el.getBoundingClientRect().top < innerHeight) return;
      el.dataset.reveal = "";
      el.style.setProperty("--i", String(Math.min(i, 4)));
      targets.push(el);
    });
  }
  const io = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      entry.target.classList.add("is-in");
      io.unobserve(entry.target);
    }
  }, { rootMargin: "0px 0px -12% 0px", threshold: 0.1 });
  targets.forEach((el) => io.observe(el));
  addEventListener("load", () => setTimeout(() => {
    targets.forEach((el) => { const r = el.getBoundingClientRect(); if (r.top < innerHeight && r.bottom > 0) el.classList.add("is-in"); });
  }, 2500));
})();
