// Pull requests agents put up for the human to merge (docs/UI.md "PRs to merge", docs/PROTOCOL.md
// "Pull requests to merge"). `pr.list` after each hello, then `pr_upserted` events; `pr.refresh`
// reads GitHub again (when the tab is shown, every 2 minutes while it is, and with Check again).
// Merging, asking for changes and Not now go through `pr.merge`, `pr.ask_changes` and `pr.later`,
// which the daemon accepts only from a click in this window; `pr.merge` is sent only once that
// card's 5 second countdown has run out (prs_merges.ts: each card's own, in the background).
//
// A card posted (or posted again) after this window's first list is new until the tab shows it:
// `arrivedPrs`. The tab moves to it or says so in its bar (PrsView.tsx); elsewhere a toast says so
// (prs_notice.ts).
//
// The pure part (states, order, the rules and the words) is prs_model.ts, re-exported here.

import { computed, signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import type { Pr, PrSize } from "./types.ts";
import { type LeftOut, leftOutOf, PostBook, type PrPost, sortMerged, sortWaiting, stillNew } from "./prs_model.ts";
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

/** Posts met after the first list, not shown in the tab yet (oldest first). */
const newPosts = signal<PrPost[]>([]);
/** Cards posted, or posted again, while this window was open that the human hasn't looked at in
 * the tab yet, oldest first (one merged, closed or withdrawn meanwhile drops out). */
export const arrivedPrs = computed(() => stillNew(newPosts.value, prs.value));
/** The PRs to merge tab is on screen in this window (PrsView.tsx keeps it). */
export const prsInView = signal(false);
const book = new PostBook();
/** The first list arrived: cards met before it were there when the window opened. */
let listed = false;

function noteArrival(p: Pr) {
  if (!book.note(p) || !listed || p.status !== "waiting") return;
  const keep = new Set(stillNew(newPosts.peek(), prs.peek()).map((x) => x.id));
  newPosts.value = [...newPosts.peek().filter((a) => a.id !== p.id && keep.has(a.id)), { id: p.id, post: p.post }];
  // Muster reads a card from GitHub only while the tab is on screen (its title, the rules,
  // Merge): one arriving then is read now, not at the next 2 minute read. Off screen, the tab
  // reads every card when it is shown.
  if (prsInView.peek() && !p.github) refreshPrs(p.id, false).catch(() => {});
}

/** The tab showed card `id`: it is new no longer. */
export function seenPr(id: number) {
  if (newPosts.peek().some((a) => a.id === id)) newPosts.value = newPosts.peek().filter((a) => a.id !== id);
}

/** Record card `p` unless this window has a newer copy (every change raises `updated_at`). */
export function applyPr(p: Pr) {
  const have = prs.value.get(p.id);
  if (have && have.updated_at > p.updated_at) return;
  changedDuringLoad?.set(p.id, p);
  // A withdrawn card stays (hidden by every list) so an older copy arriving late can't bring it back.
  const next = new Map(prs.value);
  next.set(p.id, p);
  prs.value = next;
  noteArrival(p);
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
    // Cards posted while the window was away (a reconnect) are new; on the first list none are.
    for (const p of [...loaded.values()].sort((a, b) => a.id - b.id)) noteArrival(p);
    listed = true;
  } catch (e) {
    if (/unknown method/i.test(e instanceof Error ? e.message : String(e))) prsSupported.value = false;
  } finally {
    if (changedDuringLoad === changed) changedDuringLoad = null;
  }
}

/** Read GitHub again: every waiting card (at most once a minute each), or card `id` now (Check
 * again: at most every 3 seconds; `force` false: at most once a minute, as for every card). */
export async function refreshPrs(id?: number, force = true): Promise<void> {
  if (!prsSupported.peek()) return;
  if (id === undefined) {
    const r = await rpc.call<{ prs: Pr[] }>("pr.refresh", {});
    applyList(r.prs);
  } else {
    applyPr(await rpc.call<Pr>("pr.refresh", { id, force }));
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
export const prPictures = signal<Map<string, Loaded>>(new Map());
/** `tooTall`: Muster sent no picture, only its size: too tall to read (docs/UI.md "PRs to merge"). */
type Loaded = { src?: string; tooTall?: PrSize; error?: string; at?: number };
const PICTURES_KEPT = 24;
const RETRY_MS = 5000;

export const pictureKey = (p: Pr, index: number) => `${p.id}:${p.post}:${index}`;

export function loadPrPicture(p: Pr, index: number) {
  const key = pictureKey(p, index);
  const have = prPictures.value.get(key);
  if (have && !(have.error && Date.now() - (have.at ?? 0) > RETRY_MS)) return;
  const set = (v: Loaded) => {
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
  rpc.call<{ src?: string; too_tall?: PrSize }>("pr.picture", { id: p.id, post: p.post, index })
    .then((r) => set(r.too_tall ? { tooTall: r.too_tall } : r.src ? { src: r.src } : { error: "Muster sent no picture", at: Date.now() }))
    .catch((e) => set({ error: e instanceof Error ? e.message : String(e), at: Date.now() }));
}

/** The pictures of `p` left out for being too tall: marked on the card, or so sent by `pr.picture`. */
export const leftOut = (p: Pr): LeftOut => leftOutOf(p, (i) => !!prPictures.value.get(pictureKey(p, i))?.tooTall);

/** A clock for the tab's "shown for a day after a merge" (minutes are enough). */
export const minuteClock = signal(Date.now());
setInterval(() => { minuteClock.value = Date.now(); }, 60_000);
