// Zoom maths for the picture viewers (the image app and pr_decision's enlarged picture). A scale
// is displayed CSS px per natural px: 1 is actual size. Pure, so it is unit tested.

/** The steps + and − move between, relative to the scale on screen now. */
export const ZOOM_STEPS = [0.05, 0.1, 0.15, 0.2, 0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1, 1.25, 1.5, 2, 3, 4, 6, 8, 12, 16];

/** Smallest and largest scale for a picture whose fitted scale is `fit` (both always reachable). */
export function zoomLimits(fit: number): [number, number] {
  return [Math.min(ZOOM_STEPS[0], fit), Math.max(ZOOM_STEPS[ZOOM_STEPS.length - 1], fit)];
}

export function clampZoom(scale: number, fit: number): number {
  const [lo, hi] = zoomLimits(fit);
  return Math.min(hi, Math.max(lo, scale));
}

/** The next step above (`dir` 1) or below (-1) `scale`. The fitted scale counts as a step, so
 * zooming out from a close-up stops at Fit on the way. */
export function stepZoom(scale: number, dir: 1 | -1, fit: number): number {
  const steps = [...new Set([...ZOOM_STEPS, fit])].sort((a, b) => a - b);
  const next = dir > 0 ? steps.find((s) => s > scale * 1.001) : [...steps].reverse().find((s) => s < scale / 1.001);
  return clampZoom(next ?? scale, fit);
}

/** Scale that fits `w`×`h` inside the box. Raster pictures are never enlarged past actual size
 * by fitting (that blurs them); `upscale` (a vector without a size of its own) may fill the box. */
export function fitScale(w: number, h: number, boxW: number, boxH: number, upscale = false): number {
  if (!(w > 0 && h > 0 && boxW > 0 && boxH > 0)) return 1;
  const s = Math.min(boxW / w, boxH / h);
  return upscale ? s : Math.min(1, s);
}

/** Scroll offset along one axis that keeps the picture point under `anchor` (px from the
 * stage's visible edge) where it was: `offOld`/`offNew` are the picture's offset in the stage's
 * content before and after the change (it is centred while smaller than the stage). */
export function anchoredScroll(scroll: number, anchor: number, offOld: number, offNew: number, s0: number, s1: number): number {
  const at = (scroll + anchor - offOld) / s0; // natural px under the anchor
  return Math.max(0, offNew + at * s1 - anchor);
}

const UNIT: Record<string, number> = { "": 1, px: 1, pt: 4 / 3, pc: 16, in: 96, cm: 96 / 2.54, mm: 96 / 25.4 };

function length(v: string | undefined): number | null {
  const m = v ? /^\s*([0-9]*\.?[0-9]+)\s*(px|pt|pc|in|cm|mm)?\s*$/i.exec(v) : null;
  if (!m) return null;
  const n = Number(m[1]) * UNIT[(m[2] ?? "").toLowerCase()];
  return n > 0 ? n : null;
}

/** An SVG's own size from its root tag: `width`/`height` in absolute units, else its viewBox
 * (user units are CSS px). `sized` is false when it has no absolute size, so a browser would
 * stretch it to its box; null when the text has no readable <svg> tag. */
export function svgSize(text: string): { w: number; h: number; sized: boolean } | null {
  const tag = /<svg\b[^>]*>/i.exec(text)?.[0];
  if (!tag) return null;
  const attr = (name: string) => new RegExp(`\\s${name}\\s*=\\s*("([^"]*)"|'([^']*)')`, "i").exec(tag)?.slice(2).find((x) => x !== undefined);
  const w = length(attr("width")), h = length(attr("height"));
  const vb = attr("viewBox")?.trim().split(/[\s,]+/).map(Number);
  const vw = vb && vb.length === 4 && vb[2] > 0 ? vb[2] : null, vh = vb && vb.length === 4 && vb[3] > 0 ? vb[3] : null;
  if (w && h) return { w, h, sized: true };
  if (w && vw && vh) return { w, h: (w * vh) / vw, sized: true };
  if (h && vw && vh) return { w: (h * vw) / vh, h, sized: true };
  if (vw && vh) return { w: vw, h: vh, sized: false };
  return null;
}

/** The SVG text inside a `data:image/svg+xml` URI, or null for anything else. */
export function svgFromDataUri(src: string): string | null {
  const m = /^data:image\/svg\+xml(;[^,]*)?,(.*)$/s.exec(src);
  if (!m) return null;
  try {
    return (m[1] ?? "").includes(";base64") ? new TextDecoder().decode(Uint8Array.from(atob(m[2]), (c) => c.charCodeAt(0))) : decodeURIComponent(m[2]);
  } catch {
    return null;
  }
}
