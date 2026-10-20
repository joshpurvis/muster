// Merging a note's unsaved typing onto a newer revision (docs/PROTOCOL.md "Notes"). Pure, no
// DOM: unit tested in note_merge_test.ts.
//
// Each side's change against the common base is taken as one span (common prefix and suffix
// cut off). Spans that don't overlap are both applied, so the usual case (an agent appends
// while the human types higher up, or both type in different places) merges cleanly. Spans
// that overlap keep both texts: the human's version first, then the other version between
// marker lines. Nothing typed by either side is ever dropped.

/** One side's change: base[start, end) became `text`. */
interface Span {
  start: number;
  end: number;
  text: string;
}

function span(base: string, next: string): Span {
  const max = Math.min(base.length, next.length);
  let p = 0;
  while (p < max && base.charCodeAt(p) === next.charCodeAt(p)) p++;
  let s = 0;
  while (s < max - p && base.charCodeAt(base.length - 1 - s) === next.charCodeAt(next.length - 1 - s)) s++;
  return { start: p, end: base.length - s, text: next.slice(p, next.length - s) };
}

export interface Merged {
  text: string;
  /** The two changes overlapped: both versions were kept, between marker lines. */
  conflict: boolean;
}

export const CONFLICT_START = (who: string) => `──── ${who} changed this part at the same time; their version: ────`;
export const CONFLICT_END = "──── end of their version ────";

/** Three-way merge: `mine` and `theirs` were both made from `base`. `who` names theirs. */
export function merge3(base: string, mine: string, theirs: string, who = "someone"): Merged {
  if (mine === base || mine === theirs) return { text: theirs, conflict: false };
  if (theirs === base) return { text: mine, conflict: false };
  const m = span(base, mine);
  const t = span(base, theirs);
  // Disjoint (touching counts as disjoint). At the same point, mine goes first.
  if (m.end <= t.start || (m.start === t.start && m.end === m.start && t.end === t.start)) {
    return { text: base.slice(0, m.start) + m.text + base.slice(m.end, t.start) + t.text + base.slice(t.end), conflict: false };
  }
  if (t.end <= m.start) {
    return { text: base.slice(0, t.start) + t.text + base.slice(t.end, m.start) + m.text + base.slice(m.end), conflict: false };
  }
  // Overlap: each side's version of the union of both spans.
  const u1 = Math.min(m.start, t.start), u2 = Math.max(m.end, t.end);
  const mineU = base.slice(u1, m.start) + m.text + base.slice(m.end, u2);
  const theirsU = base.slice(u1, t.start) + t.text + base.slice(t.end, u2);
  if (mineU === theirsU) return { text: base.slice(0, u1) + mineU + base.slice(u2), conflict: false };
  const nl = (s: string) => (s === "" || s.endsWith("\n") ? s : s + "\n");
  const block = `${nl(mineU)}${CONFLICT_START(who)}\n${nl(theirsU)}${CONFLICT_END}\n`;
  const rest = base.slice(u2);
  return { text: base.slice(0, u1) + block + (rest.startsWith("\n") ? rest.slice(1) : rest), conflict: true };
}

/** Where offset `i` of `old` lands in `next` (a caret kept steady across someone else's edit). */
export function mapOffset(old: string, next: string, i: number): number {
  const c = span(old, next);
  if (i <= c.start) return i;
  if (i >= c.end) return i + (next.length - old.length);
  return c.start + c.text.length;
}
