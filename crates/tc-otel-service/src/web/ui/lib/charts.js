// Thin uPlot wrapper for sparklines and live charts.
// uPlot is loaded via <script> tag, so it's available as window.uPlot.

const baseOpts = (color, accent) => ({
  scales: { x: { time: false }, y: { auto: true } },
  axes: [
    { show: false },
    { show: false },
  ],
  cursor: { show: false },
  legend: { show: false },
  series: [
    {},
    {
      stroke: color,
      width: 1.5,
      fill: accent ? color + '22' : undefined,
      points: { show: false },
    },
  ],
});

export function sparkline(el, opts = {}) {
  const w = opts.width || el.clientWidth || 80;
  const h = opts.height || el.clientHeight || 28;
  const o = baseOpts(opts.color || '#38bdf8', true);
  o.width = w;
  o.height = h;
  o.padding = [2, 2, 2, 2];
  // Initialise with a single empty point so uPlot renders.
  return new window.uPlot(o, [[0], [null]], el);
}

export function updateSpark(plot, ys) {
  if (!plot) return;
  if (!ys || ys.length === 0) {
    plot.setData([[0], [null]]);
    return;
  }
  const xs = ys.map((_, i) => i);
  plot.setData([xs, ys]);
}

export function liveChart(el, opts = {}) {
  const w = opts.width || el.clientWidth || 600;
  const h = opts.height || 160;
  const series = opts.series || [{ label: 'value', color: '#38bdf8' }];
  const seriesDefs = [{}].concat(series.map(s => ({
    label: s.label,
    stroke: s.color,
    width: 1.5,
    points: { show: false },
  })));
  const o = {
    width: w,
    height: h,
    padding: [8, 12, 18, 38],
    scales: { x: { time: false }, y: { auto: true } },
    axes: [
      { show: true, stroke: '#74849f', grid: { stroke: '#243049' }, ticks: { stroke: '#243049' } },
      { show: true, stroke: '#74849f', grid: { stroke: '#243049' }, ticks: { stroke: '#243049' }, size: 36 },
    ],
    cursor: { show: true, drag: { setScale: false } },
    legend: { show: false },
    series: seriesDefs,
  };
  const init = [[0]].concat(series.map(() => [null]));
  return new window.uPlot(o, init, el);
}

export function updateLive(plot, ys) {
  if (!plot) return;
  // ys is an array of arrays — one per series, each same length.
  if (!ys || !ys.length || !ys[0].length) {
    const empty = [[0]].concat(ys ? ys.map(() => [null]) : [[null]]);
    plot.setData(empty);
    return;
  }
  const xs = ys[0].map((_, i) => i);
  plot.setData([xs, ...ys]);
}
