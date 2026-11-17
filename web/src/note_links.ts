// Which link is under the pointer in a note's editor (docs/UI.md "Notes"). The editor is a plain
// <textarea>, which can't say where a character is on screen, so a hidden copy of it, laid out the
// same way with each link in a span of its own, says where the links are. The copy is made only
// when the pointer moves over a note that holds a link, and kept until the text, width or font
// change.
import { findTextLinks, type TextLink } from "./links.ts";

/** Styles that decide where text wraps and how wide each character is. */
const LAYOUT = [
  "font-family",
  "font-size",
  "font-weight",
  "font-style",
  "font-stretch",
  "font-variant",
  "font-feature-settings",
  "font-variation-settings",
  "font-kerning",
  "line-height",
  "letter-spacing",
  "word-spacing",
  "tab-size",
  "text-transform",
  "text-indent",
  "text-rendering",
  "white-space",
  "overflow-wrap",
  "word-break",
  "hyphens",
  "direction",
  "padding-top",
  "padding-right",
  "padding-bottom",
  "padding-left",
];

/** A link under the pointer, and its boxes (one per line it spans) relative to the textarea's
 * padding box as it scrolls now. */
export interface LinkHit {
  link: TextLink;
  boxes: { x: number; y: number; w: number; h: number }[];
}

export class NoteLinks {
  #el: HTMLTextAreaElement;
  #copy: HTMLDivElement | null = null;
  #built = false;
  #key = "";
  #text = "";
  #links: TextLink[] = [];
  #spans: HTMLSpanElement[] = [];

  constructor(el: HTMLTextAreaElement) {
    this.#el = el;
  }

  /** The link at viewport point (`x`, `y`), or null: outside the text area, on its scroll bar, or
   * not on a link's characters (beside a link at a line's end is not on it). */
  at(x: number, y: number): LinkHit | null {
    const el = this.#el;
    if (!el.isConnected || !el.value.includes("://")) return null;
    const r = el.getBoundingClientRect();
    const px = x - r.left - el.clientLeft, py = y - r.top - el.clientTop;
    if (px < 0 || py < 0 || px >= el.clientWidth || py >= el.clientHeight) return null;
    if (!this.#build()) return null;
    const origin = this.#copy!.getBoundingClientRect();
    const cx = px + el.scrollLeft, cy = py + el.scrollTop;
    for (let i = 0; i < this.#spans.length; i++) {
      const rects = [...this.#spans[i].getClientRects()];
      const boxes = rects.map((b) => ({
        x: b.left - origin.left,
        y: b.top - origin.top,
        w: b.width,
        h: b.height,
      }));
      if (boxes.some((b) => cx >= b.x && cx < b.x + b.w && cy >= b.y && cy < b.y + b.h)) {
        return {
          link: this.#links[i],
          boxes: boxes.map((b) => ({ ...b, x: b.x - el.scrollLeft, y: b.y - el.scrollTop })),
        };
      }
    }
    return null;
  }

  /** Lay the copy out again if the text, width or font changed. False when it holds no link. */
  #build(): boolean {
    const el = this.#el;
    const cs = getComputedStyle(el);
    // Not `cs.font`: Chromium gives "" for it here (a part the shorthand can't say).
    const key = LAYOUT.map((p) => cs.getPropertyValue(p)).join("|") + `|${el.clientWidth}`;
    const text = el.value;
    if (this.#built && key === this.#key && text === this.#text) return this.#links.length > 0;
    this.#built = true;
    this.#key = key;
    this.#text = text;
    this.#links = findTextLinks(text);
    this.#spans = [];
    if (!this.#links.length) {
      this.#copy?.remove();
      this.#copy = null;
      return false;
    }
    const copy = this.#copy ?? document.createElement("div");
    copy.setAttribute("aria-hidden", "true");
    const style = LAYOUT.map((p) => `${p}:${cs.getPropertyValue(p)}`).join(";");
    copy.style.cssText = `${style};position:fixed;left:0;top:0;visibility:hidden;pointer-events:none;` +
      `box-sizing:border-box;border:0;margin:0;width:${el.clientWidth}px;contain:layout style`;
    copy.replaceChildren();
    let at = 0;
    for (const l of this.#links) {
      if (l.start > at) copy.append(text.slice(at, l.start));
      const span = document.createElement("span");
      span.textContent = text.slice(l.start, l.end);
      copy.append(span);
      this.#spans.push(span);
      at = l.end;
    }
    copy.append(text.slice(at) + "\n");
    if (!this.#copy) {
      document.body.append(copy);
      this.#copy = copy;
    }
    return true;
  }

  /** Remove the copy: the editor is going. */
  dispose() {
    this.#copy?.remove();
    this.#copy = null;
    this.#built = false;
  }
}
