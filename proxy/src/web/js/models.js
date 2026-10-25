// TokenLens dashboard: Models & prices page.
"use strict";

// --- Models & prices ---
const perM = micros => "$" + (micros / 1e6).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 4 });
function renderModels() {
  const m = S.models;
  const edit = !!S.me?.instance_admin;
  if (!m) { $("#models").innerHTML = '<p class="muted">Loading…</p>'; return; }
  $("#models-sub").textContent = `Prices TokenLens uses to turn tokens into dollars, per million tokens. Built-in prices were last checked ${m.verified_as_of}; check them against your provider's price page before relying on them for billing.`;
  const providers = [...new Set(m.rows.map(r => r.provider))].sort();
  const q = S.modelFilter.toLowerCase();
  const rows = m.rows.filter(r => (!S.modelProvider || r.provider === S.modelProvider) && (!q || r.model.includes(q) || r.provider.includes(q)));
  const unpriced = m.unpriced.length && edit ? `<div class="alert"><b>${m.unpriced.length} model${m.unpriced.length === 1 ? "" : "s"} without a price</b> used recently. They're charged at the provider's most expensive rate until you add one.
    <div class="inline" style="margin-top:8px">${m.unpriced.map(u => `<button class="btn sm" data-price-new data-provider="${esc(u.provider || "")}" data-model="${esc(u.model)}">Set price: ${esc(u.provider ? u.provider + "/" : "")}${esc(u.model)}</button>`).join("")}</div></div>` : "";
  const body = rows.map(r => `<tr>
    <td>${esc(r.provider)}</td><td class="mono">${esc(r.model)}</td>
    <td class="price">${perM(r.price.input)}</td><td class="price">${perM(r.price.output)}</td>
    <td class="price">${perM(r.price.cached_input)}</td><td class="price">${perM(r.price.cache_write)}</td>
    <td>${r.source === "override" ? '<span class="badge b-info">your price</span>' : '<span class="badge b-muted">built-in</span>'}</td>
    <td><div class="row-actions">${!edit ? "" : `
      <button class="btn sm" data-price-edit data-provider="${esc(r.provider)}" data-model="${esc(r.model)}">${r.source === "override" ? "Edit" : "Override"}</button>
      ${r.source === "override" ? `<button class="btn sm danger" data-price-remove data-provider="${esc(r.provider)}" data-model="${esc(r.model)}">Remove</button>` : ""}`}
    </div></td></tr>`).join("");
  $("#models").innerHTML = `${unpriced}<div class="card">
    <div class="card-h">
      <input type="search" data-model-filter placeholder="Search models" aria-label="Search models" value="${esc(S.modelFilter)}" style="min-width:220px">
      <select data-model-provider aria-label="Provider"><option value="">All providers</option>${providers.map(p => `<option ${p === S.modelProvider ? "selected" : ""}>${esc(p)}</option>`).join("")}</select>
      ${edit ? `<div class="toolbar"><button class="btn primary sm" data-price-new>Add a model price</button></div>` : ""}
    </div>
    <div class="table-wrap"><table class="wide"><thead><tr><th>Provider</th><th>Model</th><th class="num">Input</th><th class="num">Output</th><th class="num">Cached input</th><th class="num">Cache write</th><th>Source</th><th></th></tr></thead>
    <tbody>${body || '<tr><td colspan="8" class="empty">No models match.</td></tr>'}</tbody></table></div></div>
    <p class="help" style="margin-top:12px">Local providers (ollama, vllm, lmstudio, llamacpp, local) are free. Models from resellers such as Bedrock or OpenRouter match by model name.</p>`;
}
document.addEventListener("input", e => {
  if (e.target.matches("[data-model-filter]")) { S.modelFilter = e.target.value; const pos = e.target.selectionStart; renderModels(); const f = $("[data-model-filter]"); f.focus(); f.setSelectionRange(pos, pos); }
  if (e.target.matches("[data-model-provider]")) { S.modelProvider = e.target.value; renderModels(); }
});
function openPriceDialog(provider = "", model = "") {
  const f = $("#price-form");
  f.reset();
  f.provider.value = provider; f.model.value = model;
  const row = S.models?.rows.find(r => r.provider === provider && r.model === model);
  if (row) {
    f.input.value = row.price.input / 1e6; f.output.value = row.price.output / 1e6;
    f.cached.value = row.price.cached_input / 1e6; f.write.value = row.price.cache_write / 1e6;
  }
  $("#price-dialog").showModal();
}
async function savePricing(overrides, ok) {
  const r = await mutate(() => api("/admin/pricing", { method: "PUT", body: { overrides } }), ok);
  if (r !== undefined) { S.models = await api(`/admin/models?org=${encodeURIComponent(S.org)}`); renderModels(); }
  return r;
}
document.addEventListener("click", async e => {
  const t = e.target;
  const b = t.closest("[data-price-new],[data-price-edit],[data-price-remove]");
  if (!b) return;
  const { provider = "", model = "" } = b.dataset;
  if (b.matches("[data-price-remove]")) {
    if (!confirm(`Remove your price for ${provider}/${model}? The built-in price (if any) applies again.`)) return;
    await savePricing(S.models.overrides.filter(o => !(o.provider === provider && o.model.toLowerCase() === model)), "Price removed");
  } else openPriceDialog(provider, model);
});
$("#price-cancel").addEventListener("click", () => $("#price-dialog").close());
$("#price-form").addEventListener("submit", async e => {
  e.preventDefault();
  const f = e.target;
  const num = el => el.value.trim() === "" ? undefined : Number(el.value);
  const o = { provider: f.provider.value.trim().toLowerCase(), model: f.model.value.trim().toLowerCase(),
    input_per_mtok: Number(f.input.value), output_per_mtok: Number(f.output.value) };
  if (num(f.cached) !== undefined) o.cached_input_per_mtok = num(f.cached);
  if (num(f.write) !== undefined) o.cache_write_per_mtok = num(f.write);
  const rest = (S.models?.overrides || []).filter(x => !(x.provider === o.provider && x.model.toLowerCase() === o.model));
  const r = await savePricing([o, ...rest], `Price saved for ${o.provider}/${o.model}`);
  if (r !== undefined) $("#price-dialog").close();
});
