import {
  changeChannelLayout,
  defaultSidebar,
  loadChannelLayout,
  reconcileChannels,
  rememberPlace,
  type TabHome,
  type WindowLayoutState,
} from "./channels.ts";
import {
  addonTab,
  allPanes,
  allSessions,
  emptyLayout,
  moveTab,
  QUESTIONS_TAB,
  sig,
  splitPane,
} from "./tree.ts";
function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) {
    throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
  }
}
const tab = addonTab("counter"), key = "addon:counter";
const tabs = (channel: number): TabHome[] => [
  { id: 1, channel: 10 },
  { id: 2, channel: 10 },
  { id: 3, channel: 20 },
  { id: 4, channel: 20 },
  { id: QUESTIONS_TAB, channel, questions: true },
  { id: tab, channel, pinned: key },
];
function fixture(): WindowLayoutState {
  return reconcileChannels(
    {
      v: 2,
      active: 10,
      sidebar: defaultSidebar(),
      layouts: {
        10: splitPane(emptyLayout([1]), "right", 2),
        20: splitPane(emptyLayout([3]), "right", 4),
      },
    },
    [10, 20],
    tabs(10),
  );
}
Deno.test("add-on keeps an independent place in each channel and through hide/show", () => {
  let s = fixture();
  const move = (group: number, side: "center" | "bottom") => {
    s = rememberPlace(
      changeChannelLayout(s, s.active!, (l) => moveTab(l, tab, allPanes(l.root)[group].id, side)),
      tab,
    );
  };
  const show = (active: number) => {
    s = reconcileChannels({ ...s, active }, [10, 20], tabs(active));
  };
  move(1, "bottom");
  const first = sig(s.layouts[10].root);
  show(20);
  move(0, "center");
  const second = sig(s.layouts[20].root);
  for (let i = 0; i < 3; i++) {
    show(10);
    equal(sig(s.layouts[10].root), first);
    equal(allSessions(s.layouts[20].root).includes(tab), false);
    show(20);
    equal(sig(s.layouts[20].root), second);
  }
  show(10);
  s = reconcileChannels({ ...s, pinned: { ...s.pinned, [key]: { ...s.pinned![key], hidden: true } } }, [
    10,
    20,
  ], tabs(10));
  equal(allSessions(s.layouts[10].root).includes(tab), false);
  s = loadChannelLayout(JSON.parse(JSON.stringify(s)), [10, 20], tabs(10));
  equal(s.pinned![key].hidden, true);
  delete s.pinned![key].hidden;
  show(10);
  equal(sig(s.layouts[10].root), first);
  show(20);
  equal(sig(s.layouts[20].root), second);
});
Deno.test("old Questions state loads unchanged, before add-ons existed", () => {
  const old: WindowLayoutState = {
    v: 2,
    active: 10,
    sidebar: defaultSidebar(),
    layouts: { 10: emptyLayout([1, QUESTIONS_TAB]) },
    questionsAt: { group: 0, index: 1 },
    questionsIn: { 10: { group: 0, index: 1 } },
  };
  const loaded = loadChannelLayout(old, [10], [{ id: 1, channel: 10 }, {
    id: QUESTIONS_TAB,
    channel: 10,
    questions: true,
  }]);
  equal(loaded.layouts, old.layouts);
  equal(loaded.questionsAt, old.questionsAt);
  equal(loaded.questionsIn, old.questionsIn);
  equal(loaded.pinned, undefined);
  const withAddon = reconcileChannels(loaded, [10], [{ id: 1, channel: 10 }, {
    id: QUESTIONS_TAB,
    channel: 10,
    questions: true,
  }, { id: tab, channel: 10, pinned: key }]);
  equal(allSessions(withAddon.layouts[10].root), [1, QUESTIONS_TAB, tab]);
  equal(withAddon.questionsIn, old.questionsIn);
  equal(withAddon.questionsAt, old.questionsAt);
  equal(allSessions(withAddon.layouts[10].root).includes(tab), true);
});
Deno.test("malformed pinned metadata cannot choose a session id", () => {
  const s = loadChannelLayout(
    {
      ...fixture(),
      pinned: { [key]: { tab: 1, at: { group: "bad" }, in: { x: {}, 10: { group: 0, index: 0 } } } },
    },
    [10, 20],
    tabs(10),
  );
  equal(s.pinned![key].tab, tab);
  equal(s.pinned![key].at, undefined);
  equal(s.pinned![key].in, { 10: { group: 0, index: 0 } });
});
