// Cold prompt cache checks for the mock e2e walkthrough (dev/e2e.ts calls `cacheChecks`): the
// thin bar under an idle agent's terminal, its dismissal for one idle period, Start fresh, the
// "cold" markers on the tab and the agents row, and the Cache warning settings. Also runs alone
// against the mock:
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_cache.ts http://127.0.0.1:17761/#token=dev <shots-dir>
import { type Browser, launch } from "./cdp.ts";

export interface CacheCtx {
  shots: string;
  ok: (msg: string) => void;
  check: (cond: unknown, msg: string) => void;
}

const M = "globalThis.__muster";
const MIN = 60_000;

export async function cacheChecks(a: Browser, { shots, ok, check }: CacheCtx) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const shot = async (name: string, sel?: string, pad = 0) => {
    await a.sleep(300);
    const r = sel ? await a.rect(sel) : null;
    await a.shot(
      `${shots}/${name}.png`,
      r ? { x: Math.max(0, r.x - pad), y: Math.max(0, r.y - pad), w: r.w + 2 * pad, h: r.h + 2 * pad } : undefined,
    );
  };
  const bar = `document.querySelector('.cache-bar')`;
  const prefs = await R("prefs.get", {});
  check(prefs.cache_warn === true && prefs.cache_ttl?.claude === 60, "the backend's defaults: on, Claude 1 h");

  // A Claude agent with a tab here, idle.
  if (!(await a.eval(`${M}.agentsPanelOpen.value`))) await a.key("Alt+G");
  await a.wait(`!!document.querySelector('[data-agent="digest"]')`, 5000);
  await a.click(`[data-agent="digest"]`);
  await a.wait(`(() => { const ag = ${M}.agents.value.find((x) => x.name === "digest");
    const s = ag && ${M}.sessions.value.get(ag.session_id); return !!s && s.state !== "starting" && s.state !== "exited"; })()`, 8000);
  const sid: number = await a.eval(`${M}.agents.value.find((x) => x.name === "digest").session_id`);
  /** The mock agent's own first turn (dev/mock.ts fakeAgent) is over: idle, with a turn seen. */
  const settled = () =>
    a.wait(`(() => { const s = ${M}.sessions.value.get(${sid}); return s?.state === "idle" && s.last_turn_at != null; })()`, 8000);
  // A new conversation (a run against a mock an earlier run left cold starts clean).
  await R("session.restart", { id: sid });
  await a.wait(`${M}.sessions.value.get(${sid})?.last_turn_at == null`, 3000);
  await settled();
  await a.wait(`${M}.focusedSession.value?.id === ${sid}`, 3000);
  check(!(await a.eval(`!!${bar}`)), "no bar right after a turn");

  // ---- idle 1 h 20 m past a 1 h cache: the bar, by the input box, without taking the keyboard
  await a.click(`.pane-body .term-host`);
  const focusBefore = await a.eval(`document.activeElement?.className ?? ""`);
  await R("mock.cache_idle", { id: sid, idle_ms: 80 * MIN });
  await a.wait(`!!${bar}`, 3000);
  const text: string = await a.eval(`${bar}.textContent`);
  check(text.includes("Idle 1 h 20 m. The prompt cache has likely expired"), `bar text: ${text}`);
  check(await a.eval(`${bar}.previousElementSibling?.matches('.term-host')`), "the bar sits right under the terminal");
  check((await a.eval(`document.activeElement?.className ?? ""`)) === focusBefore, "the bar never takes focus");
  check(!(await a.eval(`!!document.activeElement?.closest('.cache-bar')`)), "focus is not in the bar");
  check(await a.eval(`!!document.querySelector('[data-tab-sid="${sid}"] .tab-cold')`), "the tab says cold");
  check(await a.eval(`!!document.querySelector('[data-agent="digest"] .ap-cold')`), "the agents row says cold");
  check(await a.eval(`!!${bar}.querySelector('.link-btn')`), "a named agent's bar offers Start fresh");
  await shot("bar-window");
  await shot("bar", `.pane:has(.cache-bar) .pane-body`, 0);
  await shot("agents-row", `[data-agent="digest"]`, 6);
  await shot("tab", `[data-tab-sid="${sid}"]`, 6);
  ok("an agent idle past its cache lifetime shows the cold bar under its terminal, and cold on its tab and row");

  // ---- dismissed for this idle period; back after the next turn goes cold too
  await a.click(`.cache-bar .cache-x`);
  await a.wait(`!${bar}`, 2000);
  check(!(await a.eval(`!!document.activeElement?.closest('.cache-bar')`)), "dismissing leaves focus alone");
  check(await a.eval(`!!document.querySelector('[data-tab-sid="${sid}"] .tab-cold')`), "the tab marker stays");
  // Kept per viewer, so a reload keeps it (not reloaded here: the walkthrough's pages hold state).
  check(
    await a.eval(`(localStorage.getItem("muster.coldDismissed") ?? "").includes(${sid} + ":")`),
    "the dismissal is kept in this browser",
  );
  await R("mock.cache_idle", { id: sid, idle_ms: 95 * MIN });
  await a.wait(`!!${bar}`, 3000);
  check((await a.eval(`${bar}.textContent`)).includes("Idle 1 h 35 m"), "a new idle period brings it back");
  ok("dismissing hides the bar for that idle period only (kept in this browser)");

  // ---- Start fresh: a new conversation, nothing cached to lose
  // The button is on top (the terminal's canvases can overhang the bar until xterm refits).
  const at = await a.rect(`.cache-bar .link-btn`);
  check(
    await a.eval(`!!document.elementFromPoint(${at!.x + at!.w / 2}, ${at!.y + at!.h / 2})?.closest('.cache-bar')`),
    "nothing covers the bar",
  );
  await a.click(`.cache-bar .link-btn`);
  await a.wait(`!${bar} && ${M}.sessions.value.get(${sid})?.cache_cold_at == null`, 8000);
  await settled();
  ok("Start fresh restarts the agent on a new conversation and the bar goes");

  // ---- settings: the lifetimes and the switch
  await R("mock.cache_idle", { id: sid, idle_ms: 80 * MIN });
  await a.wait(`!!${bar}`, 3000);
  await a.key("Ctrl+Shift+P");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("Cache warning");
  await a.sleep(150);
  await a.key("Enter");
  await a.wait(`!!document.querySelector('.cache-dlg')`, 2000);
  check((await a.eval(`document.querySelector('.cache-ttl-claude').value`)) === "60", "Claude: 60 minutes");
  check((await a.eval(`document.querySelector('.cache-ttl-codex_openrouter').value`)) === "10", "Codex on OpenRouter: 10");
  await shot("settings", `.cache-dlg`, 8);
  const setValue = (sel: string, v: string) =>
    a.eval(`(() => { const i = document.querySelector(${JSON.stringify(sel)});
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(i, ${JSON.stringify(v)});
      i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
  await setValue(".cache-ttl-claude", "0");
  check(await a.eval(`document.querySelector('.cache-dlg button[type=submit]').disabled`), "0 minutes can't be saved");
  await setValue(".cache-ttl-claude", "120");
  await a.click(`.cache-dlg button[type=submit]`);
  await a.wait(`!document.querySelector('.cache-dlg')`, 3000);
  await a.wait(`!${bar} && !document.querySelector('[data-tab-sid="${sid}"] .tab-cold')`, 3000);
  check((await R("prefs.get", {})).cache_ttl.claude === 120, "saved on the backend");
  ok("a longer Claude cache lifetime (2 h) makes an agent idle 1 h 20 m warm again");

  await R("prefs.set", { cache_ttl: { claude: 60 } });
  await a.wait(`!!${bar}`, 3000);
  await R("prefs.set", { cache_warn: false });
  await a.wait(`!${bar} && !document.querySelector('.ap-cold')`, 3000);
  await R("prefs.set", { cache_warn: true });
  await a.wait(`!!${bar}`, 3000);
  ok("switching the warning off hides the bar and the markers everywhere");
  // Leave it as the rest of the walkthrough expects: warm, defaults.
  await R("session.restart", { id: sid });
  await a.wait(`!${bar}`, 3000);
}

if (import.meta.main) {
  const [url, shots] = Deno.args;
  if (!url || !shots) {
    console.error("usage: e2e_cache.ts <url#token=..> <shots-dir>");
    Deno.exit(2);
  }
  let step = 0;
  const a = await launch(9351, "/tmp/muster-cdp-cache");
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("runs against the mock daemon only");
    await cacheChecks(a, {
      shots,
      ok: (m) => console.log(`✓ ${++step}. ${m}`),
      check: (c, m) => {
        if (!c) throw new Error("check failed: " + m);
      },
    });
  } finally {
    await a.close();
  }
}
