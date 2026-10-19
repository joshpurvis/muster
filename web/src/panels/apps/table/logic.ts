// `table` app: input checks, sorting and filtering. Pure, so deno tests cover it.
import { fits, Problems, type Result } from "../shared/validate.ts";

export type Cell = string | number | boolean | null;
export type Row = Record<string, Cell>;
export type Align = "left" | "right" | "center";
export interface Column { key: string; label: string; align?: Align; numeric: boolean }
export interface Sort { key: string; dir: "asc" | "desc" }
export interface TableSpec { title?: string; columns: Column[]; rows: Row[]; sort?: Sort }

export const MAX_ROWS = 5000;
export const MAX_COLUMNS = 50;

export function validateTable(data: unknown): Result<TableSpec> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["title", "columns", "rows", "sort"]);
  const title = p.string(o, "title", "", { max: 200 });
  const columns: Column[] = [];
  const keys = new Set<string>();
  p.array(o, "columns", "", { required: true, max: MAX_COLUMNS, min: 1 })?.forEach((c, i) => {
    const co = p.object(c, `columns[${i}]`);
    if (!co) return;
    p.known(co, `columns[${i}]`, ["key", "label", "align"]);
    const key = p.string(co, "key", `columns[${i}]`, { required: true, max: 200 });
    const label = p.string(co, "label", `columns[${i}]`, { max: 200 });
    const align = p.oneOf(co, "align", `columns[${i}]`, ["left", "right", "center"] as const);
    if (key === undefined) return;
    if (keys.has(key)) p.add(`columns[${i}].key`, `duplicates ${JSON.stringify(key)}`);
    keys.add(key);
    columns.push({ key, label: label ?? key, align, numeric: false });
  });
  const rows: Row[] = [];
  p.array(o, "rows", "", { required: true, max: MAX_ROWS })?.forEach((r, i) => {
    const ro = p.object(r, `rows[${i}]`);
    if (!ro) return;
    for (const [k, v] of Object.entries(ro)) {
      const ok = v === null || typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v));
      if (!ok) p.add(`rows[${i}].${k}`, "must be a string, finite number, boolean or null");
      else if (typeof v === "string" && !fits(v, 10_000)) p.add(`rows[${i}].${k}`, "is longer than 10000 characters");
    }
    rows.push(ro as Row);
  });
  let sort: Sort | undefined;
  if (o.sort !== undefined) {
    const so = p.object(o.sort, "sort");
    if (so) {
      p.known(so, "sort", ["key", "dir"]);
      const key = p.string(so, "key", "sort", { required: true, max: 200 });
      const dir = p.oneOf(so, "dir", "sort", ["asc", "desc"] as const) ?? "asc";
      if (key !== undefined && !keys.has(key)) p.add("sort.key", `is not a column key (${JSON.stringify(key)})`);
      else if (key !== undefined) sort = { key, dir };
    }
  }
  for (const c of columns) c.numeric = isNumericColumn(rows, c.key);
  return p.result({ title, columns, rows, sort });
}

/** A column is numeric when it has a value and every value present is a number. */
export function isNumericColumn(rows: Row[], key: string): boolean {
  let any = false;
  for (const r of rows) {
    const v = r[key];
    if (v === null || v === undefined || v === "") continue;
    if (typeof v !== "number") return false;
    any = true;
  }
  return any;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
const empty = (v: Cell | undefined) => v === null || v === undefined || v === "";

/** Compare two cells ascending: numbers numerically, text naturally ("a2" before "a10"), booleans false first. */
export function compareCells(a: Cell | undefined, b: Cell | undefined): number {
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (typeof a === "boolean" && typeof b === "boolean") return Number(a) - Number(b);
  if (typeof a === "number" && typeof b !== "number") return -1;
  if (typeof b === "number" && typeof a !== "number") return 1;
  return collator.compare(String(a), String(b));
}

/** Stable sort; empty cells always last whatever the direction. */
export function sortRows(rows: Row[], sort: Sort | undefined): Row[] {
  if (!sort) return rows.slice();
  const sign = sort.dir === "desc" ? -1 : 1;
  return rows.map((row, i) => ({ row, i })).sort((x, y) => {
    const a = x.row[sort.key], b = y.row[sort.key];
    if (empty(a) || empty(b)) return empty(a) === empty(b) ? x.i - y.i : empty(a) ? 1 : -1;
    return sign * compareCells(a, b) || x.i - y.i;
  }).map((x) => x.row);
}

/** Next sort after clicking `key`'s header: ascending first (descending for numbers), then flip. */
export function nextSort(current: Sort | undefined, key: string, numeric: boolean): Sort {
  if (current?.key === key) return { key, dir: current.dir === "asc" ? "desc" : "asc" };
  return { key, dir: numeric ? "desc" : "asc" };
}

export function cellText(v: Cell | undefined): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v);
}

/** Rows containing every word of `query` (case-insensitive) somewhere in their shown cells. */
export function filterRows(rows: Row[], columns: Column[], query: string): Row[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return rows;
  return rows.filter((r) => {
    const text = columns.map((c) => cellText(r[c.key]).toLowerCase()).join("\u0000");
    return words.every((w) => text.includes(w));
  });
}
