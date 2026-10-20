// Draws Board JSON (app "board", and mk.board in kit panels) with the kit's parts. The input is
// validated first (apps/board/logic.ts); every string goes through esc().
import { clamp, esc, rich, safeColor } from "./format.ts";
import { icon } from "./icons.ts";
import { meter, stat } from "./parts.ts";
import { chart } from "./chart.ts";
import { actionText, type Block, type Board, KINDS } from "../apps/board/logic.ts";
import type { Btn } from "./buttons.ts";

const tone = (t: string | undefined) => t && ["good", "warn", "bad", "info", "accent"].includes(t) ? t : "";
const head = (b: { title?: string; aside?: string }) =>
  b.title ? `<h2 class="mk-card-title">${esc(b.title)}${b.aside ? `<span class="aside">${esc(b.aside)}</span>` : ""}</h2>` : "";

function blockHtml(b: Block, i: number): string {
  switch (b.kind) {
    case "stats":
      return (b.title ? `<div class="mk-card stats-card">${head(b)}` : "") + `<div class="mk-stats">${b.items.map((s) => stat(s)).join("")}</div>` + (b.title ? "</div>" : "");
    case "chart":
      return `<div class="mk-card">${head(b)}<div class="mk-chart" data-board-chart="${i}"></div></div>`;
    case "list":
      return `<div class="mk-card">${head(b)}<ul class="mk-list">${b.items.map((it) =>
        `<li>${it.icon ? `<span class="mk-icon ${tone(it.tone)}">${icon(it.icon)}</span>` : ""}<div class="grow"><div class="t">${esc(it.title)}</div>${it.sub ? `<div class="s">${esc(it.sub)}</div>` : ""}</div>` +
        (it.bar !== undefined ? `<div class="mk-bar-cell" aria-hidden="true"><span class="b"><i style="width:${clamp(it.bar * 100, 0, 100).toFixed(1)}%;--c:${safeColor(it.color, "var(--mk-accent)")}"></i></span></div>` : "") +
        (it.badge ? `<span class="mk-badge ${tone(it.tone)}">${esc(it.badge)}</span>` : "") +
        (it.value !== undefined ? `<span class="v">${esc(it.value)}</span>` : "") + `</li>`).join("")}</ul></div>`;
    case "meters":
      return `<div class="mk-card">${head(b)}<div class="mk-stack">${b.items.map((x) => meter(x)).join("")}</div>${b.note ? `<p class="mk-note">${esc(b.note)}</p>` : ""}</div>`;
    case "callout": {
      const name = b.icon || ({ good: "check", warn: "alert", bad: "alert", info: "info" } as Record<string, string>)[b.tone ?? ""] || "info";
      return `<div class="mk-callout ${tone(b.tone)}"${b.tone === "bad" || b.tone === "warn" ? ' role="note"' : ""}>${icon(name)}<div>${b.title ? `<b>${esc(b.title)}</b> ` : ""}${esc(b.text)}</div></div>`;
    }
    case "actions":
      return (b.title ? `<div class="mk-card">${head(b)}` : "") + `<div class="mk-row">${b.items.map((a) =>
        // The action's data stays typed JSON: it is put on the button after drawing (board()).
        `<button type="button" class="mk-btn${a.primary ? " primary" : ""}${a.danger ? " danger" : ""}" data-send="${esc(a.id)}" data-text="${esc(actionText(a.label))}"` +
        (a.confirm ? ` data-confirm="${esc(a.confirm)}"` : "") + (a.key ? ` data-key="${esc(a.key.toLowerCase())}" aria-keyshortcuts="${esc(a.key.toUpperCase())}"` : "") +
        `>${a.icon ? icon(a.icon) : ""}${esc(a.label)}${a.key ? `<kbd>${esc(a.key.toUpperCase())}</kbd>` : ""}</button>`).join("")}</div>` + (b.title ? "</div>" : "");
    default:
      return `<div class="mk-error" role="alert">${icon("alert")}<span><b>Unknown block "${esc((b as { kind: string }).kind)}".</b> Use one of: ${KINDS.join(", ")}.</span></div>`;
  }
}

/** Draw a validated Board into `el` (its charts are live kit charts). */
export function board(el: HTMLElement, d: Board, warnings: string[] = []) {
  el.innerHTML = `<div class="mk-page mk-board"><header class="mk-head"><div><h1 class="mk-title">${esc(d.title)}${d.scope ? ` <span class="mk-scope">${esc(d.scope)}</span>` : ""}</h1>` +
    `${d.takeaway ? `<p class="mk-takeaway">${rich(d.takeaway)}</p>` : ""}</div></header>` +
    (warnings.length ? `<p class="mk-warnings">${esc(warnings.join("; "))}</p>` : "") +
    `<div class="mk-grid" style="--min:${clamp(d.min || 300, 160, 1200)}px">${d.blocks.map((b, i) =>
      `<div class="mk-cell${b.span === "full" ? " mk-wide" : ""}${b.kind === "actions" ? " mk-actions" : ""}">${blockHtml(b, i)}</div>`).join("")}</div>` +
    (d.source ? `<footer class="mk-foot"><span class="mk-source">Source: ${esc(d.source)}</span></footer>` : "") + `</div>`;
  el.querySelectorAll<Btn>(".mk-actions [data-send]").forEach((btn) => {
    const a = d.blocks.flatMap((b) => b.kind === "actions" ? b.items : []).find((x) => x.id === btn.dataset.send);
    btn._mkData = { ...(a?.data ?? {}) };
  });
  el.querySelectorAll<HTMLElement>("[data-board-chart]").forEach((c) => {
    const b = d.blocks[Number(c.dataset.boardChart)];
    if (b?.kind === "chart") chart(c, b.spec, { height: b.height });
  });
}
