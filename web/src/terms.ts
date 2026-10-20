// Terminal registry: one xterm per session id, created once and reparented between panes.
// Terminals live outside the Preact tree; output never triggers a VDOM render.

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
// The self-contained UMD build: deno bundle mangles the .mjs build's default import of its
// base64 decoder ("is not a constructor"), which threw in create() and left every tab blank.
import { ImageAddon } from "@xterm/addon-image/lib/addon-image.js";
import { effect } from "@preact/signals";
import { connStatus, FRAME_OUTPUT, FRAME_REPLAY, rpc } from "./rpc.ts";
import type { SessionInfo } from "./types.ts";
import { isRightButtonReport, RIGHT_BUTTON } from "./mousereport.ts";
import { copyText } from "./util.ts";

const MAX_WEBGL = 10;

/** Is typing into this session paused (an input interruption the user has not cleared)?
 * Keystrokes are dropped here too, so nothing is sent that the backend would refuse. */
let inputPaused: (sid: number) => boolean = () => false;
export function setInputPaused(paused: (sid: number) => boolean) {
  inputPaused = paused;
}
/** Shows a failed clipboard copy or read (an error toast). */
let clipboardFailed: (text: string) => void = () => {};
export function onClipboardFailed(f: (text: string) => void) {
  clipboardFailed = f;
}
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
  /** The parser is inside replayed scrollback (between the in-band markers, see
   * `writeReplay`): queries in it are old, so xterm must not answer them. Typing is never
   * blocked. */
  inReplay: boolean;
  /** Ctrl+Shift+V: fallback clipboard read if no native paste event shows up. */
  pasteTimer: number;
  /** The fallback's clipboard read in flight; a native paste arriving meanwhile cancels it. */
  pasteRead: { cancelled: boolean } | null;
  /** Until then, a late native paste event is dropped (the fallback already pasted). */
  pasteBlockUntil: number;
  /** Native paste events seen: a failed fallback read only reports if none arrives late. */
  pastes: number;
  attached: boolean;
  /** Pending dispose of a detached terminal (its tab left this window). */
  evictTimer: number;
  lastUse: number;
  resizeTimer: number;
  sentCols: number;
  sentRows: number;
  sizeOwner?: string | null;
  /** Removes the latency probe's listeners from this terminal. */
  probeOff?: () => void;
}

const entries = new Map<number, Entry>();
let park: HTMLDivElement | null = null;
let fontSize = 13;
let shortcutMatcher: (e: KeyboardEvent) => boolean = () => false;
let focusListener: (sid: number) => void = () => {};

/** The typing latency monitor's hooks (latency_monitor.ts). Null while it is off: then no
 * listener is installed and each hook point costs one null check. */
export interface LatencyProbe {
  keydown(sid: number, ev: KeyboardEvent, webgl: boolean): void;
  /** A user's input bytes were handed to the WebSocket. */
  sent(sid: number): void;
  /** A live output frame arrived; returns a write callback when a key waits on it. */
  output(sid: number): (() => void) | undefined;
  render(sid: number): void;
  /** The terminal is gone: forget its keys. */
  dispose(sid: number): void;
}
let probe: LatencyProbe | null = null;

export function setLatencyProbe(p: LatencyProbe | null) {
  probe = p;
  for (const e of entries.values()) probeEntry(e);
}

function probeEntry(e: Entry) {
  e.probeOff?.();
  e.probeOff = undefined;
  const p = probe;
  const ta = e.term.textarea;
  if (!p || !ta) return;
  // On the terminal's element, capturing: xterm's own keydown listener on the textarea sends
  // the bytes, and a listener there would run after it.
  const onKey = (ev: KeyboardEvent) => {
    if (ev.target === ta) p.keydown(e.sid, ev, !!e.webgl);
  };
  e.el.addEventListener("keydown", onKey, true);
  const r = e.term.onRender(() => p.render(e.sid));
  e.probeOff = () => {
    e.el.removeEventListener("keydown", onKey, true);
    r.dispose();
  };
}

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
    disableStdin: connStatus.peek() !== "open",
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
  // Sixel and iTerm2 inline images. Load before installQueryHandlers: xterm runs the last
  // registered handler first, so ours keep swallowing the DA1 answer this addon would give.
  // Each cached terminal keeps its own image store, hence the lower cap (default 128 MB).
  // The store may keep one image over its cap, so a single image is capped too (2048x2048,
  // 16 MB). Optional: a failure here must not stop the terminal from opening.
  // Known gaps: the host's cursor reports (DSR 6) don't see an image move the cursor; a
  // replay swallows XTSMGRAPHICS palette changes; a replay cut mid-image drops its rest.
  try {
    term.loadAddon(new ImageAddon({ storageLimit: 32, pixelLimit: 2048 * 2048 }));
  } catch (err) {
    console.error("image addon failed to load", err);
  }
  term.open(el);

  term.attachCustomKeyEventHandler((ev) => {
    const plainCtrlShift = ev.ctrlKey && ev.shiftKey && !ev.altKey && !ev.metaKey;
    if (plainCtrlShift && ev.code === "KeyC") {
      // Returning false only stops xterm; without preventDefault Chromium opens the inspector.
      ev.preventDefault();
      ev.stopPropagation();
      if (ev.type === "keydown") {
        const sel = term.getSelection();
        // copyText's fallback copies through a hidden textarea, which takes the focus.
        if (sel) {
          copyText(sel).then((ok) => {
            term.focus();
            if (!ok) clipboardFailed("Copy failed: the clipboard isn't available here");
          });
        }
      }
      return false;
    }
    if (plainCtrlShift && ev.code === "KeyV") {
      // Chromium turns Ctrl+Shift+V into a native paste event on xterm's textarea (no
      // clipboard permission needed). WebKitGTK (the native window) may not: if no paste
      // event arrives shortly, read the clipboard ourselves. Either way xterm must not also
      // send ^V, and the text is pasted once.
      if (ev.type === "keydown" && !ev.repeat) {
        clearTimeout(entry.pasteTimer);
        entry.pasteTimer = setTimeout(() => {
          entry.pasteTimer = 0;
          const pastes = entry.pastes;
          // Unless a native paste turns up late after all.
          const failed = () =>
            setTimeout(() => {
              if (entry.pastes === pastes) {
                clipboardFailed("Paste failed: Muster can't read the clipboard here");
              }
            }, 500);
          if (!navigator.clipboard?.readText) return failed();
          const read = { cancelled: false };
          entry.pasteRead = read;
          const done = () => {
            if (entry.pasteRead === read) entry.pasteRead = null;
          };
          navigator.clipboard.readText()
            .then((t) => {
              done();
              if (read.cancelled || !t || entries.get(sid) !== entry) return;
              term.paste(t);
              // Only now can a late native paste be a duplicate.
              entry.pasteBlockUntil = Date.now() + 1500;
            })
            .catch(() => {
              done();
              if (!read.cancelled) failed();
            });
        }, 50);
      }
      return false;
    }
    return !shortcutMatcher(ev);
  });

  el.addEventListener("paste", (ev) => {
    entry.pastes++;
    if (entry.pasteTimer) {
      // The native paste arrived in time: it does the pasting.
      clearTimeout(entry.pasteTimer);
      entry.pasteTimer = 0;
    } else if (entry.pasteRead) {
      // Late, but the fallback hasn't pasted yet: the native paste wins, the read is dropped.
      entry.pasteRead.cancelled = true;
      entry.pasteRead = null;
    } else if (Date.now() < entry.pasteBlockUntil) {
      // Too late: the clipboard fallback already pasted this.
      entry.pasteBlockUntil = 0;
      ev.preventDefault();
      ev.stopImmediatePropagation();
    }
  }, true);

  installQueryHandlers(term, () => entry.inReplay, (v) => (entry.inReplay = v));

  const enc = new TextEncoder();
  let userInput = false;
  // onData also carries query answers. Only a user's bytes should take the grid back.
  // Private API: without it only focus takes the grid back.
  const userFlag = typeof (term as any)._core?.coreService?.onUserInput === "function";
  if (userFlag) (term as any)._core.coreService.onUserInput(() => userInput = true);
  const dropReport = rightButtonFilter(term);
  term.onData((d) => {
    const user = userInput;
    userInput = false;
    if (dropReport(d)) return;
    if (user) takeSizeBack(entry);
    if (inputPaused(sid)) return;
    rpc.sendInput(sid, enc.encode(d));
    // Only a user's bytes, never query answers (all bytes if xterm can't tell them apart).
    if (user || !userFlag) probe?.sent(sid);
  });
  term.onBinary((d) => {
    if (dropReport(d)) return;
    const b = new Uint8Array(d.length);
    for (let i = 0; i < d.length; i++) b[i] = d.charCodeAt(i) & 0xff;
    if (inputPaused(sid)) return;
    rpc.sendInput(sid, b);
  });
  term.onResize(({ cols, rows }) => sendResize(entry, cols, rows));
  el.addEventListener("focusin", () => {
    takeSizeBack(entry);
    focusListener(sid);
  });

  const entry: Entry = {
    sid,
    term,
    fit,
    search,
    el,
    host: null,
    webgl: null,
    webglFailed: false,
    inReplay: false,
    pasteTimer: 0,
    pasteRead: null,
    pasteBlockUntil: 0,
    pastes: 0,
    attached: false,
    evictTimer: 0,
    lastUse: Date.now(),
    resizeTimer: 0,
    sentCols: 0,
    sentRows: 0,
  };
  entries.set(sid, entry);
  probeEntry(entry);
  return entry;
}

/** The right button is Muster's (it opens the terminal menu): a program that tracks the mouse
 * never gets a report of it, wherever xterm saw the event (its document-level listeners keep
 * reporting outside the terminal while another button is held). Right-button events are dropped
 * before xterm encodes them, which also covers legacy releases that carry no button; xterm's own
 * listener cleanup still runs. Returns a check for onData/onBinary: is this chunk a right-button
 * report? Only chunks xterm's mouse code emits are looked at, never typed or pasted text. */
function rightButtonFilter(term: Terminal): (d: string) => boolean {
  // Private API (xterm 6). Without it, any chunk that is exactly one right-button report while
  // the program tracks the mouse is dropped (and a legacy right release still passes).
  const mouse = (term as any)._core?.coreMouseService;
  if (typeof mouse?.triggerMouseEvent !== "function") {
    return (d) => term.modes.mouseTrackingMode !== "none" && isRightButtonReport(d);
  }
  const trigger = mouse.triggerMouseEvent.bind(mouse);
  let reporting = false;
  mouse.triggerMouseEvent = (e: { button: number }) => {
    if (e.button === RIGHT_BUTTON) return false;
    reporting = true;
    try {
      return trigger(e);
    } finally {
      reporting = false;
    }
  };
  return (d) => reporting && isRightButtonReport(d);
}

/** In-band markers around replayed scrollback. The parser reaches them in stream order, so the
 * "inside a replay" flag is exact however writes and replays interleave. The nonce keeps
 * program output from toggling it. */
const REPLAY_OSC = 7717;
const REPLAY_NONCE = Math.random().toString(36).slice(2, 10);
const REPLAY_START = `\x1b]${REPLAY_OSC};${REPLAY_NONCE};start\x07`;
const REPLAY_END = `\x1b]${REPLAY_OSC};${REPLAY_NONCE};end\x07`;

/** Terminal query answers. The daemon answers device attributes (DA1/DA2) and status/cursor
 * reports (DSR 5/6, DECXCPR) itself, for every program output, whether or not a client is
 * attached. xterm must never answer those too, or the program gets one extra reply per
 * attached client (and old ones from every replay). Other queries only xterm can answer
 * (colors, modes, settings) are answered live, but not inside a replay. */
function installQueryHandlers(term: Terminal, inReplay: () => boolean, setReplay: (v: boolean) => void) {
  const p = term.parser;
  p.registerOscHandler(REPLAY_OSC, (data) => {
    if (data === `${REPLAY_NONCE};start`) setReplay(true);
    else if (data === `${REPLAY_NONCE};end`) setReplay(false);
    return true;
  });
  // Always swallowed: the daemon is the only responder.
  p.registerCsiHandler({ final: "c" }, () => true); // DA1 (primary; no prefix)
  p.registerCsiHandler({ prefix: ">", final: "c" }, () => true); // DA2 (secondary)
  p.registerCsiHandler({ final: "n" }, () => true); // DSR 5 (status), DSR 6 (cursor)
  p.registerCsiHandler({ prefix: "?", final: "n" }, () => true); // DECXCPR (?6n)
  // Answered by xterm live, swallowed while replaying old output.
  p.registerCsiHandler({ intermediates: "$", final: "p" }, () => inReplay()); // DECRQM
  p.registerCsiHandler({ prefix: "?", intermediates: "$", final: "p" }, () => inReplay());
  p.registerDcsHandler({ intermediates: "$", final: "q" }, () => inReplay()); // DECRQSS
  p.registerCsiHandler({ prefix: "?", final: "S" }, () => inReplay()); // XTSMGRAPHICS (image addon)
  p.registerCsiHandler({ final: "t" }, () => inReplay()); // Pixel/cell size reports (14t, 16t, 18t)
  for (const osc of [4, 10, 11, 12]) {
    // Color queries (`?`); color changes still apply during a replay.
    p.registerOscHandler(osc, (data) => inReplay() && data.includes("?"));
  }
}

function writeReplay(e: Entry, payload: Uint8Array) {
  // RIS in the stream (not term.reset()) so output already handed to xterm is parsed first.
  e.term.write("\x1bc");
  if (payload.length) {
    e.term.write(REPLAY_START);
    e.term.write(payload);
    e.term.write(REPLAY_END);
  }
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

function takeSizeBack(e: Entry) {
  if (!e.host || !e.sizeOwner || e.sizeOwner === "local") return;
  fitNow(e);
  e.sentCols = e.sentRows = 0;
  sendResize(e, e.term.cols, e.term.rows, true);
  e.sizeOwner = "local";
}

/** Follow a phone's grid until this pane is used or fitted again; never echo its resize. */
export function sessionSizeChanged(info: SessionInfo) {
  const e = entries.get(info.id);
  if (!e) return;
  e.sizeOwner = info.size_owner;
  if (!e.host || !info.size_owner || info.size_owner === "local") return;
  if (info.cols === e.term.cols && info.rows === e.term.rows) return;
  clearTimeout(e.resizeTimer);
  e.sentCols = info.cols;
  e.sentRows = info.rows;
  e.term.resize(info.cols, info.rows);
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
  clearTimeout(e.evictTimer);
  e.evictTimer = 0;
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
      // Detached (tab moved away) or disposed while the resize was in flight: don't attach.
      .then(() =>
        e.attached && entries.get(e.sid) === e ? rpc.call("session.attach", { id: e.sid }) : undefined
      )
      .catch(() => {
        // Keep `attached` (= wanted): a reconnect's reattachAll retries it. If the session is
        // gone, session_removed disposes the entry.
      });
  e.sentCols = e.term.cols;
  e.sentRows = e.term.rows;
  go();
}

/** How long a terminal whose tab left this window stays cached (it may come back). */
const EVICT_MS = 5 * 60_000;

/**
 * Stop streaming session `sid` to this window (its tab moved to another window or this
 * client no longer shows it). The xterm stays cached for a while; showing it again
 * re-attaches, and the replay resets it.
 */
export function detach(sid: number) {
  const e = entries.get(sid);
  if (!e || !e.attached) return;
  e.attached = false;
  if (e.host) unmount(sid, e.host);
  rpc.call("session.detach", { id: sid }).catch(() => {});
  clearTimeout(e.evictTimer);
  e.evictTimer = setTimeout(() => {
    if (!entries.get(sid)?.attached) dispose(sid);
  }, EVICT_MS);
}

/** Session ids with a live (attached) terminal in this client. */
export function attachedSids(): number[] {
  return [...entries.values()].filter((e) => e.attached).map((e) => e.sid);
}

/** After a reconnect, re-attach every terminal (the replay frame resets it). */
export function reattachAll() {
  for (const e of entries.values()) {
    if (!e.attached) continue;
    e.sentCols = e.sentRows = 0;
    // A phone owns the size: keep its grid until this pane is used (see takeSizeBack), rather
    // than re-sending it as ours, which would leave nothing to take back.
    if (e.host && (!e.sizeOwner || e.sizeOwner === "local")) {
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
  clearTimeout(e.pasteTimer);
  clearTimeout(e.resizeTimer);
  clearTimeout(e.evictTimer);
  e.pasteTimer =
    e.evictTimer =
      0;
  if (e.host) ro.unobserve(e.host);
  e.probeOff?.();
  probe?.dispose(sid);
  disableWebgl(e);
  e.term.dispose();
  e.el.remove();
}

rpc.onBinary = (type, sid, payload) => {
  const e = entries.get(sid);
  if (!e) return;
  if (type === FRAME_REPLAY) {
    writeReplay(e, payload.slice());
    return;
  }
  if (type !== FRAME_OUTPUT) return;
  // Straight to xterm, which batches parsing itself and paints once per frame. Holding
  // output for our own animation frame cost a whole extra frame on every echo: the write
  // then landed inside a rAF callback, so xterm's render request waited for the next one.
  // Copy: the underlying buffer may be reused.
  e.term.write(payload.slice(), probe?.output(sid));
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

/** Pane chrome above a terminal (px): the pane-tab strip (2+ sessions in the pane) or the
 * pane header (2+ panes in the tab); a lone pane with one session has none. */
export const PANE_STRIP_PX = 31;
// .term-host padding plus xterm's scrollbar gutter (FitAddon's DEFAULT_SCROLL_BAR_WIDTH).
const HOST_PAD_X = 14 + 14;
const HOST_PAD_Y = 10;

/** Cell size in CSS px, from a live terminal when there is one. */
function cellSize(): { w: number; h: number } {
  for (const e of entries.values()) {
    // Same private field FitAddon reads.
    const c = (e.term as any)._core?._renderService?.dimensions?.css?.cell;
    if (c?.width > 0 && c?.height > 0 && e.term.options.fontSize === fontSize) {
      return { w: c.width, h: c.height };
    }
  }
  const ctx = document.createElement("canvas").getContext("2d");
  let w = fontSize * 0.6;
  if (ctx) {
    ctx.font = `${fontSize}px ${FONT_FAMILY}`;
    w = ctx.measureText("W").width || w;
  }
  return { w, h: Math.ceil(fontSize * 1.2) * 1.15 };
}

/** Terminal grid that fits a terminal host of `w`×`h` px (what fit() will pick there). */
export function estimateGrid(w: number, h: number): { cols: number; rows: number } {
  const c = cellSize();
  return {
    cols: Math.max(20, Math.min(500, Math.floor((w - HOST_PAD_X) / c.w))),
    rows: Math.max(5, Math.min(300, Math.floor((h - HOST_PAD_Y) / c.h))),
  };
}

// Input typed while the socket is down would be dropped silently: refuse it instead (the panes
// show a veil saying so) and take it again once reconnected.
effect(() => {
  const down = connStatus.value !== "open";
  for (const e of entries.values()) e.term.options.disableStdin = down;
});

export function refitAll() {
  for (const e of entries.values()) if (e.host) scheduleFit(e);
}
