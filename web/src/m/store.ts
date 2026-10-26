import { batch, computed, signal } from "@preact/signals";
import { connStatus, rpc } from "../rpc.ts";
import type { Channel, Decision, SessionInfo } from "../types.ts";
import { load, save } from "../util.ts";
import { type Grid, normalizeCode } from "./logic.ts";

export interface Device {
  id: number;
  name: string;
  scopes: string[];
  created_at: number;
  last_seen_at: number;
  expires_at: number;
}
interface Prefs {
  fontSize: number;
  termSize: "follow" | "keep";
}
interface Viewer {
  sid: number;
  receive(type: number, payload: Uint8Array): void;
  update(info: SessionInfo): void;
  grid(): Grid;
}

const stored = load<Prefs>("m.prefs", { fontSize: 11, termSize: "follow" });
export const prefs = signal<Prefs>({
  fontSize: Math.max(7, Math.min(20, Number(stored?.fontSize) || 11)),
  termSize: stored?.termSize === "keep" ? "keep" : "follow",
});
export const sessions = signal(new Map<number, SessionInfo>());
export const channels = signal(new Map<number, Channel>());
export const device = signal<Device | null>(null);
export const auth = signal<"checking" | "paired" | "unpaired" | "offline">("checking");
export const selected = signal<number | null>(null);
export const online = signal(navigator.onLine);
export const pairError = signal("");
export const pairBusy = signal(false);
export const toast = signal("");
export const ready = computed(() => auth.value === "paired" && connStatus.value === "open");
export const canInteract = computed(() =>
  ready.value && !!device.value?.scopes.some((s) => s === "interact" || s === "manage")
);
let viewer: Viewer | null = null;
let attached: number | null = null;
let toastTimer = 0;
let authVersion = 0;

export function setPrefs(next: Partial<Prefs>) {
  prefs.value = { ...prefs.value, ...next };
  save("m.prefs", prefs.peek());
}
export function notify(message: string) {
  toast.value = message;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.value = "", 4000);
}
export const errorText = (e: unknown) => e instanceof Error ? e.message : String(e);

async function deviceRequest(path: string, body?: unknown): Promise<Response> {
  return await fetch(`/api/device/${path}`, {
    method: path === "me" ? "GET" : "POST",
    credentials: "same-origin",
    headers: path === "me" ? {} : { "Content-Type": "application/json", "X-Muster": "1" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
    signal: AbortSignal.timeout(10000),
  });
}

async function checkDevice(): Promise<boolean> {
  const version = ++authVersion;
  try {
    const response = await deviceRequest("me");
    if (version !== authVersion) return false;
    if (response.status === 401) {
      batch(() => {
        device.value = null;
        auth.value = "unpaired";
        sessions.value = new Map();
        channels.value = new Map();
      });
      rpc.suspend();
      return false;
    }
    if (!response.ok) throw new Error("Could not reach Muster");
    const result = await response.json();
    if (version !== authVersion) return false;
    batch(() => {
      device.value = result.device;
      auth.value = "paired";
    });
    return true;
  } catch {
    if (version === authVersion && !device.peek()) auth.value = "offline";
    return false;
  }
}

export async function reconnect() {
  if (document.visibilityState !== "visible" || pairBusy.peek()) return;
  if (await checkDevice() && document.visibilityState === "visible") rpc.resume();
}

export async function pair(code: string) {
  if (pairBusy.peek()) return;
  pairBusy.value = true;
  pairError.value = "";
  ++authVersion;
  // The code lives only in the URL: keep it for a reload unless the daemon took or refused it.
  let spent = false;
  let failure = "";
  try {
    const platform = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData
      ?.platform;
    const name = /android/i.test(platform ?? navigator.userAgent) ? "Android phone" : "Phone";
    const response = await deviceRequest("pair", { code: normalizeCode(code), name });
    if (response.status === 429) throw new Error("Too many attempts. Wait a minute, then try again.");
    spent = response.ok || response.status === 403;
    const result = await response.json();
    if (!response.ok) {
      throw new Error(result.error?.message ?? "Code expired. Run muster pair for a new code.");
    }
    batch(() => {
      device.value = result.device;
      auth.value = "paired";
    });
    // A socket still open from before belongs to the old cookie's device: start over.
    rpc.suspend();
    if (document.visibilityState === "visible") rpc.resume();
  } catch (e) {
    failure = errorText(e);
  } finally {
    if (spent) {
      const hash = new URLSearchParams(location.hash.slice(1));
      hash.delete("pair");
      history.replaceState(
        history.state,
        "",
        location.pathname + location.search + (hash.size ? `#${hash}` : ""),
      );
    }
    pairBusy.value = false;
  }
  if (!failure) return;
  // A phone that is already paired stays paired (e.g. an old QR code opened again).
  await reconnect();
  if (auth.peek() === "paired") notify(`Pairing failed: ${failure}`);
  else {
    auth.value = "unpaired";
    pairError.value = failure;
  }
}

export async function logout() {
  try {
    const response = await deviceRequest("logout");
    if (!response.ok && response.status !== 401) {
      throw new Error("Could not log out. Try again when connected.");
    }
    ++authVersion;
    device.value = null;
    auth.value = "unpaired";
    sessions.value = new Map();
    channels.value = new Map();
    selected.value = null;
    history.replaceState(null, "", location.pathname);
    rpc.suspend();
  } catch (e) {
    notify(errorText(e));
  }
}

function attach() {
  if (!viewer || !ready.peek() || attached === viewer.sid) return;
  const current = viewer;
  attached = current.sid;
  rpc.call("session.attach", { id: current.sid, replay_bytes: 262144 }).catch((e) => {
    if (viewer === current && rpc.isOpen) notify(errorText(e));
  });
}

export function bindTerminal(next: Viewer | null) {
  if (attached !== null && rpc.isOpen) rpc.call("session.detach", { id: attached }).catch(() => {});
  attached = null;
  viewer = next;
  attach();
}

export function markOpened(s: SessionInfo) {
  if (canInteract.peek()) rpc.call("channel.mark_read", { id: s.channel_id }).catch(() => {});
}
export function openSession(s: SessionInfo) {
  history.pushState({ musterSession: true }, "", `#s=${s.id}`);
  selected.value = s.id;
}
export function closeSession() {
  if (history.state?.musterSession) history.back();
  else {
    history.replaceState(null, "", location.pathname);
    selected.value = null;
  }
}

let viewOnlyScrollNoted = false;
/** Wheel reports that scroll a full-screen program (Claude Code) back through its own history,
 * sent like a desktop terminal's mouse wheel: raw input, without the resize `act` does in
 * `follow`. False when this device can't send input (view access only, or disconnected). */
export function scrollProgram(sid: number, data: string): boolean {
  if (!canInteract.peek()) {
    if (ready.peek() && !viewOnlyScrollNoted) {
      viewOnlyScrollNoted = true;
      notify("This device has view access only, so it can't scroll back in a full-screen agent");
    }
    return false;
  }
  const s = sessions.peek().get(sid);
  if (!s || s.state === "exited") return false;
  rpc.sendInput(sid, data);
  return rpc.isOpen;
}

/** Resize only on an explicit action; approval uses the version rendered by its caller. */
export async function act<T = unknown>(s: SessionInfo, method: string, params: object = {}, grid?: Grid) {
  if (!canInteract.peek()) {
    throw new Error(ready.peek() ? "This device has view access only" : "Disconnected");
  }
  if (s.state === "exited") throw new Error("Session exited");
  if (prefs.peek().termSize === "follow" && method !== "session.interrupt") {
    const target = viewer?.sid === s.id ? viewer.grid() : grid;
    if (!target) throw new Error("Open the session to measure the terminal");
    const current = sessions.peek().get(s.id) ?? s;
    if (
      current.size_owner !== `device:${device.peek()!.id}` || current.cols !== target.cols ||
      current.rows !== target.rows
    ) {
      await rpc.call("session.resize", { id: s.id, ...target });
    }
  }
  return await rpc.call<T>(method, { id: s.id, ...params });
}
export function approve(s: SessionInfo, decision: Decision, grid?: Grid) {
  return act(s, "session.approve", { decision, state_seq: s.state_seq, state_since: s.state_since }, grid);
}

export function start() {
  const readLocation = () => {
    const sid = Number(new URLSearchParams(location.hash.slice(1)).get("s"));
    selected.value = Number.isSafeInteger(sid) && sid > 0 ? sid : null;
  };
  readLocation();
  if (selected.peek() && !history.state?.musterSession) {
    const hash = location.hash;
    history.replaceState(null, "", location.pathname);
    history.pushState({ musterSession: true }, "", hash);
  }
  addEventListener("popstate", readLocation);
  addEventListener("hashchange", () => {
    // A pairing link opened while the app is already loaded only changes the hash.
    const code = new URLSearchParams(location.hash.slice(1)).get("pair");
    if (code) pair(code);
    else readLocation();
  });
  rpc.onHello = (hello) => {
    batch(() => {
      sessions.value = new Map(hello.sessions.map((s) => [s.id, s]));
      channels.value = new Map(hello.channels.map((c) => [c.id, c]));
    });
    const info = viewer && sessions.peek().get(viewer.sid);
    if (info) viewer!.update(info);
    attached = null;
    attach();
  };
  rpc.onEvent = (event) => {
    switch (event.event) {
      case "session_upserted":
        sessions.value = new Map(sessions.peek()).set(event.data.id, event.data);
        if (viewer?.sid === event.data.id) viewer.update(event.data);
        break;
      case "session_removed": {
        const next = new Map(sessions.peek());
        next.delete(event.data.id);
        sessions.value = next;
        break;
      }
      case "channel_upserted":
        channels.value = new Map(channels.peek()).set(event.data.id, event.data);
        break;
      case "channel_removed": {
        const next = new Map(channels.peek());
        next.delete(event.data.id);
        channels.value = next;
        break;
      }
      case "notice":
        notify(event.data.text);
        break;
    }
  };
  rpc.onBinary = (type, sid, payload) => {
    if (viewer?.sid === sid) viewer.receive(type, payload);
  };
  rpc.onClosed = async () => {
    attached = null;
    if (auth.peek() !== "unpaired") await checkDevice();
  };
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") rpc.suspend();
    else reconnect();
  });
  document.addEventListener("freeze", () => rpc.suspend());
  addEventListener("offline", () => {
    online.value = false;
    rpc.suspend();
  });
  addEventListener("online", () => {
    online.value = true;
    reconnect();
  });
  const code = new URLSearchParams(location.hash.slice(1)).get("pair");
  if (code) pair(code);
  else reconnect();
}
