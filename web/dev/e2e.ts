// End-to-end UI walkthrough against the MOCK daemon (dev only). Two headless browsers act as two
// client windows. Covers the v2 layout: a window split into groups, each with its own tab strip;
// a tab is one session. Also the agents panel (Alt+G, start-and-pull, the message log, New
// agent from a template, Edit ▸ Memory).
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots>,src \
//     --allow-write=<shots>,/tmp dev/e2e.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path>
//
// It refuses to run against anything but the mock (a real daemon would start real agents), and
// blocks `window.show` in the pages it drives, so it can never launch a window.
import { type Browser, launch } from "./cdp.ts";
import { allPanes, type Layout, sig } from "../src/layout/tree.ts";

const [url, shots, repo, mode] = Deno.args;
// `real`: against a real (isolated!) daemon. Agents are replaced by a named shell and the
// approval steps are skipped, so no real agent ever starts.
const real = mode === "real";
if (!url || !shots || !repo) {
  console.error("usage: e2e.ts <url#token=..> <shots-dir> <repo-path>");
  Deno.exit(2);
}

let step = 0;
const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
const M = "globalThis.__muster";
const R = (b: Browser, m: string, p: unknown) =>
  b.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
const shot = async (b: Browser, name: string) => {
  await b.sleep(300);
  await b.shot(`${shots}/${name}.png`);
};
function check(cond: unknown, msg: string) {
  if (!cond) throw new Error("check failed: " + msg);
}

/** The window's layout root, and its signature (`row([1,2] [3])`). */
const root = async (b: Browser): Promise<Layout> =>
  JSON.parse(await b.eval(`JSON.stringify(${M}.layout.value.root)`));
const layoutSig = async (b: Browser) => sig(await root(b));
const groups = (b: Browser): Promise<number> => b.eval(`document.querySelectorAll('.pane').length`);
const tabCount = (b: Browser): Promise<number> =>
  b.eval(`document.querySelectorAll('.tab[data-tab-sid]').length`);
const here = (b: Browser): Promise<number[]> => b.eval(`${M}.windowSessions.value.map((s) => s.id)`);
const focused = (b: Browser): Promise<number | null> => b.eval(`${M}.focusedSession.value?.id ?? null`);
/** Center of a tab / a group's body, in viewport px. */
const tabAt = async (b: Browser, sid: number) => {
  const r = await b.rect(`[data-tab-sid="${sid}"]`);
  check(r, `tab ${sid} on screen`);
  return [r!.x + Math.min(40, r!.w / 2), r!.y + r!.h / 2] as [number, number];
};
const bodyOf = (b: Browser, sid: number) =>
  b.eval(`(() => { const t = document.querySelector('[data-tab-sid="${sid}"]');
    const r = t.closest('.pane').querySelector('.pane-body').getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height }; })()`);

/** Never launch windows from a test: block window.show in the page. */
const guard = (b: Browser) =>
  b.eval(`(() => { const r = ${M}.rpc; if (r.__guarded) return; const call = r.call.bind(r);
    r.call = (m, p) => m === "window.show" ? Promise.reject(new Error("e2e: window.show blocked")) : call(m, p);
    r.__guarded = true; })()`);

async function open(port: number, dir: string): Promise<Browser> {
  const b = await launch(port, dir);
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.size(1440, 900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`, 10000);
  await guard(b);
  return b;
}

const a = await open(9341, "/tmp/muster-cdp-a");
let b2: Browser | null = null;
try {
  const hello: any = await R(a, "hello", {});
  if (!real && !String(hello.version).includes("mock")) {
    throw new Error(
      "e2e runs against the mock daemon (or pass `real` for an isolated real daemon; no agents start)",
    );
  }
  check(await a.eval(`${M}.windowId.value`) !== null, "the mock has windows");
  check(!(await a.eval(`location.href`)).includes("token="), "token stripped from URL");
  check(/^(\(\d+\) )?muster · \d+$/.test(await a.eval(`document.title`)), "title is muster · <window>");
  ok(`connected; token stripped; ${await a.eval(`document.title`)}`);

  // ---- empty window → first tab: a lone group with its strip
  if ((await here(a)).length === 0) {
    await a.wait(`!!document.querySelector('.empty-window')`);
    check(await a.eval(`!!document.querySelector('.empty-controls .jump-btn')`), "Jump in the empty window");
    check(await a.eval(`!document.querySelector('.win-split')`), "no split buttons without tabs");
    await shot(a, "01-empty-window");
    await a.eval(`(() => { const i = document.querySelector('.empty-window input');
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      set.call(i, ${JSON.stringify(repo)}); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
    await a.click(".empty-window .launch-btn:nth-child(3)");
    await a.wait(`${M}.windowSessions.value.length === 1 && !!document.querySelector('.pane .xterm')`, 8000);
    ok("empty window: folder + Shell opened the first tab");
  }
  // v2 daemons give sessions their own window; older ones don't (the UI degrades).
  const v2 = await a.eval(`[...${M}.sessions.value.values()].every((s) => 'window_id' in s)`);
  await a.sleep(400);
  check(
    await groups(a) === 1 && await a.eval(`document.querySelectorAll('.tabstrip').length`) === 1,
    "one group",
  );
  check(await tabCount(a) >= 1, "its strip shows the tab");
  check(await a.eval(`!!document.querySelector('.tabstrip .win-controls .jump-btn')`), "window controls");
  ok(`a lone group shows its tab strip with the window controls (${v2 ? "v2" : "pre-v2"} daemon)`);

  // ---- Alt+T: a new tab (new task) in the same group
  const n0 = (await here(a)).length;
  await a.click(".pane .xterm");
  await a.key("Alt+T");
  await a.wait(`${M}.windowSessions.value.length === ${n0 + 1}`, 6000);
  await a.wait(`document.querySelectorAll('.tab[data-tab-sid]').length === ${n0 + 1}`, 3000);
  check(await groups(a) === 1, "still one group");
  const t2 = (await here(a)).at(-1)!;
  check(await focused(a) === t2, "the new tab is the focused one");
  ok("Alt+T added a tab to the group");

  // ---- Alt+D: a new shell in the focused tab's task, in a new group on the right
  await a.key("Alt+D");
  await a.wait(`document.querySelectorAll('.pane').length === 2`, 6000);
  const t3 = await focused(a);
  check(t3 !== null && t3 !== t2, "the split's shell is focused");
  const tsig = await layoutSig(a);
  check(/^row\(\[[\d,]+\] \[\d+\]\)$/.test(tsig), `a row split: ${tsig}`);
  // Its task now has two sessions: both tabs show the session part ("name · shell").
  await a.wait(`document.querySelector('[data-tab-sid="${t3}"] .tab-sub') !== null`, 3000);
  await a.wait(`document.querySelector('[data-tab-sid="${t2}"] .tab-sub') !== null`, 3000);
  ok(`Alt+D split with a new group (${tsig}); the task's tabs show "task · session"`);
  await shot(a, "02-split");

  // ---- the top-right strip carries the window controls; its Split down button splits
  const cornerSid = await a.eval(
    `document.querySelector('.win-controls').closest('.pane').querySelector('[data-tab-sid]').dataset.tabSid`,
  );
  check(Number(cornerSid) === t3, "controls sit in the right-hand group");
  check(await a.eval(`document.querySelectorAll('.win-controls').length`) === 1, "only one set of controls");
  await a.click(`.win-split-btn[aria-label="Split down"]`);
  await a.wait(`document.querySelectorAll('.pane').length === 3`, 6000);
  const t4 = (await focused(a))!;
  const s3 = await layoutSig(a);
  check(new RegExp(`col\\(\\[${t3}\\] \\[${t4}\\]\\)`).test(s3), `split down beside ${t3}: ${s3}`);
  const corner2 = await a.eval(
    `document.querySelector('.win-controls').closest('.pane').querySelector('[data-tab-sid]').dataset.tabSid`,
  );
  check(Number(corner2) === t3, "the controls stay in the top-right group");
  ok(`the top-right strip's Split down button split the focused group (${s3})`);

  // ---- drag a tab onto another group's right edge: a new split beside that group
  const left = allPanes(await root(a))[0];
  check(left.tabs.length === 2, "the left group has two tabs");
  const dragged = left.tabs[0];
  const body = await bodyOf(a, t4);
  await a.drag(await tabAt(a, dragged), [body.x + body.w - 12, body.y + body.h / 2], 16);
  await a.wait(`document.querySelectorAll('.pane').length === 4`, 4000);
  const s4 = await layoutSig(a);
  check(s4.includes(`[${t4}] [${dragged}]`), `${dragged} split off to the right of ${t4}: ${s4}`);
  check(await focused(a) === dragged, "the dropped tab is focused");
  ok(`dragging a tab onto another group's right edge made a new split (${s4})`);

  // ---- drag a tab into another group's strip
  const strip = await a.eval(`(() => { const t = document.querySelector('[data-tab-sid="${t3}"]');
    const r = t.getBoundingClientRect(); return { x: r.right - 4, y: r.y + r.height / 2 }; })()`);
  await a.drag(await tabAt(a, dragged), [strip.x, strip.y], 16);
  await a.wait(`document.querySelectorAll('.pane').length === 3`, 4000);
  const s5 = await layoutSig(a);
  check(s5.includes(`[${t3},${dragged}]`), `${dragged} joined ${t3}'s strip after it: ${s5}`);
  ok(`dragging a tab into another group's strip moved it there; its emptied group closed (${s5})`);

  // ---- Ctrl+Alt+→ / ← move the focused tab to the neighbouring group
  await a.click(`[data-tab-sid="${t4}"]`);
  await a.wait(`${M}.focusedSession.value?.id === ${t4}`);
  await a.key("Ctrl+Alt+Left");
  await a.wait(`document.querySelectorAll('.pane').length === 2`, 3000);
  const s6 = await layoutSig(a);
  check(new RegExp(`^row\\(\\[[\\d,]*${t4}\\] \\[${t3},${dragged}\\]\\)$`).test(s6), `moved left: ${s6}`);
  await a.key("Ctrl+Alt+Right");
  await a.wait(
    `${M}.layout.value && JSON.stringify(${M}.layout.value.root).includes('${t3},${dragged},${t4}')`,
    3000,
  );
  ok(`Ctrl+Alt+← / → moved the tab between neighbouring groups (${s6} → ${await layoutSig(a)})`);

  // ---- tabs within a group: Alt+1, Ctrl+Tab, Alt+Shift+[
  await a.key("Alt+1");
  await a.wait(`${M}.focusedSession.value?.id === ${t3}`);
  await a.key("Ctrl+Tab");
  await a.wait(`${M}.focusedSession.value?.id === ${dragged}`);
  await a.key("Alt+Shift+[");
  await a.wait(`JSON.stringify(${M}.layout.value.root).includes('${dragged},${t3},${t4}')`, 3000);
  ok("Alt+1 / Ctrl+Tab switch tabs in the focused group; Alt+Shift+[ reorders");

  // ---- closing the last tab of a group removes the group
  await a.key("Ctrl+Alt+Left"); // `dragged` joins the left group
  await a.wait(`document.querySelectorAll('.pane').length === 2`);
  const g2 = allPanes(await root(a))[1].tabs;
  for (const sid of g2) {
    await a.click(`[data-tab-sid="${sid}"]`);
    await a.key("Alt+W");
    await a.wait(`!${M}.sessions.value.has(${sid})`, 4000);
  }
  await a.wait(`document.querySelectorAll('.pane').length === 1`, 3000);
  ok(`closing the last tab of a group removed the group (${await layoutSig(a)})`);

  // ---- an agent tab that needs approval, auto-named from its first prompt (mock agents only)
  const ws = await R(a, "workspace.add", { path: repo });
  const c: any = await R(a, "channel.create", {
    workspace_id: ws.id,
    agent: real ? "shell" : "claude",
    ...(real
      ? { name: "Fix the flaky login redirect test" }
      : { prompt: "Fix the flaky login redirect test" }),
    window_id: await a.eval(`${M}.windowId.value`),
    cols: 120,
    rows: 30,
  });
  await a.wait(`${M}.windowSessions.value.some((s) => s.channel_id === ${c.id})`, 6000);
  const agent: number = await a.eval(`${M}.windowSessions.value.find((s) => s.channel_id === ${c.id}).id`);
  await a.wait(
    `document.querySelector('[data-tab-sid="${agent}"] .tab-task')?.textContent === 'Fix the flaky login redirect test'`,
    4000,
  );
  ok("a new task's tab joined the focused group, named from its first prompt");
  if (!real) {
    await a.sleep(3800); // the fake agent goes idle
    await R(a, "session.send", { id: agent, text: "ask for approval" });
    await a.wait(`${M}.sessions.value.get(${agent}).state === 'needs_approval'`, 8000);
    await a.wait(`/^\\(1\\) muster/.test(document.title) && !!document.querySelector('.sl-needs')`, 3000);
    await a.click(`[data-tab-sid="${t2}"]`);
    await a.key("Alt+J");
    await a.wait(`${M}.focusedSession.value?.id === ${agent} && !!document.querySelector('.approval')`, 3000);
    ok("needs approval: (1) in the title, status line chip; Alt+J went there and shows the banner");
    await shot(a, "03-approval");
    await a.key("Alt+A");
    await a.wait(`${M}.sessions.value.get(${agent}).state !== 'needs_approval'`, 4000);
  }

  // ---- Jump: a tab of this window
  await a.click(`[data-tab-sid="${agent}"]`);
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("flaky");
  await a.sleep(200);
  await shot(a, "04-jump");
  await a.key("Escape");
  await a.wait(`!document.querySelector('.switcher')`);
  await a.click(`[data-tab-sid="${t2}"]`);
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("flaky");
  await a.sleep(150);
  await a.key("Enter");
  await a.wait(`${M}.focusedSession.value?.id === ${agent} && !document.querySelector('.switcher')`, 3000);
  await a.key("Ctrl+Shift+P");
  await a.wait(`document.querySelector('.sw-mode')?.textContent === 'Commands'`);
  await shot(a, "05-palette");
  await a.key("Escape");
  ok("Jump found a tab and went there; the command palette opens");

  // ---- rename by double-click (a task's main tab renames the task)
  const rr = await a.rect(`[data-tab-sid="${agent}"] .tab-name`);
  await a.clickAt(rr!.x + 20, rr!.y + rr!.h / 2, { count: 2 });
  await a.wait(`!!document.querySelector('.tab .inline-edit')`);
  await a.key("Ctrl+a");
  await a.type("renamed task");
  await a.key("Enter");
  await a.wait(
    `document.querySelector('[data-tab-sid="${agent}"] .tab-task')?.textContent === 'renamed task'`,
    4000,
  );
  ok("double-click renamed the tab's task");

  // ---- a second window; Jump there pulls a tab over
  b2 = await open(9342, "/tmp/muster-cdp-b");
  const w1 = await a.eval(`${M}.windowId.value`);
  const w2 = await b2.eval(`${M}.windowId.value`);
  check(w1 !== w2, "second client claimed another window");
  await b2.wait(`!!document.querySelector('.empty-window')`, 4000);
  await a.wait(`!!document.querySelector('.win-chip')`, 3000);
  ok(`second client claimed window ${w2} (first holds ${w1}), starts empty; window 1 now shows its chip`);

  // Dragging a tab shows "→ Window 2 / → New window" in the focused strip; Esc aborts.
  const sigBefore = await layoutSig(a);
  const [tx, ty] = await tabAt(a, t2);
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: tx, y: ty });
  await a.send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x: tx,
    y: ty,
    button: "left",
    clickCount: 1,
    buttons: 1,
  });
  for (let i = 1; i <= 8; i++) {
    await a.send("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x: tx + i * 30,
      y: ty + i * 40,
      button: "left",
      buttons: 1,
    });
    await a.sleep(16);
  }
  await a.wait(`document.querySelectorAll('.move-target').length === 2`, 2000);
  check(
    await a.eval(`[...document.querySelectorAll('.move-target')].map((e) => e.textContent).join('|')`) ===
      `→ Window ${w2}|→ New window`,
    "targets name the other window and a new one",
  );
  await shot(a, "06a-drag-targets");
  await a.key("Escape");
  await a.send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x: tx,
    y: ty,
    button: "left",
    clickCount: 1,
  });
  await a.wait(`!document.querySelector('.move-target') && !document.querySelector('.drag-ghost')`, 2000);
  check(await layoutSig(a) === sigBefore, "an aborted drag changes nothing");
  ok("dragging a tab shows → Window N / → New window targets; Esc aborts");
  await b2.key("Ctrl+Shift+K");
  await b2.wait(`!!document.querySelector('.switcher')`);
  await b2.type("renamed");
  await b2.sleep(150);
  await b2.key("Enter");
  await b2.wait(`${M}.windowSessions.value.some((s) => s.id === ${agent})`, 5000);
  await a.wait(`!${M}.windowSessions.value.some((s) => s.id === ${agent})`, 5000);
  await b2.wait(
    `!!document.querySelector('.pane .xterm') && ${M}.focusedSession.value?.id === ${agent}`,
    5000,
  );
  await a.wait(`!${M}.attached().includes(${agent})`, 3000);
  ok("Jump in window 2 pulled the tab over; window 1 lost it and detached its terminal");
  await shot(b2, "06-window2");
  if (v2) {
    // One tab of a two-tab task moves on its own: a second shell in t2's task, pulled by Jump.
    const task: number = await a.eval(`${M}.sessions.value.get(${t2}).channel_id`);
    const sib: any = await R(a, "session.spawn", {
      channel_id: task,
      kind: "shell",
      name: "sibling",
      window_id: w1,
    });
    await a.wait(`${M}.windowSessions.value.some((s) => s.id === ${sib.id})`, 4000);
    await a.wait(
      `document.querySelector('[data-tab-sid="${sib.id}"] .tab-sub')?.textContent === '· sibling'`,
      3000,
    );
    await b2.key("Ctrl+Space");
    await b2.wait(`!!document.querySelector('.switcher')`);
    await b2.type("sibling");
    await b2.sleep(150);
    await b2.key("Enter");
    await b2.wait(`${M}.windowSessions.value.some((s) => s.id === ${sib.id})`, 4000);
    await b2.wait(`${M}.focusedSession.value?.id === ${sib.id}`, 3000);
    check(
      await a.eval(`${M}.windowSessions.value.some((s) => s.id === ${t2})`),
      "its sibling stayed in window 1",
    );
    ok("Jump pulled one tab of a two-tab task into window 2 (session.set_window); its sibling stayed");
  }

  // ---- the agents panel (mock only: clicking an agent starts it)
  if (!real) {
    const menuItem = (b: Browser, label: string) =>
      b.eval(`(() => { const it = [...document.querySelectorAll('.menu .menu-item')]
        .find((e) => e.querySelector('.menu-label')?.textContent === ${JSON.stringify(label)});
        if (!it) return false; it.click(); return true; })()`);
    await a.click(`[data-tab-sid="${t2}"]`);
    await a.key("Alt+G");
    await a.wait(`!!document.querySelector('.agents-panel') && ${M}.layout.value.agents === true`, 3000);
    await a.wait(`document.querySelectorAll('.ap-row').length === ${M}.agents.value.length`, 3000);
    check(await a.eval(`${M}.agents.value.length`) >= 2, "the mock's agents are listed");
    check(
      await a.eval(`document.querySelector('[data-agent="orchestrator"] .ap-kind')?.textContent`) ===
        "Claude",
      "rows show the kind badge",
    );
    check(
      await a.eval(
        `document.querySelector('[data-agent="reviewer"] .ap-row-sub')?.textContent.startsWith('stopped')`,
      ),
      "a stopped agent says so",
    );
    ok("Alt+G opened the agents panel (kept in the window's layout); rows show dot, name, kind, state");

    // The message log: from → to, first line, origin and hop chips, refusals in red.
    const entries = await a.eval(`document.querySelectorAll('.ap-msg').length`);
    check(entries >= 3, `the log shows the mock's messages (${entries})`);
    check(
      await a.eval(
        `document.querySelector('.ap-msg.refused .ap-msg-refused')?.textContent.includes('accepts only human')`,
      ),
      "a refused message shows its reason",
    );
    check(
      await a.eval(
        `[...document.querySelectorAll('.ap-msg .ap-chip')].some((c) => c.textContent === 'schedule')`,
      ),
      "origin chips",
    );
    await R(a, "mock.agent_message", {
      to: "reviewer",
      from: "orchestrator",
      text: "Check the retry loop again",
      hop: 1,
    });
    await a.wait(
      `document.querySelector('.ap-msg .ap-msg-text')?.textContent === 'Check the retry loop again'`,
      3000,
    );
    check(
      await a.eval(
        `document.querySelector('.ap-msg .ap-msg-route')?.textContent.replace(/\\s+/g, ' ').trim()`,
      ) ===
        "orchestrator → reviewer",
      "newest first, from → to",
    );
    await shot(a, "09-agents-panel");
    await a.click(`.ap-msg[data-msg]:not(.refused)`);
    await a.wait(`!!document.querySelector('.msg-dlg .msg-pre')`, 2000);
    check(
      await a.eval(`document.querySelector('.msg-dlg .msg-pre').textContent`) ===
        "Check the retry loop again",
      "the overlay shows the full message",
    );
    await a.key("Escape");
    await a.click(`.ap-msg.refused`);
    await a.wait(`!!document.querySelector('.msg-dlg .msg-refused')`, 2000);
    await a.key("Escape");
    await a.wait(`!document.querySelector('.msg-dlg')`);
    ok("the message log lists messages live; an entry opens its full text and reply; refusals show why");

    // Click a stopped agent: it starts, and its tab opens in this window's focused group.
    const before = await here(a);
    await a.click(`[data-agent="reviewer"]`);
    await a.wait(`${M}.focusedSession.value?.name === 'reviewer'`, 6000);
    const rev: number = await a.eval(`${M}.focusedSession.value.id`);
    check(!before.includes(rev) && (await here(a)).includes(rev), "a new tab for the agent, here");
    await a.wait(
      `!document.querySelector('[data-agent="reviewer"] .ap-row-sub')?.textContent.startsWith('stopped')`,
      4000,
    );
    check(
      await a.eval(
        `[...document.querySelectorAll('.ap-msg')].every((m) => m.querySelector('.ap-msg-route').textContent.includes('reviewer'))`,
      ),
      "selecting the agent filters the log to it",
    );
    check(!!(await a.eval(`document.querySelector('.ap-filter')`)), "the filter chip shows");
    ok("clicking a stopped agent started it in a tab of the focused group and filtered the log");
    await shot(a, "09b-agents-running");

    // From window 2: clicking it pulls its tab over, as Jump does.
    await b2.key("Alt+G");
    await b2.wait(`!!document.querySelector('.agents-panel')`, 3000);
    check(await a.eval(`!!document.querySelector('.agents-panel')`), "window 1 keeps its own panel");
    await b2.click(`[data-agent="reviewer"]`);
    await b2.wait(
      `${M}.windowSessions.value.some((s) => s.id === ${rev}) && ${M}.focusedSession.value?.id === ${rev}`,
      5000,
    );
    await a.wait(`!${M}.windowSessions.value.some((s) => s.id === ${rev})`, 5000);
    check(await b2.eval(`${M}.sessions.value.get(${rev}).state !== 'exited'`), "pulled, not restarted");
    ok("clicking a running agent in window 2 pulled its tab into window 2's focused group");
    await b2.key("Alt+G");
    await b2.wait(`!document.querySelector('.agents-panel')`, 2000);

    // Jump lists every agent; choosing a stopped one starts it.
    await a.key("Ctrl+Space");
    await a.wait(`!!document.querySelector('.switcher')`);
    await a.type("orchestr");
    await a.sleep(150);
    check(
      await a.eval(`document.querySelector('.sw-item.sel .sw-sub')?.textContent.startsWith('agent')`),
      "Jump lists the agent",
    );
    await a.key("Enter");
    await a.wait(`${M}.focusedSession.value?.name === 'orchestrator'`, 6000);
    ok("Jump listed a stopped agent; choosing it started it and showed its tab here");

    // New agent from a template (palette), with the unattended-use warning.
    await a.key("Ctrl+Shift+P");
    await a.wait(`!!document.querySelector('.switcher')`);
    await a.type("new agent");
    await a.sleep(150);
    await a.key("Enter");
    await a.wait(`!!document.querySelector('.agent-dlg [data-tpl="researcher"]')`, 3000);
    await a.click(`.agent-dlg [data-tpl="researcher"]`);
    await a.wait(`document.querySelector('.agent-dlg .field-row input').value === 'researcher'`, 2000);
    check(
      await a.eval(`document.querySelector('.agent-dlg .persona-input').value.includes('plan mode')`),
      "the template prefilled the persona",
    );
    check(await a.eval(`!document.querySelector('.agent-warn')`), "no warning for human-only");
    await a.eval(
      `[...document.querySelectorAll('.agent-dlg .check')].find((l) => l.textContent.includes('Schedule')).querySelector('input').click()`,
    );
    await a.wait(`!!document.querySelector('.agent-warn')`, 1000);
    check(
      await a.eval(
        `document.querySelector('.agent-warn').textContent.includes("consumer terms are unclear")`,
      ),
      "the warning names the reason",
    );
    await shot(a, "10-new-agent");
    await a.eval(
      `[...document.querySelectorAll('.agent-dlg .check')].find((l) => l.textContent.includes('Schedule')).querySelector('input').click()`,
    );
    await a.wait(`!document.querySelector('.agent-warn')`, 1000);
    await a.key("Ctrl+Enter");
    await a.wait(
      `!document.querySelector('.agent-dlg') && !!document.querySelector('[data-agent="researcher"]')`,
      3000,
    );
    const made: any = (await R(a, "agent.list", {})).find((x: any) => x.name === "researcher");
    check(
      made && made.permissions.claude_mode === "plan" && made.accepts_origins.join() === "human",
      "created as the template says",
    );
    ok("New agent: the Researcher template prefilled the dialog; schedule on a Claude agent warns; created");

    // Edit ▸ Memory: list, open, edit and save a file.
    await a.click(`[data-agent="orchestrator"]`, { button: "right" });
    await a.wait(`!!document.querySelector('.menu')`);
    check(await menuItem(a, "Memory…"), "the row's menu has Memory…");
    await a.wait(`document.querySelectorAll('.mem-file').length >= 2`, 3000);
    await a.eval(
      `[...document.querySelectorAll('.mem-file')].find((b) => b.textContent === 'notes.md').click()`,
    );
    await a.wait(`document.querySelector('.mem-text')?.value.startsWith('# Notes')`, 2000);
    await a.click(".mem-text");
    await a.key("Ctrl+a");
    await a.type("# Notes\nsaved from the panel");
    await a.wait(`!!document.querySelector('.mem-dirty')`, 1000);
    // The draft survives a trip to the Settings tab, and closing asks before discarding it.
    await a.eval(
      `[...document.querySelectorAll('.dlg-tab')].find((t) => t.textContent === 'Settings').click()`,
    );
    await a.wait(`document.querySelector('.persona-input')?.offsetParent !== null`, 1000);
    await a.eval(
      `[...document.querySelectorAll('.dlg-tab')].find((t) => t.textContent === 'Memory').click()`,
    );
    await a.wait(`document.querySelector('.mem-text')?.value.endsWith('saved from the panel')`, 1000);
    await a.key("Escape");
    await a.wait(`!!document.querySelector('.mem-discard') && !!document.querySelector('.agent-dlg')`, 1000);
    await a.eval(
      `[...document.querySelectorAll('.dlg-foot .btn')].find((b) => b.textContent === 'Keep editing').click()`,
    );
    await a.wait(`!document.querySelector('.mem-discard')`, 1000);
    await a.click(".mem-save");
    await a.wait(`!document.querySelector('.mem-dirty')`, 2000);
    const saved: any = await R(a, "agent.memory.read", { name: "orchestrator", path: "notes.md" });
    check(saved.text === "# Notes\nsaved from the panel", `saved: ${JSON.stringify(saved.text)}`);
    await shot(a, "11-agent-memory");
    await a.key("Escape");
    await a.wait(`!document.querySelector('.agent-dlg')`, 2000);
    ok("Edit ▸ Memory listed memory/, kept a draft across tabs, asked before discarding it, and saved");
    await a.sleep(500); // the layout (with the panel flag) saves debounced
  }

  // ---- reload keeps the window (and, on v2 daemons, its groups)
  await a.click(".pane .xterm");
  await a.key("Alt+Shift+D");
  await a.wait(`document.querySelectorAll('.pane').length === 2`, 6000);
  const before = await layoutSig(a);
  await a.sleep(700); // the layout save is debounced (400ms)
  await a.goto("about:blank");
  await a.sleep(300);
  await a.send("Page.navigate", { url: url.replace(/#.*/, "") });
  await a.wait(`!!document.querySelector('.app') && ${M}.layout.value !== null`, 8000);
  await guard(a);
  check(await a.eval(`${M}.windowId.value`) === w1, "reload reclaimed the same window");
  if (v2) {
    const after = await layoutSig(a);
    check(after === before, `the groups came back: ${before} → ${after}`);
  }
  ok(`reload reclaimed the same window${v2 ? " and its groups" : ""} (${await layoutSig(a)})`);
  if (!real && v2) {
    await a.wait(`!!document.querySelector('.agents-panel')`, 3000);
    ok("the agents panel came back open with the window (per-window, in its layout)");
  }

  // ---- light theme, shortcuts sheet
  await a.send("Emulation.setEmulatedMedia", {
    features: [{ name: "prefers-color-scheme", value: "light" }],
  });
  await a.sleep(300);
  await shot(a, "07-light");
  await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await a.key("F1");
  await a.wait(`!!document.querySelector('.shortcuts')`);
  await shot(a, "08-shortcuts");
  await a.key("Escape");
  ok("light theme and shortcuts sheet");

  const errors = [...a.console, ...(b2?.console ?? [])].filter((l) => /^(error|exception)/.test(l));
  if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
  ok("no console errors");
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
