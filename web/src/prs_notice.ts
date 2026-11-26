// A toast for a new pull request card when the PRs to merge tab isn't on screen (docs/UI.md "PRs
// to merge", "A new card"): in the background, hidden by the human, or in a group another one
// covers. "New PR to merge: <repo> #<n>" with Show, which opens the tab on that card (a hidden tab
// comes back only then). One toast per post: several new cards share it ("3 new PRs to merge: …"),
// a card that stops waiting leaves it, a toast that has gone isn't named again, and a refresh or a
// reload of the list brings none. While the window itself is out of sight the
// toast waits until it is back. On screen, the tab says it itself (PrsView.tsx).
import { effect, signal, untracked } from "@preact/signals";
import { arrivalText, arrivedPrs, prsInView } from "./prs.ts";
import { dismissToastKey, toast, toasts } from "./state.ts";
import { openPrsTab } from "./actions.ts";

const KEY = "prs-arrived";
/** How long the toast stays. */
const SHOW_MS = 15_000;

const windowVisible = signal(document.visibilityState !== "hidden");
document.addEventListener("visibilitychange", () => {
  windowVisible.value = document.visibilityState !== "hidden";
});

/** Posts a toast (or the tab) has told about: `<id>:<post>`. */
const told = new Set<string>();
/** The posts the toast on screen names, oldest first: a toast that has gone (timed out, or
 * dismissed) is not named again; the next one names only what came after it. */
let batch: string[] = [];
const tag = (p: { id: number; post: number }) => `${p.id}:${p.post}`;
const showing = () => toasts.peek().some((t) => t.key === KEY);
const drop = () => {
  batch = [];
  if (showing()) dismissToastKey(KEY);
};

effect(() => {
  const list = arrivedPrs.value, inView = prsInView.value, visible = windowVisible.value;
  // The toasts are read and written untracked: this effect follows only the three above.
  untracked(() => {
    if (inView) {
      // The tab is on screen and says it itself.
      for (const p of list) told.add(tag(p));
      return drop();
    }
    const live = new Map(list.map((p) => [tag(p), p]));
    const was = showing() ? batch.filter((t) => live.has(t)) : [];
    // While the window is out of sight, new posts wait to be told.
    const fresh = visible ? list.filter((p) => !told.has(tag(p))).map(tag) : [];
    for (const t of fresh) told.add(t);
    const next = [...was, ...fresh];
    if (!next.length) return drop();
    if (next.join() === batch.join() && showing()) return;
    // A fresh toast (its own time again): its words and its Show name the same cards.
    const ps = next.map((t) => live.get(t)!);
    drop();
    batch = next;
    toast("info", arrivalText(ps), SHOW_MS, KEY, { label: "Show", run: () => openPrsTab(ps[0].id) });
  });
});
