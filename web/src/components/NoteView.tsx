// A note's editor (docs/UI.md "Notes"): a plain <textarea> that saves itself, plus a status line
// with who edited it last, the save state and the caret position. The text and caret live in the
// note's draft (notes.ts), so moving the tab between groups keeps them.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { authorName, draftOf, draftsChanged, editDraft, ensureBody, flushDraft, isDirty, notes } from "../notes.ts";
import { mapOffset } from "../note_merge.ts";
import { ago } from "../agents.ts";
import { connStatus } from "../rpc.ts";
import { schedules } from "../state.ts";
import { setNoteTriggers } from "../actions.ts";
import { IconZap } from "../icons.tsx";
import { cx } from "../util.ts";

/** "Allow triggers" in a note's status line: off by default, and only you switch it (agents
 * can't). With it off, a schedule's note trigger never fires on this note's changes. */
function TriggerSwitch({ id, on, title }: { id: number; on: boolean; title: string }) {
  const watching = schedules.value.filter((s) => s.when.kind === "note" && s.when.note_id === id).length;
  const what = watching === 0 ? "no trigger watches it yet" : watching === 1 ? "1 trigger watches it" : `${watching} triggers watch it`;
  const tip = on
    ? `Triggers allowed: changes to “${title}” can start runs of the triggers that watch it (${what}). Click to switch off.`
    : `Triggers off: changes to “${title}” never start a run (${what}). Click to allow them; agents can't.`;
  return (
    <button
      type="button"
      class={cx("note-trig", on && "on")}
      role="switch"
      aria-checked={on}
      aria-label={`Allow triggers on this note (${what})`}
      data-tip={tip}
      onMouseDown={(e) => e.preventDefault()}
      onClick={() => setNoteTriggers(id, !on)}
    >
      <IconZap size={11} />
      {on ? `triggers on${watching ? ` · ${watching}` : ""}` : "triggers off"}
    </button>
  );
}

const MAX_BYTES = 1024 * 1024;

function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

function caret(text: string, at: number): { line: number; col: number } {
  const before = text.slice(0, at);
  const nl = before.lastIndexOf("\n");
  return { line: before.split("\n").length, col: at - nl };
}

function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(2)} MB`;
}

const utf8 = new TextEncoder();

/** Note `id`'s editor. Every note tab of a group keeps its editor mounted (`hidden` while it isn't
 * the visible tab), so switching tabs keeps the textarea and its undo history. */
export function NoteView({ id, hidden = false }: { id: number; hidden?: boolean }) {
  const n = notes.value.get(id);
  void draftsChanged.value;
  const now = useNow();
  const ref = useRef<HTMLTextAreaElement>(null);
  const [pos, setPos] = useState({ line: 1, col: 1 });
  const d = n ? draftOf(n) : undefined;
  // Only its summary is known yet (hello carries no text): fetch the text.
  useEffect(() => {
    if (n && n.body === undefined && !d) ensureBody(id);
  }, [id, n?.version, !!d]);
  // Another tab of the group took its place: save what was typed.
  useEffect(() => {
    if (hidden && d && isDirty(d)) flushDraft(id);
  }, [hidden]);

  const track = () => {
    const el = ref.current;
    if (el) setPos(caret(el.value, el.selectionStart));
  };

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !d) return;
    el.value = d.text;
    if (d.sel) el.setSelectionRange(d.sel[0], d.sel[1]);
    track();
    // Someone else's change (or a merge) replaced the text: keep the caret and the scroll.
    d.onRemote = (before, after) => {
      if (el.value === after) return;
      const [s, e, top] = [el.selectionStart, el.selectionEnd, el.scrollTop];
      el.value = after;
      el.setSelectionRange(mapOffset(before, after, s), mapOffset(before, after, e));
      el.scrollTop = top;
      track();
    };
    return () => {
      d.sel = [el.selectionStart, el.selectionEnd];
      d.onRemote = null;
      if (isDirty(d)) flushDraft(id);
    };
  }, [id, !!d]);

  if (!n) return <div class="note-view note-missing" hidden={hidden}>This note was deleted.</div>;
  if (!d) return <div class="note-view note-missing" hidden={hidden}>Loading note…</div>;

  const bytes = utf8.encode(d.text).length;
  const over = bytes > MAX_BYTES;
  const dirty = isDirty(d);
  const state = d.inflight ? "Saving…" : d.error ? d.error : dirty ? "Edited" : "Saved";
  const by = n.updated_by;
  const lines = d.text === "" ? 0 : d.text.split("\n").length;

  return (
    <div class="note-view" hidden={hidden}>
      <textarea
        ref={ref}
        class="note-text"
        data-note-id={id}
        aria-label={`Note: ${n.title}`}
        spellcheck={false}
        placeholder="Type here. Agents read and add to this note too (note_read, note_append)."
        disabled={connStatus.value === "incompatible"}
        onInput={(e) => {
          editDraft(id, (e.target as HTMLTextAreaElement).value);
          track();
        }}
        onKeyUp={track}
        onPointerUp={track}
        onBlur={() => {
          const el = ref.current;
          if (el) d.sel = [el.selectionStart, el.selectionEnd];
          if (isDirty(d)) flushDraft(id);
        }}
        onKeyDown={(e) => {
          // Ctrl+S saves now (it saves by itself anyway); never the browser's "Save page".
          if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "s") {
            e.preventDefault();
            flushDraft(id);
          }
        }}
      />
      {d.restored && (
        <div class="note-merge" role="status">
          Brought back typing that wasn't saved when this window last closed or reloaded; it is being saved now.
        </div>
      )}
      {d.conflictWith && (
        <div class="note-merge" role="status">
          Your typing and {d.conflictWith}'s change touched the same lines. Both are kept: theirs is between the ──── lines.
        </div>
      )}
      <div class="note-status">
        <span class={d.error || over ? "note-state err" : dirty || d.inflight ? "note-state busy" : "note-state"} role="status">
          {over ? "Too long to save: a note holds at most 1 MB" : state}
        </span>
        <span class="note-by" title={new Date(n.updated_at).toLocaleString()}>
          edited by {authorName(by)} · {ago(n.updated_at, now)}
        </span>
        <span class="note-spacer" />
        {n.triggers !== undefined && <TriggerSwitch id={id} on={!!n.triggers} title={n.title} />}
        <span class="note-pos">Ln {pos.line}, Col {pos.col}</span>
        <span class="note-size">{lines} {lines === 1 ? "line" : "lines"} · {size(bytes)}</span>
      </div>
    </div>
  );
}
