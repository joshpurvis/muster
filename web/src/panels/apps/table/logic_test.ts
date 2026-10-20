import { columnMax, compareCells, filterRows, isNumericColumn, nextSort, sortRows, validateTable } from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

const rows = [
  { name: "b10", size: 3, ok: true },
  { name: "b2", size: null, ok: false },
  { name: "A1", size: 12, ok: true },
  { name: "", size: 3, ok: null },
];
const columns = [{ key: "name", label: "Name", numeric: false }, { key: "size", label: "Size", numeric: true }];

Deno.test("table input: valid specs, labels default to keys, numeric columns detected", () => {
  const r = validateTable({ columns: [{ key: "name" }, { key: "size", label: "Size", align: "right" }], rows, sort: { key: "size", dir: "desc" } });
  check(r.ok);
  eq(r.value.columns.map((c) => [c.label, c.numeric]), [["name", false], ["Size", true]]);
  eq(r.value.sort, { key: "size", dir: "desc" });
  check(!isNumericColumn([{ a: null }], "a"));
  check(!isNumericColumn([{ a: 1 }, { a: "2" }], "a"));
});

Deno.test("table input: errors name the path", () => {
  const e = errorsOf(validateTable({ columns: [{ key: "a" }, { key: "a", align: "middle" }], rows: [{ a: { x: 1 } }, 3], sort: { key: "zz" } }));
  for (const want of ["columns[1].align: must be one of", "columns[1].key: duplicates", "rows[0].a: must be a string", "rows[1]: must be an object", "sort.key: is not a column key"]) {
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  }
  check(errorsOf(validateTable({ rows: [] })).some((x) => x.startsWith("columns: is required")));
});

Deno.test("sorting: natural text, numbers, empties last both ways, stable", () => {
  eq(sortRows(rows, { key: "name", dir: "asc" }).map((r) => r.name), ["A1", "b2", "b10", ""]);
  eq(sortRows(rows, { key: "name", dir: "desc" }).map((r) => r.name), ["b10", "b2", "A1", ""]);
  eq(sortRows(rows, { key: "size", dir: "asc" }).map((r) => r.name), ["b10", "", "A1", "b2"]);
  eq(sortRows(rows, { key: "size", dir: "desc" }).map((r) => r.name), ["A1", "b10", "", "b2"]);
  eq(sortRows(rows, { key: "ok", dir: "asc" }).map((r) => r.name), ["b2", "b10", "A1", ""]);
  eq(sortRows(rows, undefined).map((r) => r.name), rows.map((r) => r.name));
  check(compareCells(2, "a") < 0 && compareCells("a", 2) > 0);
});

Deno.test("header clicks: ascending first (descending for numbers), then flip", () => {
  eq(nextSort(undefined, "name", false), { key: "name", dir: "asc" });
  eq(nextSort(undefined, "size", true), { key: "size", dir: "desc" });
  eq(nextSort({ key: "name", dir: "asc" }, "name", false), { key: "name", dir: "desc" });
  eq(nextSort({ key: "name", dir: "desc" }, "size", true), { key: "size", dir: "desc" });
});

Deno.test("filter: every word, any shown column, case-insensitive", () => {
  eq(filterRows(rows, columns, "B").map((r) => r.name), ["b10", "b2"]);
  eq(filterRows(rows, columns, "b1 3").map((r) => r.name), ["b10"]);
  eq(filterRows(rows, columns, "true").length, 0);
  eq(filterRows(rows, columns, "  ").length, 4);
});

Deno.test("table input: the Briefing fields are optional and checked", () => {
  const r = validateTable({
    title: "Flaky tests", scope: "acme · last 200 runs", takeaway: "One is **bad**.", source: "junit",
    columns: [{ key: "t" }, { key: "n", format: "bar" }, { key: "s", format: "badge", tones: { Flaky: "warn", Fixed: "good" } }, { key: "f", format: "mono" }],
    rows: [{ t: "a", n: 23, s: "Flaky", f: "a.ts" }, { t: "b", n: -4, s: "Fixed", f: "b.ts" }],
  });
  check(r.ok, JSON.stringify(r));
  eq([r.value.scope, r.value.columns[1].format, r.value.columns[2].tones], ["acme · last 200 runs", "bar", { Flaky: "warn", Fixed: "good" }]);
  eq(columnMax(r.value.rows, "n"), 23);
  eq(columnMax([{ n: null }, { n: "x" }], "n"), 0);
  const e = errorsOf(validateTable({ columns: [{ key: "a", format: "spark" }, { key: "b", tones: { x: "orange" } }, { key: "c", tones: [1] }], rows: [], takeaway: 5 }));
  for (const want of ["columns[0].format: must be one of bar, badge, mono", "columns[1].tones.x: must be one of good, warn, bad, info, accent, plain", "columns[2].tones: must be an object", "takeaway: must be a string"]) {
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  }
  // A bar column that holds text is drawn as text, with a warning rather than a refusal.
  const text = validateTable({ columns: [{ key: "a", format: "bar" }], rows: [{ a: "x" }] });
  check(text.ok && text.warnings.some((w) => w.includes('format "bar" needs numbers')), JSON.stringify(text));
});
