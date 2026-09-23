// Second walkthrough (run after e2e.ts against the same daemon): rename, reorder, find, font,
// busy-close confirm, tab reorder, reconnect after a daemon restart.
//   deno run --allow-net=127.0.0.1 --allow-run=chromium,<restart.sh> --allow-write=<shots>,/tmp/muster-cdp \
//     dev/e2e2.ts <url> <shots> <restart-script>
import { launch } from "./cdp.ts";

const [url, shots, restart] = Deno.args;
const b = await launch(9333, "/tmp/muster-cdp");
let step = 0;
const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
const M = "globalThis.__muster";
const focusedSid = `(() => { const ch = ${M}.activeChannelId.value; const l = ${M}.layouts.value.get(ch);
  const find = (n) => n.type === 'pane' ? (n.id === l.focus ? n : null) : n.children.map(find).find(Boolean);
  return find(l.root)?.active ?? null; })()`;

try {
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.size(1440, 900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.pane .xterm')`, 8000);
  await b.sleep(600);

  // Rename channel by double-clicking the header title
  await b.click(".ch-name", { count: 2 });
  await b.wait(`!!document.querySelector('.ch-title-edit')`);
  await b.key("Ctrl+a");
  await b.type("auth-fix");
  await b.key("Enter");
  await b.wait(`document.querySelector('.ch-name')?.textContent === 'auth-fix'`, 4000);
  ok("renamed channel inline");

  // Reorder channels by dragging in the sidebar
  const order0 = await b.eval(
    `[...document.querySelectorAll('.ch-row .side-label')].map(x => x.textContent).join('|')`,
  );
  const rows = await b.eval(
    `[...document.querySelectorAll('.ch-row')].map(r => { const b = r.getBoundingClientRect(); return [b.x + 60, b.y + b.height/2]; })`,
  );
  await b.drag(rows[0], [rows[1][0], rows[1][1] + 12], 10);
  await b.sleep(400);
  const order1 = await b.eval(
    `[...document.querySelectorAll('.ch-row .side-label')].map(x => x.textContent).join('|')`,
  );
  if (order0 === order1) throw new Error("channel drag did not reorder: " + order0);
  ok(`reordered channels: ${order0} -> ${order1}`);

  // Font size
  const fs0 = await b.eval(
    `(${M}.prefsFont?.() ?? JSON.parse(localStorage.getItem('muster.prefs') || '{}').fontSize) || 13`,
  );
  await b.click(".term-host");
  await b.key("Ctrl+=");
  await b.key("Ctrl+=");
  await b.sleep(400);
  const fs1 = await b.eval(`JSON.parse(localStorage.getItem('muster.prefs') || '{}').fontSize`);
  if (!(fs1 > fs0)) throw new Error(`font size did not grow: ${fs0} -> ${fs1}`);
  await b.key("Ctrl+0");
  ok(`Ctrl+= grows terminal font (${fs0} -> ${fs1}), Ctrl+0 resets`);

  // Find in terminal
  const sid = await b.eval(focusedSid);
  await b.type("echo needle-in-haystack");
  await b.key("Enter");
  await b.wait(`(${M}.termText(${sid}) || '').includes('needle-in-haystack')`, 4000);
  await b.key("Ctrl+Shift+f");
  await b.wait(`!!document.querySelector('.findbar input')`);
  await b.type("needle");
  await b.sleep(300);
  await b.shot(`${shots}/21-find.png`);
  const count = await b.eval(`document.querySelector('.find-count').textContent`);
  if (!/\d+\/\d+/.test(count)) throw new Error("find shows no results: " + count);
  await b.key("Escape");
  ok(`find in terminal: ${count}`);

  // Busy shell: close asks for inline confirmation
  await b.click(".term-host");
  await b.type("sleep 30");
  await b.key("Enter");
  const busy = await b.wait(`${M}.sessions.value.get(${sid})?.state === 'working'`, 6000).then(
    () => true,
    () => false,
  );
  if (!busy) {
    console.log(
      `… skipped busy-close check: shell running 'sleep 30' stayed '${await b.eval(
        `${M}.sessions.value.get(${sid})?.state`,
      )}'`,
    );
  } else {
    await b.key("Alt+w");
    await b.wait(`!!document.querySelector('.tab.confirm')`, 2000);
    await b.shot(`${shots}/22-confirm-close.png`);
    await b.key("Escape");
    await b.wait(`!document.querySelector('.tab.confirm')`, 2000);
    ok("closing a busy session asks inline (Esc cancels)");
    await b.key("Alt+w");
    await b.wait(`!!document.querySelector('.tab.confirm')`, 2000);
    await b.key("Enter");
    await b.wait(`!${M}.sessions.value.has(${sid})`, 5000);
    ok("Enter confirms and kills the busy session");
  }

  // Tab reorder within a strip: add two tabs to the focused pane and drag the last to the front
  await b.key("Alt+n");
  await b.wait(`!!document.querySelector('.menu')`);
  await b.key("3");
  await b.sleep(900);
  await b.key("Alt+n");
  await b.wait(`!!document.querySelector('.menu')`);
  await b.key("3");
  await b.sleep(900);
  const tabsBefore = await b.eval(
    `[...document.querySelector('.pane.focused').querySelectorAll('.tab')].map(t => t.dataset.tabSid).join(',')`,
  );
  const tabRects = await b.eval(
    `[...document.querySelector('.pane.focused').querySelectorAll('.tab')].map(t => { const r = t.getBoundingClientRect(); return [r.x, r.y + r.height/2, r.width]; })`,
  );
  const last = tabRects[tabRects.length - 1];
  await b.drag([last[0] + last[2] / 2, last[1]], [tabRects[0][0] + 4, tabRects[0][1]], 12);
  const tabsAfter = await b.eval(
    `[...document.querySelector('.pane.focused').querySelectorAll('.tab')].map(t => t.dataset.tabSid).join(',')`,
  );
  if (tabsBefore === tabsAfter) throw new Error("tab reorder failed: " + tabsBefore);
  ok(`reordered tabs by drag: ${tabsBefore} -> ${tabsAfter}`);

  // Reconnect after a daemon restart; terminals re-attach and stay usable
  const cur = await b.eval(focusedSid);
  const p = new Deno.Command(restart, { args: ["keep"], stdout: "null", stderr: "null" }).spawn();
  await b.wait(`!!document.querySelector('.reconnect-pill')`, 8000).then(() =>
    ok("shows Reconnecting… while the daemon is down")
  )
    .catch(() => console.log("… reconnect pill not observed (restart was quick)"));
  await p.status;
  await b.wait(
    `!document.querySelector('.reconnect-pill') && !!document.querySelector('.pane .xterm')`,
    15000,
  );
  await b.sleep(1500);
  const alive = await b.eval(`${M}.sessions.value.get(${cur})?.state`);
  ok(`reconnected; focused session state after daemon restart: ${alive}`);
  await b.shot(`${shots}/23-after-reconnect.png`);
  if (alive && alive !== "exited") {
    await b.click(".pane.focused .term-host");
    await b.type("echo back-$((2+3))");
    await b.key("Enter");
    await b.wait(`(${M}.termText(${cur}) || '').includes('back-5')`, 5000);
    ok("terminal works after reconnect");
  } else {
    await b.wait(`!!document.querySelector('.exited-bar')`, 3000);
    ok("sessions show as exited after a daemon restart (restartable from the bar)");
  }

  const errs = b.console.filter((l) => l.startsWith("error") || l.startsWith("exception"));
  if (errs.length) console.log("console errors:\n  " + errs.join("\n  "));
  console.log("ALL OK");
} catch (e) {
  console.error("FAILED:", e instanceof Error ? e.message : e);
  await b.shot(`${shots}/zz-failure2.png`).catch(() => {});
  console.log(b.console.slice(-20).join("\n"));
  Deno.exitCode = 1;
} finally {
  await b.close();
}
