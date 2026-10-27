const CACHE = "muster-mobile-v1";
const SHELL = [
  "/m/",
  "/m/app.js",
  "/m/app.css",
  "/m/manifest.webmanifest",
  "/m/icon.svg",
  "/m/icon-192.png",
  "/m/icon-512.png",
  "/m/icon-maskable-512.png",
];

self.addEventListener("install", (event) => {
  // Take over at once: an older worker has no push handler, so alerts the page turned on would
  // otherwise go unshown until every Muster tab closed. Pages are network-first, so a newer worker
  // under an older page is harmless.
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});
self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const key of await caches.keys()) {
      if (key.startsWith("muster-mobile-") && key !== CACHE) await caches.delete(key);
    }
    await self.clients.claim();
  })());
});
self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin || !url.pathname.startsWith("/m/")) {
    return;
  }
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const response = await fetch(request);
      if (response.ok) await cache.put(request, response.clone()).catch(() => {});
      return response;
    } catch {
      const saved = await cache.match(request) ??
        (request.mode === "navigate" ? await cache.match("/m/") : null);
      return saved ?? Response.error();
    }
  })());
});

// ----- phone alerts (docs/ANDROID.md "Notifications") -----
// The daemon sends {v: 1, kind: "session" | "test", session_id, state_since, at, title, body},
// encrypted to this browser. One notification per session (tag), so a newer alert replaces an
// older one; an alert older than the one already shown (they can arrive out of order) is dropped.

/** What the page asks for before it turns alerts on (web/src/m/alerts.ts, bump both together). */
const ALERTS_VERSION = 1;
/** A renewed subscription not yet handed to the daemon (kept outside the shell cache). */
const PENDING_CACHE = "muster-push";
const PENDING_KEY = "/m/push-pending.json";

function sessionUrl(id) {
  return Number.isSafeInteger(id) && id > 0 ? `/m/#s=${id}` : "/m/";
}

/** Is the shown notification's alert newer than `alert`? */
function newer(shown, alert) {
  const since = Number(shown?.state_since) || 0;
  const at = Number(shown?.at) || 0;
  const aSince = Number(alert.state_since) || 0;
  return since > aSince || (since === aSince && at > (Number(alert.at) || 0));
}

/**
 * One check-and-show at a time per tag: two pushes for a session handled at once would both see
 * nothing shown, and the older could replace the newer.
 */
const lanes = new Map();
function inLane(tag, job) {
  const run = (lanes.get(tag) ?? Promise.resolve()).then(job, job);
  const tail = run.catch(() => {});
  lanes.set(tag, tail);
  tail.then(() => {
    if (lanes.get(tag) === tail) lanes.delete(tag);
  });
  return run;
}

async function showAlert(data) {
  let alert = null;
  try {
    alert = data ? data.json() : null;
  } catch {
    alert = null;
  }
  const reg = self.registration;
  if (!alert || alert.v !== 1) {
    // Chrome wants each push to show something.
    return reg.showNotification("Muster", { body: "Something needs you.", tag: "muster", data: { url: "/m/" } });
  }
  const test = alert.kind === "test";
  const id = Number(alert.session_id);
  const tag = test ? "test" : `s${id}`;
  await inLane(tag, async () => {
    const shown = await reg.getNotifications({ tag });
    const later = shown.find((n) => newer(n.data, alert));
    if (later) {
      // Keep the newer one as it is, silently (re-showing it counts as this push's notification).
      return reg.showNotification(later.title, {
        body: later.body,
        tag,
        icon: later.icon,
        data: later.data,
        silent: true,
      });
    }
    await reg.showNotification(String(alert.title || "Muster"), {
      body: String(alert.body || ""),
      tag,
      renotify: true,
      icon: "/m/icon-192.png",
      timestamp: Number(alert.at) || Date.now(),
      data: {
        session_id: test ? null : id,
        url: test ? "/m/" : sessionUrl(id),
        state_since: alert.state_since,
        at: alert.at,
      },
    });
  });
  if (test) {
    for (const client of await self.clients.matchAll({ type: "window", includeUncontrolled: true })) {
      client.postMessage({ type: "alert-shown", kind: "test" });
    }
  }
}

self.addEventListener("push", (event) => {
  event.waitUntil(Promise.all([showAlert(event.data), sendPending().catch(() => {})]));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const id = Number(event.notification.data?.session_id);
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const app = windows.find((c) => new URL(c.url).pathname.startsWith("/m"));
    if (app) {
      await app.focus();
      if (Number.isSafeInteger(id) && id > 0) app.postMessage({ type: "open-session", id });
      return;
    }
    await self.clients.openWindow(sessionUrl(id));
  })());
});

// The page's handshake (does this worker show alerts?) and its "take over now".
self.addEventListener("message", (event) => {
  const d = event.data;
  if (d?.type === "muster-sw-hello") event.ports?.[0]?.postMessage({ type: "muster-sw", alerts: ALERTS_VERSION });
  else if (d?.type === "muster-skip-waiting") self.skipWaiting();
  else if (d?.type === "muster-push-retry") event.waitUntil?.(sendPending().catch(() => {}));
});

async function loadPending() {
  const r = await (await caches.open(PENDING_CACHE)).match(PENDING_KEY);
  if (!r) return null;
  try {
    const v = await r.json();
    return typeof v?.endpoint === "string" ? v : null;
  } catch {
    return null;
  }
}

async function savePending(p) {
  await (await caches.open(PENDING_CACHE)).put(
    PENDING_KEY,
    new Response(JSON.stringify(p), { headers: { "Content-Type": "application/json" } }),
  );
}

async function dropPending() {
  await (await caches.open(PENDING_CACHE)).delete(PENDING_KEY);
}

/**
 * Hand a kept renewal to the daemon. Kept while the daemon can't be reached (restarting, the
 * tailnet down: a network error or a 5xx); dropped once it answered (taken, or refused for good),
 * or when the browser no longer has that subscription. The page also sends it on its next
 * connection (web/src/m/alerts.ts `refreshAlerts`).
 */
async function sendPending() {
  const p = await loadPending();
  if (!p) return;
  const current = await self.registration.pushManager.getSubscription();
  if (!current || current.endpoint !== p.endpoint) return dropPending();
  let r;
  try {
    r = await fetch("/api/rpc", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", "X-Muster": "1" },
      body: JSON.stringify({
        id: 1,
        method: "push.subscribe",
        params: { endpoint: p.endpoint, p256dh: p.p256dh, auth: p.auth },
      }),
    });
  } catch {
    return;
  }
  if (r.status >= 500 || r.status === 0) return;
  await dropPending();
}

// The browser replaced the subscription (rare): keep the new one, then hand it to the daemon.
self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil((async () => {
    const key = event.oldSubscription?.options?.applicationServerKey;
    const sub = event.newSubscription ??
      (key ? await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }) : null);
    if (!sub) return;
    const keys = sub.toJSON().keys ?? {};
    await savePending({ endpoint: sub.endpoint, p256dh: keys.p256dh ?? "", auth: keys.auth ?? "" });
    await sendPending();
  })().catch(() => {}));
});
