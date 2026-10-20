// Where a panel draws: every panel of a window lives in one layer (PanelLayer), positioned over
// its tab's slot in the layout (PanelSlot). Moving a tab between groups, zooming or splitting
// only moves the slot: the panel's frame stays in the document, so it keeps its state (a
// removed or re-parented iframe reloads). Shared by app panels and web previews.
import { useEffect, useLayoutEffect, useRef } from "preact/hooks";
import { signal } from "@preact/signals";
import { layout, panelOrigin, shownAlone } from "../state.ts";
import { focusActiveTerminalSoon } from "../actions.ts";
import { paneOfSession, panelTab } from "../layout/tree.ts";

/** Bumped per panel by "Reload panel"; each panel view reloads its frame when its count changes. */
export const panelReloads = signal<ReadonlyMap<number, number>>(new Map());
export function reloadPanel(id: number) {
  const next = new Map(panelReloads.value);
  next.set(id, (next.get(id) ?? 0) + 1);
  panelReloads.value = next;
}

export const slots = new Map<number, HTMLElement>();
export const slotsChanged = signal(0);

function clip(r: DOMRect, by: DOMRect) {
  const left = Math.max(r.left, by.left), top = Math.max(r.top, by.top);
  return new DOMRect(left, top, Math.min(r.right, by.right) - left, Math.min(r.bottom, by.bottom) - top);
}

/** The panel server's origin, or "" when the backend has none or it is not a separate
 * loopback origin (fail closed). */
export function panelOriginUrl(): string {
  try {
    const url = new URL(panelOrigin.value);
    if (url.protocol === "http:" && url.hostname === "127.0.0.1" && url.origin !== location.origin && url.port && url.port !== "0" && url.origin === panelOrigin.value) return url.origin;
  } catch { /* Older backends have no panel origin. */ }
  return "";
}

/** Keep panel `id`'s view over its slot. `frame` is its iframe: focus inside a frame being
 * hidden would fall to <body>, so it goes to the tab that took its place. */
export function usePanelPlacement(id: number, frame: { current: HTMLIFrameElement | null }) {
  const ref = useRef<HTMLDivElement>(null);
  const visible = useRef(false);
  const tab = panelTab(id);
  const l = layout.value;
  const pane = l ? paneOfSession(l.root, tab) : null;
  const paneId = useRef<string | null>(null);
  paneId.current = pane?.id ?? null;
  // A zoomed or fullscreen group is shown alone.
  const alone = shownAlone.value;
  const active = !!l && !!pane && pane.active === tab && (!alone || alone === pane.id);
  const zoomed = !!pane && alone === pane.id;
  const split = !!l && l.root.type !== "pane" && !alone;
  const dim = split && l.focus !== pane?.id;
  const activeRef = useRef(active);
  activeRef.current = active;
  void slotsChanged.value;
  const slot = slots.get(id);

  const place = () => {
    const host = ref.current;
    const layer = host?.parentElement;
    if (!host || !layer) return;
    const el = slots.get(id);
    let r: DOMRect | null = el?.isConnected && activeRef.current ? el.getBoundingClientRect() : null;
    // Split children clip panes squeezed below their floor; clip the panel the same way.
    for (let a = el?.parentElement; r && a && a !== layer.parentElement; a = a.parentElement) {
      if (a.classList.contains("split-child") || a.classList.contains("split-root")) r = clip(r, a.getBoundingClientRect());
    }
    const show = !!r && r.width > 0 && r.height > 0;
    if (show) {
      const base = layer.getBoundingClientRect();
      host.style.left = `${r!.left - base.left}px`;
      host.style.top = `${r!.top - base.top}px`;
      host.style.width = `${r!.width}px`;
      host.style.height = `${r!.height}px`;
      host.dataset.placed = "";
    } else if (host.dataset.placed !== undefined) {
      const focused = !!frame.current && document.activeElement === frame.current;
      delete host.dataset.placed;
      if (focused) focusActiveTerminalSoon();
    }
    visible.current = show;
  };

  // Layout changes reach every group in the same render pass; measure once they are all in.
  useLayoutEffect(() => { queueMicrotask(place); });
  useEffect(() => {
    const resize = new ResizeObserver(() => place());
    if (slot) resize.observe(slot);
    if (ref.current?.parentElement) resize.observe(ref.current.parentElement);
    addEventListener("resize", place);
    return () => {
      resize.disconnect();
      removeEventListener("resize", place);
    };
  }, [slot]);

  return { ref, visible, paneId, tab, active, zoomed, split, dim };
}
