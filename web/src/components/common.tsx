// Shared bits: state dots, kbd, delegated tooltips, inline edit, spinner.
import type { JSX } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { AgentState } from "../types.ts";
import { STATE_LABEL } from "../types.ts";
import { keyLabel } from "../keys.ts";
import { cx } from "../util.ts";

export function StateDot({ state, class: c }: { state: AgentState | null; class?: string }) {
  if (!state) return <span class={cx("dot dot-none", c)} />;
  return <span class={cx("dot", "st-" + state, c)} title={STATE_LABEL[state]} />;
}

export function Kbd({ k }: { k: string }) {
  return (
    <span class="kbd-group">
      {keyLabel(k).map((p, i) => <kbd key={i}>{p}</kbd>)}
    </span>
  );
}

/** One tooltip element for the whole app, driven by [data-tip] attributes. */
export function TooltipHost() {
  const [tip, setTip] = useState<
    { text: string; keys?: string; x: number; y: number; below: boolean } | null
  >(null);
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  useEffect(() => {
    let timer = 0;
    let current: HTMLElement | null = null;
    const hide = () => {
      clearTimeout(timer);
      current = null;
      setTip(null);
    };
    const over = (e: PointerEvent) => {
      if (e.pointerType === "touch") return;
      const t = (e.target as HTMLElement).closest?.("[data-tip]") as HTMLElement | null;
      if (t === current) return;
      clearTimeout(timer);
      current = t;
      if (!t) {
        setTip(null);
        return;
      }
      timer = setTimeout(() => {
        if (!t.isConnected || current !== t || document.body.classList.contains("dragging")) return;
        const r = t.getBoundingClientRect();
        const below = r.top < 60;
        setTip({
          text: t.dataset.tip!,
          keys: t.dataset.keys,
          x: r.left + r.width / 2,
          y: below ? r.bottom + 6 : r.top - 6,
          below,
        });
      }, tipDelay(t));
    };
    addEventListener("pointerover", over);
    addEventListener("pointerdown", hide, true);
    addEventListener("keydown", hide, true);
    addEventListener("blur", hide);
    return () => {
      removeEventListener("pointerover", over);
      removeEventListener("pointerdown", hide, true);
      removeEventListener("keydown", hide, true);
      removeEventListener("blur", hide);
    };
  }, []);

  useLayoutEffect(() => {
    if (!tip || !ref.current) {
      setPos(null);
      return;
    }
    const r = ref.current.getBoundingClientRect();
    let left = tip.x - r.width / 2;
    left = Math.max(6, Math.min(innerWidth - r.width - 6, left));
    const top = tip.below ? tip.y : tip.y - r.height;
    setPos({ left, top });
  }, [tip]);

  if (!tip) return null;
  return (
    <div
      ref={ref}
      class="tooltip"
      role="tooltip"
      style={{ left: (pos?.left ?? -9999) + "px", top: (pos?.top ?? -9999) + "px" }}
    >
      {tip.text}
      {tip.keys && <Kbd k={tip.keys} />}
    </div>
  );
}

let lastTipAt = 0;
function tipDelay(_t: HTMLElement) {
  // Once a tooltip has shown, neighbours show quickly (like native toolbars).
  const now = Date.now();
  const d = now - lastTipAt < 800 ? 80 : 450;
  lastTipAt = now + d;
  return d;
}

/** Inline text edit: Enter commits, Esc/blur-without-change cancels, blur commits. */
export function InlineEdit(
  props: {
    value: string;
    onCommit: (v: string) => void;
    onCancel: () => void;
    class?: string;
    style?: JSX.CSSProperties;
  },
) {
  const ref = useRef<HTMLInputElement>(null);
  const done = useRef(false);
  useLayoutEffect(() => {
    const el = ref.current!;
    el.focus();
    el.select();
  }, []);
  const finish = (commit: boolean) => {
    if (done.current) return;
    done.current = true;
    const v = ref.current!.value.trim();
    if (commit && v && v !== props.value) props.onCommit(v);
    else props.onCancel();
  };
  return (
    <input
      ref={ref}
      class={cx("inline-edit", props.class)}
      style={props.style}
      defaultValue={props.value}
      spellcheck={false}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") finish(true);
        else if (e.key === "Escape") finish(false);
      }}
      onBlur={() => finish(true)}
      onClick={(e) => e.stopPropagation()}
      onDblClick={(e) => e.stopPropagation()}
      onPointerDown={(e) => e.stopPropagation()}
    />
  );
}

export function Spinner({ size = 12 }: { size?: number }) {
  return <span class="spinner" style={{ width: size + "px", height: size + "px" }} />;
}
