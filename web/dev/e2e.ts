// End-to-end UI walkthrough (dev only). Two headless browsers act as two client windows.
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-write=<shots> \
//     --allow-read=<shots> dev/e2e.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path> [windows|legacy]
//
// `legacy` runs the single-window part only (a daemon without windows). It never starts an agent
// outside the mock: only the mock fakes them, a real daemon would run a real one.
import { type Browser, launch } from "./cdp.ts";

const [url, shots, repo, mode = "windows"] = Deno.args;
if (!url || !shots || !repo) {
  console.error("usage: e2e.ts <url#token=..> <shots-dir> <repo-path> [windows|legacy]");
  Deno.exit(2);
}

let step = 0;
const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
const M = "globalThis.__muster";
const tabIds = `${M}.tabs.value.map((c) => c.id)`;
const R = (b: Browser, m: string, p: unknown) =>
  b.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
const shot = async (b: Browser, name: string) => {
  await b.sleep(300);
  await b.shot(`${shots}/${name}.png`);
};
function check(cond: unknown, msg: string) {
  if (!cond) throw new Error("check failed: " + msg);
}

async function open(port: number, dir: string): Promise<Browser> {
  const b = await launch(port, dir);
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.size(1440, 900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`, 10000);
  return b;
}

const a = await open(9341, "/tmp/muster-cdp-a");
let b2: Browser | null = null;
try {
  const windows = await a.eval(`${M}.windowId.value`) !== null;
  check(windows === (mode === "windows"), `window support is ${windows}, expected mode ${mode}`);
  const href = await a.eval(`location.href`);
  check(!href.includes("token="), "token stripped from URL");
  ok(`connected (${mode}); token stripped`);
  if (windows) {
    check(/^(\(\d+\) )?muster · \d+$/.test(await a.eval(`document.title`)), "title is muster · <window>");
    ok("document.title " + (await a.eval(`document.title`)));
  }

  // ---- empty window → first tab
  let n0 = (await a.eval(tabIds)).length;
  if (n0 === 0) {
    await a.wait(`!!document.querySelector('.empty-window')`);
    check(await a.eval(`!document.querySelector('.tb-split')`), "no split buttons without tabs");
    await shot(a, "01-empty-window");
    await a.eval(`(() => { const i = document.querySelector('.empty-window input');
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      set.call(i, ${JSON.stringify(repo)}); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
    await a.click(".empty-window .launch-btn:nth-child(3)");
    await a.wait(`${tabIds}.length === 1 && !!document.querySelector('.pane .xterm')`, 8000);
    ok("empty window: folder + Shell opened the first tab");
    n0 = 1;
  }
  await a.sleep(500);
  check(
    await a.eval(`document.querySelectorAll('.pane-head, .tabstrip').length`) === 0,
    "lone pane has no chrome",
  );
  ok("a lone pane with one session has no chrome");

  // ---- the tab bar's "Split right" button splits a lone pane (like Alt+D)
  const panes0: number = await a.eval(`document.querySelectorAll('.pane').length`);
  await a.click(`.tb-split-btn[aria-label="Split right"]`);
  await a.wait(`document.querySelectorAll('.pane').length === ${panes0 + 1}`, 6000);
  ok("the tab bar's Split right button split the lone pane");

  // ---- new tab (Alt+T), split (Alt+D), pane-tab (Alt+N)
  await a.key("Alt+T");
  await a.wait(`${tabIds}.length === ${n0 + 1}`, 6000);
  await a.wait(`!!document.querySelector('.pane .xterm')`, 6000);
  ok("Alt+T opened a tab in the same folder");
  await a.key("Alt+D");
  await a.wait(`document.querySelectorAll('.pane').length === 2`, 6000);
  await a.wait(`document.querySelectorAll('.pane-head').length === 2`, 3000);
  ok("Alt+D split: both panes show a header");
  await a.key("Alt+N");
  await a.wait(`!!document.querySelector('.menu')`);
  await a.key("3");
  await a.wait(
    `document.querySelectorAll('.tabstrip').length === 1 && document.querySelectorAll('.pane-head').length === 1`,
    6000,
  );
  ok("Alt+N added a pane-tab: that pane shows a strip, the other keeps its header");
  await shot(a, "02-split");

  // ---- an agent tab that needs approval, auto-named from its first prompt. Only the mock
  // (windows mode) fakes agents: against a real daemon this would start a real one.
  const mock = String((await R(a, "hello", {}) as any).version).includes("mock");
  const fake = windows && mock;
  const ws = await R(a, "workspace.add", { path: repo });
  const c: any = await R(a, "channel.create", {
    workspace_id: ws.id,
    worktree: false,
    agent: fake ? "claude" : "shell",
    prompt: fake ? "Fix the flaky login redirect test" : undefined,
    window_id: await a.eval(`${M}.windowId.value ?? undefined`),
    name: fake ? undefined : "flaky",
    cols: 120,
    rows: 30,
  });
  await a.wait(`${tabIds}.includes(${c.id})`, 6000);
  if (fake) {
    await a.wait(
      `[...document.querySelectorAll('.ttab-name')].some((e) => e.textContent === 'Fix the flaky login redirect test')`,
      4000,
    );
    ok("auto-named tab shows the name derived from its first prompt");
  }
  const sid = await a.eval(`[...${M}.sessions.value.values()].find((s) => s.channel_id === ${c.id}).id`);
  let needs = false;
  if (fake) {
    await a.sleep(4000); // the fake agent goes idle
    await R(a, "session.send", { id: sid, text: "ask for approval" });
    await a.wait(`${M}.sessions.value.get(${sid}).state === 'needs_approval'`, 8000).catch(() => {});
    needs = await a.eval(`${M}.sessions.value.get(${sid}).state === 'needs_approval'`);
  }
  if (needs) {
    await a.wait(`/^\\(1\\) muster/.test(document.title)`, 3000);
    await a.wait(`!!document.querySelector('.sl-needs')`, 3000);
    ok("needs approval: title (1) prefix and status line chip");
    // Alt+J goes there.
    await a.key("Alt+J");
    await a.wait(`${M}.activeChannel.value?.id === ${c.id}`, 3000);
    await a.wait(`!!document.querySelector('.approval')`, 3000);
    ok("Alt+J jumped to the session that needs you; approval banner shown");
    await shot(a, "03-approval");
  } else ok("(skipped approval: this daemon has no fake agent)");

  // ---- Jump overlay
  await a.key("Ctrl+Shift+K");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("flaky");
  await a.sleep(200);
  await shot(a, "04-jump");
  await a.key("Escape");
  await a.wait(`!document.querySelector('.switcher')`);
  await a.key("Ctrl+Shift+P");
  await a.wait(`document.querySelector('.sw-mode')?.textContent === 'Commands'`);
  await shot(a, "05-palette");
  await a.key("Escape");
  // Ctrl+Space opens Jump from inside a terminal, and closes it again.
  await a.click(".pane .xterm");
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.key("Ctrl+Space");
  await a.wait(`!document.querySelector('.switcher')`);
  ok("Jump and command palette open and close (Ctrl+Space toggles Jump from a terminal)");

  // ---- rename by double-click
  const first = await a.rect(".ttab");
  await a.clickAt(first!.x + 40, first!.y + first!.h / 2, { count: 2 });
  await a.wait(`!!document.querySelector('.ttab .inline-edit')`);
  await a.key("Ctrl+a");
  await a.type("renamed tab");
  await a.key("Enter");
  await a.wait(
    `[...document.querySelectorAll('.ttab-name')].some((e) => e.textContent === 'renamed tab')`,
    4000,
  );
  ok("double-click renamed a tab");

  // ---- reorder: drag the first tab to the end, then Alt+Shift+[ back
  const before: number[] = await a.eval(tabIds);
  const r1 = await a.rect(".ttab");
  const last = await a.eval(
    `(() => { const t = [...document.querySelectorAll('.ttab')].pop().getBoundingClientRect(); return { x: t.right - 4, y: t.top + t.height / 2 }; })()`,
  );
  await a.drag([r1!.x + 30, r1!.y + r1!.h / 2], [last.x, last.y]);
  await a.wait(`${tabIds}[${before.length - 1}] === ${before[0]}`, 3000);
  ok("dragging a tab reordered it");
  await a.key("Alt+Shift+[");
  await a.sleep(200);
  ok("Alt+Shift+[ moved the active tab left");

  // ---- Alt+1 / Ctrl+Tab
  await a.key("Alt+1");
  await a.wait(`${M}.activeChannel.value?.id === ${tabIds}[0]`);
  await a.key("Ctrl+Tab");
  await a.wait(`${M}.activeChannel.value?.id === ${tabIds}[1]`);
  ok("Alt+1 and Ctrl+Tab switch tabs");

  if (windows) {
    // ---- a second window
    b2 = await open(9342, "/tmp/muster-cdp-b");
    const w1 = await a.eval(`${M}.windowId.value`);
    const w2 = await b2.eval(`${M}.windowId.value`);
    check(w1 !== w2, "second client claimed another window");
    await b2.wait(`!!document.querySelector('.empty-window')`, 4000);
    ok(`second client claimed window ${w2} (first holds ${w1}); it starts empty`);
    await a.wait(`!!document.querySelector('.tb-win')`, 3000);
    ok("with 2 windows the tab bar names this window");

    // Jump in B pulls a tab from A.
    const pulled = await a.eval(`${M}.tabs.value.find((c) => c.name === 'renamed tab').id`);
    const pulledSids: number[] = await a.eval(
      `[...${M}.sessions.value.values()].filter((s) => s.channel_id === ${pulled}).map((s) => s.id)`,
    );
    await b2.key("Ctrl+Shift+K");
    await b2.wait(`!!document.querySelector('.switcher')`);
    await b2.type("renamed");
    await b2.sleep(150);
    await b2.key("Enter");
    await b2.wait(`${tabIds}.includes(${pulled})`, 5000);
    await a.wait(`!${tabIds}.includes(${pulled})`, 5000);
    await b2.wait(`!!document.querySelector('.pane .xterm')`, 5000);
    ok("Jump in window 2 pulled the tab over (channel.move); window 1 lost it");
    await a.wait(`!${M}.attached().some((s) => ${JSON.stringify(pulledSids)}.includes(s))`, 3000);
    ok("window 1 detached the moved tab's terminals");
    await shot(b2, "06-window2");

    // Split it in B; the layout must travel with the tab when it goes back to A.
    const panesBefore: number = await b2.eval(`document.querySelectorAll('.pane').length`);
    await b2.click(".pane .xterm");
    await b2.key("Alt+D");
    await b2.wait(`document.querySelectorAll('.pane').length === ${panesBefore + 1}`, 5000);
    await b2.sleep(700); // layout save is debounced (400ms)

    // Move back through the tab context menu.
    await b2.click(".ttab", { button: "right" });
    await b2.wait(`!!document.querySelector('.menu')`);
    await b2.eval(
      `[...document.querySelectorAll('.menu-item')].find((e) => e.textContent.includes('Move to window')).dispatchEvent(new MouseEvent('mouseenter'))`,
    );
    await b2.sleep(200);
    await b2.eval(
      `[...document.querySelectorAll('.menu-item')].find((e) => e.textContent.includes('Window ${w1}')).click()`,
    );
    await a.wait(`${tabIds}.includes(${pulled})`, 5000);
    await b2.wait(`${tabIds}.length === 0`, 5000);
    ok("context menu → Move to window sent it back");
    const idx: number = await a.eval(`${tabIds}.indexOf(${pulled})`);
    await a.key(`Alt+${idx + 1}`);
    await a.wait(`${M}.activeChannel.value?.id === ${pulled}`, 3000);
    await a.wait(`document.querySelectorAll('.pane').length === ${panesBefore + 1}`, 5000);
    ok("the split made in window 2 came back with the tab");

    // Reload keeps the window.
    await a.goto("about:blank");
    await a.sleep(300);
    await a.send("Page.navigate", { url: url.replace(/#.*/, "") });
    await a.wait(`!!document.querySelector('.app')`, 8000);
    check(await a.eval(`${M}.windowId.value`) === w1, "reload reclaimed the same window");
    ok("reload reclaimed the same window (sessionStorage)");

    // ---- drag a tab onto the right edge of the active tab's single pane: merged as a split.
    const winId = await a.eval(`${M}.windowId.value`);
    const into: any = await R(a, "channel.create", {
      workspace_id: ws.id,
      agent: "shell",
      window_id: winId,
      name: "into",
    });
    const merged: any = await R(a, "channel.create", {
      workspace_id: ws.id,
      agent: "shell",
      window_id: winId,
      name: "merged",
    });
    await a.wait(`${tabIds}.includes(${into.id}) && ${tabIds}.includes(${merged.id})`, 6000);
    await a.click(`[data-tab-id="${into.id}"]`);
    await a.wait(
      `${M}.activeChannel.value?.id === ${into.id} && document.querySelectorAll('.pane').length === 1 && !!document.querySelector('.pane .xterm')`,
      6000,
    );
    const sidsOf = (ch: number) =>
      a.eval(`[...${M}.sessions.value.values()].filter((s) => s.channel_id === ${ch}).map((s) => s.id)`);
    const want: number[] = [...await sidsOf(into.id), ...await sidsOf(merged.id)];
    check(want.length === 2, "each new tab has one session");
    const from = await a.rect(`[data-tab-id="${merged.id}"]`);
    const body = await a.rect(".pane-body");
    await a.drag([from!.x + 30, from!.y + from!.h / 2], [body!.x + body!.w - 20, body!.y + body!.h / 2], 16);
    await a.wait(`!${tabIds}.includes(${merged.id})`, 6000);
    await a.wait(`document.querySelectorAll('.pane').length === 2`, 4000);
    const layout = await a.eval(`JSON.stringify(${M}.layouts.value.get(${into.id}).root)`);
    const root = JSON.parse(layout);
    check(root.type === "split" && root.dir === "row", `a row split: ${layout}`);
    check(
      JSON.stringify(root.children.map((p: any) => p.tabs)) === JSON.stringify(want.map((id) => [id])),
      `left pane keeps its session, the dragged tab's session is on the right: ${layout}`,
    );
    check(
      (await sidsOf(into.id)).length === 2 &&
        !(await a.eval(`${M}.tabs.value.some((c) => c.id === ${merged.id})`)),
      "both sessions now belong to the active tab; the dragged tab closed",
    );
    ok("dragging a tab onto the right edge of the active pane merged it as a split and closed it");

    await shot(a, "06b-merged");

    // ---- and back: the right pane's header dropped on the empty part of the tab bar.
    const head = await a.rect(".split-child:last-child .pane-head");
    const space = await a.rect(".tb-space");
    const nTabs: number = (await a.eval(tabIds)).length;
    await a.drag(
      [head!.x + 60, head!.y + head!.h / 2],
      [space!.x + space!.w / 2, space!.y + space!.h / 2],
      16,
    );
    await a.wait(`${tabIds}.length === ${nTabs + 1}`, 6000);
    await a.wait(
      `${M}.activeChannel.value?.id !== ${into.id} && document.querySelectorAll('.pane').length === 1`,
      4000,
    );
    const newTab: number = await a.eval(`${M}.activeChannel.value.id`);
    check(
      JSON.stringify(await sidsOf(newTab)) === JSON.stringify([want[1]]) &&
        JSON.stringify(await sidsOf(into.id)) === JSON.stringify([want[0]]),
      "the session moved into the new, active tab",
    );
    ok("dropping a pane header on the tab bar moved its session into a new tab");

    // ---- drag the ACTIVE tab onto the right half of its own panes: it goes beside the tab
    // viewed before it, which becomes active.
    const beside: any = await R(a, "channel.create", {
      workspace_id: ws.id,
      agent: "shell",
      window_id: winId,
      name: "beside",
    });
    await a.wait(`${tabIds}.includes(${beside.id})`, 6000);
    const partner: number = await a.eval(`${M}.activeChannel.value.id`); // viewed just before
    await a.click(`[data-tab-id="${beside.id}"]`);
    await a.wait(`${M}.activeChannel.value?.id === ${beside.id} && !!document.querySelector('.pane .xterm')`, 6000);
    const bt = await a.rect(`[data-tab-id="${beside.id}"]`);
    const area = await a.rect(".panes");
    await a.drag([bt!.x + 30, bt!.y + bt!.h / 2], [area!.x + area!.w - 30, area!.y + area!.h / 2], 16);
    await a.wait(`!${tabIds}.includes(${beside.id})`, 6000);
    await a.wait(`${M}.activeChannel.value?.id === ${partner} && document.querySelectorAll('.pane').length === 2`, 6000);
    const root2 = JSON.parse(await a.eval(`JSON.stringify(${M}.layouts.value.get(${partner}).root)`));
    check(root2.type === "split" && root2.dir === "row", "the active tab went to the right of the whole layout");
    ok("dragging the active tab onto its own panes put it in a split beside the previous tab");
  }

  // ---- light theme
  await a.send("Emulation.setEmulatedMedia", {
    features: [{ name: "prefers-color-scheme", value: "light" }],
  });
  await a.sleep(300);
  await shot(a, "07-light");
  await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });

  // ---- shortcuts sheet
  await a.key("F1");
  await a.wait(`!!document.querySelector('.shortcuts')`);
  await shot(a, "08-shortcuts");
  await a.key("Escape");
  ok("shortcuts sheet");

  const errors = [...a.console, ...(b2?.console ?? [])].filter((l) => /^(error|exception)/.test(l));
  if (errors.length) console.log("console errors:\n  " + errors.join("\n  "));
  else ok("no console errors");
} catch (e) {
  console.error("✗", e instanceof Error ? e.message : e);
  await a.shot(`${shots}/zz-failure-a.png`).catch(() => {});
  if (b2) await b2.shot(`${shots}/zz-failure-b.png`).catch(() => {});
  console.log([...a.console, ...(b2?.console ?? [])].slice(-20).join("\n"));
  Deno.exitCode = 1;
} finally {
  await a.close();
  await b2?.close();
}
