import { CONFLICT_END, CONFLICT_START, mapOffset, merge3 } from "./note_merge.ts";

function eq<T>(actual: T, expected: T, msg = "") {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}\n  actual:   ${a}\n  expected: ${e}`);
}

Deno.test("an agent's append and the human's typing above it both stay", () => {
  const base = "# Plan\n- one\n";
  const mine = "# Plan\n- one, done\n";
  const theirs = "# Plan\n- one\n- two (agent)\n";
  eq(merge3(base, mine, theirs), { text: "# Plan\n- one, done\n- two (agent)\n", conflict: false });
});

Deno.test("both typing at the very end keeps both, the human's first", () => {
  eq(merge3("a\n", "a\nmine", "a\ntheirs\n"), { text: "a\nminetheirs\n", conflict: false });
});

Deno.test("one side unchanged, or both the same, is no merge at all", () => {
  eq(merge3("x", "x", "y"), { text: "y", conflict: false });
  eq(merge3("x", "y", "x"), { text: "y", conflict: false });
  eq(merge3("x", "y", "y"), { text: "y", conflict: false });
});

Deno.test("an edit below and an edit above both apply", () => {
  const base = "top\nmiddle\nbottom\n";
  eq(merge3(base, "TOP\nmiddle\nbottom\n", "top\nmiddle\nBOTTOM\n"), { text: "TOP\nmiddle\nBOTTOM\n", conflict: false });
  eq(merge3(base, "top\nmiddle\nBOTTOM\n", "TOP\nmiddle\nbottom\n"), { text: "TOP\nmiddle\nBOTTOM\n", conflict: false });
});

Deno.test("insertions at the same point both stay, the human's first", () => {
  eq(merge3("a\nline\nz\n", "a\nline by human\nz\n", "a\nline by agent\nz\n"),
    { text: "a\nline by human by agent\nz\n", conflict: false });
});

Deno.test("overlapping changes keep both versions, marked", () => {
  const r = merge3("a\nline\nz\n", "a\nLINE\nz\n", "a\nlane\nz\n", "Claude");
  eq(r.conflict, true);
  eq(r.text, `a\nLINE\n${CONFLICT_START("Claude")}\nlane\n${CONFLICT_END}\nz\n`);
});

Deno.test("a rewrite elsewhere and typing at the end both apply", () => {
  eq(merge3("old text", "old text, edited", "brand new text"), { text: "brand new text, edited", conflict: false });
  const r = merge3("old text", "old stuff", "new things", "bot");
  eq(r.conflict, true);
  for (const s of ["old stuff", "new things"]) if (!r.text.includes(s)) throw new Error(r.text);
});

Deno.test("the caret keeps its place across someone else's edit", () => {
  eq(mapOffset("hello world", "hello brave world", 0), 0);
  eq(mapOffset("hello world", "hello brave world", 11), 17);
  eq(mapOffset("abc", "abcdef", 3), 3, "an append at the caret leaves the caret before it");
  eq(mapOffset("abcdef", "abXf", 4), 3, "inside a replaced part: its end");
});
