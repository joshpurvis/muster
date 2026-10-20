// Kit buttons that talk to the agent, with no script from the agent:
//   data-send="kind" (+ any data-*)  sends the panel event `kind` with the other data attributes
//   data-confirm="Question?"         first click (or Enter) asks; a second click or Enter sends;
//                                    Esc, leaving the button or 5 s cancels
//   data-busy / data-sent / data-text  what it says while sending, once sent, and the sentence
//                                    the agent reads
// A sent button stays off until the agent's next update (or 30 s), so one click sends one event.
// Used by kit.js (custom panels) and the board app.
import { esc } from "./format.ts";
import { icon } from "./icons.ts";

export type Send = (kind: string, data: Record<string, string>, text: string) => Promise<unknown>;

const RESERVED = new Set(["send", "text", "confirm", "state", "sent", "busy", "key"]);
type Btn = HTMLButtonElement & { _mkLabel?: string; _mkT?: number };

/** The data an event carries: the button's data-* attributes other than the kit's own. */
export function buttonData(b: HTMLElement): Record<string, string> {
  const data: Record<string, string> = {};
  for (const [k, v] of Object.entries(b.dataset)) if (!RESERVED.has(k) && v !== undefined) data[k] = v;
  return data;
}

export function reset(b: Btn) {
  if (b._mkLabel !== undefined) b.innerHTML = b._mkLabel;
  delete b._mkLabel;
  clearTimeout(b._mkT);
  b.removeAttribute("data-state");
  b.removeAttribute("aria-busy");
  b.disabled = false;
}

function setLabel(b: Btn, html: string) {
  if (b._mkLabel === undefined) b._mkLabel = b.innerHTML;
  b.innerHTML = html;
}

function arm(b: Btn) {
  setLabel(b, `${icon("alert")}${esc(b.dataset.confirm)}<kbd>Enter</kbd>`);
  b.dataset.state = "armed";
  clearTimeout(b._mkT);
  b._mkT = setTimeout(() => reset(b), 5000);
}

function fire(b: Btn, send: Send) {
  const kind = b.dataset.send!;
  const label = (() => { const t = document.createElement("template"); t.innerHTML = b._mkLabel ?? b.innerHTML; return t.content.textContent?.trim() ?? ""; })();
  const text = b.dataset.text || `Pressed "${label || kind}"`;
  clearTimeout(b._mkT);
  // The post happens now, inside the click or key handler: the host only accepts a real user action.
  const result = send(kind, buttonData(b), text);
  setLabel(b, `<span class="mk-spin" aria-hidden="true"></span>${esc(b.dataset.busy || "Sending…")}`);
  b.dataset.state = "busy";
  b.setAttribute("aria-busy", "true");
  b.disabled = true;
  result.then(() => {
    b.innerHTML = `${icon("check")}${esc(b.dataset.sent || "Sent. The agent has it")}`;
    b.dataset.state = "sent";
    b.removeAttribute("aria-busy");
    b._mkT = setTimeout(() => reset(b), 30000);
  }, () => {
    b.innerHTML = `${icon("alert")}Not sent. Try again`;
    b.dataset.state = "error";
    b.removeAttribute("aria-busy");
    b.disabled = false;
    b._mkT = setTimeout(() => reset(b), 6000);
  });
}

/** A press of a [data-send] button: arms it if it asks first, else sends. */
export function press(b: Btn, send: Send) {
  if (b.disabled || !b.dataset.send) return;
  if (b.dataset.confirm && b.dataset.state !== "armed") { arm(b); return; }
  fire(b, send);
}

/** Wire every [data-send] button in the document, now and later. */
export function wireButtons(send: Send) {
  document.addEventListener("click", (e) => {
    const b = (e.target as Element).closest?.("[data-send]") as Btn | null;
    if (b) press(b, send);
  });
  document.addEventListener("keydown", (e) => {
    const b = (document.activeElement as Element | null)?.closest?.("[data-send][data-state='armed']") as Btn | null;
    if (b && e.key === "Escape") { e.preventDefault(); reset(b); }
  });
  document.addEventListener("focusout", (e) => {
    const b = (e.target as Element).closest?.("[data-send][data-state='armed']") as Btn | null;
    if (b) setTimeout(() => { if (document.activeElement !== b) reset(b); }, 0);
  });
}

/** The agent answered (a panel update): buttons waiting on it come back. */
export function resetAll() {
  document.querySelectorAll<Btn>("[data-send][data-state]").forEach(reset);
}
