// Tokenrein dashboard: People & access page (members, API tokens, users, single sign-on).
"use strict";

const ROLE_HELP = {
  viewer: "sees runs, spend and settings",
  approver: "also pauses, resumes and kills runs",
  admin: "also changes keys, limits and members",
  owner: "also makes owners and deletes the org",
};

async function loadPeople() {
  const o = encodeURIComponent(S.org || "");
  const safe = p => p.catch(err => ({ error: err.message }));
  const [members, tokens, users, sso] = await Promise.all([
    S.org ? safe(api(`/admin/orgs/${o}/members`)) : Promise.resolve({ members: [] }),
    S.me?.user_id ? safe(api("/auth/tokens")) : Promise.resolve(null),
    S.me?.instance_admin ? safe(api("/admin/users")) : Promise.resolve(null),
    S.me?.instance_admin ? safe(api("/admin/sso")) : Promise.resolve(null),
  ]);
  S.people = { members, tokens, users, sso };
}

function roleOptions(selected, max) {
  return Object.keys(ROLE_HELP).filter(r => RANK[r] <= RANK[max])
    .map(r => `<option value="${r}" ${r === selected ? "selected" : ""}>${r}</option>`).join("");
}

function signInBadge(u) {
  if (u.disabled) return '<span class="badge b-bad">disabled</span>';
  if (u.has_password) return '<span class="badge b-muted">password</span>';
  if (u.last_login_at) return '<span class="badge b-info">single sign-on</span>';
  return '<span class="badge b-warn">not signed in yet</span>';
}

function oneTime(title, text, note) {
  return `<div class="alert" data-onetime><b>${esc(title)}</b>${cmdBlock(text)}<div class="help" style="margin:4px 0 0">${note}</div></div>`;
}

function renderPeople() {
  const el = $("#people");
  const p = S.people;
  if (!p) { el.innerHTML = '<p class="muted">Loading…</p>'; return; }
  let html = S.peopleNotice || "";
  if (p.members.error) {
    html += `<div class="card"><div class="card-b"><b>People and sign-in need a database.</b> ${esc(p.members.error)}. Until then, sign in with the admin token.</div></div>`;
    el.innerHTML = html;
    return;
  }
  const mine = roleIn(S.org);
  const canAdmin = can("admin");
  const maxGrant = mine === "owner" ? "owner" : "admin";
  const passwords = S.authConfig?.password_login;
  const rows = p.members.members.map(m => {
    const manageable = canAdmin && (m.role !== "owner" || mine === "owner");
    const role = manageable
      ? `<select data-member-role="${m.id}" aria-label="Role of ${esc(m.email)}">${roleOptions(m.role, maxGrant)}</select>`
      : `<span class="badge b-muted">${esc(m.role)}</span>`;
    const actions = manageable ? `<div class="row-actions">
        ${passwords ? `<button class="btn sm" data-member-link="${m.id}" data-email="${esc(m.email)}">Setup link</button>` : ""}
        <button class="btn sm danger" data-member-remove="${m.id}" data-email="${esc(m.email)}">Remove</button></div>` : "";
    return `<tr><td><b>${esc(m.name || m.email)}</b>${m.name ? `<div class="muted">${esc(m.email)}</div>` : ""}</td>
      <td>${role}</td><td>${signInBadge(m)}</td>
      <td class="muted nowrap">${m.last_login_at ? esc(new Date(m.last_login_at).toLocaleString()) : "never"}</td>
      <td>${actions}</td></tr>`;
  }).join("");
  html += `<div class="card">
    <div class="card-h"><h2>Members of <span class="mono">${esc(S.org || "")}</span></h2><span class="muted">you are ${esc(mine || "–")}</span></div>
    <div class="card-b" style="padding-bottom:0"><p class="help">${Object.entries(ROLE_HELP).map(([r, d]) => `<b>${r}</b> ${d}`).join(" · ")}. Instance admins manage every org.</p></div>
    <div class="table-wrap"><table>${rows ? `<thead><tr><th>Person</th><th>Role</th><th>Signs in with</th><th>Last sign-in</th><th></th></tr></thead><tbody>${rows}</tbody>`
      : `<tr><td class="empty">No members yet.</td></tr>`}</table></div>
    ${canAdmin ? `<div class="card-b"><form class="inline" data-member-add>
      <input type="email" required placeholder="name@company.com" aria-label="Email of the person to add" style="min-width:240px">
      <select aria-label="Role">${roleOptions("viewer", maxGrant)}</select>
      <button class="btn" type="submit">Add member</button></form>
      <p class="help" style="margin:8px 0 0">People who sign in with single sign-on just need their email here.${passwords ? " Others get a one-time link to set a password." : ""}</p></div>` : ""}
  </div>`;

  if (p.tokens && !p.tokens.error) {
    const trs = p.tokens.tokens.map(t => `<tr><td><b>${esc(t.name)}</b></td><td class="mono">${esc(t.prefix)}…</td>
      <td class="muted nowrap">${t.last_used_at ? esc(new Date(t.last_used_at).toLocaleString()) : "never used"}</td>
      <td class="muted nowrap">${t.expires_at ? esc(new Date(t.expires_at).toLocaleDateString()) : "never"}</td>
      <td><div class="row-actions"><button class="btn sm danger" data-token-revoke="${t.id}" data-name="${esc(t.name)}">Revoke</button></div></td></tr>`).join("");
    html += `<div class="card"><div class="card-h"><h2>Your API tokens</h2></div>
      <div class="card-b" style="padding-bottom:0"><p class="help">For scripts and CI: send <span class="mono">Authorization: Bearer tru_…</span> to the admin API. A token can do what you can do, and stops working when you lose access.</p></div>
      <div class="table-wrap"><table>${trs ? `<thead><tr><th>Name</th><th>Token</th><th>Last used</th><th>Expires</th><th></th></tr></thead><tbody>${trs}</tbody>` : `<tr><td class="empty">No tokens.</td></tr>`}</table></div>
      <div class="card-b"><form class="inline" data-token-new>
        <input type="text" required maxlength="100" placeholder="Token name, e.g. terraform" aria-label="Token name" style="min-width:220px">
        <select aria-label="Expires after"><option value="30">30 days</option><option value="90" selected>90 days</option><option value="365">1 year</option></select>
        <button class="btn" type="submit">Create token</button></form></div></div>`;
  }

  if (S.me?.via === "session") {
    html += `<div class="card"><div class="card-h"><h2>Your password</h2></div><div class="card-b">
      <form class="inline" data-password>
        <input type="password" autocomplete="current-password" placeholder="Current password" aria-label="Current password">
        <input type="password" autocomplete="new-password" required minlength="12" placeholder="New password (12+ characters)" aria-label="New password" style="min-width:220px">
        <button class="btn" type="submit">Change password</button></form>
      <p class="help" style="margin:8px 0 0">Changing it signs you out everywhere else. Accounts that use single sign-on can leave this alone.</p></div></div>`;
  }

  if (p.users && !p.users.error) {
    const urows = p.users.users.map(u => {
      const roles = Object.entries(u.roles || {}).map(([o, r]) => `${esc(o)}: ${esc(r)}`).join(", ");
      const self = u.id === S.me.user_id;
      return `<tr><td><b>${esc(u.name || u.email)}</b>${u.name ? `<div class="muted">${esc(u.email)}</div>` : ""}</td>
        <td>${signInBadge(u)}</td><td class="muted">${roles || "no orgs"}</td>
        <td><label class="inline status"><input type="checkbox" data-user-admin="${u.id}" ${u.instance_admin ? "checked" : ""}> instance admin</label></td>
        <td><div class="row-actions">${self ? "" : `<button class="btn sm ${u.disabled ? "" : "danger"}" data-user-disable="${u.id}" data-on="${u.disabled ? "false" : "true"}" data-email="${esc(u.email)}">${u.disabled ? "Enable" : "Disable"}</button>`}</div></td></tr>`;
    }).join("");
    html += `<div class="card"><div class="card-h"><h2>Everyone on this install</h2></div>
      <div class="table-wrap"><table>${urows ? `<thead><tr><th>Person</th><th>Signs in with</th><th>Orgs</th><th>Install</th><th></th></tr></thead><tbody>${urows}</tbody>` : `<tr><td class="empty">No accounts yet.</td></tr>`}</table></div>
      <div class="card-b"><form class="inline" data-user-new>
        <input type="email" required placeholder="name@company.com" aria-label="Email of the new instance admin" style="min-width:240px">
        <button class="btn" type="submit">Add instance admin</button></form>
      <p class="help" style="margin:8px 0 0">Instance admins manage every org, model prices, people and single sign-on. Disabling an account signs it out and revokes its API tokens.</p></div></div>`;
  }

  if (p.sso && !p.sso.error) html += ssoCard(p.sso);
  el.innerHTML = html;
}

function ssoCard(s) {
  const c = s.config || {};
  const orgs = (S.overview?.orgs || []).map(o => o.id);
  const mappings = (c.group_roles || []).map(m => `${m.group} = ${m.org}:${m.role}`).join("\n");
  const secret = c.client_secret ? `saved …${esc(c.client_secret.last4)}` : (c.client_secret_env ? `from $${esc(c.client_secret_env)}` : "none");
  return `<div class="card" data-sso-card><div class="card-h"><h2>Single sign-on (OpenID Connect)${paidTag("sso")}</h2>
      <span class="badge ${c.enabled ? "b-ok" : "b-muted"}">${c.enabled ? "on" : "off"}</span></div>
    <form class="card-b sso-form" data-sso>
      <p class="help" style="margin:0">Works with Okta, Microsoft Entra ID, Google Workspace, Auth0, Keycloak and any OpenID Connect provider. Register Tokenrein as a web app there with this redirect URI:</p>
      ${s.redirect_uri ? cmdBlock(s.redirect_uri) : `<div class="alert">Set <span class="mono">auth.public_url</span> in the proxy config to the dashboard's address (for example <span class="mono">https://tokenrein.example.com</span>) and restart it; the redirect URI is built from it.</div>`}
      <div class="budgets">
        <label class="field">Issuer URL <input name="issuer" type="url" placeholder="https://login.example.com/oauth2/default" value="${esc(c.issuer || "")}"></label>
        <label class="field">Client ID <input name="client_id" type="text" value="${esc(c.client_id || "")}"></label>
        <label class="field"><span>Client secret <span class="muted">(${secret})</span></span><input name="client_secret" type="password" autocomplete="off" placeholder="${c.client_secret ? "leave empty to keep" : "paste to save"}" ${s.secrets_enabled ? "" : "disabled"}></label>
        <label class="field">Button label <input name="label" type="text" maxlength="40" placeholder="Okta" value="${esc(c.label || "")}"></label>
        <label class="field">Allowed email domains <input name="allowed_domains" type="text" placeholder="example.com, example.org" value="${esc((c.allowed_domains || []).join(", "))}"></label>
        <label class="field">Send the client secret
          <select name="token_auth"><option value="basic" ${c.token_auth !== "post" ? "selected" : ""}>HTTP Basic (most providers)</option><option value="post" ${c.token_auth === "post" ? "selected" : ""}>In the request body</option></select></label>
      </div>
      ${s.secrets_enabled ? "" : `<p class="help" style="margin:0">Saving a client secret here needs the proxy's secrets key. Or name an environment variable that holds it: <input name="client_secret_env" type="text" placeholder="SSO_CLIENT_SECRET" value="${esc(c.client_secret_env || "")}" style="width:200px"></p>`}
      <label class="inline status"><input type="checkbox" name="auto_create" ${c.auto_create ? "checked" : ""}> Create an account the first time someone from an allowed domain signs in</label>
      <div class="inline">
        <span class="status">New accounts join</span>
        <select name="default_org" aria-label="Default org"><option value="">no org</option>${orgs.map(o => `<option ${o === c.default_org ? "selected" : ""}>${esc(o)}</option>`).join("")}</select>
        <span class="status">as</span>
        <select name="default_role" aria-label="Default role">${roleOptions(c.default_role || "viewer", "admin")}</select>
      </div>
      <label class="field"><span>Roles from groups <span class="muted">(one per line: <span class="mono">group = org:role</span>; synced at every sign-in for the orgs listed)</span></span>
        <textarea name="group_roles" rows="3" spellcheck="false" placeholder="tokenrein-admins = acme:admin">${esc(mappings)}</textarea></label>
      <label class="field">Groups claim <input name="groups_claim" type="text" placeholder="groups" value="${esc(c.groups_claim || "")}" style="max-width:200px"></label>
      <div class="inline" style="justify-content:space-between">
        <label class="inline status"><input type="checkbox" name="enabled" ${c.enabled ? "checked" : ""}> Show “Sign in with ${esc(c.label || "single sign-on")}” on the sign-in page</label>
        <span class="inline"><button class="btn sm" type="button" data-sso-test>Test connection</button><button class="btn primary sm" type="submit">Save single sign-on</button></span>
      </div>
      ${s.password_login ? `<p class="help" style="margin:0">Password sign-in is also on. To require single sign-on, set <span class="mono">auth.password_login = false</span> in the proxy config. Keep the admin token or one password account as a way back in.</p>` : ""}
    </form></div>`;
}

function readSso(f) {
  const v = n => f.elements[n]?.value.trim() ?? "";
  const mappings = v("group_roles").split("\n").map(l => l.trim()).filter(Boolean).map(l => {
    const m = l.match(/^(.+?)\s*=\s*([^:\s]+)\s*:\s*(viewer|approver|admin|owner)$/);
    if (!m) throw new Error(`Can't read the group mapping "${l}": use group = org:role`);
    return { group: m[1], org: m[2], role: m[3] };
  });
  const body = {
    enabled: f.elements.enabled.checked, label: v("label"), issuer: v("issuer"), client_id: v("client_id"),
    token_auth: v("token_auth"), allowed_domains: v("allowed_domains").split(",").map(d => d.trim()).filter(Boolean),
    auto_create: f.elements.auto_create.checked, default_org: v("default_org") || null, default_role: v("default_role"),
    groups_claim: v("groups_claim"), group_roles: mappings,
  };
  if (v("client_secret")) body.client_secret = v("client_secret");
  if (f.elements.client_secret_env) body.client_secret_env = v("client_secret_env") || null;
  return body;
}

async function peopleMutate(fn, ok) {
  try {
    const r = await fn();
    if (ok) toast(ok);
    await loadPeople();
    renderPeople();
    return r;
  } catch (err) { if (err.message !== "unauthorized") toast(err.message, true); }
}

function setupUrl(r) { return r.setup_url || location.origin + r.setup_path; }

$("#people").addEventListener("change", async e => {
  const t = e.target;
  const o = encodeURIComponent(S.org);
  if (t.matches("[data-member-role]")) {
    await peopleMutate(() => api(`/admin/orgs/${o}/members/${t.dataset.memberRole}`, { method: "PUT", body: { role: t.value } }), `Role changed to ${t.value}`);
  } else if (t.matches("[data-user-admin]")) {
    await peopleMutate(() => api(`/admin/users/${t.dataset.userAdmin}`, { method: "PUT", body: { instance_admin: t.checked } }), t.checked ? "Now an instance admin" : "No longer an instance admin");
  }
});

$("#people").addEventListener("click", async e => {
  const b = e.target.closest("button");
  if (!b) return;
  const o = encodeURIComponent(S.org);
  if (b.matches("[data-member-remove]")) {
    if (!confirm(`Remove ${b.dataset.email} from ${S.org}? They lose access to it at once.`)) return;
    await peopleMutate(() => api(`/admin/orgs/${o}/members/${b.dataset.memberRemove}`, { method: "DELETE" }), "Member removed");
  } else if (b.matches("[data-member-link]")) {
    const r = await peopleMutate(() => api(`/admin/orgs/${o}/members/${b.dataset.memberLink}/setup-link`, { method: "POST" }));
    if (r) { S.peopleNotice = oneTime(`Password link for ${b.dataset.email}`, setupUrl(r), "Send it to them privately. It works once, for 72 hours."); renderPeople(); }
  } else if (b.matches("[data-token-revoke]")) {
    if (!confirm(`Revoke the token "${b.dataset.name}"? Anything using it stops working.`)) return;
    await peopleMutate(() => api(`/auth/tokens/${b.dataset.tokenRevoke}`, { method: "DELETE" }), "Token revoked");
  } else if (b.matches("[data-user-disable]")) {
    const on = b.dataset.on === "true";
    if (on && !confirm(`Disable ${b.dataset.email}? They are signed out everywhere and their API tokens stop working.`)) return;
    await peopleMutate(() => api(`/admin/users/${b.dataset.userDisable}`, { method: "PUT", body: { disabled: on } }), on ? "Account disabled" : "Account enabled");
  } else if (b.matches("[data-sso-test]")) {
    b.disabled = true;
    try { const r = await api("/admin/sso/test", { method: "POST" }); toast(r.message, !r.ok); }
    catch (err) { toast(err.message, true); }
    b.disabled = false;
  }
});

$("#people").addEventListener("submit", async e => {
  e.preventDefault();
  const f = e.target;
  const o = encodeURIComponent(S.org);
  if (f.matches("[data-member-add]")) {
    const email = $("input", f).value.trim();
    const role = $("select", f).value;
    const r = await peopleMutate(() => api(`/admin/orgs/${o}/members`, { method: "POST", body: { email, role } }), `${email} added as ${role}`);
    if (r?.setup_path) {
      S.peopleNotice = oneTime(`Password link for ${email}`, setupUrl(r), "Send it to them privately. It works once, for 72 hours. People who use single sign-on can ignore it.");
      renderPeople();
    }
  } else if (f.matches("[data-token-new]")) {
    const name = $("input", f).value.trim();
    const r = await peopleMutate(() => api("/auth/tokens", { method: "POST", body: { name, expires_days: Number($("select", f).value) } }), "Token created");
    if (r?.token) { S.peopleNotice = oneTime(`API token "${name}"`, r.token, "Copy it now: it is shown only once."); renderPeople(); }
  } else if (f.matches("[data-user-new]")) {
    const email = $("input", f).value.trim();
    const r = await peopleMutate(() => api("/admin/users", { method: "POST", body: { email, instance_admin: true } }), `${email} is an instance admin`);
    if (r) { S.peopleNotice = oneTime(`Password link for ${email}`, setupUrl(r), "Send it to them privately. It works once, for 72 hours."); renderPeople(); }
  } else if (f.matches("[data-password]")) {
    const [current, next] = $$("input", f).map(i => i.value);
    const r = await peopleMutate(() => api("/auth/password", { method: "PUT", body: { current: current || null, new: next } }).then(() => true), "Password changed");
    if (r) $$("input", f).forEach(i => { i.value = ""; });
  } else if (f.matches("[data-sso]")) {
    let body;
    try { body = readSso(f); } catch (err) { toast(err.message, true); return; }
    await peopleMutate(() => api("/admin/sso", { method: "PUT", body }), "Single sign-on saved");
  }
});
