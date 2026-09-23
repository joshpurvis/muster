// Tiny Chrome DevTools Protocol driver for headless UI checks (dev only).
// Permissions: --allow-net=127.0.0.1 --allow-run=chromium --allow-write=<shots dir>

export interface Browser {
  send<T = any>(method: string, params?: Record<string, unknown>): Promise<T>;
  eval<T = any>(expr: string): Promise<T>;
  goto(url: string): Promise<void>;
  size(w: number, h: number): Promise<void>;
  shot(path: string): Promise<void>;
  key(combo: string): Promise<void>;
  type(text: string): Promise<void>;
  click(sel: string, opts?: { button?: "left" | "middle" | "right"; count?: number }): Promise<void>;
  clickAt(
    x: number,
    y: number,
    opts?: { button?: "left" | "middle" | "right"; count?: number },
  ): Promise<void>;
  drag(from: [number, number], to: [number, number], steps?: number): Promise<void>;
  rect(sel: string): Promise<{ x: number; y: number; w: number; h: number } | null>;
  wait(cond: string, timeoutMs?: number): Promise<void>;
  sleep(ms: number): Promise<void>;
  console: string[];
  close(): Promise<void>;
}

const KEYS: Record<string, { key: string; code: string; vk: number }> = {
  Enter: { key: "Enter", code: "Enter", vk: 13 },
  Escape: { key: "Escape", code: "Escape", vk: 27 },
  Tab: { key: "Tab", code: "Tab", vk: 9 },
  Backspace: { key: "Backspace", code: "Backspace", vk: 8 },
  Left: { key: "ArrowLeft", code: "ArrowLeft", vk: 37 },
  Up: { key: "ArrowUp", code: "ArrowUp", vk: 38 },
  Right: { key: "ArrowRight", code: "ArrowRight", vk: 39 },
  Down: { key: "ArrowDown", code: "ArrowDown", vk: 40 },
  F1: { key: "F1", code: "F1", vk: 112 },
  "/": { key: "/", code: "Slash", vk: 191 },
  "[": { key: "[", code: "BracketLeft", vk: 219 },
  "]": { key: "]", code: "BracketRight", vk: 221 },
  "=": { key: "=", code: "Equal", vk: 187 },
  "-": { key: "-", code: "Minus", vk: 189 },
};

function keyInfo(k: string) {
  if (KEYS[k]) return KEYS[k];
  if (/^[a-zA-Z]$/.test(k)) {
    return { key: k.toLowerCase(), code: "Key" + k.toUpperCase(), vk: k.toUpperCase().charCodeAt(0) };
  }
  if (/^[0-9]$/.test(k)) return { key: k, code: "Digit" + k, vk: k.charCodeAt(0) };
  throw new Error("unknown key " + k);
}

export async function launch(port = 9333, userDir = "/tmp/muster-cdp"): Promise<Browser> {
  const proc = new Deno.Command("chromium", {
    args: [
      "--headless=new",
      "--disable-gpu",
      `--remote-debugging-port=${port}`,
      `--user-data-dir=${userDir}`,
      "--no-first-run",
      "--window-size=1440,900",
      "about:blank",
    ],
    stdout: "null",
    stderr: "null",
  }).spawn();
  let wsUrl = "";
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((t: any) => t.type === "page");
      if (page) {
        wsUrl = page.webSocketDebuggerUrl;
        break;
      }
    } catch { /* not up yet */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!wsUrl) throw new Error("chromium did not start");
  const ws = new WebSocket(wsUrl);
  await new Promise((r, j) => {
    ws.onopen = r;
    ws.onerror = j;
  });
  let id = 0;
  const pending = new Map<number, { res: (v: any) => void; rej: (e: Error) => void }>();
  const consoleLines: string[] = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) {
      const p = pending.get(m.id)!;
      pending.delete(m.id);
      if (m.error) p.rej(new Error(m.error.message));
      else p.res(m.result);
    } else if (m.method === "Runtime.consoleAPICalled") {
      consoleLines.push(
        m.params.type + ": " + m.params.args.map((a: any) => a.value ?? a.description).join(" "),
      );
    } else if (m.method === "Runtime.exceptionThrown") {
      consoleLines.push(
        "exception: " + (m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text),
      );
    }
  };
  const send = <T>(method: string, params: Record<string, unknown> = {}) =>
    new Promise<T>((res, rej) => {
      const i = ++id;
      pending.set(i, { res, rej });
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  await send("Runtime.enable");
  await send("Page.enable");
  let W = 1440, H = 900;
  const mods = { Alt: 1, Ctrl: 2, Meta: 4, Shift: 8 } as const;
  const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  const b: Browser = {
    send,
    console: consoleLines,
    async eval(expr) {
      const r: any = await send("Runtime.evaluate", {
        expression: expr,
        returnByValue: true,
        awaitPromise: true,
      });
      if (r.exceptionDetails) {
        throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
      }
      return r.result.value;
    },
    async goto(url) {
      await send("Page.navigate", { url });
      await sleep(600);
    },
    async size(w, h) {
      W = w;
      H = h;
      await send("Emulation.setDeviceMetricsOverride", {
        width: w,
        height: h,
        deviceScaleFactor: 1,
        mobile: false,
      });
    },
    async shot(path) {
      const r: any = await send("Page.captureScreenshot", {
        format: "png",
        clip: { x: 0, y: 0, width: W, height: H, scale: 1 },
      });
      await Deno.writeFile(path, Uint8Array.from(atob(r.data), (c) => c.charCodeAt(0)));
    },
    async key(combo) {
      const parts = combo.split("+");
      const k = keyInfo(parts.pop()!);
      let m = 0;
      for (const p of parts) m |= mods[p as keyof typeof mods];
      const text = m & ~mods.Shift
        ? undefined
        : (k.key.length === 1
          ? (m & mods.Shift ? k.key.toUpperCase() : k.key)
          : k.key === "Enter"
          ? "\r"
          : undefined);
      await send("Input.dispatchKeyEvent", {
        type: text ? "keyDown" : "rawKeyDown",
        modifiers: m,
        key: m & mods.Shift && k.key.length === 1 ? k.key.toUpperCase() : k.key,
        code: k.code,
        windowsVirtualKeyCode: k.vk,
        text,
        unmodifiedText: text,
      });
      await send("Input.dispatchKeyEvent", {
        type: "keyUp",
        modifiers: m,
        key: k.key,
        code: k.code,
        windowsVirtualKeyCode: k.vk,
      });
      await sleep(60);
    },
    async type(text) {
      await send("Input.insertText", { text });
      await sleep(40);
    },
    async rect(sel) {
      return await b.eval(
        `(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null;
        const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; })()`,
      );
    },
    async clickAt(x, y, opts = {}) {
      const button = opts.button ?? "left";
      const clickCount = opts.count ?? 1;
      await send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
      for (let c = 1; c <= clickCount; c++) {
        await send("Input.dispatchMouseEvent", {
          type: "mousePressed",
          x,
          y,
          button,
          clickCount: c,
          buttons: button === "left" ? 1 : button === "right" ? 2 : 4,
        });
        await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button, clickCount: c });
      }
      await sleep(80);
    },
    async click(sel, opts) {
      const r = await b.rect(sel);
      if (!r) throw new Error("no element " + sel);
      await b.clickAt(r.x + r.w / 2, r.y + r.h / 2, opts);
    },
    async drag(from, to, steps = 12) {
      await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: from[0], y: from[1] });
      await send("Input.dispatchMouseEvent", {
        type: "mousePressed",
        x: from[0],
        y: from[1],
        button: "left",
        clickCount: 1,
        buttons: 1,
      });
      for (let i = 1; i <= steps; i++) {
        const x = from[0] + (to[0] - from[0]) * i / steps;
        const y = from[1] + (to[1] - from[1]) * i / steps;
        await send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, button: "left", buttons: 1 });
        await sleep(16);
      }
      await sleep(60);
      await send("Input.dispatchMouseEvent", {
        type: "mouseReleased",
        x: to[0],
        y: to[1],
        button: "left",
        clickCount: 1,
      });
      await sleep(120);
    },
    async wait(cond, timeoutMs = 5000) {
      const t0 = Date.now();
      while (Date.now() - t0 < timeoutMs) {
        try {
          if (await b.eval(cond)) return;
        } catch { /* retry */ }
        await sleep(100);
      }
      throw new Error("timeout waiting for: " + cond);
    },
    sleep,
    async close() {
      try {
        ws.close();
      } catch { /* ignore */ }
      try {
        proc.kill("SIGTERM");
      } catch { /* ignore */ }
      await proc.status;
    },
  };
  return b;
}
