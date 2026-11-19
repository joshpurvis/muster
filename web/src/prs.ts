// Pull requests agents put up for the human to merge (docs/UI.md "PRs to merge", docs/PROTOCOL.md
// "Pull requests to merge"). `pr.list` after each hello, then `pr_upserted` events; `pr.refresh`
// reads GitHub again (when the tab is shown, every 2 minutes while it is, and with Check again).
// Merging, asking for changes and Not now go through `pr.merge`, `pr.ask_changes` and `pr.later`,
// which the daemon accepts only from a click in this window; `pr.merge` is sent only once that
// card's 5 second countdown has run out (prs_merges.ts: each card's own, in the background).
//
// The pure part (states, order, the rules and the words) is prs_model.ts, re-exported here.

import { computed, signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import type { Pr } from "./types.ts";
import { sortMerged, sortWaiting } from "./prs_model.ts";
import type { Seen } from "./prs_merge_queue.ts";

export * from "./prs_model.ts";

/** Every card this window knows, by id. */
export const prs = signal<Map<number, Pr>>(new Map());
/** The backend has pull request cards (false: an older one; the tab stays away). */
export const prsSupported = signal(false);
/** The card shown (null: the first of the list). */
export const selectedPr = signal<number | null>(null);
export const prsFilter = signal<"waiting" | "merged">("waiting");

export const waitingPrs = computed(() => sortWaiting(prs.value.values()));
export const mergedPrs = computed(() => sortMerged(prs.value.values()));
/** The count on the tab: cards waiting for the human. */
export const waitingPrCount = computed(() => (prsSupported.value ? waitingPrs.value.length : 0));

let changedDuringLoad: Map<number, Pr> | null = null;

/** Record card `p` unless this window has a newer copy (every change raises `updated_at`). */
export function applyPr(p: Pr) {
  const have = prs.value.get(p.id);
  if (have && have.updated_at > p.updated_at) return;
  changedDuringLoad?.set(p.id, p);
  // A withdrawn card stays (hidden by every list) so an older copy arriving late can't bring it back.
  const next = new Map(prs.value);
  next.set(p.id, p);
  prs.value = next;
}

function applyList(list: Pr[]) {
  for (const p of list) applyPr(p);
}

export async function loadPrs() {
  const changed = changedDuringLoad = new Map();
  try {
    const r = await rpc.call<{ prs: Pr[] }>("pr.list", {});
    const loaded = new Map(r.prs.map((p) => [p.id, p]));
    // Cards the list leaves out (withdrawn, changes asked) are kept as this window last saw them.
    for (const [id, p] of prs.peek()) {
      if (!loaded.has(id) && (p.status === "withdrawn" || p.status === "changes_asked")) loaded.set(id, p);
    }
    for (const [id, p] of changed) {
      const l = loaded.get(id);
      if (!l || l.updated_at <= p.updated_at) loaded.set(id, p);
    }
    prs.value = loaded;
    prsSupported.value = true;
  } catch (e) {
    if (/unknown method/i.test(e instanceof Error ? e.message : String(e))) prsSupported.value = false;
  } finally {
    if (changedDuringLoad === changed) changedDuringLoad = null;
  }
}

/** Read GitHub again: every waiting card (at most once a minute each), or card `id` now (Check
 * again: at most every 3 seconds). */
export async function refreshPrs(id?: number): Promise<void> {
  if (!prsSupported.peek()) return;
  if (id === undefined) {
    const r = await rpc.call<{ prs: Pr[] }>("pr.refresh", {});
    applyList(r.prs);
  } else {
    applyPr(await rpc.call<Pr>("pr.refresh", { id, force: true }));
  }
}

export type { Seen } from "./prs_merge_queue.ts";

/** Merge what the human saw (`seen`), once the countdown has run out. Returns the card, merging:
 * the result comes as `pr_upserted`. Refused at once when this window isn't connected (a queued
 * call could go out later, after the human has left the card). */
export async function mergePr(seen: Seen): Promise<Pr> {
  if (!rpc.isOpen) throw new Error("Muster's window isn't connected right now, so nothing was sent");
  const done = await rpc.call<Pr>("pr.merge", { ...seen });
  applyPr(done);
  return done;
}

export async function askChanges(p: Pr, words: string): Promise<Pr> {
  const done = await rpc.call<Pr>("pr.ask_changes", { id: p.id, post: p.post, words });
  applyPr(done);
  return done;
}

export async function setLater(p: Pr, later: boolean): Promise<Pr> {
  const done = await rpc.call<Pr>("pr.later", { id: p.id, post: p.post, later });
  applyPr(done);
  return done;
}

// ---------- pictures ----------

/** A card's pictures, read through `pr.picture` when first shown. Key `<id>:<post>:<index>`. At
 * most `PICTURES_KEPT` are kept (the oldest go first), a card's earlier posts go when a newer one
 * is shown, and a picture that failed is asked for again after a few seconds. */
export const prPictures = signal<Map<string, { src?: string; error?: string; at?: number }>>(new Map());
const PICTURES_KEPT = 24;
const RETRY_MS = 5000;

export const pictureKey = (p: Pr, index: number) => `${p.id}:${p.post}:${index}`;

export function loadPrPicture(p: Pr, index: number) {
  const key = pictureKey(p, index);
  const have = prPictures.value.get(key);
  if (have && !(have.error && Date.now() - (have.at ?? 0) > RETRY_MS)) return;
  const set = (v: { src?: string; error?: string; at?: number }) => {
    const next = new Map(prPictures.value);
    next.delete(key);
    for (const k of next.keys()) {
      const [id, post] = k.split(":").map(Number);
      if (id === p.id && post !== p.post) next.delete(k);
    }
    next.set(key, v);
    while (next.size > PICTURES_KEPT) next.delete(next.keys().next().value!);
    prPictures.value = next;
  };
  set({});
  rpc.call<{ src: string }>("pr.picture", { id: p.id, post: p.post, index })
    .then((r) => set({ src: r.src }))
    .catch((e) => set({ error: e instanceof Error ? e.message : String(e), at: Date.now() }));
}

/** A clock for the tab's "shown for a day after a merge" (minutes are enough). */
export const minuteClock = signal(Date.now());
setInterval(() => { minuteClock.value = Date.now(); }, 60_000);
