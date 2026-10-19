import { startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { cellText, filterRows, nextSort, type Sort, sortRows, type TableSpec, validateTable } from "./logic.ts";

const root = document.getElementById("app")!;
const SHOWN = 1000;
let spec: TableSpec | null = null;
let warnings: string[] = [];
let sort: Sort | undefined;
let query = "";
let limit = SHOWN;
let body: HTMLTableSectionElement | null = null;
let count: HTMLElement | null = null;
let more: HTMLElement | null = null;
let headers: HTMLElement | null = null;

function rowsShown() {
  if (!spec || !body || !count || !more) return;
  const s = spec;
  const rows = sortRows(filterRows(s.rows, s.columns, query), sort);
  body.replaceChildren(...rows.slice(0, limit).map((r) =>
    h("tr", null, s.columns.map((c) => {
      const v = r[c.key];
      const align = c.align ?? (c.numeric ? "right" : "left");
      return h("td", { class: `${c.numeric ? "num" : ""} ${align}`, title: typeof v === "string" && v.length > 80 ? v : null }, cellText(v));
    }))));
  if (!rows.length) body.append(h("tr", null, h("td", { colspan: s.columns.length, class: "empty" }, query ? "No rows match the filter." : "No rows.")));
  count.textContent = query || rows.length !== s.rows.length ? `${rows.length} of ${s.rows.length} rows` : `${s.rows.length} row${s.rows.length === 1 ? "" : "s"}`;
  more.replaceChildren(rows.length > limit
    ? h("button", { type: "button", onclick: () => { limit += SHOWN; rowsShown(); } }, `Show ${Math.min(SHOWN, rows.length - limit)} more of ${rows.length - limit} hidden rows`)
    : "");
}

function headerCells() {
  if (!spec || !headers) return;
  headers.replaceChildren(...spec.columns.map((c) => {
    const dir = sort?.key === c.key ? sort.dir : null;
    const align = c.align ?? (c.numeric ? "right" : "left");
    return h("th", { scope: "col", class: align, "aria-sort": dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none" },
      h("button", {
        type: "button", class: "sort", title: `Sort by ${c.label}`,
        onclick: () => { sort = nextSort(sort, c.key, c.numeric); headerCells(); rowsShown(); },
      }, h("span", null, c.label), h("span", { class: "arrow", "aria-hidden": "true" }, dir === "asc" ? "▲" : dir === "desc" ? "▼" : "")));
  }));
}

function render() {
  if (!spec) return;
  const focused = document.activeElement?.id;
  const filter = h("input", {
    id: "filter", type: "search", placeholder: "Filter rows", "aria-label": "Filter rows (/)", value: query, autocomplete: "off",
  }) as HTMLInputElement;
  filter.addEventListener("input", () => { query = filter.value; limit = SHOWN; rowsShown(); });
  count = h("span", { class: "muted count", "aria-live": "polite" });
  headers = h("tr");
  body = h("tbody");
  more = h("div", { class: "more" });
  mount(root, 
    h("div", { class: "top" }, spec.title ? h("h1", null, spec.title) : null, h("span", { class: "spacer" }), filter, count),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("div", { class: "scroll", tabindex: "0", "aria-label": spec.title ?? "Table" },
      h("table", null, spec.title ? h("caption", { class: "sr-only" }, spec.title) : null, h("thead", null, headers), body)),
    more,
  );
  headerCells();
  rowsShown();
  if (focused === "filter") filter.focus();
}

addEventListener("keydown", (e) => {
  if (e.key === "/" && !typing(e) && !e.ctrlKey && !e.metaKey && !e.altKey) {
    document.getElementById("filter")?.focus();
    e.preventDefault();
  } else if (e.key === "Escape" && (e.target as HTMLElement)?.id === "filter" && query) {
    query = "";
    (e.target as HTMLInputElement).value = "";
    rowsShown();
  }
});

startApp({
  name: "Muster Table",
  input: (data) => {
    const r = validateTable(data);
    if (!r.ok) { spec = null; problems(root, "This table panel's input is invalid", r.errors); return; }
    // Keep the user's sort across updates unless the agent's sort or the columns changed.
    const keys = r.value.columns.map((c) => c.key).join("\u0000");
    const prevKeys = spec?.columns.map((c) => c.key).join("\u0000");
    const agentSort = JSON.stringify(r.value.sort) !== JSON.stringify(spec?.sort);
    if (!spec || keys !== prevKeys || agentSort) sort = r.value.sort;
    spec = r.value;
    warnings = r.warnings;
    render();
  },
});
