// End-to-end UI walkthrough against a running daemon (dev only).
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-write=<shots>,/tmp/muster-cdp \
//     --allow-read=<shots> dev/e2e.ts <ui-url-with-token> <shots-dir> <repo-path>
import { launch } from "./cdp.ts";

const [url, shots, repo] = Deno.args;
if (!url || !shots || !repo) {
  console.error("usage: e2e.ts <url#token=..> <shots-dir> <repo-path>");
  Deno.exit(2);
}

const b = await launch(9333, "/tmp/muster-cdp");
let step = 0;
const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
const shot = async (name: string) => {
  await b.sleep(250);
  await b.shot(`${shots}/${name}.png`);
};
const M = "globalThis.__muster";
const layoutSig = `(() => { const ch = ${M}.activeChannelId.value; const l = ${M}.layouts.value.get(ch);
  const sig = (n) => n.type === 'pane' ? '[' + n.tabs.join(',') + ']' : n.dir + '(' + n.children.map(sig).join(' ') + ')';
  return l ? sig(l.root) : null; })()`;
const paneCount = `document.querySelectorAll('.pane').length`;
const focusedSid = `(() => { const ch = ${M}.activeChannelId.value; const l = ${M}.layouts.value.get(ch);
  const find = (n) => n.type === 'pane' ? (n.id === l.focus ? n : null) : n.children.map(find).find(Boolean);
  return find(l.root)?.active ?? null; })()`;

try {
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.size(1440, 900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.topbar')`, 8000);
  ok("connected; hello done");
  // Token must be stripped from the URL.
  const href = await b.eval(`location.href`);
  if (href.includes("token=")) throw new Error("token not stripped from URL: " + href);
  ok("token stripped from URL");
  await shot("01-welcome");

  // Add workspace
  await b.click(".welcome .btn-primary");
  await b.wait(`!!document.querySelector('.dialog .path-box input')`);
  await b.type(repo);
  await b.sleep(300);
  await shot("02-add-workspace");
  await b.key("Enter");
  await b.wait(`!!document.querySelector('.dialog .input-prefix input')`, 5000);
  ok("workspace added; new channel dialog opened");

  // New channel with a shell
  await b.type("Fix auth redirect");
  await b.eval(
    `[...document.querySelectorAll('.dialog .seg-btn')].find(x => x.textContent.includes('Shell')).click()`,
  );
  await shot("03-new-channel");
  await b.key("Ctrl+Enter");
  await b.wait(`document.querySelectorAll('.pane .xterm').length === 1`, 8000);
  ok("channel created with a shell terminal");
  await b.sleep(800);

  // Type into the terminal
  let sid = await b.eval(focusedSid);
  await b.click(".term-host");
  await b.type("echo hello-$((6*7))");
  await b.key("Enter");
  await b.wait(`(${M}.termText(${sid}) || '').includes('hello-42')`, 5000);
  ok("typing in terminal reaches the PTY and echoes back");
  await shot("04-terminal");

  // Splits
  await b.key("Alt+d");
  await b.wait(`${paneCount} === 2`, 5000);
  await b.key("Alt+Shift+d");
  await b.wait(`${paneCount} === 3`, 5000);
  ok("split right + split down: " + await b.eval(layoutSig));
  await b.sleep(700);
  await shot("05-splits");

  // Focus movement
  const before = await b.eval(focusedSid);
  await b.key("Alt+Left");
  const after = await b.eval(focusedSid);
  if (before === after) throw new Error("Alt+Left did not move focus");
  ok("Alt+Left moved pane focus");

  // New tab in focused pane, then drag it to the bottom edge of the right-top pane
  await b.key("Alt+n");
  await b.wait(`!!document.querySelector('.menu')`);
  await shot("05b-spawn-menu");
  await b.key("3");
  await b.wait(`document.querySelectorAll('.pane')[0].querySelectorAll('.tab').length === 2`, 5000);
  ok("Alt+N opened a second tab in the focused pane");
  const tab = await b.rect(".pane:nth-of-type(1) .tab:last-of-type");
  const tabs = await b.eval(
    `[...document.querySelectorAll('.pane')[0].querySelectorAll('.tab')].map(t => { const r = t.getBoundingClientRect(); return [r.x + r.width/2, r.y + r.height/2]; })`,
  );
  const target = await b.eval(
    `(() => { const r = document.querySelectorAll('.pane-body')[1].getBoundingClientRect(); return [r.x + r.width/2, r.y + r.height - 20]; })()`,
  );
  void tab;
  const sigBefore = await b.eval(layoutSig);
  await b.drag(tabs[1] as [number, number], target as [number, number], 16);
  await b.wait(`${paneCount} === 4`, 3000);
  ok(`dragged tab into a new split: ${sigBefore} -> ${await b.eval(layoutSig)}`);
  await b.sleep(500);
  await shot("06-dragged");

  // Drag a gutter
  const g = await b.rect(".split-root > .split > .gutter");
  if (!g) throw new Error("no gutter");
  const sizesBefore = await b.eval(`${M}.layouts.value.get(${M}.activeChannelId.value).root.sizes.join(',')`);
  await b.drag([g.x + g.w / 2, g.y + 200], [g.x - 200, g.y + 200], 10);
  const sizesAfter = await b.eval(`${M}.layouts.value.get(${M}.activeChannelId.value).root.sizes.join(',')`);
  if (sizesBefore === sizesAfter) throw new Error("gutter drag did not resize");
  ok(`gutter drag resized: ${sizesBefore} -> ${sizesAfter}`);

  // Zoom
  await b.key("Alt+z");
  await b.wait(`${paneCount} === 1 && !!document.querySelector('.pane.zoomed')`, 3000);
  await b.sleep(400);
  await shot("07-zoomed");
  await b.key("Alt+z");
  await b.wait(`${paneCount} === 4`, 3000);
  ok("Alt+Z zooms and restores (terminals survive)");

  // Terminals were reparented, not recreated: the first shell still has its output.
  if (!(await b.eval(`(${M}.termText(${sid}) || '').includes('hello-42')`))) {
    throw new Error("terminal content lost across relayout");
  }
  ok("terminal content survived relayouts");

  // Second channel via Alt+Shift+N
  await b.key("Alt+Shift+n");
  await b.wait(`!!document.querySelector('.dialog .input-prefix input')`);
  await b.type("new api");
  await b.eval(
    `[...document.querySelectorAll('.dialog .seg-btn')].find(x => x.textContent.includes('Shell')).click()`,
  );
  await b.key("Enter");
  await b.wait(
    `document.querySelectorAll('.ch-row').length === 2 && document.querySelectorAll('.pane').length === 1`,
    8000,
  );
  ok("second channel created and selected");
  await b.sleep(500);

  // Switch back via sidebar
  await b.eval(
    `[...document.querySelectorAll('.ch-row')].find(r => r.textContent.includes('Fix auth')).click()`,
  );
  await b.wait(`${paneCount} === 4`, 3000);
  ok("switched channel via sidebar (layout restored with 4 panes)");

  // Switch via Ctrl+Shift+K
  await b.key("Ctrl+Shift+k");
  await b.wait(`!!document.querySelector('.switcher')`);
  await b.type("new");
  await b.sleep(150);
  await shot("08-switcher");
  await b.key("Enter");
  await b.wait(`${paneCount} === 1 && !document.querySelector('.switcher')`, 3000);
  ok("switched channel via Ctrl+Shift+K");
  await b.key("Ctrl+Shift+k");
  await b.type("fix");
  await b.key("Enter");
  await b.wait(`${paneCount} === 4`, 3000);

  // Composer
  sid = await b.eval(focusedSid);
  await b.key("Alt+c");
  await b.wait(`document.activeElement?.tagName === 'TEXTAREA'`);
  await b.type("echo from-composer");
  await b.sleep(100);
  await shot("09-composer");
  await b.key("Enter");
  await b.wait(
    `[...${M}.sessions.value.keys()].some(id => (${M}.termText(id) || '').includes('from-composer'))`,
    6000,
  )
    .then(() => ok("composer message delivered to a terminal"))
    .catch(() => {
      console.log("✗ composer message did not show up in any terminal");
    });

  // Note + timeline
  await b.key("Alt+c");
  await b.type("/note checked the redirect flow");
  await b.key("Enter");
  // Alt+letters belong to the text field while the composer has focus; Esc goes back first.
  await b.key("Alt+l");
  if (await b.eval(`!!document.querySelector('.timeline')`)) {
    throw new Error("Alt+L was intercepted inside the composer");
  }
  await b.key("Escape");
  await b.key("Alt+l");
  await b.wait(`!!document.querySelector('.timeline')`);
  await b.wait(`document.querySelector('.timeline').textContent.includes('checked the redirect flow')`, 4000);
  ok("note shows in timeline");
  await b.sleep(400);
  await shot("10-timeline");

  // Diff tab
  await b.key("Alt+g");
  await b.wait(`!!document.querySelector('.diff')`);
  await b.sleep(600);
  await shot("11-diff");
  await b.key("Alt+l");
  await b.key("Alt+l");

  // Exit + restart a shell
  await b.eval(`document.querySelectorAll('.term-host')[0].click()`);
  const s0 = await b.eval(
    `(() => { const ch = ${M}.activeChannelId.value; const l = ${M}.layouts.value.get(ch);
    const first = (n) => n.type === 'pane' ? n : first(n.children[0]); return first(l.root).active; })()`,
  );
  await b.click(".pane .term-host");
  await b.type("exit");
  await b.key("Enter");
  await b.wait(`${M}.sessions.value.get(${s0})?.state === 'exited'`, 5000);
  await b.wait(`!!document.querySelector('.exited-bar')`, 2000);
  await b.sleep(300);
  await shot("12-exited");
  ok("shell exit shows the exited bar");
  await b.click(".pane .term-host");
  await b.key("Enter");
  await b.wait(`${M}.sessions.value.get(${s0})?.state !== 'exited'`, 5000);
  ok("Enter restarted the exited session");
  await b.sleep(600);
  await b.type("echo again-$((1+1))");
  await b.key("Enter");
  await b.wait(`(${M}.termText(${s0}) || '').includes('again-2')`, 5000);
  ok("restarted shell is interactive");

  // Close a tab (idle shell closes immediately)
  const nBefore = await b.eval(`${M}.sessions.value.size`);
  await b.key("Alt+w");
  await b.wait(`${M}.sessions.value.size === ${nBefore - 1}`, 5000);
  ok("Alt+W closed the focused session");
  const sigPreReload = await b.eval(layoutSig);

  // Inbox + shortcuts
  await b.key("Alt+i");
  await b.wait(`!!document.querySelector('.inbox')`);
  await shot("13-inbox-empty");
  await b.key("Alt+i");
  await b.key("Alt+/");
  await b.wait(`!!document.querySelector('.shortcuts')`);
  await shot("14-shortcuts");
  await b.key("Escape");

  // Context menu on a tab
  const t = await b.rect(".tab");
  await b.clickAt(t!.x + t!.w / 2, t!.y + t!.h / 2, { button: "right" });
  await b.wait(`!!document.querySelector('.menu')`);
  await shot("15-tab-menu");
  await b.key("Escape");

  // Layout survives reload (debounced save is 400ms)
  await b.sleep(900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.pane')`, 8000);
  await b.sleep(800);
  const sigAfter = await b.eval(layoutSig);
  if (sigAfter !== sigPreReload) {
    throw new Error(`layout changed across reload: ${sigPreReload} -> ${sigAfter}`);
  }
  ok(`layout survived reload: ${sigAfter}`);
  await shot("16-after-reload");

  // Narrow + light
  await b.size(900, 700);
  await b.sleep(500);
  await shot("17-narrow");
  await b.send("Emulation.setEmulatedMedia", {
    features: [{ name: "prefers-color-scheme", value: "light" }],
  });
  await b.size(1440, 900);
  await b.sleep(500);
  await shot("18-light");

  const errs = b.console.filter((l) => l.startsWith("error") || l.startsWith("exception"));
  if (errs.length) console.log("console errors:\n  " + errs.join("\n  "));
  console.log("ALL OK");
} catch (e) {
  console.error("FAILED:", e instanceof Error ? e.message : e);
  await b.shot(`${shots}/zz-failure.png`).catch(() => {});
  console.log(b.console.slice(-20).join("\n"));
  Deno.exitCode = 1;
} finally {
  await b.close();
}
