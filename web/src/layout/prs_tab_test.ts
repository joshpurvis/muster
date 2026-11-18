// The PRs to merge tab's place in a window (docs/UI.md "PRs to merge"): pinned key "prs", one id
// below every add-on's, placed and remembered like an add-on tab, hidden and shown again.
import {
  changeChannelLayout, defaultSidebar, loadChannelLayout, reconcileChannels, rememberPlace, type TabHome,
  type WindowLayoutState,
} from "./channels.ts";
import {
  addonTab, allPanes, allSessions, emptyLayout, isAddonTab, isPinnedTab, moveTab, PRS_TAB, QUESTIONS_TAB, sig, splitPane,
} from "./tree.ts";

function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}

const tabs = (channel: number, prs = true): TabHome[] => [
  { id: 1, channel: 10 }, { id: 2, channel: 10 }, { id: 3, channel: 20 }, { id: 4, channel: 20 },
  { id: QUESTIONS_TAB, channel, questions: true },
  ...(prs ? [{ id: PRS_TAB, channel, pinned: "prs" }] : []),
];

function fixture(): WindowLayoutState {
  return reconcileChannels({
    v: 2, active: 10, sidebar: defaultSidebar(),
    layouts: { 10: splitPane(emptyLayout([1]), "right", 2), 20: splitPane(emptyLayout([3]), "right", 4) },
  }, [10, 20], tabs(10));
}

Deno.test("the PRs tab's id is pinned, below every add-on's, and not an add-on", () => {
  equal(isPinnedTab(PRS_TAB), true);
  equal(isAddonTab(PRS_TAB), false);
  equal(isAddonTab(QUESTIONS_TAB), false);
  for (const id of ["counter", "a", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"]) {
    equal(isAddonTab(addonTab(id)), true);
    equal(addonTab(id) > PRS_TAB, true);
  }
  equal(Number.isSafeInteger(PRS_TAB), true);
});

Deno.test("the PRs tab arrives in the shown channel, keeps a place per channel, and hides", () => {
  let s = fixture();
  equal(allSessions(s.layouts[10].root).includes(PRS_TAB), true);
  // Moved into the second group of channel 10, and remembered there.
  s = rememberPlace(changeChannelLayout(s, 10, (l) => moveTab(l, PRS_TAB, allPanes(l.root)[1].id, "center", 0)), PRS_TAB);
  const there = sig(s.layouts[10].root);
  equal(there, `row([${QUESTIONS_TAB},1] [${PRS_TAB},2])`);
  s = reconcileChannels({ ...s, active: 20 }, [10, 20], tabs(20));
  equal(allSessions(s.layouts[10].root).includes(PRS_TAB), false);
  equal(allSessions(s.layouts[20].root).includes(PRS_TAB), true);
  s = reconcileChannels({ ...s, active: 10 }, [10, 20], tabs(10));
  equal(sig(s.layouts[10].root), there);
  // Saved and loaded: the place and the "prs" key survive; a hidden tab stays hidden.
  s = reconcileChannels({ ...s, pinned: { ...s.pinned, prs: { ...s.pinned!.prs, hidden: true } } }, [10, 20], tabs(10));
  equal(allSessions(s.layouts[10].root).includes(PRS_TAB), false);
  const back = loadChannelLayout(JSON.parse(JSON.stringify(s)), [10, 20], tabs(10));
  equal(back.pinned!.prs.hidden, true);
  equal(back.pinned!.prs.tab, PRS_TAB);
  delete back.pinned!.prs.hidden;
  const shown = reconcileChannels(back, [10, 20], tabs(10));
  equal(sig(shown.layouts[10].root), there);
  // Nothing waits (the tab isn't among the window's tabs): it leaves, and comes back at its place.
  const gone = reconcileChannels(shown, [10, 20], tabs(10, false));
  equal(allSessions(gone.layouts[10].root).includes(PRS_TAB), false);
  equal(sig(reconcileChannels(gone, [10, 20], tabs(10)).layouts[10].root), there);
});

Deno.test("only known pinned keys load", () => {
  const s = fixture();
  const saved = JSON.parse(JSON.stringify(s));
  saved.pinned = { ...saved.pinned, "prs-2": { tab: 5 }, "PRS": { tab: 6 }, "addon:Bad Key": { tab: 7 } };
  const back = loadChannelLayout(saved, [10, 20], tabs(10));
  equal(Object.keys(back.pinned ?? {}).sort(), ["prs"]);
});
