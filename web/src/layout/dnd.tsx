// Pointer-based tab drag & drop (Obsidian-style). Dragging a tab over a pane shows where it
// will land: edges split, center joins the pane's tabs, tab strips reorder. Dropping it on the
// empty part of the tab bar moves the session into a new tab. The same drop zones serve a
// top-level tab dragged onto the panes (TabBar.tsx), which merges that tab into this one.

import { signal } from "@preact/signals";
import { useLayoutEffect, useRef, useState } from "preact/hooks";
import { type DropZone, isNoopMove, moveTab, splitFits } from "./tree.ts";
import { layouts, updateLayout } from "../state.ts";
import { focusActiveTerminalSoon, moveSessionToNewTab } from "../actions.ts";
import type { SessionInfo } from "../types.ts";
import { KindIcon } from "../icons.tsx";

export interface Target {
  /** "" for the tab bar (`newTab`). */
  paneId: string;
  zone: DropZone;
  index?: number;
  /** Overlay rect (viewport px). */
  rect: { left: number; top: number; width: number; height: number };
  /** Insertion marker for tab strips. */
  line?: { left: number; top: number; height: number };
  /** The empty part of the tab bar: the session moves into a new tab. */
  newTab?: boolean;
}

interface DragState {
  sid: number;
  ch: number;
  name: string;
  kind: SessionInfo["kind"];
  x: number;
  y: number;
  target: Target | null;
}

export const drag = signal<DragState | null>(null);

const THRESHOLD = 5;

/**
 * Call from a tab's pointerdown. Resolves to true if a drag happened (so the caller should
 * suppress click behaviour).
 */
export function startTabDrag(e: PointerEvent, ch: number, s: SessionInfo, onClick: () => void) {
  if (e.button !== 0) return;
  const sx = e.clientX;
  const sy = e.clientY;
  let started = false;
  const move = (ev: PointerEvent) => {
    if (!started) {
      if (Math.hypot(ev.clientX - sx, ev.clientY - sy) < THRESHOLD) return;
      started = true;
      document.body.classList.add("dragging");
    }
    ev.preventDefault();
    drag.value = {
      sid: s.id,
      ch,
      name: s.name,
      kind: s.kind,
      x: ev.clientX,
      y: ev.clientY,
      target: computeTarget(ev.clientX, ev.clientY, ch, s.id),
    };
  };
  const cleanup = () => {
    removeEventListener("pointermove", move, true);
    removeEventListener("pointerup", up, true);
    removeEventListener("pointercancel", cancel, true);
    removeEventListener("keydown", key, true);
    removeEventListener("blur", cancel);
    document.body.classList.remove("dragging");
  };
  const up = () => {
    cleanup();
    const d = drag.value;
    drag.value = null;
    if (!started) {
      onClick();
      return;
    }
    if (d?.target?.newTab) moveSessionToNewTab(ch, s.id);
    else if (d?.target) {
      const t = d.target;
      updateLayout(ch, (l) => moveTab(l, s.id, t.paneId, t.zone, t.index));
      focusActiveTerminalSoon();
    }
  };
  const cancel = () => {
    cleanup();
    drag.value = null;
  };
  const key = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      cancel();
    }
  };
  addEventListener("pointermove", move, true);
  addEventListener("pointerup", up, true);
  addEventListener("pointercancel", cancel, true);
  addEventListener("keydown", key, true);
  addEventListener("blur", cancel);
}

/**
 * Where a drop at (x, y) lands in tab `ch`'s panes. `sid`: the session dragged within that tab
 * (drops that change nothing give null, and the tab bar's empty part is a target); null for a
 * whole tab dragged in from the tab bar.
 */
export function computeTarget(x: number, y: number, ch: number, sid: number | null): Target | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  if (!el) return null;
  const l = layouts.value.get(ch);
  if (!l) return null;
  const noop = (paneId: string, zone: DropZone, index?: number) =>
    sid !== null && isNoopMove(l, sid, paneId, zone, index);
  if (sid !== null && (el.closest(".tb-space") || (el.closest(".tb-tabs") && !el.closest(".ttab")))) {
    const strip = document.querySelector<HTMLElement>(".tabbar .tb-tabs");
    const last = strip?.querySelector<HTMLElement>(".ttab:last-child");
    if (!strip) return null;
    const sr = strip.getBoundingClientRect();
    const lx = Math.min(sr.right - 1, last ? last.getBoundingClientRect().right : sr.left + 4);
    return {
      paneId: "",
      zone: "center",
      newTab: true,
      rect: { left: sr.left, top: sr.top, width: sr.width, height: sr.height },
      line: { left: lx - 1, top: sr.top + 5, height: sr.height - 10 },
    };
  }
  const strip = el.closest("[data-tabstrip]") as HTMLElement | null;
  if (strip) {
    const paneId = strip.dataset.tabstrip!;
    const tabs = [...strip.querySelectorAll<HTMLElement>("[data-tab-sid]")];
    let index = tabs.length;
    for (let i = 0; i < tabs.length; i++) {
      const r = tabs[i].getBoundingClientRect();
      if (x < r.left + r.width / 2) {
        index = i;
        break;
      }
    }
    const sr = strip.getBoundingClientRect();
    let lineX: number;
    if (tabs.length === 0) lineX = sr.left + 4;
    else if (index < tabs.length) lineX = tabs[index].getBoundingClientRect().left;
    else lineX = tabs[tabs.length - 1].getBoundingClientRect().right;
    if (noop(paneId, "center", index)) return null;
    return {
      paneId,
      zone: "center",
      index,
      rect: { left: sr.left, top: sr.top, width: sr.width, height: sr.height },
      line: { left: lineX - 1, top: sr.top + 4, height: sr.height - 8 },
    };
  }
  // A pane header joins that pane's sessions (like dropping on its center).
  const head = el.closest("[data-pane-head]") as HTMLElement | null;
  if (head) {
    const paneId = head.dataset.paneHead!;
    if (noop(paneId, "center")) return null;
    const r = (head.closest("[data-pane]") ?? head).getBoundingClientRect();
    return { paneId, zone: "center", rect: { left: r.left, top: r.top, width: r.width, height: r.height } };
  }
  const body = el.closest("[data-pane-body]") as HTMLElement | null;
  if (!body) return null;
  const paneId = body.dataset.paneBody!;
  const r = body.getBoundingClientRect();
  const rx = (x - r.left) / r.width;
  const ry = (y - r.top) / r.height;
  let zone: DropZone = "center";
  // Outer 28% on each side = edge zone; nearest edge wins.
  const edge = 0.28;
  const d = { left: rx, right: 1 - rx, top: ry, bottom: 1 - ry };
  const nearest = (Object.keys(d) as (keyof typeof d)[]).reduce((a, b) => (d[a] < d[b] ? a : b));
  if (d[nearest] < edge) zone = nearest;
  // No room for another split there: joining the pane's tabs is the only option.
  if (zone !== "center") {
    const box = document.querySelector(".panes")?.getBoundingClientRect();
    if (box && !splitFits(l.root, paneId, zone, { w: box.width, h: box.height })) zone = "center";
  }
  if (noop(paneId, zone)) return null;
  let rect = { left: r.left, top: r.top, width: r.width, height: r.height };
  if (zone === "left") rect = { ...rect, width: r.width / 2 };
  if (zone === "right") rect = { ...rect, left: r.left + r.width / 2, width: r.width / 2 };
  if (zone === "top") rect = { ...rect, height: r.height / 2 };
  if (zone === "bottom") rect = { ...rect, top: r.top + r.height / 2, height: r.height / 2 };
  return { paneId, zone, rect };
}

export function DragOverlay() {
  const d = drag.value;
  if (!d) return null;
  return (
    <>
      <DropMarker t={d.target} />
      <Ghost x={d.x} y={d.y} kind={d.kind} name={d.name} />
    </>
  );
}

/** The drop-zone overlay of a target (half a pane, a whole pane) or its insertion line. */
export function DropMarker({ t }: { t: Target | null }) {
  return (
    <>
      {t && !t.line && (
        <div
          class={"drop-zone zone-" + t.zone}
          style={{
            left: t.rect.left + 4 + "px",
            top: t.rect.top + 4 + "px",
            width: t.rect.width - 8 + "px",
            height: t.rect.height - 8 + "px",
          }}
        />
      )}
      {t?.line && (
        <div
          class="drop-line"
          style={{ left: t.line.left + "px", top: t.line.top + "px", height: t.line.height + "px" }}
        />
      )}
    </>
  );
}

/** The dragged tab's label, following the pointer but kept inside the window. */
function Ghost({ x, y, kind, name }: { x: number; y: number; kind: SessionInfo["kind"]; name: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 160, h: 26 });
  useLayoutEffect(() => {
    const r = ref.current?.getBoundingClientRect();
    if (r && (r.width !== size.w || r.height !== size.h)) setSize({ w: r.width, h: r.height });
  }, [name]);
  let left = x + 12;
  let top = y + 10;
  if (left + size.w > innerWidth - 4) left = x - size.w - 8;
  if (top + size.h > innerHeight - 4) top = y - size.h - 6;
  left = Math.max(4, Math.min(left, innerWidth - size.w - 4));
  top = Math.max(4, Math.min(top, innerHeight - size.h - 4));
  return (
    <div ref={ref} class="drag-ghost" style={{ left: left + "px", top: top + "px" }}>
      <KindIcon kind={kind} size={13} />
      <span>{name}</span>
    </div>
  );
}
