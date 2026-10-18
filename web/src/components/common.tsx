// Shared bits: state dots, kbd, delegated tooltips, inline edit, spinner.
import { Fragment } from "preact";
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

/** Inline text edit of one or more fields side by side (`[task] · [session]`): Enter commits,
 * Esc / leaving without a change cancels, leaving commits. Tab / Shift+Tab move between the fields
 * (and only blur when there is one). `onCommit` gets every field's trimmed value; `byKey` says it
 * closed with Enter / Esc (the caller puts the focus back), not by focus leaving. Removed before it
 * closed (its tab went away), it cancels. A field's `tag` is a short visible label before it (its
 * accessible name is `label`); clicking it focuses the field. */
export function InlineEdit(
  props: {
    fields: { value: string; label: string; tag?: string }[];
    /** The field that starts focused. */
    focus?: number;
    onCommit: (values: string[], byKey: boolean) => void;
    onCancel: (byKey: boolean) => void;
    class?: string;
  },
) {
  const wrap = useRef<HTMLSpanElement>(null);
  const refs = useRef<(HTMLInputElement | null)[]>([]);
  const done = useRef(false);
  const cancel = useRef(props.onCancel);
  cancel.current = props.onCancel;
  const n = props.fields.length;
  const focusField = (i: number) => {
    const el = refs.current[i];
    el?.focus();
    el?.select();
  };
  useLayoutEffect(() => {
    const el = refs.current[props.focus ?? 0];
    el?.focus({ preventScroll: true });
    el?.select();
    // All of the fields in view (a crowded strip scrolls to them), with the tab around them.
    const box = wrap.current?.closest<HTMLElement>(".tab") ?? wrap.current;
    box?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, []);
  // Cleanups run before the inputs leave the DOM, so the blur that removal may fire is ignored.
  useEffect(() => () => {
    if (done.current) return;
    done.current = true;
    cancel.current(false);
  }, []);
  const finish = (commit: boolean, byKey: boolean) => {
    if (done.current) return;
    done.current = true;
    const vs = refs.current.slice(0, n).map((el) => el?.value.trim() ?? "");
    if (commit && vs.some((v, i) => v && v !== props.fields[i].value)) props.onCommit(vs, byKey);
    else props.onCancel(byKey);
  };
  const input = (i: number) => (
    <input
      ref={(el) => {
        refs.current[i] = el;
      }}
      class="inline-edit"
      aria-label={props.fields[i].label}
      defaultValue={props.fields[i].value}
      spellcheck={false}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") finish(true, true);
        else if (e.key === "Escape") finish(false, true);
        else if (e.key === "Tab" && n > 1) {
          e.preventDefault();
          focusField((i + (e.shiftKey ? n - 1 : 1)) % n);
        }
      }}
      // Moving between the fields isn't leaving.
      onBlur={(e) => !refs.current.includes(e.relatedTarget as HTMLInputElement) && finish(true, false)}
    />
  );
  return (
    <span
      ref={wrap}
      class={cx("inline-edit-fields", props.class)}
      onClick={(e) => e.stopPropagation()}
      onDblClick={(e) => e.stopPropagation()}
      onPointerDown={(e) => e.stopPropagation()}
      // A press on the gap or the separator keeps the focus where it is.
      onMouseDown={(e) => !(e.target instanceof HTMLInputElement) && e.preventDefault()}
    >
      {props.fields.map((f, i) => (
        <Fragment key={i}>
          {i > 0 && <span class="inline-edit-sep">·</span>}
          {f.tag && <span class="inline-edit-tag" aria-hidden="true" onClick={() => focusField(i)}>{f.tag}</span>}
          {input(i)}
        </Fragment>
      ))}
    </span>
  );
}

export function Spinner({ size = 12 }: { size?: number }) {
  return <span class="spinner" style={{ width: size + "px", height: size + "px" }} />;
}
