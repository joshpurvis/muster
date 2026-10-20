// Kit buttons that talk to the agent, with no script from the agent:
//   data-send="kind" (+ any data-*)  sends the panel event `kind` with the other data attributes
//   data-confirm="Question?"         first click (or Enter) asks; a second click or Enter sends;
//                                    Esc, leaving the button or 5 s cancels. A key held down
//                                    never confirms: every key must be let go first
//   (board actions carry their typed JSON data on the button instead of data-* strings)
//   data-busy / data-sent / data-text  what it says while sending, once sent, and the sentence
//                                    the agent reads
// A sent button stays off until the agent's next update (or 30 s), so one click sends one event.
// Used by kit.js (custom panels) and the board app.
import { esc } from "./format.ts";
import { icon } from "./icons.ts";

export type Send = (kind: string, data: Record<string, unknown>, text: string) => Promise<unknown>;

const RESERVED = new Set(["send", "text", "confirm", "state", "sent", "busy", "key"]);
/** `_mkData`: typed JSON data (a board action's), sent as is instead of the data-* strings.
 * `_mkReady`: an armed button may confirm (no key was held when it armed, or all were let go). */
export type Btn = HTMLButtonElement & { _mkLabel?: string; _mkT?: number; _mkData?: Record<string, unknown>; _mkReady?: boolean };

/** The data an event carries: a board action's typed data, else the button's data-* attributes
 * other than the kit's own (strings). */
export function buttonData(b: HTMLElement): Record<string, unknown> {
  const typed = (b as Btn)._mkData;
  if (typed) return { ...typed };
  const data: Record<string, string> = {};
  for (const [k, v] of Object.entries(b.dataset)) if (!RESERVED.has(k) && v !== undefined) data[k] = v;
  return data;
}

/** Physical keys down now (modifiers aside): an armed button confirms only once all are up. */
const held = new Set<string>();
const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]);
function release() {
  if (held.size) return;
  document.querySelectorAll<Btn>("[data-send][data-state='armed']").forEach((b) => { b._mkReady = true; });
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
  // Armed by a key press (Enter, a shortcut): that key's auto-repeat must not confirm.
  b._mkReady = held.size === 0;
  clearTimeout(b._mkT);
  b._mkT = setTimeout(() => reset(b), 5000);
}

function fire(b: Btn, send: Send) {
  const kind = b.dataset.send!;
  // The label as read, without its shortcut hint.
  const label = (() => {
    const t = document.createElement("template");
    t.innerHTML = b._mkLabel ?? b.innerHTML;
    t.content.querySelectorAll("kbd").forEach((k) => k.remove());
    return t.content.textContent?.trim() ?? "";
  })();
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
  if (b.dataset.confirm && !b._mkReady) return;
  fire(b, send);
}

/** Wire every [data-send] button in the document, now and later. */
export function wireButtons(send: Send) {
  document.addEventListener("click", (e) => {
    const b = (e.target as Element).closest?.("[data-send]") as Btn | null;
    if (b) press(b, send);
  });
  // Capture phase: recorded before the key's own default action (Enter clicks a button).
  document.addEventListener("keydown", (e) => {
    if (!MODIFIERS.has(e.key)) held.add(e.code || e.key);
    const on = (document.activeElement as Element | null)?.closest?.("[data-send]") as Btn | null;
    // Auto-repeat never presses a kit button again.
    if (on && e.repeat && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); return; }
    const b = on?.dataset.state === "armed" ? on : null;
    if (b && e.key === "Escape") { e.preventDefault(); reset(b); }
  }, true);
  document.addEventListener("keyup", (e) => { held.delete(e.code || e.key); release(); }, true);
  // Focus left the panel: the key-ups may never come.
  addEventListener("blur", () => { held.clear(); release(); });
  document.addEventListener("focusout", (e) => {
    const b = (e.target as Element).closest?.("[data-send][data-state='armed']") as Btn | null;
    if (b) setTimeout(() => { if (document.activeElement !== b) reset(b); }, 0);
  });
}

/** The agent answered (a panel update): buttons waiting on it come back. */
export function resetAll() {
  document.querySelectorAll<Btn>("[data-send][data-state]").forEach(reset);
}
