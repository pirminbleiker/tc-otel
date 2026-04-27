// Domain-centric dashboard view.
// Polls /api/status, /api/connections, /api/cycle-metrics, /api/domains every 2s
// and renders KPIs, domain cards with cycle-time sparklines, and a
// connections table.

import { request, fmtUptime, fmtNum, escapeHtml, RingBuffer, showError } from '../lib/util.js';
import { domainColor, domainInitials, chipsFor } from '../lib/domains.js';
import { sparkline, updateSpark } from '../lib/charts.js';

const POLL_MS = 2000;
const LOG_BUFFER = 60;       // samples shown in logs/sec sparkline
const CYCLE_BUFFER = 60;     // samples per task

// Per-task ring buffer of avg cycle-time, keyed by `${ams_net_id}|${task_index}|${task_name}`.
const cycleBuffers = new Map();
const cyclePlots = new Map();

// Logs/sec ring buffer (delta of logs_dispatched between polls).
const logsBuffer = new RingBuffer(LOG_BUFFER);
let logsPlot = null;
let lastLogsDispatched = null;
let lastLogsTs = null;

let timer = null;

export function start() {
  refresh();
  timer = setInterval(refresh, POLL_MS);
}

export function stop() {
  if (timer) clearInterval(timer);
  timer = null;
}

async function refresh() {
  try {
    const [st, cn, cy, dm] = await Promise.all([
      request('/api/status'),
      request('/api/connections'),
      request('/api/cycle-metrics'),
      request('/api/domains'),
    ]);
    renderKpis(st);
    renderDomains(dm, cy);
    renderConnections(cn);
    setLastUpdate();
    setStatusPill('ok', st.status || 'running');
  } catch (e) {
    showError('Refresh failed: ' + e.message);
    setStatusPill('err', 'offline');
  }
}

function setStatusPill(kind, text) {
  const pill = document.getElementById('status-pill');
  if (!pill) return;
  pill.className = 'pill pill-' + kind;
  pill.textContent = text;
}

function setLastUpdate() {
  const el = document.getElementById('last-update');
  if (el) el.textContent = 'Updated ' + new Date().toLocaleTimeString();
}

// --- KPIs ---

function renderKpis(st) {
  const now = performance.now();
  let logsPerSec = null;
  if (lastLogsDispatched != null && lastLogsTs != null) {
    const dt = (now - lastLogsTs) / 1000;
    if (dt > 0) logsPerSec = Math.max(0, (st.logs_dispatched - lastLogsDispatched) / dt);
  }
  lastLogsDispatched = st.logs_dispatched;
  lastLogsTs = now;
  if (logsPerSec != null) logsBuffer.push(logsPerSec);

  const root = document.getElementById('kpis');
  if (!root) return;

  // Render once, then update values in place.
  if (!root.dataset.rendered) {
    root.innerHTML = `
      <div class="kpi" data-k="status"><div class="kpi-label">Status</div><div class="kpi-value ok" data-v>—</div><div class="kpi-sub" data-s></div></div>
      <div class="kpi" data-k="uptime"><div class="kpi-label">Uptime</div><div class="kpi-value" data-v>—</div></div>
      <div class="kpi" data-k="logs_in"><div class="kpi-label">Logs Received</div><div class="kpi-value" data-v>—</div><div class="kpi-sub" data-s></div></div>
      <div class="kpi" data-k="logs_out"><div class="kpi-label">Logs Dispatched</div><div class="kpi-value" data-v>—</div><div class="kpi-spark" data-spark></div></div>
      <div class="kpi" data-k="logs_failed"><div class="kpi-label">Logs Failed</div><div class="kpi-value" data-v>—</div></div>
      <div class="kpi" data-k="conns"><div class="kpi-label">Active Connections</div><div class="kpi-value" data-v>—</div></div>
      <div class="kpi" data-k="tasks"><div class="kpi-label">Registered Tasks</div><div class="kpi-value" data-v>—</div></div>`;
    root.dataset.rendered = '1';
  }

  setKpi(root, 'status', st.status, '', 'ok');
  setKpi(root, 'uptime', fmtUptime(st.uptime_secs));
  setKpi(root, 'logs_in', fmtNum(st.logs_received));
  setKpi(root, 'logs_out', fmtNum(st.logs_dispatched));
  setKpi(root, 'logs_failed', fmtNum(st.logs_failed), '', st.logs_failed > 0 ? 'warn' : '');
  setKpi(root, 'conns', fmtNum(st.connections_active));
  setKpi(root, 'tasks', fmtNum(st.registered_tasks));

  if (logsPerSec != null) {
    const sub = root.querySelector('[data-k="logs_in"] [data-s]');
    if (sub) sub.textContent = `${logsPerSec.toFixed(1)} / s`;
  }

  // Sparkline for logs/sec
  const sparkEl = root.querySelector('[data-k="logs_out"] [data-spark]');
  if (sparkEl && window.uPlot) {
    if (!logsPlot) logsPlot = sparkline(sparkEl, { width: 80, height: 26, color: '#38bdf8' });
    updateSpark(logsPlot, logsBuffer.values());
  }
}

function setKpi(root, key, value, sub, valueClass) {
  const node = root.querySelector(`[data-k="${key}"]`);
  if (!node) return;
  const v = node.querySelector('[data-v]');
  if (v) {
    v.textContent = value == null ? '—' : value;
    v.className = 'kpi-value' + (valueClass ? ' ' + valueClass : '');
  }
  if (sub != null) {
    const s = node.querySelector('[data-s]');
    if (s) s.textContent = sub;
  }
}

// --- Domain cards ---

function renderDomains(domains, cycleStats) {
  const grid = document.getElementById('domain-grid');
  const count = document.getElementById('domain-count');
  if (count) count.textContent = `${domains.length} domain${domains.length === 1 ? '' : 's'}`;
  if (!grid) return;

  if (!domains.length) {
    grid.innerHTML = `<div class="empty-state"><div class="icon">∅</div><div>No domains configured.</div><div class="muted small">Add a target under <code>diagnostics.targets</code> or <code>metrics.custom_metrics</code> in your config.</div></div>`;
    return;
  }

  // Group cycle stats by ams_net_id for fast lookup.
  const cycleByDomain = new Map();
  for (const c of cycleStats || []) {
    if (!cycleByDomain.has(c.ams_net_id)) cycleByDomain.set(c.ams_net_id, []);
    cycleByDomain.get(c.ams_net_id).push(c);
    const k = `${c.ams_net_id}|${c.task_index}|${c.task_name}`;
    if (!cycleBuffers.has(k)) cycleBuffers.set(k, new RingBuffer(CYCLE_BUFFER));
    cycleBuffers.get(k).push(c.avg_us);
  }

  // Re-render markup if domain set changed; otherwise patch in place.
  const ids = domains.map(d => d.ams_net_id).join('|');
  if (grid.dataset.ids !== ids) {
    grid.dataset.ids = ids;
    grid.innerHTML = domains.map(d => domainCardHtml(d, cycleByDomain.get(d.ams_net_id) || [])).join('');
    // Reset plots since DOM was replaced.
    cyclePlots.clear();
  }

  // Update / create sparklines for each task row.
  for (const d of domains) {
    const tasks = cycleByDomain.get(d.ams_net_id) || [];
    for (const c of tasks) {
      const k = `${d.ams_net_id}|${c.task_index}|${c.task_name}`;
      const row = grid.querySelector(`[data-task-key="${cssEscape(k)}"]`);
      if (!row) continue;
      const sparkEl = row.querySelector('.spark');
      if (!sparkEl) continue;
      const buf = cycleBuffers.get(k);
      const values = buf ? buf.values() : [];
      if (!cyclePlots.has(k) && window.uPlot && sparkEl.clientWidth > 0) {
        cyclePlots.set(k, sparkline(sparkEl, { color: domainColor(d.ams_net_id) }));
      }
      updateSpark(cyclePlots.get(k), values);
      const numEl = row.querySelector('.num');
      if (numEl) {
        numEl.textContent = `${c.avg_us.toFixed(1)} µs · J ${c.jitter_us.toFixed(1)}`;
        numEl.className = 'num' + (c.jitter_us > c.avg_us * 0.5 ? ' warn' : '');
      }
    }
  }
}

function domainCardHtml(d, tasks) {
  const color = domainColor(d.ams_net_id);
  const initials = domainInitials(d);
  const chips = chipsFor(d).map(c => `<span class="chip ${c.cls}">${escapeHtml(c.text)}</span>`).join('');
  const host = d.router_host ? ` · ${escapeHtml(d.router_host)}` : '';

  const taskRows = tasks.length ? tasks.map(c => {
    const k = `${d.ams_net_id}|${c.task_index}|${c.task_name}`;
    return `<div class="task-row" data-task-key="${escapeHtml(k)}">
      <span class="name">${escapeHtml(c.task_name)} <span class="muted small">[${c.task_index}]</span></span>
      <div class="spark"></div>
      <span class="num">${c.avg_us.toFixed(1)} µs · J ${c.jitter_us.toFixed(1)}</span>
    </div>`;
  }).join('') : `<div class="muted small">No cycle samples yet.</div>`;

  return `<article class="domain-card" style="--domain-color:${color}">
    <div class="domain-head">
      <div class="domain-badge" style="background:${color}">${escapeHtml(initials)}</div>
      <div>
        <div class="domain-title">${escapeHtml(d.friendly_name)}</div>
        <div class="domain-id mono">${escapeHtml(d.ams_net_id)}${host}</div>
      </div>
    </div>
    <div class="domain-chips">${chips}</div>
    <div class="domain-stats">
      <div class="domain-stat"><div class="domain-stat-v">${d.task_count}</div><div class="domain-stat-l">Tasks</div></div>
      <div class="domain-stat"><div class="domain-stat-v">${d.metric_count}</div><div class="domain-stat-l">Metrics</div></div>
      <div class="domain-stat"><div class="domain-stat-v">${d.symbol_count != null ? fmtNum(d.symbol_count) : '—'}</div><div class="domain-stat-l">Symbols</div></div>
    </div>
    <div class="domain-tasks">${taskRows}</div>
    <div class="domain-actions">
      <a class="btn" href="#/symbols?target=${encodeURIComponent(d.ams_net_id)}">Browse symbols</a>
      <a class="btn btn-ghost" href="#/config">Configure</a>
    </div>
  </article>`;
}

function cssEscape(s) {
  // Minimal escape for use inside [data-task-key="…"] selector.
  return String(s).replace(/["\\]/g, '\\$&');
}

// --- Connections table ---

function renderConnections(list) {
  const body = document.getElementById('conn-body');
  if (!body) return;
  if (!list.length) {
    body.innerHTML = `<tr><td colspan="2" class="empty">No active connections</td></tr>`;
    return;
  }
  body.innerHTML = list.map(c =>
    `<tr><td><code>${escapeHtml(c.ip)}</code></td><td>${c.count}</td></tr>`
  ).join('');
}
