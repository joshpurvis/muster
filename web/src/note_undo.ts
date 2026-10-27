// Undo and redo in a note's editor (docs/UI.md "Notes"). Pure, no DOM: unit tested in
// note_undo_test.ts.
//
// The browser's own undo can't serve a note: WebKitGTK (the native window) binds no key to it,
// and a browser forgets it whenever the text is set from script, which every agent change does.
// So the editor keeps its own history of the human's edits, on the note's draft (notes.ts), which
// also lets it survive moving the tab to another group.
//
// Each step is one span: at `at`, `removed` became `inserted`. Typing groups into one step per
// word or burst (a pause ends it); deleting groups the same way; a paste, a cut, a drop or an
// autocorrection is a step of its own, and so is each input method's text. Someone else's
// change moves the steps it doesn't touch, so Ctrl+Z only ever takes back the human's own text.
// A step the change touched (or, where repeated text makes it ambiguous where the change was,
// could have touched) can't be taken back without rewriting theirs: it and every older step are
// dropped, and Ctrl+Z stops there.

/** How an edit was made: typing and deleting group into words and bursts, the rest never. */
export type EditKind = "type" | "delete" | "compose" | "other";

export interface Step {
  /** Where the step starts. In an undo step the text holds `inserted` there; in a redo step, `removed`. */
  at: number;
  removed: string;
  inserted: string;
  kind: EditKind;
  /** When it last grew (ms). */
  time: number;
}

export interface History {
  /** Oldest first. */
  undo: Step[];
  /** Next to redo last. */
  redo: Step[];
  /** The newest undo step may still grow (typing goes on). */
  open: boolean;
}

/** Typing more than this long after the last key starts a new step. */
export const GROUP_MS = 1000;
const MAX_STEPS = 500;
/** Characters kept over all steps (a 1 MB paste, many times over, would otherwise pile up). */
const MAX_CHARS = 4_000_000;

export const newHistory = (): History => ({ undo: [], redo: [], open: false });

/** The next edit starts a new step (an input method starts composing). */
export const endGroup = (h: History) => {
  h.open = false;
};

/** What an `input` event's `inputType` did. */
export function kindOf(inputType: string | undefined, composing = false): EditKind {
  // WebKit commits an input method's text after `compositionend`, as these two.
  if (composing || inputType === "insertCompositionText" || inputType === "deleteCompositionText" ||
    inputType === "insertFromComposition") return "compose";
  if (inputType === "insertText" || inputType === "insertLineBreak" || inputType === "insertParagraph") return "type";
  if (inputType === "deleteByCut" || inputType === "deleteByDrag") return "other";
  if (inputType?.startsWith("delete")) return "delete";
  return "other";
}

const isHigh = (c: number) => c >= 0xd800 && c <= 0xdbff;
const isLow = (c: number) => c >= 0xdc00 && c <= 0xdfff;

/** Plain text, as the daemon keeps it (src/notes.rs `clean`): CRLF and CR become LF; control
 * characters other than tab and newline are dropped. The editor cleans what is typed or pasted
 * before it is recorded, so the save never changes the human's own text under the history. */
// deno-lint-ignore no-control-regex
export const clean = (s: string) => s.replace(/\r\n?/g, "\n").replace(/[\0-\x08\x0b-\x1f\x7f-\x9f]/g, "");

/** How the human made an edit: where the selection started before it, where it left the caret,
 * and its kind. */
export interface How {
  from: number;
  caret: number;
  kind: EditKind;
}

/**
 * The human's edit from `before` to `after`. It began no later than the selection's start
 * (`from`) and ended no earlier than the caret it left: so typing "a" into "a|a" is placed where
 * it was typed, not at the end, and typing "t" over a selected "this" replaces "this" rather
 * than deleting "his".
 */
export function editOf(before: string, after: string, from: number, caret: number): { at: number; removed: string; inserted: string } {
  const max = Math.min(before.length, after.length);
  const tail = Math.min(max, after.length - Math.max(0, Math.min(caret, after.length)));
  let s = 0;
  while (s < tail && before.charCodeAt(before.length - 1 - s) === after.charCodeAt(after.length - 1 - s)) s++;
  if (s > 0 && isLow(before.charCodeAt(before.length - s))) s--;
  const head = Math.min(max - s, Math.max(0, from));
  let p = 0;
  while (p < head && before.charCodeAt(p) === after.charCodeAt(p)) p++;
  if (p > 0 && isHigh(before.charCodeAt(p - 1))) p--;
  return { at: p, removed: before.slice(p, before.length - s), inserted: after.slice(p, after.length - s) };
}

/** Whether edit `e` (just made, of `kind`) carries on step `top` instead of starting a new one. */
function continues(top: Step, e: { at: number; removed: string; inserted: string }, kind: EditKind, now: number): boolean {
  if (kind !== top.kind || kind === "other") return false;
  // An input method's text is one step however long it takes.
  if (kind === "compose") return true;
  if (now - top.time > GROUP_MS) return false;
  if (kind === "type") {
    // Typing on at the end of the step, and a new word after a space starts a new step.
    return e.removed === "" && e.at === top.at + top.inserted.length &&
      !(/\s$/.test(top.inserted) && /^\S/.test(e.inserted));
  }
  // Deleting on, backwards (Backspace) or forwards (Delete), from where the step left off.
  return e.inserted === "" && top.inserted === "" && (e.at + e.removed.length === top.at || e.at === top.at);
}

/** The human turned `before` into `after` (an `input` event). */
export function record(h: History, before: string, after: string, { from, caret, kind }: How, now: number) {
  if (before === after) return;
  const e = editOf(before, after, from, caret);
  h.redo = [];
  const top = h.undo.at(-1);
  if (h.open && top && continues(top, e, kind, now)) {
    // One span over both: `top` happened first, `e` on the text it left (`before`).
    const lo = Math.min(top.at, e.at);
    const hi = Math.max(top.at + top.inserted.length, e.at + e.removed.length);
    top.removed = before.slice(lo, top.at) + top.removed + before.slice(top.at + top.inserted.length, hi);
    top.inserted = after.slice(lo, hi + e.inserted.length - e.removed.length);
    top.at = lo;
    top.time = now;
  } else {
    h.undo.push({ ...e, kind, time: now });
    let chars = 0;
    for (const s of h.undo) chars += s.removed.length + s.inserted.length;
    while (h.undo.length > 1 && (h.undo.length > MAX_STEPS || chars > MAX_CHARS)) {
      const s = h.undo.shift()!;
      chars -= s.removed.length + s.inserted.length;
    }
  }
  h.open = kind !== "other";
}

/**
 * The part of `before` that changing it into `after` may have touched, under every placement of
 * the change. Repeated text makes the place ambiguous: removing one of two "todo\n" lines may
 * be the first or the second, so the whole run counts.
 */
export function reach(before: string, after: string): { lo: number; hi: number } {
  const max = Math.min(before.length, after.length);
  let p = 0;
  while (p < max && before.charCodeAt(p) === after.charCodeAt(p)) p++;
  let s = 0;
  while (s < max && before.charCodeAt(before.length - 1 - s) === after.charCodeAt(after.length - 1 - s)) s++;
  // Placed as late as it goes (the longest common start), then as early (the longest common end).
  let lo = Math.min(p, max - s);
  let hi = before.length - Math.min(s, max - p);
  // Never between the two halves of a surrogate pair.
  if (lo > 0 && isLow(before.charCodeAt(lo))) lo--;
  if (hi < before.length && isLow(before.charCodeAt(hi))) hi++;
  return { lo, hi };
}

/**
 * Carry the steps of `list` (newest last; `undo` says which stack) across someone else's change,
 * which may have touched [start, end) of the text and changed its length by `delta`. The newest
 * step applies to the text as it is, each older one to what the newer left, so the change is
 * carried down the list step by step. The first step it may overlap, and every older one, is
 * dropped.
 */
function carry(list: Step[], undo: boolean, start: number, end: number, delta: number): Step[] {
  for (let i = list.length - 1; i >= 0; i--) {
    const s = list[i];
    const here = undo ? s.inserted : s.removed;
    const there = undo ? s.removed : s.inserted;
    if (end <= s.at) s.at += delta; // the change is before the step
    else if (start >= s.at + here.length) {
      // after it: the older steps see it where it was before this step
      start += there.length - here.length;
      end += there.length - here.length;
    } else return list.slice(i + 1);
  }
  return list;
}

/** Someone else's change (an agent, a merge) turned `before` into `after`. */
export function rebaseHistory(h: History, before: string, after: string) {
  h.open = false;
  if (before === after) return;
  const { lo, hi } = reach(before, after);
  const delta = after.length - before.length;
  h.undo = carry(h.undo, true, lo, hi, delta);
  h.redo = carry(h.redo, false, lo, hi, delta);
}

/**
 * Someone else made the same change as the human's unsaved one (`text`, made from `base`), so
 * merging left `text` as it was: what the human typed there is theirs too, and its steps (with
 * every older one) are dropped, as for any change that touches them.
 */
export function sameChange(h: History, base: string, text: string) {
  h.open = false;
  if (base === text) return;
  const { lo, hi } = reach(base, text);
  const end = hi + text.length - base.length;
  h.undo = carry(h.undo, true, lo, end, 0);
  h.redo = carry(h.redo, false, lo, end, 0);
}

/** An undo or redo to make: text[at, end) becomes `insert`, then the selection is `sel`. */
export interface Change {
  at: number;
  end: number;
  insert: string;
  sel: [number, number];
}

/** Take back the newest step of `text` (null: nothing to undo). It moves to the redo stack. */
export function undo(h: History, text: string): Change | null {
  h.open = false;
  const s = h.undo.pop();
  if (!s) return null;
  // Out of step with the text (it changed without `rebaseHistory`): never undo blind.
  if (!text.startsWith(s.inserted, s.at)) {
    h.undo = [];
    h.redo = [];
    return null;
  }
  h.redo.push(s);
  // What comes back is selected; typing taken back leaves the caret where it began.
  return { at: s.at, end: s.at + s.inserted.length, insert: s.removed, sel: [s.at, s.at + s.removed.length] };
}

/** Make the last undone step again (null: nothing to redo). */
export function redo(h: History, text: string): Change | null {
  h.open = false;
  const s = h.redo.pop();
  if (!s) return null;
  if (!text.startsWith(s.removed, s.at)) {
    h.undo = [];
    h.redo = [];
    return null;
  }
  h.undo.push(s);
  const caret = s.at + s.inserted.length;
  return { at: s.at, end: s.at + s.removed.length, insert: s.inserted, sel: [caret, caret] };
}

/** `text` with change `c` made. */
export const applyChange = (text: string, c: Change) => text.slice(0, c.at) + c.insert + text.slice(c.end);
