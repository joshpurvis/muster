// The kit's parts take options from JSON an agent wrote (<mk-ring>, <mk-spark>, mk.ring …):
// nothing in them may become markup.
import { meter, ring, spark, stat } from "./parts.ts";
import { num } from "./format.ts";
import { check, eq } from "../apps/shared/test_util.ts";

const evil = '132" onload="globalThis.injected=1';
const clean = (html: string) => !/onload|onerror|injected|<script/i.test(html);

Deno.test("ring, spark, meter and stat never put option values into markup as given", () => {
  const r = ring([{ label: "a", value: 3 }, { label: '<b>"x"</b>', value: '2" onload="x' as unknown as number }],
    { size: evil as unknown as number, thickness: '12" onload="globalThis.injected=1' as unknown as number, centerSize: evil as unknown as number, center: "<i>", sub: '"', label: undefined });
  check(clean(r) && r.includes('width="132"') && r.includes('stroke-width="12"'), r);
  const s = spark([1, 2, 3], { width: evil as unknown as number, height: evil as unknown as number, color: 'red" onload="x' });
  check(clean(s) && s.includes('width="120"'), s);
  const m = meter({ label: "l", value: '5" onload="x' as unknown as number, max: '9" onload="x' as unknown as number });
  check(clean(m) && m.includes('aria-valuenow="0"') && m.includes('aria-valuemax="100"'), m);
  const t = stat({ label: "l", value: 1, trend: [1, 2], trendWidth: evil as unknown as number });
  check(clean(t), t);
});

Deno.test("num: finite numbers and numeric strings, clamped; anything else is the fallback", () => {
  eq([num(5, 0), num("7", 0), num(" 8 ", 0), num("", 1), num("8px", 1), num(NaN, 2), num(Infinity, 3), num(null, 4), num(500, 0, 0, 100)], [5, 7, 8, 1, 1, 2, 3, 4, 100]);
});
