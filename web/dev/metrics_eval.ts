// Controlled browser half of scripts/eval.py; never connects to a user's profile or daemon.
import { launch } from "./cdp.ts";
const [url, base, port] = Deno.args;
if (!base?.startsWith("/tmp/muster-eval-") || new URL(url).hostname !== "127.0.0.1") {
  throw new Error("requires the isolated eval fixture");
}
const browser = await launch(Number(port), `${base}/chromium`);
const M = "globalThis.__muster";
const reader = Deno.stdin.readable.pipeThrough(new TextDecoderStream()).getReader();
let buffered = "";
async function command(): Promise<any> {
  while (!buffered.includes("\n")) {
    const next = await reader.read();
    if (next.done) return null;
    buffered += next.value;
  }
  const end = buffered.indexOf("\n");
  const line = buffered.slice(0, end);
  buffered = buffered.slice(end + 1);
  return JSON.parse(line);
}
try {
  await browser.goto(url);
  await browser.wait(`${M}?.rpc.isOpen`, 15000);
  console.log(JSON.stringify({ ready: true, browser: await browser.send("Browser.getVersion") }));
  while (true) {
    const cmd = await command();
    if (!cmd || cmd.phase === "close") break;
    let result: any = {};
    if (cmd.phase === "typing") {
      await browser.eval(`${M}.activeChannelId.value = ${cmd.channel}`);
      await browser.wait(
        `${M}.attached().includes(${cmd.sid}) && ${M}.termText(${cmd.sid}).includes("EVAL-READY")`,
      );
      await browser.eval(`${M}.term(${cmd.sid}).focus()`);
      const samples: number[] = [];
      const idle: number[] = [];
      let failures = 0;
      for (let i = 0; i < cmd.keys; i++) {
        const expected = `EVAL${cmd.start + i + 1}:x`;
        if (i % 10 === 0) await browser.sleep(1100);
        await browser.eval(`(() => {
          const term = ${M}.term(${cmd.sid});
          const expected = ${JSON.stringify(expected)};
          const state = globalThis.__echo = { start: null, ms: null };
          const key = () => { state.start = performance.now(); };
          term.textarea.addEventListener("keydown", key, {once: true, capture: true});
          const render = term.onRender(() => {
            if (state.start !== null && ${M}.termText(${cmd.sid}).split("\\n").some(l => l.trim() === expected)) {
              render.dispose();
              requestAnimationFrame(() => { state.ms = performance.now() - state.start; });
            }
          });
          globalThis.__echoCleanup = () => { render.dispose(); term.textarea.removeEventListener("keydown", key); };
        })()`);
        await browser.key("x");
        try {
          await browser.wait("globalThis.__echo.ms !== null", 5000);
          const ms = await browser.eval<number>("globalThis.__echo.ms");
          (i % 10 === 0 ? idle : samples).push(ms);
        } catch {
          failures++;
        }
        await browser.eval("globalThis.__echoCleanup()");
      }
      result = { samples, idle, failures, attempts: cmd.keys };
    } else if (cmd.phase === "before_restart") {
      await browser.eval("globalThis.__recoveryStart = performance.now()");
    } else if (cmd.phase === "recovered") {
      await browser.wait(
        `${M}.rpc.isOpen && ${M}.termText(${cmd.sid})?.includes(${JSON.stringify(cmd.marker)})`,
        20000,
      );
      result.ms = await browser.eval("performance.now() - globalThis.__recoveryStart");
      const lines = await browser.eval<string[]>(
        `${M}.termText(${cmd.sid}).split("\\n").map(l => l.trim()).filter(l => /^EVAL[0-9]+:[xy]$/.test(l))`,
      );
      result.lines = lines;
    } else if (cmd.phase === "flush") {
      await browser.sleep(15500);
      result.errors = browser.console.filter((line) => line.startsWith("exception:"));
    } else throw new Error(`unknown phase: ${cmd.phase}`);
    console.log(JSON.stringify({ phase: cmd.phase, ...result }));
  }
} finally {
  await browser.close();
  await reader.cancel();
  reader.releaseLock();
}
