import { findMusterLinks, findTextLinks, linkAt, followLink, type LinkDeps, linkColumns, linkLabels, linkRanges, markdownLinks, type MusterLink, parseMusterLink } from "./links.ts";

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
    [{ kind: "agent", name: "idle" }, "says: idle has no tab yet: start it from its row in the sidebar."],
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

const uris = (text: string) => findTextLinks(text).map((l) => `${l.kind}:${l.uri}`);

Deno.test("findTextLinks: web and Muster addresses, with their places", () => {
  const text = "Docs: https://example.org/guide and muster://note/91.";
  assertEquals(findTextLinks(text), [
    { start: 6, end: 31, uri: "https://example.org/guide", kind: "web" },
    { start: 36, end: 52, uri: "muster://note/91", kind: "muster" },
  ]);
  assertEquals(uris("http://127.0.0.1:7433/x?a=1&b=2#top"), ["web:http://127.0.0.1:7433/x?a=1&b=2#top"]);
  assertEquals(uris("HTTPS://Example.org"), ["web:HTTPS://Example.org"]);
});

Deno.test("findTextLinks: what ends a sentence or wraps an address isn't part of it", () => {
  assertEquals(uris("See https://example.org/a."), ["web:https://example.org/a"]);
  assertEquals(uris("See https://example.org/a, then"), ["web:https://example.org/a"]);
  assertEquals(uris("Really? https://example.org/a?!"), ["web:https://example.org/a"]);
  assertEquals(uris("(https://example.org/a)"), ["web:https://example.org/a"]);
  assertEquals(uris("(see https://example.org/a)."), ["web:https://example.org/a"]);
  assertEquals(uris("[the guide](https://example.org/a)"), ["web:https://example.org/a"]);
  assertEquals(uris("<https://example.org/a>"), ["web:https://example.org/a"]);
  assertEquals(uris(`"https://example.org/a"`), ["web:https://example.org/a"]);
  assertEquals(uris("'https://example.org/a'"), ["web:https://example.org/a"]);
  assertEquals(uris("`https://example.org/a`"), ["web:https://example.org/a"]);
  assertEquals(uris("**https://example.org/a**"), ["web:https://example.org/a"]);
  // Brackets the address opened itself stay.
  assertEquals(uris("https://en.wikipedia.org/wiki/Rust_(language)."), ["web:https://en.wikipedia.org/wiki/Rust_(language)"]);
  assertEquals(uris("(https://en.wikipedia.org/wiki/Rust_(language))"), ["web:https://en.wikipedia.org/wiki/Rust_(language)"]);
  assertEquals(uris("https://example.org/a[1]"), ["web:https://example.org/a[1]"]);
});

Deno.test("findTextLinks: lists of links, one per line or several on one", () => {
  const list = "Links:\n- https://a.example/one\n- https://b.example/two\n1. muster://panel/3\n";
  assertEquals(uris(list), ["web:https://a.example/one", "web:https://b.example/two", "muster:muster://panel/3"]);
  assertEquals(uris("https://a.example https://b.example,https://c.example;http://d.example"), [
    "web:https://a.example",
    "web:https://b.example",
    "web:https://c.example",
    "web:http://d.example",
  ]);
  assertEquals(uris("https://a.example,muster://note/7;muster://agent/reviewer"), [
    "web:https://a.example",
    "muster:muster://note/7",
    "muster:muster://agent/reviewer",
  ]);
  // Markdown links side by side are two.
  assertEquals(uris("[one](https://one.example)[two](https://two.example/x)"), [
    "web:https://one.example",
    "web:https://two.example/x",
  ]);
  // An address in a query value stays part of the address it is in.
  assertEquals(uris("https://a.example/login?next=https://b.example/x,y"), ["web:https://a.example/login?next=https://b.example/x,y"]);
  const lines = findTextLinks(list);
  for (const l of lines) assertEquals(list.slice(l.start, l.end), l.uri);
});

Deno.test("findTextLinks: anything else is no link", () => {
  for (
    const bad of [
      "",
      "no links here",
      "https://",
      "https://.",
      "https:///path",
      "xhttps://example.org",
      "file:///etc/passwd",
      "ftp://example.org",
      "javascript:alert(1)",
      "mailto:someone@example.org",
      "www.example.org",
      "muster://note/0",
    ]
  ) assertEquals(uris(bad), [], bad);
});

Deno.test("findTextLinks: a Muster address inside a web address is part of it", () => {
  assertEquals(uris("https://example.org/muster://note/1"), ["web:https://example.org/muster://note/1"]);
});

Deno.test("findTextLinks: an address may end in _ * ~ unless markup wraps it", () => {
  assertEquals(uris("https://files.example/download?token=abc_"), ["web:https://files.example/download?token=abc_"]);
  assertEquals(uris("https://example.org/~user/a~"), ["web:https://example.org/~user/a~"]);
  assertEquals(uris("https://example.org/a*"), ["web:https://example.org/a*"]);
  assertEquals(uris("**https://example.org/a**."), ["web:https://example.org/a"]);
  assertEquals(uris("~~https://example.org/a~~"), ["web:https://example.org/a"]);
  assertEquals(uris("*https://example.org/a_*"), ["web:https://example.org/a_"]);
});

Deno.test("findTextLinks: a long run of brackets or links takes linear time", () => {
  const brackets = "https://example.org/" + ")".repeat(200_000) + "]".repeat(200_000);
  let t = performance.now();
  assertEquals(uris(brackets), ["web:https://example.org/"]);
  const opens = "https://example.org/" + "(".repeat(400_000);
  assertEquals(findTextLinks(opens).length, 1);
  const many = "https://a.example/ muster://note/1 ".repeat(20_000);
  assertEquals(findTextLinks(many).length, 40_000);
  t = performance.now() - t;
  if (t > 1500) throw new Error(`took ${t.toFixed(0)} ms`);
});

Deno.test("linkAt: the link the caret is in, on its own line", () => {
  const text = "one\nsee https://example.org/a. and muster://note/7\nlast";
  const web = text.indexOf("https");
  assertEquals(linkAt(text, web)?.uri, "https://example.org/a");
  assertEquals(linkAt(text, web + 5)?.uri, "https://example.org/a");
  assertEquals(linkAt(text, web + 21)?.uri, "https://example.org/a", "right after its last character");
  assertEquals(linkAt(text, web + 22), null, "after the full stop");
  assertEquals(linkAt(text, web - 1), null);
  assertEquals(linkAt(text, web + 2, web + 10)?.uri, "https://example.org/a", "a selection inside it");
  assertEquals(linkAt(text, web - 2, web + 3), null, "a selection reaching outside it");
  const m = linkAt(text, text.indexOf("muster://") + 3);
  assertEquals([m?.uri, m && text.slice(m.start, m.end)], ["muster://note/7", "muster://note/7"]);
  assertEquals(linkAt(text, 0), null);
  assertEquals(linkAt(text, text.length), null);
  assertEquals(linkAt("https://a.example", 0)?.uri, "https://a.example");
});

Deno.test("markdownLinks: Muster and web links in an agent's text become links; nothing else does", () => {
  // The shape agents write (`muster link panel 393` prints it), and the tool's `mention` escapes.
  assertEquals(markdownLinks("Read it first: [Read the words](muster://panel/393). Then pick."), [
    "Read it first: ",
    { label: "Read the words", url: "muster://panel/393", muster: { kind: "panel", id: 393 } },
    ". Then pick.",
  ]);
  assertEquals(markdownLinks(String.raw`[a\]b\[c \*x\* \\](muster://note/1)`), [
    { label: String.raw`a]b[c *x* \ `.trim(), url: "muster://note/1", muster: { kind: "note", id: 1 } },
  ]);
  assertEquals(markdownLinks("[PR](https://x.example/pr/1) and [Bread](https://en.wikipedia.org/wiki/Bread_(food))"), [
    { label: "PR", url: "https://x.example/pr/1", muster: null },
    " and ",
    { label: "Bread", url: "https://en.wikipedia.org/wiki/Bread_(food)", muster: null },
  ]);
  assertEquals(markdownLinks("[agent](muster://agent/writer)")[0], {
    label: "agent", url: "muster://agent/writer", muster: { kind: "agent", name: "writer" },
  });
  // Everything else stays exactly as written: other schemes, bad Muster links, empty labels,
  // a bare address, other Markdown.
  for (
    const plain of [
      "no links here",
      "[x](javascript:alert(1))",
      "[x](file:///etc/passwd)",
      "[x](/relative/path)",
      "[x](muster://panel/0)",
      "[x](muster://panel/3?a=b)",
      "[x](muster://window/1)",
      "[ ](muster://note/1)",
      "[x] (muster://note/1)",
      "[x](https://a b)",
      "muster://note/1 on its own",
      "**bold** and `code` and ![img](https://x.example/a.png",
      "[two\nlines](muster://note/1)",
      // An image, an escaped bracket, an escaped ")" that would change the address, a user name
      // before the host, no host.
      "see ![mock-up](https://x.example/card.png) here",
      String.raw`\[Draft](muster://note/91)`,
      String.raw`[Report](https://example.com/report\))`,
      "[x](https://docs.example@other.example/)",
      "[x](https://)",
    ]
  ) {
    assertEquals(markdownLinks(plain), [plain], plain);
  }
  // A link beside a refused one: only the good one is a link, the rest is the text as written.
  assertEquals(markdownLinks("[bad](javascript:x) [good](muster://note/2)"), [
    "[bad](javascript:x) ",
    { label: "good", url: "muster://note/2", muster: { kind: "note", id: 2 } },
  ]);
  assertEquals(markdownLinks(""), []);
  // An even run of backslashes escapes itself, not the bracket.
  assertEquals(markdownLinks(String.raw`\\[a](muster://note/3)`)[1], { label: "a", url: "muster://note/3", muster: { kind: "note", id: 3 } });
  // Any escaped ASCII punctuation in a label.
  assertEquals(markdownLinks(String.raw`[Shipping \& handling](muster://note/91)`)[0], {
    label: "Shipping & handling", url: "muster://note/91", muster: { kind: "note", id: 91 },
  });
});

Deno.test("markdownLinks reads what src/links.rs mention writes", () => {
  // The exact outputs src/links.rs `mentions_stay_one_markdown_link` asserts for odd titles.
  for (
    const [mention, label, url] of [
      ["[Pairs to review](muster://note/91)", "Pairs to review", "muster://note/91"],
      [String.raw`[a\]b\[c](muster://note/1)`, "a]b[c", "muster://note/1"],
      ["[\\*bold\\* \\`code\\` \\<x\\> \\\\](muster://panel/2)", "*bold* `code` <x> \\", "muster://panel/2"],
      ["[muster://tab/4](muster://tab/4)", "muster://tab/4", "muster://tab/4"],
    ]
  ) {
    const parts = markdownLinks(`Read ${mention} first.`);
    assertEquals(parts.length, 3, mention);
    const link = parts[1] as { label: string; url: string };
    assertEquals([link.label, link.url], [label, url], mention);
  }
  assertEquals(linkLabels("Keep [the old card](muster://panel/9) as it is"), "Keep the old card as it is");
});
