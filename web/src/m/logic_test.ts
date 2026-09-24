import { bufferText, normalizeCode, phoneGrid, relativeTime, scrollPosition, sessionOrder } from "./logic.ts";
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
