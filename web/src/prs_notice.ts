// A toast for a new pull request card when the PRs to merge tab isn't on screen (docs/UI.md "PRs
// to merge", "A new card"): in the background, hidden by the human, or in a group another one
// covers. "New PR to merge: <repo> #<n>" with Show, which opens the tab on that card (a hidden tab
// comes back only then). One toast per post: several new cards share it ("3 new PRs to merge: …"),
// and a refresh or a reload of the list brings none. While the window itself is out of sight the
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
const tag = (p: { id: number; post: number }) => `${p.id}:${p.post}`;
const drop = () => toasts.peek().some((t) => t.key === KEY) && dismissToastKey(KEY);

effect(() => {
  const list = arrivedPrs.value, inView = prsInView.value, visible = windowVisible.value;
  // The toasts are read and written untracked: this effect follows only the three above.
  untracked(() => {
    if (!list.length) return void drop();
    if (inView) {
      // The tab is on screen and says it itself.
      for (const p of list) told.add(tag(p));
      return void drop();
    }
    if (!visible || list.every((p) => told.has(tag(p)))) return;
    for (const p of list) told.add(tag(p));
    // A fresh toast (its own time again) naming every new card still waiting.
    drop();
    toast("info", arrivalText(list), SHOW_MS, KEY, {
      label: "Show",
      run: () => openPrsTab(arrivedPrs.peek()[0]?.id),
    });
  });
});
