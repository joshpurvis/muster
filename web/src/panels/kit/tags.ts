// <mk-*> tags in light DOM (no customElements registry, no shadow DOM), upgraded by a query so
// kit.css applies to them. One bad tag never stops the rest: it shows what went wrong in its
// place and in the console.
import { esc } from "./format.ts";
import { icon } from "./icons.ts";
import { chart } from "./chart.ts";
import { meter, ring, spark, stat } from "./parts.ts";

function json(el: Element): any {
  const s = el.querySelector('script[type="application/json"]');
  return s ? JSON.parse(s.textContent ?? "") : JSON.parse(el.getAttribute("spec") || "{}");
}
const nums = (s: string | null) => String(s || "").split(",").map((x) => x.trim()).filter(Boolean).map(Number).filter(Number.isFinite);
const num = (s: string | null) => s === null || s === "" ? undefined : Number.isFinite(Number(s)) ? Number(s) : undefined;

export const TAGS: Record<string, (el: HTMLElement) => void> = {
  "mk-chart": (el) => {
    const spec = json(el);
    if (!spec || typeof spec !== "object" || !spec.x || !Array.isArray(spec.series)) throw new Error("the JSON needs x.values and series (the chart app's spec)");
    chart(el, spec, { pin: num(el.getAttribute("pin")), height: num(el.getAttribute("height")), legend: el.getAttribute("legend") === "false" ? false : el.getAttribute("legend") === "true" ? true : undefined });
  },
  "mk-spark": (el) => {
    el.innerHTML = spark(nums(el.getAttribute("values")), { width: num(el.getAttribute("width")) || 120, height: num(el.getAttribute("height")) || 30, color: el.getAttribute("color") || undefined });
  },
  "mk-stat": (el) => {
    const a = (k: string) => el.getAttribute(k) ?? undefined;
    const t = document.createElement("template");
    t.innerHTML = stat({ label: a("label") ?? "", value: a("value") ?? "", unit: a("unit"), delta: a("delta"), tone: a("tone"), note: a("note"),
      trend: el.hasAttribute("trend") ? nums(a("trend") ?? "") : undefined, color: a("color"), trendWidth: num(a("trend-width") ?? null) });
    el.classList.add("mk-stat");
    el.replaceChildren(...t.content.firstElementChild!.childNodes);
  },
  "mk-ring": (el) => {
    const s = json(el);
    if (!Array.isArray(s.segments)) throw new Error("the JSON needs segments: [{label, value}]");
    el.innerHTML = ring(s.segments, s);
  },
  "mk-meter": (el) => {
    const a = (k: string) => el.getAttribute(k);
    el.innerHTML = meter({ label: a("label") ?? "", value: num(a("value")) ?? 0, max: num(a("max")) || 100, text: a("text") || undefined, of: a("of") || undefined,
      mark: num(a("mark")), markLabel: a("mark-label") || undefined, color: a("color") || undefined, tone: a("tone") || undefined });
  },
  "mk-icon": (el) => { el.innerHTML = icon(el.getAttribute("name") ?? ""); },
};

export function upgrade(root: ParentNode = document) {
  for (const [tag, draw] of Object.entries(TAGS)) {
    root.querySelectorAll<HTMLElement>(`${tag}:not([data-ok])`).forEach((el) => {
      el.dataset.ok = "1";
      try { draw(el); } catch (err) {
        console.error(`[mk] <${tag}> could not be drawn:`, err);
        el.innerHTML = `<div class="mk-error" role="alert">${icon("alert")}<span><b>&lt;${tag}&gt; could not be drawn.</b> ${esc((err as Error)?.message ?? err)}</span></div>`;
      }
    });
  }
}
