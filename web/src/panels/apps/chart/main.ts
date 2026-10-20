// The chart app: the finding first (title, scope, takeaway, headline numbers), then the chart
// drawn by the kit, then the source and the data table toggle.
import { startApp } from "../shared/client.ts";
import { problems } from "../shared/dom.ts";
import { chart, isEmpty } from "../../kit/chart.ts";
import { esc, rich } from "../../kit/format.ts";
import { icon } from "../../kit/icons.ts";
import { stat } from "../../kit/parts.ts";
import { type ChartSpec, validateChart } from "./logic.ts";

const root = document.getElementById("app")!;
let showTable = false;

function render(spec: ChartSpec, warnings: string[]) {
  const head = spec.title || spec.scope || spec.takeaway
    ? `<header class="app-head">${spec.title || spec.scope ? `<h1 class="mk-title">${esc(spec.title ?? "")}${spec.scope ? ` <span class="mk-scope">${esc(spec.scope)}</span>` : ""}</h1>` : ""}` +
      `${spec.takeaway ? `<p class="mk-takeaway">${rich(spec.takeaway)}</p>` : ""}</header>`
    : "";
  const heads = spec.stats?.length ? `<div class="mk-heads">${spec.stats.map((s) => stat(s)).join("")}</div>` : "";
  root.innerHTML = head + (warnings.length ? `<p class="warnings">${esc(warnings.join("; "))}</p>` : "") + heads +
    `<div class="chart-slot"></div>` +
    (spec.source || !isEmpty(spec) ? `<footer class="app-foot">${spec.source ? `<span class="mk-source">Source: ${esc(spec.source)}</span>` : ""}` +
      (isEmpty(spec) ? "" : `<span class="keys"><button type="button" class="ghost sm" id="toggle-table" aria-expanded="${showTable}">${icon("table")}${showTable ? "Hide data table" : "Data table"}</button>` +
        `<span class="step-hint" aria-hidden="true"><kbd>←</kbd> <kbd>→</kbd> step</span></span>`) + `</footer>` : "");
  const slot = root.querySelector<HTMLElement>(".chart-slot")!;
  // Headline numbers name the series with their colours, so they replace the legend.
  chart(slot, spec, { showTable, legendValues: spec.series.length > 1, legend: spec.stats?.length ? false : undefined });
  const toggle = document.getElementById("toggle-table");
  const table = slot.querySelector<HTMLElement>("[id$='-data']");
  if (table) toggle?.setAttribute("aria-controls", table.id);
  toggle?.addEventListener("click", () => {
    showTable = !showTable;
    render(spec, warnings);
    document.getElementById("toggle-table")?.focus();
  });
}

startApp({
  name: "Muster Chart",
  input: (data) => {
    const r = validateChart(data);
    if (!r.ok) { problems(root, "This chart panel's input is invalid", r.errors); return; }
    render(r.value, r.warnings);
  },
});
