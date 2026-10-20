// A picture viewer for the built-in apps (the image app, pr_decision's enlarged picture): starts
// at Fit, zooms in and out from the size on screen (+ and −, Ctrl+wheel or a pinch around the
// pointer), 100% is actual pixels, and a picture bigger than its stage scrolls and pans by drag.
// The zoom maths is in zoom.ts.
import { h } from "./dom.ts";
import { typing } from "./client.ts";
import { iconEl } from "../../kit/icons.ts";
import { anchoredScroll, clampZoom, fitScale, stepZoom, svgFromDataUri, svgSize } from "./zoom.ts";

/** What a viewer keeps across re-renders of its app: Fit or a scale, and where it was scrolled. */
export interface ViewState { fit: boolean; scale: number; left: number; top: number }
export const freshView = (): ViewState => ({ fit: true, scale: 1, left: 0, top: 0 });

export interface ViewerIds { zoomIn: string; zoomOut: string; fit: string; actual: string }

export interface Viewer {
  /** The scrolling stage, holding the picture. */
  stage: HTMLElement;
  /** − + Fit 100%, as one segmented group. */
  controls: HTMLElement;
  /** The scale on screen, as "43%" (empty until the picture has loaded). */
  readout: HTMLElement;
  /** Handle a zoom key (+ = − 0 1 Z, also with Ctrl so the browser does not zoom the page). */
  key(e: KeyboardEvent): boolean;
  /** The picture is bigger than the stage, so arrows and drags move it. */
  overflowing(): boolean;
  destroy(): void;
}

const near = (a: number, b: number) => Math.abs(a - b) < 0.0005;

export function viewer(img: HTMLImageElement, state: ViewState, opts: { stageClass: string; label: string; ids: ViewerIds; onChange?: () => void }): Viewer {
  const stage = h("div", { class: opts.stageClass, tabindex: "0", "aria-label": opts.label }, img);
  img.draggable = false;
  // The visible readout, and a polite live region that says the zoom once it settles (not on
  // every wheel step).
  const readout = h("span", { class: "zoom-readout", "aria-hidden": "true" });
  const live = h("span", { class: "mk-sr", role: "status", "aria-live": "polite" });
  let liveTimer = 0;
  const announce = () => {
    clearTimeout(liveTimer);
    liveTimer = setTimeout(() => { live.textContent = state.fit ? `Fit, ${readout.textContent}` : `Zoom ${readout.textContent}`; }, 400);
  };
  const btn = (id: string, label: string, keys: string, content: (Node | string)[], run: () => void) =>
    h("button", { type: "button", id, title: `${label} (${keys})`, "aria-label": label, "aria-keyshortcuts": keys, onclick: run }, ...content);
  const outBtn = btn(opts.ids.zoomOut, "Zoom out", "-", [iconEl("minus")], () => zoom(stepZoom(scale(), -1, fitNow())));
  const inBtn = btn(opts.ids.zoomIn, "Zoom in", "+", [iconEl("plus")], () => zoom(stepZoom(scale(), 1, fitNow())));
  const fitBtn = btn(opts.ids.fit, "Fit", "0", ["Fit"], () => fit());
  const actualBtn = btn(opts.ids.actual, "Actual size", "1", ["100%"], () => zoom(1));
  const controls = h("div", { class: "mk-seg zoom-tools", role: "group", "aria-label": "Zoom" }, outBtn, inBtn, fitBtn, actualBtn, live);

  // The picture's own size in CSS px, and whether fitting may enlarge it (a vector with no size
  // of its own, which a browser would stretch to its box anyway).
  let natural: { w: number; h: number; upscale: boolean } | null = null;
  const measure = () => {
    if (!img.naturalWidth && !img.naturalHeight) return null;
    const text = svgFromDataUri(img.currentSrc || img.src);
    const s = text ? svgSize(text) : null;
    if (s) return { w: s.w, h: s.h, upscale: !s.sized };
    const svgUrl = /\.svg(\?|#|$)/i.test(img.src) || img.src.startsWith("data:image/svg");
    return { w: img.naturalWidth || 300, h: img.naturalHeight || 150, upscale: svgUrl };
  };
  const box = () => {
    const cs = getComputedStyle(stage);
    return [stage.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight),
      stage.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom)];
  };
  const fitNow = () => natural ? fitScale(natural.w, natural.h, ...box() as [number, number], natural.upscale) : 1;
  const scale = () => state.fit ? fitNow() : state.scale;
  const overflowing = () => stage.scrollWidth > stage.clientWidth + 1 || stage.scrollHeight > stage.clientHeight + 1;

  const paint = () => {
    if (!natural) return;
    const s = scale();
    img.style.width = `${natural.w * s}px`;
    img.style.height = `${natural.h * s}px`;
    const over = overflowing();
    stage.classList.add("sized");
    stage.classList.toggle("fit", state.fit);
    stage.classList.toggle("actual", !state.fit && near(s, 1));
    stage.classList.toggle("zoomed", !state.fit && !near(s, 1));
    stage.classList.toggle("can-pan", over);
    stage.dataset.scale = s.toFixed(4);
    readout.textContent = `${Math.round(s * 100)}%`;
    fitBtn.setAttribute("aria-pressed", String(state.fit));
    actualBtn.setAttribute("aria-pressed", String(near(s, 1)));
    const [lo, hi] = [clampZoom(0, fitNow()), clampZoom(Infinity, fitNow())];
    // aria-disabled, not disabled: a focused button that became disabled would drop the focus.
    outBtn.setAttribute("aria-disabled", String(s <= lo * 1.001));
    inBtn.setAttribute("aria-disabled", String(s >= hi / 1.001));
  };

  /** Zoom to `next`, keeping the picture point under `anchor` (stage px; default its centre) still. */
  const zoom = (next: number, anchor?: { x: number; y: number }) => {
    if (!natural) return;
    const s0 = scale(), s1 = clampZoom(next, fitNow());
    const ax = anchor?.x ?? stage.clientWidth / 2, ay = anchor?.y ?? stage.clientHeight / 2;
    const [sl, st, ox, oy] = [stage.scrollLeft, stage.scrollTop, img.offsetLeft, img.offsetTop];
    state.fit = false;
    state.scale = s1;
    paint();
    stage.scrollLeft = anchoredScroll(sl, ax, ox, img.offsetLeft, s0, s1);
    stage.scrollTop = anchoredScroll(st, ay, oy, img.offsetTop, s0, s1);
    paint(); // scrolling can change what overflows (scrollbars)
    announce();
    opts.onChange?.();
  };
  const fit = () => {
    state.fit = true;
    paint();
    announce();
    opts.onChange?.();
  };
  /** Z and a click: Fit ↔ actual size (or twice the fitted size when that is actual size already). */
  const toggle = (anchor?: { x: number; y: number }) => {
    if (!state.fit) return fit();
    const f = fitNow();
    zoom(near(f, 1) || f > 1 ? f * 2 : 1, anchor);
  };
  const at = (e: { clientX: number; clientY: number }) => {
    const r = stage.getBoundingClientRect();
    return { x: e.clientX - r.left - stage.clientLeft, y: e.clientY - r.top - stage.clientTop };
  };

  img.addEventListener("load", () => {
    natural = measure();
    paint();
    if (!state.fit) {
      stage.scrollLeft = state.left;
      stage.scrollTop = state.top;
    }
  });
  stage.addEventListener("scroll", () => {
    state.left = stage.scrollLeft;
    state.top = stage.scrollTop;
  }, { passive: true });
  // Ctrl+wheel (and a touchpad pinch, which arrives as one) zooms around the pointer, never the
  // whole page; a plain wheel scrolls.
  stage.addEventListener("wheel", (e) => {
    if (!(e.ctrlKey || e.metaKey) || !natural) return;
    e.preventDefault();
    const rate = e.deltaMode === 1 ? 0.05 : e.deltaMode === 2 ? 1 : 0.0025;
    zoom(scale() * Math.exp(-e.deltaY * rate), at(e));
  }, { passive: false });
  // Drag pans a picture bigger than the stage; a click without a drag toggles the size.
  let drag: { x: number; y: number; id: number; moved: boolean; onImg: boolean } | null = null;
  stage.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || controls.contains(e.target as Node)) return;
    drag = { x: e.clientX, y: e.clientY, id: e.pointerId, moved: false, onImg: e.target === img };
  });
  stage.addEventListener("pointermove", (e) => {
    if (!drag || e.pointerId !== drag.id || !(e.buttons & 1)) return;
    const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
    if (!drag.moved && Math.abs(dx) + Math.abs(dy) <= 3) return;
    if (!drag.moved) {
      drag.moved = true;
      if (overflowing()) {
        stage.setPointerCapture(e.pointerId);
        stage.classList.add("panning");
      }
    }
    if (stage.classList.contains("panning")) stage.scrollBy(-dx, -dy);
    drag.x = e.clientX;
    drag.y = e.clientY;
  });
  const end = (e: PointerEvent) => {
    if (!drag || e.pointerId !== drag.id) return;
    const click = !drag.moved && drag.onImg && e.type === "pointerup";
    drag = null;
    stage.classList.remove("panning");
    if (click) toggle(at(e));
  };
  stage.addEventListener("pointerup", end);
  stage.addEventListener("pointercancel", end);
  const resize = new ResizeObserver(() => paint());
  resize.observe(stage);

  return {
    stage, controls, readout, overflowing,
    key(e) {
      if (typing(e) || e.altKey || !natural) return false;
      const mod = e.ctrlKey || e.metaKey;
      const k = e.key;
      if (k === "+" || k === "=") zoom(stepZoom(scale(), 1, fitNow()));
      else if (k === "-" || k === "_") zoom(stepZoom(scale(), -1, fitNow()));
      else if (k === "0") fit();
      else if (k === "1" && !mod) zoom(1);
      else if ((k === "z" || k === "Z") && !mod) toggle();
      else return false;
      e.preventDefault();
      return true;
    },
    destroy() { resize.disconnect(); clearTimeout(liveTimer); },
  };
}
