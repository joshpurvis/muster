// Rendered regression checks against an isolated fixture with three shell tabs in one channel.
// deno run --allow-net=127.0.0.1 --allow-run=chromium dev/e2e_tab_stability.ts <url> <fixture-dir> <cdp-port>
import { launch } from "./cdp.ts";

const [url, base, port] = Deno.args;
if (!base?.startsWith("/tmp/muster-eval-") || new URL(url).hostname !== "127.0.0.1") {
  throw new Error("An isolated eval fixture is required");
}
const b = await launch(Number(port), `${base}/tab-stability-browser`);
try {
  await b.goto(url);
  await b.wait(`document.querySelectorAll('.tab[data-tab-sid]').length >= 3`);
  await b.eval(`document.fonts.ready`);
  await b.click(".pane .xterm");
  await b.sleep(700);
  await b.eval(`(() => {
    const tabs = [...document.querySelectorAll('.tab[data-tab-sid]')];
    const baseline = tabs.map(tab => {
      const dot = tab.querySelector('.dot');
      return { tab, dot, color: getComputedStyle(dot).backgroundColor, width: tab.getBoundingClientRect().width };
    });
    const probe = globalThis.__tabStability = { errors: [], frames: 0, stop: false };
    function frame() {
      probe.frames++;
      for (const {tab, dot, color, width} of baseline) {
        const fail = message => { if (!probe.errors.includes(message)) probe.errors.push(message); };
        if (!tab.isConnected || tab.querySelector('.dot') !== dot) fail('replaced tab or orb');
        if (getComputedStyle(dot).backgroundColor !== color) fail('orb changed color');
        if (Math.abs(tab.getBoundingClientRect().width - width) > .1) fail('tab changed width');
        if (tab.classList.contains('unread')) fail('visible completion flashed unread');
      }
      if (!probe.stop) requestAnimationFrame(frame);
    }
    requestAnimationFrame(frame);
    const M = globalThis.__muster;
    let tick = 0;
    const timer = setInterval(() => {
      M.sessions.value = new Map([...M.sessions.value].map(([id,s]) => [id, {...s, state_detail: 'refresh ' + tick}]));
      M.rpc.onEvent({event:'event', data:{id:Date.now(), channel_id:M.focusedSession.value.channel_id,
        session_id:null, kind:'state_change', ts:Date.now(), data:{from:'working',to:'idle'}}});
      if (++tick === 8) clearInterval(timer);
    }, 250);
  })()`);
  await b.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Alt", code: "AltLeft", windowsVirtualKeyCode: 18, modifiers: 1 });
  await b.wait(`document.querySelectorAll('.tab-num').length >= 3`);
  await b.sleep(600);
  await b.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Alt", code: "AltLeft", windowsVirtualKeyCode: 18, modifiers: 0 });
  await b.wait(`!document.querySelector('.tab-num')`);
  // Cross the shared 20-second status/cache clock as well as the 600ms read acknowledgement.
  await b.sleep(21_000);
  const result = await b.eval(`(() => {
    const p = globalThis.__tabStability; p.stop = true;
    // Genuine background unread/read styling must not resize the labels either.
    const tab = document.querySelector('.tab[data-tab-sid]:not(.active)');
    const before = tab.getBoundingClientRect().width;
    tab.classList.add('unread');
    if (Math.abs(tab.getBoundingClientRect().width - before) > .1) p.errors.push('bold label changed width');
    tab.classList.remove('unread');
    return p;
  })()`);
  if (result.errors.length || result.frames < 10) throw new Error(JSON.stringify(result));
  // A real state change must still update the dot immediately.
  await b.eval(`(() => {
    const M = globalThis.__muster, s = M.focusedSession.value;
    M.rpc.onEvent({event:'session_upserted',data:{...s,state:'exited'}});
  })()`);
  await b.wait(`document.querySelector('.tab.active .dot')?.classList.contains('st-exited')`);
  console.log(`PASS: stable orbs and tab widths across ${result.frames} frames, refreshes, completion events, Alt hints and unread styling; real state changes still render`);
} finally {
  await b.close();
}
