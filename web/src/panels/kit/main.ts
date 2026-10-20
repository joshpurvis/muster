// Muster Kit runtime (kit.js): injected by Muster in front of a custom panel's HTML when the
// agent opens it with `kit: true`, after kit.css. One global, `mk`. No network, no dependencies.
//  - the MCP Apps handshake, theme and data updates, so an agent writes no JSON-RPC
//  - [data-send] buttons send panel events; [data-confirm] asks first; busy / sent states
//  - mk.chart / spark / ring / meter / icon, and the same as <mk-*> tags
//  - mk.board(el, data): Board JSON inside a custom page
// It adds nothing to what the sandbox allows: no fetch, no nested frames.
import { esc, fmt } from "./format.ts";
import { icon, iconNames } from "./icons.ts";
import { meter, ring, seriesColor, spark } from "./parts.ts";
import { chart } from "./chart.ts";
import { resetAll, wireButtons } from "./buttons.ts";
import { upgrade } from "./tags.ts";
import { board } from "./board.ts";
import { validateBoard } from "../apps/board/logic.ts";

type Json = Record<string, unknown>;
const w = globalThis as unknown as { mk?: Json; MK_NO_AUTO?: boolean };
const inFrame = parent !== self;
let rpcId = 0;
const pending = new Map<number, { res: (v: unknown) => void; rej: (e: unknown) => void }>();
const dataFns: ((d: unknown) => void)[] = [];
let data: unknown = undefined;

function post(msg: Json) { if (inFrame) parent.postMessage({ jsonrpc: "2.0", ...msg }, "*"); }
function call(method: string, params: Json): Promise<unknown> {
  if (!inFrame) return Promise.resolve({ standalone: true });
  const id = ++rpcId;
  post({ id, method, params });
  return new Promise((res, rej) => pending.set(id, { res, rej }));
}
function setTheme(ctx: unknown) {
  const t = (ctx as Json | undefined)?.theme;
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
}

/** Send a panel event from code. Call it straight from a click or key handler: the host takes
 * events only during a real user action. `text` is the sentence the agent reads. */
function send(kind: string, payload: Json = {}, text?: string): Promise<unknown> {
  return call("ui/message", { role: "user", content: { type: "text", text: text || `${kind} ${JSON.stringify(payload)}` }, _meta: { muster: { kind, data: payload } } });
}

function gotData(d: unknown) {
  data = d;
  if (mk) mk.data = d;
  resetAll();
  for (const f of dataFns) {
    try { f(d); } catch (err) { console.error("[mk] onData handler failed:", err); }
  }
}

addEventListener("message", (e) => {
  if (e.source !== parent) return;
  const m = (e.data || {}) as Json & { id?: number; method?: string; params?: Json; error?: unknown; result?: unknown };
  if (typeof m.id === "number" && pending.has(m.id) && !m.method) {
    const p = pending.get(m.id)!;
    pending.delete(m.id);
    if (m.error) p.rej(m.error);
    else p.res(m.result);
    return;
  }
  if (m.method === "ui/notifications/tool-input") gotData((m.params?.arguments as unknown) ?? {});
  else if (m.method === "ui/notifications/host-context-changed") setTheme(m.params);
  else if (m.method === "ui/resource-teardown" && m.id !== undefined) post({ id: m.id, result: {} });
});

const mk: Json = {
  data: undefined,
  fmt, icon, icons: iconNames, color: seriesColor, spark, ring, meter, upgrade, send,
  chart: (el: HTMLElement, spec: never, o?: never) => chart(el, spec, o),
  onData: (fn: (d: unknown) => void) => { dataFns.push(fn); if (data !== undefined) fn(data); },
  board: (el: HTMLElement, d: unknown) => {
    const r = validateBoard(d);
    if (!r.ok) {
      el.innerHTML = `<div class="mk-error" role="alert">${icon("alert")}<span><b>This board's JSON is invalid.</b> ${esc(r.errors.slice(0, 8).join("; "))}</span></div>`;
      console.error("[mk] board:", r.errors);
      return;
    }
    board(el, r.value, r.warnings);
  },
};
w.mk = Object.assign(w.mk ?? {}, mk);

// Before the page draws: the kit's base styles and a theme until the host says which.
document.documentElement.classList.add("mk");
if (!document.documentElement.dataset.theme) {
  document.documentElement.dataset.theme = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

async function init() {
  upgrade(document);
  if (!inFrame) return;
  try {
    const r = await call("ui/initialize", { protocolVersion: "2026-01-26", appInfo: { name: "muster-kit", version: "1" }, appCapabilities: {} }) as Json;
    setTheme(r?.hostContext);
    post({ method: "ui/notifications/initialized", params: {} });
    new ResizeObserver(() => post({ method: "ui/notifications/size-changed", params: { width: innerWidth, height: document.documentElement.scrollHeight } })).observe(document.body);
  } catch (err) {
    console.error("[mk] the Muster handshake failed:", err);
  }
}

wireButtons((kind, payload, text) => send(kind, payload, text));
if (!w.MK_NO_AUTO) {
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", () => void init());
  else void init();
}
