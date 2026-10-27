// Phone alerts (docs/ANDROID.md "Notifications"): this browser's Web Push subscription, held by
// the daemon (`push.*`), shown by the service worker (web/m/sw.js).
import { signal } from "@preact/signals";
import { rpc } from "../rpc.ts";
import {
  alertsFailure,
  type AlertState,
  alertState,
  fromB64url,
  keepBrowserSubscription,
  type PushInfo,
  renewal,
  repairedState,
  sameBytes,
  subscriptionRefused,
  type TestAlert,
  testAlertStep,
} from "./logic.ts";
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
/** A test alert's way (logic.ts `testAlertStep`). */
export const testAlert = signal<TestAlert>("idle");
let lateTimer = 0;
/** When this page last asked for a test alert (the daemon allows one per 5 s). */
let lastTestAt = 0;
const TEST_GAP_MS = 5100;

/** What web/m/sw.js answers to `muster-sw-hello` once it shows pushes (bump both together). */
const ALERTS_VERSION = 1;
/** Where the service worker keeps a renewed subscription it couldn't hand over yet. */
const PENDING_CACHE = "muster-push";
const PENDING_KEY = "/m/push-pending.json";

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

/** Does `worker` show pushes? It answers the alerts handshake (older workers don't answer). */
function speaksAlerts(worker: ServiceWorker | null, ms = 1500): Promise<boolean> {
  if (!worker) return Promise.resolve(false);
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const done = (ok: boolean) => {
      clearTimeout(timer);
      channel.port1.close();
      resolve(ok);
    };
    const timer = setTimeout(() => done(false), ms);
    channel.port1.onmessage = (e: MessageEvent) => done(Number(e.data?.alerts) >= ALERTS_VERSION);
    worker.postMessage({ type: "muster-sw-hello" }, [channel.port2]);
  });
}

function activated(worker: ServiceWorker, ms: number): Promise<void> {
  return new Promise((resolve) => {
    if (worker.state === "activated" || worker.state === "redundant") return resolve();
    const timer = setTimeout(finish, ms);
    function finish() {
      clearTimeout(timer);
      worker.removeEventListener("statechange", change);
      resolve();
    }
    function change() {
      if (worker.state === "activated" || worker.state === "redundant") finish();
    }
    worker.addEventListener("statechange", change);
  });
}

/**
 * Make sure the worker that receives pushes is one that shows them. After an upgrade
 * `serviceWorker.ready` can still be the old worker (no push handler) while the new one waits:
 * fetch the new one if need be, let it take over, and ask again. False when it still isn't.
 */
async function ensureAlertWorker(reg: ServiceWorkerRegistration): Promise<boolean> {
  if (await speaksAlerts(reg.active)) return true;
  await reg.update().catch(() => {});
  const next = reg.installing ?? reg.waiting;
  if (next) {
    next.postMessage({ type: "muster-skip-waiting" });
    await activated(next, 5000);
  }
  return await speaksAlerts(reg.active);
}

function show(status: PushStatus, endpoint: string | null, worker = true) {
  alerts.value = alertState(true, Notification.permission, status.subscription, endpoint, worker);
}

async function pendingRenewal(): Promise<string | null> {
  try {
    const r = await (await caches.open(PENDING_CACHE)).match(PENDING_KEY);
    const v = r ? await r.json() : null;
    return typeof v?.endpoint === "string" ? v.endpoint : null;
  } catch {
    return null;
  }
}

async function dropPendingRenewal() {
  try {
    await (await caches.open(PENDING_CACHE)).delete(PENDING_KEY);
  } catch {
    // Nothing kept.
  }
}

function subscribeParams(sub: PushSubscription) {
  const keys = sub.toJSON().keys ?? {};
  return { endpoint: sub.endpoint, p256dh: keys.p256dh ?? "", auth: keys.auth ?? "" };
}

let refreshAgain = false;

/**
 * Ask the daemon and the browser how alerts stand (each connection, a `push_changed` event,
 * opening the menu). Also hands over a subscription the browser renewed while the daemon was out
 * of reach (logic.ts `renewal`). Keeps the last answer while offline. While a tap's own flow
 * runs it decides what is shown; one refresh follows it.
 */
export async function refreshAlerts() {
  if (!alertsSupported()) {
    alerts.value = { kind: "unsupported" };
    return;
  }
  if (alertsBusy.peek()) {
    refreshAgain = true;
    return;
  }
  try {
    const reg = await registration().catch(() => null);
    const [status, worker, sub, pending] = await Promise.all([
      rpc.call<PushStatus>("push.status").catch((e) => {
        const state = alertsFailure(errorText(e));
        if (state) alerts.value = state;
        throw e;
      }),
      reg ? ensureAlertWorker(reg) : Promise.resolve(false),
      reg ? reg.pushManager.getSubscription().catch(() => null) : Promise.resolve(null),
      pendingRenewal(),
    ]);
    if (alertsBusy.peek()) {
      refreshAgain = true;
      return;
    }
    // A subscription for another daemon key is no use to this daemon.
    const ours = sub && sameBytes(sub.options.applicationServerKey, fromB64url(status.public_key)) ? sub : null;
    const plan = renewal(status.subscription, ours?.endpoint ?? null, pending, Notification.permission);
    let now = status;
    if (plan.send && ours) {
      try {
        now = await rpc.call<PushStatus>("push.subscribe", subscribeParams(ours));
        await dropPendingRenewal();
      } catch (e) {
        // Kept for the next connection unless the daemon refused it for good.
        if (subscriptionRefused(errorText(e))) await dropPendingRenewal();
        throw e;
      }
    } else if (plan.dropPending) {
      await dropPendingRenewal();
    }
    show(now, sub?.endpoint ?? null, worker);
  } catch {
    // Disconnected: the next connection asks again.
  }
}

function settle() {
  alertsBusy.value = false;
  if (refreshAgain) {
    refreshAgain = false;
    refreshAlerts();
  }
}

/**
 * From a tap (the permission prompt needs one). `repair` ("Turn on again" while alerts fail):
 * start over with a new subscription and send a test, and show alerts as working only when that
 * test went out.
 */
export async function turnOnAlerts(repair = false) {
  if (alertsBusy.peek()) return;
  alertsBusy.value = true;
  // What this flow shows at the end stands; `push_changed` events it caused meanwhile are its own.
  let refresh = false;
  try {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      alerts.value = permission === "denied" ? { kind: "denied" } : { kind: "off" };
      if (permission !== "denied") notify("Alerts need permission to show notifications");
      return;
    }
    const status = await rpc.call<PushStatus>("push.status");
    const key = fromB64url(status.public_key);
    const reg = await registration();
    if (!await ensureAlertWorker(reg)) {
      alerts.value = { kind: "outdated" };
      notify("Reload the page to finish updating Muster, then turn alerts on");
      return;
    }
    let sub = await reg.pushManager.getSubscription();
    // Only the very subscription the daemon holds is reused (logic.ts `keepBrowserSubscription`):
    // one it deleted on 404/410 is still handed out by the browser but is dead.
    const keep = sub && keepBrowserSubscription(
      repair,
      sameBytes(sub.options.applicationServerKey, key),
      status.subscription?.endpoint ?? null,
      sub.endpoint,
    );
    if (sub && !keep) {
      await sub.unsubscribe();
      sub = null;
    }
    sub ??= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key as BufferSource });
    const subscribed = await rpc.call<PushStatus>("push.subscribe", subscribeParams(sub));
    await dropPendingRenewal();
    if (!repair) {
      show(subscribed, sub.endpoint);
      notify("Alerts are on");
      return;
    }
    const wait = lastTestAt + TEST_GAP_MS - Date.now();
    if (wait > 0) await new Promise((r) => setTimeout(r, wait));
    let result: TestResult | null = null;
    let error: string | null = null;
    try {
      startTest();
      result = await rpc.call<TestResult>("push.test");
      error = result.error;
    } catch (e) {
      error = errorText(e);
    }
    finishTest(result);
    alerts.value = repairedState(result, subscribed.subscription, error, Date.now());
    notify(result?.delivered ? "Alerts work again" : `Alerts still fail: ${error ?? "unknown error"}`);
  } catch (e) {
    notify(`Couldn't turn on alerts: ${errorText(e)}`);
    refresh = true;
  } finally {
    refreshAgain = refresh;
    settle();
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
    settle();
  }
}

function startTest() {
  clearTimeout(lateTimer);
  lastTestAt = Date.now();
  testAlert.value = testAlertStep(testAlert.peek(), "start");
}

/** `push.test` answered (null: the call itself failed). */
function finishTest(r: TestResult | null) {
  testAlert.value = testAlertStep(testAlert.peek(), r?.delivered ? "delivered" : "failed");
  if (testAlert.peek() === "sent") {
    lateTimer = setTimeout(() => {
      testAlert.value = testAlertStep(testAlert.peek(), "timeout");
    }, 15000);
  }
}

export async function sendTestAlert() {
  if (testAlert.peek() === "sending") return;
  startTest();
  try {
    const r = await rpc.call<TestResult>("push.test");
    show(r, (await browserSubscription().catch(() => null))?.endpoint ?? null);
    finishTest(r);
    if (!r.delivered && testAlert.peek() !== "arrived") notify(`Test alert failed: ${r.error ?? "unknown error"}`);
  } catch (e) {
    finishTest(null);
    notify(errorText(e));
  }
}

async function browserSubscription(): Promise<PushSubscription | null> {
  return await (await registration()).pushManager.getSubscription();
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
  await dropPendingRenewal();
}

/** Messages from the service worker: a tapped alert, a test alert shown. */
export function listenToServiceWorker(open: (sessionId: number) => void) {
  if (!("serviceWorker" in navigator)) return;
  navigator.serviceWorker.addEventListener("message", (e: MessageEvent) => {
    const d = e.data;
    if (d?.type === "open-session" && Number.isSafeInteger(d.id) && d.id > 0) open(d.id);
    if (d?.type === "alert-shown" && d.kind === "test") {
      const next = testAlertStep(testAlert.peek(), "shown");
      if (next === "arrived") clearTimeout(lateTimer);
      testAlert.value = next;
    }
  });
}
