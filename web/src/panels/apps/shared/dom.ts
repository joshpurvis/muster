// Tiny DOM builders. Text always goes in as text nodes, never as HTML.

type Child = Node | string | number | null | undefined | false | Child[];
type Attrs = Record<string, string | number | boolean | null | undefined | EventListener>;

function fill(el: Element, attrs: Attrs | null, children: Child[]) {
  for (const [k, v] of Object.entries(attrs ?? {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  const add = (c: Child) => {
    if (c === null || c === undefined || c === false) return;
    if (Array.isArray(c)) c.forEach(add);
    else el.append(typeof c === "string" || typeof c === "number" ? document.createTextNode(String(c)) : c);
  };
  children.forEach(add);
  return el;
}

export type { Child };

export function h<K extends keyof HTMLElementTagNameMap>(tag: K, attrs?: Attrs | null, ...children: Child[]): HTMLElementTagNameMap[K] {
  return fill(document.createElement(tag), attrs ?? null, children) as HTMLElementTagNameMap[K];
}

export function svg(tag: string, attrs?: Attrs | null, ...children: Child[]): SVGElement {
  return fill(document.createElementNS("http://www.w3.org/2000/svg", tag), attrs ?? null, children) as SVGElement;
}

/** Replace `root`'s content; null and false children are skipped. */
export function mount(root: Element, ...children: Child[]) {
  root.replaceChildren();
  fill(root, null, children);
}

/** Show input problems in place of the app, as a readable list. */
export function problems(root: Element, title: string, errors: string[]) {
  root.replaceChildren(
    h("div", { class: "error", role: "alert" },
      h("strong", null, title),
      h("ul", null, errors.slice(0, 20).map((e) => h("li", null, e))),
      errors.length > 20 ? h("p", null, `…and ${errors.length - 20} more`) : null),
  );
}

/** Scroll `el` into view above a sticky bottom bar (scrollIntoView alone can leave it under it). */
export function reveal(el: Element | null, bar: Element | null) {
  if (!el) return;
  const r = el.getBoundingClientRect();
  const bottom = innerHeight - (bar ? bar.getBoundingClientRect().height : 0) - 8;
  if (r.top < 8) scrollBy(0, r.top - 8);
  else if (r.bottom > bottom) scrollBy(0, Math.min(r.bottom - bottom, r.top - 8));
}
