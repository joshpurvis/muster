import {
  age, arrange, canHold, holdEvent, type Item, matches, nextEvent, nextPr, openEvent, type Overview, queue, sortPrs, tally, validateOverview,
} from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

export const sampleOverview = {
  repo: "acme/api",
  prs: [
    { number: 812, title: "Rate-limit failed logins", author: "dana", created_at: "2026-10-15T09:00:00Z", additions: 120, deletions: 14, files: 6, checks: "passing", review: "REVIEW_REQUIRED", risk: "medium", labels: ["security"] },
    { number: 815, title: "Fix typo in README", author: "lee", created_at: "2026-10-18T09:00:00Z", additions: 1, deletions: 1, files: 1, checks: "passing", review: "APPROVED", risk: "low" },
    { number: 801, title: "Rewrite the session store", author: "sam", created_at: "2026-10-01T09:00:00Z", additions: 900, deletions: 450, files: 31, checks: "failing", review: "", risk: "high", draft: true },
    { number: 820, title: "Bump serde", author: "bot", created_at: "2026-10-19T08:00:00Z", additions: 4, deletions: 4, files: 2, checks: "pending", risk: "low" },
  ],
};

function valid(data: unknown): Overview {
  const r = validateOverview(data);
  if (!r.ok) throw new Error(r.errors.join("\n"));
  return r.value;
}
const nums = (xs: Item[]) => xs.map((x) => x.number);

Deno.test("overview input: defaults, gh's review values and dates", () => {
  const o = valid(sampleOverview);
  eq([o.sort, o.actions, o.current], ["risk", [], undefined]);
  eq(o.prs.map((p) => p.status), ["new", "new", "new", "new"]);
  eq(o.prs[2].review, "NONE");
  eq(o.prs[0].created, Date.parse("2026-10-15T09:00:00Z"));
  check(valid({ repo: "acme/api", prs: [] }).prs.length === 0);
});

Deno.test("overview input: names the bad field", () => {
  const bad = (patch: Record<string, unknown>, want: string) => {
    const e = errorsOf(validateOverview({ ...sampleOverview, ...patch }));
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  };
  const pr = (p: Record<string, unknown>) => ({ prs: [{ ...sampleOverview.prs[0], ...p }] });
  bad({ repo: "acme" }, "repo: must look like owner/name");
  bad({ prs: undefined }, "prs: is required");
  bad(pr({ number: 0 }), "prs[0].number: must be a positive whole number");
  bad({ prs: [sampleOverview.prs[0], sampleOverview.prs[0]] }, "prs[1].number: #812 is listed twice");
  bad(pr({ title: "" }), "prs[0].title: must not be empty");
  bad(pr({ status: "done" }), "prs[0].status: must be one of new, reviewing");
  bad(pr({ risk: "extreme" }), "prs[0].risk");
  bad(pr({ checks: "green" }), "prs[0].checks");
  bad(pr({ review: "LGTM" }), "prs[0].review");
  bad(pr({ additions: -1 }), "prs[0].additions");
  bad(pr({ created_at: "last week" }), "prs[0].created_at");
  bad(pr({ url: "http://github.com/x" }), "prs[0].url: must be an https URL");
  bad(pr({ head_sha: "abc" }), "prs[0].head_sha");
  bad(pr({ labels: [""] }), "prs[0].labels[0]");
  bad({ current: 999 }, "current: #999 is not in prs");
  bad({ sort: "newest" }, "sort: must be one of risk, quick, oldest");
  bad({ actions: [{ id: "x", label: "X", key: "j", description: "d" }] }, "actions[0].key: \"j\" is a built-in key");
});

Deno.test("sorts: riskiest, quick wins, oldest", () => {
  const prs = valid(sampleOverview).prs;
  eq(nums(sortPrs(prs, "risk")), [801, 812, 820, 815], "high, medium, then low with pending checks before passing");
  eq(nums(sortPrs(prs, "quick")), [815, 812, 820, 801], "ready (checks passing) and small first; pending checks later; drafts last");
  eq(nums(sortPrs(prs, "oldest")), [801, 812, 815, 820]);
  const unknown = valid({ repo: "a/b", prs: [{ number: 2, title: "t", author: "a" }, { number: 1, title: "t", author: "a", risk: "low" }] }).prs;
  eq(nums(sortPrs(unknown, "risk")), [1, 2], "unknown risk sorts after low");
  eq(nums(sortPrs(unknown, "oldest")), [1, 2], "no date: by number");
});

Deno.test("groups: held at the end, decided before it; filter matches every word", () => {
  const o = valid({ ...sampleOverview, prs: sampleOverview.prs.map((p) => ({ ...p, status: p.number === 812 ? "held" : p.number === 815 ? "merged" : "new" })) });
  const g = arrange(o.prs, "risk", "");
  eq(g.map((x) => [x.group, nums(x.items)]), [["todo", [801, 820]], ["done", [815]], ["held", [812]]]);
  eq(tally(o.prs), { todo: 2, done: 1, held: 1 });
  check(!canHold(o.prs[0]) && canHold(o.prs[2]));
  eq(nums(arrange(o.prs, "risk", "session sam").flatMap((x) => x.items)), [801]);
  eq(nums(arrange(o.prs, "risk", "SECURITY").flatMap((x) => x.items)), [812], "labels, case-insensitive");
  check(matches(o.prs[2], "#801 draft") && matches(o.prs[2], "high risk") && !matches(o.prs[2], "nope"));
  eq(arrange(o.prs, "risk", "zzz"), []);
});

Deno.test("next: the first new or skipped PR after the current one, in the shown order, wrapping", () => {
  const o = valid({ ...sampleOverview, prs: sampleOverview.prs.map((p) => ({ ...p, status: p.number === 820 ? "merged" : "new" })) });
  const order = sortPrs(o.prs, "risk"); // 801, 812, 820, 815
  eq(nextPr(order)?.number, 801);
  eq(nums(queue(order, 812)), [815, 801], "the rest to go through, from after the current one");
  eq(nextPr(order, 801)?.number, 812);
  eq(nextPr(order, 812)?.number, 815, "skips the merged one");
  eq(nextPr(order, 815)?.number, 801, "wraps");
  const one = valid({ repo: "a/b", prs: [{ number: 1, title: "t", author: "a" }, { number: 2, title: "t", author: "a", status: "held" }] });
  eq(nextPr(one.prs, 1), undefined, "the current one is not offered again; held ones never");
  eq(nextPr(one.prs)?.number, 1);
});

Deno.test("age and events", () => {
  const now = Date.parse("2026-10-19T12:00:00Z");
  eq([age(now - 5 * 60_000, now), age(now - 3 * 3600_000, now), age(now - 4 * 86400_000, now), age(now - 90 * 86400_000, now), age(undefined, now)],
    ["5m", "3h", "4d", "3mo", ""]);
  const pr = valid(sampleOverview).prs[0];
  eq(openEvent("acme/api", pr).data, { action: "open", repo: "acme/api", number: 812 });
  eq(holdEvent("acme/api", pr).data, { action: "hold", repo: "acme/api", number: 812 });
  const [, b] = valid(sampleOverview).prs;
  eq(nextEvent("acme/api", [pr, b]).data, { action: "next", repo: "acme/api", number: 812, order: [812, 815] });
  check(nextEvent("acme/api", [pr, b]).text.includes("#812 (acme/api) now, then #815"));
  eq([openEvent("a/b", pr).kind, holdEvent("a/b", pr).kind, nextEvent("a/b", [pr]).kind], ["open", "hold", "next"]);
});
