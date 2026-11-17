// A note's editor (docs/UI.md "Notes"): a plain <textarea> that saves itself, plus a status line
// with who edited it last, the save state and the caret position. The text, caret and undo history
// live in the note's draft (notes.ts), so moving the tab between groups keeps them. Web and Muster
// addresses in the text open on Ctrl+click (⌘-click on a Mac); a plain click places the caret.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { authorName, draftOf, draftsChanged, editDraft, ensureBody, flushDraft, isDirty, notes, undoDraft } from "../notes.ts";
import { mapOffset } from "../note_merge.ts";
import { clean, endGroup, kindOf } from "../note_undo.ts";
import { ago } from "../agents.ts";
import { connStatus } from "../rpc.ts";
import { schedules } from "../state.ts";
import { setNoteTriggers, showLink } from "../actions.ts";
import { linkAt, parseMusterLink, type TextLink } from "../links.ts";
import { type LinkHit, NoteLinks } from "../note_links.ts";
import { IconZap } from "../icons.tsx";
import { cx, isMac } from "../util.ts";

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

/** The letter of a shortcut's key, by its place on the keyboard when the layout isn't Latin
 * (Ctrl+я on a Russian layout is Ctrl+Z), as Muster's own shortcuts match. */
function letter(e: KeyboardEvent): string {
  const k = e.key.toLowerCase();
  if (/^[a-z]$/.test(k)) return k;
  return /^Key[A-Z]$/.test(e.code) ? e.code.slice(3).toLowerCase() : k;
}

/** Scroll `el` so offset `at` shows: WebKit doesn't when the selection is set from script. A
 * hidden copy of the textarea, as wide and holding the text up to `at`, says how far down it is. */
function reveal(el: HTMLTextAreaElement, at: number) {
  const m = el.cloneNode() as HTMLTextAreaElement;
  m.removeAttribute("data-note-id");
  m.setAttribute("aria-hidden", "true");
  m.style.cssText = `position:absolute;visibility:hidden;flex:none;height:0;width:${el.offsetWidth}px`;
  el.after(m);
  m.value = el.value.slice(0, at);
  const cs = getComputedStyle(el);
  const bottom = m.scrollHeight - (parseFloat(cs.paddingBottom) || 0);
  m.remove();
  const top = bottom - (parseFloat(cs.lineHeight) || 18);
  if (top < el.scrollTop || bottom > el.scrollTop + el.clientHeight) {
    el.scrollTop = Math.max(0, top - el.clientHeight / 3);
  }
}

/** The key that makes a click open a link: ⌘ on a Mac (where Ctrl+click is a right click), Ctrl
 * elsewhere. */
const linkMod = (e: { ctrlKey: boolean; metaKey: boolean }) => isMac ? e.metaKey : e.ctrlKey;
const MOD_CLICK = isMac ? "⌘-click" : "Ctrl+click";
const MOD_ENTER = isMac ? "⌘Enter" : "Ctrl+Enter";

/** Open a link from a note: a web address in the browser (from the native window, the system
 * browser), a Muster address in Muster, as a click on it in a terminal does. */
function openLink(l: TextLink) {
  if (l.kind === "web") {
    globalThis.open(l.uri, "_blank", "noopener,noreferrer");
    return;
  }
  const m = parseMusterLink(l.uri);
  if (m) showLink(m);
}

/** What the status line says a link under the pointer opens: a note by its title. */
function linkLabel(l: TextLink): string {
  const m = l.kind === "muster" ? parseMusterLink(l.uri) : null;
  const n = m?.kind === "note" ? notes.value.get(m.id) : undefined;
  return n ? `note “${n.title}”` : l.uri;
}

/** Note `id`'s editor. Every note tab of a group keeps its editor mounted (`hidden` while it isn't
 * the visible tab), so switching tabs keeps the textarea and its scroll. */
export function NoteView({ id, hidden = false }: { id: number; hidden?: boolean }) {
  const n = notes.value.get(id);
  void draftsChanged.value;
  const now = useNow();
  const ref = useRef<HTMLTextAreaElement>(null);
  /** Where the selection started before the edit on its way (for undo; `beforeinput`). */
  const from = useRef<number | null>(null);
  const [pos, setPos] = useState({ line: 1, col: 1 });
  /** The link under the pointer, and whether the key that opens it is down (it is underlined). */
  const [hover, setHover] = useState<{ hit: LinkHit; mod: boolean } | null>(null);
  /** The link the caret is in (Ctrl+Enter opens it). */
  const [atCaret, setAtCaret] = useState<TextLink | null>(null);
  /** Look for the link under the pointer again: the text changed without an input event (an
   * agent's change, undo), so the status line must not name what is no longer there. */
  const relook = useRef(() => {});
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
    if (!el) return;
    setPos(caret(el.value, el.selectionStart));
    const l = linkAt(el.value, el.selectionStart, el.selectionEnd);
    setAtCaret((p) => p?.start === l?.start && p?.uri === l?.uri ? p : l);
  };

  // Ctrl+Z / Ctrl+Shift+Z: the note's own history (note_undo.ts), never the browser's, which
  // WebKitGTK has no key for and which an agent's change wipes.
  const undo = (again: boolean) => {
    const el = ref.current;
    const c = undoDraft(id, again);
    if (!el || !c) return;
    el.setRangeText(c.insert, c.at, c.end);
    el.setSelectionRange(c.sel[0], c.sel[1]);
    reveal(el, c.sel[0]);
    track();
    relook.current();
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
      relook.current();
    };
    const onBefore = (e: InputEvent) => {
      // The browser's own Undo and Redo (its context menu, when it offers them there, which it
      // decides by its own history) use the note's instead of replaying a stale one.
      if (e.inputType === "historyUndo" || e.inputType === "historyRedo") {
        e.preventDefault();
        undo(e.inputType === "historyRedo");
      } else from.current = el.selectionStart;
    };
    // Each input method composition is a step of its own.
    const onCompose = () => endGroup(d.history);
    el.addEventListener("beforeinput", onBefore);
    el.addEventListener("compositionstart", onCompose);
    return () => {
      el.removeEventListener("beforeinput", onBefore);
      el.removeEventListener("compositionstart", onCompose);
      d.sel = [el.selectionStart, el.selectionEnd];
      d.onRemote = null;
      if (isDirty(d)) flushDraft(id);
    };
  }, [id, !!d]);

  // Links under the pointer: found on a move, on Ctrl going down or up while the pointer rests,
  // and when the text scrolls, changes or reflows under it. A press with Ctrl on one is kept from
  // the textarea (no caret move, no selection); its release on the same link opens it, unless the
  // pointer moved away from where it was pressed, or the text changed, in between.
  useEffect(() => {
    const el = ref.current;
    if (!el || !d) return;
    const map = new NoteLinks(el);
    let at: { x: number; y: number } | null = null;
    let mod = false;
    let pressed: { link: TextLink; x: number; y: number } | null = null;
    let frame = 0;
    let shown = "";
    const look = () => {
      frame = 0;
      const hit = at ? map.at(at.x, at.y) : null;
      const key = hit ? `${hit.link.start}|${hit.link.uri}|${mod}|${JSON.stringify(hit.boxes)}` : "";
      el.classList.toggle("on-link", !!hit && mod);
      if (key === shown) return;
      shown = key;
      setHover(hit ? { hit, mod } : null);
    };
    const soon = () => {
      if (!frame) frame = requestAnimationFrame(look);
    };
    const changed = () => {
      pressed = null;
      soon();
    };
    relook.current = changed;
    const moved = (p: { x: number; y: number }, e: MouseEvent) => Math.hypot(e.clientX - p.x, e.clientY - p.y) > 4;
    const onMove = (e: PointerEvent) => {
      at = { x: e.clientX, y: e.clientY };
      if (pressed && moved(pressed, e)) pressed = null;
      mod = linkMod(e);
      soon();
    };
    const onLeave = () => {
      at = null;
      soon();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Control" && e.key !== "Meta") return;
      mod = linkMod(e);
      if (at) soon();
    };
    const onBlur = () => {
      mod = false;
      pressed = null;
      soon();
    };
    const onDown = (e: MouseEvent) => {
      pressed = null;
      if (e.button !== 0 || !linkMod(e)) return;
      const hit = map.at(e.clientX, e.clientY);
      if (!hit) return;
      e.preventDefault();
      pressed = { link: hit.link, x: e.clientX, y: e.clientY };
    };
    // On the window, so a release anywhere else ends the press too.
    const onUp = (e: MouseEvent) => {
      const p = pressed;
      pressed = null;
      if (!p || e.button !== 0 || e.target !== el || moved(p, e)) return;
      const hit = map.at(e.clientX, e.clientY);
      if (hit && hit.link.start === p.link.start && hit.link.uri === p.link.uri) openLink(p.link);
    };
    const resized = new ResizeObserver(soon);
    resized.observe(el);
    el.addEventListener("pointermove", onMove);
    el.addEventListener("pointerleave", onLeave);
    el.addEventListener("scroll", soon);
    el.addEventListener("input", changed);
    el.addEventListener("pointercancel", onBlur);
    el.addEventListener("mousedown", onDown);
    globalThis.addEventListener("mouseup", onUp, true);
    globalThis.addEventListener("keydown", onKey, true);
    globalThis.addEventListener("keyup", onKey, true);
    globalThis.addEventListener("blur", onBlur);
    return () => {
      cancelAnimationFrame(frame);
      el.removeEventListener("pointermove", onMove);
      el.removeEventListener("pointerleave", onLeave);
      el.removeEventListener("scroll", soon);
      el.removeEventListener("input", changed);
      el.removeEventListener("pointercancel", onBlur);
      el.removeEventListener("mousedown", onDown);
      globalThis.removeEventListener("mouseup", onUp, true);
      resized.disconnect();
      relook.current = () => {};
      globalThis.removeEventListener("keydown", onKey, true);
      globalThis.removeEventListener("keyup", onKey, true);
      globalThis.removeEventListener("blur", onBlur);
      el.classList.remove("on-link");
      map.dispose();
      setHover(null);
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
        aria-describedby={`note-links-help-${id}`}
        spellcheck={false}
        placeholder="Type here. Agents read and add to this note too (note_read, note_append)."
        disabled={connStatus.value === "incompatible"}
        onInput={(e) => {
          const el = e.target as HTMLTextAreaElement;
          const ie = e as InputEvent;
          // Control characters (a pasted ESC) go at once, as saving would drop them anyway, so
          // what is recorded for undo is what the daemon keeps.
          if (!ie.isComposing && clean(el.value) !== el.value) {
            const at = clean(el.value.slice(0, el.selectionEnd)).length;
            el.value = clean(el.value);
            el.setSelectionRange(at, at);
          }
          editDraft(id, el.value, {
            from: from.current ?? Infinity,
            caret: el.selectionEnd,
            kind: kindOf(ie.inputType, ie.isComposing),
          });
          from.current = null;
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
          const k = letter(e);
          if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && k === "s") {
            e.preventDefault();
            flushDraft(id);
          }
          // Ctrl+Enter (⌘Enter) opens the link the caret is in, as Ctrl+click does.
          if (linkMod(e) && !e.altKey && !e.shiftKey && e.key === "Enter" && !e.isComposing) {
            const el = e.currentTarget as HTMLTextAreaElement;
            const l = linkAt(el.value, el.selectionStart, el.selectionEnd);
            if (l) {
              e.preventDefault();
              e.stopPropagation();
              openLink(l);
            }
          }
          // Undo; redo on Ctrl+Shift+Z or Ctrl+Y (⌘Z, ⇧⌘Z and ⌘Y on a Mac). Kept from the
          // browser's own undo and from every other key handler.
          const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
          if (mod && !e.altKey && !e.isComposing && (k === "z" || (k === "y" && !e.shiftKey))) {
            e.preventDefault();
            e.stopPropagation();
            undo(k === "y" || e.shiftKey);
          }
        }}
      />
      <span id={`note-links-help-${id}`} class="sr-only">
        {MOD_ENTER} or {MOD_CLICK} on a web or Muster address opens it.
      </span>
      {hover?.mod && ref.current && (
        <div
          class="note-link-lines"
          aria-hidden="true"
          style={{
            left: ref.current.offsetLeft + ref.current.clientLeft,
            top: ref.current.offsetTop + ref.current.clientTop,
            width: ref.current.clientWidth,
            height: ref.current.clientHeight,
          }}
        >
          {hover.hit.boxes.map((b, i) => <div key={i} style={{ left: b.x, top: b.y + b.h - 2, width: b.w }} />)}
        </div>
      )}
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
      <div class={cx("note-status", (hover || atCaret) && "linking")}>
        <span class={d.error || over ? "note-state err" : dirty || d.inflight ? "note-state busy" : "note-state"} role="status">
          {over ? "Too long to save: a note holds at most 1 MB" : state}
        </span>
        {hover || atCaret
          ? (
            <span class="note-link-hint">
              <span class="how">{hover ? (hover.mod ? "Click" : MOD_CLICK) : MOD_ENTER} to open</span>{" "}
              <span class="dest">{linkLabel(hover ? hover.hit.link : atCaret!)}</span>
            </span>
          )
          : (
            <span class="note-by" title={new Date(n.updated_at).toLocaleString()}>
              edited by {authorName(by)} · {ago(n.updated_at, now)}
            </span>
          )}
        <span class="note-spacer" />
        {n.triggers !== undefined && <TriggerSwitch id={id} on={!!n.triggers} title={n.title} />}
        <span class="note-pos">Ln {pos.line}, Col {pos.col}</span>
        <span class="note-size">{lines} {lines === 1 ? "line" : "lines"} · {size(bytes)}</span>
      </div>
    </div>
  );
}
