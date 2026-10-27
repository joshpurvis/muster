import { useCallback, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { SessionInfo } from "../types.ts";
import { Composer } from "./Composer.tsx";
import {
  bindTerminal,
  closeSession,
  markOpened,
  notify,
  prefs,
  scrollProgram,
  setPrefs,
} from "./store.ts";
import { SessionTitle } from "./SessionActions.tsx";
import { PhoneTerminal } from "./term.ts";
import { Icon, Settings, StateDot } from "./ui.tsx";
import { clearAlert } from "./alerts.ts";

export function SessionView({ session: s }: { session: SessionInfo }) {
  const stage = useRef<HTMLDivElement>(null);
  const host = useRef<HTMLDivElement>(null);
  const surface = useRef<HTMLDivElement>(null);
  const pre = useRef<HTMLPreElement>(null);
  const terminal = useRef<PhoneTerminal | null>(null);
  const closedHeight = useRef(0);
  const [bottom, setBottom] = useState(true);
  const [snapshot, setSnapshot] = useState<ReturnType<PhoneTerminal["snapshot"]> | null>(null);
  const fontSize = prefs.value.fontSize;
  const onHeight = useCallback((height: number) => {
    stage.current?.style.setProperty("--composer-height", `${height + 16}px`);
  }, []);
  useLayoutEffect(() => {
    const el = stage.current!;
    let width = el.clientWidth;
    const measure = () => {
      if (el.clientWidth !== width) closedHeight.current = 0;
      width = el.clientWidth;
      closedHeight.current = Math.max(closedHeight.current, el.clientHeight);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    const term = new PhoneTerminal(
      s.id,
      host.current!,
      surface.current!,
      s,
      prefs.peek().fontSize,
      () =>
        closedHeight.current - 8 -
        parseFloat(getComputedStyle(el.querySelector(".composer-wrap")!).paddingBottom),
      setBottom,
      (size) => setPrefs({ fontSize: size }),
      (data) => scrollProgram(s.id, data),
    );
    terminal.current = term;
    bindTerminal(term);
    markOpened(s);
    clearAlert(s.id);
    return () => {
      ro.disconnect();
      bindTerminal(null);
      term.dispose();
      terminal.current = null;
    };
  }, [s.id]);
  useLayoutEffect(() => terminal.current?.fontSize(fontSize), [fontSize]);
  useLayoutEffect(() => {
    const el = pre.current;
    if (!snapshot || !el?.firstChild) return;
    const range = document.createRange();
    range.setStart(el.firstChild, Math.min(snapshot.offset, snapshot.text.length));
    range.collapse(true);
    el.scrollTop = range.getBoundingClientRect().top - el.getBoundingClientRect().top;
  }, [snapshot]);
  const toggleSelect = () => {
    const active = !snapshot;
    (document.activeElement as HTMLElement)?.blur();
    terminal.current?.select(active);
    setSnapshot(active ? terminal.current?.snapshot() ?? null : null);
  };
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      notify("Copied");
    } catch {
      notify("Could not copy. Long-press the text to select and copy.");
    }
  };
  return (
    <main class="session-screen">
      <header class="topbar session-header">
        <button type="button" class="icon-button" aria-label="Back to sessions" onClick={closeSession}>
          <Icon name="back" />
        </button>
        <StateDot state={s.state} />
        <SessionTitle session={s} />
        <button
          type="button"
          class={`select-toggle ${snapshot ? "active" : ""}`}
          aria-pressed={!!snapshot}
          onClick={toggleSelect}
        >
          {snapshot ? "Done" : "Select"}
        </button>
        <Settings />
      </header>
      {snapshot && (
        <div class="selection-tools">
          <span>Output paused</span>
          <button type="button" onClick={() => copy(snapshot.screen)}>Copy screen</button>
          <button type="button" onClick={() => copy(snapshot.text)}>Copy all</button>
        </div>
      )}
      <div class={`terminal-stage ${snapshot ? "selecting" : ""}`} ref={stage}>
        <div class="terminal-viewport" aria-hidden={!!snapshot}>
          <div class="phone-terminal" ref={host} />
        </div>
        <div
          class="touch-surface"
          ref={surface}
          role="region"
          aria-label="Terminal output. Drag to scroll, pinch to change font size. Use Select to copy."
          style={{ display: snapshot ? "none" : undefined }}
        />
        {snapshot && (
          <pre class="selection-text" ref={pre} style={{ fontSize, lineHeight: `${snapshot.cell.height}px` }}>
            {snapshot.text}
          </pre>
        )}
        {!snapshot && !bottom && (
          <button
            type="button"
            class="jump-bottom"
            onClick={() => terminal.current?.bottom()}
          >
            <Icon name="down" />Latest output
          </button>
        )}
        <div style={{ display: snapshot ? "none" : undefined }}>
          <Composer key={s.id} session={s} onHeight={onHeight} />
        </div>
      </div>
    </main>
  );
}
