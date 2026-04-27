// Configuration view — schema-driven form generated from /api/config/schema.
// Migrated from the original DASHBOARD_HTML inline IIFE; structure preserved
// so existing config payloads round-trip identically.

import { request, toast as showToast, escapeHtml } from '../lib/util.js';

const MASKED = '***MASKED***';
let rootSchema = null;
let currentData = null;
let initialized = false;

export async function ensureLoaded() {
  if (rootSchema) return;
  await loadAndRender();
}

export function init() {
  if (initialized) return;
  initialized = true;
  const root = document.getElementById('config-form-root');
  const saveBtn = document.getElementById('config-save-btn');
  if (saveBtn) saveBtn.addEventListener('click', save);
  if (root) bindEvents(root);
}

function resolveRef(ref) {
  if (!ref || !ref.startsWith('#/')) return null;
  const parts = ref.slice(2).split('/');
  let c = rootSchema;
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

function titleOf(schema, key) {
  return schema.title || (key ? key.replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase()) : '');
}

function renderField(schema, value, path, key) {
  schema = resolve(schema);
  const desc = schema.description ? `<div class="hint">${escapeHtml(schema.description)}</div>` : '';
  const lbl = key != null ? `<label>${escapeHtml(titleOf(schema, key))}</label>` : '';

  if (schema.enum) {
    const opts = schema.enum.map(e => `<option value="${escapeHtml(e)}"${e === value ? ' selected' : ''}>${escapeHtml(e)}</option>`).join('');
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<select data-kind="enum">${opts}</select></div>`;
  }
  if (schema.type === 'boolean') {
    return `<div class="cfg-field" data-path="${path}">${desc}<label><input type="checkbox" data-kind="bool"${value ? ' checked' : ''}> ${escapeHtml(titleOf(schema, key))}</label></div>`;
  }
  if (schema.type === 'integer' || schema.type === 'number') {
    const min = schema.minimum != null ? ` min="${schema.minimum}"` : '';
    const max = schema.maximum != null ? ` max="${schema.maximum}"` : '';
    const step = schema.type === 'integer' ? ' step="1"' : '';
    const v = value != null ? value : '';
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="number" data-kind="num"${min}${max}${step} value="${escapeHtml(String(v))}"></div>`;
  }
  if (schema.type === 'string' && schema.format === 'password') {
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="password" data-kind="pw" placeholder="${value === MASKED ? 'unchanged' : ''}" data-orig="${value === MASKED ? '1' : '0'}"></div>`;
  }
  if (schema.type === 'string') {
    const v = value != null ? value : '';
    return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="str" value="${escapeHtml(String(v))}"></div>`;
  }
  if (schema.type === 'array') {
    return renderArray(schema, value || [], path, key);
  }
  if (schema.type === 'object' || schema.properties) {
    return renderObject(schema, value || {}, path, key);
  }
  if (schema.oneOf || schema.anyOf) {
    return renderUnion(schema, value, path, key);
  }
  return `<div class="cfg-field" data-path="${path}">${lbl}${desc}<input type="text" data-kind="json" value="${escapeHtml(JSON.stringify(value || null))}"></div>`;
}

function renderObject(schema, value, path, key) {
  const props = schema.properties || {};
  let body = '';
  for (const [pk, ps] of Object.entries(props)) {
    body += renderField(ps, value ? value[pk] : undefined, path ? `${path}.${pk}` : pk, pk);
  }
  if (!path) return body;
  if (key == null) return `<div data-obj="${path}">${body}</div>`;
  return `<div class="cfg-field" data-path="${path}" data-obj="1"><details class="cfg-section" open><summary>${escapeHtml(titleOf(schema, key))}</summary><div class="cfg-body">${body}</div></details></div>`;
}

function renderArray(schema, items, path, key) {
  const itemSchema = resolve(schema.items || {});
  const id = 'arr-' + path.replace(/[^a-z0-9]/gi, '_');
  const inner = items.map((it, i) =>
    `<div class="cfg-array-item" data-idx="${i}"><button type="button" class="cfg-rm">×</button>${renderField(itemSchema, it, `${path}[${i}]`, null)}</div>`
  ).join('');
  return `<div class="cfg-field" data-path="${path}" data-arr="1"><label>${escapeHtml(titleOf(schema, key))}</label><div class="cfg-array" id="${id}" data-item-schema='${escapeHtml(JSON.stringify(itemSchema))}'>${inner}</div><button type="button" class="cfg-add" data-target="${id}" data-path="${path}">+ Add</button></div>`;
}

function renderUnion(schema, value, path, key) {
  const variants = (schema.oneOf || schema.anyOf).map(resolve);
  const allStringLit = variants.every(v => v && v.type === 'string' && Array.isArray(v.enum) && v.enum.length === 1 && !v.properties);
  if (allStringLit) {
    const lits = variants.map(v => v.enum[0]);
    const opts = lits.map(lit => `<option value="${escapeHtml(lit)}"${lit === value ? ' selected' : ''}>${escapeHtml(lit)}</option>`).join('');
    const desc = schema.description ? `<div class="hint">${escapeHtml(schema.description)}</div>` : '';
    return `<div class="cfg-field" data-path="${path}">${key != null ? `<label>${escapeHtml(titleOf(schema, key))}</label>` : ''}${desc}<select data-kind="enum">${opts}</select></div>`;
  }
  let active = 0;
  if (value && typeof value === 'object' && value.type) {
    variants.forEach((v, i) => {
      const t = v.properties && v.properties.type;
      if (t && (t.const === value.type || (t.enum && t.enum.includes(value.type)))) active = i;
    });
  }
  const tabs = variants.map((v, i) => {
    const t = v.properties && v.properties.type;
    const name = (t && (t.const || (t.enum && t.enum[0]))) || v.title || `Variant ${i + 1}`;
    return `<button type="button" class="cfg-union-tab${i === active ? ' active' : ''}" data-tab="${i}">${escapeHtml(name)}</button>`;
  }).join('');
  const panels = variants.map((v, i) =>
    `<div class="cfg-union-panel${i === active ? ' active' : ''}" data-panel="${i}">${renderObject(v, i === active ? (value || {}) : {}, path, null)}</div>`
  ).join('');
  return `<div class="cfg-field" data-path="${path}" data-union="1"><label>${escapeHtml(titleOf(schema, key))}</label><div class="cfg-union-tabs">${tabs}</div>${panels}</div>`;
}

function collect(el) {
  const out = {};
  el.querySelectorAll('[data-path]').forEach(f => {
    if (f.dataset.obj || f.dataset.arr || f.dataset.union) return;
    let p = f.parentElement;
    while (p && p !== el) {
      if (p.classList && p.classList.contains('cfg-union-panel') && !p.classList.contains('active')) return;
      p = p.parentElement;
    }
    const fp = f.dataset.path;
    const input = f.querySelector('input,select');
    if (!input) return;
    let val;
    const k = input.dataset.kind;
    if (k === 'bool') val = input.checked;
    else if (k === 'num') { val = input.value === '' ? null : Number(input.value); }
    else if (k === 'pw') { if (input.value === '') val = input.dataset.orig === '1' ? MASKED : null; else val = input.value; }
    else if (k === 'json') { try { val = JSON.parse(input.value); } catch { val = input.value; } }
    else val = input.value === '' ? null : input.value;
    setPath(out, fp, val);
  });
  el.querySelectorAll('[data-arr="1"]').forEach(a => {
    const fp = a.dataset.path;
    const items = [];
    a.querySelectorAll(':scope > .cfg-array > .cfg-array-item').forEach((it, i) => {
      const sub = collect(it);
      const leafKey = `${fp}[${i}]`;
      if (sub && typeof sub === 'object' && Object.keys(sub).length === 1 && sub[fp] !== undefined) {
        items.push(sub[fp][`[${i}]`] || sub[fp]);
      } else {
        items.push(getPath(sub, leafKey) || sub);
      }
    });
    setPath(out, fp, items);
  });
  return out;
}

function setPath(obj, path, val) {
  const tokens = tokenize(path);
  let c = obj;
  for (let i = 0; i < tokens.length - 1; i++) {
    const t = tokens[i], nxt = tokens[i + 1], isArr = typeof nxt === 'number';
    if (c[t] == null) c[t] = isArr ? [] : {};
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

async function loadAndRender() {
  const root = document.getElementById('config-form-root');
  if (!root) return;
  try {
    const [cfg, schema] = await Promise.all([
      request('/api/config'),
      request('/api/config/schema'),
    ]);
    rootSchema = schema;
    currentData = cfg.config || {};
    root.innerHTML = renderObject(rootSchema, currentData, '', null);
    if (cfg.restart_pending) showToast('Restart pending — Änderungen warten auf Prozess-Neustart.', 'warn');
  } catch (e) {
    showToast('Config laden fehlgeschlagen: ' + e.message, 'err');
  }
}

function bindEvents(root) {
  root.addEventListener('click', ev => {
    const rm = ev.target.closest('.cfg-rm');
    if (rm) { const item = rm.closest('.cfg-array-item'); if (item) item.remove(); ev.preventDefault(); return; }
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
      ev.preventDefault();
      return;
    }
    const tab = ev.target.closest('.cfg-union-tab');
    if (tab) {
      const union = tab.closest('[data-union="1"]');
      union.querySelectorAll('.cfg-union-tab').forEach(t => t.classList.remove('active'));
      union.querySelectorAll('.cfg-union-panel').forEach(p => p.classList.remove('active'));
      tab.classList.add('active');
      union.querySelector(`.cfg-union-panel[data-panel="${tab.dataset.tab}"]`).classList.add('active');
      ev.preventDefault();
      return;
    }
  });
}

function stripNullsFromItem(obj) {
  if (Array.isArray(obj)) return obj.map(stripNullsFromItem);
  if (obj && typeof obj === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(obj)) {
      if (v == null) continue;
      out[k] = stripNullsFromItem(v);
    }
    return out;
  }
  return obj;
}

function normalizeOptionalStructs(payload) {
  // The Mqtt transport variant carries a *non-Option* ca_cert_path, so an
  // empty tls block must be dropped entirely instead of submitted as nulls.
  const r = payload?.receiver;
  if (r && r.transport && r.transport.tls && typeof r.transport.tls === 'object') {
    const hasAnyPath = ['ca_cert_path', 'client_cert_path', 'client_key_path']
      .some(k => r.transport.tls[k] != null);
    if (!hasAnyPath) r.transport.tls = null;
  }
  return payload;
}

function normalizeCustomMetrics(payload) {
  const items = payload && payload.metrics && Array.isArray(payload.metrics.custom_metrics)
    ? payload.metrics.custom_metrics : null;
  if (!items) return payload;
  payload.metrics.custom_metrics = items.map(item => {
    if (!item || typeof item !== 'object') return item;
    if (item.source === 'poll' || item.source === 'push') item.notification = null;
    if (item.source === 'notification' || item.source === 'push') item.poll = null;
    const allNull = o => o && typeof o === 'object' && Object.values(o).every(v => v == null);
    if (allNull(item.poll)) item.poll = null;
    if (allNull(item.notification)) item.notification = null;
    return stripNullsFromItem(item);
  });
  return payload;
}

async function save() {
  if (!rootSchema) { showToast('Schema noch nicht geladen.', 'err'); return; }
  const root = document.getElementById('config-form-root');
  const saveBtn = document.getElementById('config-save-btn');
  if (saveBtn) saveBtn.disabled = true;
  try {
    const payload = normalizeOptionalStructs(normalizeCustomMetrics(collect(root)));
    const r = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const res = await r.json();
    if (r.ok) {
      const hot = (res.hot_reloaded || []).join(', ') || '–';
      const rr = (res.restart_required || []).join(', ');
      const msg = `✓ Gespeichert. Hot-reloaded: ${hot}.` + (rr ? ` Restart erforderlich: ${rr}.` : '');
      showToast(msg, rr ? 'warn' : 'ok');
      currentData = payload;
    } else if (res.errors) {
      showToast('Validierung: ' + res.errors.join('; '), 'err');
    } else {
      showToast('Fehler: ' + (res.detail || res.error || r.statusText), 'err');
    }
  } catch (e) {
    showToast('Save fehlgeschlagen: ' + e.message, 'err');
  } finally {
    if (saveBtn) saveBtn.disabled = false;
  }
}
