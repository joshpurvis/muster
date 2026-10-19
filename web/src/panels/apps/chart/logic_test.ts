import {
  contrast, formatTick, formatValue, labelEvery, linear, niceStep, niceTicks, numericX, PALETTE, validateChart, yScale,
} from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

const bar = { type: "bar", title: "Builds", x: { label: "Day", values: ["Mon", "Tue", "Wed"] }, series: [{ name: "ok", values: [3, 7, null] }] };

Deno.test("chart input: a valid spec passes through", () => {
  const r = validateChart(bar);
  check(r.ok);
  eq(r.value.series[0].values, [3, 7, null]);
  eq(r.value.y, {});
});

Deno.test("chart input: every problem is named with its path", () => {
  const e = errorsOf(validateChart({ type: "pie", x: { values: [1, 2] }, series: [{ name: "a", values: [1] }, { values: [1, "2"] }], y: { min: 5, max: 1 } }));
  check(e.some((x) => x.startsWith("type: must be one of bar, line, scatter, area")), e.join("\n"));
  check(e.some((x) => x.startsWith("series[0].values: has 1 values but x.values has 2")), e.join("\n"));
  check(e.some((x) => x.startsWith("series[1].name: is required")), e.join("\n"));
  check(e.some((x) => x.startsWith("series[1].values[1]: must be a finite number or null")), e.join("\n"));
  check(e.some((x) => x.startsWith("y: min must be less than max")), e.join("\n"));
  check(errorsOf(validateChart({ type: "scatter", x: { values: ["a"] }, series: [{ name: "s", values: [1] }] }))[0].includes("numbers"));
  check(errorsOf(validateChart({ type: "line", series: [] })).some((x) => x.startsWith("x: is required")));
  check(errorsOf(validateChart({ type: "line", x: { values: [] }, series: [{ name: "a", values: [] }] })).some((x) => x.includes("at least 1")));
  check(errorsOf(validateChart({ type: "line", x: { values: [1] }, series: Array.from({ length: 13 }, (_, i) => ({ name: `${i}`, values: [1] })) }))[0].includes("at most 12"));
});

Deno.test("nice steps and ticks", () => {
  eq([niceStep(0.13), niceStep(2.4), niceStep(4), niceStep(8), niceStep(130)], [0.1, 2, 5, 10, 100]);
  eq(niceTicks(0, 97), { min: 0, max: 100, step: 20, ticks: [0, 20, 40, 60, 80, 100] });
  eq(niceTicks(-3.2, 7.9).ticks, [-4, -2, 0, 2, 4, 6, 8]);
  eq(niceTicks(0.1, 0.3, 4).ticks, [0.1, 0.15, 0.2, 0.25, 0.3]);
  eq(niceTicks(5, 5).ticks, [4.4, 4.6, 4.8, 5, 5.2, 5.4, 5.6]);
  eq(niceTicks(0, 0).ticks, [0, 0.2, 0.4, 0.6, 0.8, 1]);
  eq(niceTicks(10, 0).min, 0);
});

Deno.test("tick and value labels", () => {
  eq([formatTick(0.30000000000000004, 0.1), formatTick(1500000, 500000), formatTick(20000, 5000), formatTick(250, 50), formatTick(-2, 2), formatTick(12500, 2500)], ["0.3", "1.5M", "20k", "250", "-2", "12.5k"]);
  eq([formatValue(0.1 + 0.2), formatValue(null), formatValue(12345.678)], ["0.3", "–", "12345.678"]);
});

Deno.test("scales: bars and areas include zero, fixed y bounds win", () => {
  const r = validateChart(bar);
  check(r.ok);
  eq(yScale(r.value), { min: 0, max: 7, step: 1, ticks: [0, 1, 2, 3, 4, 5, 6, 7] });
  const line = validateChart({ type: "line", x: { values: [1, 2] }, series: [{ name: "a", values: [50, 60] }] });
  check(line.ok);
  eq(yScale(line.value).min, 50);
  check(numericX(line.value));
  const fixed = validateChart({ ...bar, y: { min: 0, max: 7.5 } });
  check(fixed.ok);
  eq(yScale(fixed.value), { min: 0, max: 7.5, step: 2, ticks: [0, 2, 4, 6] });
  check(!numericX(r.value));
  const map = linear(0, 10, 100, 0);
  eq([map(0), map(5), map(10)], [100, 50, 0]);
  eq([labelEvery(100, 500, 50), labelEvery(5, 500, 50), labelEvery(0, 500, 50)], [10, 1, 1]);
});

Deno.test("palette keeps 3:1 contrast on light and dark backgrounds", () => {
  for (const c of PALETTE) {
    check(contrast(c, "#ffffff") >= 3, `${c} on light: ${contrast(c, "#ffffff")}`);
    check(contrast(c, "#19191d") >= 3, `${c} on dark: ${contrast(c, "#19191d")}`);
  }
  eq(new Set(PALETTE).size, PALETTE.length);
});
