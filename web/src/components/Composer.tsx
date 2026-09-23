// Slack-style message box at the bottom of a channel.
import { signal } from "@preact/signals";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { Channel, SessionInfo } from "../types.ts";
import { layouts, sessions, sessionsByChannel, toast } from "../state.ts";
import * as A from "../actions.ts";
import * as terms from "../terms.ts";
import { StateDot } from "./common.tsx";
import { type MenuItem, openMenuAt } from "./Menu.tsx";
import { IconChevronDown, IconSend, IconX, KindIcon } from "../icons.tsx";
import { findPane } from "../layout/tree.ts";
import { STATE_LABEL } from "../types.ts";
import { cx, load, save } from "../util.ts";

/** Per-channel explicit target (overrides the focused-pane default). */
const targetOverride = signal<Map<number, number>>(new Map());
const history: Map<number, string[]> = new Map(
  Object.entries(load<Record<string, string[]>>("history", {})).map(([k, v]) => [Number(k), v]),
);
const drafts = new Map<number, string>();

function pushHistory(ch: number, text: string) {
  const h = [text, ...(history.get(ch) ?? []).filter((x) => x !== text)].slice(0, 30);
  history.set(ch, h);
  save("history", Object.fromEntries(history));
}

export function composerTarget(ch: number): SessionInfo | null {
  const list = sessionsByChannel.value.get(ch) ?? [];
  const o = targetOverride.value.get(ch);
  if (o !== undefined) {
    const s = sessions.value.get(o);
    if (s && s.channel_id === ch) return s;
  }
  const l = layouts.value.get(ch);
  const focused = l ? findPane(l.root, l.focus)?.active ?? null : null;
  const fs = focused !== null ? sessions.value.get(focused) : undefined;
  if (fs && fs.kind !== "shell" && fs.state !== "exited") return fs;
  const agent = list.find((s) => s.kind !== "shell" && s.state !== "exited");
  if (agent) return agent;
  if (fs) return fs;
  return list[0] ?? null;
}

export function Composer({ c }: { c: Channel }) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const [text, setText] = useState(drafts.get(c.id) ?? "");
  const [histIdx, setHistIdx] = useState(-1);
  const [flash, setFlash] = useState<string | null>(null);
  const [sending, setSending] = useState(0);
  const target = composerTarget(c.id);
  const list = sessionsByChannel.value.get(c.id) ?? [];
  const chipRef = useRef<HTMLButtonElement>(null);

  // Per-channel drafts.
  useEffect(() => {
    setText(drafts.get(c.id) ?? "");
    setHistIdx(-1);
  }, [c.id]);
  useEffect(() => {
    drafts.set(c.id, text);
  }, [text, c.id]);

  useEffect(() => {
    if (A.composerFocus.value) ref.current?.focus();
  }, [A.composerFocus.value]);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    const max = Math.round(innerHeight * 0.4);
    el.style.height = Math.min(el.scrollHeight, max) + "px";
    el.style.overflowY = el.scrollHeight > max ? "auto" : "hidden";
  }, [text]);

  const showFlash = (s: string) => {
    setFlash(s);
    setTimeout(() => setFlash((cur) => (cur === s ? null : cur)), 2200);
  };

  const textRef = useRef(text);
  textRef.current = text;

  // Optimistic: clear right away (sends can take a while when the agent is busy) and put the
  // text back only if the send fails and the box is still empty.
  const restore = (raw: string) => {
    if (!textRef.current) setText(raw);
  };

  const submit = async (interrupt: boolean) => {
    const raw = text;
    const t = raw.trim();
    if (!t) return;
    if (t.startsWith("/note ") || t === "/note") {
      const body = t.slice(5).trim();
      if (!body) return;
      setText("");
      pushHistory(c.id, t);
      setSending((n) => n + 1);
      try {
        await A.postNote(c.id, body);
        showFlash("Note posted");
      } catch (e) {
        toast("error", `Couldn't post note: ${e instanceof Error ? e.message : e}`);
        restore(raw);
      } finally {
        setSending((n) => n - 1);
      }
      return;
    }
    if (!target) {
      toast("warn", "No session to send to. Start one first, or use /note.");
      return;
    }
    const to = target;
    setText("");
    setHistIdx(-1);
    pushHistory(c.id, raw);
    setSending((n) => n + 1);
    try {
      const r = await A.send(to.id, raw.replace(/\s+$/, ""), interrupt ? "interrupt" : "queue");
      showFlash(r.delivered ? `Sent to ${to.name}` : `Queued for ${to.name}`);
    } catch (e) {
      toast("error", `Send failed: ${e instanceof Error ? e.message : e}`);
      restore(raw);
    } finally {
      setSending((n) => n - 1);
    }
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.isComposing) return;
    if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
      e.preventDefault();
      submit(e.ctrlKey || e.metaKey);
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      if (target) terms.focusTerminal(target.id) || A.focusActiveTerminalSoon();
      else A.focusActiveTerminalSoon();
      return;
    }
    const el = ref.current!;
    const h = history.get(c.id) ?? [];
    if (e.key === "ArrowUp" && (text === "" || histIdx >= 0) && el.selectionStart === 0) {
      if (histIdx + 1 < h.length) {
        e.preventDefault();
        setHistIdx(histIdx + 1);
        setText(h[histIdx + 1]);
      }
    } else if (e.key === "ArrowDown" && histIdx >= 0 && el.selectionEnd === el.value.length) {
      e.preventDefault();
      const n = histIdx - 1;
      setHistIdx(n);
      setText(n >= 0 ? h[n] : "");
    }
  };

  const pickTarget = () => {
    const items: MenuItem[] = [
      { header: "Send to" },
      ...list.map((s): MenuItem => ({
        label: s.name,
        icon: <KindIcon kind={s.kind} size={13} />,
        hint: STATE_LABEL[s.state],
        checked: target?.id === s.id,
        run: () => {
          const m = new Map(targetOverride.value);
          m.set(c.id, s.id);
          targetOverride.value = m;
          ref.current?.focus();
        },
      })),
      "sep",
      {
        label: "Follow focused pane",
        checked: !targetOverride.value.has(c.id),
        run: () => {
          const m = new Map(targetOverride.value);
          m.delete(c.id);
          targetOverride.value = m;
          ref.current?.focus();
        },
      },
    ];
    openMenuAt(chipRef.current!, items, { alignRight: false, minWidth: 220 });
  };

  // The menu opens below the anchor; the composer is at the bottom so it gets clamped upward.
  const isNote = text.startsWith("/note");
  const busy = target && ["working", "starting"].includes(target.state);
  const placeholder = !target
    ? "No sessions yet. Type /note to post to the timeline"
    : busy
    ? `Message ${target.name} (busy: will queue)…`
    : `Message ${target.name}…`;

  return (
    <div class={cx("composer", isNote && "note-mode")}>
      <div class="composer-box">
        <button
          type="button"
          ref={chipRef}
          class="target-chip"
          onClick={pickTarget}
          disabled={!list.length}
          data-tip="Choose who receives the message"
        >
          {isNote ? <span class="note-tag">note</span> : target
            ? (
              <>
                <StateDot state={target.state} />
                <KindIcon kind={target.kind} size={13} />
                <span class="target-name">{target.name}</span>
              </>
            )
            : <span class="target-name muted">no target</span>}
          <IconChevronDown size={12} />
        </button>
        <textarea
          ref={ref}
          rows={1}
          value={text}
          placeholder={placeholder}
          spellcheck
          onInput={(e) => {
            setText((e.target as HTMLTextAreaElement).value);
            setHistIdx(-1);
          }}
          onKeyDown={onKey}
        />
        <div class="composer-right">
          {target && target.queued > 0 && (
            <span class="queued-chip">
              queued {target.queued}
              <button
                type="button"
                aria-label="Clear queue"
                data-tip="Clear queue"
                onClick={() => A.clearQueue(target.id)}
              >
                <IconX size={11} />
              </button>
            </span>
          )}
          {sending > 0 && !flash && <span class="composer-flash">Sending…</span>}
          {flash && <span class="composer-flash">{flash}</span>}
          <button
            type="button"
            class={cx("send-btn", text.trim() && "ready")}
            aria-label="Send"
            data-tip="Send · Ctrl+Enter sends and interrupts"
            data-keys="Enter"
            disabled={!text.trim()}
            onClick={() => submit(false)}
          >
            <IconSend size={15} />
          </button>
        </div>
      </div>
      <div class="composer-hint">
        <span>
          <kbd>Enter</kbd> send
        </span>
        <span>
          <kbd>Shift</kbd>+<kbd>Enter</kbd> newline
        </span>
        <span>
          <kbd>Ctrl</kbd>+<kbd>Enter</kbd> send &amp; interrupt
        </span>
        <span>
          <kbd>/note</kbd> post to timeline
        </span>
        <span>
          <kbd>Esc</kbd> back to terminal
        </span>
      </div>
    </div>
  );
}
