// Draws a validated chart spec as accessible SVG. Used by the chart app and by pr_decision's
// `media` charts, so both look and behave the same.
import { h, mount, svg } from "../shared/dom.ts";
import {
  type ChartSpec, formatTick, formatValue, labelEvery, linear, niceTicks, numericX, seriesStyle, yScale,
} from "./logic.ts";

export interface ChartOptions {
  /** Unique per chart in the document: prefixes its element ids. */
  id: string;
  /** Width available for the SVG, in CSS pixels. */
  width: number;
  minWidth?: number;
  /** Largest height (default 460). */
  maxHeight?: number;
  showTable: boolean;
  onToggle: () => void;
  warnings?: string[];
  heading?: "h1" | "h3";
}

const typeName = { bar: "Bar chart", line: "Line chart", scatter: "Scatter plot", area: "Area chart" };
const xText = (v: string | number) => typeof v === "number" ? formatValue(v) : v;

function describe(s: ChartSpec, y: { min: number; max: number }) {
  const n = s.x.values.length;
  const names = s.series.map((x) => x.name).join(", ");
  return `${typeName[s.type]} of ${s.series.length} series (${names}) over ${n} ${s.x.label ?? "x"} value${n === 1 ? "" : "s"}` +
    `, from ${xText(s.x.values[0])} to ${xText(s.x.values[n - 1])}. ${s.y.label ?? "y"} axis from ${formatValue(y.min)} to ${formatValue(y.max)}.`;
}

/** Diagonal hatching in a series colour, for series 9–12. */
function hatchDefs(count: number, id: string) {
  return svg("defs", null, Array.from({ length: count }, (_, i) => seriesStyle(i)).flatMap((st, i) => st.hatch ? [
    svg("pattern", { id: `${id}-hatch-${i}`, width: 6, height: 6, patternUnits: "userSpaceOnUse", patternTransform: "rotate(45)" },
      svg("rect", { width: 6, height: 6, fill: st.color, "fill-opacity": 0.25 }),
      svg("line", { x1: 0, y1: 0, x2: 0, y2: 6, stroke: st.color, "stroke-width": 3 })),
  ] : []));
}

/** The legend and tooltip key: the same colour, dash and hatching as the marks. */
function swatch(i: number, type: ChartSpec["type"], id: string) {
  const st = seriesStyle(i);
  const line = type === "line" || type === "scatter";
  return svg("svg", { class: "swatch-svg", width: 16, height: 10, "aria-hidden": "true" },
    line
      ? svg("line", { x1: 1, y1: 5, x2: 15, y2: 5, stroke: st.color, "stroke-width": 3, "stroke-dasharray": st.dash ? "4 2" : null })
      : svg("rect", { x: 1, y: 0, width: 14, height: 10, rx: 2, fill: st.hatch ? `url(#${id}-hatch-${i})` : st.color, stroke: st.hatch ? st.color : null }));
}

function dataTable(s: ChartSpec, o: ChartOptions) {
  // A table ignores width: 1px, so the visually hidden wrapper is a div.
  return h("div", { class: o.showTable ? "data-wrap" : "sr-only", id: `${o.id}-data` }, h("table", { class: "data" },
    h("caption", null, `${s.title ?? "Chart"} data`),
    h("thead", null, h("tr", null, h("th", { scope: "col" }, s.x.label ?? "x"), s.series.map((x) => h("th", { scope: "col" }, x.name)))),
    h("tbody", null, s.x.values.map((xv, i) =>
      h("tr", null, h("th", { scope: "row" }, xText(xv)), s.series.map((x) => h("td", null, formatValue(x.values[i] ?? null))))))));
}

export function renderChart(container: HTMLElement, s: ChartSpec, o: ChartOptions) {
  const width = Math.max(o.minWidth ?? 260, o.width);
  const height = Math.round(Math.min(o.maxHeight ?? 460, Math.max(220, width * 0.55)));
  const y = yScale(s, height < 300 ? 4 : 6);
  const numeric = numericX(s);
  const n = s.x.values.length;
  const yLabels = y.ticks.map((t) => formatTick(t, y.step));
  const m = {
    top: 12, right: 16,
    bottom: 30 + (s.x.label ? 20 : 0),
    left: 12 + Math.max(...yLabels.map((l) => l.length), 1) * 7.2 + (s.y.label ? 20 : 0),
  };
  const pw = width - m.left - m.right;
  const ph = height - m.top - m.bottom;
  const Y = linear(y.min, y.max, m.top + ph, m.top);
  const clampY = (v: number) => Math.max(m.top, Math.min(m.top + ph, Y(v)));
  // x position of point i.
  let X: (i: number) => number;
  let xTicks: { at: number; label: string }[];
  const band = pw / n;
  if (numeric) {
    const xs = s.x.values as number[];
    const lo = Math.min(...xs), hi = Math.max(...xs);
    const t = niceTicks(lo, hi, Math.max(2, Math.floor(pw / 90)));
    const [d0, d1] = s.type === "scatter" ? [t.min, t.max] : [lo, hi === lo ? lo + 1 : hi];
    const XV = linear(d0, d1, m.left, m.left + pw);
    X = (i) => XV(xs[i]);
    xTicks = t.ticks.filter((v) => v >= d0 - 1e-9 && v <= d1 + 1e-9).map((v) => ({ at: XV(v), label: formatTick(v, t.step) }));
  } else {
    X = (i) => m.left + band * (i + 0.5);
    const longest = Math.max(...s.x.values.map((v) => xText(v).length));
    const every = labelEvery(n, pw, Math.min(longest, 18) * 7 + 12);
    xTicks = s.x.values.flatMap((v, i) => i % every ? [] : [{ at: X(i), label: xText(v).length > 18 ? xText(v).slice(0, 17) + "…" : xText(v) }]);
  }

  const marks: SVGElement[] = [];
  const zero = clampY(Math.max(y.min, Math.min(0, y.max)));
  s.series.forEach((ser, si) => {
    const st = seriesStyle(si);
    const c = st.color;
    const fill = st.hatch ? `url(#${o.id}-hatch-${si})` : c;
    // A point marker: circles for the first eight series, squares for the dashed ones.
    const dot = (cx: number, cy: number, r: number, cls?: string) => st.hatch
      ? svg("rect", { x: cx - r, y: cy - r, width: 2 * r, height: 2 * r, fill: c, class: cls, })
      : svg("circle", { cx, cy, r, fill: c, class: cls });
    if (s.type === "bar") {
      const inner = band * 0.8;
      const bw = Math.max(1, inner / s.series.length);
      ser.values.forEach((v, i) => {
        if (v === null) return;
        const x0 = m.left + band * i + band * 0.1 + bw * si;
        const top = Math.min(clampY(v), zero), bottom = Math.max(clampY(v), zero);
        marks.push(svg("rect", { x: x0, y: top, width: Math.max(1, bw - (s.series.length > 1 ? 1 : 0)), height: Math.max(v === 0 ? 0 : 1, bottom - top), fill, stroke: st.hatch ? c : null, class: "bar", "data-i": i }));
      });
      return;
    }
    if (s.type === "scatter") {
      ser.values.forEach((v, i) => {
        if (v !== null) { const d = dot(X(i), clampY(v), 4, "dot"); d.setAttribute("data-i", String(i)); marks.push(d); }
      });
      return;
    }
    // line and area: break the path at nulls.
    const runs: number[][] = [];
    let run: number[] = [];
    ser.values.forEach((v, i) => {
      if (v === null) { if (run.length) runs.push(run); run = []; } else run.push(i);
    });
    if (run.length) runs.push(run);
    for (const r of runs) {
      const pts = r.map((i) => `${X(i).toFixed(1)},${clampY(ser.values[i]!).toFixed(1)}`);
      if (s.type === "area") {
        marks.push(svg("path", { d: `M${X(r[0]).toFixed(1)},${zero}L${pts.join("L")}L${X(r[r.length - 1]).toFixed(1)},${zero}Z`, fill, "fill-opacity": st.hatch ? 0.5 : 0.16, stroke: "none" }));
      }
      marks.push(svg("path", { d: `M${pts.join("L")}`, fill: "none", stroke: c, "stroke-width": 2, "stroke-dasharray": st.dash ?? null, "stroke-linejoin": "round", "stroke-linecap": "round" }));
      if (n <= 60 || r.length === 1) for (const i of r) marks.push(dot(X(i), clampY(ser.values[i]!), 2.5));
    }
  });

  const guide = svg("line", { class: "guide", y1: m.top, y2: m.top + ph, x1: 0, x2: 0, visibility: "hidden" });
  const focusRing = svg("g", { class: "focus" });
  const titleId = `${o.id}-title`, descId = `${o.id}-desc`;
  const chart = svg("svg", {
    viewBox: `0 0 ${width} ${height}`, width, height, role: "img", tabindex: "0",
    "aria-labelledby": titleId, "aria-describedby": descId,
  },
  svg("title", { id: titleId }, s.title ?? typeName[s.type]),
  hatchDefs(s.series.length, o.id),
  svg("desc", { id: descId }, describe(s, y)),
  svg("g", { class: "grid" }, y.ticks.map((t) => svg("line", { x1: m.left, x2: m.left + pw, y1: Y(t), y2: Y(t) }))),
  svg("g", { class: "axis y" }, y.ticks.map((t, i) => svg("text", { x: m.left - 8, y: Y(t), "text-anchor": "end", "dominant-baseline": "middle" }, yLabels[i]))),
  svg("line", { class: "baseline", x1: m.left, x2: m.left + pw, y1: zero, y2: zero }),
  svg("g", { class: "axis x" }, xTicks.map((t) => svg("text", { x: t.at, y: m.top + ph + 18, "text-anchor": "middle" }, t.label))),
  s.x.label ? svg("text", { class: "axis-label", x: m.left + pw / 2, y: height - 6, "text-anchor": "middle" }, s.x.label) : null,
  s.y.label ? svg("text", { class: "axis-label", transform: `translate(14 ${m.top + ph / 2}) rotate(-90)`, "text-anchor": "middle" }, s.y.label) : null,
  guide, svg("g", { class: "marks" }, marks), focusRing);

  const tip = h("div", { class: "tip", hidden: true, "aria-hidden": "true" });
  const live = h("div", { class: "sr-only", "aria-live": "polite" });
  const figure = h("figure", null, chart, tip);

  let active = -1;
  const show = (i: number, px?: number, py?: number) => {
    if (i < 0 || i >= n) { tip.hidden = true; guide.setAttribute("visibility", "hidden"); focusRing.replaceChildren(); active = -1; return; }
    active = i;
    const x = X(i);
    if (s.type !== "scatter") {
      guide.setAttribute("x1", String(x));
      guide.setAttribute("x2", String(x));
      guide.setAttribute("visibility", "visible");
    }
    focusRing.replaceChildren(...s.series.flatMap((ser, si) => ser.values[i] === null || s.type === "bar" ? [] :
      [svg("circle", { cx: x, cy: clampY(ser.values[i]!), r: 5, fill: "none", stroke: seriesStyle(si).color, "stroke-width": 2 })]));
    mount(tip, 
      h("div", { class: "tip-x" }, `${s.x.label ?? "x"}: ${xText(s.x.values[i])}`),
      s.series.map((ser, si) => h("div", { class: "tip-row" },
        swatch(si, s.type, o.id), h("span", null, ser.name), h("b", null, formatValue(ser.values[i] ?? null)))));
    tip.hidden = false;
    const fw = figure.clientWidth;
    const left = (px ?? x) + 14;
    tip.style.left = `${Math.min(left, fw - tip.offsetWidth - 4)}px`;
    tip.style.top = `${Math.max(0, (py ?? m.top + 8) - 10)}px`;
    live.textContent = `${xText(s.x.values[i])}: ` + s.series.map((ser) => `${ser.name} ${formatValue(ser.values[i] ?? null)}`).join(", ");
  };
  const nearest = (mx: number, my: number) => {
    if (!numeric) return Math.floor((mx - m.left) / band);
    let best = -1, dist = Infinity;
    for (let i = 0; i < n; i++) {
      let d = Math.abs(X(i) - mx);
      if (s.type === "scatter") {
        d = Math.min(...s.series.map((ser) => ser.values[i] === null ? Infinity : Math.hypot(X(i) - mx, clampY(ser.values[i]!) - my)));
      }
      if (d < dist) { dist = d; best = i; }
    }
    return s.type === "scatter" && dist > 24 ? -1 : best;
  };
  chart.addEventListener("pointermove", (e) => {
    const r = chart.getBoundingClientRect();
    const mx = (e.clientX - r.left) * (width / r.width), my = (e.clientY - r.top) * (height / r.height);
    if (mx < m.left || mx > m.left + pw || my < m.top - 6 || my > m.top + ph + 6) { show(-1); return; }
    show(nearest(mx, my), mx, my);
  });
  chart.addEventListener("pointerleave", () => show(-1));
  chart.addEventListener("blur", () => show(-1));
  chart.addEventListener("keydown", (e) => {
    const next = e.key === "ArrowRight" ? active + 1 : e.key === "ArrowLeft" ? (active < 0 ? n - 1 : active - 1)
      : e.key === "Home" ? 0 : e.key === "End" ? n - 1 : e.key === "Escape" ? -1 : null;
    if (next === null) return;
    e.preventDefault();
    show(Math.max(-1, Math.min(n - 1, next)));
  });

  const toggleId = `${o.id}-toggle`;
  const toggle = h("button", { type: "button", class: "link", id: toggleId, "aria-controls": `${o.id}-data`, "aria-expanded": String(o.showTable),
    onclick: () => { o.onToggle(); document.getElementById(toggleId)?.focus(); } }, o.showTable ? "Hide data table" : "Show data table");
  mount(container,
    s.title ? h(o.heading ?? "h1", { class: "chart-title" }, s.title) : null,
    o.warnings?.length ? h("p", { class: "warnings" }, o.warnings.join("; ")) : null,
    h("ul", { class: "legend", "aria-label": "Series" }, s.series.map((ser, si) =>
      h("li", null, swatch(si, s.type, o.id), ser.name))),
    figure,
    h("p", { class: "hint muted" }, "Hover for values, or focus the chart and use the arrow keys. ", toggle),
    live,
    dataTable(s, o),
  );
}

