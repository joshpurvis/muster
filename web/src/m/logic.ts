import { needsHuman, type SessionInfo } from "../types.ts";

export const FONT_FAMILY =
  '"JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Menlo, ui-monospace, monospace';
export const COLLAPSED_HEIGHT = 68;
export interface Grid {
  cols: number;
  rows: number;
}
export interface Cell {
  width: number;
  height: number;
}

export function normalizeCode(code: string): string {
  const clean = code.replace(/[\s-]/g, "");
  return clean.length === 64 ? clean.toLowerCase() : clean.toUpperCase();
}

export function sessionOrder(a: SessionInfo, b: SessionInfo): number {
  return Number(needsHuman(b.state)) - Number(needsHuman(a.state)) || b.state_since - a.state_since ||
    a.id - b.id;
}

export function phoneGrid(width: number, closedHeight: number, cell: Cell): Grid {
  return {
    cols: Math.max(2, Math.min(500, Math.floor(width / Math.max(1, cell.width)))),
    rows: Math.max(
      1,
      Math.min(300, Math.floor((closedHeight - COLLAPSED_HEIGHT) / Math.max(1, cell.height))),
    ),
  };
}

export function scrollPosition(position: number, base: number, height: number, overflow: number) {
  const y = Math.max(0, Math.min(position, base * height + overflow));
  const row = Math.min(base, Math.floor(y / height));
  const pan = Math.min(overflow, y - row * height);
  return { row, pan, remainder: y - row * height - pan };
}

/** A full-screen program keeps its own history (Claude Code's fullscreen view): the alternate
 * screen has no scrollback, so the phone can only show it by asking the program to scroll, with
 * the mouse wheel, as a desktop terminal does. xterm.js sends no wheel reports in X10 mode. */
export function programScrolls(buffer: "normal" | "alternate", mouseTrackingMode: string): boolean {
  return buffer === "alternate" && mouseTrackingMode !== "none" && mouseTrackingMode !== "x10";
}

/** A vertical drag over a full-screen program: pan the phone's view of the screen first, then
 * the part the pan can't take scrolls the program (negative = toward older output). */
export function splitPan(delta: number, pan: number, overflow: number) {
  const next = Math.max(0, Math.min(overflow, pan + delta));
  return { pan: next, rest: delta - (next - pan) };
}

/** Turn drag pixels into whole wheel notches, one per text row, carrying the rest. */
export function wheelSteps(px: number, remainder: number, step: number) {
  const total = remainder + px;
  const count = Math.trunc(total / Math.max(1, step));
  return { count, remainder: total - count * Math.max(1, step) };
}

/** One mouse-wheel report as xterm.js sends it to a program tracking the mouse, at 1-based
 * cell `col`, `row` in the program's active encoding, as the raw bytes to write. xterm.js has
 * only `DEFAULT`, `SGR` and `SGR_PIXELS` (it ignores the UTF-8 and URXVT modes, 1005/1015, so
 * a program asking for those gets `DEFAULT` from the desktop too). `DEFAULT` is one byte per
 * value, never UTF-8, and can't address a cell past 223: xterm.js sends nothing there, nor
 * does this (null), as for an encoding it doesn't know. */
export function wheelReport(
  up: boolean,
  col: number,
  row: number,
  encoding: string,
  cell: Cell,
): Uint8Array | null {
  const button = up ? 64 : 65;
  const text = (s: string) => new TextEncoder().encode(s);
  if (encoding === "DEFAULT") {
    if (col > 223 || row > 223) return null;
    return Uint8Array.of(0x1b, 0x5b, 0x4d, 32 + button, 32 + col, 32 + row);
  }
  if (encoding === "SGR_PIXELS") {
    const x = Math.floor((col - 0.5) * cell.width);
    const y = Math.floor((row - 0.5) * cell.height);
    return text(`\x1b[<${button};${x};${y}M`);
  }
  if (encoding === "SGR") return text(`\x1b[<${button};${col};${row}M`);
  return null;
}

/** `report` `count` times, as one write. */
export function repeatBytes(report: Uint8Array, count: number): Uint8Array {
  const out = new Uint8Array(report.length * Math.max(0, count));
  for (let i = 0; i < count; i++) out.set(report, i * report.length);
  return out;
}

/** How a phone left a full-screen program it scrolled back (per session and run, kept across
 * leaving and reopening it): `back` wheel notches up not yet sent back down, at cell `at`;
 * `behind` once the agent was busy meanwhile, so new lines may sit below the program's view and
 * returning the notches alone would fall short of the newest. `run` is the process it belongs
 * to (its pid): a restarted session's new process never inherits it. */
export interface ProgramScroll {
  run: number | null;
  back: number;
  behind: boolean;
  at: { col: number; row: number } | null;
}

/** Output may be arriving: an agent not idle, or a shell (its state doesn't say). */
export function sessionBusy(s: { kind: string; state: string }): boolean {
  return s.kind === "shell" || s.state !== "idle";
}

/** The saved state for this run of session `s`, a fresh one for a new process. */
export function programScrollFor(saved: Map<number, ProgramScroll>, s: { id: number; pid: number | null }) {
  let p = saved.get(s.id);
  if (!p || p.run !== s.pid) {
    p = { run: s.pid, back: 0, behind: false, at: null };
    saved.set(s.id, p);
  }
  return p;
}

/** Any update of session `s`, viewed or not: busy while scrolled back marks it behind; a new
 * process drops the old run's state. */
export function noteSession(
  saved: Map<number, ProgramScroll>,
  s: { id: number; pid: number | null; kind: string; state: string },
) {
  const p = saved.get(s.id);
  if (!p) return;
  if (p.run !== s.pid) saved.delete(s.id);
  else noteBusy(p, sessionBusy(s));
}

/** `count` notches were sent (negative = up) at `at`, while the agent was `busy` or not. */
export function noteWheel(p: ProgramScroll, count: number, at: ProgramScroll["at"], busy: boolean) {
  p.back = Math.max(0, p.back - count);
  if (count < 0) {
    p.at = at;
    if (busy) p.behind = true;
  }
}

/** The agent got busy (or still is): while scrolled back, new lines land below the view. */
export function noteBusy(p: ProgramScroll, busy: boolean) {
  if (busy && p.back > 0) p.behind = true;
}

/** Whether the phone's view of a program it scrolled back can be taken as the newest output. */
export function programAtLatest(p: ProgramScroll): boolean {
  return p.back === 0 && !p.behind;
}

/** Wheel notches down that bring a program back to its newest line: the ones sent up, plus,
 * when output may have arrived meanwhile, more screens (a program stops at its bottom). Only
 * wheel reports, never keys: the session's kind doesn't say what runs in front (an editor
 * opened from Claude, say), and a program only scrolls on the wheel. A Claude session gets
 * twenty screens, since an agent can print a lot while you read; others five. */
export function returnNotches(p: ProgramScroll, rows: number, kind: string): number {
  return p.back + (p.behind ? (kind === "claude" ? 20 : 5) * rows : 0);
}

export interface BufferLine {
  isWrapped: boolean;
  translateToString(trimRight?: boolean): string;
}
export interface TextBuffer {
  length: number;
  getLine(index: number): BufferLine | undefined;
}

/** Keep an offset per physical row so selection opens at the current scroll position. */
export function bufferText(buffer: TextBuffer, start = 0, end = buffer.length) {
  let text = "";
  const offsets: number[] = [];
  for (let i = start; i < Math.min(end, buffer.length); i++) {
    const line = buffer.getLine(i);
    if (i > start && !line?.isWrapped) text += "\n";
    offsets.push(text.length);
    text += line?.translateToString(!buffer.getLine(i + 1)?.isWrapped) ?? "";
  }
  return { text, offsets };
}

/** The daemon's view of this phone's alerts (`push.status`). */
export interface PushInfo {
  endpoint: string;
  endpoint_host: string;
  created_at: number;
  last_ok_at: number | null;
  last_error: string | null;
  last_error_at: number | null;
}
export type AlertState =
  | { kind: "checking" }
  | { kind: "unsupported" }
  | { kind: "denied" }
  /** The page's service worker is an older one without alerts (an update is still waiting). */
  | { kind: "outdated" }
  /** The daemon has no phone alerts (an older backend: `push.*` is an unknown method). */
  | { kind: "unavailable" }
  | { kind: "off" }
  | { kind: "on"; info: PushInfo; failing: boolean };

/**
 * What the alerts setting shows. Alerts are on only when the daemon holds a subscription and it is
 * the one this browser has: after site data was cleared, or once the push service said the old one
 * is gone, they are off, and turning them on again fixes it. `worker`: the active service worker
 * answered the alerts handshake (web/m/sw.js), so pushes would be shown.
 */
export function alertState(
  supported: boolean,
  permission: string,
  server: PushInfo | null,
  browserEndpoint: string | null,
  worker = true,
): AlertState {
  if (!supported) return { kind: "unsupported" };
  if (permission === "denied") return { kind: "denied" };
  if (!worker) return { kind: "outdated" };
  if (!server || !browserEndpoint || server.endpoint !== browserEndpoint) return { kind: "off" };
  return { kind: "on", info: server, failing: isFailing(server) };
}

function isFailing(info: PushInfo): boolean {
  return info.last_error_at !== null && (info.last_ok_at === null || info.last_error_at > info.last_ok_at);
}

/**
 * After "Turn on again" renewed the subscription and sent a test: on and working only if the test
 * was delivered. The new subscription starts with a clean record, so a test that failed or never
 * ran (`error` without a result) still shows as failing; one the push service called gone is off.
 */
export function repairedState(
  result: { delivered: boolean; subscription: PushInfo | null } | null,
  subscribed: PushInfo | null,
  error: string | null,
  now: number,
): AlertState {
  if (result ? !result.subscription : !subscribed) return { kind: "off" };
  const info = (result?.subscription ?? subscribed)!;
  if (result?.delivered && !isFailing(info)) return { kind: "on", info, failing: false };
  if (!result?.delivered && !isFailing(info)) {
    return {
      kind: "on",
      info: { ...info, last_error: error ?? "the test alert didn't go out", last_error_at: now },
      failing: true,
    };
  }
  return { kind: "on", info, failing: true };
}

/**
 * The browser renewed its subscription (`pushsubscriptionchange`) and the worker couldn't tell the
 * daemon then (web/m/sw.js keeps it as `pending`). On the next connection: send the browser's
 * subscription when it is that pending one, or when the daemon holds an older one for this
 * phone; drop a pending renewal the browser no longer has. Never re-sends when the daemon holds
 * none and nothing is pending (alerts were turned off, or the push service said it's gone).
 */
export function renewal(
  server: PushInfo | null,
  browserEndpoint: string | null,
  pendingEndpoint: string | null,
  permission: string,
): { send: boolean; dropPending: boolean } {
  const usable = browserEndpoint !== null && permission === "granted";
  const pendingIsCurrent = pendingEndpoint !== null && pendingEndpoint === browserEndpoint;
  const send = usable && (pendingIsCurrent || (server !== null && server.endpoint !== browserEndpoint));
  return { send, dropPending: pendingEndpoint !== null && (!pendingIsCurrent || !usable) };
}

/**
 * What a failed `push.status` means for the setting: an older backend without alerts answers
 * "unknown method" (say so plainly); anything else is a lost connection (null: keep what is
 * shown until the next connection asks again).
 */
export function alertsFailure(message: string): AlertState | null {
  return /^unknown method: push\./.test(message) ? { kind: "unavailable" } : null;
}

/** The daemon refused a subscription itself (src/push.rs `REFUSED`): retrying won't help. */
export function subscriptionRefused(message: string): boolean {
  return message.startsWith("subscription refused: ");
}

/**
 * Turning alerts on: keep the browser's subscription only when the daemon holds exactly it (for
 * this daemon's key). Any other one may be what the push service called gone (the daemon deleted
 * it on 404/410, but the browser still hands it out), so a fresh one is made; "Turn on again"
 * (`repair`) always starts fresh.
 */
export function keepBrowserSubscription(
  repair: boolean,
  keyMatches: boolean,
  daemonEndpoint: string | null,
  browserEndpoint: string,
): boolean {
  return !repair && keyMatches && daemonEndpoint === browserEndpoint;
}

/** A test alert's way: sent to the push service, then shown here (or not, after a while). */
export type TestAlert = "idle" | "sending" | "sent" | "arrived" | "late";

/**
 * `shown`: the service worker says it showed the test alert, which can come before `push.test`
 * answers (`delivered`/`failed`), and then stays "arrived" whatever that answer says.
 */
export function testAlertStep(
  s: TestAlert,
  e: "start" | "shown" | "delivered" | "failed" | "timeout",
): TestAlert {
  switch (e) {
    case "start":
      return "sending";
    case "shown":
      return s === "idle" ? "idle" : "arrived";
    case "delivered":
      return s === "sending" ? "sent" : s;
    case "failed":
      return s === "arrived" ? "arrived" : "idle";
    case "timeout":
      return s === "sent" ? "late" : s;
  }
}

/** Base64url (padding optional) to bytes, as `applicationServerKey` wants it. */
export function fromB64url(s: string): Uint8Array {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64 + "=".repeat((4 - b64.length % 4) % 4));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

export function sameBytes(a: ArrayBuffer | Uint8Array | null | undefined, b: Uint8Array): boolean {
  if (!a) return false;
  const x = a instanceof Uint8Array ? a : new Uint8Array(a);
  return x.length === b.length && x.every((v, i) => v === b[i]);
}

export function relativeTime(since: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}
