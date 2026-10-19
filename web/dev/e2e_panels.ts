// Agent panel checks for the mock e2e walkthrough (dev/e2e.ts calls `panelChecks`). Also runs
// alone against the mock, for quick iteration:
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_panels.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path>
import { type Browser, launch } from "./cdp.ts";

export interface PanelCtx {
  url: string;
  shots: string;
  hello: any;
  ok: (msg: string) => void;
  check: (cond: unknown, msg: string) => void;
}

const M = "globalThis.__muster";

export async function panelChecks(a: Browser, { url, shots, hello, ok, check }: PanelCtx) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const shot = async (name: string) => {
    await a.sleep(300);
    await a.shot(`${shots}/${name}.png`);
  };
  const theme = (value: "light" | "dark") =>
    a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value }] });
  const opener: number = (await a.eval(`${M}.windowSessions.value.map((s) => s.id)`))[0];
  check(opener !== undefined, "panel opener exists");
  const open = async (args: Record<string, unknown>): Promise<number> =>
    (await R("panel.open", { from_session: opener, arguments: args })).panel_id;
  const ready = (pid: number, ms = 8000) =>
    a.wait(`!!document.querySelector('[data-panel-view="${pid}"] iframe') && !document.querySelector('[data-panel-view="${pid}"] .panel-status')`, ms);
  const events = async (pid: number): Promise<any[]> =>
    (await R("panel.wait", { from_session: opener, arguments: { panel_id: pid, timeout_seconds: 0 } })).events;
  const activeEl = () => a.eval(`(() => { const e = document.activeElement; return e?.tagName === 'IFRAME' ? 'panel:' + e.dataset.panelId : e?.closest?.('.term-el') ? 'terminal' : e?.tagName; })()`);

  /** Evaluate inside panel `pid`'s app (the proxy's inner srcdoc frame), in an isolated world. */
  const appOf = async (pid: number) => {
    const tree: any = await a.send("Page.getFrameTree");
    const frames: any[] = [];
    const collect = (n: any) => { frames.push(n.frame); for (const c of n.childFrames ?? []) collect(c); };
    collect(tree.frameTree);
    const proxy = frames.find((f) => f.name === `muster-panel-${pid}`);
    check(proxy?.url.startsWith(hello.panel_origin + "/proxy"), `panel ${pid}: proxy on the second origin: ${JSON.stringify(frames)}`);
    // Chromium puts the opaque srcdoc in its own renderer process on recent builds.
    const targets: any = await a.send("Target.getTargets");
    const child = targets.targetInfos.find((t: any) => t.type === "iframe" && t.parentFrameId === proxy.id && t.url === "about:srcdoc");
    const app = frames.find((f) => f.parentId === proxy.id && f.url === "about:srcdoc");
    check(app || child, `panel ${pid}: app in its inner srcdoc frame`);
    const session = child ? (await a.send("Target.attachToTarget", { targetId: child.targetId, flatten: true })).sessionId : undefined;
    const world: any = await a.send("Page.createIsolatedWorld", { frameId: app?.id ?? child.targetId, worldName: "panel-check" }, session);
    const inApp = async (expression: string): Promise<any> => {
      const r: any = await a.send("Runtime.evaluate", { expression, contextId: world.executionContextId, returnByValue: true, awaitPromise: true }, session);
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
      return r.result.value;
    };
    await inApp(`globalThis.panelRequest = (method, params) => new Promise((resolve, reject) => {
      const id = 'e2e:' + Math.random();
      const timer = setTimeout(() => { removeEventListener('message', listen); reject(new Error('panel RPC timed out')); }, 3000);
      const listen = (e) => { if (e.source === parent && e.data?.id === id) { clearTimeout(timer); removeEventListener('message', listen); resolve(e.data); } };
      addEventListener('message', listen); parent.postMessage({jsonrpc:'2.0',id,method,params}, '*');
    }); true`);
    /** Click at the center of `selector` inside the app (a trusted input event). */
    const click = async (selector: string) => {
      const outer = (await a.rect(`[data-panel-id="${pid}"]`))!;
      await inApp(`document.querySelector(${JSON.stringify(selector)}).scrollIntoView({block:'center'})`);
      const r = await inApp(`(() => { const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return {x:r.x+Math.min(20,r.width/2),y:r.y+r.height/2}; })()`);
      await a.clickAt(outer.x + r.x, outer.y + r.y);
    };
    return { inApp, click };
  };

  const message = (text: string) => `panelRequest('ui/message', {role:'user',content:{type:'text',text:${JSON.stringify(text)}}})`;
  const pid = await open({
    title: "Panel review", app: "markdown",
    data: { markdown: "# Sandbox review\n\n**Safe** text and `<script>` literals, and [a link](https://example.com/docs).\n\n| Check | Result |\n| --- | --- |\n| Bridge | Ready |" },
  });
  const tab = -pid - 1;
  await ready(pid);
  const { inApp, click } = await appOf(pid);
  check(await inApp(`document.querySelector('h1')?.textContent`) === "Sandbox review", "markdown heading renders");
  check(await inApp(`document.querySelectorAll('table tr').length`) === 2, "simple table renders");
  check(await a.eval(`document.querySelector('[data-panel-id="${pid}"]').sandbox.value`) === "allow-scripts allow-same-origin", "outer sandbox grants only script and same-origin permissions");
  check(await inApp(`(() => { try { return top.sessionStorage.getItem('muster.token'); } catch { return 'blocked'; } })()`) === "blocked", "app cannot read host token");
  check(await inApp(`typeof globalThis.__musterNative`) === "undefined", "no native capability in app");
  ok("Markdown panel renders in the separate-origin proxy and opaque sandbox; token stays private");

  await a.wait(`navigator.userActivation.isActive === false`, 8000);
  const denied = await inApp(message("unsolicited"));
  check(denied.error?.message.includes("user action"), "message without activation refused");
  const disallowed = await inApp(`panelRequest('tools/call', {name:'send_message',arguments:{session_id:${opener},text:'bad'}})`);
  check(disallowed.error?.code === -32601, "non-allowlisted tool refused");
  const scoped = await inApp(`panelRequest('tools/call', {name:'get_diff',arguments:{channel_id:9999}})`);
  check(!!scoped.error, "app cannot override task scope");
  const diff = await inApp(`panelRequest('tools/call', {name:'get_diff',arguments:{}})`);
  check(diff.result?.content?.[0]?.text === "Mock diff", "allowlisted get_diff works");
  const blockedFetch = await inApp(`fetch(${JSON.stringify(hello.panel_origin + "/api/rpc")}).then(() => false, () => true)`);
  check(blockedFetch, "fetch blocked by CSP");
  const navigated = await inApp(`(() => { try { top.location.href='https://example.invalid/panel-escape'; return true; } catch { return false; } })()`);
  check(!navigated && (await a.eval(`location.origin`)) === new URL(url).origin, "top navigation blocked");
  ok("bridge rejects unsolicited events and forbidden tools; task override, fetch and top navigation are blocked");

  // ---- user activation is the panel's own, and one click buys only a small budget
  await a.click(`[data-tab-sid="${tab}"] .tab-name`);
  await a.wait(`document.activeElement?.dataset?.panelId === "${pid}"`, 2000);
  check(await a.eval(`navigator.userActivation.isActive`), "the tab click activated the host (the old gate passed here)");
  const fromTab = await inApp(message("after a tab click"));
  check(fromTab.error?.message.includes("user action"), `a tab click and frame focus do not let the app send: ${JSON.stringify(fromTab)}`);
  check((await events(pid)).length === 0, "nothing queued after a tab click");
  await click("h1");
  const burst = [];
  for (let i = 0; i < 6; i++) burst.push(await inApp(message(`poll ${i}`)));
  const passed = burst.filter((r) => r.result !== undefined).length;
  check(passed === 3 && burst.slice(3).every((r) => r.error?.message.includes("user action")), `one click lets exactly 3 messages through: ${JSON.stringify(burst)}`);
  check((await events(pid)).length === 3, "the queue holds exactly the budget");
  await a.sleep(5700);
  check(await inApp(`navigator.userActivation.isActive`) === false, "activation expired");
  check((await inApp(message("after expiry"))).error, "polling after the activation expired is refused");
  ok("a tab click or frame focus never authorizes app events; a click inside the app allows 3, then none until the next action");

  await click("textarea");
  await a.type("Looks good from the sandbox");
  await click("button");
  await a.sleep(250);
  const delivery = await events(pid);
  check(delivery.length === 1 && delivery[0].kind === "ui/message" && delivery[0].payload.content.text === "Looks good from the sandbox", `real click reaches event queue exactly once: ${JSON.stringify(delivery)}`);
  check(await inApp(`document.querySelector('output').textContent`) === "Sent", "app receives event acknowledgement");
  ok("a trusted click inside the app delivers one panel event");

  // ---- shortcuts work while the app has focus; editing keys stay in text fields
  const zoom = () => a.eval(`${M}.layout.value.zoom`);
  await click("p");
  check(await activeEl() === `panel:${pid}`, "focus is inside the panel");
  await a.key("Ctrl+Shift+P");
  await a.wait(`document.querySelector('.sw-mode')?.textContent === 'Commands'`, 2000);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.switcher')`);
  await click("p");
  await a.key("Alt+Z");
  await a.wait(`!!${M}.layout.value.zoom`, 2000);
  await a.key("Alt+Z");
  await a.wait(`!${M}.layout.value.zoom`, 2000);
  await click("textarea");
  const focusBefore = await a.eval(`${M}.layout.value.focus`);
  await a.key("Alt+Left");
  await a.sleep(150);
  check(await a.eval(`${M}.layout.value.focus`) === focusBefore && await activeEl() === `panel:${pid}`, "Alt+Left in the app's text field stays word motion");
  await a.key("Ctrl+Shift+P");
  await a.wait(`document.querySelector('.sw-mode')?.textContent === 'Commands'`, 2000);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.switcher')`);
  check(await zoom() === null, "unzoomed");
  await click("p");
  const panelPane = await a.eval(`${M}.layout.value.focus`);
  await a.key("Alt+Left");
  await a.wait(`${M}.layout.value.focus !== ${JSON.stringify(panelPane)} && !!${M}.focusedSession.value`, 2000);
  await a.wait(`!!document.activeElement?.closest?.('.term-el')`, 2000);
  ok("Ctrl+Shift+P, Alt+Z and Alt+Left work from inside a panel; Alt+arrows in its text field stay with the app");

  // ---- moving the panel between groups keeps the app (and an unsent draft) alive
  await inApp(`globalThis.__alive = true; document.querySelector('textarea').value = 'Unsent review comment'; true`);
  const sigOf = () => a.eval(`JSON.stringify(${M}.layout.value.root, (k, v) => k === 'sizes' || k === 'id' ? undefined : v)`);
  const before = await sigOf();
  const tabRect = (await a.rect(`[data-tab-sid="${tab}"]`))!;
  const strip = (await a.eval(`(() => { const r = document.querySelector('[data-tab-sid="${opener}"]').closest('[data-tabstrip]').getBoundingClientRect(); return {x: r.right - 30, y: r.y + r.height / 2}; })()`));
  await shot("panel-09-draft-before-move");
  await a.drag([tabRect.x + 20, tabRect.y + tabRect.h / 2], [strip.x, strip.y]);
  const sameStrip = `document.querySelector('[data-tab-sid="${tab}"]')?.closest('[data-tabstrip]') === document.querySelector('[data-tab-sid="${opener}"]').closest('[data-tabstrip]')`;
  await a.wait(sameStrip, 3000);
  check(await sigOf() !== before, "the panel moved into the terminal's group");
  await a.click(`[data-tab-sid="${tab}"] .tab-name`);
  await a.sleep(200);
  check(await inApp(`globalThis.__alive === true && document.querySelector('textarea').value`) === "Unsent review comment", "moving the tab kept the app and its draft");
  const slot = (await a.rect(`[data-panel-slot="${pid}"]`))!;
  const view = (await a.rect(`[data-panel-view="${pid}"]`))!;
  check(Math.abs(slot.x - view.x) < 1 && Math.abs(slot.y - view.y) < 1 && Math.abs(slot.w - view.w) < 1 && Math.abs(slot.h - view.h) < 1, `the app sits over its new slot: ${JSON.stringify({ slot, view })}`);
  await shot("panel-10-draft-after-move");
  // And back out into a split of its own.
  const body = await a.eval(`(() => { const r = document.querySelector('[data-tab-sid="${tab}"]').closest('.pane').querySelector('[data-pane-body]').getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; })()`);
  const t2 = (await a.rect(`[data-tab-sid="${tab}"]`))!;
  const panes = await a.eval(`document.querySelectorAll('.pane').length`);
  await a.drag([t2.x + 20, t2.y + t2.h / 2], [body.x + body.w - 20, body.y + body.h / 2]);
  await a.wait(`document.querySelectorAll('.pane').length === ${panes + 1} && !(${sameStrip})`, 3000);
  check(await inApp(`globalThis.__alive === true && document.querySelector('textarea').value`) === "Unsent review comment", "splitting it out again kept the app");
  ok("dragging a panel tab between groups and into a new split keeps its app, state and unsent draft");

  // ---- Jump finds panels; the status line counts them
  await a.click(`[data-tab-sid="${opener}"]`);
  const tabs = await a.eval(`${M}.windowSessions.value.length + 1`);
  const counted = await a.eval(`document.querySelector('.sl-count')?.textContent ?? ''`);
  check(counted === `${tabs} tabs`, `status line counts the panel tab: ${counted}, want ${tabs}`);
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("Panel review");
  await a.sleep(200);
  check(await a.eval(`[...document.querySelectorAll('.sw-item')].some((e) => e.textContent.includes('Panel review'))`), "Jump lists the panel");
  await shot("panel-11-jump-finds-panel");
  await a.key("Enter");
  await a.wait(`!document.querySelector('.switcher') && ${M}.layout.value.focus === document.querySelector('[data-tab-sid="${tab}"]').closest('.pane').dataset.pane`, 2000);
  await a.wait(`document.activeElement?.dataset?.panelId === "${pid}"`, 2000);
  ok("Jump finds a panel by title and focuses it; the status line counts panel tabs");

  // ---- update in place, theme, link contrast, fullscreen
  await R("panel.update", { from_session: opener, arguments: { panel_id: pid, data: { markdown: "# Updated in place\n\nSee [the docs](https://example.com/docs)." } } });
  await a.sleep(350);
  check(await inApp(`document.querySelector('h1')?.textContent`) === "Updated in place" && await inApp(`globalThis.__alive`) === true, "input update keeps the same frame");
  await theme("light");
  await a.sleep(200);
  check(await inApp(`document.documentElement.dataset.theme`) === "light", "theme reaches app");
  const contrast = await inApp(`(() => {
    const rgb = (s) => s.match(/\\d+(\\.\\d+)?/g).slice(0, 3).map(Number);
    const lum = (c) => { const [r, g, b] = c.map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
    const a = lum(rgb(getComputedStyle(document.querySelector('a')).color)), b = lum(rgb(getComputedStyle(document.documentElement).backgroundColor));
    return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
  })()`);
  check(contrast >= 4.5, `light-theme links meet WCAG AA: ${contrast}`);
  await shot("panel-02-markdown-light");
  await inApp(`panelRequest('ui/request-display-mode', {mode:'fullscreen'})`);
  await a.sleep(150);
  check(await a.eval(`!!${M}.layout.value.zoom`) && await inApp(`globalThis.__alive`) === true, "fullscreen zoom preserves the app frame");
  await inApp(`panelRequest('ui/request-display-mode', {mode:'inline'})`);
  await a.sleep(150);
  check(await a.eval(`${M}.layout.value.zoom === null`) && await inApp(`globalThis.__alive`) === true, "inline mode restores groups without reloading the app");
  await theme("dark");
  ok(`input updates keep the frame, theme follows the host, light links reach ${contrast.toFixed(1)}:1, zoom keeps the app`);

  // ---- startup failures: an error with Reload, readable over the app, in both themes
  const broken = await open({ title: "Broken app", app: "custom", html: `<h1 style="color:#fff;background:#fff">Review controls</h1><script>throw new Error("boom before init")</script>` });
  const silent = await open({ title: "Static page", app: "custom", html: `<body style="background:#fff"><h1>Review controls</h1></body>` });
  await a.wait(`document.querySelector('[data-panel-view="${silent}"] .panel-status[role="status"]')?.textContent.includes('Loading')`, 3000);
  await a.wait(`!!document.querySelector('[data-panel-view="${broken}"] .panel-status-detail')`, 4000);
  check((await a.eval(`document.querySelector('[data-panel-view="${broken}"] .panel-status').textContent`)).includes("boom before init"), "the loading state shows the app's startup error");
  await a.wait(`!!document.querySelector('[data-panel-view="${broken}"] .panel-status[role="alert"]') && !!document.querySelector('[data-panel-view="${silent}"] .panel-status[role="alert"]')`, 12_000);
  const failure = (id: number) => a.eval(`(() => { const s = document.querySelector('[data-panel-view="${id}"] .panel-status'); const c = getComputedStyle(s);
    return { text: s.textContent, bg: c.backgroundColor, color: c.color, reload: !!s.querySelector('button') }; })()`);
  const f1 = await failure(broken), f2 = await failure(silent);
  check(f1.text.includes("never sent ui/initialize") && f1.text.includes("boom before init") && f1.reload, `broken app: ${JSON.stringify(f1)}`);
  check(f2.text.includes("never sent ui/initialize") && f2.reload, `static page: ${JSON.stringify(f2)}`);
  const opaque = (bg: string) => /^rgb\(/.test(bg) || /^rgba\(.*,\s*1\)$/.test(bg);
  check(opaque(f1.bg) && opaque(f2.bg), `failure overlays are opaque: ${f1.bg} ${f2.bg}`);
  await a.click(`[data-tab-sid="${-broken - 1}"] .tab-name`);
  await shot("panel-06-failed-app-dark");
  await theme("light");
  await a.sleep(200);
  const f3 = await failure(broken);
  check(opaque(f3.bg) && f3.bg !== f1.bg, `light theme has its own opaque surface: ${f3.bg}`);
  await shot("panel-06-failed-app-light");
  await theme("dark");
  await a.click(`[data-panel-view="${broken}"] .panel-status button`);
  await a.wait(`!!document.querySelector('[data-panel-view="${broken}"] .panel-status[role="status"]')`, 2000);
  ok("apps that fail or never initialize show what went wrong and Reload after 10 s, on an opaque surface in both themes");
  for (const id of [broken, silent]) await R("panel.close", { from_session: opener, arguments: { panel_id: id } });

  // ---- a dropped connection while loading retries on reconnect
  await R("mock.panel_delay", { ms: 1500 });
  const flaky = await open({ title: "Flaky load", app: "markdown", data: { markdown: "# Loaded after reconnect" } });
  await a.sleep(200);
  await R("mock.panel_delay", { ms: 0 });
  await R("mock.drop_clients", {});
  await a.wait(`!!document.querySelector('[data-panel-view="${flaky}"] .panel-status')?.textContent.includes('Connection lost')`, 3000);
  await ready(flaky, 10_000);
  const flakyApp = await appOf(flaky);
  check(await flakyApp.inApp(`document.querySelector('h1')?.textContent`) === "Loaded after reconnect", "the panel loaded after reconnecting");
  await flakyApp.inApp(`globalThis.__alive = true`);
  await R("mock.panel_delay", { ms: 1500 });
  await R("panel.update", { from_session: opener, arguments: { panel_id: flaky, data: { markdown: "# Update survived the drop" } } });
  await a.sleep(200);
  await R("mock.panel_delay", { ms: 0 });
  await R("mock.drop_clients", {});
  await a.sleep(600);
  await a.wait(`${M}.rpc.isOpen`, 10_000);
  await a.sleep(500);
  const updated = await flakyApp.inApp(`[document.querySelector('h1')?.textContent, globalThis.__alive === true]`);
  check(updated[0] === "Update survived the drop" && updated[1], `an update cut off by a disconnect is applied on reconnect, in the same frame: ${JSON.stringify(updated)}`);
  ok("a panel whose load or update was cut off by a disconnect retries on reconnect (no manual Reload)");
  await R("panel.close", { from_session: opener, arguments: { panel_id: flaky } });

  // ---- closing the focused panel hands the keyboard to the tab that takes its place
  await a.click(`[data-tab-sid="${tab}"] .tab-name`);
  await click("p");
  check(await activeEl() === `panel:${pid}`, "focus inside the panel before closing");
  await a.key("Alt+W");
  await a.wait(`!document.querySelector('[data-tab-sid="${tab}"]')`, 3000);
  await a.wait(`!!document.activeElement?.closest?.('.term-el')`, 2000);
  const closed = await events(pid);
  check(closed.length === 1 && closed[0].kind === "closed", "Alt+W from inside the app closed it and queued closed");
  const p2 = await open({ title: "Agent closes me", app: "markdown", data: { markdown: "# Bye" } });
  await ready(p2);
  await (await appOf(p2)).click("h1");
  check(await activeEl() === `panel:${p2}`, "focus inside the second panel");
  await R("panel.close", { from_session: opener, arguments: { panel_id: p2 } });
  await a.wait(`!document.querySelector('[data-tab-sid="${-p2 - 1}"]')`, 3000);
  await a.wait(`!!document.activeElement?.closest?.('.term-el')`, 2000);
  const p3 = await open({ title: "Close button", app: "markdown", data: { markdown: "# Bye" } });
  await ready(p3);
  await a.click(`[data-tab-sid="${-p3 - 1}"] .tab-close`);
  await a.wait(`!document.querySelector('[data-tab-sid="${-p3 - 1}"]')`, 3000);
  await a.wait(`!!document.activeElement?.closest?.('.term-el')`, 2000);
  ok("closing a focused panel (Alt+W in the app, the agent, or its close button) leaves the terminal focused for typing");
}

if (import.meta.main) {
  const [url, shots, repo] = Deno.args;
  if (!url || !shots || !repo) throw new Error("usage: e2e_panels.ts <mock url#token=dev> <shots-dir> <repo-path>");
  let step = 0;
  const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
  const check = (cond: unknown, msg: string) => { if (!cond) throw new Error("check failed: " + msg); };
  const a = await launch(9351, "/tmp/muster-cdp-panels");
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("panel checks run against the mock daemon");
    if (!(await a.eval(`${M}.windowSessions.value.length`))) {
      await a.wait(`!!document.querySelector('.empty-window')`);
      await a.eval(`(() => { const i = document.querySelector('.empty-window input');
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
        set.call(i, ${JSON.stringify(repo)}); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
      await a.click(".empty-window .launch-btn:nth-child(3)");
      await a.wait(`${M}.windowSessions.value.length === 1 && !!document.querySelector('.pane .xterm')`, 8000);
    }
    await panelChecks(a, { url, shots, hello, ok, check });
    console.log("panel checks passed");
  } catch (e) {
    await a.shot(`${shots}/panel-zz-failure.png`).catch(() => {});
    console.error(a.console.slice(-20).join("\n"));
    throw e;
  } finally {
    await a.close();
  }
}
