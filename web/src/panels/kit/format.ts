// Muster Kit: numbers, ticks and small helpers. Pure (no DOM), so deno tests cover them. Used by
// the chart renderer, the Board, every built-in panel and kit.js (window.mk.fmt).

export type Format = "int" | "pct" | "usd" | "ms" | "s" | "plain";
export const FORMATS: readonly Format[] = ["int", "pct", "usd", "ms", "s", "plain"];

/** HTML-escape text for an element or a double- or single-quoted attribute. */
export const esc = (s: unknown) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));

/** Escaped text where **word** is bold and `code` is code: enough for a takeaway sentence. */
export const rich = (s: unknown) =>
  esc(s).replace(/\*\*([^*\n]{1,200})\*\*/g, "<b>$1</b>").replace(/`([^`\n]{1,200})`/g, "<code>$1</code>");

export const clamp =(v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

/** A finite number from `v` (a number or a numeric string) kept within [lo, hi], else
 * `fallback`. Every number the kit writes into markup goes through it or arithmetic, so JSON
 * from outside (a <mk-ring> spec, a mk.* call) can never carry markup into an attribute. */
export function num(v: unknown, fallback: number, lo = -Infinity, hi = Infinity): number {
  const n = typeof v === "number" ? v : typeof v === "string" && v.trim() !== "" ? Number(v) : NaN;
  return Number.isFinite(n) ? clamp(n, lo, hi) : fallback;
}

/** A true minus sign, and never "-0". */
export const minus = (s: string) => s === "-0" ? "0" : s.replace(/^-(?=[\d$.])/, "−");

export const lin = (d0: number, d1: number, r0: number, r1: number) => (v: number) => r0 + ((v - d0) / (d1 - d0 || 1)) * (r1 - r0);

const UNITS: [number, string][] = [[1e12, "T"], [1e9, "B"], [1e6, "M"], [1e3, "k"]];

/** Up to `max` decimals, trailing zeros dropped, with thousands separators. */
function grouped(v: number, max: number, min = 0): string {
  return v.toLocaleString("en-US", { maximumFractionDigits: max, minimumFractionDigits: Math.min(min, max) });
}

/** Small numbers keep two significant digits instead of rounding to 0. */
function small(a: number, extra: number): number {
  if (a === 0 || a >= 1) return (a >= 100 ? 0 : a >= 10 ? 1 : 2) + extra;
  return Math.min(8, Math.max(2, 1 - Math.floor(Math.log10(a))) + extra);
}

/**
 * Compact: 1,234 · 12.5k · 3.4M · 2.5B · 1.2T. Past 10 000 a unit is used; `extra` adds decimals
 * (axis ticks use it so neighbouring ticks never print the same).
 */
export function compact(v: number, extra = 0): string {
  const a = Math.abs(v);
  // Past a thousand trillion, or too small for a few decimals: scientific notation.
  if (a >= 1e15 || (a > 0 && a < 1e-4)) return minus(v.toExponential(Math.min(4, 1 + extra)).replace(/\.?0+e/, "e").replace("e+", "e"));
  if (a >= 1e4) {
    let i = UNITS.findIndex(([d]) => a >= d);
    let n = a / UNITS[i][0];
    let digits = (n >= 100 ? 0 : 1) + extra;
    // 999.96k rounds to "1000k": use the next unit up.
    if (Number(n.toFixed(digits)) >= 1000 && i > 0) {
      i--;
      n = a / UNITS[i][0];
      digits = 1 + extra;
    }
    return minus((v < 0 ? "-" : "") + trimZeros(n.toFixed(digits), extra) + UNITS[i][1]);
  }
  return minus((v < 0 ? "-" : "") + grouped(a, small(a, extra)));
}

/** "2.50" → "2.5", "3.0" → "3", unless extra decimals were asked for (then keep them). */
function trimZeros(s: string, extra: number): string {
  if (!s.includes(".") || extra > 0) return s;
  return s.replace(/\.?0+$/, "");
}

/** Seconds as a clock-friendly duration: 45 s, 4.6 s, 1m 54s, 2m, 1h 05m. */
function seconds(v: number): string {
  const a = Math.abs(v);
  const sign = v < 0 ? "−" : "";
  if (a < 60) return sign + `${Number((Math.round(a * 10) / 10).toFixed(1))} s`;
  const total = Math.round(a);
  const h = Math.floor(total / 3600), m = Math.floor((total % 3600) / 60), s = total % 60;
  if (h) return sign + `${h}h${m ? ` ${String(m).padStart(2, "0")}m` : ""}`;
  return sign + `${m}m${s ? ` ${String(s).padStart(2, "0")}s` : ""}`;
}

/** The kit's number formatting. `extra` adds decimals where a format has them (axis ticks). */
export function fmt(v: unknown, f?: string, extra = 0): string {
  if (v === null || v === undefined || (typeof v === "number" && !Number.isFinite(v))) return "–";
  if (typeof v !== "number") return String(v);
  const a = Math.abs(v);
  switch (f) {
    case "plain": return minus(String(Number(v.toFixed(Math.min(6, 2 + extra)))));
    case "int": return compact(Math.round(v), a >= 1e4 ? extra : 0);
    case "usd": {
      const sign = v < 0 ? "−" : "";
      if (a >= 1e4) return sign + "$" + compact(a, extra);
      const cents = a < 100 && Math.round(a * 100) % 100 !== 0;
      return sign + "$" + grouped(a, cents ? 2 : extra, cents ? 2 : 0);
    }
    // Values are already percents: 0.34 means 0.34 %.
    case "pct": return compact(v, extra) + "%";
    case "ms":
      if (a >= 60_000) return seconds(v / 1000);
      if (a >= 1000) return minus((v / 1000).toFixed(a >= 1e4 ? extra : 1 + extra)) + " s";
      return minus(a < 10 && a !== Math.round(a) ? String(Number(v.toFixed(1 + extra))) : v.toFixed(extra)) + " ms";
    case "s": return a < 60 && extra ? minus(v.toFixed(extra)) + " s" : seconds(v);
    default: return compact(v, extra);
  }
}

/** A number in format `f` written out in full (no k/M/B, durations in their own unit), for a
 * data table where neighbouring values must stay apart: 12,345 · $1,234,567.89 · 0.3412% · 61,250 ms. */
export function fmtFull(v: unknown, f?: string): string {
  if (v === null || v === undefined || (typeof v === "number" && !Number.isFinite(v))) return "–";
  if (typeof v !== "number") return String(v);
  const a = Math.abs(v), sign = v < 0 ? "−" : "";
  switch (f) {
    case "plain": return minus(String(Number(v.toFixed(6))));
    case "int": return sign + grouped(Math.abs(Math.round(v)), 0);
    case "usd": { const cents = Math.round(a * 100) % 100 !== 0; return sign + "$" + grouped(a, cents ? 2 : 0, cents ? 2 : 0); }
    case "pct": return sign + grouped(a, 6) + "%";
    case "ms": return sign + grouped(a, 3) + " ms";
    case "s": return sign + grouped(a, 3) + " s";
    default: return sign + grouped(a, a !== 0 && a < 1 ? 8 : 6);
  }
}

/** Decimal places that show `step` exactly (0.25 → 2, 2.5 → 1, 500 → 0). */
export function decimals(step: number): number {
  if (!(step > 0) || !Number.isFinite(step)) return 0;
  for (let d = 0; d <= 20; d++) {
    const x = step * 10 ** d;
    if (Math.round(x) !== 0 && Math.abs(x - Math.round(x)) <= 1e-6 * x) return d;
  }
  return 20;
}

/** Clock steps for durations in seconds: 15 s, 30 s, 1 m, 2 m, 5 m … 1 day. */
const CLOCK = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 14400, 21600, 43200, 86400];

export interface Ticks { min: number; max: number; step: number; ticks: number[] }

/** Round, evenly spaced ticks covering [lo, hi] in about `n` steps. `f` "s" lands on clock steps. */
export function niceTicks(lo: number, hi: number, n = 4, f?: string): Ticks {
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) { lo = 0; hi = 1; }
  if (lo > hi) [lo, hi] = [hi, lo];
  if (lo === hi) {
    const pad = Math.abs(lo) * 0.1 || 1;
    lo = lo >= 0 && lo - pad < 0 ? 0 : lo - pad;
    hi += pad;
  }
  const raw = (hi - lo) / Math.max(1, n);
  let step: number;
  if (f === "s" && raw >= 1) step = CLOCK.find((s) => s >= raw) ?? 86400 * Math.ceil(raw / 86400);
  else {
    const p = 10 ** Math.floor(Math.log10(raw));
    // Whole numbers ("int") get whole steps: never 0.25 or 2.5, which would print as repeats.
    const whole = f === "int";
    step = [1, 2, 2.5, 5, 10].map((m) => m * p).filter((s) => !whole || (s >= 1 && Number.isInteger(s)))
      .find((s) => s >= raw * (1 - 1e-9)) ?? (whole ? Math.max(1, 10 * p) : 10 * p);
  }
  const d = Math.min(20, decimals(step));
  const min = Number((Math.floor(lo / step + 1e-9) * step).toFixed(d));
  const max = Number((Math.ceil(hi / step - 1e-9) * step).toFixed(d));
  const ticks: number[] = [];
  for (let i = 0; i <= 1000; i++) {
    const t = Number((min + i * step).toFixed(d));
    if (t > max + step / 2) break;
    ticks.push(t === 0 ? 0 : t);
  }
  // Float precision ran out (ticks not strictly increasing): fall back to the bare range.
  if (ticks.length < 2 || ticks.some((t, i) => i > 0 && t <= ticks[i - 1]) || !(max > min)) {
    return { min: lo, max: hi, step: hi - lo, ticks: [lo, hi] };
  }
  return { min, max, step, ticks };
}

/** Tick labels in format `f`, with just enough decimals that no two neighbours print the same. */
export function tickLabels(ticks: number[], f?: string): string[] {
  const step = ticks.length > 1 ? Math.abs(ticks[1] - ticks[0]) : 0;
  for (let extra = 0; extra <= 3; extra++) {
    const labels = ticks.map((t) => fmt(t, f, extra));
    const distinct = labels.every((l, i) => i === 0 || l !== labels[i - 1]);
    // Each label must also say its own tick (not 1.3M for 1.25M); clock labels are exact by construction.
    const exact = f === "s" || f === "ms" || labels.every((l, i) => {
      const back = parseLabel(l);
      return back === null || Math.abs(back - ticks[i]) <= step * 1e-6 + Math.abs(ticks[i]) * 1e-12;
    });
    if (distinct && exact) return labels;
  }
  // Still repeats (fixed fractional bounds on an "int" axis): show the decimals they need.
  return ticks.map((t) => fmt(t, f === "int" && !Number.isInteger(t) ? undefined : f, 3));
}

/** The number a compact label stands for ("1.25M" → 1250000, "−$4.5k" → -4500), or null. */
export function parseLabel(label: string): number | null {
  const m = /^(−|-)?\$?([\d,]*\.?\d+(?:e-?\d+)?)([kMBT])?%?$/.exec(label.trim());
  if (!m) return null;
  const n = Number(m[2].replace(/,/g, "")) * ({ k: 1e3, M: 1e6, B: 1e9, T: 1e12 }[m[3] as "k"] ?? 1);
  return m[1] ? -n : n;
}

/** Monotone cubic path (Fritsch–Carlson): smooth, and it never overshoots the data. */
export function smooth(pts: [number, number][]): string {
  const p = (x: number, y: number) => `${x.toFixed(1)},${y.toFixed(1)}`;
  if (pts.length < 3) return pts.map((q, i) => (i ? "L" : "M") + p(q[0], q[1])).join("");
  const n = pts.length, dx: number[] = [], m: number[] = [], t: number[] = [];
  for (let i = 0; i < n - 1; i++) {
    dx[i] = pts[i + 1][0] - pts[i][0];
    m[i] = (pts[i + 1][1] - pts[i][1]) / (dx[i] || 1);
  }
  t[0] = m[0];
  t[n - 1] = m[n - 2];
  for (let i = 1; i < n - 1; i++) {
    t[i] = m[i - 1] * m[i] <= 0 ? 0 : (3 * (dx[i - 1] + dx[i])) / ((2 * dx[i] + dx[i - 1]) / m[i - 1] + (dx[i] + 2 * dx[i - 1]) / m[i]);
  }
  let d = "M" + p(pts[0][0], pts[0][1]);
  for (let i = 0; i < n - 1; i++) {
    const h = dx[i] / 3;
    d += `C${p(pts[i][0] + h, pts[i][1] + h * t[i])} ${p(pts[i + 1][0] - h, pts[i + 1][1] - h * t[i + 1])} ${p(pts[i + 1][0], pts[i + 1][1])}`;
  }
  return d;
}

/** Least-squares line through points: y = a + b·x. Null when x does not vary. */
export function trendLine(pts: [number, number][]): { a: number; b: number } | null {
  if (pts.length < 2) return null;
  const mx = pts.reduce((s, q) => s + q[0], 0) / pts.length;
  const my = pts.reduce((s, q) => s + q[1], 0) / pts.length;
  const sxx = pts.reduce((s, q) => s + (q[0] - mx) ** 2, 0);
  if (!sxx) return null;
  const b = pts.reduce((s, q) => s + (q[0] - mx) * (q[1] - my), 0) / sxx;
  return { a: my - b * mx, b };
}

/** A colour an agent may give: a kit token (var(--mk-…)) or a hex colour. Anything else is refused. */
export const COLOR = /^(#[0-9a-fA-F]{3}|#[0-9a-fA-F]{6}|var\(--mk-[a-z0-9-]{1,30}\))$/;
export const safeColor = (c: unknown, fallback: string) => typeof c === "string" && COLOR.test(c) ? c : fallback;
