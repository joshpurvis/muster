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
  | { kind: "off" }
  | { kind: "on"; info: PushInfo; failing: boolean };

/**
 * What the alerts setting shows. Alerts are on only when the daemon holds a subscription and it is
 * the one this browser has: after site data was cleared, or once the push service said the old one
 * is gone, they are off, and turning them on again fixes it.
 */
export function alertState(
  supported: boolean,
  permission: string,
  server: PushInfo | null,
  browserEndpoint: string | null,
): AlertState {
  if (!supported) return { kind: "unsupported" };
  if (permission === "denied") return { kind: "denied" };
  if (!server || !browserEndpoint || server.endpoint !== browserEndpoint) return { kind: "off" };
  const failing = server.last_error_at !== null &&
    (server.last_ok_at === null || server.last_error_at > server.last_ok_at);
  return { kind: "on", info: server, failing };
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
