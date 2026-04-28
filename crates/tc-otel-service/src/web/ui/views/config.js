// config.js — domain-centric configuration view
//
// Layout:
//   1. PLC Domains  — one card per AMS Net ID, combining diagnostics + metrics
//   2. Global Settings — schema-driven collapsed panels for all other sections

import { request, toast as showToast, escapeHtml } from '../lib/util.js';
import { domainColor, domainInitials } from '../lib/domains.js';

const MASKED = '***MASKED***';
let schema = null;
let currentConfig = null;
let initialized = false;

// ── Public API ─────────────────────────────────────────────────────────────

export async function ensureLoaded() {
  if (schema) return;
  await loadAndRender();
}

export function init() {
  if (initialized) return;
  initialized = true;
  const saveBtn = document.getElementById('config-save-btn');
  if (saveBtn) saveBtn.addEventListener('click', save);
  const root = document.getElementById('config-form-root');
  if (root) bindRoot(root);
}

// ── Load & render ──────────────────────────────────────────────────────────

async function loadAndRender() {
  const root = document.getElementById('config-form-root');
  if (!root) return;
  try {
    const [cfg, sch] = await Promise.all([
      request('/api/config'),
      request('/api/config/schema'),
    ]);
    schema = sch;
    currentConfig = cfg.config ?? {};
    root.innerHTML = renderAll(currentConfig);
    if (cfg.restart_pending) showToast('Restart pending — changes awaiting process restart.', 'warn');
  } catch (e) {
    showToast('Failed to load config: ' + e.message, 'err');
  }
}

// ── Schema helpers ─────────────────────────────────────────────────────────

function resolveRef(ref) {
  if (!ref?.startsWith('#/')) return null;
  const parts = ref.slice(2).split('/');
  let c = schema;
  for (const p of parts) { if (!c || typeof c !== 'object') return null; c = c[p]; }
  return c;
}

function resolve(s) {
  if (!s || typeof s !== 'object') return s;
  if (s.$ref) {
    const r = resolveRef(s.$ref);
    return r ? resolve(Object.assign({}, r, Object.fromEntries(Object.entries(s).filter(([k]) => k !== '$ref')))) : s;
  }
  if (Array.isArray(s.allOf) && s.allOf.length > 0) {
    let merged = Object.fromEntries(Object.entries(s).filter(([k]) => k !== 'allOf'));
    for (const part of s.allOf) {
      const r = resolve(part);
      if (r && typeof r === 'object') merged = Object.assign({}, r, merged);
    }
    return merged;
  }
  return s;
}

function titleOf(s, key) {
  return s.title || (key ? key.replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase()) : '');
}

// ── Schema-driven field renderer (global settings panels) ──────────────────

function renderField(s, value, path, key) {
  s = resolve(s);
  const desc = s.description ? `<div class="hint">${escapeHtml(s.description)}</div>` : '';
  const lbl = key != null ? `<label>${escapeHtml(titleOf(s, key))}</label>` : '';

  if (s.enum) {
    const opts = s.enum.map(e => `<option value="${escapeHtml(e)}"${e === value ? ' selected' : ''}>${escapeHtml(e)}</option>`).join('');
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<select data-kind="enum">${opts}</select></div>`;
  }
  if (s.type === 'boolean') {
    return `<div class="cfg-field" data-path="${path}">${desc}<label><input type="checkbox" data-kind="bool"${value ? ' checked' : ''}> ${escapeHtml(titleOf(s, key))}</label></div>`;
  }
  if (s.type === 'integer' || s.type === 'number') {
    const min = s.minimum != null ? ` min="${s.minimum}"` : '';
    const max = s.maximum != null ? ` max="${s.maximum}"` : '';
    const step = s.type === 'integer' ? ' step="1"' : '';
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="number" data-kind="num"${min}${max}${step} value="${escapeHtml(String(value ?? ''))}"></div>`;
  }
  if (s.type === 'string' && s.format === 'password') {
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="password" data-kind="pw" placeholder="${value === MASKED ? 'unchanged' : ''}" data-orig="${value === MASKED ? '1' : '0'}"></div>`;
  }
  if (s.type === 'string') {
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="str" value="${escapeHtml(String(value ?? ''))}"></div>`;
  }
  if (s.type === 'array') return renderSchemaArray(s, value || [], path, key);
  if (s.type === 'object' || s.properties) return renderSchemaObject(s, value || {}, path, key);
  if (s.oneOf || s.anyOf) return renderUnion(s, value, path, key);
  return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="json" value="${escapeHtml(JSON.stringify(value ?? null))}"></div>`;
}

function renderSchemaObject(s, value, path, key) {
  const props = s.properties || {};
  let body = '';
  for (const [pk, ps] of Object.entries(props)) {
    body += renderField(ps, value?.[pk], path ? `${path}.${pk}` : pk, pk);
  }
  if (!path) return body;
  if (key == null) return `<div data-obj="${path}">${body}</div>`;
  return `<div class="cfg-field" data-path="${path}" data-obj="1"><details class="cfg-section" open><summary>${escapeHtml(titleOf(s, key))}</summary><div class="cfg-body">${body}</div></details></div>`;
}

function renderSchemaArray(s, items, path, key) {
  const itemSchema = resolve(s.items || {});
  const id = 'arr-' + path.replace(/[^a-z0-9]/gi, '_');
  const inner = items.map((it, i) =>
    `<div class="cfg-array-item" data-idx="${i}"><button type="button" class="cfg-rm">×</button>${renderField(itemSchema, it, `${path}[${i}]`, null)}</div>`
  ).join('');
  return `<div class="cfg-field" data-path="${path}" data-arr="1"><label>${escapeHtml(titleOf(s, key))}</label><div class="cfg-array" id="${id}" data-item-schema='${escapeHtml(JSON.stringify(itemSchema))}'>${inner}</div><button type="button" class="cfg-add" data-target="${id}" data-path="${path}">+ Add</button></div>`;
}

function renderUnion(s, value, path, key) {
  const variants = (s.oneOf || s.anyOf).map(resolve);
  const allStringLit = variants.every(v => v?.type === 'string' && Array.isArray(v.enum) && v.enum.length === 1 && !v.properties);
  if (allStringLit) {
    const lits = variants.map(v => v.enum[0]);
    const opts = lits.map(lit => `<option value="${escapeHtml(lit)}"${lit === value ? ' selected' : ''}>${escapeHtml(lit)}</option>`).join('');
    const desc = s.description ? `<div class="hint">${escapeHtml(s.description)}</div>` : '';
    return `<div class="cfg-field" data-path="${path}">${key != null ? `<label>${escapeHtml(titleOf(s, key))}</label>` : ''}${desc}<select data-kind="enum">${opts}</select></div>`;
  }
  let active = 0;
  if (value && typeof value === 'object' && value.type) {
    variants.forEach((v, i) => {
      const t = v.properties?.type;
      if (t && (t.const === value.type || (t.enum && t.enum.includes(value.type)))) active = i;
    });
  }
  const tabs = variants.map((v, i) => {
    const t = v.properties?.type;
    const name = (t && (t.const || t.enum?.[0])) || v.title || `Variant ${i + 1}`;
    return `<button type="button" class="cfg-union-tab${i === active ? ' active' : ''}" data-tab="${i}">${escapeHtml(name)}</button>`;
  }).join('');
  const panels = variants.map((v, i) =>
    `<div class="cfg-union-panel${i === active ? ' active' : ''}" data-panel="${i}">${renderSchemaObject(v, i === active ? (value || {}) : {}, path, null)}</div>`
  ).join('');
  return `<div class="cfg-field" data-path="${path}" data-union="1"><label>${escapeHtml(titleOf(s, key))}</label><div class="cfg-union-tabs">${tabs}</div>${panels}</div>`;
}

// ── Schema-driven data collector (global settings) ─────────────────────────

function collect(el) {
  const out = {};
  el.querySelectorAll('[data-path]').forEach(f => {
    if (f.dataset.obj || f.dataset.arr || f.dataset.union) return;
    let p = f.parentElement;
    while (p && p !== el) {
      if (p.classList?.contains('cfg-union-panel') && !p.classList.contains('active')) return;
      p = p.parentElement;
    }
    const input = f.querySelector('input,select');
    if (!input) return;
    let val;
    const k = input.dataset.kind;
    if (k === 'bool') val = input.checked;
    else if (k === 'num') val = input.value === '' ? null : Number(input.value);
    else if (k === 'pw') val = input.value === '' ? (input.dataset.orig === '1' ? MASKED : null) : input.value;
    else if (k === 'json') { try { val = JSON.parse(input.value); } catch { val = input.value; } }
    else val = input.value === '' ? null : input.value;
    setPath(out, f.dataset.path, val);
  });
  el.querySelectorAll('[data-arr="1"]').forEach(a => {
    const fp = a.dataset.path;
    const items = [];
    a.querySelectorAll(':scope > .cfg-array > .cfg-array-item').forEach((it, i) => {
      const sub = collect(it);
      items.push(getPath(sub, `${fp}[${i}]`) ?? sub);
    });
    setPath(out, fp, items);
  });
  return out;
}

function setPath(obj, path, val) {
  const tokens = tokenize(path);
  let c = obj;
  for (let i = 0; i < tokens.length - 1; i++) {
    const t = tokens[i], nxt = tokens[i + 1];
    if (c[t] == null) c[t] = typeof nxt === 'number' ? [] : {};
    c = c[t];
  }
  c[tokens[tokens.length - 1]] = val;
}

function getPath(obj, path) {
  const tokens = tokenize(path);
  let c = obj;
  for (const t of tokens) { if (c == null) return undefined; c = c[t]; }
  return c;
}

function tokenize(path) {
  const out = [];
  path.replace(/([^.\[\]]+)|\[(\d+)\]/g, (_, name, idx) => {
    if (name != null) out.push(name); else out.push(Number(idx));
  });
  return out;
}

// ── Domain model ──────────────────────────────────────────────────────────

function buildDomains(cfg) {
  const map = new Map();
  for (const t of cfg.diagnostics?.targets ?? []) {
    if (!map.has(t.ams_net_id)) {
      map.set(t.ams_net_id, { ams_net_id: t.ams_net_id, router_host: null, diag: null, metrics: [] });
    }
    map.get(t.ams_net_id).diag = { ...t };
  }
  for (const m of cfg.metrics?.custom_metrics ?? []) {
    const id = m.ams_net_id ?? '';
    if (!map.has(id)) {
      map.set(id, { ams_net_id: id, router_host: m.ams_router_host ?? null, diag: null, metrics: [] });
    }
    const d = map.get(id);
    if (!d.router_host && m.ams_router_host) d.router_host = m.ams_router_host;
    d.metrics.push({ ...m });
  }
  return [...map.values()];
}

// ── Top-level render ───────────────────────────────────────────────────────

function renderAll(cfg) {
  const domains = buildDomains(cfg);
  return `
    <section class="cfg-domains-section">
      <div class="cfg-section-hd">
        <h3>PLC Domains</h3>
        <button type="button" class="btn cfg-add-domain">+ Add Domain</button>
      </div>
      <div id="cfg-domain-cards">${
        domains.length
          ? domains.map(renderDomainCard).join('')
          : emptyDomainsMsg()
      }</div>
    </section>
    <section class="cfg-global-section">
      <div class="cfg-global-hd"><h3>Global Settings</h3></div>
      ${renderGlobalSettings(cfg)}
    </section>`;
}

function emptyDomainsMsg() {
  return `<div class="cfg-empty-hint">No domains configured yet. Click <strong>+ Add Domain</strong> to start collecting metrics or diagnostics from a PLC.</div>`;
}

// ── Domain card ────────────────────────────────────────────────────────────

function renderDomainCard(d) {
  const color = domainColor(d.ams_net_id);
  const initials = domainInitials({ ams_net_id: d.ams_net_id, friendly_name: d.ams_net_id });
  return `<div class="domain-cfg-card">
    <div class="domain-cfg-head">
      <div class="domain-cfg-id-row">
        <div class="domain-badge" style="background:${color};flex-shrink:0">${escapeHtml(initials)}</div>
        <div class="cfg-labeled" style="flex:1">
          <label>AMS Net ID</label>
          <input class="input input-mono" data-ams-net-id-input value="${escapeHtml(d.ams_net_id)}" placeholder="e.g. 172.28.41.37.1.1">
        </div>
      </div>
      <button type="button" class="btn btn-danger btn-sm cfg-remove-domain">Remove</button>
    </div>
    <div class="domain-cfg-body">
      ${renderDiagSection(d)}
      ${renderMetricsSection(d)}
    </div>
  </div>`;
}

// ── Diagnostics subsection ─────────────────────────────────────────────────

function renderDiagSection(d) {
  const on = d.diag !== null;
  const t = d.diag ?? {};
  const ports = t.task_ports ?? [];
  return `<div class="cfg-subsection">
    <div class="cfg-subsec-hd">
      <label class="cfg-toggle-label">
        <input type="checkbox" data-diag-enabled ${on ? 'checked' : ''}>
        <span class="cfg-subsec-title">Diagnostics</span>
      </label>
      <span class="muted small">Cycle time · RT usage · exceed counter</span>
    </div>
    <div class="cfg-subsec-body"${on ? '' : ' hidden'}>
      <div class="cfg-inline-row">
        <div class="cfg-labeled">
          <label>Poll interval</label>
          <div class="input-affixed"><input type="number" class="input input-sm" data-diag-poll-ms min="100" step="100" value="${t.poll_interval_ms ?? 1000}"><span class="input-affix">ms</span></div>
        </div>
        <div class="cfg-labeled">
          <label>AMS RT port</label>
          <input type="number" class="input input-sm" data-diag-rt-port min="1" max="65535" value="${t.rt_port ?? 200}">
        </div>
        <label class="cfg-check"><input type="checkbox" data-diag-exceed ${(t.exceed_counter ?? true) ? 'checked' : ''}> Exceed counter</label>
        <label class="cfg-check"><input type="checkbox" data-diag-rt-usage ${(t.rt_usage ?? true) ? 'checked' : ''}> RT usage</label>
      </div>
      <div class="cfg-labeled">
        <label>Task ports <span class="muted small">(AMS task ports to poll)</span></label>
        <div class="cfg-chips" data-diag-task-ports>
          ${ports.map(p => portChip(p)).join('')}
          <button type="button" class="btn btn-ghost btn-xs cfg-add-task-port">+ Port</button>
        </div>
      </div>
    </div>
  </div>`;
}

function portChip(p) {
  return `<span class="cfg-chip"><input type="number" class="input input-xs" data-diag-task-port value="${p}" min="1" max="65535"><button type="button" class="cfg-chip-rm" title="Remove">×</button></span>`;
}

// ── Metrics subsection ─────────────────────────────────────────────────────

function renderMetricsSection(d) {
  return `<div class="cfg-subsection cfg-subsec-last">
    <div class="cfg-subsec-hd">
      <div style="display:flex;align-items:center;gap:.5rem">
        <span class="cfg-subsec-title">Metrics</span>
        ${d.metrics.length ? `<span class="cfg-count-badge">${d.metrics.length}</span>` : ''}
      </div>
      <div class="cfg-labeled">
        <label>Router host</label>
        <input type="text" class="input input-sm" data-metrics-router placeholder="hostname or IP" value="${escapeHtml(d.router_host ?? '')}">
      </div>
    </div>
    <div class="cfg-metric-list" data-metric-list>
      ${d.metrics.map(renderMetricRow).join('')}
    </div>
    <button type="button" class="btn btn-ghost btn-xs cfg-add-metric" style="margin-top:.35rem">+ Add Metric</button>
  </div>`;
}

// ── Metric row ─────────────────────────────────────────────────────────────

function renderMetricRow(m) {
  const src = m.source ?? 'push';
  const kind = m.kind ?? 'gauge';
  const srcLabel = { push: 'push', poll: 'poll', notification: 'notify' }[src] ?? src;
  return `<div class="cfg-metric-row">
    <div class="cfg-metric-sum">
      <code class="small" style="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis">${escapeHtml(m.symbol ?? '—')}</code>
      <span class="muted small">→</span>
      <code class="small" style="flex:1.5;min-width:0;overflow:hidden;text-overflow:ellipsis">${escapeHtml(m.metric_name ?? '—')}</code>
      <span class="cfg-chip-row">
        <span class="chip chip-sm">${escapeHtml(kind)}</span>
        <span class="chip chip-sm chip-src-${escapeHtml(src)}">${escapeHtml(srcLabel)}</span>
        ${m.unit ? `<span class="muted small">${escapeHtml(m.unit)}</span>` : ''}
      </span>
      <button type="button" class="btn btn-ghost btn-xs cfg-metric-toggle" title="Edit">▾</button>
      <button type="button" class="btn btn-danger btn-xs cfg-remove-metric" title="Remove">×</button>
    </div>
    <div class="cfg-metric-detail" hidden>
      ${renderMetricDetail(m)}
    </div>
  </div>`;
}

function renderMetricDetail(m) {
  const src = m.source ?? 'push';
  const kind = m.kind ?? 'gauge';
  const poll = m.poll ?? {};
  const notif = m.notification ?? {};
  return `<div class="cfg-mf">
    <div class="cfg-inline-row">
      <div class="cfg-labeled" style="flex:1">
        <label>PLC symbol</label>
        <input type="text" class="input" data-m-symbol placeholder="GVL.myVar" value="${escapeHtml(m.symbol ?? '')}">
      </div>
      <div class="cfg-labeled" style="flex:1">
        <label>Metric name</label>
        <input type="text" class="input" data-m-name placeholder="plc.my.metric" value="${escapeHtml(m.metric_name ?? '')}">
      </div>
    </div>
    <div class="cfg-inline-row">
      <div class="cfg-labeled" style="flex:2">
        <label>Description <span class="muted small">(optional)</span></label>
        <input type="text" class="input" data-m-desc value="${escapeHtml(m.description ?? '')}">
      </div>
      <div class="cfg-labeled">
        <label>Unit <span class="muted small">e.g. Cel, rpm</span></label>
        <input type="text" class="input input-sm" data-m-unit value="${escapeHtml(m.unit ?? '')}">
      </div>
    </div>
    <div class="cfg-inline-row">
      <div class="cfg-labeled">
        <label>Kind</label>
        <select class="input input-sm" data-m-kind>
          ${['gauge', 'sum', 'histogram'].map(k => `<option value="${k}"${k === kind ? ' selected' : ''}>${k}</option>`).join('')}
        </select>
      </div>
      <div class="cfg-labeled">
        <label>Source</label>
        <select class="input input-sm" data-m-source>
          <option value="push"${src === 'push' ? ' selected' : ''}>push (PLC-mapped)</option>
          <option value="poll"${src === 'poll' ? ' selected' : ''}>poll (ADS read)</option>
          <option value="notification"${src === 'notification' ? ' selected' : ''}>notification (ADS subscribe)</option>
        </select>
      </div>
      <div class="cfg-labeled">
        <label>AMS port <span class="muted small">default 851</span></label>
        <input type="number" class="input input-sm" data-m-ams-port min="1" max="65535" value="${m.ams_port != null ? m.ams_port : ''}">
      </div>
    </div>
    <div data-src-detail>${renderSourceDetail(src, poll, notif)}</div>
    <label class="cfg-check" data-monotonic-row${kind !== 'sum' ? ' hidden' : ''}>
      <input type="checkbox" data-m-monotonic ${m.is_monotonic ? 'checked' : ''}> Monotonic (counter, not up-down)
    </label>
  </div>`;
}

function renderSourceDetail(src, poll, notif) {
  if (src === 'poll') {
    return `<div class="cfg-inline-row">
      <div class="cfg-labeled">
        <label>Poll interval</label>
        <div class="input-affixed"><input type="number" class="input input-sm" data-m-poll-ms min="100" step="100" value="${poll.interval_ms ?? 1000}"><span class="input-affix">ms</span></div>
      </div>
    </div>`;
  }
  if (src === 'notification') {
    return `<div class="cfg-inline-row">
      <div class="cfg-labeled">
        <label>Transmission</label>
        <select class="input input-sm" data-m-notif-mode>
          <option value="on_change"${(notif.transmission_mode ?? 'on_change') === 'on_change' ? ' selected' : ''}>on change</option>
          <option value="cyclic"${notif.transmission_mode === 'cyclic' ? ' selected' : ''}>cyclic</option>
        </select>
      </div>
      <div class="cfg-labeled">
        <label>Max delay</label>
        <div class="input-affixed"><input type="number" class="input input-sm" data-m-notif-maxdelay min="0" value="${notif.max_delay_ms ?? 5000}"><span class="input-affix">ms</span></div>
      </div>
      <div class="cfg-labeled">
        <label>Min period</label>
        <div class="input-affixed"><input type="number" class="input input-sm" data-m-notif-minperiod min="0" value="${notif.min_period_ms ?? 0}"><span class="input-affix">ms</span></div>
      </div>
    </div>`;
  }
  return ''; // push — no extra config
}

// ── Global settings panels ─────────────────────────────────────────────────

const GLOBAL_SECTIONS = [
  { key: 'receiver',    label: 'Receiver',         hint: 'Log ingestion — gRPC, HTTP, ADS ports', skip: [] },
  { key: 'export',      label: 'Log Export',        hint: 'Batch export to OTLP / Loki',           skip: [] },
  { key: 'metrics',     label: 'Metrics Export',    hint: 'OTLP metrics export and cycle-time',    skip: ['custom_metrics'] },
  { key: 'diagnostics', label: 'Diagnostics',       hint: 'Global diagnostics on/off toggle',      skip: ['targets'] },
  { key: 'traces',      label: 'Traces',            hint: 'Distributed trace collection',          skip: [] },
  { key: 'service',     label: 'Service',           hint: 'Process name, threads, capacity',       skip: [] },
  { key: 'logging',     label: 'Logging',           hint: 'Log format and level',                  skip: [] },
  { key: 'web',         label: 'Web UI',            hint: 'Built-in HTTP dashboard',               skip: [] },
];

function renderGlobalSettings(cfg) {
  return GLOBAL_SECTIONS.map(({ key, label, hint, skip }) => {
    const sSchema = resolve(schema?.properties?.[key]);
    if (!sSchema?.properties) return '';
    const props = Object.entries(sSchema.properties).filter(([k]) => !skip.includes(k));
    if (!props.length) return '';
    const value = cfg[key] ?? {};
    const body = props.map(([pk, ps]) => renderField(ps, value[pk], `${key}.${pk}`, pk)).join('');
    return `<details class="cfg-global-panel">
      <summary><strong>${escapeHtml(label)}</strong><span class="muted small">${escapeHtml(hint)}</span></summary>
      <div class="cfg-global-body">${body}</div>
    </details>`;
  }).join('');
}

// ── Event binding ──────────────────────────────────────────────────────────

function bindRoot(root) {
  root.addEventListener('click', ev => {
    // Diagnostics toggle
    if (ev.target.matches('[data-diag-enabled]')) {
      const body = ev.target.closest('.cfg-subsection')?.querySelector('.cfg-subsec-body');
      if (body) body.hidden = !ev.target.checked;
      return;
    }
    // Remove domain card
    if (ev.target.closest('.cfg-remove-domain')) {
      ev.target.closest('.domain-cfg-card')?.remove();
      syncEmptyDomains();
      return;
    }
    // Add domain card
    if (ev.target.closest('.cfg-add-domain')) {
      addDomain();
      return;
    }
    // Add metric row
    if (ev.target.closest('.cfg-add-metric')) {
      const list = ev.target.closest('.cfg-subsection')?.querySelector('[data-metric-list]');
      if (list) {
        const div = document.createElement('div');
        div.innerHTML = renderMetricRow({ source: 'push', kind: 'gauge' });
        const row = div.firstElementChild;
        row.querySelector('.cfg-metric-detail').hidden = false;
        row.querySelector('.cfg-metric-toggle').textContent = '▴';
        list.appendChild(row);
      }
      return;
    }
    // Remove metric row
    if (ev.target.closest('.cfg-remove-metric')) {
      ev.target.closest('.cfg-metric-row')?.remove();
      return;
    }
    // Expand/collapse metric detail
    if (ev.target.closest('.cfg-metric-toggle')) {
      const row = ev.target.closest('.cfg-metric-row');
      const detail = row?.querySelector('.cfg-metric-detail');
      if (detail) {
        detail.hidden = !detail.hidden;
        ev.target.closest('.cfg-metric-toggle').textContent = detail.hidden ? '▾' : '▴';
      }
      return;
    }
    // Add task port chip
    if (ev.target.closest('.cfg-add-task-port')) {
      const chips = ev.target.closest('[data-diag-task-ports]');
      if (chips) {
        const span = document.createElement('span');
        span.innerHTML = portChip('');
        chips.insertBefore(span.firstElementChild, ev.target.closest('.cfg-add-task-port'));
      }
      return;
    }
    // Remove task port chip
    if (ev.target.closest('.cfg-chip-rm')) {
      ev.target.closest('.cfg-chip')?.remove();
      return;
    }
    // Schema-driven array remove
    const rm = ev.target.closest('.cfg-rm');
    if (rm) { rm.closest('.cfg-array-item')?.remove(); return; }
    // Schema-driven array add
    const add = ev.target.closest('.cfg-add');
    if (add) {
      const arr = document.getElementById(add.dataset.target);
      const itemSchema = JSON.parse(
        arr.dataset.itemSchema
          .replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>')
          .replace(/&quot;/g, '"').replace(/&#39;/g, "'")
      );
      const i = arr.children.length;
      const wrap = document.createElement('div');
      wrap.className = 'cfg-array-item';
      wrap.dataset.idx = i;
      wrap.innerHTML = `<button type="button" class="cfg-rm">×</button>` + renderField(itemSchema, null, `${add.dataset.path}[${i}]`, null);
      arr.appendChild(wrap);
      return;
    }
    // Schema-driven union tab
    const tab = ev.target.closest('.cfg-union-tab');
    if (tab) {
      const union = tab.closest('[data-union="1"]');
      union.querySelectorAll('.cfg-union-tab').forEach(t => t.classList.remove('active'));
      union.querySelectorAll('.cfg-union-panel').forEach(p => p.classList.remove('active'));
      tab.classList.add('active');
      union.querySelector(`.cfg-union-panel[data-panel="${tab.dataset.tab}"]`).classList.add('active');
    }
  });

  root.addEventListener('change', ev => {
    // Source select → refresh source-specific fields
    if (ev.target.matches('[data-m-source]')) {
      const detail = ev.target.closest('.cfg-mf')?.querySelector('[data-src-detail]');
      if (detail) detail.innerHTML = renderSourceDetail(ev.target.value, {}, {});
      return;
    }
    // Kind select → show/hide monotonic checkbox
    if (ev.target.matches('[data-m-kind]')) {
      const row = ev.target.closest('.cfg-mf')?.querySelector('[data-monotonic-row]');
      if (row) row.hidden = ev.target.value !== 'sum';
    }
  });
}

function syncEmptyDomains() {
  const cards = document.getElementById('cfg-domain-cards');
  if (cards && !cards.querySelector('.domain-cfg-card')) {
    cards.innerHTML = emptyDomainsMsg();
  }
}

function addDomain() {
  const cards = document.getElementById('cfg-domain-cards');
  if (!cards) return;
  cards.querySelector('.cfg-empty-hint')?.remove();
  const div = document.createElement('div');
  div.innerHTML = renderDomainCard({ ams_net_id: '', router_host: '', diag: null, metrics: [] });
  cards.appendChild(div.firstElementChild);
  cards.lastElementChild?.querySelector('[data-ams-net-id-input]')?.focus();
}

// ── Data collection on save ────────────────────────────────────────────────

function collectDomains() {
  const diagTargets = [];
  const customMetrics = [];

  document.querySelectorAll('.domain-cfg-card').forEach(card => {
    const amsNetId = card.querySelector('[data-ams-net-id-input]')?.value?.trim() ?? '';
    if (!amsNetId) return;

    if (card.querySelector('[data-diag-enabled]')?.checked) {
      const ports = [...card.querySelectorAll('[data-diag-task-port]')]
        .map(i => Number(i.value)).filter(v => v > 0);
      // Preserve task_names from the original config for this target (not editable in UI)
      const existing = (currentConfig?.diagnostics?.targets ?? []).find(t => t.ams_net_id === amsNetId);
      diagTargets.push({
        ams_net_id: amsNetId,
        poll_interval_ms: Number(card.querySelector('[data-diag-poll-ms]')?.value) || 1000,
        rt_port: Number(card.querySelector('[data-diag-rt-port]')?.value) || 200,
        exceed_counter: card.querySelector('[data-diag-exceed]')?.checked ?? true,
        rt_usage: card.querySelector('[data-diag-rt-usage]')?.checked ?? true,
        task_ports: ports,
        task_names: existing?.task_names ?? {},
      });
    }

    const routerHost = card.querySelector('[data-metrics-router]')?.value?.trim() || null;
    card.querySelectorAll('.cfg-metric-row').forEach(mRow => {
      const src = mRow.querySelector('[data-m-source]')?.value ?? 'push';
      const kind = mRow.querySelector('[data-m-kind]')?.value ?? 'gauge';
      const amsPort = mRow.querySelector('[data-m-ams-port]')?.value;
      customMetrics.push(stripNulls({
        ams_net_id: amsNetId,
        ams_router_host: routerHost,
        ams_port: amsPort ? Number(amsPort) : null,
        symbol: mRow.querySelector('[data-m-symbol]')?.value?.trim() ?? '',
        metric_name: mRow.querySelector('[data-m-name]')?.value?.trim() ?? '',
        description: mRow.querySelector('[data-m-desc]')?.value ?? '',
        kind,
        source: src,
        unit: mRow.querySelector('[data-m-unit]')?.value ?? '',
        is_monotonic: mRow.querySelector('[data-m-monotonic]')?.checked ?? false,
        poll: src === 'poll' ? {
          interval_ms: Number(mRow.querySelector('[data-m-poll-ms]')?.value ?? 1000),
        } : null,
        notification: src === 'notification' ? {
          max_delay_ms: Number(mRow.querySelector('[data-m-notif-maxdelay]')?.value ?? 5000),
          min_period_ms: Number(mRow.querySelector('[data-m-notif-minperiod]')?.value ?? 0),
          max_period_ms: 10000,
          transmission_mode: mRow.querySelector('[data-m-notif-mode]')?.value ?? 'on_change',
        } : null,
      }));
    });
  });

  return { diagTargets, customMetrics };
}

function stripNulls(obj) {
  if (Array.isArray(obj)) return obj.map(stripNulls);
  if (obj && typeof obj === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(obj)) {
      if (v == null) continue;
      out[k] = stripNulls(v);
    }
    return out;
  }
  return obj;
}

// ── Save ───────────────────────────────────────────────────────────────────

async function save() {
  if (!schema) { showToast('Schema not yet loaded.', 'err'); return; }
  const saveBtn = document.getElementById('config-save-btn');
  if (saveBtn) saveBtn.disabled = true;
  try {
    const root = document.getElementById('config-form-root');
    const globalData = root ? collect(root.querySelector('.cfg-global-section') ?? root) : {};
    const { diagTargets, customMetrics } = collectDomains();

    const payload = {
      ...currentConfig,
      ...globalData,
      diagnostics: {
        ...(globalData.diagnostics ?? currentConfig?.diagnostics ?? {}),
        enabled: diagTargets.length > 0,
        targets: diagTargets,
      },
      metrics: {
        ...(globalData.metrics ?? currentConfig?.metrics ?? {}),
        custom_metrics: customMetrics,
      },
      outputs: currentConfig?.outputs ?? [],
    };

    const r = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const res = await r.json();
    if (r.ok) {
      currentConfig = payload;
      const hot = (res.hot_reloaded || []).join(', ') || '–';
      const rr = (res.restart_required || []).join(', ');
      showToast(`✓ Saved. Hot-reloaded: ${hot}.` + (rr ? ` Restart required: ${rr}.` : ''), rr ? 'warn' : 'ok');
    } else if (res.errors) {
      showToast('Validation: ' + res.errors.join('; '), 'err');
    } else {
      showToast('Error: ' + (res.detail || res.error || r.statusText), 'err');
    }
  } catch (e) {
    showToast('Save failed: ' + e.message, 'err');
  } finally {
    if (saveBtn) saveBtn.disabled = false;
  }
}
