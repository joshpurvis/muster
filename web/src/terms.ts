// Terminal registry: one xterm per session id, created once and reparented between panes.
// Terminals live outside the Preact tree; output never triggers a VDOM render.

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { FRAME_OUTPUT, FRAME_REPLAY, rpc } from "./rpc.ts";

const MAX_WEBGL = 10;
const FONT_FAMILY =
  '"JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Menlo, ui-monospace, monospace';

interface Entry {
  sid: number;
  term: Terminal;
  fit: FitAddon;
  search: SearchAddon;
  el: HTMLDivElement;
  host: HTMLElement | null;
  webgl: WebglAddon | null;
  webglFailed: boolean;
  queue: Uint8Array[];
  queued: number;
  raf: number;
  /** Replayed scrollback is being parsed: drop the replies xterm generates for old
   * terminal queries (DA, cursor reports) so they don't reach the live program as keys. */
  replaying: boolean;
  attached: boolean;
  lastUse: number;
  resizeTimer: number;
  sentCols: number;
  sentRows: number;
}

const entries = new Map<number, Entry>();
let park: HTMLDivElement | null = null;
let fontSize = 13;
let shortcutMatcher: (e: KeyboardEvent) => boolean = () => false;
let focusListener: (sid: number) => void = () => {};

export function setShortcutMatcher(f: (e: KeyboardEvent) => boolean) {
  shortcutMatcher = f;
}
export function onTerminalFocus(f: (sid: number) => void) {
  focusListener = f;
}

function parkEl(): HTMLDivElement {
  if (!park) {
    park = document.createElement("div");
    park.className = "term-park";
    park.setAttribute("aria-hidden", "true");
    document.body.appendChild(park);
  }
  return park;
}

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function themeFromCss() {
  const ansi = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
  ] as const;
  const t: Record<string, string> = {
    background: cssVar("--term-bg", "#1a1a1e"),
    foreground: cssVar("--term-fg", "#e4e4e8"),
    cursor: cssVar("--term-cursor", "#e4e4e8"),
    cursorAccent: cssVar("--term-bg", "#1a1a1e"),
    selectionBackground: cssVar("--term-selection", "#8b7cf655"),
    selectionInactiveBackground: cssVar("--term-selection-inactive", "#8b7cf630"),
    scrollbarSliderBackground: cssVar("--term-scrollbar", "#ffffff20"),
    scrollbarSliderHoverBackground: cssVar("--term-scrollbar-hover", "#ffffff38"),
    scrollbarSliderActiveBackground: cssVar("--term-scrollbar-hover", "#ffffff48"),
  };
  ansi.forEach((k, i) => (t[k] = cssVar(`--ansi-${i}`, "")));
  for (const k of Object.keys(t)) if (!t[k]) delete t[k];
  return t;
}

export function applyTheme() {
  const theme = themeFromCss();
  for (const e of entries.values()) e.term.options.theme = theme;
}

export function setFontSize(px: number) {
  fontSize = px;
  for (const e of entries.values()) {
    e.term.options.fontSize = px;
    scheduleFit(e);
  }
}

function create(sid: number): Entry {
  const el = document.createElement("div");
  el.className = "term-el";
  el.dataset.sid = String(sid);
  parkEl().appendChild(el);
  const term = new Terminal({
    allowProposedApi: true,
    fontFamily: FONT_FAMILY,
    fontSize,
    lineHeight: 1.15,
    scrollback: 10000,
    cursorBlink: true,
    cursorStyle: "bar",
    cursorInactiveStyle: "outline",
    drawBoldTextInBrightColors: false,
    macOptionIsMeta: true,
    rightClickSelectsWord: false,
    theme: themeFromCss(),
    // App shortcuts use Alt; don't let xterm swallow Alt+letters on macOS.
    macOptionClickForcesSelection: true,
  });
  const fit = new FitAddon();
  const search = new SearchAddon();
  term.loadAddon(fit);
  term.loadAddon(search);
  term.loadAddon(
    new WebLinksAddon((ev, uri) => {
      if (ev.ctrlKey || ev.metaKey) globalThis.open(uri, "_blank", "noopener");
    }),
  );
  try {
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = "11";
  } catch { /* optional */ }
  term.open(el);

  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== "keydown") return !shortcutMatcher(ev);
    if (ev.ctrlKey && ev.shiftKey && !ev.altKey && ev.code === "KeyC") {
      const sel = term.getSelection();
      if (sel) navigator.clipboard?.writeText(sel).catch(() => {});
      return false;
    }
    if (ev.ctrlKey && ev.shiftKey && !ev.altKey && ev.code === "KeyV") {
      navigator.clipboard?.readText().then((t) => t && term.paste(t)).catch(() => {});
      return false;
    }
    return !shortcutMatcher(ev);
  });

  const enc = new TextEncoder();
  term.onData((d) => {
    if (!entry.replaying) rpc.sendInput(sid, enc.encode(d));
  });
  term.onBinary((d) => {
    if (entry.replaying) return;
    const b = new Uint8Array(d.length);
    for (let i = 0; i < d.length; i++) b[i] = d.charCodeAt(i) & 0xff;
    rpc.sendInput(sid, b);
  });
  term.onResize(({ cols, rows }) => sendResize(entry, cols, rows));
  el.addEventListener("focusin", () => focusListener(sid));

  const entry: Entry = {
    sid,
    term,
    fit,
    search,
    el,
    host: null,
    webgl: null,
    webglFailed: false,
    queue: [],
    queued: 0,
    raf: 0,
    replaying: false,
    attached: false,
    lastUse: Date.now(),
    resizeTimer: 0,
    sentCols: 0,
    sentRows: 0,
  };
  entries.set(sid, entry);
  return entry;
}

function sendResize(e: Entry, cols: number, rows: number, now = false) {
  if (!e.host) return; // only visible terminals resize the PTY
  clearTimeout(e.resizeTimer);
  const go = () => {
    if (!e.host || (e.sentCols === cols && e.sentRows === rows)) return;
    e.sentCols = cols;
    e.sentRows = rows;
    rpc.call("session.resize", { id: e.sid, cols, rows }).catch(() => {});
  };
  if (now) go();
  else e.resizeTimer = setTimeout(go, 60);
}

function enableWebgl(e: Entry) {
  if (e.webgl || e.webglFailed) return;
  // Keep the number of live GL contexts bounded (browsers cap them around 16).
  const live = [...entries.values()].filter((x) => x.webgl);
  if (live.length >= MAX_WEBGL) {
    const victim = live.filter((x) => !x.host).sort((a, b) => a.lastUse - b.lastUse)[0];
    if (victim) disableWebgl(victim);
    else return;
  }
  try {
    const gl = new WebglAddon();
    gl.onContextLoss(() => disableWebgl(e));
    e.term.loadAddon(gl);
    e.webgl = gl;
  } catch {
    e.webglFailed = true;
  }
}

function disableWebgl(e: Entry) {
  try {
    e.webgl?.dispose();
  } catch { /* ignore */ }
  e.webgl = null;
}

const ro = new ResizeObserver((items) => {
  for (const it of items) {
    const sid = Number((it.target as HTMLElement).dataset.hostSid);
    const e = entries.get(sid);
    if (e) scheduleFit(e);
  }
});

const fitPending = new Set<Entry>();
let fitRaf = 0;
function scheduleFit(e: Entry) {
  fitPending.add(e);
  if (fitRaf) return;
  fitRaf = requestAnimationFrame(() => {
    fitRaf = 0;
    const list = [...fitPending];
    fitPending.clear();
    list.forEach(fitNow);
  });
}

function fitNow(e: Entry) {
  if (!e.host || !e.host.isConnected) return;
  const r = e.host.getBoundingClientRect();
  if (r.width < 20 || r.height < 20) return;
  const dims = e.fit.proposeDimensions();
  if (!dims || !Number.isFinite(dims.cols) || !Number.isFinite(dims.rows)) return;
  const cols = Math.max(10, dims.cols);
  const rows = Math.max(3, dims.rows);
  if (cols !== e.term.cols || rows !== e.term.rows) e.term.resize(cols, rows);
  else sendResize(e, cols, rows);
}

/** Put session `sid`'s terminal inside `host` (creating/attaching it on first use). */
export function mount(sid: number, host: HTMLElement) {
  const e = entries.get(sid) ?? create(sid);
  e.lastUse = Date.now();
  if (e.host && e.host !== host) ro.unobserve(e.host);
  e.host = host;
  host.dataset.hostSid = String(sid);
  if (e.el.parentElement !== host) host.appendChild(e.el);
  ro.observe(host);
  enableWebgl(e);
  requestAnimationFrame(() => {
    fitNow(e);
    if (!e.attached) attach(e);
    else e.term.refresh(0, e.term.rows - 1);
  });
}

/** Move the terminal back to the offscreen park (if `host` still owns it). */
export function unmount(sid: number, host: HTMLElement) {
  const e = entries.get(sid);
  if (!e) return;
  if (e.host === host) {
    ro.unobserve(host);
    e.host = null;
    e.sentCols = e.sentRows = 0;
  }
  if (e.el.parentElement === host) parkEl().appendChild(e.el);
}

function attach(e: Entry) {
  e.attached = true;
  const go = () =>
    rpc.call("session.resize", { id: e.sid, cols: e.term.cols, rows: e.term.rows })
      .catch(() => {})
      .then(() => rpc.call("session.attach", { id: e.sid }))
      .catch(() => {
        // Keep `attached` (= wanted): a reconnect's reattachAll retries it. If the session is
        // gone, session_removed disposes the entry.
      });
  e.sentCols = e.term.cols;
  e.sentRows = e.term.rows;
  go();
}

/** After a reconnect, re-attach every terminal (the replay frame resets it). */
export function reattachAll() {
  for (const e of entries.values()) {
    if (!e.attached) continue;
    e.sentCols = e.sentRows = 0;
    if (e.host) {
      e.sentCols = e.term.cols;
      e.sentRows = e.term.rows;
      rpc.call("session.resize", { id: e.sid, cols: e.term.cols, rows: e.term.rows }).catch(() => {});
    }
    rpc.call("session.attach", { id: e.sid }).catch(() => {});
  }
}

export function dispose(sid: number) {
  const e = entries.get(sid);
  if (!e) return;
  entries.delete(sid);
  if (e.host) ro.unobserve(e.host);
  disableWebgl(e);
  e.term.dispose();
  e.el.remove();
}

function flush(e: Entry) {
  e.raf = 0;
  if (!e.queue.length) return;
  let data: Uint8Array;
  if (e.queue.length === 1) data = e.queue[0];
  else {
    data = new Uint8Array(e.queued);
    let off = 0;
    for (const c of e.queue) {
      data.set(c, off);
      off += c.length;
    }
  }
  e.queue = [];
  e.queued = 0;
  e.term.write(data);
}

rpc.onBinary = (type, sid, payload) => {
  const e = entries.get(sid);
  if (!e) return;
  if (type === FRAME_REPLAY) {
    e.queue = [];
    e.queued = 0;
    if (e.raf) cancelAnimationFrame(e.raf);
    e.raf = 0;
    e.term.reset();
    if (payload.length) {
      e.replaying = true;
      e.term.write(payload.slice(), () => {
        e.replaying = false;
      });
    }
    return;
  }
  if (type !== FRAME_OUTPUT) return;
  // Copy: the underlying buffer may be reused.
  e.queue.push(payload.slice());
  e.queued += payload.length;
  if (e.queued > 1 << 20) flush(e);
  else if (!e.raf) {
    e.raf = requestAnimationFrame(() => flush(e));
    // Hidden tabs don't run rAF; make sure output still lands.
    setTimeout(() => e.raf && flush(e), 250);
  }
};

export function focusTerminal(sid: number): boolean {
  const e = entries.get(sid);
  if (!e || !e.host) return false;
  e.term.focus();
  return true;
}

export function hasTerminalFocus(): number | null {
  const a = document.activeElement;
  if (!a) return null;
  const el = (a as HTMLElement).closest?.(".term-el") as HTMLElement | null;
  return el ? Number(el.dataset.sid) : null;
}

export function terminal(sid: number): Terminal | null {
  return entries.get(sid)?.term ?? null;
}

export function searchNext(sid: number, q: string, prev = false) {
  const e = entries.get(sid);
  if (!e) return false;
  const opts = {
    incremental: !prev,
    decorations: {
      matchBackground: "#5a4fb0",
      matchOverviewRuler: "#8b7cf6",
      activeMatchBackground: "#f0a020",
      activeMatchColorOverviewRuler: "#f0a020",
    },
  };
  return prev ? e.search.findPrevious(q, opts) : e.search.findNext(q, opts);
}

export function searchClear(sid: number) {
  entries.get(sid)?.search.clearDecorations();
}

export function onSearchResults(sid: number, f: (idx: number, count: number) => void) {
  const e = entries.get(sid);
  if (!e) return () => {};
  const d = e.search.onDidChangeResults((r) => f(r.resultIndex, r.resultCount));
  return () => d.dispose();
}

export function scrollToBottom(sid: number) {
  entries.get(sid)?.term.scrollToBottom();
}

export function refitAll() {
  for (const e of entries.values()) if (e.host) scheduleFit(e);
}

/** Re-attach one terminal (e.g. after a restart cleared its replay). */
export function reattach(sid: number) {
  const e = entries.get(sid);
  if (!e || !e.attached) return;
  rpc.call("session.attach", { id: sid }).catch(() => {});
}
