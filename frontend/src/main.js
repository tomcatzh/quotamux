const app = document.querySelector("#app");
const esc = (v) =>
  String(v ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
// Product labels do not change saved identities or historical attribution.
const displayName = (value) => value === "zhipu-coding-plan" ? "GLM Coding Plan" : value;
const optionName = (path, value) => /\.(adapter|backend)$|\.order\.\d+$/.test(path) ? displayName(value) : value;
const state = {
  live: null,
  draft: null,
  revision: null,
  selected: "all",
  view: "statistics",
  page: "workspace",
  accountTab: null,
  editing: false,
  dirty: false,
  busy: false,
  message: "",
  preview: null,
  at: "",
  stats: null,
  history: null,
  totals: null,
  alerts: null,
  upstreamErrors: null,
  window: "1d",
  cursor: null,
  cursors: [],
  nextCursor: null,
  childGeneration: 0,
  loading: false,
  refreshing: false,
  updatedAt: null,
  pendingScope: null,
};
const browseActions = new Set([
  "enable-edit",
  "disable-edit",
  "accounts",
  "settings",
  "account-tab",
  "workspace",
  "select",
  "view",
  "window",
  "refresh",
  "older",
  "newer",
  "preview",
]);
const protocols = ["openai-chat", "openai-responses", "anthropic-messages"];
const adapters = [
  "aws-bedrock",
  "opencode-go",
  "deepseek-official",
  "kimi-official",
  "kimi-code",
  "zhipu-coding-plan",
  "custom-chat-completions",
  "custom-responses",
  "custom-anthropic",
  "ollama-cloud",
];
const get = (p) => p.split(".").reduce((v, k) => v?.[k], state.draft);
function set(p, v) {
  const keys = p.split(".");
  const last = keys.pop();
  keys.reduce((v, k) => v[k], state.draft)[last] = v;
  state.dirty = true;
  status();
}
const button = (action, label, path = "", extra = "") =>
  `<button type="button" data-action="${action}" data-path="${esc(path)}" ${extra}>${esc(label)}</button>`;
function field(path, label, kind = "text", choices = null) {
  const value = get(path);
  return `<label><span>${esc(label)}</span>${choices ? `<select data-field="${path}">${[...new Set([...(value ? [value] : []), ...choices])].map((v) => `<option value="${esc(v)}" ${value === v ? "selected" : ""}>${esc(optionName(path, v) || "None")}</option>`).join("")}</select>` : `<input data-field="${path}" data-kind="${kind}" type="${kind === "number" ? "number" : kind === "password" ? "password" : "text"}" ${kind === "number" ? 'min="0" step="any"' : ""} value="${esc(kind === "list" ? (value || []).join(", ") : value)}" autocomplete="${kind === "password" ? "new-password" : "off"}">`}</label>`;
}
function choices(path, label, values) {
  return `<fieldset class="choices"><legend>${esc(label)}</legend>${values.map(([v, title]) => `<label><input type="checkbox" data-choice="${path}" value="${v}" ${(get(path) || []).includes(v) ? "checked" : ""}>${esc(title)}</label>`).join("")}</fieldset>`;
}
function refs(path) {
  const t = get(path),
    backend = state.draft.backends.find((b) => b.id === t.backend);
  return `<div class="target-editor">${field(path + ".backend", "Account", "text", ["", ...state.draft.backends.map((b) => b.id)])}${field(path + ".credential", "Credential", "text", ["", ...(backend?.credentials || []).map((c) => c.id)])}${field(path + ".model", "Upstream model", "text", ["", ...(backend?.models || []).map((m) => m.name)])}${button("remove", "Remove target", path)}</div>`;
}
function render() {
  if (!state.draft) {
    app.innerHTML =
      '<main><h1>Routing</h1><p role="status">Loading workspace…</p></main>';
    return;
  }
  const model = state.draft.models[state.selected];
  app.innerHTML = `<header><strong>QuotaMux</strong><span>Routing workspace</span><div class="header-actions">${button("workspace", "Workspace", "", `aria-current="${state.page === "workspace" ? "page" : "false"}"`)}${button("accounts", "Accounts", "", `aria-current="${state.page === "accounts" ? "page" : "false"}"`)}${button("settings", "Settings", "", `aria-current="${state.page === "settings" ? "page" : "false"}"`)}${button(state.editing ? "disable-edit" : "enable-edit", state.editing ? "Disable edit" : "Enable edit")}</div></header><div id="notice" role="status" aria-live="polite">${esc(state.message)}</div><div id="migration-notice" role="status">${migrationMessage()}</div>${state.page === "settings" ? `<main class="accounts-page" aria-label="Settings"><h1>Settings</h1>${advanced()}</main>` : state.page === "accounts" ? accounts() : `<div class="workspace"><aside class="model-rail"><h2>Models</h2>${button("select", "All", "all", `aria-current="${state.selected === "all" ? "page" : "false"}"`)}${state.draft.models.map((m, i) => button("select", m.name || "New model", String(i), `aria-current="${i === state.selected ? "page" : "false"}"`)).join("")}${state.editing ? button("add-model", "+ Add model") : ""}<p class="muted">${state.live?.now ? "Live status " + esc(new Date(state.live.now).toLocaleTimeString()) : ""}</p></aside><main><nav class="child-nav" aria-label="Model views">${(state.selected === "all" ? ["statistics", "history"] : ["routing", "statistics", "history"]).map((v) => button("view", v[0].toUpperCase() + v.slice(1), v, `aria-pressed="${state.view === v}"`)).join("")}</nav>${state.view === "routing" ? (model ? modelEditor(model) : "<h1>Add your first model</h1>") : childView()}</main></div>`}${state.editing ? `<footer><span id="draft-status">${state.dirty ? "Unsaved changes" : "All changes saved"}</span><span>Save validates upstreams and immediately updates routing.</span>${button("discard", "Discard", "", state.busy ? "disabled" : "")}${button("save", state.busy ? "Validating and saving…" : "Save", "", `data-testid="save-workspace" ${state.busy || !state.editing ? "disabled" : ""}`)}</footer>` : ""}`;
  if (state.busy)
    app
      .querySelectorAll("input,select,button")
      .forEach((n) => (n.disabled = true));
  if (!state.editing) {
    app
      .querySelectorAll("[data-field],[data-choice]")
      .forEach((node) => (node.disabled = true));
    app.querySelectorAll("[data-action]").forEach((node) => {
      if (!browseActions.has(node.dataset.action)) node.disabled = true;
    });
  }
  const selectedTab = app.querySelector('.account-tabs [aria-selected="true"]');
  if (selectedTab) {
    const tabs = selectedTab.parentElement;
    const bounds = tabs.getBoundingClientRect(), selected = selectedTab.getBoundingClientRect();
    if (selected.right > bounds.right) tabs.scrollLeft += selected.right - bounds.right;
    else if (selected.left < bounds.left) tabs.scrollLeft += selected.left - bounds.left;
  }
  updateLive();
  syncHash(false);
}
function modelEditor(m) {
  if (!state.editing) return `<div class="heading"><div><p class="eyebrow">Public model</p><h1>${esc(m.name || "New model")}</h1></div></div><div class="effective" id="effective-order"></div>`;
  const p = `models.${state.selected}`;
  return `<div class="heading"><div><p class="eyebrow">Public model</p><h1>${esc(m.name || "New model")}</h1></div>${button("remove", "Delete model", p)}</div><div class="row">${field(p + ".name", "Model name")}${field(p + ".aliases", "Aliases (comma separated)", "list")}</div>${choices(
    p + ".protocols",
    "Accepted protocols",
    protocols.map((v) => [v, v]),
  )}<div class="effective" id="effective-order"></div><section><div class="heading"><h2>Routing layers</h2>${button("add-layer", "+ Add layer", p + ".layers")}</div><p class="muted">Default priority runs top to bottom. A full target is skipped so another target or layer can serve the request.</p>${m.layers
    .map((l, i) => {
      const q = `${p}.layers.${i}`;
      return `<article class="layer-card"><div class="row">${field(q + ".name", "Layer name")}${field(q + ".strategy", "Selection strategy", "text", ["random", "prompt-prefix-affinity"])}<div class="buttons">${button("up", "Move up", q, i === 0 ? "disabled" : "")}${button("down", "Move down", q, i === m.layers.length - 1 ? "disabled" : "")}${button("remove", "Delete layer", q)}</div></div>${l.targets.map((t, j) => `<div class="target">${refs(`${q}.targets.${j}`)}<div class="target-live" data-live-target="${esc([l.name, t.backend, t.credential, t.model].join("|"))}"></div></div>`).join("")}${button("add-target", "+ Add target", q + ".targets")}</article>`;
    })
    .join(
      "",
    )}</section><section><div class="heading"><h2>Time windows</h2>${button("add-schedule", "+ Add time window", p + ".schedules")}</div>${field(p + ".timezone", "Timezone (IANA name)")}<p class="muted">No matching window uses default priority. Start inclusive, end exclusive; overnight windows belong to their starting day.</p>${(
    m.schedules || []
  )
    .map((s, i) => {
      const q = `${p}.schedules.${i}`;
      return `<article><div class="row">${field(q + ".name", "Window name")}${field(q + ".start", "Start (HH:MM or HH:MM:SS)")}${field(q + ".end", "End (HH:MM or HH:MM:SS)")}</div>${choices(
        q + ".days",
        "Starting days",
        [
          [1, "Mon"],
          [2, "Tue"],
          [3, "Wed"],
          [4, "Thu"],
          [5, "Fri"],
          [6, "Sat"],
          [7, "Sun"],
        ],
      )}${scheduleOrder(q, m)}${button("remove", "Delete window", q)}</article>`;
    })
    .join(
      "",
    )}<div class="preview"><h3>Preview saved policy</h3><p class="muted">Read-only preview uses the currently saved rules, not unsaved edits.</p><label><span>Instant (RFC3339 with timezone)</span><input id="preview-at" value="${esc(state.at)}" placeholder="2026-09-07T12:00:00Z"></label>${button("preview", "Preview saved order")}<div id="preview-result">${state.preview ? esc(state.preview) : ""}</div></div></section>`;
}
function normalizeAccountTab() {
  const index = Number(state.accountTab?.split(":")[1]);
  if (state.accountTab?.startsWith("backend:") && state.draft.backends[index]) return;
  state.accountTab = state.draft.backends.length ? "backend:0" : null;
}
function accounts() {
  normalizeAccountTab();
  const tabs = state.draft.backends.map((backend, index) => [`backend:${index}`, displayName(backend.id || "New account")]);
  const index = Number(state.accountTab?.split(":")[1]);
  const backend = state.draft.backends[index];
  const panel = backend ? (state.editing ? accountEditor(backend, index) : accountSummary(backend)) : '<p class="muted">No accounts configured.</p>';
  return `<main class="accounts-page" aria-label="Accounts"><div class="heading"><h1>Accounts</h1>${state.editing ? button("add-backend", "+ Add account") : ""}</div><nav class="account-tabs" role="tablist" aria-label="Account settings">${tabs.map(([id, label]) => button("account-tab", label, id, `role="tab" id="account-tab-${id}" aria-controls="account-panel" aria-selected="${state.accountTab === id}"`)).join("")}</nav><div role="tabpanel" id="account-panel" ${state.accountTab ? `aria-labelledby="account-tab-${state.accountTab}"` : 'aria-label="Accounts"'}>${panel}</div></main>`;
}
function accountEditor(b, i) {
      const p = `backends.${i}`;
      const bedrock = b.adapter === "aws-bedrock";
      return `${bedrock ? "" : '<p class="muted">Credentials stay blank when loaded. Leave an existing key blank to preserve it.</p>'}<article><div class="heading"><h3 data-account-heading>${esc(displayName(b.id || "New account"))}</h3>${button("remove", "Delete account", p)}</div><div class="row">${field(p + ".id", "Account ID")}${field(p + ".adapter", "Adapter", "text", adapters)}</div>${b.adapter.startsWith("custom-") ? field(p + ".endpoint", "Endpoint URL (blank preserves existing)") : '<p class="muted">Official endpoint managed by adapter</p>'}${bedrock ? field(p + ".region", "AWS 区域") + '<p class="muted">使用运行环境的 IAM Role；无需 API Key。EC2 上由实例角色自动提供凭据。</p>' : ""}<h4>Credentials</h4>${b.credentials
        .map((c, j) => {
          const q = `${p}.credentials.${j}`;
          return `<div class="credential"><div class="row">${field(q + ".id", "Credential ID")}${bedrock ? "" : field(q + ".api_key", "API key (blank preserves existing)", "password")}${field(q + ".pool", "Shared pool", "text", ["", ...(state.draft.capacity_pools || []).map((p) => p.id)])}</div>${!c.pool ? button("add-pool", "Create capacity pool", q) : ""}${button("remove", "Delete credential", q)}</div>`;
        })
        .join(
          "",
        )}${!bedrock || !b.credentials.length ? button("add-credential", "+ Add credential", p + ".credentials") : ""}${capacityPanel(b)}<h4>Upstream models</h4>${b.models
        .map((m, j) => {
          const q = `${p}.models.${j}`;
          return `<fieldset><legend>Upstream model</legend>${field(q + ".name", "Upstream model name")}${bedrock ? '<p class="muted">原生协议：Anthropic Messages；其他协议使用网关转换。</p>' : choices(
            q + ".protocols",
            "Protocols (empty uses adapter defaults)",
            protocols.map((v) => [v, v]),
          )}${priceEditor(q,b.id,m)}${button("remove", "Delete upstream model", q)}</fieldset>`;
        })
        .join(
          "",
        )}${button("add-upstream", "+ Add upstream model", p + ".models")}</article>`;
}
function infoRows(rows) {
  return `<dl class="info-grid">${rows.map(([label, value]) => `<div><dt>${esc(label)}</dt><dd>${esc(value ?? "—")}</dd></div>`).join("")}</dl>`;
}
function poolText(id) {
  if (!id) return "No local limit";
  const pool = state.live?.pools?.find(row => row.id === id);
  return pool ? `${id} · ${pool.in_flight} / ${pool.max_in_flight} in flight` : `${id} · No live occupancy available`;
}
function rateSummary(rates) {
  return rates ? `Cached input ${rates.cache_hit_input_usd_per_million}; uncached input ${rates.cache_miss_input_usd_per_million}; output ${rates.output_usd_per_million} USD / million tokens` : "No published usage rates configured";
}
function accountSummary(backend) {
  const dayNames = ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
  return `<article><h2>${esc(displayName(backend.id))}</h2>${infoRows([["Adapter", displayName(backend.adapter)], ...(backend.adapter === "aws-bedrock" ? [["AWS 区域", backend.region], ["鉴权", "运行环境 IAM Role"]] : []), ["Endpoint", backend.adapter.startsWith("custom-") ? backend.endpoint || "Saved endpoint is redacted" : "Official endpoint managed by adapter"]])}<h3>Credentials</h3>${backend.credentials.length ? `<div class="table-scroll"><table><thead><tr><th>Credential</th><th>Shared capacity</th></tr></thead><tbody>${backend.credentials.map(credential => `<tr><td>${esc(credential.id)}</td><td >${esc(credential.pool || "No local limit")}</td></tr>`).join("")}</tbody></table></div>` : '<p class="muted">No credentials configured.</p>'}</article>${capacityPanel(backend)}<h2>Upstream models</h2>${backend.models.map(model => `<article><h3>${esc(model.name)}</h3>${infoRows([["Supported protocols", model.protocols?.length ? model.protocols.join(", ") : "Adapter defaults"], ["Billing context", model.pricing_note || "Not configured"], ["Rate reference", model.pricing_source || "Not configured"], ["Default rates", rateSummary(model.pricing)]])}<h4>Active rates</h4><p data-price-backend="${esc(backend.id)}" data-price-model="${esc(model.name)}">${esc(activePriceText(state.live?.pricing?.find(row => row.backend === backend.id && row.model === model.name)))}</p><p class="muted">Usage value is an estimate, not an actual cash bill. Subscription requests may consume quota.</p>${model.pricing_schedule ? `<h4>Weekly price windows · ${esc(model.pricing_schedule.timezone)}</h4><div class="table-scroll"><table><thead><tr><th>Window</th><th>Starting days</th><th>Start</th><th>End</th><th>Rates (USD / million tokens)</th></tr></thead><tbody>${model.pricing_schedule.windows.map(window => `<tr><td>${esc(window.name)}</td><td>${esc(window.days.map(day => dayNames[day] || day).join(", "))}</td><td>${esc(window.start)}</td><td>${esc(window.end)}</td><td>${esc(rateSummary(window.pricing))}</td></tr>`).join("")}</tbody></table></div>` : ""}</article>`).join("") || '<p class="muted">No upstream models configured.</p>'}`;
}
function poolReferences(id) {
  const pool = state.draft.capacity_pools.find(pool => pool.id === id);
  const bound = draftPoolBindings.get(pool) || [];
  return state.draft.backends.flatMap(backend => backend.credentials.filter(credential => (id && credential.pool === id) || bound.includes(credential)).map(credential => `${displayName(backend.id)} / ${credential.id}`));
}
function capacityPanel(backend) {
  const ids = new Set(backend.credentials.map(credential => credential.pool).filter(Boolean));
  const pools = state.draft.capacity_pools.map((pool, index) => ({pool, index})).filter(({pool}) => ids.has(pool.id) || draftPoolBindings.get(pool)?.some(credential => backend.credentials.includes(credential)));
  return `<section><h3>Shared capacity</h3>${pools.length ? pools.map(({pool, index}) => `<article><p data-pool-index="${index}" data-pool-count="${esc(pool.id)}" data-live-pool="${esc(pool.id)}">${esc(poolText(pool.id))}</p><p class="muted" data-pool-references="${index}">Shared by: ${esc(poolReferences(pool.id).join(", "))}</p>${state.editing ? `<div class="row">${field(`capacity_pools.${index}.id`, "Pool ID")}${field(`capacity_pools.${index}.max_in_flight`, "Maximum concurrent requests", "number")}${button("remove", "Delete pool", `capacity_pools.${index}`)}</div>` : infoRows([["Pool", pool.id], ["Maximum concurrent requests", pool.max_in_flight]])}</article>`).join("") : '<p class="muted">No local limit. No capacity pools assigned to this account.</p>'}</section>`;
}
const windows = [
  ["1h", "1 hour"],
  ["1d", "1 day"],
  ["1w", "1 week"],
  ["1m", "1 month"],
  ["all", "All time"],
];
const num = (value) => Number(value || 0).toLocaleString();
// One token shape everywhere: uncached input / output on the main line, cache-hit
// input and thinking tokens underneath. "—" means the upstream did not report a
// thinking-token count for that request or aggregate. "≈" marks a locally
// apportioned estimate used only when the upstream omitted the split while still
// streaming thinking text (Ollama Cloud never returns completion_tokens_details).
function tokenDetail(usage) {
  if (!usage) return '<span class="cell-main">—</span>';
  const miss = Number(usage.cache_miss_tokens || 0),
    hit = Number(usage.cache_hit_tokens || 0),
    out = Number(usage.output_tokens || 0),
    thinking = Number(usage.reasoning_tokens || 0),
    estimated = Number(usage.reasoning_estimated_tokens || 0);
  // Rows recorded before the report flag existed still prove reporting with a
  // nonzero count; only a zero without the flag stays unknown.
  const reported = usage.reasoning_reported === true || thinking > 0;
  const think = reported
    ? num(thinking)
    : estimated > 0
      ? "≈" + num(estimated)
      : "—";
  const thinkingText = reported
    ? num(thinking)
    : estimated > 0
      ? `≈${num(estimated)} (estimated from thinking text; upstream omitted the split)`
      : "not reported";
  const title = `Uncached input ${num(miss)} · cache-hit input ${num(hit)} · output ${num(out)} · thinking ${thinkingText} · total ${num(usage.total_tokens)}`;
  return `<span class="cell-main" title="${esc(title)}">${num(miss)} / ${num(out)}</span><span class="cell-sub">cache ↓ ${num(hit)} · think ${think}</span>`;
}
// History stays five columns wide: time over date, layer over account/key, so a
// laptop-width window does not need a horizontal scroll. A failed row adds the
// retained upstream discriminator under its status.
function historyRow(r) {
  const started = new Date(r.started_at_ms);
  const time = `<span class="cell-main">${esc(started.toLocaleTimeString())}</span><span class="cell-sub">${esc(started.toLocaleDateString())}</span>`;
  const layer = `<span class="cell-main">${esc(displayName(r.route_layer))}</span><span class="cell-sub">${esc(displayName(r.backend))} / ${esc(r.credential)}</span>`;
  const detail = upstreamErrorText(r);
  const status = detail
    ? `<span class="cell-main">${esc(r.status)}</span><span class="cell-sub" title="${esc(upstreamErrorTitle(r))}">${detail}</span>`
    : esc(r.status);
  return `<tr><td title="${esc(started.toLocaleString())}">${time}</td><td>${esc(r.served_model)}</td><td>${layer}</td><td>${status}</td><td class="numeric">${tokenDetail(r.usage)}</td></tr>`;
}
function shortText(value, limit = 72) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  return text.length > limit ? text.slice(0, limit - 1) + "…" : text;
}
function upstreamErrorText(row) {
  if (!row?.upstream_error_type && !row?.upstream_error_code && !row?.upstream_error_message)
    return "";
  const code = row.upstream_error_code || row.upstream_error_type;
  return esc(code ? shortText(code, 40) : shortText(row.upstream_error_message, 40));
}
function upstreamErrorTitle(row) {
  return [
    row.upstream_error_type ? `type: ${row.upstream_error_type}` : "",
    row.upstream_error_code ? `code: ${row.upstream_error_code}` : "",
    row.upstream_error_message || "",
  ]
    .filter(Boolean)
    .join("\n");
}
// Upstream rejections kept for the selected range: the type/code pair a
// provider returned plus its sanitized own message. Five columns with two-line
// cells keep it readable without a horizontal scroll, and rows recorded before
// retention state their failure class instead of an unexplained dash.
function upstreamErrorCell(row) {
  const retained = row.upstream_code || row.upstream_type;
  const shortClass = row.error_class
    ? row.error_class.replace(/^provider_/, "")
    : "";
  if (retained) {
    const code = shortText(row.upstream_code || row.upstream_type, 34);
    const sub =
      row.upstream_code &&
      row.upstream_type &&
      row.upstream_code !== row.upstream_type
        ? shortText(row.upstream_type, 34)
        : shortClass;
    return `<span class="cell-main">${esc(code)}</span>${sub ? `<span class="cell-sub">${esc(sub)}</span>` : ""}`;
  }
  if (shortClass) {
    return `<span class="cell-main">${esc(shortClass)}</span><span class="cell-sub">code not retained</span>`;
  }
  return `<span class="cell-main">—</span>`;
}
function upstreamErrorsPanel() {
  const errors = state.upstreamErrors?.errors || [];
  const rows = errors
    .map((row) => {
      const seen = new Date(row.last_seen_ms);
      const detail = [
        row.error_class,
        row.upstream_type,
        row.upstream_code,
        row.message,
      ]
        .filter(Boolean)
        .join("\n");
      return `<tr><td class="numeric">${num(row.count)}</td><td class="upstream-error" title="${esc(detail)}">${upstreamErrorCell(row)}</td><td title="${esc(seen.toLocaleString())}"><span class="cell-main">${row.backend ? esc(displayName(row.backend)) : "—"}</span><span class="cell-sub">${esc(seen.toLocaleDateString())} ${esc(seen.toLocaleTimeString())}</span></td><td class="upstream-message" title="${esc(row.message || "")}">${row.message ? esc(shortText(row.message, 96)) : "—"}</td></tr>`;
    })
    .join("");
  return `<h2>Upstream errors</h2><p class="muted">Failed attempts in this range, grouped by the upstream error type and code. Messages are the upstream's own sanitized text, retained locally for diagnosis. Rows recorded before September 20 keep no upstream code and group by their failure class.</p><div class="table-scroll"><table class="upstream-errors"><thead><tr><th class="numeric">Count</th><th>Upstream error</th><th>Account / last seen</th><th>Message</th></tr></thead><tbody>${rows || '<tr><td colspan="4" class="muted">No upstream errors recorded for this range.</td></tr>'}</tbody></table></div>`;
}
function updatedText() {
  return state.updatedAt
    ? `Updated ${new Date(state.updatedAt).toLocaleTimeString()}`
    : "Not updated yet";
}
function viewHead(title, controls) {
  return `<div class="view-head"><h1>${esc(title)}</h1><div class="view-actions"><span class="muted" id="updated-at">${esc(updatedText())}</span>${button("refresh", "Refresh", "", state.refreshing ? "disabled" : "")}</div></div>${controls}`;
}
function tokenCells(totals) {
  const reported = Number(totals?.reasoning_tokens || 0);
  const estimated = Number(totals?.reasoning_estimated_tokens || 0);
  const think = reported + estimated;
  const thinkTitle = estimated
    ? `${num(reported)} reported · ${num(estimated)} estimated from thinking text`
    : `${num(reported)} reported`;
  return `<td class="numeric">${num(totals?.cache_miss_tokens)}</td><td class="numeric">${num(totals?.cache_hit_tokens)}</td><td class="numeric">${num(totals?.output_tokens)}</td><td class="numeric" title="${esc(thinkTitle)}">${num(think)}${estimated ? " ≈" : ""}</td><td class="numeric">${num(totals?.total_tokens)}</td>`;
}
function scheduleOrder(path, model) {
  const order = get(path + ".order");
  return `<fieldset><legend>Layer priority for this window</legend>${order
    .map(
      (name, index) =>
        `<div class="row">${field(
          `${path}.order.${index}`,
          `Priority ${index + 1}`,
          "text",
          model.layers.map((layer) => layer.name),
        )}${button("up", "Move priority up", `${path}.order.${index}`, index === 0 ? "disabled" : "")}${button("down", "Move priority down", `${path}.order.${index}`, index === order.length - 1 ? "disabled" : "")}</div>`,
    )
    .join("")}</fieldset>`;
}
function priceFields(path) {
  return `<div class="row">${field(path+'.cache_hit_input_usd_per_million','Cached input USD / million','number')}${field(path+'.cache_miss_input_usd_per_million','Uncached input USD / million','number')}${field(path+'.output_usd_per_million','Output USD / million','number')}</div>`;
}
function activePriceText(row) {
  if(!row?.rates)return 'No published usage rates configured';
  const rates=row.rates;
  return `${row.active_window || 'Base rates'} · cached input ${rates.cache_hit_input_usd_per_million}, uncached input ${rates.cache_miss_input_usd_per_million}, output ${rates.output_usd_per_million} USD / million tokens`;
}
function priceEditor(path, backend, model) {
  return `<h4>Published usage rates (USD per million tokens)</h4><p class="muted" data-price-backend="${esc(backend)}" data-price-model="${esc(model.name)}">${esc(activePriceText(state.live.pricing?.find(row=>row.backend===backend&&row.model===model.name)))}</p><p class="muted">Usage value is an estimate, not an actual cash bill. Subscription requests may consume quota.</p>${field(path+'.pricing_note','Billing context (subscription, points or quota)')}${field(path+'.pricing_source','Rate reference URL or source')}${model.pricing?`${priceFields(path+'.pricing')}${button('remove-pricing','Remove published rates',path)}${button('add-price-window','Add price window',path)}`:button('add-pricing','Add pricing',path)}${model.pricing_schedule?`<div>${field(path+'.pricing_schedule.timezone','Price timezone (IANA name)')}${model.pricing_schedule.windows.map((window,index)=>{const q=`${path}.pricing_schedule.windows.${index}`;return `<fieldset><legend>Price window</legend><div class="row">${field(q+'.name','Price window name')}${field(q+'.start','Price start (HH:MM or HH:MM:SS)')}${field(q+'.end','Price end (HH:MM or HH:MM:SS)')}</div>${choices(q+'.days','Price starting days',[[1,'Mon'],[2,'Tue'],[3,'Wed'],[4,'Thu'],[5,'Fri'],[6,'Sat'],[7,'Sun']])}${priceFields(q+'.pricing')}${button('remove','Delete price window',q)}</fieldset>`;}).join('')}</div>`:''}`;
}
function advanced() {
  const tuning = {
    "server.max_inference_body_bytes": "Maximum request body (bytes)",
    "server.timeouts.upstream_connect_ms": "Upstream connection timeout (ms)",
    "server.timeouts.upstream_read_ms": "Upstream response timeout (ms)",
    "server.timeouts.upstream_stream_read_ms": "Stream inactivity timeout (ms)",
    "server.timeouts.upstream_total_ms": "Total upstream timeout (ms)",
    "server.timeouts.downstream_sse_heartbeat_ms":
      "Stream heartbeat interval (ms)",
    "server.timeouts.route_probe_wait_ms": "Route recovery wait (ms)",
    "affinity.checkpoint_bytes": "Prefix checkpoint size (bytes)",
    "affinity.max_checkpoints_per_path": "Maximum checkpoints per request",
    "affinity.max_candidates_per_prefix": "Maximum candidates per prefix",
    "affinity.max_leases": "Maximum affinity leases",
    "affinity.success_ttl_ms": "Successful affinity lifetime (ms)",
  };
  const location = infoRows([["Listener", state.draft.server.listen], ["Data directory", state.draft.server.data_dir]]);
  if (!state.editing) return `<h2>Runtime settings</h2>${location}${infoRows(Object.entries(tuning).map(([path, label]) => [label, get(path)]))}`;
  return `<h2>Runtime settings</h2><p class="muted">These settings apply on Save. Listener and data location require a restart.</p>${location}<div class="row">${Object.entries(tuning).map(([path, label]) => field(path, label, "number")).join("")}</div>`;
}
function childView() {
  const model = state.draft.models[state.selected];
  if (
    state.selected !== "all" &&
    (!model || !state.live.models.some((row) => row.name === model.name))
  )
    return "<h1>No saved data</h1><p>Save this model to view its statistics and history.</p>";
  if (state.loading) return '<p role="status">Loading selected scope…</p>';
  if (state.view === "statistics" && state.selected === "all")
    return globalStatistics();
  if (state.view === "history") {
    const rows = (state.history?.requests || []).map(historyRow).join("");
    return `${viewHead("Recent request history", `<div class="buttons">${button("newer", "Newer", "", state.cursors.length ? "" : "disabled")}<span>Page ${state.cursors.length + 1}</span>${button("older", "Older", "", state.nextCursor ? "" : "disabled")}</div>`)}<div class="table-scroll"><table><thead><tr><th>Started</th><th>Model</th><th>Layer / account</th><th>Status</th><th class="numeric">Uncached input / output</th></tr></thead><tbody>${rows || '<tr><td colspan="5" class="muted">No requests recorded for this scope.</td></tr>'}</tbody></table></div>`;
  }
  const targetRow = (target, layer, historical = false) =>
    `<tr><td>${historical ? "Historical target" : "Target"}</td><td>${esc(displayName(layer))}</td><td>${esc(displayName(target.backend))}</td><td>${esc(target.credential)}</td><td>${esc(target.upstream_model || target.model)}</td><td class="numeric">${num(target.totals?.calls)}</td>${tokenCells(target.totals)}</tr>`;
  const layers = state.stats?.layers || [];
  const historical = state.stats?.historical_targets || [];
  const unattributed = state.stats?.unattributed;
  return `${viewHead("Model statistics", `<div class="buttons">${windows.map(([id, label]) => button("window", label, id, `aria-pressed="${id === state.window}"`)).join("")}</div>`)}<p>Model total: ${num(state.stats?.totals?.total_tokens)} tokens · ${num(state.stats?.totals?.calls)} calls</p><div class="table-scroll"><table><thead><tr><th>Entry</th><th>Layer</th><th>Account</th><th>Key</th><th>Upstream model</th><th class="numeric">Calls</th><th class="numeric">Uncached input</th><th class="numeric">Cached input</th><th class="numeric">Output</th><th class="numeric">Think</th><th class="numeric">Total</th></tr></thead><tbody>${layers.map((layer) => `<tr><th>Layer total</th><td>${esc(displayName(layer.name))}</td><td></td><td></td><td></td><td class="numeric">${num(layer.totals?.calls)}</td>${tokenCells(layer.totals)}</tr>${(layer.targets || []).map((target) => targetRow(target, layer.name)).join("")}`).join("")}${historical.map((target) => targetRow(target, target.layer_name || target.layer_index, true)).join("")}${unattributed?.calls ? `<tr><td>No final target</td><td colspan="4">No final layer or target recorded for these requests.</td><td class="numeric">${num(unattributed.calls)}</td>${tokenCells(unattributed)}</tr>` : ""}${layers.length || historical.length || unattributed?.calls ? "" : '<tr><td colspan="11" class="muted">No requests recorded for this scope.</td></tr>'}</tbody></table></div>${upstreamErrorsPanel()}`;
}
function globalStatistics() {
  const rows = Object.entries(state.totals?.backends || {})
    .map(([id, row]) => {
      const total =
        Number(row.cache_hit_tokens || 0) + Number(row.cache_miss_tokens || 0);
      return `<tr><td>${esc(displayName(id))}</td><td class="numeric">${num(row.attempts)}</td><td class="numeric">${num(row.successes)}</td><td class="numeric">${num(row.errors)}</td>${tokenCells(row)}<td class="numeric">${row.cache_reported_attempts ? (total ? (100 * row.cache_hit_tokens) / total : 0).toFixed(1) + "%" : "—"}</td><td class="numeric">${Number(row.cost_usd || 0).toFixed(6)}</td></tr>`;
    })
    .join("");
  const alerts = (state.alerts?.alerts || [])
    .filter((a) => a.active)
    .map(
      (a) =>
        `<tr><td>${esc(displayName(a.backend))}</td><td>${esc(a.credential)}</td><td>${esc(a.class)}</td><td>${esc(new Date(a.last_seen_ms).toLocaleString())}</td><td>${a.next_probe_at_ms ? esc(new Date(a.next_probe_at_ms).toLocaleString()) : "—"}</td></tr>`,
    )
    .join("");
  return `${viewHead("All statistics", `<div class="buttons">${windows.map(([id, label]) => button("window", label, id, `aria-pressed="${id === state.window}"`)).join("")}</div>`)}<p class="muted">Account totals count upstream attempts. Token columns are attempt usage: uncached input / cached input / output / thinking-token totals.</p><h2>Account totals</h2><div class="table-scroll"><table><thead><tr><th>Account</th><th class="numeric">Attempts</th><th class="numeric">Successes</th><th class="numeric">Errors</th><th class="numeric">Uncached input</th><th class="numeric">Cached input</th><th class="numeric">Output</th><th class="numeric">Think</th><th class="numeric">Total</th><th class="numeric">Cache hit</th><th class="numeric">Usage value estimate (USD)</th></tr></thead><tbody>${rows || '<tr><td colspan="11" class="muted">No account attempts recorded for this range.</td></tr>'}</tbody></table></div><h2>Active alerts</h2><p class="muted">Current account and credential alerts, independent of the statistics time range.</p><div class="table-scroll"><table><thead><tr><th>Account</th><th>Key</th><th>Class</th><th>Last seen</th><th>Next probe</th></tr></thead><tbody>${alerts}</tbody></table></div>${upstreamErrorsPanel()}`;
}
function updateLive() {
  document.querySelectorAll("[data-pool-status]").forEach(node => node.textContent = poolText(node.dataset.poolStatus));
  document.querySelectorAll("[data-pool-count]").forEach(node => node.textContent = node.dataset.poolCount ? poolText(node.dataset.poolCount) : "Pool name pending · No live occupancy available");
  document.querySelectorAll("[data-price-backend]").forEach(node=>node.textContent=activePriceText(state.live?.pricing?.find(row=>row.backend===node.dataset.priceBackend&&row.model===node.dataset.priceModel)));
  const m = state.draft?.models[state.selected];
  const live = state.live?.models.find((row) => row.name === m?.name);
  const order = document.querySelector("#effective-order");
  if (order)
    order.textContent = live
      ? `Live priority: ${live.effective_order.map(displayName).join(" → ")} · ${live.active_schedule || "Default order"}`
      : "New model — not saved yet";
  document.querySelectorAll("[data-live-target]").forEach((node) => {
    const [layer, backend, credential, model] =
      node.dataset.liveTarget.split("|");
    const t = live?.layers
      .find((l) => l.name === layer)
      ?.targets.find(
        (t) =>
          t.backend === backend &&
          t.credential === credential &&
          t.upstream_model === model,
      );
    node.textContent = t
      ? `${t.circuit?.mode || "Unknown health"} · ${t.pool ? `Pool ${t.pool}: ${t.in_flight} / ${t.max_in_flight} in flight` : "No local capacity limit"}`
      : "Not in active routing";
  });
}
function migrationMessage() {
  return state.live?.migration?.persisted === false
    ? "Active routing loaded. Make the configuration directory writable to save the migration."
    : "";
}
function status() {
  const n = document.querySelector("#draft-status");
  if (n) n.textContent = state.dirty ? "Unsaved changes" : "All changes saved";
  const notice = document.querySelector("#notice");
  if (notice) notice.textContent = state.message;
  const migration = document.querySelector("#migration-notice");
  if (migration) migration.textContent = migrationMessage();
}
async function request(path, options = {}) {
  const response = await fetch(path, {
    cache: "no-store",
    ...options,
    headers: {
      ...options.headers,
    },
  });
  const data = await response.json();
  if (!response.ok)
    throw Error(
      `${response.status === 409 ? "Conflict: " : ""}${data.error || response.statusText}`,
    );
  return data;
}
function adopt(data) {
  draftPoolBindings = new Map();
  const accountIndex = Number(state.accountTab?.split(":")[1]);
  const selectedAccountId = state.draft?.backends[accountIndex]?.id;
  const selectedName = state.draft?.models[state.selected]?.name;
  const wasAll = state.selected === "all";
  resetChildren();
  state.preview = null;
  state.live = data;
  state.draft = structuredClone(data.document);
  state.draft.capacity_pools ||= [];
  state.draft.models.forEach((m) => {
    m.timezone ||= "UTC";
    m.schedules ||= [];
  });
  if (state.accountTab?.startsWith("backend:")) {
    const matchedAccount = state.draft.backends.findIndex(backend => backend.id === selectedAccountId);
    state.accountTab = `backend:${matchedAccount >= 0 ? matchedAccount : Math.min(accountIndex, state.draft.backends.length - 1)}`;
  }
  normalizeAccountTab();
  state.revision = data.revision;
  const matched = state.draft.models.findIndex(model => model.name === selectedName);
  state.selected = wasAll || !state.draft.models.length ? "all" : matched >= 0 ? matched : Math.min(state.selected, Math.max(0, state.draft.models.length - 1));
  if (state.pendingScope) {
    const pending = state.draft.models.findIndex(model => model.name === state.pendingScope);
    if (pending >= 0) state.selected = pending;
    state.pendingScope = null;
  }
  state.dirty = false;
  if (state.selected === 'all' && state.view === 'routing') state.view = 'statistics';
  if (state.page === "workspace" && state.view !== 'routing') queueMicrotask(() => children());
}
async function refresh() {
  try {
    const data = await request("/api/routing");
    state.live = data;
    if (
      !state.draft ||
      (!state.dirty && !state.busy && data.revision !== state.revision)
    ) {
      adopt(data);
      render();
    } else {
      if (state.dirty && data.revision !== state.revision && !state.busy) {
        state.message =
          "Routing changed elsewhere. Your draft is retained; use Discard to load the latest saved routing.";
      }
      updateLive();
      status();
    }
  } catch (e) {
    state.message = e.message;
    if (!state.draft)
      app.innerHTML = `<main><h1>Routing unavailable</h1><p>${esc(e.message)}</p></main>`;
    else status();
  }
}
function resetChildren() {
  state.childGeneration++;
  state.cursor = null;
  state.cursors = [];
  state.nextCursor = null;
  state.stats = null;
  state.totals = null;
  state.alerts = null;
  state.upstreamErrors = null;
  state.history = null;
  state.loading = false;
  state.refreshing = false;
}
// A silent child reload keeps the rendered table, pagination and focus intact and
// only swaps in fresher data. Manual and 20-second automatic refreshes both use it.
async function children(options = {}) {
  if (state.page !== "workspace" || state.view === "routing") return;
  const silent = options.silent === true;
  const generation = ++state.childGeneration;
  const selected = state.selected,
    view = state.view,
    window = state.window,
    cursor = state.cursor;
  const model = state.draft.models[selected];
  if (
    selected !== "all" &&
    (!model || !state.live.models.some((row) => row.name === model.name))
  ) {
    render();
    return;
  }
  if (!silent) {
    state.loading = true;
    render();
  } else {
    state.refreshing = true;
    const updated = document.querySelector("#updated-at");
    if (updated) updated.textContent = "Refreshing…";
    const control = document.querySelector('[data-action="refresh"]');
    if (control) control.disabled = true;
  }
  try {
    let result;
    if (view === "statistics") {
      // Upstream rejections are aggregated on demand for the same window and
      // scope, so the panel never needs a rollup schema change.
      const errors = request(
        `/api/upstream-errors?window=${window}` +
          (selected === "all"
            ? ""
            : "&model=" + encodeURIComponent(model.name)),
      );
      result =
        selected === "all"
          ? await Promise.all([request(`/api/stats?window=${window}`), request("/api/status"), errors])
          : await Promise.all([
              request(
                `/api/routing/stats?model=${encodeURIComponent(model.name)}&window=${window}`,
              ),
              errors,
            ]);
    }
    if (view === "history")
      result = await request(
        "/api/requests?limit=20" +
          (selected === "all"
            ? ""
            : "&model=" + encodeURIComponent(model.name)) +
          (cursor ? "&before=" + encodeURIComponent(cursor) : ""),
      );
    if (generation !== state.childGeneration) return;
    state.loading = false;
    state.refreshing = false;
    state.updatedAt = Date.now();
    if (view === "statistics") {
      if (selected === "all") [state.totals, state.alerts, state.upstreamErrors] = result;
      else [state.stats, state.upstreamErrors] = result;
    }
    if (view === "history") {
      state.history = result;
      state.nextCursor = result.next_cursor;
    }
    render();
  } catch (error) {
    if (generation !== state.childGeneration) return;
    state.loading = false;
    state.refreshing = false;
    state.message = error.message;
    render();
  }
}
// --- Location hash -----------------------------------------------------------
// #/workspace/<all|model:NAME>/<view>/<window>, #/accounts, #/settings
// The hash is the single source of truth for reload, back/forward and bookmarks.
function locationState() {
  const parts = location.hash
    .replace(/^#\/?/, "")
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent);
  return {
    page: ["workspace", "accounts", "settings"].includes(parts[0]) ? parts[0] : "workspace",
    scope: parts[1] || "all",
    view: ["statistics", "history", "routing"].includes(parts[2]) ? parts[2] : "statistics",
    window: windows.some(([id]) => id === parts[3]) ? parts[3] : "1d",
  };
}
function hashFor() {
  if (state.page === "accounts" || state.page === "settings") return `#/${state.page}`;
  // Only the model name is escaped so the scope stays readable in a bookmark.
  const scope =
    state.selected === "all"
      ? "all"
      : `model:${encodeURIComponent(state.draft?.models[state.selected]?.name || "")}`;
  return `#/workspace/${scope}/${state.view}/${state.window}`;
}
function syncHash(push = true) {
  if (!state.draft) return;
  const next = hashFor();
  if (location.hash === next) return;
  if (push) location.hash = next;
  else history.replaceState(null, "", next);
}
function applyLocation() {
  if (!state.draft) return;
  const target = locationState();
  const wanted = target.scope.startsWith("model:") ? target.scope.slice(6) : "all";
  const current = state.selected === "all" ? "all" : state.draft.models[state.selected]?.name || "";
  const sameScope = wanted === "all" ? state.selected === "all" : current === wanted;
  if (
    state.page === target.page &&
    state.view === target.view &&
    state.window === target.window &&
    sameScope
  )
    return;
  resetChildren();
  state.page = target.page;
  if (target.page === "workspace") {
    const found = state.draft.models.findIndex((model) => model.name === wanted);
    state.selected = wanted === "all" || found < 0 ? "all" : found;
    state.view =
      state.selected === "all" && target.view === "routing" ? "statistics" : target.view;
    state.window = target.window;
  }
  state.message = "";
  render();
  if (state.page === "workspace" && state.view !== "routing") children();
}
const poolRenameBindings = new WeakMap();
let draftPoolBindings = new Map();
app.addEventListener("input", (event) => {
  const n = event.target;
  if (n.id === "preview-at") {
    state.at = n.value;
    return;
  }
  if (!state.editing || state.busy) return;
  if (n.dataset.field?.match(/^models\.\d+\.layers\.\d+\.name$/)) {
    const old = get(n.dataset.field);
    const model = get(n.dataset.field.split(".").slice(0, 2).join("."));
    model.schedules.forEach(
      (schedule) =>
        (schedule.order = schedule.order.map((name) =>
          name === old ? n.value : name,
        )),
    );
  }
  if (/^capacity_pools\.\d+\.id$/.test(n.dataset.field || "")) {
    const oldId = get(n.dataset.field);
    const pool = get(n.dataset.field.split(".").slice(0, -1).join("."));
    if (!poolRenameBindings.has(n)) {
      const credentials = state.draft.backends.flatMap(backend => backend.credentials);
      poolRenameBindings.set(n, oldId
        ? credentials.filter(credential => credential.pool === oldId)
        : (draftPoolBindings.get(pool) || []).filter(credential => credentials.includes(credential) && credential.pool === oldId));
    }
    draftPoolBindings.set(pool, poolRenameBindings.get(n));
    poolRenameBindings.get(n).forEach(credential => { credential.pool = n.value; });
  }
  if (n.dataset.field) {
    set(
      n.dataset.field,
      n.dataset.kind === "list"
        ? n.value
            .split(",")
            .map((v) => v.trim())
            .filter(Boolean)
        : n.dataset.kind === "number"
          ? Number(n.value)
          : n.value,
    );
  }
  if (n.dataset.choice) {
    const numeric = n.dataset.choice.endsWith(".days");
    const v = numeric ? Number(n.value) : n.value;
    const values = new Set(get(n.dataset.choice) || []);
    n.checked ? values.add(v) : values.delete(v);
    set(n.dataset.choice, [...values]);
  }
});
app.addEventListener("change", (event) => {
  if (!state.editing || state.busy) return;
  const path = event.target.dataset.field;
  if (path?.endsWith(".pool")) {
    const credential = get(path.split(".").slice(0, -1).join("."));
    draftPoolBindings.forEach((bindings, pool) => draftPoolBindings.set(pool, bindings.filter(item => item !== credential)));
    render();
  }
  if (path?.endsWith(".backend")) {
    const target = get(path.slice(0, -8));
    target.credential = "";
    target.model = "";
    render();
  }
  if (path?.endsWith(".adapter")) {
    const backend = get(path.slice(0, -8));
    delete backend.endpoint;
    delete backend.region;
    backend.credentials.forEach(credential => credential.api_key = "");
    if (backend.adapter === "aws-bedrock") {
      backend.region = "us-east-1";
      backend.credentials = backend.credentials.slice(0, 1);
    }
    const adapterProtocols = {
      "opencode-go": [],
      "aws-bedrock": [],
      "custom-chat-completions": ["openai-chat"],
      "custom-responses": ["openai-responses"],
      "custom-anthropic": ["anthropic-messages"],
      "kimi-code": ["openai-chat", "anthropic-messages"],
      "kimi-official": ["openai-chat"],
      "ollama-cloud": ["openai-chat"],
    };
    backend.models.forEach(
      (model) =>
        (model.protocols = [
          ...(adapterProtocols[backend.adapter] || protocols),
        ]),
    );
    render();
  }
  if (path?.match(/\.order\.\d+$/)) {
    const keys = path.split(".");
    const index = Number(keys.pop());
    const order = get(keys.join("."));
    const names = state.draft.models[state.selected].layers.map(
      (layer) => layer.name,
    );
    const missing = names.find((name) => !order.includes(name));
    const duplicate = order.findIndex(
      (name, i) => i !== index && name === order[index],
    );
    if (duplicate >= 0 && missing) order[duplicate] = missing;
    render();
  }
  if (path?.endsWith(".name") || path?.endsWith(".id")) {
    // Refresh references in place so a blur cannot replace the clicked control.
    app.querySelectorAll('select[data-field]').forEach(select => {
      const reference = select.dataset.field;
      let values;
      if (reference.endsWith('.pool')) values = ['', ...state.draft.capacity_pools.map(pool => pool.id)];
      else if (reference.endsWith('.backend')) values = ['', ...state.draft.backends.map(backend => backend.id)];
      else if (/\.targets\.\d+\.(credential|model)$/.test(reference)) {
        const target = get(reference.split('.').slice(0, -1).join('.'));
        const backend = state.draft.backends.find(row => row.id === target.backend);
        values = reference.endsWith('.credential') ? ['', ...(backend?.credentials || []).map(row => row.id)] : ['', ...(backend?.models || []).map(row => row.name)];
      } else if (/\.order\.\d+$/.test(reference)) values = get(reference.split('.').slice(0, 2).join('.')).layers.map(layer => layer.name);
      if (values) {
        const current = get(reference);
        select.innerHTML = [...new Set([...(current ? [current] : []), ...values])].map(value => `<option value="${esc(value)}" ${value === current ? 'selected' : ''}>${esc(optionName(reference, value) || 'None')}</option>`).join('');
      }
    });
    state.draft.backends.forEach((backend, index) => {
      const tab = app.querySelector(`[data-action="account-tab"][data-path="backend:${index}"]`);
      if (tab) tab.textContent = displayName(backend.id || "New account");
    });
    app.querySelectorAll("[data-pool-index]").forEach(node => {
      const pool = state.draft.capacity_pools[Number(node.dataset.poolIndex)];
      if (pool) {
        node.dataset.poolCount = pool.id;
        node.dataset.livePool = pool.id;
        node.textContent = pool.id ? poolText(pool.id) : "Pool name pending · No live occupancy available";
      }
    });
    app.querySelectorAll("[data-pool-references]").forEach(node => {
      const pool = state.draft.capacity_pools[Number(node.dataset.poolReferences)];
      if (pool) node.textContent = `Shared by: ${poolReferences(pool.id).join(", ")}`;
    });
    const accountHeading = app.querySelector("[data-account-heading]");
    if (accountHeading) accountHeading.textContent = displayName(state.draft.backends[Number(state.accountTab?.split(":")[1])]?.id || "New account");
    state.draft.models.forEach((model, index) => {
      const rail = app.querySelector(`[data-action="select"][data-path="${index}"]`);
      if (rail) rail.textContent = model.name || 'New model';
    });
    if (state.page === "workspace" && state.view === 'routing') {
      const heading = app.querySelector('main h1');
      if (heading) heading.textContent = state.draft.models[state.selected]?.name || 'New model';
    }
  }
});
function accountDependencies(id) {
  if (!id) return [];
  return state.draft.models.flatMap(model => model.layers
    .filter(layer => layer.targets.some(target => target.backend === id))
    .map(layer => `${model.name || "New model"} / ${displayName(layer.name || "New layer")}`));
}
function confirmRemoval(path, item) {
  const collection = path.split(".").at(-2);
  const type = collection === "models"
    ? (path.startsWith("backends.") ? "upstream model" : "model")
    : ({ layers: "layer", targets: "target", schedules: "time window", backends: "account", credentials: "credential", windows: "price window", capacity_pools: "shared capacity pool" })[collection] || "item";
  const name = item?.name || item?.id || (collection === "targets" ? [displayName(item.backend), item.credential, item.model].filter(Boolean).join(" / ") : "") || "Unnamed";
  const impact = collection === "capacity_pools" ? ` This clears pool assignments for: ${poolReferences(item.id).join(", ") || "no credentials"}. Those credentials will have no local concurrency limit.` : "";
  return confirm(`Delete ${type} "${displayName(name)}"?${impact} This changes only the draft until Save.`);
}
app.addEventListener("click", async (event) => {
  const n = event.target.closest("[data-action]");
  if (!n || n.disabled || state.busy) return;
  const a = n.dataset.action,
    p = n.dataset.path;
  if ((!state.editing) && !browseActions.has(a)) return;
  try {
    if (a === "enable-edit") {
      state.editing = true;
      render();
      return;
    }
    if (a === "disable-edit") {
      if (state.dirty) {
        if (!confirm("Discard unsaved changes and disable editing?")) return;
        adopt(await request("/api/routing"));
      }
      state.editing = false;
      render();
      return;
    }
    if (a === "window") {
      state.window = p;
      syncHash();
      await children();
      return;
    }
    if (a === "refresh") {
      await children({ silent: true, manual: true });
      return;
    }
    if (a === "older") {
      if (state.nextCursor) {
        state.cursors.push(state.cursor);
        state.cursor = state.nextCursor;
        await children();
      }
      return;
    }
    if (a === "newer") {
      if (state.cursors.length) {
        state.cursor = state.cursors.pop();
        await children();
      }
      return;
    }
    if (a === "account-tab") {
      if (!(/^backend:\d+$/.test(p) && state.draft.backends[Number(p.split(":")[1])])) return;
      state.accountTab = p;
      render();
      return;
    }
    if (["accounts", "workspace", "settings"].includes(a)) {
      if (state.page === a) return;
      resetChildren();
      state.page = a;
      syncHash();
      render();
      if (state.page === "workspace") await children();
      return;
    }
    if (a === "select") {
      resetChildren();
      state.selected = p === "all" ? "all" : Number(p);
      state.view = "statistics";
      state.window = "1d";
      state.preview = null;
      syncHash();
      render();
      if (state.view !== "routing") await children();
      return;
    }
    if (a === "view") {
      if (state.selected === "all" && p === "routing") return;
      resetChildren();
      state.view = p;
      syncHash();
      render();
      if (p !== "routing") await children();
      return;
    }
    if (a === "discard") {
      if (state.dirty && !confirm("Discard all unsaved changes?")) return;
      adopt(await request("/api/routing"));
      state.message = "Draft discarded.";
      render();
      return;
    }
    if (a === "save") {
      if (!confirm("Save all pending changes, including accounts, prices and shared capacity? After validation, the saved configuration applies immediately to new requests.")) return;
      state.busy = true;
      state.message = "Validating upstreams and saving…";
      render();
      const doc = structuredClone(state.draft);
      doc.backends.forEach((b) =>
        b.credentials.forEach((c) => {
          if (!c.pool) delete c.pool;
        }),
      );
      try {
        adopt(
          await request("/api/routing", {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ revision: state.revision, document: doc }),
          }),
        );
        state.message = "Saved. New requests use this routing.";
      } finally {
        state.busy = false;
        render();
      }
      return;
    }
    if (a === "preview") {
      const generation = state.childGeneration;
      const selectedName = state.draft.models[state.selected]?.name;
      const view = state.view;
      const instant = state.at;
      const data = await request("/api/routing?at=" + encodeURIComponent(instant));
      if (generation !== state.childGeneration || view !== state.view || selectedName !== state.draft.models[state.selected]?.name || instant !== state.at) return;
      const model = data.models.find(model => model.name === selectedName);
      const pairs = new Set((model?.layers || []).flatMap(layer => layer.targets.map(target => JSON.stringify([target.backend, target.upstream_model]))));
      state.preview = model ? `${model.active_schedule || "Default order"}: ${model.effective_order.map(displayName).join(" → ")}` : "Model not yet saved";
      state.preview += "\n" + (data.pricing || []).filter(row => pairs.has(JSON.stringify([row.backend, row.model]))).map(row => `${displayName(row.backend)}/${row.model}: ${activePriceText(row)}`).join("\n");
      const result = document.querySelector('#preview-result');
      if (result) result.textContent = state.preview;
      return;
    }
    if (["remove", "up", "down"].includes(a)) {
      const keys = p.split("."),
        i = Number(keys.pop()),
        array = get(keys.join("."));
      if (!Array.isArray(array) || i < 0 || i >= array.length) return;
      if (a === "remove") {
        if (/^backends\.\d+$/.test(p)) {
          const dependencies = accountDependencies(array[i].id);
          if (dependencies.length) {
            state.message = `Cannot delete account "${displayName(array[i].id)}". Referenced by: ${dependencies.join(", ")}. Remove or replace these routing targets first.`;
            status();
            return;
          }
        }
        if (!confirmRemoval(p, array[i])) return;
        if (/^backends\.\d+$/.test(p)) state.message = "";
        if (p.match(/^models\.\d+\.layers\.\d+$/)) {
          const model = get(keys.slice(0, 2).join("."));
          model.schedules.forEach(
            (schedule) =>
              (schedule.order = schedule.order.filter(
                (name) => name !== array[i].name,
              )),
          );
        }
        if (/^capacity_pools\.\d+$/.test(p)) {
          const pool = array[i], id = pool.id;
          const bound = draftPoolBindings.get(pool) || [];
          state.draft.backends.forEach(backend => backend.credentials.forEach(credential => { if ((id && credential.pool === id) || bound.includes(credential)) delete credential.pool; }));
          draftPoolBindings.delete(pool);
        }
        array.splice(i, 1);
        if (/^backends\.\d+$/.test(p)) {
          const selectedAccount = Number(state.accountTab?.split(":")[1]);
          if (state.accountTab?.startsWith("backend:")) state.accountTab = `backend:${selectedAccount > i ? selectedAccount - 1 : Math.min(selectedAccount, array.length - 1)}`;
          normalizeAccountTab();
        }
        if (/^models\.\d+$/.test(p)) {
          resetChildren();
          state.selected = array.length ? Math.min(i, array.length - 1) : "all";
          state.view = "statistics";
          state.window = "1d";
          state.metric = "calls";
          state.preview = null;
        }
      } else {
        const j = i + (a === "up" ? -1 : 1);
        if (j >= 0 && j < array.length)
          [array[i], array[j]] = [array[j], array[i]];
      }
      state.selected = state.selected === "all" ? "all" : Math.min(
        state.selected,
        Math.max(0, state.draft.models.length - 1),
      );
    }
    if (a === "add-pool") {
      const credential = get(p);
      if (!/^backends\.\d+\.credentials\.\d+$/.test(p) || !credential || credential.pool) return;
      const backend = get(p.split(".").slice(0, 2).join("."));
      const base = `${backend.id || "account"}-${credential.id || "credential"}`.replace(/[^a-zA-Z0-9_-]/g, "-") || "pool";
      let id = base, suffix = 2;
      while (state.draft.capacity_pools.some(pool => pool.id === id)) id = `${base}-${suffix++}`;
      state.draft.capacity_pools.push({ id, max_in_flight: 1 });
      credential.pool = id;
      state.dirty = true;
      render();
      return;
    }
    const defaults = {
      "add-model": {
        name: "",
        aliases: [],
        protocols: ["openai-chat"],
        layers: [],
        timezone: "UTC",
        schedules: [],
      },
      "add-layer": { name: "", strategy: "random", targets: [] },
      "add-target": { backend: "", credential: "", model: "" },
      "add-backend": {
        id: "",
        adapter: "custom-chat-completions",
        endpoint: "",
        credentials: [],
        models: [],
      },
      "add-credential": { id: "", api_key: "" },
      "add-upstream": { name: "", protocols: ["openai-chat"] },
      "add-schedule": {
        name: "",
        days: [1, 2, 3, 4, 5, 6, 7],
        start: "09:00",
        end: "17:00",
        order:
          state.draft.models[state.selected]?.layers.map((l) => l.name) || [],
      },
    };
    if (defaults[a]) {
      const array =
        a === "add-model"
          ? state.draft.models
          : a === "add-backend"
            ? state.draft.backends
            : get(p);
      if (!Array.isArray(array)) return;
      if (a === "add-upstream" && get(p.split(".").slice(0, 2).join(".")).adapter === "aws-bedrock") {
        defaults[a].protocols = [];
      }
      array.push(defaults[a]);
      if (a === "add-backend") state.accountTab = `backend:${array.length - 1}`;
      if (a === "add-layer") {
        get(p.split(".").slice(0, 2).join(".")).schedules.forEach((schedule) =>
          schedule.order.push(""),
        );
      }
      if (a === "add-model") {resetChildren();state.selected = array.length - 1;state.view="routing";}
    }
    if (a === "add-pricing")
      get(p).pricing = {
        cache_hit_input_usd_per_million: 0,
        cache_miss_input_usd_per_million: 0,
        output_usd_per_million: 0,
      };
    if (a === "remove-pricing") {
      const model = get(p);
      if (!model || !confirm(`Remove published rates and all price windows for upstream model "${model.name || "Unnamed"}"? This changes only the draft until Save.`)) return;
      delete model.pricing;
      delete model.pricing_schedule;
    }
    if (a === "add-price-window") {
      const model=get(p);
      model.pricing_schedule ||= {timezone:'UTC',windows:[]};
      model.pricing_schedule.windows.push({name:'',days:[1,2,3,4,5,6,7],start:'09:00',end:'17:00',pricing:structuredClone(model.pricing)});
    }
    state.dirty = true;
    render();
    if (a === "remove" && /^models\.\d+$/.test(p)) await children();
  } catch (e) {
    state.message = e.message;
    status();
  }
});
const initial = locationState();
state.page = initial.page;
state.view =
  initial.scope === "all" && initial.view === "routing" ? "statistics" : initial.view;
state.window = initial.window;
state.pendingScope = initial.scope.startsWith("model:") ? initial.scope.slice(6) : null;
window.addEventListener("hashchange", applyLocation);
render();
refresh();
// Live routing state keeps its short poll so circuit and capacity views stay
// current; statistics and history data refresh every 20 seconds and on demand.
setInterval(refresh, 5000);
setInterval(() => {
  if (!state.draft || state.page !== "workspace" || state.view === "routing") return;
  if (state.busy || state.loading || state.refreshing) return;
  children({ silent: true });
}, 20000);
