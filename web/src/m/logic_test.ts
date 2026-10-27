import {
  bufferText,
  normalizeCode,
  noteBusy,
  noteSession,
  noteWheel,
  phoneGrid,
  programAtLatest,
  type ProgramScroll,
  programScrollFor,
  programScrolls,
  relativeTime,
  repeatBytes,
  ReplayCursor,
  returnNotches,
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

const bytes = (b: Uint8Array | null) => b && [...b];
const ascii = (t: string) => [...new TextEncoder().encode(t)];

Deno.test("wheel reports are the bytes xterm.js sends in each mouse encoding", () => {
  const cell = { width: 8, height: 16 };
  equal(bytes(wheelReport(true, 10, 5, "SGR", cell)), ascii("\x1b[<64;10;5M"));
  equal(bytes(wheelReport(false, 10, 5, "SGR", cell)), ascii("\x1b[<65;10;5M"));
  equal(bytes(wheelReport(true, 2, 3, "SGR_PIXELS", cell)), ascii("\x1b[<64;12;40M"));
  // Legacy: one raw byte per value. Col 100, row 81 is 0x84 and 0x71, not UTF-8 (c2 84), which
  // a legacy decoder would cut after six bytes, leaving a stray `q` typed into the program.
  equal(bytes(wheelReport(false, 100, 81, "DEFAULT", cell)), [0x1b, 0x5b, 0x4d, 97, 132, 113]);
  equal(bytes(wheelReport(true, 223, 223, "DEFAULT", cell)), [0x1b, 0x5b, 0x4d, 96, 255, 255]);
  // Past 223 legacy can't say where: xterm.js sends nothing, and neither does the phone.
  equal(wheelReport(true, 224, 5, "DEFAULT", cell), null);
  equal(wheelReport(true, 5, 300, "DEFAULT", cell), null);
  equal(wheelReport(true, 5, 5, "URXVT", cell), null);
  equal(bytes(repeatBytes(Uint8Array.of(1, 2), 3)), [1, 2, 1, 2, 1, 2]);
});

Deno.test("returning to the newest line covers output that arrived while scrolled back", () => {
  const p: ProgramScroll = { run: 7, back: 0, behind: false, at: null };
  // Idle agent: scrolled up 10, back down 4, the rest returns exactly what was sent.
  noteWheel(p, -10, { col: 3, row: 7 }, false);
  noteWheel(p, 4, { col: 9, row: 9 }, false);
  equal(p, { run: 7, back: 6, behind: false, at: { col: 3, row: 7 } });
  equal(programAtLatest(p), false);
  equal(returnNotches(p, 40, "claude"), 6);
  noteWheel(p, 6, { col: 3, row: 7 }, false);
  equal(programAtLatest(p), true);
  // Busy agent: new lines sit below the view, so the notches alone fall short. Dragging back
  // down by them doesn't count as the newest; the return over-sends (programs stop at the end).
  noteWheel(p, -5, { col: 1, row: 2 }, false);
  noteBusy(p, true);
  noteWheel(p, 5, { col: 1, row: 2 }, true);
  equal(p.back, 0);
  equal(programAtLatest(p), false);
  // Wheel reports only, never a key (an editor opened from Claude would take it); a Claude
  // session over-sends more, since an agent can print a lot while you read.
  equal(returnNotches(p, 40, "shell"), 200);
  equal(returnNotches(p, 40, "claude"), 800);
  // Busy only after the view was back at the bottom: nothing below to miss.
  const q: ProgramScroll = { run: 7, back: 0, behind: false, at: null };
  noteBusy(q, true);
  equal(programAtLatest(q), true);
  noteWheel(q, -1, null, true);
  equal(q.behind, true);
});

Deno.test("saved scroll state follows the session's run and its unseen activity", () => {
  const saved = new Map<number, ProgramScroll>();
  const claude = { id: 4, pid: 100, kind: "claude", state: "idle" };
  const p = programScrollFor(saved, claude);
  noteWheel(p, -8, { col: 2, row: 3 }, false);
  // Reopening the same run gets the same state back.
  equal(programScrollFor(saved, claude) === p, true);
  // The agent worked while the session view was closed: dragging back by the notches alone
  // must not pass for the newest.
  noteSession(saved, { ...claude, state: "working" });
  equal(p.behind, true);
  noteSession(saved, { id: 99, pid: 1, kind: "shell", state: "working" });
  equal(saved.has(99), false);
  // A restart (new pid) never inherits the old run's notches.
  noteSession(saved, { ...claude, pid: 101 });
  equal(saved.has(4), false);
  const fresh = programScrollFor(saved, { ...claude, pid: 101 });
  equal(fresh, { run: 101, back: 0, behind: false, at: null });
  equal(programScrollFor(saved, { ...claude, pid: 102 }) === fresh, false);
});

Deno.test("the replay cursor counts live bytes on from the replay and appends only deltas", () => {
  const c = new ReplayCursor();
  equal(c.since(), null);
  c.info({ id: 1, kind: "screen", offset: 100, generation: 2, epoch: "e" });
  equal(c.replay(), false);
  c.output(5);
  c.output(7);
  equal(c.since(), { offset: 112, generation: 2, epoch: "e" });
  c.info({ id: 1, kind: "delta", offset: 150, generation: 2, epoch: "e" });
  equal(c.replay(), true);
  c.output(1);
  equal(c.since()?.offset, 151);
  // A replay without attach_info (an older daemon) replaces and leaves the offset unknown.
  equal(c.replay(), false);
  equal(c.since(), null);
  c.output(3);
  equal(c.since(), null);
  // A session from an earlier daemon run can't be resumed.
  c.info({ id: 1, kind: "full", offset: 0, generation: null, epoch: "e" });
  c.replay();
  equal(c.since(), null);
});
