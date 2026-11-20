// The merges this window started (docs/UI.md "PRs to merge"): the queue of prs_merge_queue.ts
// wired to Muster. Lives outside the tab, so a card's countdown keeps going when the human moves
// to another card, clicks another Merge, opens a picture or the list, or goes to another tab; at
// 0 its `pr.merge` is sent, and the card shows the result when Muster sends it. Nothing here
// moves focus or the view: a failed send is said in a toast, and the card says how it went.
// Closing or reloading the window ends the countdowns with it (nothing is sent).
import { effect, signal } from "@preact/signals";
import { mergeQueue, type Queued, type Seen } from "./prs_merge_queue.ts";
import { canMerge, COUNTDOWN_S, mergePr, offReason, prs, target } from "./prs.ts";
import { rpc, RpcError } from "./rpc.ts";
import { toast } from "./state.ts";

/** Each card counting down or being sent from this window, by card id. */
export const queuedMerges = signal<ReadonlyMap<number, Queued>>(new Map());
/** Cards sent to merge from this window since the tab was shown: the Waiting strip keeps their
 * places (a merged one is shown there, marked merged). */
export const mergedHere = signal<ReadonlySet<number>>(new Set());
/** Why a card's last send from here failed, for the post and commit tried (shown on that card
 * until Merge is clicked again). */
export const failedMerges = signal<ReadonlyMap<number, { text: string; post: number; sha: string }>>(new Map());
/** Cards whose `pr.merge` went out on an open connection (until its failure is handled). */
const sentOpen = new Set<number>();
const LOST = "its answer was lost on the way, so Muster may be merging it: the card updates when Muster is reachable " +
  "again, and Merge again can't merge it twice";

/** Forget the cards whose merge has ended (the tab shown again, the filter changed); one still
 * counting down or merging keeps its place. */
export function clearSettled() {
  const q = queuedMerges.peek();
  const cards = prs.peek();
  mergedHere.value = new Set([...mergedHere.peek()].filter((id) => q.has(id) || cards.get(id)?.status === "merging"));
}

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const nameOf = (id: number) => {
  const p = prs.peek().get(id);
  return p ? target(p) : "The pull request";
};

const queue = mergeQueue(COUNTDOWN_S * 1000, {
  now: () => Date.now(),
  setTimer: (fn, ms) => setTimeout(fn, ms),
  clearTimer: (t) => clearTimeout(t as number),
  // Kept in the strip from the moment it is sent: an answer lost on the way can't drop it.
  send: (seen: Seen) => {
    mergedHere.value = new Set(mergedHere.peek()).add(seen.id);
    if (rpc.isOpen) sentOpen.add(seen.id);
    return mergePr(seen).then((r) => { sentOpen.delete(seen.id); return r; });
  },
  // Not sent (disconnected, asleep, or refused at the click): the card is back to its Merge, and
  // says why until Merge is clicked again.
  failed: (seen, e) => {
    const p = prs.peek().get(seen.id);
    // Sent on an open connection and not answered by Muster (lost, timed out): it may have been
    // taken, so the card keeps its place and says the outcome is unknown.
    const lost = !(e instanceof RpcError && e.answered) && sentOpen.has(seen.id);
    sentOpen.delete(seen.id);
    if (!lost && p?.status !== "merging" && p?.status !== "merged") {
      const kept = new Set(mergedHere.peek());
      kept.delete(seen.id);
      mergedHere.value = kept;
    }
    const text = lost ? LOST : errText(e);
    failedMerges.value = new Map(failedMerges.peek()).set(seen.id, { text, post: seen.post, sha: seen.sha });
    toast("error", `${nameOf(seen.id)}: ${text}`, 8000);
  },
  changed: (all) => { queuedMerges.value = all; },
});

/** Start card `seen.id`'s countdown (false: it has one already). */
export function startMerge(seen: Seen): boolean {
  if (failedMerges.peek().has(seen.id)) {
    const f = new Map(failedMerges.peek());
    f.delete(seen.id);
    failedMerges.value = f;
  }
  return queue.start(seen);
}
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
