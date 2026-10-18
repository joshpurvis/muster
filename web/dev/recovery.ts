// Driven only by scripts/backend-smoke.py --browser, against its private instance.
// A fresh Chromium profile renders the actual production UI and stays open while
// the fixture kills and replaces its own backend. No real agents are launched.
import { launch } from "./cdp.ts";

const [url, sidText, base, portText] = Deno.args;
if (!base?.startsWith("/tmp/muster-backend-smoke-") || new URL(url).hostname !== "127.0.0.1") {
  throw new Error("recovery checks require the private backend smoke fixture");
}
const sid = Number(sidText);
const browser = await launch(Number(portText), `${base}/chromium`);
const M = "globalThis.__muster";
const reader = Deno.stdin.readable.pipeThrough(new TextDecoderStream()).getReader();
let buffered = "";
async function command(): Promise<string | null> {
  while (!buffered.includes("\n")) {
    const next = await reader.read();
    if (next.done) return null;
    buffered += next.value;
  }
  const newline = buffered.indexOf("\n");
  const line = buffered.slice(0, newline);
  buffered = buffered.slice(newline + 1);
  return JSON.parse(line).phase;
}

/** Click the first `selector` element whose text includes `text`. */
async function clickText(selector: string, text: string, timeoutMs = 10000) {
  const find = `[...document.querySelectorAll(${JSON.stringify(selector)})]
    .find((e) => e.textContent.includes(${JSON.stringify(text)}) && !e.disabled)`;
  await browser.wait(`!!(${find})`, timeoutMs);
  const r = await browser.eval(`(() => { const r = (${find}).getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
  await browser.clickAt(r.x, r.y);
}

/** Everything inside `container` stays within it and the viewport (nothing clipped). */
async function contained(container: string) {
  const bad = await browser.eval(`(() => {
    const box = document.querySelector(${JSON.stringify(container)}).getBoundingClientRect();
    const out = [];
    for (const e of document.querySelectorAll(${JSON.stringify(container)} + " button")) {
      const r = e.getBoundingClientRect();
      if (r.left < box.left - 1 || r.right > box.right + 1 || r.right > innerWidth || r.bottom > innerHeight
        || e.scrollWidth > e.clientWidth + 1) out.push(e.textContent.trim());
    }
    if (document.documentElement.scrollWidth > innerWidth) out.push("page overflows horizontally");
    return out;
  })()`);
  if (bad.length) throw new Error(`clipped or overflowing in ${container}: ${bad.join(", ")}`);
}

async function queueSettled() {
  await browser.wait(`${M}.rpc.call("session.queue", { id: ${sid} })
    .then((q) => !q.paused && !q.uncertain && q.queued.length === 0 && q.stage === null)`, 20000);
}

try {
  await Deno.mkdir(`${base}/shots`, { recursive: true });
  await browser.size(1440, 900);
  await browser.goto(url);
  await browser.wait(`${M}?.attached().includes(${sid})`, 15000);
  console.log(JSON.stringify({ phase: "ready" }));
  while (true) {
    const phase = await command();
    if (phase === null || phase === "close") break;
    if (phase === "before") {
      await browser.wait(`${M}.termText(${sid})?.includes("COUNT:2")`, 10000);
      await browser.shot(`${base}/shots/before-backend-replacement.png`);
    } else if (phase === "after") {
      await browser.wait(`${M}.termText(${sid})?.includes("COUNT:5")`, 15000);
      const hello = await browser.eval(`${M}.rpc.call("hello", {})`);
      const session = hello.sessions.find((item: { id: number }) => item.id === sid);
      if (!session?.pid || session.state === "exited") throw new Error("UI lost the surviving terminal");
      await browser.shot(`${base}/shots/after-backend-replacement.png`);
      // Keyboard input must still reach the same terminal after it reconnects.
      await browser.wait(`${M}.termText(${sid})?.includes("COUNT:80")`, 15000);
      await browser.click(".pane .xterm");
      await browser.type("printf 'BROWSER-INPUT-OK\\n'");
      await browser.key("Enter");
      await browser.wait(`${M}.termText(${sid})?.includes("BROWSER-INPUT-OK\\r\\n") ||
        ${M}.termText(${sid})?.split("\\n").some(line => line.trim() === "BROWSER-INPUT-OK")`, 10000);
      await browser.shot(`${base}/shots/reconnected-input.png`);
      const exceptions = browser.console.filter((line) => line.startsWith("exception:"));
      if (exceptions.length) throw new Error(exceptions.join("\n"));
      console.log(JSON.stringify({ phase, pid: session.pid }));
      continue;
    } else if (phase === "mobile") {
      const pairing = await browser.eval(`${M}.rpc.call("device.pair_code", {
        url: "https://muster-fixture.invalid", name: "Recovery fixture", scopes: ["view", "interact"]
      })`);
      await browser.size(412, 915, true);
      await browser.goto(`${new URL(url).origin}/m/#pair=${pairing.code}`);
      await browser.wait(`!!document.querySelector('[data-session="${sid}"] .session-open')`, 15000);
      await browser.click(`[data-session="${sid}"] .session-open`);
      await browser.wait("!!document.querySelector('.input-row textarea')");
      await browser.click(".input-row textarea");
      await browser.type("Keep this unsent draft across backend replacement");
      await browser.shot(`${base}/shots/mobile-before-backend-replacement.png`);
    } else if (phase === "mobile_offline") {
      await browser.wait("!!document.querySelector('.connection-banner')", 10000);
      if (!await browser.eval("document.querySelector('.send-button').disabled")) {
        throw new Error("mobile send remained enabled during the backend outage");
      }
      await browser.shot(`${base}/shots/mobile-backend-offline.png`);
    } else if (phase === "mobile_after") {
      await browser.wait("!document.querySelector('.connection-banner') && !!document.querySelector('.send-button:not(:disabled)')", 15000);
      if (!await browser.eval("document.querySelector('.input-row textarea').value === 'Keep this unsent draft across backend replacement'")) {
        throw new Error("mobile composer lost its unsent draft");
      }
      await browser.click(".select-toggle");
      await browser.wait("document.querySelector('.selection-text')?.textContent.includes('MOBILE-DOWNTIME-OK')", 10000);
      await browser.shot(`${base}/shots/mobile-reconnected-output.png`);
      await browser.click(".select-toggle");
      await browser.shot(`${base}/shots/mobile-reconnected-draft.png`);
      const response = await browser.eval(`fetch('/api/rpc', {method:'POST',
        headers: {'Content-Type':'application/json','X-Muster':'1'},
        body: JSON.stringify({id:1,method:'hello',params:{}})}).then(r => r.json())`);
      const session = response.result.sessions.find((item: { id: number }) => item.id === sid);
      const exceptions = browser.console.filter((line) => line.startsWith("exception:"));
      if (exceptions.length) throw new Error(exceptions.join("\n"));
      console.log(JSON.stringify({ phase, pid: session.pid }));
      continue;
    } else if (phase === "review_desktop") {
      // A paused delivery with prompts waiting behind it: review it from the tab menu.
      await browser.wait(`!!document.querySelector('[data-tab-sid="${sid}"]')`, 15000);
      await browser.wait(`${M}.rpc.call("session.queue", { id: ${sid} }).then((q) => q.paused && q.queued.length === 2)`, 15000);
      await browser.click(`[data-tab-sid="${sid}"]`, { button: "right" });
      await clickText(".menu-item", "Review queued prompts");
      await browser.wait("!!document.querySelector('.confirm-dlg')");
      await contained(".confirm-dlg");
      await browser.shot(`${base}/shots/review-desktop.png`);
      await clickText(".confirm-dlg button", "Resume queue");
      await browser.wait("!document.querySelector('.confirm-dlg')");
      await queueSettled();
      await browser.shot(`${base}/shots/review-desktop-resumed.png`);
    } else if (phase === "review_mobile") {
      const pairing = await browser.eval(`${M}.rpc.call("device.pair_code", {
        url: "https://muster-fixture.invalid", name: "Queue review fixture", scopes: ["view", "interact"]
      })`);
      await browser.size(412, 915, true);
      await browser.goto(`${new URL(url).origin}/m/#pair=${pairing.code}`);
      await browser.wait(`!!document.querySelector('[data-session="${sid}"] .session-open')`, 15000);
      await browser.click(`[data-session="${sid}"] .session-open`);
      await clickText(".composer-handle button", "Review queue", 15000);
      await browser.wait("[...document.querySelectorAll('.approval-actions button')].some((b) => b.textContent.includes('Resume queue'))");
      await contained(".approval-prompt");
      await browser.shot(`${base}/shots/review-mobile.png`);
      await clickText(".approval-actions button", "Resume queue");
      await browser.wait("![...document.querySelectorAll('.approval-actions button')].some((b) => b.textContent.includes('Resume queue'))");
      await browser.shot(`${base}/shots/review-mobile-resumed.png`);
      const exceptions = browser.console.filter((line) => line.startsWith("exception:"));
      if (exceptions.length) throw new Error(exceptions.join("\n"));
    } else {
      throw new Error(`unknown recovery phase: ${phase}`);
    }
    console.log(JSON.stringify({ phase }));
  }
} finally {
  await browser.close();
  await reader.cancel();
  reader.releaseLock();
}
