// Notes (docs/UI.md "Notes", docs/PROTOCOL.md "Notes"): every note the daemon knows, and the
// editor drafts. The daemon holds the text and its revision. An editor keeps a draft per note:
// the text on screen and the server revision it was made from. Typing saves itself (debounced)
// with `note.write {base_rev}`. A newer revision arriving while the draft has unsaved typing is
// merged with it (note_merge.ts), and a write refused because the note moved on is merged the
// same way and sent again, so neither the human's typing nor an agent's change is lost.
//
// Drafts live here, not in the editor component, so moving the tab to another group (which
// remounts the editor) keeps everything, including the caret.

import { signal } from "@preact/signals";
import type { Note, NoteAuthor } from "./types.ts";
import { rpc } from "./rpc.ts";
import { merge3 } from "./note_merge.ts";

/** Every note, by id (`hello.notes`, then `note_upserted` / `note_removed`). */
export const notes = signal<Map<number, Note>>(new Map());
/** The backend has notes (hello carried `notes`). */
export const notesSupported = signal(false);
/** Bumped when a draft's save state changes (the editors' status lines read it). */
export const draftsChanged = signal(0);

/** How long typing rests before it is saved. */
export const SAVE_DELAY_MS = 500;

export interface Draft {
  id: number;
  /** The server text the draft is built on, and its revision. */
  base: string;
  baseRev: number;
  /** What the editor shows. Unsaved when it differs from `base`. */
  text: string;
  /** A `note.write` is on its way; server updates wait for its answer. */
  inflight: boolean;
  /** The newest server note that arrived during the write. */
  waiting: Note | null;
  timer: number;
  /** The last save failed for this reason (shown in the editor; retried on the next change). */
  error: string | null;
  /** The last merge kept two overlapping versions (who wrote the other one). */
  conflictWith: string | null;
  /** The caret, kept while the editor is unmounted. */
  sel: [number, number] | null;
  /** The editor's own hook: the text changed under it (old, new). */
  onRemote: ((before: string, after: string) => void) | null;
}

const drafts = new Map<number, Draft>();

export function authorName(a: NoteAuthor | undefined): string {
  if (!a || a.kind === "human") return "you";
  return a.name || "an agent";
}

export function draftOf(n: Note): Draft {
  let d = drafts.get(n.id);
  if (!d) {
    d = { id: n.id, base: n.body, baseRev: n.rev, text: n.body, inflight: false, waiting: null, timer: 0, error: null,
      conflictWith: null, sel: null, onRemote: null };
    drafts.set(n.id, d);
  }
  return d;
}

export const isDirty = (d: Draft) => d.text !== d.base;

function bump() {
  draftsChanged.value++;
}

function setText(d: Draft, text: string) {
  if (text === d.text) return;
  const before = d.text;
  d.text = text;
  d.onRemote?.(before, text);
}

/** A newer revision of the note reached the draft. */
function rebase(d: Draft, n: Note) {
  // Our own write's echo, or an older event.
  if (n.rev <= d.baseRev) return;
  if (!isDirty(d)) {
    d.base = n.body;
    d.baseRev = n.rev;
    setText(d, n.body);
    return;
  }
  const m = merge3(d.base, d.text, n.body, authorName(n.updated_by));
  d.base = n.body;
  d.baseRev = n.rev;
  if (m.conflict) d.conflictWith = authorName(n.updated_by);
  setText(d, m.text);
  bump();
  if (isDirty(d)) schedule(d, 0);
}

function serverNote(n: Note) {
  const d = drafts.get(n.id);
  if (!d) return;
  if (d.inflight) {
    if (!d.waiting || n.rev >= d.waiting.rev) d.waiting = n;
    return;
  }
  rebase(d, n);
}

function schedule(d: Draft, ms = SAVE_DELAY_MS) {
  clearTimeout(d.timer);
  d.timer = setTimeout(() => save(d), ms);
}

/** The editor's text changed (typing, paste, cut). */
export function editDraft(id: number, text: string) {
  const d = drafts.get(id);
  if (!d || text === d.text) return;
  d.text = text;
  d.error = null;
  d.conflictWith = null;
  bump();
  schedule(d);
}

/** Save now (Ctrl+S, leaving the editor, closing the tab). */
export function flushDraft(id: number): Promise<void> {
  const d = drafts.get(id);
  if (!d) return Promise.resolve();
  clearTimeout(d.timer);
  return save(d);
}

async function save(d: Draft): Promise<void> {
  if (d.inflight || !isDirty(d) || !drafts.has(d.id)) return;
  const sent = d.text;
  d.inflight = true;
  bump();
  let refused = false;
  try {
    const n = await rpc.call<Note>("note.write", { id: d.id, body: sent, base_rev: d.baseRev });
    d.base = n.body;
    d.baseRev = n.rev;
    d.error = null;
    // The daemon keeps plain text only; show what it kept when nothing was typed since.
    if (d.text === sent) setText(d, n.body);
    upsert(n);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (/changed since revision/.test(msg)) refused = true;
    else if (/no note with id/.test(msg)) d.error = "This note was deleted.";
    else d.error = /^Disconnected|^timeout/i.test(msg) ? "Not saved yet: reconnecting…" : `Not saved: ${msg}`;
  } finally {
    d.inflight = false;
  }
  const w = d.waiting;
  d.waiting = null;
  if (w) rebase(d, w);
  let delay = SAVE_DELAY_MS;
  if (refused) {
    // Someone wrote first: merge onto their text and try again.
    try {
      rebase(d, await rpc.call<Note>("note.get", { id: d.id }));
      delay = 0;
    } catch { /* the event will bring it; retry after the usual pause */ }
  }
  bump();
  if (isDirty(d) && !d.inflight && !d.error && drafts.has(d.id)) schedule(d, delay);
}

function upsert(n: Note) {
  const cur = notes.value.get(n.id);
  if (cur && cur.rev > n.rev) return;
  const next = new Map(notes.value);
  next.set(n.id, n);
  notes.value = next;
}

// ---------- server state ----------

export function applyNotesHello(list: Note[] | undefined) {
  notesSupported.value = Array.isArray(list);
  notes.value = new Map((list ?? []).map((n) => [n.id, n]));
  for (const d of [...drafts.values()]) {
    const n = notes.value.get(d.id);
    if (!n) {
      dropDraft(d.id);
      continue;
    }
    serverNote(n);
    // A save lost to a dropped connection goes again.
    if (isDirty(d) && !d.inflight) {
      d.error = null;
      schedule(d, 0);
    }
  }
}

export function applyNoteUpsert(n: Note) {
  // The window and title change without the text: keep the newest text we have.
  const cur = notes.value.get(n.id);
  const next = new Map(notes.value);
  next.set(n.id, cur && cur.rev > n.rev ? { ...n, body: cur.body, rev: cur.rev, updated_at: cur.updated_at, updated_by: cur.updated_by } : n);
  notes.value = next;
  serverNote(n);
}

export function applyNoteRemoved(id: number) {
  if (!notes.value.has(id)) return;
  const next = new Map(notes.value);
  next.delete(id);
  notes.value = next;
  dropDraft(id);
}

/** Record a note from an RPC reply (its event may lag). */
export function knowNote(n: Note) {
  applyNoteUpsert(n);
}

function dropDraft(id: number) {
  const d = drafts.get(id);
  if (!d) return;
  clearTimeout(d.timer);
  drafts.delete(id);
  bump();
}

/** Unsaved typing anywhere (the page is about to go away). */
export function flushAllDrafts(): Promise<unknown> {
  return Promise.all([...drafts.values()].filter(isDirty).map((d) => flushDraft(d.id)));
}

/** The draft of note `id`, if an editor opened it. */
export function draft(id: number): Draft | undefined {
  return drafts.get(id);
}
