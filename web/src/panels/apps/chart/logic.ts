// `chart` app: input checks, scales and ticks. Pure, so deno tests cover it.
import { Problems, type Result } from "../shared/validate.ts";

export const TYPES = ["bar", "line", "scatter", "area"] as const;
export type ChartType = typeof TYPES[number];
export interface Series { name: string; values: (number | null)[] }
export interface ChartSpec {
  type: ChartType;
  title?: string;
  x: { label?: string; values: (string | number)[] };
  series: Series[];
  y: { label?: string; min?: number; max?: number };
}

export const MAX_POINTS = 2000;
export const MAX_SERIES = 12;

/** Series colours, in order. Each keeps 3:1 contrast on both the light and dark panel background. */
export const PALETTE = ["#2f7fd0", "#d0661a", "#2a9450", "#7f6ad8", "#c94f7c", "#18938f", "#b07d12", "#7a8296"];
export const color = (i: number) => PALETTE[i % PALETTE.length];

export function validateChart(data: unknown): Result<ChartSpec> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["type", "title", "x", "series", "y"]);
  const type = p.oneOf(o, "type", "", TYPES, true);
  const title = p.string(o, "title", "", { max: 200 });
  const xo = o.x === undefined ? (p.add("x", "is required"), undefined) : p.object(o.x, "x");
  let xValues: (string | number)[] = [];
  let xLabel: string | undefined;
  if (xo) {
    p.known(xo, "x", ["label", "values"]);
    xLabel = p.string(xo, "label", "x", { max: 200 });
    const values = p.array(xo, "values", "x", { required: true, max: MAX_POINTS, min: 1 });
    if (values) {
      values.forEach((v, i) => {
        if (typeof v === "number" ? !Number.isFinite(v) : typeof v !== "string" || v.length > 200) {
          p.add(`x.values[${i}]`, "must be a finite number or a string of at most 200 characters");
        }
      });
      xValues = values as (string | number)[];
      if (type === "scatter" && values.some((v) => typeof v !== "number")) p.add("x.values", "must all be numbers for a scatter chart");
    }
  }
  const series: Series[] = [];
  const list = p.array(o, "series", "", { required: true, max: MAX_SERIES, min: 1 });
  list?.forEach((s, i) => {
    const so = p.object(s, `series[${i}]`);
    if (!so) return;
    p.known(so, `series[${i}]`, ["name", "values"]);
    const name = p.string(so, "name", `series[${i}]`, { required: true, max: 100 });
    const values = p.array(so, "values", `series[${i}]`, { required: true, max: MAX_POINTS });
    if (!values) return;
    if (xo && values.length !== xValues.length) {
      p.add(`series[${i}].values`, `has ${values.length} values but x.values has ${xValues.length}`);
    }
    values.forEach((v, j) => {
      if (v !== null && (typeof v !== "number" || !Number.isFinite(v))) p.add(`series[${i}].values[${j}]`, "must be a finite number or null");
    });
    if (name !== undefined) series.push({ name, values: values as (number | null)[] });
  });
  const y: ChartSpec["y"] = {};
  if (o.y !== undefined) {
    const yo = p.object(o.y, "y");
    if (yo) {
      p.known(yo, "y", ["label", "min", "max"]);
      y.label = p.string(yo, "label", "y", { max: 200 });
      y.min = p.number(yo, "min", "y");
      y.max = p.number(yo, "max", "y");
      if (y.min !== undefined && y.max !== undefined && y.min >= y.max) p.add("y", "min must be less than max");
    }
  }
  return p.result({ type: type!, title, x: { label: xLabel, values: xValues }, series, y });
}

/** A "nice" step (1, 2 or 5 times a power of ten) near `rough`. */
export function niceStep(rough: number): number {
  if (!(rough > 0) || !Number.isFinite(rough)) return 1;
  const exp = Math.floor(Math.log10(rough));
  const f = rough / 10 ** exp;
  const nice = f < 1.5 ? 1 : f < 3 ? 2 : f < 7 ? 5 : 10;
  return nice * 10 ** exp;
}

/** Decimal places that show `step` exactly (0.25 → 2, 2.5 → 1, 500 → 0). */
export function decimals(step: number): number {
  for (let d = 0; d < 12; d++) {
    const x = step * 10 ** d;
    if (Math.abs(x - Math.round(x)) < 1e-9 * Math.max(1, x)) return d;
  }
  return 12;
}

/** Round, evenly spaced ticks covering [min, max] with about `count` intervals. */
export function niceTicks(min: number, max: number, count = 5): { min: number; max: number; step: number; ticks: number[] } {
  if (min > max) [min, max] = [max, min];
  if (min === max) {
    const pad = min === 0 ? 1 : Math.abs(min) * 0.1;
    min -= min === 0 ? 0 : pad;
    max += pad;
  }
  const step = niceStep((max - min) / Math.max(1, count));
  const d = decimals(step);
  const lo = Number((Math.floor(min / step + 1e-9) * step).toFixed(d));
  const hi = Number((Math.ceil(max / step - 1e-9) * step).toFixed(d));
  const ticks: number[] = [];
  for (let i = 0; i <= 1000; i++) {
    const v = Number((lo + i * step).toFixed(d));
    if (v > hi + step / 2) break;
    ticks.push(v === 0 ? 0 : v);
  }
  return { min: lo, max: hi, step, ticks };
}

/** Tick label: fixed decimals for the step, with k/M/B for large magnitudes. */
export function formatTick(v: number, step: number): string {
  const a = Math.abs(v);
  for (const [at, div, suffix] of [[1e9, 1e9, "B"], [1e6, 1e6, "M"], [1e4, 1e3, "k"]] as const) {
    if (a >= at && step >= div / 100) {
      return trim((v / div).toFixed(Math.min(2, decimals(step / div)))) + suffix;
    }
  }
  return trim(v.toFixed(decimals(step)));
}

/** Values in tooltips and the data table: as given, with float noise removed. */
export function formatValue(v: number | null): string {
  if (v === null) return "–";
  return String(Number(v.toPrecision(12)));
}

const trim = (s: string) => s.includes(".") ? s.replace(/\.?0+$/, "") : s;

export function linear(d0: number, d1: number, r0: number, r1: number): (v: number) => number {
  const span = d1 - d0 || 1;
  return (v) => r0 + ((v - d0) / span) * (r1 - r0);
}

/** The y range to draw: data (with 0 for bars and areas), widened to nice ticks, then any fixed y.min/max. */
export function yScale(spec: ChartSpec, count = 5) {
  const all = spec.series.flatMap((s) => s.values.filter((v): v is number => v !== null));
  let lo = all.length ? Math.min(...all) : 0;
  let hi = all.length ? Math.max(...all) : 1;
  if (spec.type === "bar" || spec.type === "area") { lo = Math.min(lo, 0); hi = Math.max(hi, 0); }
  if (spec.y.min !== undefined) lo = spec.y.min;
  if (spec.y.max !== undefined) hi = spec.y.max;
  const t = niceTicks(lo, hi, count);
  const min = spec.y.min ?? t.min;
  const max = spec.y.max ?? t.max;
  return { min, max, step: t.step, ticks: t.ticks.filter((v) => v >= min - 1e-9 && v <= max + 1e-9) };
}

/** Numeric x (scatter, or line/area with only numbers) uses a linear axis; anything else is categorical. */
export function numericX(spec: ChartSpec): boolean {
  return spec.type === "scatter" || (spec.type !== "bar" && spec.x.values.every((v) => typeof v === "number"));
}

/** Show every n-th category label so labels about `labelWidth` px wide don't overlap. */
export function labelEvery(count: number, width: number, labelWidth: number): number {
  if (count <= 0 || width <= 0) return 1;
  const fit = Math.max(1, Math.floor(width / Math.max(1, labelWidth)));
  return Math.max(1, Math.ceil(count / fit));
}

/** Relative luminance contrast of two #rrggbb colours (WCAG 2). */
export function contrast(a: string, b: string): number {
  const lum = (hex: string) => {
    const [r, g, bl] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
      .map((c) => c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [x, y] = [lum(a), lum(b)].sort((m, n) => n - m);
  return (x + 0.05) / (y + 0.05);
}
