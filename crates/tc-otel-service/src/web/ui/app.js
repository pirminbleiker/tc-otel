// Entry point: hash-based router and view lifecycle.

import * as dashboard from './views/dashboard.js';
import * as config from './views/config.js';
import * as symbols from './views/symbols.js';

const VIEWS = {
  '': { id: 'dashboard-view', enter: () => dashboard.start(), exit: () => dashboard.stop() },
  'config': { id: 'config-view', enter: async () => { config.init(); await config.ensureLoaded(); }, exit: () => {} },
  'symbols': { id: 'symbols-view', enter: async () => { symbols.init(); await symbols.show(); }, exit: () => {} },
};

let activeRoute = null;

function parseRoute() {
  const h = location.hash || '#/';
  const path = h.replace(/^#\/?/, '').split('?')[0];
  return path in VIEWS ? path : '';
}

async function route() {
  const next = parseRoute();
  if (next === activeRoute) {
    // Same view: still let it refresh (e.g. ?target=… changed for symbols).
    if (next === 'symbols') await symbols.show();
    return;
  }
  if (activeRoute != null && VIEWS[activeRoute]) VIEWS[activeRoute].exit();
  activeRoute = next;
  for (const [key, def] of Object.entries(VIEWS)) {
    const el = document.getElementById(def.id);
    if (el) el.hidden = key !== next;
  }
  document.querySelectorAll('.nav-link').forEach(a => {
    a.classList.toggle('active', (a.dataset.route || '') === next);
  });
  await VIEWS[next].enter();
}

window.addEventListener('hashchange', route);
route();
