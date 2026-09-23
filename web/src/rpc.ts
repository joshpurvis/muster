// WebSocket client for the daemon protocol (docs/PROTOCOL.md).
// JSON text frames: requests/responses by numeric id + unsolicited events.
// Binary frames: [u8 type][u32 BE session id][payload].

import { signal } from "@preact/signals";
import type { HelloResult, ServerEvent } from "./types.ts";

export const FRAME_OUTPUT = 0x01;
export const FRAME_INPUT = 0x02;
export const FRAME_REPLAY = 0x03;

export type ConnStatus = "connecting" | "open" | "reconnecting" | "no_token";

export const connStatus = signal<ConnStatus>("connecting");

export class RpcError extends Error {
  constructor(public method: string, message: string) {
    super(message);
  }
}

interface Pending {
  method: string;
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: number;
}

type BinaryHandler = (type: number, sid: number, payload: Uint8Array) => void;
type EventHandler = (ev: ServerEvent) => void;
type HelloHandler = (h: HelloResult, reconnect: boolean) => void;

const TOKEN_KEY = "muster.token";

function storeToken(tok: string) {
  try {
    sessionStorage.setItem(TOKEN_KEY, tok);
  } catch { /* ignore */ }
}

/** Remove `keys` from the URL hash (history.replaceState, no reload). */
function stripHash(keys: string[]) {
  const params = new URLSearchParams(location.hash.slice(1));
  for (const k of keys) params.delete(k);
  const rest = params.toString();
  history.replaceState(null, "", location.pathname + location.search + (rest ? "#" + rest : ""));
}

/** Read the client token. `muster` opens `#code=<one-time code>`, which is exchanged for the
 * token (so the token never appears on a process command line); `muster url` links carry
 * `#token=` directly; the native window puts it in sessionStorage before the page loads.
 * The token is kept in sessionStorage for this tab. A code is stripped from the URL only once
 * the daemon has answered (redeemed or refused), so a reload during the exchange can retry. */
async function readToken(): Promise<string | null> {
  const params = new URLSearchParams(location.hash.slice(1));
  const code = params.get("code");
  let tok = params.get("token");
  if (tok) stripHash(["token", "code"]);
  if (!tok && code) {
    try {
      const r = await fetch("/api/exchange", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code }),
      });
      if (r.ok) tok = ((await r.json()) as { token?: string }).token ?? null;
      // Redeemed or refused (used/expired): either way the code is spent.
      if (r.ok || r.status === 403) stripHash(["code"]);
    } catch { /* network error: keep the code for a reload; fall back to a stored token */ }
  }
  if (tok) {
    storeToken(tok);
    return tok;
  }
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

class Rpc {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  /** Calls made while disconnected; flushed after hello on (re)connect. */
  private queue: { id: number; frame: string }[] = [];
  private ready = false;
  private everOpened = false;
  private backoff = 250;
  private token: string | null = null;
  private retryTimer = 0;

  onEvent: EventHandler = () => {};
  onBinary: BinaryHandler = () => {};
  onHello: HelloHandler = () => {};

  async start() {
    this.token = await readToken();
    if (!this.token) {
      connStatus.value = "no_token";
      return;
    }
    this.connect();
    // Reconnect promptly when the tab comes back or the network returns.
    addEventListener("online", () => this.kick());
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") this.kick();
    });
  }

  private kick() {
    if (!this.ws || this.ws.readyState === WebSocket.CLOSED) {
      this.backoff = 250;
      this.connect();
    }
  }

  private connect() {
    clearTimeout(this.retryTimer);
    this.retryTimer = 0;
    if (this.ws && this.ws.readyState !== WebSocket.CLOSED && this.ws.readyState !== WebSocket.CLOSING) {
      return;
    }
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws?token=${encodeURIComponent(this.token!)}`;
    let ws: WebSocket;
    try {
      ws = new WebSocket(url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.ready = false;
    ws.onopen = () => {
      if (this.ws !== ws) {
        ws.close();
        return;
      }
      this.backoff = 250;
      const reconnect = this.everOpened;
      this.everOpened = true;
      const id = this.nextId++;
      this.track(id, "hello").then((h) => {
        if (this.ws !== ws) return;
        this.ready = true;
        connStatus.value = "open";
        this.onHello(h as HelloResult, reconnect);
        const q = this.queue;
        this.queue = [];
        for (const item of q) ws.send(item.frame);
      }).catch(() => ws.close());
      ws.send(JSON.stringify({ id, method: "hello", params: {} }));
    };
    ws.onmessage = (e) => {
      if (typeof e.data === "string") this.handleText(e.data);
      else this.handleBinary(new Uint8Array(e.data as ArrayBuffer));
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.ready = false;
      // In-flight calls can't complete on this socket.
      for (const [id, p] of this.pending) {
        if (!this.queue.some((q) => q.id === id)) {
          clearTimeout(p.timer);
          p.reject(new RpcError(p.method, "Connection lost"));
          this.pending.delete(id);
        }
      }
      connStatus.value = "reconnecting";
      this.scheduleReconnect();
    };
    ws.onerror = () => {/* onclose follows */};
  }

  private scheduleReconnect() {
    const d = this.backoff;
    this.backoff = Math.min(5000, this.backoff * 2);
    clearTimeout(this.retryTimer);
    this.retryTimer = setTimeout(() => this.connect(), d);
  }

  private handleText(text: string) {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof msg.id === "number") {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) {
        const err = msg.error as { message?: string };
        p.reject(new RpcError(p.method, err.message ?? String(msg.error)));
      } else p.resolve(msg.result);
      return;
    }
    if (typeof msg.event === "string") {
      try {
        this.onEvent(msg as unknown as ServerEvent);
      } catch (e) {
        console.error("event handler failed", msg.event, e);
      }
    }
  }

  private handleBinary(buf: Uint8Array) {
    if (buf.length < 5) return;
    const type = buf[0];
    const sid = ((buf[1] << 24) | (buf[2] << 16) | (buf[3] << 8) | buf[4]) >>> 0;
    this.onBinary(type, sid, buf.subarray(5));
  }

  private track(id: number, method: string, timeoutMs = 30000): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        this.queue = this.queue.filter((q) => q.id !== id);
        reject(new RpcError(method, "Timed out"));
      }, timeoutMs);
      this.pending.set(id, { method, resolve, reject, timer });
    });
  }

  call<T = unknown>(method: string, params: unknown = {}): Promise<T> {
    const id = this.nextId++;
    const frame = JSON.stringify({ id, method, params });
    const p = this.track(id, method) as Promise<T>;
    if (this.ws && this.ready && this.ws.readyState === WebSocket.OPEN) this.ws.send(frame);
    else this.queue.push({ id, frame });
    return p;
  }

  get isOpen() {
    return this.ready && !!this.ws && this.ws.readyState === WebSocket.OPEN;
  }

  sendInput(sid: number, data: string | Uint8Array) {
    if (!this.isOpen) return;
    const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
    const buf = new Uint8Array(5 + bytes.length);
    buf[0] = FRAME_INPUT;
    new DataView(buf.buffer).setUint32(1, sid, false);
    buf.set(bytes, 5);
    this.ws!.send(buf);
  }
}

export const rpc = new Rpc();
