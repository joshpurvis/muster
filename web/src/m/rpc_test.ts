// Exercise the shared transport with both auth modes; no network or browser needed.
function assert(value: unknown, message: string) {
  if (!value) throw new Error(message);
}

async function harness(path: string) {
  const saved = new Map<string, PropertyDescriptor | undefined>();
  const set = (name: string, value: unknown) => {
    saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
    Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
  };
  let reads = 0;
  const sockets: Socket[] = [];
  class Socket {
    static OPEN = 1;
    static CLOSED = 3;
    static CLOSING = 2;
    readyState = 0;
    binaryType = "";
    onopen = () => {};
    onclose = () => {};
    onmessage = (_e: { data: string }) => {};
    sent: { id: number; method: string; params: unknown }[] = [];
    constructor(readonly url: string) {
      sockets.push(this);
    }
    send(frame: string) {
      this.sent.push(JSON.parse(frame));
    }
    close() {
      this.readyState = Socket.CLOSED;
      this.onclose();
    }
    async open() {
      this.readyState = Socket.OPEN;
      this.onopen();
      this.reply(this.sent.at(-1)!.id, {});
      await Promise.resolve();
      await Promise.resolve();
    }
    reply(id: number, result: unknown) {
      this.onmessage({ data: JSON.stringify({ id, result }) });
    }
  }
  set("location", { pathname: path, protocol: "https:", host: "muster.test", hash: "", search: "" });
  set("sessionStorage", {
    getItem: () => {
      reads++;
      return "desktop-token";
    },
  });
  set("document", { addEventListener: () => {} });
  set("addEventListener", () => {});
  set("WebSocket", Socket);
  const { rpc } =
    await (path === "/"
      ? import("../rpc.ts?desktop-test")
      : path === "/m"
      ? import("../rpc.ts?close-test")
      : import("../rpc.ts?cookie-test"));
  return {
    rpc,
    sockets,
    reads: () => reads,
    restore() {
      rpc.onClosed = undefined;
      rpc.suspend();
      for (const [name, descriptor] of saved) {
        if (descriptor) Object.defineProperty(globalThis, name, descriptor);
        else Reflect.deleteProperty(globalThis, name);
      }
    },
  };
}

Deno.test("mobile cookie auth never reads a token or queues offline approvals", async () => {
  const h = await harness("/m/");
  try {
    await h.rpc.start();
    assert(h.reads() === 0, "cookie mode must not read sessionStorage");
    assert(h.sockets[0].url === "wss://muster.test/ws", "cookie socket must not contain token query");
    const denied = await h.rpc.call("session.approve", { id: 1 }).then(() => false, () => true);
    assert(denied, "offline approval must fail immediately");
    await h.sockets[0].open();
    assert(h.sockets[0].sent.length === 1, "only hello should have been sent");
    const pending = h.rpc.call("session.send", { id: 1, text: "hello" }).then(() => false, () => true);
    h.rpc.suspend();
    assert(await pending, "suspend must reject in-flight calls");
    h.rpc.resume();
    await h.sockets[1].open();
    assert(h.sockets[1].sent.length === 1, "resume must not replay actions");
  } finally {
    h.restore();
  }
});

Deno.test("desktop token auth still flushes calls after hello; suspend rejects queued calls", async () => {
  const h = await harness("/");
  try {
    const pending = h.rpc.call("session.attach", { id: 1 });
    await h.rpc.start();
    assert(h.reads() === 1, "desktop should read its stored token");
    const ws = h.sockets[0];
    assert(ws.url === "wss://muster.test/ws?token=desktop-token", "desktop URL changed");
    assert(ws.sent.length === 0, "queue must wait for hello");
    await ws.open();
    assert(ws.sent[1]?.method === "session.attach", "desktop queue did not flush");
    ws.reply(ws.sent[1].id, { attached: true });
    assert((await pending as { attached: boolean }).attached, "response not delivered");
    ws.close();
    const queued = h.rpc.call("session.approve", { id: 1 }).then(() => false, () => true);
    h.rpc.suspend();
    assert(await queued, "suspend must reject queued calls too");
  } finally {
    h.restore();
  }
});

Deno.test("close hook can suspend an unpaired phone before reconnect is scheduled", async () => {
  const h = await harness("/m");
  try {
    await h.rpc.start();
    await h.sockets[0].open();
    let closed = 0;
    h.rpc.onClosed = () => {
      closed++;
      h.rpc.suspend();
    };
    h.sockets[0].close();
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert(closed === 1 && h.sockets.length === 1, "unpaired phone reconnected");
  } finally {
    h.restore();
  }
});
