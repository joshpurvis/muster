// Pointer-based tab drag & drop (Obsidian-style). Dragging a tab over a pane shows where it
// will land: edges split, center joins the pane's tabs, tab strips reorder.

import { signal } from "@preact/signals";
import { type DropZone, moveTab } from "./tree.ts";
import { updateLayout } from "../state.ts";
import { focusActiveTerminalSoon } from "../actions.ts";
import type { SessionInfo } from "../types.ts";
import { KindIcon } from "../icons.tsx";

interface Target {
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
      target: computeTarget(ev.clientX, ev.clientY, s.id),
    };
  };
  const cleanup = () => {
    removeEventListener("pointermove", move, true);
    removeEventListener("pointerup", up, true);
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
    if (d?.target) {
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
  addEventListener("keydown", key, true);
  addEventListener("blur", cancel);
}

function computeTarget(x: number, y: number, sid: number): Target | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  if (!el) return null;
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
    // Dropping a tab right next to itself is a no-op; still show the marker.
    void sid;
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
  const t = d.target;
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
      <div class="drag-ghost" style={{ left: d.x + 12 + "px", top: d.y + 10 + "px" }}>
        <KindIcon kind={d.kind} size={13} />
        <span>{d.name}</span>
      </div>
    </>
  );
}
