// The phone service worker (web/m/sw.js), run against fakes of the worker globals.

function equal(actual: unknown, expected: unknown, what = "") {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${what ? what + ": " : ""}expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

type Handler = (event: any) => void;
interface Shown {
  title: string;
  tag: string;
  body: string;
  data: any;
  silent?: boolean;
}

const SOURCE = await Deno.readTextFile(new URL("../../m/sw.js", import.meta.url));

/** A fresh worker: its handlers, what it showed, what it fetched, its caches. */
function worker(opts: {
  getNotificationsDelay?: (call: number) => number;
  fetch?: (body: any) => Promise<Response>;
  subscription?: { endpoint: string } | null;
} = {}) {
  const handlers = new Map<string, Handler>();
  let shown: Shown[] = [];
  /** Every showNotification call, in order. */
  const log: Shown[] = [];
  const fetched: any[] = [];
  const stores = new Map<string, Map<string, string>>();
  let calls = 0;
  const registration = {
    showNotification(title: string, o: any) {
      shown = shown.filter((n) => n.tag !== o.tag);
      const n = { title, tag: o.tag, body: o.body, data: o.data, silent: o.silent };
      shown.push(n);
      log.push(n);
      return Promise.resolve();
    },
    getNotifications({ tag }: { tag: string }) {
      // What is shown when asked, answered after a delay (as a busy browser would).
      const snapshot = shown.filter((n) => n.tag === tag).map((n) => ({
        ...n,
        close: () => void (shown = shown.filter((m) => m.tag !== tag || m.title !== n.title)),
      }));
      const delay = opts.getNotificationsDelay?.(calls++) ?? 0;
      return new Promise((r) => setTimeout(() => r(snapshot), delay));
    },
    pushManager: {
      getSubscription: () => Promise.resolve(opts.subscription === undefined ? null : opts.subscription),
      subscribe: () => Promise.reject(new Error("not in tests")),
    },
  };
  const self = {
    addEventListener: (type: string, fn: Handler) => handlers.set(type, fn),
    registration,
    clients: { matchAll: () => Promise.resolve([]), claim: () => Promise.resolve() },
    skipWaiting: () => Promise.resolve(),
    location: { origin: "https://box.ts.net" },
  };
  const caches = {
    open(name: string) {
      const m = stores.get(name) ?? new Map<string, string>();
      stores.set(name, m);
      return Promise.resolve({
        match: (k: string) => Promise.resolve(m.has(k) ? new Response(m.get(k)) : undefined),
        put: async (k: string, r: Response) => void m.set(k, await r.text()),
        delete: (k: string) => Promise.resolve(m.delete(k)),
        addAll: () => Promise.resolve(),
      });
    },
    keys: () => Promise.resolve([...stores.keys()]),
    delete: (k: string) => Promise.resolve(stores.delete(k)),
  };
  const fetchFake = (_url: string, init: any) => {
    const body = JSON.parse(init.body);
    fetched.push(body);
    return opts.fetch ? opts.fetch(body) : Promise.resolve(new Response(JSON.stringify({ id: 1, result: {} })));
  };
  new Function("self", "caches", "fetch", SOURCE)(self, caches, fetchFake);
  /** Dispatch an event; resolves when everything it handed to waitUntil settled. */
  const dispatch = async (type: string, fields: Record<string, unknown>) => {
    const waits: Promise<unknown>[] = [];
    handlers.get(type)!({ ...fields, waitUntil: (p: Promise<unknown>) => waits.push(p) });
    await Promise.allSettled(waits);
  };
  const push = (alert: unknown) => dispatch("push", { data: { json: () => alert } });
  const pending = () => stores.get("muster-push")?.get("/m/push-pending.json") ?? null;
  /** The user tapped or the page closed a tag's notification. */
  const close = (tag: string) => void (shown = shown.filter((n) => n.tag !== tag));
  return { dispatch, push, shown: () => shown, log, close, fetched, pending };
}

const alert = (state_since: number, at: number, title: string) => ({
  v: 1,
  kind: "session",
  session_id: 7,
  state_since,
  at,
  title,
  body: title,
});

Deno.test("the worker answers the page's alerts handshake", async () => {
  const w = worker();
  const replies: any[] = [];
  await w.dispatch("message", { data: { type: "muster-sw-hello" }, ports: [{ postMessage: (m: any) => replies.push(m) }] });
  equal(replies.length, 1);
  equal(replies[0].alerts >= 1, true);
});

Deno.test("two pushes for one session at once: the newer stays shown", async () => {
  // The newer alert's lookup answers at once, the older one's late: without one check-and-show at
  // a time per session, the older would see nothing shown and replace the newer.
  const w = worker({ getNotificationsDelay: (call) => (call === 0 ? 1 : 30) });
  await Promise.all([w.push(alert(2000, 2, "newer")), w.push(alert(1000, 1, "older"))]);
  const shown = w.shown().filter((n) => n.tag === "s7");
  equal(shown.length, 1);
  equal(shown[0].title, "newer");
  // And in order, a newer one does replace an older one.
  await w.push(alert(3000, 3, "newest"));
  equal(w.shown().find((n) => n.tag === "s7")?.title, "newest");
});

Deno.test("a renewed subscription the daemon couldn't take is kept and sent later", async () => {
  let up = false;
  const sub = {
    endpoint: "https://fcm.googleapis.com/fcm/send/new",
    toJSON: () => ({ keys: { p256dh: "P", auth: "A" } }),
  };
  const w = worker({
    subscription: sub,
    fetch: () =>
      up
        ? Promise.resolve(new Response(JSON.stringify({ id: 1, result: { subscription: null } })))
        : Promise.reject(new TypeError("network down")),
  });
  await w.dispatch("pushsubscriptionchange", { newSubscription: sub, oldSubscription: null });
  equal(w.fetched.length, 1, "tried at once");
  equal(JSON.parse(w.pending()!).endpoint, sub.endpoint, "kept while the daemon is out of reach");
  // The next time the worker runs (a push, or the page asking), it tries again.
  up = true;
  await w.push(alert(1, 1, "x"));
  equal(w.fetched.length, 2);
  equal(w.fetched[1].method, "push.subscribe");
  equal(w.fetched[1].params, { endpoint: sub.endpoint, p256dh: "P", auth: "A" });
  equal(w.pending(), null, "handed over");
});

Deno.test("a kept renewal waits out 5xx, is dropped once the daemon answered or the browser moved on", async () => {
  let status = 503;
  const sub = { endpoint: "https://fcm.googleapis.com/fcm/send/n", toJSON: () => ({ keys: {} }) };
  const w = worker({ subscription: sub, fetch: () => Promise.resolve(new Response("{}", { status })) });
  await w.dispatch("pushsubscriptionchange", { newSubscription: sub });
  equal(w.pending() !== null, true, "a restarting daemon: kept");
  status = 401;
  await w.dispatch("message", { data: { type: "muster-push-retry" } });
  equal(w.pending(), null, "refused for good: dropped");

  const gone = worker({ subscription: { endpoint: "https://fcm.googleapis.com/fcm/send/other" } });
  await gone.dispatch("pushsubscriptionchange", { newSubscription: sub });
  equal(gone.pending(), null, "the browser no longer has it: nothing to send");
  equal(gone.fetched.length, 0);
});

Deno.test("a late older alert is dropped even after the newer one was closed", async () => {
  const w = worker();
  await w.push(alert(2000, 2, "newer"));
  w.close("s7"); // tapped, or the session was opened on the phone
  await w.push(alert(1000, 1, "older, delayed"));
  equal(w.shown().filter((n) => n.tag === "s7").length, 0, "nothing back on screen");
  equal(w.log.filter((n) => n.title === "older, delayed").length, 0, "the older one never shown");
  equal(w.log.slice(1).every((n) => n.silent), true, "no sound for the dropped one");
  // The same alert delivered twice: shown once.
  await w.push(alert(2000, 2, "newer"));
  equal(w.log.filter((n) => n.title === "newer" && !n.silent).length, 1);
  // A newer one still shows.
  await w.push(alert(3000, 3, "newest"));
  equal(w.shown().find((n) => n.tag === "s7")?.title, "newest");
});

Deno.test("a renewal is dropped only once the daemon took it or refused it for good", async () => {
  const sub = { endpoint: "https://fcm.googleapis.com/fcm/send/n", toJSON: () => ({ keys: { p256dh: "P", auth: "A" } }) };
  let answer: Response = new Response("");
  const w = worker({ subscription: sub, fetch: () => Promise.resolve(answer) });
  const rpcError = (message: string) => new Response(JSON.stringify({ id: 1, error: { message } }));
  answer = rpcError("database is locked");
  await w.dispatch("pushsubscriptionchange", { newSubscription: sub });
  equal(w.pending() !== null, true, "a daemon error (HTTP 200) that isn't a refusal: kept");
  answer = rpcError("unknown method: push.subscribe");
  await w.dispatch("message", { data: { type: "muster-push-retry" } });
  equal(w.pending() !== null, true, "a backend without alerts: kept");
  answer = new Response("not json");
  await w.dispatch("message", { data: { type: "muster-push-retry" } });
  equal(w.pending() !== null, true, "an unreadable answer: kept");
  answer = rpcError("subscription refused: push endpoint has characters a URL can't have");
  await w.dispatch("message", { data: { type: "muster-push-retry" } });
  equal(w.pending(), null, "refused for good: dropped");
});
