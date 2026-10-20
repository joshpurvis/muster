// Muster Kit: sparkline, ring (donut), meter and stat tile, as HTML strings. All text goes
// through esc(); colours through safeColor().
import { clamp, esc, fmt, lin, safeColor, smooth } from "./format.ts";

let uid = 0;
export const nid = (p: string) => `mk-${p}${++uid}`;
/** The n-th categorical series colour (8 hues, then they repeat). */
export const seriesColor = (i: number) => `var(--mk-c${(i % 8) + 1})`;

export interface SparkOptions { width?: number; height?: number; color?: string; area?: boolean }

/** A sparkline; fewer than two numbers draw nothing. */
export function spark(values: (number | null)[], o: SparkOptions = {}): string {
  const w = o.width || 120, h = o.height || 32, c = safeColor(o.color, "var(--mk-accent)"), pad = 3;
  const pts: [number, number][] = [];
  const v = values.filter((x): x is number => x !== null && Number.isFinite(x));
  if (v.length < 2) return "";
  const lo = Math.min(...v), hi = Math.max(...v);
  const X = lin(0, values.length - 1, pad, w - pad), Y = lin(lo, hi === lo ? lo + 1 : hi, h - pad, pad);
  values.forEach((x, i) => { if (x !== null && Number.isFinite(x)) pts.push([X(i), Y(x)]); });
  const d = smooth(pts), g = nid("sg"), last = pts[pts.length - 1];
  return `<svg class="spark" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true">` +
    (o.area !== false ? `<defs><linearGradient id="${g}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" style="stop-color:${c};stop-opacity:.26"/><stop offset="1" style="stop-color:${c};stop-opacity:0"/></linearGradient></defs><path d="${d}L${last[0].toFixed(1)},${h}L${pts[0][0].toFixed(1)},${h}Z" fill="url(#${g})"/>` : "") +
    `<path d="${d}" fill="none" style="stroke:${c}" stroke-width="1.75" stroke-linecap="round"/><circle cx="${last[0].toFixed(1)}" cy="${last[1].toFixed(1)}" r="3" style="fill:${c};stroke:var(--mk-surface)" stroke-width="1.5"/></svg>`;
}

export interface RingSegment { label: string; value: number; color?: string }
export interface RingOptions { size?: number; thickness?: number; center?: string; centerColor?: string; centerSize?: number; sub?: string; label?: string }

/** Arc lengths for a ring: any non-zero segment keeps at least `minLen`, taken from the largest. */
export function ringLengths(values: number[], circumference: number, minLen: number): number[] {
  const total = values.reduce((a, b) => a + Math.max(0, b), 0);
  if (!total) return values.map(() => 0);
  const raw = values.map((v) => (Math.max(0, v) / total) * circumference);
  const lens = raw.map((l, i) => values[i] > 0 ? Math.max(minLen, l) : 0);
  const extra = lens.reduce((a, b) => a + b, 0) - circumference;
  const big = raw.indexOf(Math.max(...raw));
  lens[big] = Math.max(minLen, lens[big] - Math.max(0, extra));
  return lens;
}

export function ring(segs: RingSegment[], o: RingOptions = {}): string {
  const s = o.size || 132, th = o.thickness || 12, r = (s - th) / 2, C = 2 * Math.PI * r;
  const gap = segs.filter((g) => g.value > 0).length > 1 ? 3 : 0;
  const lens = ringLengths(segs.map((g) => g.value), C, 4 + gap);
  let at = 0;
  const arcs = segs.map((g, i) => {
    const len = Math.max(0, lens[i] - gap);
    const arc = len
      ? `<circle cx="${s / 2}" cy="${s / 2}" r="${r}" fill="none" style="stroke:${safeColor(g.color, seriesColor(i))}" stroke-width="${th}" stroke-dasharray="${len.toFixed(2)} ${(C - len).toFixed(2)}" stroke-dashoffset="${(-at).toFixed(2)}" transform="rotate(-90 ${s / 2} ${s / 2})"/>`
      : "";
    at += lens[i];
    return arc;
  }).join("");
  const label = o.label || segs.map((g) => `${g.label}: ${g.value}`).join(", ");
  return `<svg width="${s}" height="${s}" viewBox="0 0 ${s} ${s}" role="img" aria-label="${esc(label)}">` +
    `<circle cx="${s / 2}" cy="${s / 2}" r="${r}" fill="none" style="stroke:var(--mk-sunken)" stroke-width="${th}"/>${arcs}` +
    (o.center ? `<text x="50%" y="${s / 2 + (o.sub ? 4 : 9)}" text-anchor="middle" class="ring-center" style="fill:${safeColor(o.centerColor, "var(--mk-text)")};font-size:${clamp(o.centerSize || 28, 11, 60)}px">${esc(o.center)}</text>` : "") +
    (o.sub ? `<text x="50%" y="${s / 2 + 22}" text-anchor="middle" class="ring-sub">${esc(o.sub)}</text>` : "") + `</svg>`;
}

export interface MeterOptions {
  label: string; value: number; max?: number; text?: string; of?: string; mark?: number; markLabel?: string;
  color?: string; tone?: string; format?: string;
}

export function meter(o: MeterOptions): string {
  const max = o.max && o.max > 0 ? o.max : 100;
  const pct = clamp((o.value / max) * 100, 0, 100);
  const color = o.tone && ["good", "warn", "bad", "info", "accent"].includes(o.tone) ? `var(--mk-${o.tone})` : safeColor(o.color, "var(--mk-accent)");
  const hasMark = typeof o.mark === "number" && Number.isFinite(o.mark);
  const mark = hasMark ? `<span class="mark" style="left:${clamp((o.mark! / max) * 100, 0, 100).toFixed(2)}%"></span>` : "";
  const desc = hasMark ? (o.markLabel || `mark at ${fmt(o.mark, o.format)}`) : "";
  return `<div class="mk-meter" role="meter" aria-valuenow="${o.value}" aria-valuemin="0" aria-valuemax="${max}" aria-label="${esc(o.label)}"${desc ? ` aria-description="${esc(desc)}" title="${esc(desc)}"` : ""}>` +
    `<div class="top"><span>${esc(o.label)}</span><span><b>${esc(o.text ?? fmt(o.value, o.format))}</b>${o.of ? ` <span>${esc(o.of)}</span>` : ""}</span></div>` +
    `<div class="track"><span class="fill" style="width:${pct.toFixed(2)}%;--c:${color}"></span>${mark}</div></div>`;
}

export interface StatOptions {
  label: string; value: string | number; unit?: string; delta?: string; tone?: string; note?: string;
  trend?: number[]; color?: string; trendWidth?: number;
}

/** `tone` says whether the change is good news (good, bad or flat); a minus sign alone says nothing. */
export function stat(o: StatOptions, tag = "div"): string {
  const tone = ({ good: "up", bad: "down", up: "up", down: "down", flat: "flat" } as Record<string, string>)[o.tone ?? ""] || (o.delta ? "flat" : "");
  const color = o.color ? safeColor(o.color, "") : "";
  const label = `<span class="label">${color ? `<i style="background:${color}"></i>` : ""}${esc(o.label)}</span>`;
  const value = `<span class="value">${esc(o.value)}${o.unit ? `<small>${esc(o.unit)}</small>` : ""}</span>`;
  const meta = o.delta || o.note
    ? `<span class="meta">${o.delta ? `<span class="mk-delta ${tone}">${esc(o.delta)}</span>` : ""}${o.note ? `<span>${esc(o.note)}</span>` : ""}</span>`
    : "";
  const trend = o.trend?.length ? spark(o.trend, { width: o.trendWidth || 140, height: 28, color: color || undefined }) : "";
  return `<${tag} class="mk-stat">${label}${value}${meta}${trend}</${tag}>`;
}
