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
