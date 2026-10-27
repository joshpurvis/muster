import { needsHuman, type SessionInfo } from "../types.ts";

export const FONT_FAMILY =
  '"JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Menlo, ui-monospace, monospace';
export const COLLAPSED_HEIGHT = 68;
export interface Grid {
  cols: number;
  rows: number;
}
export interface Cell {
  width: number;
  height: number;
}

export function normalizeCode(code: string): string {
  const clean = code.replace(/[\s-]/g, "");
  return clean.length === 64 ? clean.toLowerCase() : clean.toUpperCase();
}

export function sessionOrder(a: SessionInfo, b: SessionInfo): number {
  return Number(needsHuman(b.state)) - Number(needsHuman(a.state)) || b.state_since - a.state_since ||
    a.id - b.id;
}

export function phoneGrid(width: number, closedHeight: number, cell: Cell): Grid {
  return {
    cols: Math.max(2, Math.min(500, Math.floor(width / Math.max(1, cell.width)))),
    rows: Math.max(
      1,
      Math.min(300, Math.floor((closedHeight - COLLAPSED_HEIGHT) / Math.max(1, cell.height))),
    ),
  };
}

export function scrollPosition(position: number, base: number, height: number, overflow: number) {
  const y = Math.max(0, Math.min(position, base * height + overflow));
  const row = Math.min(base, Math.floor(y / height));
  const pan = Math.min(overflow, y - row * height);
  return { row, pan, remainder: y - row * height - pan };
}

/** A full-screen program keeps its own history (Claude Code's fullscreen view): the alternate
 * screen has no scrollback, so the phone can only show it by asking the program to scroll, with
 * the mouse wheel, as a desktop terminal does. xterm.js sends no wheel reports in X10 mode. */
export function programScrolls(buffer: "normal" | "alternate", mouseTrackingMode: string): boolean {
  return buffer === "alternate" && mouseTrackingMode !== "none" && mouseTrackingMode !== "x10";
}

/** A vertical drag over a full-screen program: pan the phone's view of the screen first, then
 * the part the pan can't take scrolls the program (negative = toward older output). */
export function splitPan(delta: number, pan: number, overflow: number) {
  const next = Math.max(0, Math.min(overflow, pan + delta));
  return { pan: next, rest: delta - (next - pan) };
}

/** Turn drag pixels into whole wheel notches, one per text row, carrying the rest. */
export function wheelSteps(px: number, remainder: number, step: number) {
  const total = remainder + px;
  const count = Math.trunc(total / Math.max(1, step));
  return { count, remainder: total - count * Math.max(1, step) };
}

/** One mouse-wheel report as xterm.js sends it to a program tracking the mouse, at 1-based
 * cell `col`, `row` in the program's active encoding, as the raw bytes to write. xterm.js has
 * only `DEFAULT`, `SGR` and `SGR_PIXELS` (it ignores the UTF-8 and URXVT modes, 1005/1015, so
 * a program asking for those gets `DEFAULT` from the desktop too). `DEFAULT` is one byte per
 * value, never UTF-8, and can't address a cell past 223: xterm.js sends nothing there, nor
 * does this (null), as for an encoding it doesn't know. */
export function wheelReport(
  up: boolean,
  col: number,
  row: number,
  encoding: string,
  cell: Cell,
): Uint8Array | null {
  const button = up ? 64 : 65;
  const text = (s: string) => new TextEncoder().encode(s);
  if (encoding === "DEFAULT") {
    if (col > 223 || row > 223) return null;
    return Uint8Array.of(0x1b, 0x5b, 0x4d, 32 + button, 32 + col, 32 + row);
  }
  if (encoding === "SGR_PIXELS") {
    const x = Math.floor((col - 0.5) * cell.width);
    const y = Math.floor((row - 0.5) * cell.height);
    return text(`\x1b[<${button};${x};${y}M`);
  }
  if (encoding === "SGR") return text(`\x1b[<${button};${col};${row}M`);
  return null;
}

/** `report` `count` times, as one write. */
export function repeatBytes(report: Uint8Array, count: number): Uint8Array {
  const out = new Uint8Array(report.length * Math.max(0, count));
  for (let i = 0; i < count; i++) out.set(report, i * report.length);
  return out;
}

/** How a phone left a full-screen program it scrolled back (per session, kept across leaving
 * and reopening it): `back` wheel notches up not yet sent back down, at cell `at`; `behind`
 * once the agent was busy meanwhile, so new lines may sit below the program's view and
 * returning the notches alone would fall short of the newest. */
export interface ProgramScroll {
  back: number;
  behind: boolean;
  at: { col: number; row: number } | null;
}

/** `count` notches were sent (negative = up) at `at`, while the agent was `busy` or not. */
export function noteWheel(p: ProgramScroll, count: number, at: ProgramScroll["at"], busy: boolean) {
  p.back = Math.max(0, p.back - count);
  if (count < 0) {
    p.at = at;
    if (busy) p.behind = true;
  }
}

/** The agent got busy (or still is): while scrolled back, new lines land below the view. */
export function noteBusy(p: ProgramScroll, busy: boolean) {
  if (busy && p.back > 0) p.behind = true;
}

/** Whether the phone's view of a program it scrolled back can be taken as the newest output. */
export function programAtLatest(p: ProgramScroll): boolean {
  return p.back === 0 && !p.behind;
}

/** Wheel notches down that bring a program back to its newest line: the ones sent up, plus,
 * when output may have arrived meanwhile, five screens more (a program stops at its bottom). */
export function returnNotches(p: ProgramScroll, rows: number): number {
  return p.back + (p.behind ? 5 * rows : 0);
}

/** Ctrl+End as xterm.js sends it: Claude Code's fullscreen view binds it to "scroll to bottom"
 * in the same context as the wheel, so it reaches the newest line however much arrived. */
export const CTRL_END = new TextEncoder().encode("\x1b[1;5F");

export interface BufferLine {
  isWrapped: boolean;
  translateToString(trimRight?: boolean): string;
}
export interface TextBuffer {
  length: number;
  getLine(index: number): BufferLine | undefined;
}

/** Keep an offset per physical row so selection opens at the current scroll position. */
export function bufferText(buffer: TextBuffer, start = 0, end = buffer.length) {
  let text = "";
  const offsets: number[] = [];
  for (let i = start; i < Math.min(end, buffer.length); i++) {
    const line = buffer.getLine(i);
    if (i > start && !line?.isWrapped) text += "\n";
    offsets.push(text.length);
    text += line?.translateToString(!buffer.getLine(i + 1)?.isWrapped) ?? "";
  }
  return { text, offsets };
}

export function relativeTime(since: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}
