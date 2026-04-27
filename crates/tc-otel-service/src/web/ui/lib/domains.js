// Domain helpers: deterministic colour from AMS Net ID, friendly name, initials.

const PALETTE = [
  '#38bdf8', // sky
  '#818cf8', // indigo
  '#34d399', // emerald
  '#fbbf24', // amber
  '#f472b6', // pink
  '#a78bfa', // violet
  '#facc15', // yellow
  '#22d3ee', // cyan
  '#fb7185', // rose
  '#4ade80', // green
];

function hash32(str) {
  let h = 0x811c9dc5;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export function domainColor(amsNetId) {
  return PALETTE[hash32(amsNetId) % PALETTE.length];
}

export function domainInitials(domain) {
  // Prefer friendly name's first letter; fall back to last AMS Net ID octets.
  if (domain.friendly_name && domain.friendly_name !== domain.ams_net_id) {
    const fn = domain.friendly_name.trim();
    const parts = fn.split(/[\s\-_.]+/).filter(Boolean);
    if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
    return fn.slice(0, 2).toUpperCase();
  }
  // From "172.28.41.37.1.1" → "37"
  const parts = domain.ams_net_id.split('.');
  return parts[3] || parts[parts.length - 1] || '??';
}

export function chipsFor(domain) {
  const out = [];
  if (domain.sources.includes('diagnostics')) out.push({ cls: 'chip-diag', text: 'diagnostics' });
  if (domain.sources.includes('metrics')) out.push({ cls: 'chip-metric', text: 'metrics' });
  if (domain.sources.includes('registered')) out.push({ cls: 'chip-reg', text: 'registered' });
  if (domain.symbols_cached) out.push({ cls: 'chip-sym', text: 'symbols cached' });
  return out;
}
