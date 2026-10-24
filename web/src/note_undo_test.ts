import { applyChange, editOf, type EditKind, GROUP_MS, kindOf, newHistory, rebaseHistory, record, redo, undo } from "./note_undo.ts";

function eq<T>(actual: T, expected: T, msg = "") {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}\n  actual:   ${a}\n  expected: ${e}`);
}

/** A note's editor without the DOM: the text, the selection and the history. */
function editor(text = "") {
  const h = newHistory();
  let now = 1000;
  let sel: [number, number] = [text.length, text.length];
  const edit = (next: string, caret: number, kind: EditKind) => {
    record(h, text, next, { from: sel[0], caret, kind }, now);
    text = next;
    sel = [caret, caret];
  };
  return {
    h,
    get text() {
      return text;
    },
    get sel() {
      return sel;
    },
    wait(ms: number) {
      now += ms;
    },
    select(at: number, to = at) {
      sel = [at, to];
    },
    /** Types `s` one key at a time (the first over the selection), 50 ms apart. */
    type(s: string) {
      for (const ch of s) {
        edit(text.slice(0, sel[0]) + ch + text.slice(sel[1]), sel[0] + ch.length, "type");
        now += 50;
      }
    },
    backspace(n = 1) {
      for (let i = 0; i < n; i++) {
        const at = sel[0] === sel[1] ? sel[0] - 1 : sel[0];
        edit(text.slice(0, at) + text.slice(sel[1]), at, "delete");
        now += 50;
      }
    },
    paste(s: string) {
      edit(text.slice(0, sel[0]) + s + text.slice(sel[1]), sel[0] + s.length, "other");
    },
    cut() {
      edit(text.slice(0, sel[0]) + text.slice(sel[1]), sel[0], "other");
    },
    /** Someone else's change: the editor shows `next`. */
    remote(next: string) {
      rebaseHistory(h, text, next);
      text = next;
    },
    undo() {
      const c = undo(h, text);
      if (c) [text, sel] = [applyChange(text, c), c.sel];
      return !!c;
    },
    redo() {
      const c = redo(h, text);
      if (c) [text, sel] = [applyChange(text, c), c.sel];
      return !!c;
    },
  };
}

Deno.test("typing undoes a word at a time, and redo brings it back", () => {
  const e = editor();
  e.type("hello brave world");
  eq(e.h.undo.length, 3);
  e.undo();
  eq(e.text, "hello brave ");
  e.undo();
  eq(e.text, "hello ");
  eq(e.sel, [6, 6], "the caret goes where the word began");
  e.undo();
  eq(e.text, "");
  eq(e.undo(), false, "nothing left to undo");
  e.redo();
  e.redo();
  eq(e.text, "hello brave ");
  eq(e.sel, [12, 12]);
  e.redo();
  eq(e.text, "hello brave world");
  eq(e.redo(), false);
});

Deno.test("a pause starts a new step; a new line counts as a space", () => {
  const e = editor();
  e.type("abc");
  e.wait(GROUP_MS + 1);
  e.type("def\nghi");
  e.undo();
  eq(e.text, "abcdef\n");
  e.undo();
  eq(e.text, "abc");
});

Deno.test("a paste and a cut are steps of their own", () => {
  const e = editor();
  e.type("one ");
  e.paste("PASTED");
  e.type("two");
  e.select(0, 4);
  e.cut();
  eq(e.text, "PASTEDtwo");
  e.undo();
  eq(e.text, "one PASTEDtwo");
  eq(e.sel, [0, 4], "what came back is selected");
  e.undo();
  eq(e.text, "one PASTED");
  e.undo();
  eq(e.text, "one ");
});

Deno.test("Backspace runs group, and typing over a selection is one step", () => {
  const e = editor("keep this word");
  e.backspace(4);
  eq(e.text, "keep this ");
  e.undo();
  eq(e.text, "keep this word");
  eq(e.sel, [10, 14]);
  e.select(5, 9);
  e.type("that");
  eq(e.text, "keep that word");
  eq(e.h.undo.length, 1, "the undone Backspace went to redo, and typing dropped it");
  e.undo();
  eq(e.text, "keep this word");
  eq(e.sel, [5, 9], "the replaced text is selected again");
});

Deno.test("typing again after an undo drops the redo", () => {
  const e = editor();
  e.type("abc ");
  e.wait(GROUP_MS + 1);
  e.type("def");
  e.undo();
  e.type("x");
  eq(e.redo(), false);
  eq(e.text, "abc x");
});

Deno.test("an agent's change elsewhere stays when the human's typing is undone", () => {
  const e = editor("# Plan\n");
  e.type("- mine");
  // The agent appends below while the human types on the first line.
  e.remote("# Plan\n- mine\n- agent's line\n");
  e.select(0);
  e.type("Today: ");
  e.remote("Intro by agent\nToday: # Plan\n- mine\n- agent's line\n");
  e.undo();
  eq(e.text, "Intro by agent\n# Plan\n- mine\n- agent's line\n");
  e.undo();
  eq(e.text, "Intro by agent\n# Plan\n- \n- agent's line\n");
  e.redo();
  e.redo();
  eq(e.text, "Intro by agent\nToday: # Plan\n- mine\n- agent's line\n");
});

Deno.test("an agent appending right after the typing keeps its text", () => {
  const e = editor("a\n");
  e.type("mine");
  e.remote("a\nmine and theirs");
  e.undo();
  eq(e.text, "a\n and theirs");
});

Deno.test("a step the agent's change touched is not undone, nor anything older", () => {
  const e = editor();
  e.type("first ");
  e.wait(GROUP_MS + 1);
  e.type("second ");
  e.wait(GROUP_MS + 1);
  e.type("third");
  // The agent rewrites "second" (inside the human's second step).
  e.remote("first SECOND! third");
  e.undo();
  eq(e.text, "first SECOND! ", "the step after it still undoes");
  eq(e.undo(), false, "the touched step and the older ones are gone");
  eq(e.text, "first SECOND! ");
});

Deno.test("typing after someone else's change starts a new step", () => {
  const e = editor();
  e.type("ab");
  e.remote("ab\nagent");
  e.select(2);
  e.type("cd");
  e.undo();
  eq(e.text, "ab\nagent");
});

Deno.test("redo steps move with someone else's change too", () => {
  const e = editor("x");
  e.type("one");
  e.undo();
  e.remote("agent\nx");
  e.redo();
  eq(e.text, "agent\nxone");
});

Deno.test("the edit is placed where it was typed when the text repeats", () => {
  eq(editOf("aaa", "aaaa", 1, 2), { at: 1, removed: "", inserted: "a" });
  eq(editOf("aaa", "aa", 2, 1), { at: 1, removed: "a", inserted: "" }, "Backspace");
  eq(editOf("aaa", "aa", 1, 1), { at: 1, removed: "a", inserted: "" }, "Delete");
  eq(editOf("keep this", "keep t", 5, 6), { at: 5, removed: "this", inserted: "t" }, "typed over a selection");
  eq(editOf("abc", "aXc", Infinity, 2), { at: 1, removed: "b", inserted: "X" }, "no selection known");
  // Never between an emoji's two halves.
  eq(editOf("😀", "😁", 0, 2), { at: 0, removed: "😀", inserted: "😁" });
  eq(editOf("x😀", "x😁", 3, 3), { at: 1, removed: "😀", inserted: "😁" });
  const e = editor("aa");
  e.select(1);
  e.type("aa");
  eq(e.h.undo.length, 1, "typing on in repeated text is one step");
  e.undo();
  eq(e.text, "aa");
  eq(e.sel, [1, 1]);
});

Deno.test("an input method's text is one step; the input type decides the kind", () => {
  eq(kindOf("insertText"), "type");
  eq(kindOf("insertLineBreak"), "type");
  eq(kindOf("deleteContentBackward"), "delete");
  eq(kindOf("deleteWordBackward"), "delete");
  eq(kindOf("deleteByCut"), "other");
  eq(kindOf("insertFromPaste"), "other");
  eq(kindOf("insertText", true), "compose");
  eq(kindOf(undefined), "other");
  const h = newHistory();
  record(h, "", "k", { from: 0, caret: 1, kind: "compose" }, 0);
  record(h, "k", "か", { from: 0, caret: 1, kind: "compose" }, 5000);
  record(h, "か", "かn", { from: 1, caret: 2, kind: "compose" }, 5100);
  record(h, "かn", "かな", { from: 1, caret: 2, kind: "compose" }, 5200);
  eq(h.undo.length, 1);
  eq(h.undo[0].inserted, "かな");
});

Deno.test("a history out of step with the text is dropped, not applied blind", () => {
  const h = newHistory();
  record(h, "", "abc", { from: 0, caret: 3, kind: "other" }, 0);
  eq(undo(h, "something else"), null);
  eq(h.undo.length + h.redo.length, 0);
});
