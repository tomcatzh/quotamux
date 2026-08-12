const state = {
  view: 'overview',
  routeView: 'structure',
  models: [],
  model: '',
  window: '1h',
  metric: 'calls',
  statsData: null,
};
const windows = [['1h','1 hour'],['1d','1 day'],['1w','1 week'],['1m','1 month'],['all','All time']];
const metrics = [['calls','Calls'],['total_tokens','Total tokens'],['input_tokens','Input tokens'],['output_tokens','Output tokens']];
const $ = (selector) => document.querySelector(selector);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (character) => ({
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
})[character]);
const num = (value) => Number(value || 0).toLocaleString();
const when = (value) => value == null ? '—' : new Date(value).toLocaleString();
const list = (value) => (value || []).length ? (value || []).map(esc).join(', ') : '—';
const metricLabel = () => metrics.find(([id]) => id === state.metric)?.[1] || 'Calls';
const emptyRow = (columns, text) => `<tr><td class="empty" colspan="${columns}">${esc(text)}</td></tr>`;

async function api(path) {
  const response = await fetch(path, { cache: 'no-store' });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `${response.status} ${response.statusText}`);
  return payload;
}

function showError(error) {
  const node = $('#error');
  node.textContent = error?.message || String(error);
  node.hidden = false;
}

function clearError() {
  $('#error').hidden = true;
  $('#error').textContent = '';
}

function aliases(model) {
  return model.aliases?.length ? ` <span class="aliases">(${model.aliases.map(esc).join(', ')})</span>` : '';
}

function activeModel() {
  return state.models.find((model) => model.name === state.model) || state.models[0];
}

function renderOverview(stats, status, recent) {
  const requests = stats.requests || {};
  const cards = [
    ['Requests', num(requests.total)],
    ['Logical errors', num(requests.errors)],
    ['Fallbacks', num(requests.fallbacks)],
    ['Output tokens', num(requests.output_tokens)],
    ['Traffic', `${(Number(requests.bytes || 0) / 1048576).toFixed(2)} MiB`],
  ];
  $('#summary-cards').innerHTML = cards.map(([label, value]) => `<div class="card"><div class="label">${esc(label)}</div><div class="value">${esc(value)}</div></div>`).join('');
  const providers = Object.entries(stats.providers || {});
  $('#provider-rows').innerHTML = providers.map(([provider, row]) => {
    const reported = Number(row.cache_reported_attempts || 0);
    const hit = Number(row.cache_hit_tokens || 0);
    const miss = Number(row.cache_miss_tokens || 0);
    const cache = reported ? (hit + miss ? `${(100 * hit / (hit + miss)).toFixed(1)}%` : '0.0%') : '—';
    return `<tr><th scope="row">${esc(provider)}</th><td>${list(row.served_models)}</td><td>${list(row.models)}</td><td class="numeric">${num(row.attempts)}</td><td class="numeric good">${num(row.successes)}</td><td class="numeric ${row.errors ? 'bad' : ''}">${num(row.errors)}</td><td class="numeric">${cache}</td><td class="numeric">$${Number(row.cost_usd || 0).toFixed(6)}</td></tr>`;
  }).join('') || emptyRow(8, 'No provider attempts recorded');
  $('#request-rows').innerHTML = (recent.requests || []).map((row) => `<tr><td>${when(row.started_at_ms)}</td><td>${esc(row.served_model || '—')}</td><td>${esc(row.route_layer || '—')}</td><td>${esc(row.provider || '—')}</td><td>${esc(row.credential || '—')}</td><td>${esc(row.upstream_model || '—')}</td><td class="numeric ${row.status >= 400 ? 'bad' : 'good'}">${num(row.status)}</td><td>${row.fallback ? 'Yes' : 'No'}</td><td class="numeric">${num(row.total_ms)} ms</td><td class="numeric">${num(row.usage?.total_tokens)}</td></tr>`).join('') || emptyRow(10, 'No requests recorded');
  const alerts = (status.alerts || []).filter((row) => row.active);
  $('#alert-rows').innerHTML = alerts.map((row) => `<tr><th scope="row">${esc(row.provider)}</th><td>${esc(row.credential || '—')}</td><td class="bad">${esc(row.class)}</td><td>${when(row.last_seen_ms)}</td><td>${when(row.next_probe_at_ms)}</td></tr>`).join('') || emptyRow(5, 'No active alerts');
}

function circuitLabel(circuit) {
  if (!circuit) return ['Unknown', ''];
  return {
    closed: ['Active', 'state-closed'],
    open: ['Avoided', 'state-open'],
    'half-open': ['Half-open probe', 'state-half-open'],
    suspended: ['Suspended', 'state-suspended'],
  }[circuit.mode] || [circuit.mode, ''];
}

function renderStructure(model) {
  const rows = [];
  for (const layer of model?.layers || []) {
    for (const target of layer.targets || []) {
      const [label, className] = circuitLabel(target.circuit);
      rows.push(`<tr><th scope="row" class="layer-cell">${num(layer.index + 1)} · ${esc(layer.name)}</th><td class="strategy">${esc(layer.strategy)}</td><td>${esc(target.provider)}</td><td>${esc(target.credential)}</td><td>${esc(target.upstream_model)}</td><td><span class="state ${className}">${esc(label)}</span></td><td>${esc(target.circuit?.reason || '—')}</td><td>${when(target.circuit?.next_probe_at_ms)}</td></tr>`);
    }
  }
  $('#structure-rows').innerHTML = rows.join('') || emptyRow(8, 'No routing targets configured');
}

function renderModelControl() {
  const node = $('#model-control');
  if (state.models.length > 1) {
    node.innerHTML = `<label class="label" for="model-select">Model</label><br><select id="model-select" class="model-select">${state.models.map((model) => `<option value="${esc(model.name)}" ${model.name === state.model ? 'selected' : ''}>${esc(model.name)}${model.aliases?.length ? ` (${model.aliases.map(esc).join(', ')})` : ''}</option>`).join('')}</select>`;
    $('#model-select').addEventListener('change', (event) => {
      state.model = event.target.value;
      state.statsData = null;
      $('#stats-total').textContent = '—';
      $('#statistics-rows').innerHTML = emptyRow(6, 'Loading statistics');
      renderRouting();
    });
  } else {
    const model = activeModel();
    node.innerHTML = `<div class="label">Model</div><div class="model-static">${model ? esc(model.name) + aliases(model) : 'No served model configured'}</div>`;
  }
}

function renderControls() {
  $('#window-control').innerHTML = windows.map(([id, label]) => `<button type="button" data-window="${id}" aria-pressed="${id === state.window}">${esc(label)}</button>`).join('');
  $('#metric-control').innerHTML = metrics.map(([id, label]) => `<button type="button" data-metric="${id}" aria-pressed="${id === state.metric}">${esc(label)}</button>`).join('');
  $('#window-control').querySelectorAll('button').forEach((button) => button.addEventListener('click', () => {
    state.window = button.dataset.window;
    renderControls();
    loadStatistics().catch(showError);
  }));
  $('#metric-control').querySelectorAll('button').forEach((button) => button.addEventListener('click', () => {
    state.metric = button.dataset.metric;
    renderControls();
    if (state.statsData) renderStatistics(state.statsData);
  }));
}

function statValue(totals) {
  return num(totals?.[state.metric]);
}

function renderStatistics(data) {
  $('#metric-heading').textContent = metricLabel();
  $('#stats-total').textContent = statValue(data.totals);
  $('#stats-period').textContent = windows.find(([id]) => id === state.window)?.[1] || data.window?.id || 'Statistics';
  const rows = [];
  for (const layer of data.layers || []) {
    rows.push(`<tr class="ledger-layer"><td class="ledger-kind">Layer total</td><th scope="row">${num(layer.index + 1)} · ${esc(layer.name)}</th><td>—</td><td>—</td><td>—</td><td class="numeric">${statValue(layer.totals)}</td></tr>`);
    for (const target of layer.targets || []) {
      rows.push(`<tr class="ledger-target"><th scope="row">Target</th><td>${esc(layer.name)}</td><td>${esc(target.provider)}</td><td>${esc(target.credential)}</td><td>${esc(target.upstream_model)}</td><td class="numeric">${statValue(target.totals)}</td></tr>`);
    }
  }
  for (const target of data.historical_targets || []) {
    rows.push(`<tr class="ledger-target ledger-historical"><th scope="row">Historical target</th><td>${esc(target.layer_name || `Layer ${Number(target.layer_index || 0) + 1}`)}</td><td>${esc(target.provider)}</td><td>${esc(target.credential)}</td><td>${esc(target.upstream_model)}</td><td class="numeric">${statValue(target.totals)}</td></tr>`);
  }
  const unattributed = Number(data.unattributed?.[state.metric] || 0);
  if (unattributed) rows.push(`<tr class="ledger-historical"><th scope="row">Unattributed</th><td>—</td><td>—</td><td>—</td><td>—</td><td class="numeric">${num(unattributed)}</td></tr>`);
  $('#statistics-rows').innerHTML = rows.join('') || emptyRow(6, 'No routing layers configured');
}

async function loadOverview() {
  const [status, stats, recent] = await Promise.all([
    api('/api/status'),
    api('/api/stats'),
    api('/api/requests?limit=50'),
  ]);
  renderOverview(stats, status, recent);
}

async function loadRouting() {
  const payload = await api('/api/routing');
  state.models = payload.models || [];
  if (!state.models.some((model) => model.name === state.model)) state.model = state.models[0]?.name || '';
  renderModelControl();
  renderRouting();
}

async function loadStatistics() {
  if (!state.model) return;
  const data = await api(`/api/routing/stats?model=${encodeURIComponent(state.model)}&window=${encodeURIComponent(state.window)}`);
  state.statsData = data;
  renderStatistics(data);
}

function renderRouting() {
  const model = activeModel();
  $('#routing-title').innerHTML = model ? esc(model.name) + aliases(model) : 'Routing';
  renderStructure(model);
  if (state.routeView === 'statistics') loadStatistics().catch(showError);
}

async function refresh() {
  clearError();
  try {
    if (state.view === 'overview') await loadOverview();
    else await loadRouting();
  } catch (error) {
    showError(error);
  }
}

function selectView(view) {
  state.view = view;
  $('#overview-view').hidden = view !== 'overview';
  $('#routing-view').hidden = view !== 'routing';
  document.querySelectorAll('[data-view]').forEach((button) => button.setAttribute('aria-selected', String(button.dataset.view === view)));
  refresh();
}

function selectRouteView(view) {
  state.routeView = view;
  $('#structure-panel').hidden = view !== 'structure';
  $('#statistics-panel').hidden = view !== 'statistics';
  document.querySelectorAll('[data-route-view]').forEach((button) => button.setAttribute('aria-selected', String(button.dataset.routeView === view)));
  renderRouting();
}

document.querySelectorAll('[data-view]').forEach((button) => button.addEventListener('click', () => selectView(button.dataset.view)));
document.querySelectorAll('[data-route-view]').forEach((button) => button.addEventListener('click', () => selectRouteView(button.dataset.routeView)));
renderControls();
refresh();
setInterval(refresh, 5000);
