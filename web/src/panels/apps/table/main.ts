// The table app: the finding first (title, scope, takeaway), then a filterable, sortable table
// in the kit's style: numbers right-aligned in tabular figures, optional inline bars, badges
// and mono columns, a sticky header, and a comfortable or compact density.
import { startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { iconEl } from "../../kit/icons.ts";
import { esc, fmt, rich } from "../../kit/format.ts";
import { cellText, type Column, columnMax, filterRows, nextSort, type Sort, sortRows, type TableSpec, validateTable } from "./logic.ts";

const root = document.getElementById("app")!;
const SHOWN = 1000;
let spec: TableSpec | null = null;
let warnings: string[] = [];
let sort: Sort | undefined;
let query = "";
let limit = SHOWN;
let compact = false;
let body: HTMLTableSectionElement | null = null;
let count: HTMLElement | null = null;
let sorted: HTMLElement | null = null;
let more: HTMLElement | null = null;
let headers: HTMLElement | null = null;
let empty: HTMLElement | null = null;
let maxes = new Map<string, number>();

const alignOf = (c: Column) => c.align ?? (c.numeric ? "right" : "left");

function cell(c: Column, v: unknown, first: boolean): HTMLElement {
  const align = alignOf(c);
  const text = cellText(v as never);
  const long = typeof v === "string" && v.length > 80 ? v : null;
  if (c.format === "bar" && c.numeric && typeof v === "number") {
    const max = maxes.get(c.key) || 1;
    return h("td", { class: `num n ${align}` },
      h("div", { class: "mk-bar-cell" }, h("span", null, text),
        h("span", { class: "b", "aria-hidden": "true" }, h("i", { style: `width:${Math.min(100, (Math.abs(v) / max) * 100).toFixed(1)}%` }))));
  }
  if (c.format === "badge" && text) {
    const tone = c.tones?.[text] ?? "plain";
    return h("td", { class: align }, h("span", { class: `mk-badge ${tone}` }, text));
  }
  if (c.format === "mono") return h("td", { class: `file ${align}`, title: long }, text);
  if (c.numeric) return h("td", { class: `num n ${align}` }, text);
  return h("td", { class: `${align}${first ? " t1" : ""}`, title: long }, text);
}

function rowsShown() {
  if (!spec || !body || !count || !more || !empty || !sorted) return;
  const s = spec;
  const rows = sortRows(filterRows(s.rows, s.columns, query), sort);
  body.replaceChildren(...rows.slice(0, limit).map((r) => h("tr", null, s.columns.map((c, i) => cell(c, r[c.key], i === 0)))));
  // No match: say why and how to get out, in place of the rows.
  empty.hidden = rows.length > 0;
  empty.replaceChildren(...(rows.length ? [] : query
    ? [h("div", { class: "mk-icon" }, iconEl("filter")), h("p", { class: "e-title" }, `No rows match "${query}"`),
      h("p", { class: "e-why" }, "Every word has to match somewhere in the row."),
      h("button", { type: "button", class: "sm", id: "clear-filter", onclick: () => clearFilter(true) }, "Clear filter ", h("kbd", null, "Esc"))]
    : [h("p", { class: "e-title" }, "No rows."), h("p", { class: "e-why" }, "The agent sent this table without any rows.")]));
  count.replaceChildren(...(query || rows.length !== s.rows.length
    ? [h("b", null, fmt(rows.length, "int")), ` of ${fmt(s.rows.length, "int")} rows`]
    : [h("b", null, fmt(s.rows.length, "int")), ` row${s.rows.length === 1 ? "" : "s"}`]));
  const by = sort ? s.columns.find((c) => c.key === sort!.key)?.label : undefined;
  sorted.textContent = by ? ` · sorted by ${by}` : "";
  more.replaceChildren(rows.length > limit
    ? h("button", { type: "button", class: "sm", onclick: () => { limit += SHOWN; rowsShown(); } }, `Show ${Math.min(SHOWN, rows.length - limit)} more of ${rows.length - limit} hidden rows`)
    : "");
}

function clearFilter(focus: boolean) {
  query = "";
  const f = document.getElementById("filter") as HTMLInputElement | null;
  if (f) { f.value = ""; if (focus) f.focus(); }
  rowsShown();
}

function headerCells() {
  if (!spec || !headers) return;
  headers.replaceChildren(...spec.columns.map((c) => {
    const dir = sort?.key === c.key ? sort.dir : null;
    return h("th", { scope: "col", class: alignOf(c), "aria-sort": dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none" },
      h("button", {
        type: "button", class: "sort", title: `Sort by ${c.label}`, "data-key": c.key,
        onclick: () => {
          sort = nextSort(sort, c.key, c.numeric);
          headerCells();
          rowsShown();
          // The header was rebuilt: keep the keyboard on the same column.
          [...document.querySelectorAll<HTMLElement>("button.sort")].find((b) => b.dataset.key === c.key)?.focus();
        },
      }, h("span", null, c.label), h("span", { class: "arr", "aria-hidden": "true" }, dir === "asc" ? "↑" : dir === "desc" ? "↓" : "")));
  }));
}

function setDensity(next: boolean) {
  compact = next;
  document.querySelector(".mk-table")?.classList.toggle("compact", compact);
  document.querySelectorAll<HTMLElement>(".density button").forEach((b) => b.setAttribute("aria-pressed", String((b.dataset.d === "compact") === compact)));
}

function render() {
  if (!spec) return;
  const s = spec;
  const focused = document.activeElement?.id;
  maxes = new Map(s.columns.filter((c) => c.format === "bar").map((c) => [c.key, columnMax(s.rows, c.key)]));
  const filter = h("input", {
    id: "filter", class: "mk-input", type: "search", placeholder: "Filter rows", "aria-label": "Filter rows (/)", value: query, autocomplete: "off",
  }) as HTMLInputElement;
  filter.addEventListener("input", () => { query = filter.value; limit = SHOWN; rowsShown(); });
  count = h("span", { class: "count" });
  sorted = h("span", { class: "sorted" });
  headers = h("tr");
  body = h("tbody");
  more = h("div", { class: "more" });
  empty = h("div", { class: "empty-state", role: "status" });
  const head = h("header", { class: "app-head" });
  if (s.title || s.scope) head.innerHTML = `<h1 class="mk-title">${esc(s.title ?? "")}${s.scope ? ` <span class="mk-scope">${esc(s.scope)}</span>` : ""}</h1>`;
  if (s.takeaway) head.insertAdjacentHTML("beforeend", `<p class="mk-takeaway">${rich(s.takeaway)}</p>`);
  const density = h("div", { class: "mk-seg density", role: "group", "aria-label": "Row density" },
    h("button", { type: "button", "data-d": "comfortable", "aria-pressed": String(!compact), onclick: () => setDensity(false) }, "Comfortable"),
    h("button", { type: "button", "data-d": "compact", "aria-pressed": String(compact), onclick: () => setDensity(true) }, "Compact"));
  mount(root,
    s.title || s.scope || s.takeaway ? head : null,
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("div", { class: "tbl-bar" },
      h("label", { class: "mk-search" }, iconEl("search"), filter, h("kbd", { "aria-hidden": "true" }, "/")),
      density,
      h("span", { class: "tally", "aria-live": "polite" }, count, sorted)),
    h("div", { class: "mk-table-wrap", tabindex: "0", "aria-label": s.title ?? "Table" },
      h("table", { class: `mk-table sortable${compact ? " compact" : ""}` }, s.title ? h("caption", { class: "sr-only" }, s.title) : null, h("thead", null, headers), body),
      empty),
    more,
    h("footer", { class: "app-foot" }, s.source ? h("span", { class: "mk-source" }, `Source: ${s.source}`) : null,
      h("span", { class: "keys" }, "Click a header to sort · ", h("kbd", null, "/"), " filter")),
  );
  headerCells();
  rowsShown();
  if (focused === "filter") filter.focus();
}

addEventListener("keydown", (e) => {
  if (e.key === "/" && !typing(e) && !e.ctrlKey && !e.metaKey && !e.altKey) {
    document.getElementById("filter")?.focus();
    e.preventDefault();
  } else if (e.key === "Escape" && query && ((e.target as HTMLElement)?.id === "filter" || (e.target as HTMLElement)?.id === "clear-filter")) {
    e.preventDefault();
    clearFilter((e.target as HTMLElement).id === "clear-filter");
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
