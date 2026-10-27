// Phone alerts (docs/ANDROID.md "Notifications"): this browser's Web Push subscription, held by
// the daemon (`push.*`), shown by the service worker (web/m/sw.js).
import { signal } from "@preact/signals";
import { rpc } from "../rpc.ts";
import { type AlertState, alertState, fromB64url, type PushInfo, sameBytes } from "./logic.ts";
import { errorText, notify } from "./store.ts";

interface PushStatus {
  public_key: string;
  subscription: PushInfo | null;
}
interface TestResult extends PushStatus {
  delivered: boolean;
  error: string | null;
}

export const alerts = signal<AlertState>({ kind: "checking" });
export const alertsBusy = signal(false);
/** A test alert's way: sent to the push service, then shown here (or not, after a while). */
export const testAlert = signal<"idle" | "sending" | "sent" | "arrived" | "late">("idle");
let lateTimer = 0;

export function alertsSupported(): boolean {
  return isSecureContext && "serviceWorker" in navigator && "PushManager" in globalThis &&
    "Notification" in globalThis;
}

async function registration(): Promise<ServiceWorkerRegistration> {
  let timer = 0;
  try {
    return await Promise.race([
      navigator.serviceWorker.ready,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("the page's service worker isn't running; reload")), 5000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function browserSubscription(): Promise<PushSubscription | null> {
  return await (await registration()).pushManager.getSubscription();
}

function show(status: PushStatus, endpoint: string | null) {
  alerts.value = alertState(true, Notification.permission, status.subscription, endpoint);
}

/** Ask the daemon and the browser how alerts stand. Keeps the last answer while offline. */
export async function refreshAlerts() {
  if (!alertsSupported()) {
    alerts.value = { kind: "unsupported" };
    return;
  }
  try {
    const [status, sub] = await Promise.all([
      rpc.call<PushStatus>("push.status"),
      browserSubscription().catch(() => null),
    ]);
    show(status, sub?.endpoint ?? null);
  } catch {
    // Disconnected: the next connection asks again.
  }
}

/** From a tap (the permission prompt needs one). */
export async function turnOnAlerts() {
  if (alertsBusy.peek()) return;
  alertsBusy.value = true;
  try {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      alerts.value = permission === "denied" ? { kind: "denied" } : { kind: "off" };
      if (permission !== "denied") notify("Alerts need permission to show notifications");
      return;
    }
    const key = fromB64url((await rpc.call<PushStatus>("push.key")).public_key);
    const reg = await registration();
    let sub = await reg.pushManager.getSubscription();
    // Subscribed for another key (a different daemon, or its key was replaced): start over.
    if (sub && !sameBytes(sub.options.applicationServerKey, key)) {
      await sub.unsubscribe();
      sub = null;
    }
    sub ??= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key as BufferSource });
    const keys = sub.toJSON().keys ?? {};
    show(
      await rpc.call<PushStatus>("push.subscribe", {
        endpoint: sub.endpoint,
        p256dh: keys.p256dh ?? "",
        auth: keys.auth ?? "",
      }),
      sub.endpoint,
    );
    notify("Alerts are on");
  } catch (e) {
    notify(`Couldn't turn on alerts: ${errorText(e)}`);
    await refreshAlerts();
  } finally {
    alertsBusy.value = false;
  }
}

export async function turnOffAlerts() {
  if (alertsBusy.peek()) return;
  alertsBusy.value = true;
  try {
    await rpc.call("push.unsubscribe");
    await forgetBrowserSubscription();
    alerts.value = { kind: "off" };
    testAlert.value = "idle";
    notify("Alerts are off");
  } catch (e) {
    notify(`Couldn't turn off alerts: ${errorText(e)}`);
  } finally {
    alertsBusy.value = false;
  }
}

export async function sendTestAlert() {
  if (testAlert.peek() === "sending") return;
  testAlert.value = "sending";
  clearTimeout(lateTimer);
  try {
    const r = await rpc.call<TestResult>("push.test");
    show(r, (await browserSubscription().catch(() => null))?.endpoint ?? null);
    if (!r.delivered) {
      testAlert.value = "idle";
      notify(`Test alert failed: ${r.error ?? "unknown error"}`);
      return;
    }
    testAlert.value = "sent";
    lateTimer = setTimeout(() => {
      if (testAlert.peek() === "sent") testAlert.value = "late";
    }, 15000);
  } catch (e) {
    testAlert.value = "idle";
    notify(errorText(e));
  }
}

/** Close a session's alert once it has been seen here. */
export async function clearAlert(sessionId: number) {
  if (!alertsSupported()) return;
  try {
    for (const n of await (await registration()).getNotifications({ tag: `s${sessionId}` })) n.close();
  } catch {
    // Nothing shown, or no service worker.
  }
}

/** Logging out: this browser stops listening (the daemon deletes its copy on revoke). */
export async function forgetBrowserSubscription() {
  try {
    await (await browserSubscription())?.unsubscribe();
  } catch {
    // Nothing to forget.
  }
}

/** Messages from the service worker: a tapped alert, a test alert shown. */
export function listenToServiceWorker(open: (sessionId: number) => void) {
  if (!("serviceWorker" in navigator)) return;
  navigator.serviceWorker.addEventListener("message", (e: MessageEvent) => {
    const d = e.data;
    if (d?.type === "open-session" && Number.isSafeInteger(d.id) && d.id > 0) open(d.id);
    if (d?.type === "alert-shown" && d.kind === "test" && ["sent", "late"].includes(testAlert.peek())) {
      clearTimeout(lateTimer);
      testAlert.value = "arrived";
    }
  });
}
