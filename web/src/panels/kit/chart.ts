// Muster Kit chart renderer: the chart app, pr_decision's media charts, Board chart blocks and
// <mk-chart> in kit panels all draw with this. It redraws at its element's width (text is never
// scaled), and hover, arrow keys, legend toggles and a live region work everywhere. Every value is
// also in a data table (visually hidden unless shown), so the chart is never the only way in.
import { clamp, esc, fmt, lin, niceTicks, safeColor, smooth, tickLabels, trendLine } from "./format.ts";
import { icon } from "./icons.ts";
import { nid, seriesColor } from "./parts.ts";

export type ChartType = "bar" | "line" | "area" | "scatter";
export interface KitSeries { name: string; values: (number | null)[]; color?: string }
export interface KitChartSpec {
  type: ChartType;
  title?: string;
  x: { label?: string; values: (string | number)[] };
  series: KitSeries[];
  y?: { label?: string; min?: number; max?: number };
  format?: string;
  xFormat?: string;
  highlight?: string;
  annotations?: { x: string | number; label: string }[];
  stacked?: boolean;
  labels?: "auto" | "values" | "end" | "none";
  trend?: boolean;
  empty?: string;
  pin?: number;
}
export interface ChartOptions {
  height?: number;
  /** Index to show the tooltip at when the pointer is away. */
  pin?: number | null;
  /** true: always; false: never; default: when there are several series and no end labels. */
  legend?: boolean;
  /** Each legend key also shows the series total (bars) or last value (lines). Default true. */
  legendValues?: boolean;
  /** The tooltip shows each value's change from the previous point. Default true. */
  tipDelta?: boolean;
  /** Show the data table instead of keeping it for screen readers only. */
  showTable?: boolean;
  hidden?: string[];
}

const TYPE: Record<ChartType, string> = { bar: "Bar chart", line: "Line chart", area: "Area chart", scatter: "Scatter plot" };

interface State {
  id: string;
  spec: KitChartSpec;
  o: ChartOptions;
  hidden: Set<string>;
  focus: number | null;
  width: number;
  idxAt?: (px: number, py: number) => number | null;
  raf: number;
  observer?: ResizeObserver;
}

type Host = HTMLElement & { _mk?: State };

export const isEmpty = (spec: KitChartSpec) =>
  !spec.x.values.length || !spec.series.length ||
  spec.series.every((s) => s.values.every((v) => v === null || !Number.isFinite(v)));

/** The series drawn in colour while the rest turn grey: `highlight`, or past 8 series the first. */
export function highlightOf(spec: KitChartSpec): string | null {
  if (spec.highlight && spec.series.some((s) => s.name === spec.highlight)) return spec.highlight;
  return spec.series.length > 8 ? spec.series[0].name : null;
}

/** The y range to draw: stacks keep positives above zero and negatives below; bars and areas
 * include zero; lines and scatters get a little air; a fixed y.min / y.max wins. */
export function yDomain(spec: KitChartSpec, series: KitSeries[], n: number) {
  const type = spec.type;
  const stacked = !!spec.stacked && (type === "bar" || type === "area");
  let lo = Infinity, hi = -Infinity;
  if (stacked) {
    for (let i = 0; i < n; i++) {
      let p = 0, q = 0;
      for (const s of series) {
        const v = s.values[i];
        if (v === null || v === undefined || !Number.isFinite(v)) continue;
        if (v >= 0) p += v;
        else q += v;
      }
      hi = Math.max(hi, p);
      lo = Math.min(lo, q);
    }
  } else {
    for (const s of series) for (const v of s.values) if (v !== null && Number.isFinite(v)) { lo = Math.min(lo, v); hi = Math.max(hi, v); }
  }
  if (!Number.isFinite(lo)) { lo = 0; hi = 1; }
  if (type === "bar" || type === "area") { lo = Math.min(lo, 0); hi = Math.max(hi, 0); }
  else if (hi > lo) {
    const pad = (hi - lo) * 0.08, dataLo = lo;
    lo -= pad;
    hi += pad;
    if (lo < 0 && dataLo >= 0) lo = 0;
  }
  return { lo, hi };
}

/** Ticks for the y axis, with any fixed y.min / y.max kept exactly. */
export function yTicks(spec: KitChartSpec, lo: number, hi: number, count: number) {
  const fmin = spec.y?.min, fmax = spec.y?.max;
  const span = Math.max(hi - lo, Math.abs(hi), Math.abs(lo)) || 1;
  if (fmin !== undefined) lo = fmin;
  if (fmax !== undefined) hi = fmax;
  // One fixed bound beyond the data: keep a non-empty range on the open side.
  if (hi <= lo) {
    if (fmax === undefined) hi = lo + span;
    else if (fmin === undefined) lo = hi - span;
    else hi = lo + 1;
  }
  const t = niceTicks(lo, hi, count, spec.format);
  const min = fmin ?? t.min, max = fmax ?? t.max;
  const eps = (max - min) * 1e-9;
  return { min, max, step: t.step, ticks: t.ticks.filter((v) => v >= min - eps && v <= max + eps) };
}

/** Stack each point: positives up from zero, negatives down from zero. [bottom, top] per series and point. */
export function stackBars(series: KitSeries[], n: number): ([number, number] | null)[][] {
  const pos = new Array(n).fill(0), neg = new Array(n).fill(0);
  return series.map((s) => s.values.slice(0, n).map((v, i) => {
    if (v === null || !Number.isFinite(v)) return null;
    if (v >= 0) { const b = pos[i]; pos[i] += v; return [b, pos[i]] as [number, number]; }
    const b = neg[i];
    neg[i] += v;
    return [b, neg[i]] as [number, number];
  }));
}

/** "+15%", "−0.04 pt" (for pct values), or null without a previous value. */
export function change(v: number | null, p: number | null | undefined, pct: boolean): string | null {
  if (v === null || p === null || p === undefined || !Number.isFinite(v) || !Number.isFinite(p)) return null;
  if (pct) return `${v - p >= 0 ? "+" : "−"}${Math.abs(v - p).toFixed(2)} pt`;
  if (!p) return null;
  const r = Math.round(((v - p) / Math.abs(p)) * 100);
  return `${r >= 0 ? "+" : "−"}${Math.abs(r)}%`;
}

function render(el: Host) {
  const st = el._mk!;
  const { spec, o, id } = st;
  const width = Math.max(240, st.width);
  const type = spec.type || "bar";
  const xs = spec.x.values, n = xs.length;
  const series = spec.series.map((s, i) => ({ ...s, i, color: safeColor(s.color, seriesColor(i)) }));
  const vis = series.filter((s) => !st.hidden.has(s.name));
  const F = (v: number | null | undefined) => fmt(v, spec.format);
  const hl = highlightOf(spec);
  const isCtx = (s: { name: string }) => !!hl && s.name !== hl;
  const scol = (s: { name: string; color: string }) => isCtx(s) ? "var(--mk-context)" : s.color;
  const numericX = xs.length > 0 && xs.every((v) => typeof v === "number");
  const intX = numericX && xs.every((v) => Number.isInteger(v));
  const XF = (v: string | number) => typeof v === "number" ? fmt(v, spec.xFormat || (intX ? "plain" : undefined)) : v;
  const stacked = !!spec.stacked && (type === "area" || type === "bar");
  const narrow = width < 420;
  const height = Math.round(o.height || clamp(width * 0.44, 200, 380));
  const title = spec.title || TYPE[type];
  const hadFocus = el.contains(document.activeElement) ? document.activeElement as HTMLElement : null;

  if (isEmpty(spec) || !vis.length && !series.length) {
    el.innerHTML = `<div class="mk-empty" role="img" aria-label="${esc(title)}: no data" style="min-height:${Math.min(height, 200)}px">${icon("chart")}<b>No data to show</b><span>${esc(spec.empty || "The agent sent this chart without any values.")}</span></div>`;
    st.idxAt = undefined;
    return;
  }

  const { lo, hi } = yDomain(spec, vis.length ? vis : series, n);
  const yt = yTicks(spec, lo, hi, height < 150 ? 2 : height < 260 ? 3 : 4);
  const ylabels = tickLabels(yt.ticks, spec.format);

  const endLabels = (type === "line" || type === "area") && !narrow && spec.labels !== "none" && vis.length > 0 && vis.length <= 6;
  const last = (s: KitSeries) => { for (let i = n - 1; i >= 0; i--) if (s.values[i] !== null) return i; return -1; };
  const endW = endLabels ? Math.min(150, Math.max(...vis.map((s) => (s.name.length + F(s.values[last(s)] ?? null).length + 1) * 6.4)) + 16) : 0;
  const m = { top: spec.y?.label || spec.annotations?.length ? 28 : 14, right: 12 + endW, bottom: 26, left: Math.max(...ylabels.map((l) => l.length)) * 7 + 12 };
  const pw = Math.max(40, width - m.left - m.right), ph = Math.max(40, height - m.top - m.bottom);
  const Y0 = lin(yt.min, yt.max, m.top + ph, m.top);
  const Y = (v: number) => clamp(Y0(v), m.top - 2, m.top + ph + 2);
  const band = pw / n;
  let X: (i: number) => number;
  let XAt: ((x: number) => number) | null = null;
  let xticks: { at: number; label: string }[];
  if (numericX && type !== "bar") {
    const nx = xs as number[];
    const xl = Math.min(...nx), xh = Math.max(...nx);
    const t = niceTicks(xl, xh, Math.max(2, Math.floor(pw / 100)));
    const [d0, d1] = type === "scatter" ? [t.min, t.max] : [xl, xh === xl ? xl + 1 : xh];
    const XV = lin(d0, d1, m.left, m.left + pw);
    XAt = XV;
    X = (i) => XV(nx[i]);
    const labels = tickLabels(t.ticks, spec.xFormat || (intX ? "plain" : undefined));
    xticks = t.ticks.flatMap((v, k) => v >= d0 - 1e-9 && v <= d1 + 1e-9 ? [{ at: XV(v), label: labels[k] }] : []);
  } else {
    X = type === "bar" ? (i) => m.left + band * (i + 0.5) : (i) => m.left + (n === 1 ? pw / 2 : (pw * i) / (n - 1));
    const longest = Math.max(...xs.map((v) => String(XF(v)).length));
    const per = type === "bar" ? band : pw / Math.max(1, n - 1);
    const every = Math.max(1, Math.ceil((Math.min(longest, 14) * 6.8 + 16) / per));
    xticks = xs.flatMap((v, i) => {
      if (i % every) return [];
      const s = String(XF(v));
      return [{ at: X(i), label: s.length > 14 ? s.slice(0, 13) + "…" : s }];
    });
  }
  st.idxAt = (px) => {
    if (px < m.left - 8 || px > m.left + pw + 8) return null;
    if (type === "bar") return clamp(Math.floor((px - m.left) / band), 0, n - 1);
    let best = 0, bd = Infinity;
    for (let i = 0; i < n; i++) { const d = Math.abs(X(i) - px); if (d < bd) { bd = d; best = i; } }
    return best;
  };

  const P: string[] = [], defs: string[] = [];
  P.push(`<g class="grid">${yt.ticks.map((t) => `<line x1="${m.left}" x2="${m.left + pw}" y1="${Y0(t).toFixed(1)}" y2="${Y0(t).toFixed(1)}"/>`).join("")}</g>`);
  P.push(`<g class="yaxis">${yt.ticks.map((t, k) => `<text class="tick" x="${m.left - 10}" y="${(Y0(t) + 4).toFixed(1)}" text-anchor="end">${esc(ylabels[k])}</text>`).join("")}</g>`);
  if (spec.y?.label) P.push(`<text class="ylab" x="0" y="${spec.annotations?.length ? 11 : m.top - 14}">${esc(spec.y.label)}</text>`);
  P.push(`<g class="xaxis">${xticks.map((t, k) => {
    const anchor = type !== "bar" && k === xticks.length - 1 && t.at > m.left + pw - 24 ? "end" : type !== "bar" && k === 0 && t.at < m.left + 24 ? "start" : "middle";
    return `<text class="tick x" x="${t.at.toFixed(1)}" y="${m.top + ph + 18}" text-anchor="${anchor}">${esc(t.label)}</text>`;
  }).join("")}</g>`);

  const focus = st.focus !== null && st.focus >= 0 && st.focus < n ? st.focus : null;
  if (focus !== null) {
    if (type === "bar") P.push(`<rect class="band" x="${(m.left + band * focus + 2).toFixed(1)}" y="${m.top}" width="${Math.max(1, band - 4).toFixed(1)}" height="${ph}" rx="6"/>`);
    else if (type !== "scatter") P.push(`<line class="guide" x1="${X(focus).toFixed(1)}" x2="${X(focus).toFixed(1)}" y1="${m.top}" y2="${m.top + ph}"/>`);
  }

  const zero = Y(clamp(0, yt.min, yt.max));
  if (type === "bar") {
    const k = stacked ? 1 : Math.max(1, vis.length);
    const inner = band * (k > 1 ? 0.74 : 0.56);
    const gap = k > 1 ? Math.min(3, (inner / k) * 0.12) : 0;
    const bw = Math.max(2, (inner - gap * (k - 1)) / k);
    const want = spec.labels === "values" || (spec.labels !== "none" && spec.labels !== "end" && n * k <= 16 && bw >= 16);
    const fits = (txt: string, room: number) => txt.length * 6.6 <= room + 6;
    const stack = stacked ? stackBars(vis, n) : null;
    // The outermost positive bar of each stack gets the rounded top.
    const topOf = stack ? Array.from({ length: n }, (_, i) => vis.map((s) => (s.values[i] ?? 0) > 0).lastIndexOf(true)) : [];
    vis.forEach((s, si) => {
      s.values.forEach((v, i) => {
        if (i >= n) return;
        const x0 = m.left + band * i + (band - inner) / 2 + (stacked ? 0 : si * (bw + gap));
        if (v === null || !Number.isFinite(v)) {
          if (want && !stacked) P.push(`<text class="vlab nil" x="${(x0 + bw / 2).toFixed(1)}" y="${(zero - 6).toFixed(1)}" text-anchor="middle">–</text>`);
          return;
        }
        const [b0, b1] = stack ? stack[si][i]! : [0, v];
        const yTop = Math.min(Y(b1), Y(b0)), yBot = Math.max(Y(b1), Y(b0));
        const h = Math.max(v === 0 ? 0 : 1.5, yBot - yTop);
        const outer = !stacked || (v > 0 && topOf[i] === si);
        const r = outer && v > 0 ? Math.min(4, bw / 3, h) : 0;
        const top = v >= 0 ? yBot - h : yTop;
        P.push(`<path class="bar" data-i="${i}" data-s="${si}" d="M${x0.toFixed(1)},${(top + h).toFixed(1)}V${(top + r).toFixed(1)}Q${x0.toFixed(1)},${top.toFixed(1)} ${(x0 + r).toFixed(1)},${top.toFixed(1)}H${(x0 + bw - r).toFixed(1)}Q${(x0 + bw).toFixed(1)},${top.toFixed(1)} ${(x0 + bw).toFixed(1)},${(top + r).toFixed(1)}V${(top + h).toFixed(1)}Z" style="fill:${scol(s)}"/>`);
        if (want && !stacked && fits(F(v), bw)) {
          P.push(`<text class="vlab${isCtx(s) ? " ctx" : ""}" x="${(x0 + bw / 2).toFixed(1)}" y="${(v >= 0 ? top - 6 : top + h + 13).toFixed(1)}" text-anchor="middle">${esc(F(v))}</text>`);
        }
      });
    });
    if (stack && want) {
      for (let i = 0; i < n; i++) {
        let p = 0, q = 0, any = false;
        for (const s of vis) { const v = s.values[i]; if (v !== null && Number.isFinite(v)) { any = true; if (v >= 0) p += v; else q += v; } }
        const x0 = m.left + band * i + (band - inner) / 2;
        if (any && fits(F(p + q), inner)) P.push(`<text class="vlab" x="${(x0 + inner / 2).toFixed(1)}" y="${(Y(p) - 6).toFixed(1)}" text-anchor="middle">${esc(F(p + q))}</text>`);
      }
    }
  } else if (type === "scatter") {
    vis.forEach((s) => {
      s.values.forEach((v, i) => {
        if (v !== null && Number.isFinite(v)) P.push(`<circle class="dot" data-i="${i}" cx="${X(i).toFixed(1)}" cy="${Y(v).toFixed(1)}" r="${narrow ? 4 : 5}" style="fill:${scol(s)}"/>`);
      });
      if (spec.trend) {
        const pts = s.values.flatMap((v, i) => v === null || !Number.isFinite(v) ? [] : [[xs[i] as number, v] as [number, number]]);
        const line = trendLine(pts);
        if (line && XAt) {
          const xl = Math.min(...pts.map((q) => q[0])), xh = Math.max(...pts.map((q) => q[0]));
          const XV = XAt;
          P.push(`<line class="trend" x1="${XV(xl).toFixed(1)}" y1="${Y(line.a + line.b * xl).toFixed(1)}" x2="${XV(xh).toFixed(1)}" y2="${Y(line.a + line.b * xh).toFixed(1)}" style="stroke:${scol(s)}"/>`);
        }
      }
    });
  } else {
    const base = new Array(n).fill(0);
    const layers = vis.map((s) => {
      const top = s.values.slice(0, n).map((v, i) => v === null || !Number.isFinite(v) ? null : stacked ? base[i] + v : v);
      const bot = stacked ? base.slice() : null;
      if (stacked) s.values.forEach((v, i) => { if (v !== null && Number.isFinite(v)) base[i] += v; });
      return { s, top, bot };
    });
    // Grey context lines first, so the highlighted one draws on top.
    const order = stacked ? layers : layers.slice().sort((a, b) => (isCtx(a.s) ? 0 : 1) - (isCtx(b.s) ? 0 : 1));
    order.forEach(({ s, top, bot }) => {
      const runs: number[][] = [];
      let cur: number[] = [];
      top.forEach((v, i) => { if (v === null) { if (cur.length) runs.push(cur); cur = []; } else cur.push(i); });
      if (cur.length) runs.push(cur);
      const c = scol(s);
      let gid: string | null = null;
      if (type === "area" || (vis.length <= 2 && !isCtx(s)) || (hl && !isCtx(s))) {
        gid = nid("g");
        const a = type === "area" ? (stacked ? 0.55 : 0.3) : 0.22;
        defs.push(`<linearGradient id="${gid}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" style="stop-color:${c};stop-opacity:${a}"/><stop offset="1" style="stop-color:${c};stop-opacity:${stacked ? a * 0.55 : 0}"/></linearGradient>`);
      }
      runs.forEach((r) => {
        const pts = r.map((i) => [X(i), Y(top[i]!)] as [number, number]);
        const d = smooth(pts);
        if (gid) {
          const under = bot
            ? smooth(r.slice().reverse().map((i) => [X(i), Y(bot[i])] as [number, number])).replace(/^M/, "L")
            : `L${pts[pts.length - 1][0].toFixed(1)},${zero.toFixed(1)}L${pts[0][0].toFixed(1)},${zero.toFixed(1)}`;
          P.push(`<path class="fill" d="${d}${under}Z" fill="url(#${gid})"/>`);
        }
        P.push(`<path class="line" d="${d}" fill="none" style="stroke:${c}" stroke-width="${isCtx(s) ? 1.5 : 2.25}" stroke-linejoin="round" stroke-linecap="round"/>`);
        if (r.length === 1) P.push(`<circle cx="${pts[0][0].toFixed(1)}" cy="${pts[0][1].toFixed(1)}" r="3" style="fill:${c}"/>`);
      });
      const li = top.length - 1;
      if (top[li] !== null && !isCtx(s)) P.push(`<circle class="end-dot" cx="${X(li).toFixed(1)}" cy="${Y(top[li]!).toFixed(1)}" r="3.5" style="fill:${c};stroke:var(--mk-surface)" stroke-width="2"/>`);
      if (focus !== null && top[focus] !== null) P.push(`<circle class="focus-dot" cx="${X(focus).toFixed(1)}" cy="${Y(top[focus]!).toFixed(1)}" r="4.5" style="fill:var(--mk-surface);stroke:${c}" stroke-width="2.25"/>`);
    });
    if (endLabels) {
      const labs = layers.flatMap((l) => {
        const at = last(l.s);
        return at < 0 || l.top[at] === null ? [] : [{ s: l.s, y: Y(l.top[at]!), v: l.s.values[at] }];
      });
      labs.sort((a, b) => a.y - b.y);
      for (let k = 1; k < labs.length; k++) if (labs[k].y - labs[k - 1].y < 15) labs[k].y = labs[k - 1].y + 15;
      const over = labs.length ? labs[labs.length - 1].y - (m.top + ph) : 0;
      if (over > 0) labs.forEach((l) => { l.y -= over; });
      labs.forEach((l) => P.push(`<text class="end" x="${m.left + pw + 10}" y="${(l.y + 4).toFixed(1)}" style="fill:${isCtx(l.s) ? "var(--mk-text-2)" : scol(l.s)}">${esc(l.s.name)} <tspan class="n">${esc(F(l.v))}</tspan></text>`));
    }
  }
  P.push(`<line class="base" x1="${m.left}" x2="${m.left + pw}" y1="${zero.toFixed(1)}" y2="${zero.toFixed(1)}"/>`);
  for (const a of spec.annotations ?? []) {
    const i = xs.indexOf(a.x);
    if (i < 0) continue;
    if (type === "scatter") {
      const s = vis.find((q) => q.values[i] !== null && q.values[i] !== undefined);
      const v = s?.values[i];
      if (!s || v === null || v === undefined) continue;
      const cx = X(i), cy = Y(v), left = cx > m.left + pw * 0.6;
      P.push(`<g class="ann"><circle cx="${cx.toFixed(1)}" cy="${cy.toFixed(1)}" r="9" fill="none" style="stroke:${scol(s)}" stroke-width="1.5"/><text x="${(left ? cx - 15 : cx + 15).toFixed(1)}" y="${(cy + 4).toFixed(1)}" text-anchor="${left ? "end" : "start"}" class="callout">${esc(a.label)}</text></g>`);
      continue;
    }
    const x = X(i), w = a.label.length * 6.2 + 16, lx = clamp(x - w / 2, m.left, Math.max(m.left, m.left + pw - w));
    P.push(`<g class="ann"><line x1="${x.toFixed(1)}" x2="${x.toFixed(1)}" y1="${m.top + 16}" y2="${m.top + ph}"/><rect x="${lx.toFixed(1)}" y="${m.top - 5}" width="${w.toFixed(1)}" height="20" rx="10"/><text x="${(lx + w / 2).toFixed(1)}" y="${m.top + 9}" text-anchor="middle">${esc(a.label)}</text></g>`);
  }

  const desc = `${TYPE[type]} of ${series.map((s) => s.name).join(", ")} over ${n} value${n === 1 ? "" : "s"} of ${spec.x.label || "x"}, from ${XF(xs[0])} to ${XF(xs[n - 1])}` +
    `; ${spec.y?.label || "values"} from ${F(yt.min)} to ${F(yt.max)}. Use the arrow keys to read each value.`;
  const svg = `<svg viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" role="img" tabindex="0" aria-labelledby="${id}-t" aria-describedby="${id}-d"><title id="${id}-t">${esc(title)}</title><desc id="${id}-d">${esc(desc)}</desc><defs>${defs.join("")}</defs>${P.join("")}</svg>`;

  const showLegend = o.legend === true || (o.legend !== false && series.length > 1 && !endLabels);
  const agg = (s: KitSeries) => type === "bar" ? s.values.reduce<number>((a, b) => a + (b ?? 0), 0) : s.values[last(s)] ?? null;
  const legend = showLegend
    ? `<ul class="mk-legend" aria-label="Series: press to show or hide">${series.map((s) => `<li><button type="button" aria-pressed="${!st.hidden.has(s.name)}" data-series="${esc(s.name)}"><span class="sw${type === "line" ? " line" : ""}" style="--c:${scol(s)}"></span>${esc(s.name)}${o.legendValues !== false ? ` <span class="v">${esc(F(agg(s)))}</span>` : ""}</button></li>`).join("")}</ul>`
    : "";

  let tip = "", live = "";
  if (focus !== null) {
    const tx = type === "bar" ? m.left + band * (focus + 1) + 8 : X(focus) + 14;
    const flip = tx + 196 > m.left + pw;
    const left = flip ? (type === "bar" ? m.left + band * focus - 8 : X(focus) - 14) : tx;
    const prev = focus > 0 ? focus - 1 : null;
    const pct = spec.format === "pct";
    const deltas = o.tipDelta !== false && prev !== null && type !== "scatter";
    const rows = vis.map((s) => ({ s, v: s.values[focus] ?? null, d: deltas ? change(s.values[focus] ?? null, s.values[prev!], pct) : null }));
    tip = `<div class="mk-tip" aria-hidden="true" style="left:${left.toFixed(1)}px;top:${m.top + (spec.annotations?.length ? 30 : 8)}px;${flip ? "transform:translateX(-100%)" : ""}"><div class="x">${esc(XF(xs[focus]))}${deltas ? `<span>vs ${esc(XF(xs[prev!]))}</span>` : ""}</div>` +
      rows.map((r) => `<div class="row" style="--c:${scol(r.s)}"><i></i>${esc(r.s.name)}<b>${esc(F(r.v))}</b>${deltas ? `<em>${esc(r.d ?? "")}</em>` : ""}</div>`).join("") + `</div>`;
    live = `${XF(xs[focus])}: ` + rows.map((r) => `${r.s.name} ${F(r.v)}${r.d ? ` (${r.d} vs ${XF(xs[prev!])})` : ""}`).join(", ");
  }
  const table = `<div class="${o.showTable ? "mk-data" : "mk-sr"}" id="${id}-data"><table><caption>${esc(title)} data</caption><thead><tr><th scope="col">${esc(spec.x.label || "x")}</th>${series.map((s) => `<th scope="col">${esc(s.name)}</th>`).join("")}</tr></thead><tbody>${xs.map((x, i) => `<tr><th scope="row">${esc(XF(x))}</th>${series.map((s) => `<td>${esc(F(s.values[i] ?? null))}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`;

  const focusSel = hadFocus ? (hadFocus.dataset?.series !== undefined ? `[data-series="${CSS.escape(hadFocus.dataset.series)}"]` : hadFocus.tagName.toLowerCase() === "svg" ? "figure svg" : null) : null;
  el.innerHTML = legend + `<figure>${svg}${tip}</figure><div class="mk-sr mk-live" aria-live="polite">${esc(live)}</div>` + table;
  if (focusSel) (el.querySelector(focusSel) as HTMLElement | SVGElement | null)?.focus({ preventScroll: true });
}

function schedule(el: Host) {
  const st = el._mk!;
  cancelAnimationFrame(st.raf);
  st.raf = requestAnimationFrame(() => render(el));
}

/** Draw `spec` into `el`, or redraw it with a new spec (keeping hidden series and the pin). */
export function chart(el: HTMLElement, spec: KitChartSpec, o: ChartOptions = {}): HTMLElement {
  const host = el as Host;
  const first = !host._mk;
  const pin = o.pin ?? spec.pin ?? null;
  host._mk = {
    id: host._mk?.id || nid("c"), spec, o, raf: host._mk?.raf ?? 0, observer: host._mk?.observer,
    hidden: new Set(o.hidden ?? host._mk?.hidden ?? []), focus: pin, width: el.clientWidth || host._mk?.width || 640,
  };
  // A legend toggle never hides every series.
  for (const name of [...host._mk.hidden]) if (!spec.series.some((s) => s.name === name)) host._mk.hidden.delete(name);
  el.classList.add("mk-chart");
  render(host);
  if (!first) return el;
  // Width changes (a split dragged, the window resized): redraw at the new width, never scale text.
  if ("ResizeObserver" in globalThis) {
    host._mk.observer = new ResizeObserver(() => {
      const w = el.clientWidth;
      if (w && host._mk && Math.abs(w - host._mk.width) > 2) { host._mk.width = w; schedule(host); }
    });
    host._mk.observer.observe(el);
  }
  el.addEventListener("pointermove", (e) => {
    const st = host._mk!;
    const svg = el.querySelector("figure svg");
    if (!svg || !st.idxAt) return;
    const r = svg.getBoundingClientRect();
    const scale = (Number(svg.getAttribute("width")) || r.width) / (r.width || 1);
    const i = st.idxAt((e.clientX - r.left) * scale, (e.clientY - r.top) * scale);
    const next = i ?? (st.o.pin ?? st.spec.pin ?? null);
    if (next !== st.focus) { st.focus = next; schedule(host); }
  });
  el.addEventListener("pointerleave", () => {
    const st = host._mk!;
    const pinned = st.o.pin ?? st.spec.pin ?? null;
    if (st.focus !== pinned) { st.focus = pinned; schedule(host); }
  });
  el.addEventListener("keydown", (e) => {
    if (!(e.target instanceof SVGElement)) return;
    const st = host._mk!;
    const n = st.spec.x.values.length, f = st.focus;
    const next = ({ ArrowRight: (f ?? -1) + 1, ArrowLeft: (f ?? n) - 1, Home: 0, End: n - 1, Escape: null } as Record<string, number | null>)[e.key];
    if (next === undefined) return;
    e.preventDefault();
    st.focus = next === null ? null : clamp(next, 0, n - 1);
    render(host);
  });
  el.addEventListener("focusout", (e) => {
    if (!(e.target instanceof SVGElement)) return;
    // A redraw replaces the focused SVG and puts focus back on the new one; only a real leave
    // (focus now outside the chart) drops the tooltip.
    setTimeout(() => {
      const st = host._mk!;
      if (el.contains(document.activeElement)) return;
      const pinned = st.o.pin ?? st.spec.pin ?? null;
      if (st.focus !== pinned) { st.focus = pinned; schedule(host); }
    }, 0);
  });
  el.addEventListener("click", (e) => {
    const b = (e.target as Element).closest?.("[data-series]") as HTMLElement | null;
    if (!b) return;
    const st = host._mk!;
    const name = b.dataset.series!;
    if (st.hidden.has(name)) st.hidden.delete(name);
    else st.hidden.add(name);
    if (st.hidden.size >= st.spec.series.length) st.hidden.delete(name);
    render(host);
  });
  return el;
}
