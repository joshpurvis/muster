import { compact, decimals, esc, fmt, fmtFull, niceTicks, parseLabel, rich, safeColor, smooth, tickLabels, trendLine } from "./format.ts";
import { ringLengths } from "./parts.ts";
import { check, eq } from "../apps/shared/test_util.ts";

Deno.test("numbers compact to k, M, B and T with a true minus sign", () => {
  eq([1234, 12500, 999950, 2.5e9, 2500e6, 1.2e12, -4200, -0, 0.5, 0.034, 12345.678].map((v) => fmt(v)),
    ["1,234", "12.5k", "1M", "2.5B", "2.5B", "1.2T", "−4,200", "0", "0.5", "0.034", "12.3k"]);
  // Never "2500M" or "1000k": a rounded value moves up a unit.
  eq([compact(999_999), compact(999_960_000), compact(-1_500_000)], ["1M", "1B", "−1.5M"]);
  // Past a thousand trillion, and for tiny values, scientific notation instead of nonsense.
  eq([fmt(3e15), fmt(2.5e-11), fmt(-7e-9)], ["3e15", "2.5e-11", "−7e-9"]);
  // Missing values read as a dash, never as zero.
  eq([fmt(null), fmt(undefined), fmt(NaN), fmt(Infinity)], ["–", "–", "–", "–"]);
});

Deno.test("formats: usd, pct (already percents), ms, s on the clock, int and plain", () => {
  eq([fmt(4480, "usd"), fmt(4.5, "usd"), fmt(-5, "usd"), fmt(12500, "usd")], ["$4,480", "$4.50", "−$5", "$12.5k"]);
  eq([fmt(0.34, "pct"), fmt(2.1, "pct"), fmt(-0.04, "pct")], ["0.34%", "2.1%", "−0.04%"]);
  eq([fmt(158, "ms"), fmt(1250, "ms"), fmt(90_000, "ms")], ["158 ms", "1.3 s", "1m 30s"]);
  eq([fmt(4.6, "s"), fmt(114, "s"), fmt(120, "s"), fmt(3900, "s"), fmt(-30, "s")], ["4.6 s", "1m 54s", "2m", "1h 05m", "−30 s"]);
  eq([fmt(1216, "int"), fmt(1215.6, "int"), fmt(1470, "plain"), fmt(2026, "plain")], ["1,216", "1,216", "1470", "2026"]);
  eq(fmt("Fri"), "Fri");
});

Deno.test("ticks land on round steps, and on clock steps for seconds", () => {
  eq(niceTicks(0, 124, 4), { min: 0, max: 150, step: 50, ticks: [0, 50, 100, 150] });
  eq(niceTicks(-120, 340, 4).ticks, [-200, 0, 200, 400]);
  eq(niceTicks(0, 2.1, 3).ticks, [0, 1, 2, 3]);
  // Never "5m 50s": durations step by 15 s, 30 s, 1 m, 2 m, 5 m …
  eq(niceTicks(0, 350, 3, "s").ticks, [0, 120, 240, 360]);
  eq(niceTicks(0, 3000, 4, "s").step, 900);
  eq(niceTicks(0, 40, 4, "s").step, 10);
  eq(tickLabels(niceTicks(0, 350, 3, "s").ticks, "s"), ["0 s", "2m", "4m", "6m"]);
});

Deno.test("degenerate ranges still give distinct, increasing ticks", () => {
  eq(niceTicks(5, 5).ticks, [4.5, 4.75, 5, 5.25, 5.5]);
  eq(niceTicks(0, 0).ticks, [0, 0.25, 0.5, 0.75, 1]);
  eq(niceTicks(10, 0).min, 0);
  const nan = niceTicks(NaN, Infinity);
  check(nan.ticks.length >= 2 && nan.max > nan.min, JSON.stringify(nan));
  const flat = niceTicks(1e-300, 1e-300);
  check(flat.ticks.length >= 2 && flat.max > flat.min, JSON.stringify(flat));
  const big = niceTicks(1e17, 1e17 + 1);
  check(big.ticks.length >= 2 && big.ticks.every((t, i) => !i || t > big.ticks[i - 1]), JSON.stringify(big));
  eq([decimals(1e-10), decimals(0.25), decimals(2.5), decimals(500)], [10, 2, 1, 0]);
});

Deno.test("tick labels are distinct and say their own value", () => {
  eq(tickLabels(niceTicks(1e6, 2e6, 4).ticks), ["1.00M", "1.25M", "1.50M", "1.75M", "2.00M"]);
  eq(tickLabels(niceTicks(0, 2.5e9, 4).ticks), ["0", "1B", "2B", "3B"]);
  eq(tickLabels(niceTicks(0, 1e-10, 4).ticks), ["0", "2.5e-11", "5e-11", "7.5e-11", "1e-10"]);
  eq(tickLabels([0, 1, 2, 3], "pct"), ["0%", "1%", "2%", "3%"]);
  eq(tickLabels([-0.5, 0, 0.5]), ["−0.5", "0", "0.5"]);
  eq([parseLabel("1.25M"), parseLabel("−$4.5k"), parseLabel("2%"), parseLabel("2m")], [1_250_000, -4500, 2, null]);
});

Deno.test("smooth lines never overshoot monotone data", () => {
  const pts: [number, number][] = [[0, 100], [10, 80], [20, 79], [30, 20]];
  const d = smooth(pts);
  const ys = [...d.matchAll(/[ ,C]([\d.]+),([\d.]+)/g)].map((m) => Number(m[2]));
  check(ys.every((y) => y <= 100 && y >= 20), d);
  eq(smooth([[0, 1], [5, 2]]), "M0.0,1.0L5.0,2.0");
});

Deno.test("trend line, ring segments and colours", () => {
  const t = trendLine([[0, 1], [1, 3], [2, 5]]);
  check(t && Math.abs(t.b - 2) < 1e-9 && Math.abs(t.a - 1) < 1e-9, JSON.stringify(t));
  eq(trendLine([[1, 1], [1, 2]]), null);
  // 3 failures out of 1,216 tests stay visible as an arc of at least 4 px.
  const lens = ringLengths([1213, 3], 100, 4);
  check(lens[1] >= 4 && Math.abs(lens[0] + lens[1] - 100) < 1e-9, JSON.stringify(lens));
  eq(ringLengths([0, 0], 100, 4), [0, 0]);
  eq([safeColor("var(--mk-c2)", "x"), safeColor("#6a56e6", "x"), safeColor("red;background:url(x)", "x"), safeColor(undefined, "x")],
    ["var(--mk-c2)", "#6a56e6", "x", "x"]);
});

Deno.test("text is escaped; takeaways allow **bold** and `code` only", () => {
  eq(esc(`<b a="1">'&`), "&lt;b a=&quot;1&quot;&gt;&#39;&amp;");
  eq(rich("PR builds used **1.75×** the minutes; see `9be04f1` <script>"), "PR builds used <b>1.75×</b> the minutes; see <code>9be04f1</code> &lt;script&gt;");
});

Deno.test("int axes get whole-number ticks, never repeated labels", () => {
  for (const [lo, hi] of [[0, 1], [0, 3], [0, 10], [0, 7], [2, 3]]) {
    const t = niceTicks(lo, hi, 4, "int");
    check(t.ticks.every(Number.isInteger), `${lo}–${hi}: ${t.ticks}`);
    const labels = tickLabels(t.ticks, "int");
    check(new Set(labels).size === labels.length, `${lo}–${hi}: ${labels}`);
  }
  // Fixed fractional bounds still print distinct labels.
  const l = tickLabels([0, 0.25, 0.5, 0.75, 1], "int");
  check(new Set(l).size === 5, l.join());
});

Deno.test("fmtFull: data table values in full, so neighbours stay apart", () => {
  eq([12345, 12346, 12347].map((v) => fmtFull(v)), ["12,345", "12,346", "12,347"]);
  eq([fmtFull(1234567.891, "usd"), fmtFull(-12, "usd"), fmtFull(12345.6, "int"), fmtFull(0.3412, "pct"), fmtFull(61250, "ms"), fmtFull(null)],
    ["$1,234,567.89", "−$12", "12,346", "0.3412%", "61,250 ms", "–"]);
  eq([fmtFull(1234.5, "plain"), fmtFull(-0.000123)], ["1234.5", "−0.000123"]);
});
