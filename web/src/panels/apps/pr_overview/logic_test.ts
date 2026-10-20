import {
  quickMergeBlocked, quickMergeEvent, quickTarget, quickWins,
  age, arrange, followCurrent, canHold, holdEvent, type Item, matches, nextEvent, nextPr, openEvent, type Overview, queue, sortPrs, summary, tally, validateOverview,
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
  bad(pr({ status: "done" }), "prs[0].status: must be one of new, ready, reviewing");
  bad(pr({ risk: "extreme" }), "prs[0].risk");
  bad(pr({ checks: "green" }), "prs[0].checks");
  bad(pr({ review: "LGTM" }), "prs[0].review");
  bad(pr({ additions: -1 }), "prs[0].additions");
  bad(pr({ created_at: "last week" }), "prs[0].created_at");
  bad(pr({ created_at: "2026-10-15" }), "prs[0].created_at", );
  bad(pr({ title: "😀".repeat(501) }), "prs[0].title: is longer than 500");
  check(validateOverview({ ...sampleOverview, ...pr({ title: "😀".repeat(500), created_at: "2026-10-15T09:00:00.5+02:00" }) }).ok, "500 emoji fit, as in the backend");
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
  eq(nums(arrange(o.prs, "risk", "failing").flatMap((x) => x.items)), [801], "checks words");
  eq(nums(arrange(o.prs, "risk", "approved").flatMap((x) => x.items)), [815], "review words");
  eq(nums(arrange(o.prs, "risk", "review required").flatMap((x) => x.items)), [812]);
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
  eq(nextEvent("acme/api", [pr, b]).data, { action: "next", repo: "acme/api", number: 812, order: [815] }, "order never repeats number");
  eq(nextEvent("acme/api", [pr]).data.order, []);
  check(nextEvent("acme/api", [pr, b]).text.includes("#812 (acme/api) now, then #815"));
  eq([openEvent("a/b", pr).kind, holdEvent("a/b", pr).kind, nextEvent("a/b", [pr]).kind], ["open", "hold", "next"]);
});

Deno.test("selection: the agent moving on never retargets a card the user is on", () => {
  eq(followCurrent(812, 812, 815, false), 815, "follows the agent when the user is not on a card");
  eq(followCurrent(801, 812, 815, true), 801, "keeps the user's pick while they navigate");
  eq(followCurrent(801, 812, 812, false), 801, "no change of current: no change");
  eq(followCurrent(801, 812, undefined, false), 801);
});

const win = { number: 815, title: "Fix a typo in the README", author: "lee", additions: 1, deletions: 1, files: 1, checks: "passing", review: "APPROVED",
  risk: "low", quick_win: true, head_sha: "0123456789abcdef0123456789abcdef01234567", base: "master", verdict: "Safe to merge: one word in the docs.",
  status: "ready" };

Deno.test("quick wins: only after a review (head, base, verdict), green and not a draft", () => {
  const o = valid({ ...sampleOverview, prs: [win, ...sampleOverview.prs.filter((p) => p.number !== 815)] });
  const w = o.prs[0];
  eq([w.quick_win, w.status, w.merge_method_default, w.delete_branch_default], [true, "ready", "squash", false]);
  eq(o.prs[1].quick_win, false);
  const bad = (patch: Record<string, unknown>, want: string) => {
    const e = errorsOf(validateOverview({ ...sampleOverview, prs: [{ ...win, ...patch }] }));
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  };
  bad({ head_sha: undefined }, "prs[0].head_sha: is required for a quick win");
  bad({ base: undefined }, "prs[0].base: is required for a quick win");
  bad({ verdict: undefined }, "prs[0].verdict: is required for a quick win");
  bad({ verdict: "a\nb" }, "prs[0].verdict: must be one line");
  bad({ checks: "pending" }, "prs[0].quick_win: needs checks passing");
  bad({ draft: true }, "prs[0].quick_win: cannot be a draft");
  bad({ base: "ma ster" }, "prs[0].base: is not a valid branch name");
  bad({ merge_method_default: "ff" }, "prs[0].merge_method_default: must be one of squash");
  bad({ quick_win: "yes" }, "prs[0].quick_win: must be true or false");
  check(validateOverview({ ...sampleOverview, prs: [{ ...win, quick_win: false, verdict: undefined, base: undefined }] }).ok, "a plain card needs none of it");
});

Deno.test("quick wins: first in the quick sort, the filter and the queue; merge gating", () => {
  const o = valid({ ...sampleOverview, prs: [...sampleOverview.prs.filter((p) => p.number !== 815), win] });
  eq(nums(sortPrs(o.prs, "quick"))[0], 815, "quick wins first");
  eq(nums(arrange(o.prs, "risk", "", true).flatMap((g) => g.items)), [815], "quick wins only");
  eq(nums(arrange(o.prs, "risk", "quick win").flatMap((g) => g.items)), [815], "the filter finds them by name");
  check(nums(queue(sortPrs(o.prs, "risk"))).includes(815), "a ready PR is still to review");
  const w = o.prs.find((p) => p.number === 815)!;
  eq(quickMergeBlocked(w, false), null);
  check(quickMergeBlocked(w, true)!.includes("Waiting"));
  for (const status of ["merged", "merging", "queued", "reviewing", "changes_requested"]) check(quickMergeBlocked({ ...w, status: status as never }, false), status);
  eq(quickMergeBlocked({ ...w, status: "held" }, false), null, "a held quick win can still be merged from here");
  check(quickMergeBlocked(o.prs[0], false)!.startsWith("Not a quick win"));
  eq(nums(quickWins(o.prs)), [815]);
});

Deno.test("quick wins: the merge event has pr_decision's shape and the card's exact command; any card change cancels the confirm", () => {
  const w = valid({ repo: "acme/api", prs: [{ ...win, merge_method_default: "rebase", delete_branch_default: true }] }).prs[0];
  const ev = quickMergeEvent("acme/api", w);
  eq(ev.kind, "merge");
  eq(ev.data, { action: "merge", repo: "acme/api", number: 815, base: "master", method: "rebase", delete_branch: true, head_sha: win.head_sha, answers: {}, options: [] });
  check(ev.text.endsWith(`Run exactly: gh pr merge 815 --repo acme/api --rebase --delete-branch --match-head-commit ${win.head_sha}`), ev.text);
  const t = quickTarget("acme/api", w);
  eq(quickTarget("acme/api", { ...w }), t);
  for (const change of [{ head_sha: "f".repeat(40) }, { base: "release" }, { verdict: "Changed." }, { checks: "failing" }, { status: "reviewing" }]) {
    check(quickTarget("acme/api", { ...w, ...change } as never) !== t, JSON.stringify(change));
  }
  check(quickTarget("acme/web", w) !== t, "another repo");
});

Deno.test("summary: open counts every PR not merged yet, with its checks and reviews", () => {
  const o = valid(sampleOverview);
  const st = (n: number, status: string) => ({ ...o.prs.find((p) => p.number === n)!, status } as Item);
  const c = summary([st(812, "changes_requested"), st(801, "queued"), st(820, "merging"), st(815, "merged")]);
  eq([c.open, c.failing, c.review, c.merged], [3, 1, 1, 1]);
  const all = summary(o.prs);
  eq([all.open, all.quick], [4, quickWins(o.prs).length]);
});
