// PLC symbol browser view — fetches the per-domain symbol cache via the
// active client-bridge endpoints. Migrated from DASHBOARD_HTML.

import { request, escapeHtml, debounce } from '../lib/util.js';
import { domainColor, domainInitials } from '../lib/domains.js';

let currentTarget = null;
let lastSymbols = [];
let initialized = false;

export function init() {
  if (initialized) return;
  initialized = true;
  const filter = document.getElementById('sym-filter');
  const refresh = document.getElementById('sym-refresh-btn');
  if (filter) filter.addEventListener('input', debounce(loadSymbols, 200));
  if (refresh) refresh.addEventListener('click', refreshCurrent);
}

export async function show() {
  await loadTargets();
  // Allow deep-linking via #/symbols?target=…
  const m = location.hash.match(/[?&]target=([^&]+)/);
  if (m) {
    const t = decodeURIComponent(m[1]);
    if (t !== currentTarget) await pick(t);
  } else if (currentTarget) {
    await loadSymbols();
  }
}

function banner(msg, kind) {
  const el = document.getElementById('sym-banner');
  if (!el) return;
  if (!msg) { el.hidden = true; return; }
  el.textContent = msg;
  el.hidden = false;
  el.className = 'banner ' + (kind === 'err' ? 'banner-err' : 'banner-warn');
}

async function loadTargets() {
  banner('', '');
  const tb = document.getElementById('sym-targets');
  if (!tb) return;
  try {
    const list = await request('/api/client/targets');
    if (!list.length) {
      tb.innerHTML = `<tr><td colspan="5" class="empty">No custom_metrics with poll/notification source configured.</td></tr>`;
      return;
    }
    tb.innerHTML = list.map(t => {
      const cached = t.cached
        ? `<span style="color:var(--ok)">✓</span>`
        : `<span class="muted">—</span>`;
      const when = t.fetched_at || 'never';
      const pickLabel = currentTarget === t.ams_net_id ? 'Selected' : 'Browse';
      const color = domainColor(t.ams_net_id);
      const initials = domainInitials({ ams_net_id: t.ams_net_id, friendly_name: t.ams_net_id });
      return `<tr>
        <td>
          <div style="display:flex;align-items:center;gap:.5rem">
            <span class="domain-badge" style="background:${color};width:24px;height:24px;font-size:.7rem">${escapeHtml(initials)}</span>
            <code>${escapeHtml(t.ams_net_id)}</code>
          </div>
        </td>
        <td>${cached}</td>
        <td>${t.symbol_count}</td>
        <td class="muted small">${escapeHtml(when)}</td>
        <td><button class="btn" data-pick="${escapeHtml(t.ams_net_id)}">${pickLabel}</button></td>
      </tr>`;
    }).join('');
    tb.querySelectorAll('button[data-pick]').forEach(b => {
      b.addEventListener('click', () => pick(b.dataset.pick));
    });
  } catch (e) {
    banner('Failed to load targets: ' + e.message, 'err');
  }
}

async function pick(target) {
  currentTarget = target;
  const sel = document.getElementById('sym-selected-target');
  if (sel) sel.textContent = target;
  await loadSymbols();
  await loadTargets();
}

async function loadSymbols() {
  if (!currentTarget) return;
  const filter = document.getElementById('sym-filter')?.value.trim() || '';
  const url = '/api/client/symbols?target=' + encodeURIComponent(currentTarget)
    + (filter ? '&filter=' + encodeURIComponent(filter) : '');
  const stats = document.getElementById('sym-stats');
  const rows = document.getElementById('sym-rows');
  try {
    const res = await request(url);
    lastSymbols = res.symbols;
    if (stats) stats.textContent = `${res.count} symbol(s) — fetched ${res.fetched_at || '—'}`;
    renderSymbols();
  } catch (e) {
    if (rows) rows.innerHTML = `<tr><td colspan="6" style="color:var(--err)">Error: ${escapeHtml(e.message)}</td></tr>`;
    if (stats) stats.textContent = '';
  }
}

function renderSymbols() {
  const body = document.getElementById('sym-rows');
  if (!body) return;
  if (!lastSymbols.length) {
    body.innerHTML = `<tr><td colspan="6" class="empty">No symbols match.</td></tr>`;
    return;
  }
  const limit = 500;
  const shown = lastSymbols.slice(0, limit);
  body.innerHTML = shown.map(s =>
    `<tr>
      <td><code>${escapeHtml(s.name)}</code></td>
      <td>${escapeHtml(s.type_name)}</td>
      <td>${s.size}</td>
      <td>0x${s.igroup.toString(16)}</td>
      <td>${s.ioffset}</td>
      <td><button class="btn btn-ghost" data-copy="${escapeHtml(s.name)}" style="padding:.15rem .5rem;font-size:.7rem">copy</button></td>
    </tr>`
  ).join('') + (lastSymbols.length > limit
    ? `<tr><td colspan="6" class="muted small">… ${lastSymbols.length - limit} more, narrow the filter.</td></tr>`
    : '');
  body.querySelectorAll('button[data-copy]').forEach(b => {
    b.addEventListener('click', () => navigator.clipboard?.writeText(b.dataset.copy));
  });
}

async function refreshCurrent() {
  if (!currentTarget) return;
  try {
    await request('/api/client/symbols/refresh?target=' + encodeURIComponent(currentTarget), { method: 'POST' });
    banner('Cache invalidated — waiting for next reconcile to repopulate.', '');
    await loadTargets();
  } catch (e) {
    banner('Refresh failed: ' + e.message, 'err');
  }
}
