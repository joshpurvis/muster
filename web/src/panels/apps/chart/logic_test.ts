import { type ChartSpec, statsKeySeries, validateChart } from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

const bar = { type: "bar", title: "Builds", x: { label: "Day", values: ["Mon", "Tue", "Wed"] }, series: [{ name: "ok", values: [3, 7, null] }] };

Deno.test("chart input: a valid spec passes through", () => {
  const r = validateChart(bar);
  check(r.ok);
  eq(r.value.series[0].values, [3, 7, null]);
  eq(r.value.y, {});
  eq(r.warnings, []);
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
  check(errorsOf(validateChart({ type: "line", x: { values: [1] }, series: Array.from({ length: 13 }, (_, i) => ({ name: `${i}`, values: [1] })) }))[0].includes("at most 12"));
});

Deno.test("chart input: no data is allowed, so the panel can say so", () => {
  const r = validateChart({ type: "bar", x: { values: [] }, series: [{ name: "deploys", values: [] }], empty: "No deploys in this range" });
  check(r.ok);
  eq(r.value.empty, "No deploys in this range");
});

Deno.test("chart input: the Briefing fields are optional and checked", () => {
  const full = {
    ...bar, takeaway: "Wednesday is **missing**.", scope: "acme · this week", source: "gh run list", format: "int", xFormat: "plain",
    highlight: "ok", annotations: [{ x: "Tue", label: "v2.4" }], stacked: true, labels: "values", empty: "Nothing yet",
    stats: [{ label: "master", value: 287, unit: "min", delta: "−12%", tone: "good", note: "vs last week", color: "var(--mk-c1)" }],
    series: [{ name: "ok", values: [3, 7, null], color: "var(--mk-add)" }], pin: 1,
  };
  const r = validateChart(full);
  check(r.ok, JSON.stringify(r));
  eq(r.warnings, []);
  eq([r.value.format, r.value.highlight, r.value.annotations, r.value.stats?.[0].tone, r.value.series[0].color],
    ["int", "ok", [{ x: "Tue", label: "v2.4" }], "good", "var(--mk-add)"]);
  const e = errorsOf(validateChart({
    ...bar, format: "eur", highlight: "nope", annotations: [{ x: "Sun", label: "x" }, { label: "y" }], stacked: "yes", labels: "some",
    trend: true, series: [{ name: "ok", values: [1, 2, 3], color: "red; background: url(x)" }], stats: [{ label: "a" }, { value: 1 }], pin: 9,
  }));
  for (const want of ["format: must be one of int, pct, usd, ms, s, plain", "highlight: must be the name of a series", "annotations[0].x: must be one of x.values",
    "annotations[1].x: is required", "stacked: must be true or false", "labels: must be one of auto, values, end, none", "trend: applies to scatter charts only",
    "series[0].color: must be a kit colour token", "stats[0].value: is required", "stats[1].label: is required", "pin: must be the index of an x value"]) {
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  }
  check(errorsOf(validateChart({ ...bar, type: "line", stacked: true })).some((x) => x.startsWith("stacked: applies to bar and area")));
  // A typo is a warning, not an error: the chart still shows.
  const typo = validateChart({ ...bar, takeway: "x" });
  check(typo.ok && typo.warnings[0] === "takeway: unknown field, ignored", JSON.stringify(typo));
});

Deno.test("headline numbers replace the legend only when they key every series in its colour", () => {
  const two = { type: "bar", x: { values: ["a"] }, series: [{ name: "PRs", values: [1] }, { name: "master", values: [2] }], y: {} } as ChartSpec;
  check(!statsKeySeries(two), "no stats: the legend");
  check(!statsKeySeries({ ...two, stats: [{ label: "PRs", value: 1, color: "var(--mk-c1)" }] }), "one of two series named: the legend stays");
  check(!statsKeySeries({ ...two, stats: [{ label: "Total", value: 3 }, { label: "Median", value: 1 }] }), "totals name no series");
  check(!statsKeySeries({ ...two, stats: [{ label: "PRs", value: 1, color: "var(--mk-c1)" }, { label: "master", value: 2, color: "#ff0000" }] }), "a colour that is not the series'");
  check(statsKeySeries({ ...two, stats: [{ label: "PRs", value: 1, color: "var(--mk-c1)" }, { label: "master", value: 2, color: "var(--mk-c2)" }] }));
  const own = { ...two, series: [{ name: "PRs", values: [1], color: "#6A56E6" }, { name: "master", values: [2] }] };
  check(statsKeySeries({ ...own, stats: [{ label: "PRs", value: 1, color: "#6a56e6" }, { label: "master", value: 2, color: "var(--mk-c2)" }] }));
});
