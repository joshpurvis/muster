// The merges this window started (docs/UI.md "PRs to merge"): the queue of prs_merge_queue.ts
// wired to Muster. Lives outside the tab, so a card's countdown keeps going when the human moves
// to another card, clicks another Merge, opens a picture or the list, or goes to another tab; at
// 0 its `pr.merge` is sent, and the card shows the result when Muster sends it. Nothing here
// moves focus or the view: a failed send is said in a toast, and the card says how it went.
// Closing or reloading the window ends the countdowns with it (nothing is sent).
import { effect, signal } from "@preact/signals";
import { mergeQueue, type Queued, type Seen } from "./prs_merge_queue.ts";
import { canMerge, COUNTDOWN_S, mergePr, offReason, prs, target } from "./prs.ts";
import { toast } from "./state.ts";

/** Each card counting down or being sent from this window, by card id. */
export const queuedMerges = signal<ReadonlyMap<number, Queued>>(new Map());
/** Cards merged from this window since the tab was shown: the Waiting strip keeps their places. */
export const mergedHere = signal<ReadonlySet<number>>(new Set());

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const nameOf = (id: number) => {
  const p = prs.peek().get(id);
  return p ? target(p) : "The pull request";
};

const queue = mergeQueue(COUNTDOWN_S * 1000, {
  now: () => Date.now(),
  setTimer: (fn, ms) => setTimeout(fn, ms),
  clearTimer: (t) => clearTimeout(t as number),
  send: async (seen: Seen) => {
    await mergePr(seen);
    mergedHere.value = new Set(mergedHere.peek()).add(seen.id);
  },
  // Not sent (disconnected, or refused at the click): the card is back to its Merge.
  failed: (seen, e) => toast("error", `${nameOf(seen.id)}: ${errText(e)}`, 8000),
  changed: (all) => { queuedMerges.value = all; },
});

/** Start card `seen.id`'s countdown (false: it has one already). */
export const startMerge = (seen: Seen) => queue.start(seen);
/** Cancel card `id`'s countdown (false: none, or sent already). */
export const cancelMerge = (id: number) => queue.stop(id);

// A card that stops being mergeable, or is posted again, while it counts down: its countdown ends
// and a toast says why (the merge would be refused anyway).
effect(() => {
  const cards = prs.value;
  for (const [id, q] of queuedMerges.peek()) {
    if (q.phase !== "count") continue;
    const p = cards.get(id);
    if (p && canMerge(p) && p.head_sha === q.seen.sha && p.post === q.seen.post && p.github?.base === q.seen.base) continue;
    queue.stop(id);
    const why = !p ? "the card is gone" : offReason(p) ?? "the card changed since you clicked Merge (look at it again)";
    toast("error", `${p ? target(p) : "The pull request"} wasn't merged: ${why}`, 8000);
  }
});
