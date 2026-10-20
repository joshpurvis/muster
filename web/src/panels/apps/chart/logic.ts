// `chart` app: input checks. Pure, so deno tests cover it. The same spec is drawn by the kit's
// chart renderer (kit/chart.ts) here, in pr_decision `media`, in Board chart blocks and in
// <mk-chart>. Every field added for the Briefing look is optional: older specs still work.
import { fits, Problems, type Result } from "../shared/validate.ts";
import { COLOR, FORMATS, type Format } from "../../kit/format.ts";
import type { KitChartSpec } from "../../kit/chart.ts";

export const TYPES = ["bar", "line", "scatter", "area"] as const;
export type ChartType = typeof TYPES[number];
export const LABELS = ["auto", "values", "end", "none"] as const;
export const TONES = ["good", "bad", "flat"] as const;
export interface Series { name: string; values: (number | null)[]; color?: string }
/** A headline number above the chart (the same shape as a Board stats item). */
export interface Stat { label: string; value: string | number; unit?: string; delta?: string; tone?: typeof TONES[number]; note?: string; trend?: number[]; color?: string }
export interface ChartSpec extends KitChartSpec {
  type: ChartType;
  title?: string;
  x: { label?: string; values: (string | number)[] };
  series: Series[];
  y: { label?: string; min?: number; max?: number };
  takeaway?: string;
  scope?: string;
  source?: string;
  format?: Format;
  xFormat?: Format;
  stats?: Stat[];
}

export const MAX_POINTS = 2000;
export const MAX_SERIES = 12;
export const MAX_ANNOTATIONS = 20;
export const MAX_STATS = 4;

/** A stats item (chart `stats`, Board `stats` blocks); `at` is its path in errors. */
export function validateStat(p: Problems, v: unknown, at: string): Stat | undefined {
  const o = p.object(v, at);
  if (!o) return undefined;
  p.known(o, at, ["label", "value", "unit", "delta", "tone", "note", "trend", "color"]);
  const label = p.string(o, "label", at, { required: true, max: 100 });
  let value: string | number | undefined;
  if (o.value === undefined || o.value === null) p.add(`${at}.value`, "is required");
  else if (typeof o.value === "number" && Number.isFinite(o.value)) value = o.value;
  else if (typeof o.value === "string" && fits(o.value, 40)) value = o.value;
  else p.add(`${at}.value`, "must be a number or a string of at most 40 characters");
  const unit = p.string(o, "unit", at, { max: 20 });
  const delta = p.string(o, "delta", at, { max: 20 });
  const tone = p.oneOf(o, "tone", at, TONES);
  const note = p.string(o, "note", at, { max: 100 });
  const color = color_(p, o, at);
  const raw = p.array(o, "trend", at, { max: 200 });
  let trend: number[] | undefined;
  if (raw) {
    if (raw.every((n) => typeof n === "number" && Number.isFinite(n))) trend = raw as number[];
    else p.add(`${at}.trend`, "must be an array of finite numbers");
  }
  if (label === undefined || value === undefined) return undefined;
  return { label, value, unit, delta, tone, note, trend, color };
}

/** An optional colour: a kit token (var(--mk-…)) or a hex colour. */
export function color_(p: Problems, o: Record<string, unknown>, at: string): string | undefined {
  const c = p.string(o, "color", at, { max: 40 });
  if (c !== undefined && !COLOR.test(c)) { p.add(`${at ? at + "." : ""}color`, "must be a kit colour token such as var(--mk-c2) or a hex colour such as #6a56e6"); return undefined; }
  return c;
}

export function validateChart(data: unknown): Result<ChartSpec> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["type", "title", "x", "series", "y", "takeaway", "scope", "source", "format", "xFormat", "highlight", "annotations",
    "stacked", "labels", "trend", "empty", "stats", "pin"]);
  const type = p.oneOf(o, "type", "", TYPES, true);
  const title = p.string(o, "title", "", { max: 200 });
  const xo = o.x === undefined ? (p.add("x", "is required"), undefined) : p.object(o.x, "x");
  let xValues: (string | number)[] = [];
  let xLabel: string | undefined;
  if (xo) {
    p.known(xo, "x", ["label", "values"]);
    xLabel = p.string(xo, "label", "x", { max: 200 });
    const values = p.array(xo, "values", "x", { required: true, max: MAX_POINTS });
    if (values) {
      values.forEach((v, i) => {
        if (typeof v === "number" ? !Number.isFinite(v) : typeof v !== "string" || !fits(v, 200)) {
          p.add(`x.values[${i}]`, "must be a finite number or a string of at most 200 characters");
        }
      });
      xValues = values as (string | number)[];
      if (type === "scatter" && values.some((v) => typeof v !== "number")) p.add("x.values", "must all be numbers for a scatter chart");
    }
  }
  const series: Series[] = [];
  const list = p.array(o, "series", "", { required: true, max: MAX_SERIES });
  const names = new Set<string>();
  list?.forEach((s, i) => {
    const so = p.object(s, `series[${i}]`);
    if (!so) return;
    p.known(so, `series[${i}]`, ["name", "values", "color"]);
    const name = p.string(so, "name", `series[${i}]`, { required: true, max: 100 });
    const color = color_(p, so, `series[${i}]`);
    const values = p.array(so, "values", `series[${i}]`, { required: true, max: MAX_POINTS });
    if (!values) return;
    if (xo && values.length !== xValues.length) {
      p.add(`series[${i}].values`, `has ${values.length} values but x.values has ${xValues.length}`);
    }
    values.forEach((v, j) => {
      if (v !== null && (typeof v !== "number" || !Number.isFinite(v))) p.add(`series[${i}].values[${j}]`, "must be a finite number or null");
    });
    if (name !== undefined) {
      if (names.has(name)) p.warnings.push(`series[${i}].name: ${JSON.stringify(name)} is used twice`);
      names.add(name);
      series.push({ name, values: values as (number | null)[], color });
    }
  });
  const y: ChartSpec["y"] = {};
  if (o.y !== undefined && o.y !== null) {
    const yo = p.object(o.y, "y");
    if (yo) {
      p.known(yo, "y", ["label", "min", "max"]);
      y.label = p.string(yo, "label", "y", { max: 200 });
      y.min = p.number(yo, "min", "y");
      y.max = p.number(yo, "max", "y");
      if (y.min !== undefined && y.max !== undefined && y.min >= y.max) p.add("y", "min must be less than max");
    }
  }
  const takeaway = p.string(o, "takeaway", "", { max: 600 });
  const scope = p.string(o, "scope", "", { max: 200 });
  const source = p.string(o, "source", "", { max: 300 });
  const format = p.oneOf(o, "format", "", FORMATS);
  const xFormat = p.oneOf(o, "xFormat", "", FORMATS);
  const highlight = p.string(o, "highlight", "", { max: 100 });
  if (highlight !== undefined && list && !names.has(highlight)) p.add("highlight", `must be the name of a series (got ${JSON.stringify(highlight).slice(0, 60)})`);
  const annotations: { x: string | number; label: string }[] = [];
  p.array(o, "annotations", "", { max: MAX_ANNOTATIONS })?.forEach((a, i) => {
    const ao = p.object(a, `annotations[${i}]`);
    if (!ao) return;
    p.known(ao, `annotations[${i}]`, ["x", "label"]);
    const label = p.string(ao, "label", `annotations[${i}]`, { required: true, max: 60 });
    const x = ao.x;
    if (x === undefined || x === null) p.add(`annotations[${i}].x`, "is required");
    else if (!xValues.includes(x as string | number)) p.add(`annotations[${i}].x`, `must be one of x.values (got ${JSON.stringify(x).slice(0, 60)})`);
    else if (label !== undefined) annotations.push({ x: x as string | number, label });
  });
  const stacked = p.boolean(o, "stacked", "");
  if (stacked && type && type !== "bar" && type !== "area") p.add("stacked", "applies to bar and area charts only");
  const labels = p.oneOf(o, "labels", "", LABELS);
  const trend = p.boolean(o, "trend", "");
  if (trend && type && type !== "scatter") p.add("trend", "applies to scatter charts only");
  const empty = p.string(o, "empty", "", { max: 200 });
  const stats: Stat[] = [];
  p.array(o, "stats", "", { max: MAX_STATS })?.forEach((s, i) => { const v = validateStat(p, s, `stats[${i}]`); if (v) stats.push(v); });
  const pin = p.number(o, "pin", "");
  if (pin !== undefined && (!Number.isInteger(pin) || pin < 0 || pin >= xValues.length)) p.add("pin", "must be the index of an x value");
  return p.result({
    type: type!, title, x: { label: xLabel, values: xValues }, series, y, takeaway, scope, source, format, xFormat, highlight,
    annotations: annotations.length ? annotations : undefined, stacked, labels, trend, empty, stats: stats.length ? stats : undefined, pin,
  });
}
