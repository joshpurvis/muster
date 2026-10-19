// Pointer-based tab drag & drop. Dragging a tab over a group shows where it will land: the body's
// edges split that group, its center joins the group's tabs, a strip inserts at the marker. The
// "→ Window N / → New window" targets in the focused strip move it to another window.

import { signal } from "@preact/signals";
import { useLayoutEffect, useRef, useState } from "preact/hooks";
import { type DropZone, isNoopMove, splitFits } from "./tree.ts";
import { layout } from "../state.ts";
import { dropTab, moveTabToWindow, revealSession } from "../actions.ts";
import type { SessionInfo } from "../types.ts";
import { IconPanel, KindIcon } from "../icons.tsx";

export interface Target {
  paneId: string;
  zone: DropZone;
  index?: number;
  /** Overlay rect (viewport px). */
  rect: { left: number; top: number; width: number; height: number };
  /** Insertion marker for tab strips. */
  line?: { left: number; top: number; height: number };
}

interface DragState {
  sid: number;
  label: string;
  kind: SessionInfo["kind"] | "panel";
  x: number;
  y: number;
  target: Target | null;
  /** Over a "→ Window N" (id) or "→ New window" target. */
  win: number | "new" | null;
}

export const drag = signal<DragState | null>(null);

const THRESHOLD = 5;

/** Call from a tab's pointerdown. A press without movement is a click (`onClick`). */
export function startTabDrag(e: PointerEvent, s: { id: number; kind: DragState["kind"] }, label: string, onClick: () => void) {
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
    const win = windowTarget(ev.clientX, ev.clientY);
    drag.value = {
      sid: s.id,
      label,
      kind: s.kind,
      x: ev.clientX,
      y: ev.clientY,
      win,
      target: win !== null ? null : computeTarget(ev.clientX, ev.clientY, s.id),
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
    if (d?.win != null) moveTabToWindow(s.id, d.win);
    else if (d?.target) {
      const t = d.target;
      dropTab(s.id, t.paneId, t.zone, t.index);
      revealSession(s.id);
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

function windowTarget(x: number, y: number): number | "new" | null {
  const el = (document.elementFromPoint(x, y) as HTMLElement | null)?.closest("[data-move-to]") as
    | HTMLElement
    | null;
  if (!el) return null;
  const v = el.dataset.moveTo!;
  return v === "new" ? "new" : Number(v);
}

/** Where a drop of tab `sid` at (x, y) lands among the groups (null: nowhere, or no change). */
export function computeTarget(x: number, y: number, sid: number): Target | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  const l = layout.value;
  if (!el || !l) return null;
  const noop = (paneId: string, zone: DropZone, index?: number) => isNoopMove(l, sid, paneId, zone, index);
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
    if (noop(paneId, "center", index)) return null;
    const box = (strip.querySelector(".tabs") ?? strip).getBoundingClientRect();
    const sr = strip.getBoundingClientRect();
    let lineX: number;
    if (tabs.length === 0) lineX = box.left + 4;
    else if (index < tabs.length) lineX = tabs[index].getBoundingClientRect().left;
    else lineX = tabs[tabs.length - 1].getBoundingClientRect().right;
    lineX = Math.max(box.left + 1, Math.min(box.right - 1, lineX));
    return {
      paneId,
      zone: "center",
      index,
      rect: { left: sr.left, top: sr.top, width: sr.width, height: sr.height },
      line: { left: lineX - 1, top: sr.top + 4, height: sr.height - 8 },
    };
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
  // No room for another split there: joining the group's tabs is the only option.
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
      <Ghost x={d.x} y={d.y} kind={d.kind} label={d.label} />
    </>
  );
}

/** The drop-zone overlay of a target (half a group, a whole group) or its insertion line. */
function DropMarker({ t }: { t: Target | null }) {
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
function Ghost({ x, y, kind, label }: { x: number; y: number; kind: DragState["kind"]; label: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 160, h: 26 });
  useLayoutEffect(() => {
    const r = ref.current?.getBoundingClientRect();
    if (r && (r.width !== size.w || r.height !== size.h)) setSize({ w: r.width, h: r.height });
  }, [label]);
  let left = x + 12;
  let top = y + 10;
  if (left + size.w > innerWidth - 4) left = x - size.w - 8;
  if (top + size.h > innerHeight - 4) top = y - size.h - 6;
  left = Math.max(4, Math.min(left, innerWidth - size.w - 4));
  top = Math.max(4, Math.min(top, innerHeight - size.h - 4));
  return (
    <div ref={ref} class="drag-ghost" style={{ left: left + "px", top: top + "px" }}>
      {kind === "panel" ? <IconPanel size={13} /> : <KindIcon kind={kind} size={13} />}
      <span>{label}</span>
    </div>
  );
}
