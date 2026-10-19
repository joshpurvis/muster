import { startApp } from "../shared/client.ts";
import { h, problems } from "../shared/dom.ts";
import { type ChartSpec, validateChart } from "./logic.ts";
import { renderChart } from "./view.ts";

const root = document.getElementById("app")!;
let spec: ChartSpec | null = null;
let warnings: string[] = [];
let showTable = false;
let lastWidth = 0;

function render() {
  if (!spec) return;
  lastWidth = root.clientWidth;
  const view = h("div", { class: "chart-view" });
  root.replaceChildren(view);
  renderChart(view, spec, {
    id: "chart", width: root.clientWidth - 32, showTable, warnings, onToggle: () => { showTable = !showTable; render(); },
  });
}

new ResizeObserver(() => { if (spec && Math.abs(root.clientWidth - lastWidth) > 4) render(); }).observe(root);

startApp({
  name: "Muster Chart",
  input: (data) => {
    const r = validateChart(data);
    if (!r.ok) { spec = null; problems(root, "This chart panel's input is invalid", r.errors); return; }
    spec = r.value;
    warnings = r.warnings;
    render();
  },
});
