import { findMusterLinks, followLink, type LinkDeps, linkColumns, linkRanges, type MusterLink, parseMusterLink } from "./links.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

Deno.test("parseMusterLink: each kind, exactly", () => {
  assertEquals(parseMusterLink("muster://note/91"), { kind: "note", id: 91 });
  assertEquals(parseMusterLink("muster://panel/3"), { kind: "panel", id: 3 });
  assertEquals(parseMusterLink("muster://tab/12"), { kind: "tab", id: 12 });
  assertEquals(parseMusterLink("muster://agent/reviewer"), { kind: "agent", name: "reviewer" });
  assertEquals(parseMusterLink("muster://agent/PR_reviewer-2"), { kind: "agent", name: "PR_reviewer-2" });
});

Deno.test("parseMusterLink: anything else is no link", () => {
  for (
    const bad of [
      "",
      "muster://note/",
      "muster://note/0",
      "muster://note/007",
      "muster://note/-1",
      "muster://note/1.5",
      "muster://note/1/",
      "muster://note/1?token=abc",
      "muster://note/1#x",
      "muster://note/1 ",
      " muster://note/1",
      "muster://notes/1",
      "muster://session/1",
      "muster://window/1",
      "muster://note/1234567890123456",
      "muster://agent/",
      "muster://agent/-x",
      "muster://agent/a b",
      "muster://agent/a/b",
      `muster://agent/${"a".repeat(41)}`,
      "MUSTER://note/1",
      "muster:/note/1",
      "https://example.com/muster://note/1",
      "javascript:alert(1)//muster://note/1",
      "muster://note/1\nmuster://note/2",
    ]
  ) assertEquals(parseMusterLink(bad), null, JSON.stringify(bad));
});

Deno.test("findMusterLinks: in Claude's and Codex's plain output", () => {
  const t = "See Smoke note (muster://note/7) and the panel muster://panel/3.";
  assertEquals(findMusterLinks(t), [
    { start: 16, end: 31, uri: "muster://note/7" },
    { start: 47, end: 63, uri: "muster://panel/3" },
  ]);
  assertEquals(findMusterLinks("[Plan](muster://tab/12)").map((l) => l.uri), ["muster://tab/12"]);
  assertEquals(findMusterLinks("ask muster://agent/reviewer, then").map((l) => l.uri), ["muster://agent/reviewer"]);
  assertEquals(findMusterLinks("no link here"), []);
  // Glued to other text, or not one of the shapes: none.
  assertEquals(findMusterLinks("xmuster://note/1 muster://note/1x muster://note/1/2 muster://note/0"), []);
  assertEquals(findMusterLinks("muster://note/1?token=abc").length, 0);
});

Deno.test("linkColumns: wide characters before a link shift its columns", () => {
  const cells = (s: string) => [...s].map((chars) => ({ chars, width: 1 }));
  assertEquals(linkColumns(cells("a muster://note/5")), [{ start: 3, end: 17, uri: "muster://note/5" }]);
  // "界" is wide: one character, two cells (the second has width 0 and no text).
  const wide = [{ chars: "界", width: 2 }, { chars: "", width: 0 }, ...cells(" muster://note/5")];
  assertEquals(linkColumns(wide), [{ start: 4, end: 18, uri: "muster://note/5" }]);
  // An emoji is two UTF-16 units in one wide cell.
  const emoji = [{ chars: "🙂", width: 2 }, { chars: "", width: 0 }, ...cells(" muster://tab/2")];
  assertEquals(linkColumns(emoji), [{ start: 4, end: 17, uri: "muster://tab/2" }]);
  // Empty cells read as spaces.
  assertEquals(linkColumns([{ chars: "", width: 1 }, ...cells("muster://panel/1")]).map((l) => l.start), [2]);
});

Deno.test("linkRanges: a link split by a wrap is one range over both rows, never half of it", () => {
  const cells = (s: string) => [...s].map((chars) => ({ chars, width: 1 }));
  // 10 columns, buffer rows 6 and 7 (0-based; xterm's ranges are 1-based): "see muster" + "://note/91".
  assertEquals(linkRanges(cells("see muster").concat(cells("://note/91")), 10, 6), [
    { start: { x: 5, y: 7 }, end: { x: 10, y: 8 }, uri: "muster://note/91" },
  ]);
  // Split inside the id: note 91, not note 9.
  assertEquals(linkRanges(cells("ab muster:").concat(cells("//note/9")).concat(cells("1 ")), 10, 0).map((l) => l.uri), [
    "muster://note/91",
  ]);
});

Deno.test("linkColumns: a link the next row would turn into another is none, never the wrong id", () => {
  const cells = (s: string) => [...s].map((chars) => ({ chars, width: 1 }));
  // A replay drew "muster://note/91" as two rows cut after the 9: the 9 alone is no link.
  assertEquals(linkColumns(cells("muster://note/9"), { next: "1)" }), []);
  assertEquals(linkColumns(cells("see muster://agent/rev"), { next: "iewer" }), []);
  // What follows can't continue it: the link stands.
  for (const next of [" ", ")", "muster-links $", "", "- item"]) {
    assertEquals(linkColumns(cells("muster://note/9"), { next }).map((l) => l.uri), ["muster://note/9"], next);
  }
  // Glued to the row before.
  assertEquals(linkColumns(cells("muster://note/9 "), { before: "x" }), []);
  assertEquals(linkColumns(cells("muster://note/9 "), { before: "(" }).map((l) => l.start), [1]);
});

/** A fake world that records every call: following a link may only look things up, show one of
 * them, or say it is gone. */
function world(): { deps: LinkDeps; calls: string[] } {
  const calls: string[] = [];
  const notes = new Set([91]);
  const panels = new Set([3]);
  const sessions = new Set([12, 40]);
  const agents = new Map([["reviewer", { name: "Reviewer", session: 40 }], ["idle", { name: "idle", session: null }]]);
  const deps: LinkDeps = {
    hasNote: (id) => notes.has(id),
    hasPanel: (id) => panels.has(id),
    hasSession: (id) => sessions.has(id),
    agent: (name) => agents.get(name.toLowerCase()) ?? null,
    gotoNote: (id) => calls.push(`note ${id}`),
    gotoPanel: (id) => calls.push(`panel ${id}`),
    gotoSession: (id) => calls.push(`tab ${id}`),
    notify: (text) => calls.push(`says: ${text}`),
  };
  return { deps, calls };
}

Deno.test("followLink: shows each kind", () => {
  const cases: [MusterLink, string][] = [
    [{ kind: "note", id: 91 }, "note 91"],
    [{ kind: "panel", id: 3 }, "panel 3"],
    [{ kind: "tab", id: 12 }, "tab 12"],
    [{ kind: "agent", name: "REVIEWER" }, "tab 40"],
  ];
  for (const [link, want] of cases) {
    const { deps, calls } = world();
    assertEquals(followLink(link, deps), true, want);
    assertEquals(calls, [want]);
  }
});

Deno.test("followLink: a link to something gone says so in a short message", () => {
  const cases: [MusterLink, string][] = [
    [{ kind: "note", id: 5 }, "says: Note 5 no longer exists."],
    [{ kind: "panel", id: 9 }, "says: Panel 9 is closed."],
    [{ kind: "tab", id: 77 }, "says: Tab 77 no longer exists."],
    [{ kind: "agent", name: "nobody" }, "says: There is no agent named nobody."],
    [{ kind: "agent", name: "idle" }, "says: idle has no tab yet: start it from the Agents panel."],
  ];
  for (const [link, want] of cases) {
    const { deps, calls } = world();
    assertEquals(followLink(link, deps), false, want);
    assertEquals(calls, [want]);
  }
});

Deno.test("followLink: a click changes nothing", () => {
  // Every dependency that could change something is absent: a click can only reach the lookups,
  // the three show actions and the message. Any other access throws.
  const { deps, calls } = world();
  const guarded = new Proxy(deps, {
    get(target, key) {
      if (!(key in target)) throw new Error(`a link touched ${String(key)}`);
      return target[key as keyof LinkDeps];
    },
  });
  for (
    const uri of [
      "muster://note/91",
      "muster://panel/3",
      "muster://tab/12",
      "muster://agent/reviewer",
      "muster://note/5",
      "muster://agent/nobody",
    ]
  ) {
    const link = parseMusterLink(uri);
    if (!link) throw new Error(uri);
    followLink(link, guarded);
  }
  assertEquals(calls.length, 6);
  assertEquals(calls.every((c) => /^(note|panel|tab) \d+$|^says: /.test(c)), true, calls.join(" | "));
});
