// Small shared helpers — request, format, escape, toast, ring buffer.

export async function request(url, opts) {
  const r = await fetch(url, opts);
  if (r.status === 503) throw new Error('client-bridge is not enabled (HTTP 503)');
  if (!r.ok) {
    let detail = '';
    try { detail = await r.text(); } catch (_) {}
    throw new Error(detail || r.statusText || `HTTP ${r.status}`);
  }
  if (r.status === 204) return null;
  const ct = r.headers.get('content-type') || '';
  return ct.includes('application/json') ? r.json() : r.text();
}

export function fmtUptime(s) {
  if (s == null) return '—';
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return `${h}h ${m}m ${sec}s`;
}

export function fmtNum(n) {
  if (n == null || isNaN(n)) return '—';
  return Number(n).toLocaleString();
}

export function escapeHtml(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, c => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

const TOAST_ROOT_ID = 'toast-root';
export function toast(msg, kind = '') {
  const root = document.getElementById(TOAST_ROOT_ID);
  if (!root) return;
  const el = document.createElement('div');
  el.className = 'toast' + (kind ? ` toast-${kind}` : '');
  el.textContent = msg;
  root.appendChild(el);
  setTimeout(() => { el.style.opacity = '0'; el.style.transform = 'translateY(8px)'; }, kind === 'err' ? 8000 : 4000);
  setTimeout(() => el.remove(), kind === 'err' ? 8500 : 4500);
}

export function showError(msg) {
  const e = document.getElementById('error');
  if (!e) return;
  e.textContent = msg;
  e.hidden = false;
  e.className = 'banner banner-err';
  clearTimeout(showError._t);
  showError._t = setTimeout(() => { e.hidden = true; }, 6000);
}

// Bounded FIFO buffer for chart series.
export class RingBuffer {
  constructor(size) { this.size = size; this.buf = []; }
  push(v) { this.buf.push(v); if (this.buf.length > this.size) this.buf.shift(); }
  values() { return this.buf.slice(); }
  length() { return this.buf.length; }
}

// Debounce factory (for filter inputs).
export function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}
