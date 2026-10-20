// Notes (docs/UI.md "Notes", docs/PROTOCOL.md "Notes"): every note the daemon knows, and the
// editor drafts. The daemon holds the text and its revision. An editor keeps a draft per note:
// the text on screen and the server revision it was made from. Typing saves itself (debounced)
// with `note.write {base_rev}`. A newer revision arriving while the draft has unsaved typing is
// merged with it (note_merge.ts), and a write refused because the note moved on is merged the
// same way and sent again, so neither the human's typing nor an agent's change is lost.
//
// Drafts live here, not in the editor component, so moving the tab to another group (which
// remounts the editor) keeps everything, including the caret. Unsaved typing is also kept in
// localStorage, per window, until the daemon has it: a reload, a closed window or a crash
// brings it back the next time this window opens (merged onto the note as it is by then).
//
// `hello.notes` carries summaries only (no text, so it stays small however many notes there
// are); a note's text arrives with its events, or is fetched (`note.get`) when an editor shows it.

import { signal } from "@preact/signals";
import type { Note, NoteAuthor, NoteInfo, NoteSummary } from "./types.ts";
import { rpc } from "./rpc.ts";
import { merge3 } from "./note_merge.ts";

/** Every note, by id (`hello.notes`, then `note_upserted` / `note_removed`). */
export const notes = signal<Map<number, NoteInfo>>(new Map());
/** The backend has notes (hello carried `notes`). */
export const notesSupported = signal(false);
/** Bumped when a draft's save state changes (the editors' status lines read it). */
export const draftsChanged = signal(0);

/** How long typing rests before it is saved. */
export const SAVE_DELAY_MS = 500;
const PERSIST_DELAY_MS = 300;
const KEY_PREFIX = "muster.noteDraft.";
/** Kept drafts nobody came back for are forgotten after this long. */
const KEEP_MS = 30 * 24 * 3600 * 1000;

export interface Draft {
  id: number;
  /** The server text the draft is built on, and its revision. */
  base: string;
  baseRev: number;
  /** What the editor shows. Unsaved when it differs from `base`. */
  text: string;
  /** The `note.write` on its way (server updates wait for its answer). */
  inflight: Promise<void> | null;
  /** The last text sent and the revision it was sent from, until its answer: an update that is
   * exactly that write (its echo, after a lost answer) is ours, not someone else's change. */
  sent: { text: string; baseRev: number } | null;
  /** The newest server note that arrived during the write. */
  waiting: Note | null;
  timer: number;
  persistTimer: number;
  /** The last save failed for this reason (shown in the editor; retried on the next change). */
  error: string | null;
  /** The last merge kept two overlapping versions (who wrote the other one). */
  conflictWith: string | null;
  /** Unsaved typing from before a reload or a closed window was brought back. */
  restored: boolean;
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

const utf8 = new TextEncoder();

/** What the UI keeps of a note record (a summary, or a whole note with its text). */
function infoOf(n: Note | NoteSummary): NoteInfo {
  if ("body" in n) {
    const body = n.body;
    return {
      ...n, body, bytes: utf8.encode(body).length,
      lines: body === "" ? 0 : body.split("\n").length - (body.endsWith("\n") ? 1 : 0),
      preview: (body.split("\n").map((l) => l.trim()).find((l) => l) ?? "").slice(0, 120),
    };
  }
  const { open: _open, ...rest } = n;
  return rest;
}

/** Keep `next` unless what we have is newer (every change raises `version`). */
function upsert(next: NoteInfo) {
  const cur = notes.value.get(next.id);
  if (cur && cur.version > next.version) return;
  if (cur && cur.version === next.version && next.body === undefined && cur.body !== undefined) return;
  const m = new Map(notes.value);
  m.set(next.id, next);
  notes.value = m;
}

export function draftOf(n: NoteInfo): Draft | undefined {
  let d = drafts.get(n.id);
  if (!d && n.body !== undefined) {
    d = newDraft(n.id, n.body, n.rev, n.body);
    drafts.set(n.id, d);
  }
  return d;
}

function newDraft(id: number, base: string, baseRev: number, text: string): Draft {
  return { id, base, baseRev, text, inflight: null, sent: null, waiting: null, timer: 0, persistTimer: 0, error: null,
    conflictWith: null, restored: false, sel: null, onRemote: null };
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

// ---------- kept drafts (localStorage) ----------

/** The window whose kept drafts this page writes and restores (undefined: not known yet). */
let draftWindow: number | null | undefined = undefined;
const keyOf = (id: number, win = draftWindow) => `${KEY_PREFIX}${win ?? "none"}.${id}`;

interface Kept {
  base: string;
  baseRev: number;
  text: string;
  at: number;
}

/** Keep (or, once saved, forget) draft `d`'s unsaved typing now. False: it could not be kept. */
function persistNow(d: Draft): boolean {
  clearTimeout(d.persistTimer);
  d.persistTimer = 0;
  try {
    if (isDirty(d)) {
      const kept: Kept = { base: d.base, baseRev: d.baseRev, text: d.text, at: Date.now() };
      localStorage.setItem(keyOf(d.id), JSON.stringify(kept));
    } else localStorage.removeItem(keyOf(d.id));
    return true;
  } catch {
    return false;
  }
}

function persistSoon(d: Draft) {
  if (d.persistTimer) return;
  d.persistTimer = setTimeout(() => persistNow(d), PERSIST_DELAY_MS);
}

function forgetKept(id: number) {
  try {
    localStorage.removeItem(keyOf(id));
  } catch { /* storage unavailable */ }
}

/** Keep every draft's unsaved typing now (the page is going away). False: some could not be kept. */
export function persistAllDrafts(): boolean {
  let ok = true;
  for (const d of drafts.values()) if (!persistNow(d) && isDirty(d)) ok = false;
  return ok;
}

/**
 * This page shows window `win`: its kept drafts are written under it from now on, and the ones a
 * page of this window left behind (a reload, a closed window, a crash) come back, merged onto the
 * note as it is now, and are saved.
 */
export function restoreDrafts(win: number | null) {
  if (draftWindow === win) return;
  const before = draftWindow;
  draftWindow = win;
  // Drafts of this page move with it to the new window id.
  for (const d of drafts.values()) {
    if (before !== undefined) {
      try {
        localStorage.removeItem(keyOf(d.id, before));
      } catch { /* storage unavailable */ }
    }
    if (isDirty(d)) persistNow(d);
  }
  let found: [number, Kept][] = [];
  try {
    const now = Date.now();
    for (let i = localStorage.length - 1; i >= 0; i--) {
      const k = localStorage.key(i);
      if (!k?.startsWith(KEY_PREFIX)) continue;
      let kept: Kept | null = null;
      try {
        kept = JSON.parse(localStorage.getItem(k) ?? "null");
      } catch { /* unreadable: dropped below */ }
      if (!kept || typeof kept.text !== "string" || now - (kept.at ?? 0) > KEEP_MS) {
        localStorage.removeItem(k);
        continue;
      }
      const [w, id] = k.slice(KEY_PREFIX.length).split(".");
      if (w === String(win ?? "none") && Number(id) > 0) found.push([Number(id), kept]);
    }
  } catch {
    found = [];
  }
  for (const [id, kept] of found) {
    if (drafts.has(id)) continue;
    const d = newDraft(id, kept.base, kept.baseRev, kept.text);
    // Maybe its last save did land before the page went: then the note is exactly that.
    d.sent = { text: kept.text, baseRev: kept.baseRev };
    d.restored = true;
    drafts.set(id, d);
    rpc.call<Note>("note.get", { id }).then((n) => {
      upsert(infoOf(n));
      if (drafts.get(id) !== d) return;
      rebase(d, n);
      if (isDirty(d)) schedule(d, 0);
      else {
        d.restored = false;
        forgetKept(id);
      }
      bump();
    }).catch((e) => {
      if (/no note with id/.test(e instanceof Error ? e.message : String(e))) {
        forgetKept(id);
        dropDraft(id);
      }
    });
  }
  bump();
}

// ---------- merging and saving ----------

/** A newer revision of the note reached the draft. */
function rebase(d: Draft, n: Note) {
  // Our own write's echo, or an older event.
  if (n.rev <= d.baseRev) return;
  // Exactly the write we sent (its answer was lost, or came after this): ours, nothing to merge.
  if (d.sent && n.rev === d.sent.baseRev + 1 && n.body === d.sent.text) {
    d.base = n.body;
    d.baseRev = n.rev;
    d.sent = null;
    persistSoon(d);
    bump();
    if (isDirty(d)) schedule(d);
    return;
  }
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
  persistSoon(d);
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
  d.timer = setTimeout(() => void save(d), ms);
}

/** The editor's text changed (typing, paste, cut). */
export function editDraft(id: number, text: string) {
  const d = drafts.get(id);
  if (!d || text === d.text) return;
  d.text = text;
  d.error = null;
  d.conflictWith = null;
  d.restored = false;
  persistSoon(d);
  bump();
  schedule(d);
}

/**
 * Save now (Ctrl+S, leaving the editor, closing the tab, the page going away), and resolve once
 * the daemon has everything typed so far: a write on its way is awaited, and what was typed or
 * merged meanwhile goes in another one. Gives up only on a failure (the draft keeps it).
 */
export async function flushDraft(id: number): Promise<void> {
  for (let i = 0; i < 6; i++) {
    const d = drafts.get(id);
    if (!d) return;
    clearTimeout(d.timer);
    if (d.inflight) {
      await d.inflight;
      continue;
    }
    if (!isDirty(d) || d.error) return;
    await save(d);
  }
}

function save(d: Draft): Promise<void> {
  if (d.inflight) return d.inflight;
  if (!isDirty(d) || drafts.get(d.id) !== d) return Promise.resolve();
  const run = async () => {
    const sent = d.text;
    d.sent = { text: sent, baseRev: d.baseRev };
    bump();
    let refused = false;
    try {
      const n = await rpc.call<Note>("note.write", { id: d.id, body: sent, base_rev: d.baseRev });
      d.base = n.body;
      d.baseRev = n.rev;
      d.sent = null;
      d.error = null;
      // The daemon keeps plain text only; show what it kept when nothing was typed since.
      if (d.text === sent) setText(d, n.body);
      upsert(infoOf(n));
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/changed since revision/.test(msg)) refused = true;
      else if (/no note with id/.test(msg)) d.error = "This note was deleted.";
      else d.error = /^Disconnected|^timeout/i.test(msg) ? "Not saved yet: reconnecting…" : `Not saved: ${msg}`;
    } finally {
      d.inflight = null;
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
    persistNow(d);
    bump();
    if (isDirty(d) && !d.inflight && !d.error && drafts.get(d.id) === d) schedule(d, delay);
  };
  d.inflight = run();
  return d.inflight;
}

// ---------- server state ----------

export function applyNotesHello(list: NoteSummary[] | undefined) {
  notesSupported.value = Array.isArray(list);
  const prev = notes.value;
  const next = new Map<number, NoteInfo>();
  for (const s of list ?? []) {
    const cur = prev.get(s.id);
    // The same version: the text we already have is still current.
    next.set(s.id, cur && cur.version === s.version && cur.body !== undefined ? { ...infoOf(s), body: cur.body } : infoOf(s));
  }
  notes.value = next;
  for (const d of [...drafts.values()]) {
    const s = next.get(d.id);
    if (!s) {
      forgetKept(d.id);
      dropDraft(d.id);
      continue;
    }
    if (s.rev > d.baseRev) {
      // Changed while we were away: fetch the text to merge onto (the save follows).
      rpc.call<Note>("note.get", { id: d.id }).then((n) => applyNoteUpsert(n)).catch(() => {});
    }
    // A save lost to a dropped connection goes again.
    if (isDirty(d) && !d.inflight) {
      d.error = null;
      schedule(d, s.rev > d.baseRev ? SAVE_DELAY_MS : 0);
    }
  }
}

export function applyNoteUpsert(n: Note) {
  upsert(infoOf(n));
  serverNote(n);
}

export function applyNoteRemoved(id: number) {
  if (!notes.value.has(id)) return;
  const next = new Map(notes.value);
  next.delete(id);
  notes.value = next;
  forgetKept(id);
  dropDraft(id);
}

/** Record a note from an RPC reply (its event may lag). */
export function knowNote(n: Note) {
  applyNoteUpsert(n);
}

/** Fetch note `id`'s text when only its summary is known (an editor is about to show it). */
const fetching = new Set<number>();
export function ensureBody(id: number) {
  const n = notes.value.get(id);
  if (!n || n.body !== undefined || fetching.has(id)) return;
  fetching.add(id);
  rpc.call<Note>("note.get", { id }).then(applyNoteUpsert).catch(() => {}).finally(() => fetching.delete(id));
}

/** Notes whose text holds `query` (two characters or more), with the matching line. */
export function searchNotes(query: string): Promise<{ id: number; line: string }[]> {
  if (query.trim().length < 2 || !notesSupported.value) return Promise.resolve([]);
  return rpc.call<{ id: number; line: string }[]>("note.search", { query: query.trim() }).catch(() => []);
}

function dropDraft(id: number) {
  const d = drafts.get(id);
  if (!d) return;
  clearTimeout(d.timer);
  clearTimeout(d.persistTimer);
  drafts.delete(id);
  bump();
}

/** Save unsaved typing everywhere (the page is about to go away). Kept drafts are written first. */
export function flushAllDrafts(): Promise<unknown> {
  persistAllDrafts();
  return Promise.all([...drafts.values()].filter((d) => isDirty(d) || d.inflight).map((d) => flushDraft(d.id)));
}

/** Notes with typing the daemon doesn't have yet. */
export function unsavedNotes(): number[] {
  return [...drafts.values()].filter((d) => isDirty(d) || d.inflight).map((d) => d.id);
}

/** The draft of note `id`, if an editor opened it. */
export function draft(id: number): Draft | undefined {
  return drafts.get(id);
}
