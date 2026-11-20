import {
  banner, canMerge, cardState, dotClass, firstLine, pillMark, toldText, groupOf, nextReady, offReason, otherSide, pairs, pairText, rules, sortMerged,
  sortWaiting, stepPicture, tagText, viewOrder, worthShowing,
} from "./prs_model.ts";
import type { Pr, PrRead, PrVerdict } from "./types.ts";

function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}

const HEAD = "9f3c2ab0c41d9e77b2a5f08c3d6e19a4b7f2c5d8";
const read = (over: Partial<PrRead> = {}): PrRead => ({
  at: 5, state: "OPEN", draft: false, head_sha: HEAD, base: "master", mergeable: "MERGEABLE", merge_state: "CLEAN",
  title: "Show a word count under each note", url: "https://github.com/acme/notes-app/pull/214", author: "builder-2",
  additions: 86, deletions: 12, files: 4, checks: { total: 3, passed: 3, failed: 0, running: 0 }, ...over,
});
const ok: PrVerdict = { ok: true, state: "pass", commit: "pass", base: "pass", checks: "pass" };

function pr(id: number, over: Partial<Pr> = {}): Pr {
  return {
    id, card: `P${id}`, status: "waiting", created_at: id * 1000, updated_at: id * 1000,
    poster: { label: "builder-2", session_id: 3, kind: "codex", agent_id: 7 }, repo: "acme/notes-app", number: 200 + id,
    head_sha: HEAD, summary: "s", who_notices: "w", tests: "t", undo: "u", after_merge: "a", post: 1,
    github: read(), verdict: ok, ...over,
  };
}

Deno.test("a card's state comes from its status, its refusal and its last read", () => {
  equal(cardState(pr(1)), "ready");
  equal(canMerge(pr(1)), true);
  equal(cardState(pr(1, { github: undefined, verdict: undefined })), "reading");
  equal(cardState(pr(1, { verdict: { ...ok, ok: false, checks: "wait", reason: "a check is still running" } })), "running");
  equal(cardState(pr(1, { verdict: { ...ok, ok: false, checks: "fail", reason: "1 of 3 checks failed" } })), "blocked");
  // A refusal stands until a later read passes (the backend clears it then).
  equal(cardState(pr(1, { refused: { at: 9, reason: "it conflicts with master" } })), "refused");
  equal(canMerge(pr(1, { refused: { at: 9, reason: "x" } })), false);
  for (const status of ["stale", "merging", "merged", "closed"] as const) equal(canMerge(pr(1, { status })), false);
  equal(cardState(pr(1, { status: "changes_asked" })), "gone");
});

Deno.test("waiting cards: ready first, then can't merge yet, then not now; oldest first in each", () => {
  const list = sortWaiting([
    pr(1, { later: 5 }), pr(2, { verdict: { ...ok, ok: false, checks: "wait" } }), pr(3), pr(4, { status: "stale" }), pr(5),
    pr(6, { status: "merged" }), pr(7, { status: "changes_asked" }), pr(8, { status: "closed" }),
  ]);
  equal(list.map((p) => p.id), [3, 5, 2, 4, 1]);
  equal(list.map(groupOf), ["ready", "ready", "cant", "cant", "later"]);
  const merged = sortMerged([pr(1, { status: "merged", merge: { at: 1, sha: HEAD, done_at: 50 } }),
    pr(2, { status: "merged", merge: { at: 1, sha: HEAD, done_at: 90 } }), pr(3)]);
  equal(merged.map((p) => p.id), [2, 1]);
  equal(nextReady(list, list[0])?.id, 5);
});

Deno.test("a card merged from this window keeps its place in the waiting strip, so nothing shifts", () => {
  const cards = [pr(1), pr(2, { status: "merged" }), pr(3), pr(4, { status: "merged" })];
  equal(sortWaiting(cards).map((p) => p.id), [1, 3]);
  equal(sortWaiting(cards, new Set([2])).map((p) => p.id), [1, 2, 3]);
});

Deno.test("the tab shows while something waits or merged lately", () => {
  const now = 100 * 3600_000;
  equal(worthShowing([], now), false);
  equal(worthShowing([pr(1)], now), true);
  equal(worthShowing([pr(1, { status: "merged", updated_at: now - 3600_000 })], now), true);
  equal(worthShowing([pr(1, { status: "merged", updated_at: now - 25 * 3600_000 })], now), false);
  equal(worthShowing([pr(1, { status: "withdrawn" }), pr(2, { status: "changes_asked" })], now), false);
});

Deno.test("the four rules, in order, with the words the design gives", () => {
  equal(rules(pr(1, { github: undefined })), null);
  equal(rules(pr(1))!.map((r) => [r.mark, r.text, r.sha ?? ""]), [
    ["pass", "Open, and not a draft", ""], ["pass", "The commit builder-2 posted,", "9f3c2ab"],
    ["pass", "Fits onto master without conflicts", ""], ["pass", "Checks: 3 of 3 passed", ""],
  ]);
  const stale = rules(pr(1, { status: "stale", github: read({ head_sha: "e51a9b2" + HEAD.slice(7) }),
    verdict: { ...ok, ok: false, commit: "warn" } }))!;
  equal([stale[1].mark, stale[1].sha], ["warn", "e51a9b2"]);
  const none = rules(pr(1, { github: read({ checks: { total: 0, passed: 0, failed: 0, running: 0 } }), verdict: { ...ok, checks: "none" } }))!;
  equal([none[3].mark, none[3].text], ["none", "GitHub runs no checks here"]);
  const failed = rules(pr(1, {
    github: read({ draft: true, mergeable: "CONFLICTING", checks: { total: 3, passed: 2, failed: 1, running: 0, failing: ["lint"] } }),
    verdict: { ok: false, state: "fail", commit: "pass", base: "fail", checks: "fail", reason: "it is still a draft" },
  }))!;
  equal(failed.map((r) => r.text), ["Still a draft", "The commit builder-2 posted,", "Conflicts with master", "Checks: 1 of 3 failed (lint)"]);
  const running = rules(pr(1, { github: read({ mergeable: "UNKNOWN", checks: { total: 2, passed: 1, failed: 0, running: 1 } }),
    verdict: { ...ok, ok: false, base: "wait", checks: "wait" } }))!;
  equal([running[2].mark, running[3].text], ["wait", "Checks: 1 of 2 passed, 1 still running"]);
});

Deno.test("dots, tags, banners and why Merge is off", () => {
  equal(dotClass(pr(1)), "");
  equal(dotClass(pr(1, { github: read({ checks: { total: 0, passed: 0, failed: 0, running: 0 } }) })), "none");
  equal(dotClass(pr(1, { later: 3 })), "none");
  equal(dotClass(pr(1, { status: "stale" })), "warn");
  equal(dotClass(pr(1, { refused: { at: 1, reason: "x" } })), "bad");
  equal(tagText(pr(1, { verdict: { ...ok, ok: false, base: "fail" }, refused: { at: 1, reason: "x" } })).text, "conflict");
  equal(tagText(pr(1, { status: "merged" })), { text: "merged ✓", tone: "ok" });
  equal(offReason(pr(1)), null);
  equal(offReason(pr(1, { status: "stale" })), "new commits since it was posted");
  const clock = (at: number) => `t${at}`;
  equal(banner(pr(1), clock), null);
  equal(banner(pr(1, { status: "stale", stale_told: true }), clock)!.head, "Changed since it was posted: new commits");
  const refused = banner(pr(1, { refused: { at: 7, reason: "it conflicts with master", github_said: "not mergeable" }, merge: { at: 6, sha: HEAD } }), clock)!;
  equal(refused.head, "Not merged: it conflicts with master");
  equal(refused.body.includes("You clicked Merge at t6"), true);
  equal(refused.body.includes("builder-2 was told"), true);
});

Deno.test("pictures pair in posting order; the large view steps before 1, after 1, before 2…", () => {
  const pic = (side: "before" | "after") => ({ path: "/x.png", caption: side, side, file: "1/0.png" });
  const p = pr(1, { pictures: [pic("before"), pic("before"), pic("after"), pic("after"), pic("after")] });
  equal(pairs(p), [{ before: 0, after: 2 }, { before: 1, after: 3 }, { before: null, after: 4 }]);
  equal(viewOrder(p), [0, 2, 1, 3, 4]);
  equal(stepPicture(p, 4, 1), 0);
  equal(stepPicture(p, 0, -1), 4);
  equal(stepPicture(p, 2, 1), 1);
  equal(otherSide(p, 1), 3);
  equal(otherSide(p, 4), null);
  equal(pairText(p, 3), "pair 2 of 3");
  equal(pairs(pr(1)), []);
});

Deno.test("an unknown result, a restart and GitHub's words are told as they are", () => {
  const clock = (at: number) => `t${at}`;
  const unknown = banner(pr(1, { refused: { at: 7, reason: "gh didn't answer in time", unknown: true }, merge: { at: 6, sha: HEAD } }), clock)!;
  equal(unknown.head, "Not known yet: gh didn't answer in time");
  equal(unknown.body.includes("Nothing was merged"), false);
  equal(unknown.body.includes("may still merge"), true);
  const restart = banner(pr(1, { refused: { at: 7, reason: "the merge didn't start: Muster restarted first", not_told: true } }), clock)!;
  equal(restart.body.includes("builder-2 wasn't told"), true);
  equal(firstLine("X is not mergeable: policy.\nadd the `--auto` flag.\nadd the `--admin` flag."), "X is not mergeable: policy.");
  const said = banner(pr(1, { refused: { at: 7, reason: "GitHub refused the merge", github_said: "X is not mergeable.\nuse `--admin`" } }), clock)!;
  equal(said.body.includes("--admin"), false);
  equal(said.body.includes("GitHub said: “X is not mergeable.”"), true);
  // Whether the poster got the message, from the card's own record.
  const note = (status: string, n?: string) => [{ kind: "merged" as const, at: 1, from: "muster", delivery: { status, note: n } as never }];
  equal(toldText(pr(1, { outbox: note("delivered") }), "merged"), "builder-2 was told");
  equal(toldText(pr(1, { outbox: note("pending") }), "merged"), "builder-2 is being told");
  equal(toldText(pr(1, { outbox: note("undelivered", "not delivered: the agent was removed") }), "merged"),
    "Muster couldn't tell builder-2: the agent was removed");
});

Deno.test("auto-merge and an incomplete reply fail the first rule; pills carry a glyph and a word", () => {
  equal(rules(pr(1, { github: read({ auto_merge: true }), verdict: { ...ok, ok: false, state: "fail" } }))![0].text,
    "Auto-merge is on for it on GitHub");
  equal(rules(pr(1, { github: read({ incomplete: true }), verdict: { ...ok, ok: false, state: "fail" } }))![0].text,
    "GitHub's reply lacked something Muster checks");
  equal(pillMark(pr(1)), { glyph: "✓", cls: "", status: "checks passed" });
  equal(pillMark(pr(1, { status: "stale" })).glyph, "!");
  equal(pillMark(pr(1, { refused: { at: 1, reason: "x" }, verdict: { ...ok, ok: false, base: "fail" } })).glyph, "✕");
  equal(pillMark(pr(1, { later: 5 })).status, "not now, checks passed");
});
