import {
  bufferText,
  normalizeCode,
  phoneGrid,
  programScrolls,
  relativeTime,
  scrollPosition,
  sessionOrder,
  splitPan,
  wheelReport,
  wheelSteps,
} from "./logic.ts";
import type { SessionInfo } from "../types.ts";

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

Deno.test("pairing codes accept mixed case, spaces and dashes without changing QR tokens", () => {
  equal(normalizeCode(" abc-def ghJ "), "ABCDEFGHJ");
  equal(normalizeCode("AB".repeat(32)), "ab".repeat(32));
});

Deno.test("phone grid reserves the collapsed composer and rounds down to whole cells", () => {
  equal(phoneGrid(396, 855, { width: 6.625, height: 15 }), { cols: 59, rows: 52 });
  equal(phoneGrid(1, 1, { width: 7, height: 14 }), { cols: 2, rows: 1 });
});

Deno.test("small touch movements accumulate even when the grid has no vertical overflow", () => {
  let position = 140;
  for (let i = 0; i < 7; i++) {
    const next = scrollPosition(position + 2, 100, 14, 0);
    position = next.row * 14 + next.pan + next.remainder;
  }
  equal(position, 154);
  equal(scrollPosition(position, 100, 14, 0).row, 11);
  equal(scrollPosition(Infinity, 100, 14, 45), { row: 100, pan: 45, remainder: 0 });
  equal(scrollPosition(-2, 100, 14, 45), { row: 0, pan: 0, remainder: 0 });
});

Deno.test("selection joins soft wraps, retains boundary spaces and maps physical rows", () => {
  const lines = [
    { text: "Hello ", isWrapped: false },
    { text: "world  ", isWrapped: true },
    { text: "next   ", isWrapped: false },
    { text: "       ", isWrapped: false },
  ];
  const buffer = {
    length: lines.length,
    getLine(i: number) {
      const line = lines[i];
      return line && { ...line, translateToString: (trim = false) => trim ? line.text.trimEnd() : line.text };
    },
  };
  equal(bufferText(buffer), { text: "Hello world\nnext\n", offsets: [0, 6, 12, 17] });
  equal(bufferText(buffer, 1, 3).text, "world\nnext");
  equal(bufferText({ length: 0, getLine: () => undefined }), { text: "", offsets: [] });
});

Deno.test("session list prioritizes human attention, then most recent state change", () => {
  const list = [
    { id: 1, state: "working", state_since: 500 },
    { id: 2, state: "needs_approval", state_since: 100 },
    { id: 3, state: "asking_question", state_since: 200 },
    { id: 4, state: "idle", state_since: 700 },
  ] as SessionInfo[];
  equal(list.sort(sessionOrder).map((s) => s.id), [3, 2, 4, 1]);
});

Deno.test("relative times tolerate future timestamps and use daemon milliseconds", () => {
  equal(relativeTime(2000, 1000), "just now");
  equal(relativeTime(0, 120000), "2m");
  equal(relativeTime(0, 7200000), "2h");
});

// Claude Code's fullscreen view draws on the alternate screen, which has no scrollback: the phone
// could only ever show one screen of it. It scrolls by asking the program, as a mouse wheel does.
Deno.test("only a full-screen program that tracks the mouse scrolls its own history", () => {
  equal(programScrolls("alternate", "any"), true);
  equal(programScrolls("alternate", "vt200"), true);
  equal(programScrolls("alternate", "drag"), true);
  equal(programScrolls("alternate", "none"), false);
  equal(programScrolls("alternate", "x10"), false);
  equal(programScrolls("normal", "any"), false);
});

Deno.test("a drag pans the phone's view of the screen first, the rest scrolls the program", () => {
  equal(splitPan(-50, 30, 100), { pan: 0, rest: -20 });
  equal(splitPan(-50, 0, 0), { pan: 0, rest: -50 });
  equal(splitPan(40, 80, 100), { pan: 100, rest: 20 });
  equal(splitPan(10, 20, 100), { pan: 30, rest: 0 });
});

Deno.test("drag pixels become one wheel notch per row, carrying the rest either way", () => {
  equal(wheelSteps(-35, 0, 16), { count: -2, remainder: -3 });
  equal(wheelSteps(-13, -3, 16), { count: -1, remainder: 0 });
  equal(wheelSteps(10, 0, 16), { count: 0, remainder: 10 });
  equal(wheelSteps(7, 10, 16), { count: 1, remainder: 1 });
  // Many small moves (a slow drag) add up to the same notches as one long one.
  let count = 0, remainder = 0;
  for (let i = 0; i < 100; i++) {
    const step = wheelSteps(-3.2, remainder, 16);
    count += step.count;
    remainder = step.remainder;
  }
  equal(count, -20);
});

Deno.test("wheel reports match what xterm.js sends in each mouse encoding", () => {
  const cell = { width: 8, height: 16 };
  equal(wheelReport(true, 10, 5, "SGR", cell), "\x1b[<64;10;5M");
  equal(wheelReport(false, 10, 5, "SGR", cell), "\x1b[<65;10;5M");
  equal(wheelReport(true, 1, 2, "DEFAULT", cell), '\x1b[M`!"');
  equal(wheelReport(false, 300, 2, "DEFAULT", cell), '\x1b[Ma\xff"');
  equal(wheelReport(true, 2, 3, "SGR_PIXELS", cell), "\x1b[<64;12;40M");
});
