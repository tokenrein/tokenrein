// Tokenrein dashboard: Keys & limits page.
"use strict";

// --- settings: orgs, projects, keys, budgets ---
function budgetEditor(scope, budgets, inherited, policies) {
  const cells = policies.map(([key, title, desc]) => {
    const p = budgets?.[key];
    const inh = inherited?.[key];
    const inhText = inherited
      ? (inh ? `Inherits ${usd(inh.limit_usd * 1e6)} · ${inh.mode} from the org` : "Inherits: no limit")
      : "";
    return `<div class="budget" data-policy="${key}">
      <div class="t">${esc(title)}</div><div class="d">${esc(desc)}</div>
      <div class="inline">
        <input type="number" min="0" step="any" placeholder="${inherited ? "inherit" : "no limit"}" aria-label="${esc(title)} limit in USD" data-f="limit" value="${p ? esc(p.limit_usd) : ""}">
        <select data-f="mode" aria-label="${esc(title)} mode" ${p ? "" : "disabled"}>${Object.entries(key === "run" ? { ...MODES, ...ASK_MODE } : MODES).map(([m, label]) => `<option value="${m}" ${(p?.mode || "check") === m ? "selected" : ""} title="${esc(label)}">${esc(label.split(":")[0])}</option>`).join("")}</select>
      </div>
      ${inhText ? `<div class="inherit">${inhText}</div>` : ""}
    </div>`;
  }).join("");
  const max = budgets?.max_run_usd;
  const inhMax = inherited ? (inherited.max_run_usd ? `Inherits ${usd(inherited.max_run_usd * 1e6)}` : "Inherits: none") : "";
  return `<div class="budgets" data-scope="${esc(scope)}">${cells}
    <div class="budget" data-policy="max_run_usd">
      <div class="t">Max run budget</div><div class="d">Caps any x-tr-run-budget-usd a caller asks for.</div>
      <div class="inline"><input type="number" min="0" step="any" placeholder="${inherited ? "inherit" : "none"}" aria-label="Max run budget in USD" data-f="max" value="${max ?? ""}"></div>
      ${inhMax ? `<div class="inherit">${inhMax}</div>` : ""}
    </div>${loopEditor(budgets?.loops, inherited ? (inherited.loops || null) : undefined)}</div>`;
}

// Run budgets can also ask an approver instead of failing.
const ASK_MODE = { ask: "Ask: pause the run and ask an approver" };
const LOOP_ACTIONS = { warn: "Warn: alert only", pause: "Pause and ask an approver", kill: "Kill the run" };

// Loop detection settings; `inherited` is the org's policy when editing a project.
function loopEditor(l, inherited) {
  const project = inherited !== undefined;
  const inh = project ? (inherited ? `Inherits from the org: ${esc(LOOP_ACTIONS[inherited.action || "pause"].split(":")[0].toLowerCase())} after ${inherited.max_repeats} repeats` : "Inherits: off") : "";
  const num = (f, v, ph, label, extra = "") => `<label class="field">${label}<input type="number" min="0" step="any" data-loop="${f}" value="${v ?? ""}" placeholder="${ph}" ${extra}></label>`;
  return `<div class="budget loop-guard" data-policy="loops">
    <div class="t">Loop guard</div>
    <div class="d">Stops runaway runs early: the same request or tool call repeated, spend per minute, or too many calls in one run.</div>
    <div class="inline">
      <select data-loop="action" aria-label="Loop guard action">
        <option value="">${project ? "Inherit" : "Off"}</option>
        ${Object.entries(LOOP_ACTIONS).map(([k, label]) => `<option value="${k}" ${l?.action === k ? "selected" : ""}>${esc(label)}</option>`).join("")}
      </select>
    </div>
    <div class="loop-fields">
      ${num("max_repeats", l?.max_repeats, "6", "Same request, times", 'min="2" step="1"')}
      ${num("window_min", l ? Math.round((l.repeat_window_secs || 600) / 60) : "", "10", "…within minutes", 'min="1" step="1"')}
      ${num("max_usd_per_minute", l?.max_usd_per_minute, "none", "Max $ per minute")}
      ${num("max_calls_per_run", l?.max_calls_per_run, "none", "Max calls per run", 'step="1"')}
    </div>
    ${inh ? `<div class="inherit">${inh}</div>` : ""}
  </div>`;
}

function readLoops(el) {
  const action = $("[data-loop=action]", el).value;
  if (!action) return undefined;
  const v = f => { const x = $(`[data-loop=${f}]`, el).value.trim(); return x === "" ? undefined : Number(x); };
  const out = { action, max_repeats: v("max_repeats") ?? 6, repeat_window_secs: Math.round((v("window_min") ?? 10) * 60) };
  if (v("max_usd_per_minute") !== undefined) out.max_usd_per_minute = v("max_usd_per_minute");
  if (v("max_calls_per_run") !== undefined) out.max_calls_per_run = v("max_calls_per_run");
  return out;
}
function readBudgets(root) {
  const out = {};
  for (const el of $$(".budget[data-policy]", root)) {
    const key = el.dataset.policy;
    if (key === "loops") {
      const l = readLoops(el);
      if (l) out.loops = l;
      continue;
    }
    if (key === "max_run_usd") {
      const v = $("[data-f=max]", el).value.trim();
      if (v !== "") out.max_run_usd = Number(v);
      continue;
    }
    const v = $("[data-f=limit]", el).value.trim();
    if (v !== "") out[key] = { limit_usd: Number(v), mode: $("[data-f=mode]", el).value };
  }
  return out;
}

function renderSettings() {
  const org = currentOrg();
  const modeHelp = `<p class="help">Modes: <b>enforce</b> blocks a call that would go over (HTTP 402) · <b>check</b> lets it through and adds an <span class="mono">x-tr-warning</span> header · <b>skip</b> only tracks spend. Leave a limit blank for no limit${org ? " (or to inherit, for projects)" : ""}.</p>`;
  let html = "";
  if (org) {
    html += `<div class="card" data-org-card>
      <div class="card-h"><h2>Org <span class="mono">${esc(org.id)}</span> defaults</h2>
        <div class="toolbar">
          <label class="inline status">If Redis is down
            <select data-f="fail_mode"><option value="open" ${org.fail_mode === "open" ? "selected" : ""}>Keep serving (fail open)</option><option value="closed" ${org.fail_mode === "closed" ? "selected" : ""}>Refuse calls (fail closed)</option></select>
          </label>
          <button class="btn primary sm" data-save-org>Save org limits</button>
        </div></div>
      <div class="card-b">${modeHelp}${budgetEditor("org", org.budgets, null, POLICIES)}</div></div>`;
    html += providerKeysCard(org);
    if (can("admin")) html += alertsCard();
    for (const p of org.projects) {
      const keys = p.keys.map(k => `<tr>
        <td>${esc(k.name)}${k.source === "config" ? ' <span class="badge b-muted">config file</span>' : ""}</td>
        <td class="mono">${k.prefix ? esc(k.prefix) + "…" : `hash ${esc(k.id)}…`}</td>
        <td class="muted">${k.created_at ? new Date(k.created_at).toLocaleString() : "–"}</td>
        <td><div class="row-actions"><button class="btn sm danger" data-revoke="${esc(k.id)}" data-project="${esc(p.id)}" data-name="${esc(k.name)}">Revoke</button></div></td></tr>`).join("");
      html += `<div class="card" data-project-card="${esc(p.id)}">
        <div class="card-h"><h2>Project <span class="mono">${esc(p.id)}</span></h2>
          <span class="muted">${meter(p.spend.project_monthly)}</span>
          <div class="toolbar"><button class="btn sm danger" data-delete-project="${esc(p.id)}">Delete project</button></div></div>
        <div class="card-b">
          <h2 style="margin-bottom:8px">Virtual keys</h2>
          <p class="help">Give a key to an agent instead of your real OpenAI or Anthropic key. Calls made with it are counted against this project.</p>
          <div class="table-wrap" style="border:1px solid var(--line);border-radius:10px;margin-bottom:10px">
            <table>${p.keys.length ? `<thead><tr><th>Name</th><th>Key</th><th>Created</th><th></th></tr></thead><tbody>${keys}</tbody>` : `<tr><td class="empty">No keys yet: agents can't use this project.</td></tr>`}</table>
          </div>
          <form class="inline" data-new-key="${esc(p.id)}">
            <input type="text" required maxlength="100" placeholder="Key name, e.g. support-bot prod" aria-label="New key name" style="min-width:240px">
            <button class="btn" type="submit">Create key</button>
          </form>
          <h2 style="margin:20px 0 8px">Guardrail pack</h2>
          <form class="inline" data-project-pack="${esc(p.id)}">
            <select aria-label="Guardrail pack for ${esc(p.id)}">${S.packs.map(k => `<option value="${esc(k.id)}" ${p.pack === k.id ? "selected" : ""}>${esc(k.title)}</option>`).join("")}</select>
            <button class="btn sm" type="submit">${p.pack ? "Re-apply" : "Apply"} in Check mode</button>
            <span class="muted">${p.pack ? `Current: ${esc(S.packs.find(k => k.id === p.pack)?.title || p.pack)}` : "No pack applied"}</span>
          </form>
          <h2 style="margin:20px 0 8px">Limits for this project</h2>
          ${budgetEditor("project", p.budgets, org.budgets, POLICIES.slice(1))}
          <div class="inline" style="justify-content:flex-end;margin-top:12px"><button class="btn primary sm" data-save-project="${esc(p.id)}">Save project limits</button></div>
        </div></div>`;
    }
    html += `<div class="card"><div class="card-h"><h2>New project in ${esc(org.id)}</h2></div>
      <div class="card-b"><form class="inline" data-new-project>
        <input type="text" required pattern="[A-Za-z0-9._\\-]{1,128}" placeholder="project id, e.g. research-agent" aria-label="New project id" style="min-width:240px">
        <button class="btn" type="submit">Create project</button></form></div></div>`;
  }
  if (S.me?.instance_admin) html += `<div class="card"><div class="card-h"><h2>New org</h2></div>
    <div class="card-b"><p class="help">An org has its own monthly budget and projects. It uses the proxy's provider keys.</p><form class="inline" data-new-org>
      <input type="text" required pattern="[A-Za-z0-9._\\-]{1,128}" placeholder="org id, e.g. acme" aria-label="New org id" style="min-width:240px">
      <button class="btn" type="submit">Create org</button></form></div></div>`;
  if (org && can("owner")) {
    html += `<div class="card"><div class="card-h"><h2>Danger zone</h2>
      <div class="toolbar"><button class="btn sm danger" data-delete-org>Delete org ${esc(org.id)}</button></div></div>
      <div class="card-b"><p class="help" style="margin:0">Deleting an org or project revokes all of its keys at once. Recorded spend stays in Redis until it expires.</p></div></div>`;
  }
  if (!S.overview.managed) {
    html = `<div class="card"><div class="card-b"><b>Read-only.</b> This proxy has not loaded the managed config, so edits are disabled.</div></div>` + html;
  }
  if (org && !can("admin")) {
    html = `<div class="card"><div class="card-b"><b>Read-only.</b> You are ${esc(roleIn() || "not a member")} in ${esc(org.id)}; an admin or owner can change keys and limits.</div></div>` + html;
  }
  if (S.alertsNotice) { html = S.alertsNotice + html; }
  $("#settings").innerHTML = html;
  if (org && !can("admin")) $$("#settings input, #settings select, #settings button").forEach(el => { el.disabled = true; });
}
// A mode only means something once a limit is set.
$("#settings").addEventListener("input", e => {
  if (e.target.matches("[data-f=limit]")) {
    $("[data-f=mode]", e.target.closest(".budget")).disabled = e.target.value.trim() === "";
  }
});
const PROVIDERS = [["anthropic", "Anthropic (Claude)", "sk-ant-…"], ["openai", "OpenAI", "sk-…"]];
function providerKeysCard(org) {
  const enabled = S.overview.secrets_enabled;
  const rows = PROVIDERS.map(([id, label, ph]) => {
    const st = org.providers?.[id] || {};
    let status;
    if (!st.configured) status = `<span class="badge b-muted">not set up</span> <span class="muted">add a <span class="mono">[providers.${id}]</span> section to the proxy config</span>`;
    else if (st.source === "dashboard") status = `<span class="badge b-ok">saved here</span> <span class="mono">…${esc(st.last4)}</span> <span class="muted">· ${st.updated_at ? esc(new Date(st.updated_at).toLocaleString()) : ""}</span>`;
    else if (st.source === "environment") status = `<span class="badge b-info">from environment</span> <span class="mono">${esc(st.env_var)}</span>`;
    else status = `<span class="badge b-bad">no key</span> <span class="muted">calls to ${esc(label)} fail until you add one</span>`;
    if (st.error) status += `<div class="err" style="margin-top:4px">${esc(st.error)}</div>`;
    const canSave = st.configured && enabled;
    const removable = st.source === "dashboard" || (st.error && st.last4);
    return `<div class="prov-row">
      <div><b>${esc(label)}</b></div>
      <div>${status}</div>
      <div>${st.configured ? `<form data-provider-key="${id}">
        <input type="password" autocomplete="off" spellcheck="false" placeholder="${ph}" aria-label="${esc(label)} API key" ${canSave ? "" : "disabled"} required>
        <button class="btn sm" type="submit" ${canSave ? "" : "disabled"}>${st.source === "dashboard" ? "Replace" : "Save"}</button>
        ${st.source ? `<button class="btn sm" type="button" data-test-provider="${id}">Test</button>` : ""}
        ${removable ? `<button class="btn sm danger" type="button" data-remove-provider="${id}">Remove</button>` : ""}
      </form>` : ""}</div>
      ${id === "anthropic" && st.configured ? `<form class="ws" data-workspace>
        <label class="field">Workspace ID <span class="muted">(only if your key isn't scoped to a workspace)</span>
          <span class="inline" style="flex-wrap:nowrap">
            <input type="text" spellcheck="false" placeholder="wrkspc_…" aria-label="Anthropic workspace ID" value="${esc(org.anthropic_workspace_id || "")}">
            <button class="btn sm" type="submit">Save</button>
          </span></label>
        <span class="muted" style="font-size:12px">${org.anthropic_workspace_id ? "Sent as <span class=\"mono\">anthropic-workspace-id</span> on this org's Claude calls (a client's own header wins)." : "Find it in the Anthropic Console under Settings → Workspaces. Leave empty for workspace-scoped keys."}</span>
      </form>` : ""}</div>`;
  }).join("");
  const note = enabled
    ? `Keys are encrypted (AES-256-GCM) with the proxy's <span class="mono">${esc(S.overview.secrets_key_env)}</span> before they are stored, are never shown again (only the last 4 characters), and take effect within a few seconds. A key saved here overrides the environment variable.`
    : `<b>Saving keys here is off.</b> Start the proxy with <span class="mono">${esc(S.overview.secrets_key_env)}</span> set to a random secret (<span class="mono">openssl rand -hex 32</span>), which encrypts the keys you save here. Until then the proxy uses the environment variables.`;
  return `<div class="card" data-providers-card>
    <div class="card-h"><h2>Provider API keys for <span class="mono">${esc(org.id)}</span></h2></div>
    <div class="card-b"><p class="help">Your real OpenAI / Anthropic keys. The proxy uses them to call the provider; agents only ever get Tokenrein keys. ${note}</p>
    <div class="prov">${rows}</div></div></div>`;
}

// Don't re-render the forms under someone who is typing.
$("#settings").addEventListener("focusin", () => { S.editing = true; });
$("#settings").addEventListener("focusout", () => { setTimeout(() => { S.editing = $("#settings").contains(document.activeElement); }, 0); });


$("#settings").addEventListener("click", async e => {
  const org = encodeURIComponent(S.org);
  const t = e.target;
  if (t.closest("[data-save-org]")) {
    const card = t.closest("[data-org-card]");
    await mutate(() => api(`/admin/orgs/${org}`, { method: "PUT", body: { fail_mode: $("[data-f=fail_mode]", card).value, budgets: readBudgets(card) } }), "Org limits saved");
  } else if (t.closest("[data-save-project]")) {
    const id = t.closest("[data-save-project]").dataset.saveProject;
    const card = t.closest("[data-project-card]");
    await mutate(() => api(`/admin/orgs/${org}/projects/${encodeURIComponent(id)}`, { method: "PUT", body: { budgets: readBudgets(card) } }), `Limits for ${id} saved`);
  } else if (t.closest("[data-revoke]")) {
    const b = t.closest("[data-revoke]");
    if (!confirm(`Revoke key "${b.dataset.name}"? Agents using it get 401 within a few seconds.`)) return;
    await mutate(() => api(`/admin/orgs/${org}/projects/${encodeURIComponent(b.dataset.project)}/keys/${encodeURIComponent(b.dataset.revoke)}`, { method: "DELETE" }), "Key revoked");
  } else if (t.closest("[data-delete-project]")) {
    const id = t.closest("[data-delete-project]").dataset.deleteProject;
    if (!confirm(`Delete project ${id} and revoke all its keys?`)) return;
    await mutate(() => api(`/admin/orgs/${org}/projects/${encodeURIComponent(id)}`, { method: "DELETE" }), `Project ${id} deleted`);
  } else if (t.closest("[data-test-provider]")) {
    const b = t.closest("[data-test-provider]");
    const id = b.dataset.testProvider;
    b.disabled = true; b.textContent = "Testing…";
    try {
      const r = await api(`/admin/orgs/${org}/provider-keys/${id}/test`, { method: "POST" });
      toast(`${id}: ${r.message}`, !r.ok);
    } catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
    b.disabled = false; b.textContent = "Test";
  } else if (t.closest("[data-remove-provider]")) {
    const id = t.closest("[data-remove-provider]").dataset.removeProvider;
    if (!confirm(`Remove the ${id} key saved in the dashboard? The proxy falls back to its environment variable, if set.`)) return;
    await mutate(() => api(`/admin/orgs/${org}/provider-keys/${id}`, { method: "DELETE" }), `${id} key removed`);
  } else if (t.closest("[data-delete-org]")) {
    if (prompt(`Type the org id (${S.org}) to delete it and revoke every key in it.`) !== S.org) return;
    await mutate(() => api(`/admin/orgs/${org}`, { method: "DELETE" }), `Org ${S.org} deleted`);
  }
});
$("#settings").addEventListener("submit", async e => {
  e.preventDefault();
  const f = e.target;
  const value = $("input", f)?.value.trim() ?? "";
  const org = encodeURIComponent(S.org);
  if (f.matches("[data-workspace]")) {
    await mutate(() => api(`/admin/orgs/${org}`, { method: "PUT", body: { anthropic_workspace_id: value } }), value ? "Anthropic workspace ID saved" : "Anthropic workspace ID cleared");
  } else if (f.matches("[data-project-pack]")) {
    await applyPack(f.dataset.projectPack, $("select", f).value);
  } else if (f.matches("[data-provider-key]")) {
    const id = f.dataset.providerKey;
    const input = $("input", f);
    const r = await mutate(() => api(`/admin/orgs/${org}/provider-keys/${id}`, { method: "PUT", body: { api_key: input.value.trim() } }));
    input.value = "";
    if (r) toast(`${id} key saved (…${r.last4})`);
  } else if (f.matches("[data-new-key]")) {
    const project = f.dataset.newKey;
    const r = await mutate(() => api(`/admin/orgs/${org}/projects/${encodeURIComponent(project)}/keys`, { method: "POST", body: { name: value } }));
    if (r?.key) showKey(r.key);
  } else if (f.matches("[data-new-project]")) {
    await mutate(() => api(`/admin/orgs/${org}/projects`, { method: "POST", body: { id: value } }), `Project ${value} created`);
  } else if (f.matches("[data-new-org]")) {
    const r = await mutate(() => api(`/admin/orgs`, { method: "POST", body: { id: value } }), `Org ${value} created`);
    if (r) { S.org = value; store("local", ORG_KEY, value); await refresh(); }
  }
});
function showKey(key) {
  $("#key-value").textContent = key;
  const proxy = `${location.protocol}//${location.hostname}:8787`;
  $("#key-usage").textContent = `export TOKENREIN_API_KEY=${key}\nexport TOKENREIN_BASE_URL=${proxy}`;
  $("#key-usage").style.whiteSpace = "pre-wrap";
  $("#key-dialog").showModal();
}
$("#key-copy").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("#key-value").textContent); toast("Key copied"); }
  catch (_) { toast("Copy failed: select the key and copy it manually", true); }
});
$("#key-close").addEventListener("click", () => { $("#key-value").textContent = ""; $("#key-dialog").close(); });
