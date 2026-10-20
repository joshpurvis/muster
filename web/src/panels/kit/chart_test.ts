import { change, endLabel, highlightOf, isEmpty, type KitChartSpec, lineBounds, nearestPoint, stackBars, tipLeft, yDomain, yTicks } from "./chart.ts";
import { check, eq } from "../apps/shared/test_util.ts";

const spec = (o: Partial<KitChartSpec>): KitChartSpec => ({ type: "bar", x: { values: ["a", "b", "c"] }, series: [], ...o });

Deno.test("no data: no x values, no series, or only missing values", () => {
  check(isEmpty(spec({ x: { values: [] }, series: [] })));
  check(isEmpty(spec({ series: [{ name: "s", values: [null, null, null] }] })));
  check(!isEmpty(spec({ series: [{ name: "s", values: [null, 0, null] }] })));
});

Deno.test("stacks keep positives above zero and negatives below", () => {
  const series = [{ name: "in", values: [5, 3, null] }, { name: "out", values: [-2, 4, -1] }, { name: "fees", values: [-1, null, 2] }];
  eq(stackBars(series, 3), [[[0, 5], [0, 3], null], [[0, -2], [3, 7], [0, -1]], [[-2, -3], null, [0, 2]]]);
  const s = spec({ type: "bar", stacked: true, series });
  eq(yDomain(s, series, 3), { lo: -3, hi: 7 });
  // Not stacked: the extremes of the values themselves.
  eq(yDomain({ ...s, stacked: false }, series, 3), { lo: -2, hi: 5 });
  // Stacking applies to bars and areas only.
  eq(yDomain({ ...s, type: "line" }, series, 3).hi > 5, true);
});

Deno.test("bars include zero; lines get air but never go below zero for positive data", () => {
  const series = [{ name: "p95", values: [182, 262, 158] }];
  eq(yDomain(spec({ type: "bar", series }), series, 3), { lo: 0, hi: 262 });
  const line = yDomain(spec({ type: "line", series }), series, 3);
  check(line.lo < 158 && line.lo > 0 && line.hi > 262, JSON.stringify(line));
  const near = [{ name: "e", values: [0.1, 2.1, 0.3] }];
  eq(yDomain(spec({ type: "line", series: near }), near, 3).lo, 0);
});

Deno.test("fixed y bounds win and ticks stay inside them", () => {
  const t = yTicks(spec({ y: { min: 0, max: 7.5 } }), 0, 7, 4);
  eq([t.min, t.max], [0, 7.5]);
  check(t.ticks.every((v) => v >= 0 && v <= 7.5), JSON.stringify(t));
  const above = yTicks(spec({ y: { min: 10 } }), 1, 2, 4);
  check(above.min === 10 && above.max > 10, JSON.stringify(above));
  const below = yTicks(spec({ y: { max: 5 } }), 10, 20, 4);
  check(below.max === 5 && below.min < 5, JSON.stringify(below));
});

Deno.test("tooltip change: percent, or points for pct values; nothing without a previous value", () => {
  eq([change(70, 61, false), change(124, 110, false), change(54, 55, false)], ["+15%", "+13%", "−2%"]);
  eq([change(1.09, 0.92, true), change(0.53, 0.54, true)], ["+0.17 pt", "−0.01 pt"]);
  eq([change(5, null, false), change(null, 5, false), change(5, 0, false), change(5, undefined, false)], [null, null, null, null]);
  eq(change(-5, -10, false), "+50%");
});

Deno.test("highlight: the named series, or past 8 series the first", () => {
  const many = Array.from({ length: 9 }, (_, i) => ({ name: `s${i}`, values: [1, 2, 3] }));
  eq(highlightOf(spec({ series: many })), "s0");
  eq(highlightOf(spec({ series: many.slice(0, 8) })), null);
  eq(highlightOf(spec({ series: many, highlight: "s4" })), "s4");
  eq(highlightOf(spec({ series: many.slice(0, 3), highlight: "nope" })), null);
});

Deno.test("stacked areas with negatives use the same pos/neg stacks as bars (a negative layer sits below zero)", () => {
  const layers = lineBounds([{ name: "a", values: [5, 10, null] }, { name: "b", values: [-2, -5, 3] }, { name: "c", values: [1, -1, 2] }], 3, true);
  eq(layers[0], { top: [5, 10, null], bot: [0, 0, 0] });
  eq(layers[1], { top: [-2, -5, 3], bot: [0, 0, 0] });
  eq(layers[2], { top: [6, -6, 5], bot: [5, -5, 3] });
  // Not stacked: the values themselves, filled to zero.
  eq(lineBounds([{ name: "b", values: [-2, null] }], 2, false), [{ top: [-2, null], bot: null }]);
});

Deno.test("scatter hover: the nearest point in both directions, within reach", () => {
  const pts = [{ i: 0, x: 100, y: 200 }, { i: 1, x: 100, y: 20 }, { i: 2, x: 300, y: 100 }];
  eq(nearestPoint(pts, 101, 22, 24), 1);
  eq(nearestPoint(pts, 99, 195, 24), 0);
  eq(nearestPoint(pts, 200, 110, 24), null);
  eq(nearestPoint([], 0, 0, 24), null);
});

Deno.test("tooltip placement: right of the point, else left of it, always inside the chart", () => {
  eq(tipLeft(100, 72, 180, 600), 100);
  eq(tipLeft(500, 472, 180, 600), 292);
  eq(tipLeft(120, 92, 180, 240), 0);
  eq(tipLeft(20, -8, 400, 300), 0);
});

Deno.test("end labels fit their room: whole, shortened, or the legend instead", () => {
  eq(endLabel("p95", "182 ms", 150), "p95");
  const cut = endLabel("checkout-service-production-eu-west", "2.1%", 150)!;
  check(cut.endsWith("…") && (cut.length + 1 + 4) * 6.4 <= 150, cut);
  eq(endLabel("checkout-service", "1,234,567.89 ms", 60), null);
});
