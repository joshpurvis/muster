// End-to-end UI walkthrough against the MOCK daemon (dev only). Two headless browsers act as two
// client windows. Covers the v2 layout: a window split into groups, each with its own tab strip;
// a tab is one session. Also the agents panel (Alt+G, start-and-pull, the message log, New
// agent from a template, Edit ▸ Memory), agent panels and the built-in panel apps.
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots>,src \
//     --allow-write=<shots>,/tmp dev/e2e.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path>
//
// It refuses to run against anything but the mock (a real daemon would start real agents), and
// blocks `window.show` in the pages it drives, so it can never launch a window.
import { type Browser, launch } from "./cdp.ts";
import { allPanes, type Layout, sig } from "../src/layout/tree.ts";
import { panelChecks } from "./e2e_panels.ts";
import { noteChecks } from "./e2e_notes.ts";
import { scheduleChecks } from "./e2e_schedules.ts";
import { cacheChecks } from "./e2e_cache.ts";
import { panelApps } from "./panel_apps_e2e.ts";

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

  // ---- rename by double-click (a one-session task's tab shows, so renames, the task)
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
  await a.wait(`document.querySelector('.sl-task')?.textContent === 'renamed task'`, 3000);
  ok("the status line names the focused tab's task");

  // ---- a task with two sessions: Rename edits `[task] · [session]` from any of its tabs
  const extra: any = await R(a, "session.spawn", {
    channel_id: c.id,
    kind: "shell",
    window_id: await a.eval(`${M}.windowId.value`),
    cols: 120,
    rows: 30,
  });
  const sh: number = extra.id;
  await a.wait(
    `!!document.querySelector('[data-tab-sid="${sh}"] .tab-sub') && !!document.querySelector('[data-tab-sid="${agent}"] .tab-sub')`,
    5000,
  );
  // Record the rename calls the page makes from here on.
  await a.eval(`(() => { const r = ${M}.rpc; r.__renames = []; if (r.__countsRenames) return;
    const call = r.call.bind(r);
    r.call = (m, p) => { if (/\.rename$/.test(m)) r.__renames.push(m + ":" + p.name); return call(m, p); };
    r.__countsRenames = true; })()`);
  const renames = async () => JSON.stringify(await a.eval(`${M}.rpc.__renames`));
  const fieldFocus = (): Promise<string | null> =>
    a.eval(`document.activeElement?.getAttribute('aria-label') ?? null`);
  const fields = (n: number) =>
    a.wait(`document.querySelectorAll('.tab .inline-edit').length === ${n}`, 3000);
  const tabFocused = async (sid: number) =>
    await a.eval(`document.activeElement?.dataset?.tabSid ?? null`) === String(sid);
  const sub = (sid: number): Promise<string> =>
    a.eval(`document.querySelector('[data-tab-sid="${sid}"] .tab-sub')?.textContent ?? ''`);
  const pickMenu = async (label: string) => {
    await a.wait(`!!document.querySelector('.menu')`, 2000);
    await a.eval(`(() => { const it = [...document.querySelectorAll('.menu .menu-item')]
      .find((e) => e.querySelector('.menu-label')?.textContent === ${JSON.stringify(label)});
      it.setAttribute('data-e2e-pick', ''); })()`);
    await a.click(`[data-e2e-pick]`);
  };

  // Double-click on the task's main tab: both fields, the task's focused; Tab moves, Esc cancels.
  const ra = await a.rect(`[data-tab-sid="${agent}"] .tab-name`);
  await a.clickAt(ra!.x + 10, ra!.y + ra!.h / 2, { count: 2 });
  await fields(2);
  const tags = await a.eval(`[...document.querySelectorAll('.tab .inline-edit-tag')].map((e) => e.textContent).join()`);
  check(tags === "Task,Tab", `each field is labelled: ${tags}`);
  check(await fieldFocus() === "Task name", "the main tab starts in the task field");
  await a.key("Tab");
  check(await fieldFocus() === "Tab name", "Tab moves to the session field");
  await a.key("Shift+Tab");
  check(await fieldFocus() === "Task name", "Shift+Tab moves back");
  await a.type("ignored");
  await shot(a, "05b-rename-two-fields");
  await a.key("Escape");
  await fields(0);
  check(await renames() === "[]", "moving between the fields and Esc renamed nothing");
  check(await tabFocused(agent), "Esc gives the focus back to the tab");
  ok("a two-session task's main tab renames `[task] · [session]`, starting in the task field");

  // F2 on the other tab: starts in the session field; Enter renames only what changed.
  await a.click(`[data-tab-sid="${sh}"]`);
  await a.wait(`${M}.focusedSession.value?.id === ${sh}`);
  await a.eval(`document.querySelector('[data-tab-sid="${sh}"]').focus()`);
  await a.key("F2");
  await fields(2);
  check(await fieldFocus() === "Tab name", "a secondary tab starts in the session field");
  await a.key("Ctrl+a");
  await a.type("tests");
  await a.key("Enter");
  await fields(0);
  await a.wait(
    `document.querySelector('[data-tab-sid="${sh}"] .tab-sub')?.textContent.includes('tests')`,
    4000,
  );
  check(await renames() === `["session.rename:tests"]`, "Enter renamed only the session");
  check(await tabFocused(sh), "Enter gives the focus back to the tab");
  ok("F2 on a secondary tab starts in the session field; Enter renamed just the session");

  // Tab menu → Rename: a blanked task field beside a changed session renames only the session.
  const rs = await a.rect(`[data-tab-sid="${sh}"]`);
  await a.clickAt(rs!.x + 20, rs!.y + rs!.h / 2, { button: "right" });
  await pickMenu("Rename");
  await fields(2);
  await a.key("Ctrl+a");
  await a.type("logs");
  await a.key("Shift+Tab");
  await a.key("Ctrl+a");
  await a.key("Backspace");
  await a.key("Enter");
  await fields(0);
  await a.wait(
    `document.querySelector('[data-tab-sid="${sh}"] .tab-sub')?.textContent.includes('logs')`,
    4000,
  );
  check(await renames() === `["session.rename:tests","session.rename:logs"]`, "only the session renamed");
  check(
    await a.eval(`document.querySelector('[data-tab-sid="${agent}"] .tab-task')?.textContent`) ===
      "renamed task",
    "the blank task field left the task alone",
  );
  ok("tab menu → Rename: a blank field is left alone, the changed one renamed");

  // Terminal menu → Rename: Enter renames the task (every tab of it), the terminal gets the keys.
  const bsh = await bodyOf(a, sh);
  await a.clickAt(bsh.x + bsh.w / 2, bsh.y + bsh.h / 2, { button: "right" });
  await pickMenu("Rename");
  await fields(2);
  await a.key("Shift+Tab");
  await a.key("Ctrl+a");
  await a.type("renamed task two");
  await a.key("Enter");
  await fields(0);
  await a.wait(
    `document.querySelector('[data-tab-sid="${agent}"] .tab-task')?.textContent === 'renamed task two'`,
    4000,
  );
  check(
    await renames() === `["session.rename:tests","session.rename:logs","channel.rename:renamed task two"]`,
    "only the task renamed",
  );
  check(
    await a.eval(`!!document.activeElement?.closest('.xterm') &&
      !!document.activeElement.closest('.pane')?.querySelector('[data-tab-sid="${sh}"]')`),
    "Enter gives the focus back to the terminal",
  );
  ok("terminal menu → Rename renamed the task on every tab and returned to the terminal");

  // Clicking away commits.
  const ra2 = await a.rect(`[data-tab-sid="${agent}"] .tab-name`);
  await a.clickAt(ra2!.x + 10, ra2!.y + ra2!.h / 2, { count: 2 });
  await fields(2);
  await a.key("Tab");
  await a.key("Ctrl+a");
  await a.type("main");
  await a.click(".pane .xterm");
  await fields(0);
  await a.wait(
    `document.querySelector('[data-tab-sid="${agent}"] .tab-sub')?.textContent.includes('main')`,
    4000,
  );
  check((await renames()).endsWith(`"session.rename:main"]`), "click-away renamed the session");
  check(!(await sub(agent)).includes("ignored"), "the cancelled edit stayed cancelled");
  ok("clicking away commits the rename");

  // Right-click over a program that tracks the mouse: Muster's menu, and the program gets no
  // right-button report (left clicks still reach it).
  await a.eval(`(() => { const r = ${M}.rpc; r.__sent = []; if (r.__countsSent) return;
    const send = r.sendInput.bind(r);
    r.sendInput = (sid, d) => {
      r.__sent.push(sid + ":" + (typeof d === "string" ? d : new TextDecoder().decode(d)));
      return send(sid, d);
    };
    const call = r.call.bind(r);
    r.call = (m, p) => { if (/^session\.(interrupt|restart)$/.test(m)) r.__sent.push(p.id + ":" + m); return call(m, p); };
    r.__countsSent = true; })()`);
  const sent = (): Promise<string[]> => a.eval(`${M}.rpc.__sent.splice(0)`);
  await a.click(`[data-tab-sid="${sh}"]`);
  await a.eval(`${M}.term(${sh}).write("\\x1b[?1000h\\x1b[?1006h")`);
  await a.wait(`${M}.term(${sh}).modes.mouseTrackingMode !== 'none'`, 3000);
  const bm = await bodyOf(a, sh);
  await sent();
  await a.clickAt(bm.x + bm.w / 2, bm.y + bm.h / 2);
  check((await sent()).some((x) => x.startsWith(`${sh}:\x1b[<0;`)), "a left click reached the program");
  await a.clickAt(bm.x + bm.w / 2, bm.y + bm.h / 2, { button: "right" });
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  check(
    await a.eval(
      `[...document.querySelectorAll('.menu .menu-label')].some((e) => e.textContent === 'Find…')`,
    ),
    "the terminal menu opened",
  );
  check((await sent()).length === 0, "the right-click sent the program nothing");
  await a.key("Escape");
  ok("right-click over a mouse-tracking program opens the terminal menu and sends it nothing");

  // A chord: xterm keeps reporting from the whole document while a button is held, so the right
  // button's drag and release outside the terminal must not reach the program either; xterm's
  // listeners must still be removed once every button is up. Drag tracking (?1002h) on.
  await a.eval(`${M}.term(${sh}).write("\\x1b[?1002h")`);
  await a.wait(`${M}.term(${sh}).modes.mouseTrackingMode === 'drag'`, 3000);
  const cx = bm.x + bm.w / 2, cy = bm.y + bm.h / 2, ox = cx, oy = bm.y - 12; // oy: the tab strip
  const mouse = (type: string, x: number, y: number, button: string, buttons: number) =>
    a.send("Input.dispatchMouseEvent", { type, x, y, button, buttons, clickCount: 1 });
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy });
  await sent();
  await mouse("mousePressed", cx, cy, "left", 1);
  await mouse("mousePressed", cx, cy, "right", 3);
  await mouse("mouseMoved", cx + 30, cy, "left", 3);
  await mouse("mouseMoved", ox, oy, "left", 3);
  await mouse("mouseReleased", ox, oy, "left", 2);
  await mouse("mouseMoved", ox + 20, oy, "right", 2); // right-only drag
  await mouse("mouseMoved", ox + 40, oy, "right", 2);
  await mouse("mouseReleased", ox + 40, oy, "right", 0);
  await a.sleep(150);
  const chord = await sent();
  // deno-lint-ignore no-control-regex
  const codes = chord.map((x) => /^\d+:\x1b\[<(\d+);/.exec(x)?.[1]).filter((c) => c !== undefined).map(
    Number,
  );
  check(codes.includes(0) && codes.includes(32), `the left press and drag reached the program (${codes})`);
  check(!codes.some((c) => (c & 195) === 2), `no right-button report reached the program (${codes})`);
  // xterm's document listeners are gone: a stray left release outside reports nothing.
  await mouse("mouseReleased", ox, oy, "left", 0);
  await a.sleep(100);
  check((await sent()).length === 0, "xterm's document listeners were removed after the chord");
  await a.key("Escape");
  await a.eval(`${M}.term(${sh}).write("\\x1b[?1002l\\x1b[?1000l\\x1b[?1006l")`);
  ok(`a left+right chord dragged out of the terminal sent only left reports (${codes.length})`);

  if (!real) {
    // Interrupt on a working agent asks first; Cancel (focused) sends nothing. An idle one doesn't ask.
    await R(a, "session.send", { id: agent, text: "ask for approval" });
    await a.wait(`${M}.sessions.value.get(${agent}).state === 'needs_approval'`, 8000);
    const ri = await a.rect(`[data-tab-sid="${agent}"]`);
    await a.clickAt(ri!.x + 20, ri!.y + ri!.h / 2, { button: "right" });
    await pickMenu("Interrupt");
    await a.wait(`!!document.querySelector('.confirm-dlg')`, 2000);
    check(
      await a.eval(`document.querySelector('.confirm-dlg h2')?.textContent`) === "Claude is working" &&
        await a.eval(`document.querySelector('.confirm-dlg .dlg-body')?.textContent`) ===
          "Stop its current turn?",
      "the dialog names the agent and asks",
    );
    check(await a.eval(`document.activeElement?.textContent`) === "Cancel", "Cancel is focused");
    check(
      await a.eval(`(() => { const d = document.querySelector('.confirm-dlg');
        return document.getElementById(d.getAttribute('aria-describedby'))?.textContent; })()`) ===
        "Stop its current turn?",
      "the question describes the dialog for screen readers",
    );
    await a.key("Enter");
    await a.wait(`!document.querySelector('.confirm-dlg')`, 2000);
    await a.sleep(200);
    check((await sent()).length === 0, "Cancel sent nothing");
    // Restart from the tab menu asks too, with its own warning.
    await a.clickAt(ri!.x + 20, ri!.y + ri!.h / 2, { button: "right" });
    await pickMenu("Restart");
    await a.wait(`!!document.querySelector('.confirm-dlg')`, 2000);
    check(
      // A Claude tab reopens its conversation; other agents start fresh.
      /^Restart it\? Its current turn is lost(; it reopens the same conversation| and the program starts fresh)\.$/.test(
        await a.eval(`document.querySelector('.confirm-dlg .dlg-body')?.textContent`),
      ) &&
        await a.eval(`document.querySelector('.confirm-dlg .btn-danger')?.textContent`) === "Restart",
      "Restart warns the turn is lost",
    );
    await a.click(`.confirm-dlg .dlg-foot .btn:not(.btn-danger)`);
    await a.wait(`!document.querySelector('.confirm-dlg')`, 2000);
    await a.sleep(200);
    check((await sent()).length === 0, "Cancel on Restart sent nothing");
    await R(a, "session.approve", { id: agent, decision: "deny" });
    await a.wait(`${M}.sessions.value.get(${agent}).state === 'idle'`, 4000);
    await a.clickAt(ri!.x + 20, ri!.y + ri!.h / 2, { button: "right" });
    await pickMenu("Interrupt");
    await a.sleep(200);
    check(await a.eval(`!document.querySelector('.confirm-dlg')`), "an idle agent doesn't ask");
    check((await sent()).includes(`${agent}:session.interrupt`), "an idle agent is interrupted at once");
    ok(
      "tab menu: Interrupt and Restart on a working agent ask first (Cancel sends nothing); idle doesn't ask",
    );

    // The terminal menu asks too; confirming acts, once.
    await R(a, "session.send", { id: agent, text: "ask for approval" });
    await a.wait(`${M}.sessions.value.get(${agent}).state === 'needs_approval'`, 8000);
    await a.click(`[data-tab-sid="${agent}"]`);
    const ab = await bodyOf(a, agent);
    const termMenu = async (label: string) => {
      await a.clickAt(ab.x + ab.w / 2, ab.y + ab.h / 2, { button: "right" });
      await pickMenu(label);
      await a.wait(`!!document.querySelector('.confirm-dlg')`, 2000);
    };
    await sent();
    await termMenu("Interrupt");
    check(
      await a.eval(`document.querySelector('.confirm-dlg .dlg-body')?.textContent`) ===
        "Stop its current turn?",
      "terminal menu Interrupt asks",
    );
    await a.click(`.confirm-dlg .btn-danger`);
    await a.wait(`!document.querySelector('.confirm-dlg')`, 2000);
    await a.sleep(200);
    check(
      JSON.stringify(await sent()) === JSON.stringify([`${agent}:session.interrupt`]),
      "confirming interrupted once",
    );
    await termMenu("Restart");
    check(
      (await a.eval<string>(`document.querySelector('.confirm-dlg .dlg-body')?.textContent ?? ''`))
        .startsWith("Restart it?"),
      "terminal menu Restart asks",
    );
    await a.key("Enter"); // Cancel is focused
    await a.wait(`!document.querySelector('.confirm-dlg')`, 2000);
    await a.sleep(200);
    check((await sent()).length === 0, "Cancel on the terminal menu's Restart sent nothing");
    await termMenu("Restart");
    await a.click(`.confirm-dlg .btn-danger`);
    await a.wait(`!document.querySelector('.confirm-dlg')`, 2000);
    await a.sleep(300);
    check(
      JSON.stringify(await sent()) === JSON.stringify([`${agent}:session.restart`]),
      "confirming restarted once",
    );
    await a.wait(`${M}.sessions.value.get(${agent}).state === 'idle'`, 10000);
    ok("terminal menu: Interrupt and Restart on a working agent ask first; confirming sends it once");
  }

  // Back to one session: the tab shows (and renames) just the task again.
  await a.click(`[data-tab-sid="${sh}"]`);
  await a.key("Alt+W");
  await a.wait(`!${M}.sessions.value.has(${sh})`, 4000);
  await a.wait(`!document.querySelector('[data-tab-sid="${agent}"] .tab-sub')`, 3000);

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

    // Closing an agent's tab only hides it: no confirm, the agent runs on, its row shows it again.
    await b2.key("Alt+W");
    await b2.wait(`!${M}.windowSessions.value.some((s) => s.id === ${rev})`, 3000);
    check(
      await b2.eval(
        `${M}.sessions.value.get(${rev})?.hidden === true && ${M}.sessions.value.get(${rev}).state !== 'exited'`,
      ),
      "the agent's session runs on, hidden",
    );
    check(!(await here(a)).includes(rev) && !(await here(b2)).includes(rev), "a tab in no window");
    check(await b2.eval(`!document.querySelector('.confirm-dlg') && !document.querySelector('.confirm-close')`), "no confirm");
    check(
      !(await b2.eval(`document.querySelector('[data-agent="reviewer"] .ap-row-sub')?.textContent.startsWith('stopped')`)),
      "its row still shows it running",
    );
    await b2.click(`[data-agent="reviewer"]`);
    await b2.wait(
      `${M}.focusedSession.value?.id === ${rev} && !${M}.sessions.value.get(${rev}).hidden`,
      5000,
    );
    check(await b2.eval(`${M}.sessions.value.get(${rev}).state !== 'exited'`), "the same session, not restarted");
    ok("closing an agent's tab hid it (still running); clicking its row brought the same session back");

    // A window that doesn't know the agents yet (its list still loading) can't stop one either:
    // the daemon hides it on `session.remove`; the toast's Show brings it back.
    await b2.click(`[data-tab-sid="${rev}"]`);
    await b2.eval(`(() => { globalThis.__agents = ${M}.agents.value; ${M}.agents.value = []; })()`);
    await b2.key("Alt+W");
    // A busy agent asks first here (this window can't tell it is an agent): confirm.
    await b2.sleep(300);
    if (await b2.eval(`!!document.querySelector('.confirm-close')`)) await b2.key("Enter");
    await b2.wait(`${M}.sessions.value.get(${rev})?.hidden === true`, 3000).catch(async (e) => {
      console.error(await b2!.eval(`JSON.stringify({ s: ${M}.sessions.value.get(${rev}), focus: ${M}.focusedSession.value?.id,
        active: document.activeElement?.className, toasts: [...document.querySelectorAll('.toast')].map((t) => t.textContent) })`));
      throw e;
    });
    check(await b2.eval(`${M}.sessions.value.get(${rev}).state !== 'exited'`), "removed by an unknowing window: hidden, still running");
    await b2.eval(`${M}.agents.value = globalThis.__agents`);
    await b2.wait(`!!document.querySelector('.toast .toast-action')`, 2000);
    check(
      await b2.eval(`document.querySelector('.toast-action').closest('.toast').textContent.includes('keeps running')`),
      "the toast says it keeps running",
    );
    await b2.click(`.toast .toast-action`);
    await b2.wait(`${M}.focusedSession.value?.id === ${rev} && !${M}.sessions.value.get(${rev}).hidden`, 5000);
    ok("closing before the agent list loaded still only hid it; the toast's Show brought it back");

    // Stop (the row's menu) is what stops it; the tab stays, showing it stopped.
    await b2.click(`[data-agent="reviewer"]`, { button: "right" });
    await b2.wait(`!!document.querySelector('.menu')`);
    check(await menuItem(b2, "Stop"), "the row's menu has Stop");
    await b2.wait(`${M}.sessions.value.get(${rev}).state === 'exited'`, 5000);
    await b2.wait(
      `document.querySelector('[data-agent="reviewer"] .ap-row-sub')?.textContent.startsWith('stopped')`,
      3000,
    );
    check((await here(b2)).includes(rev), "its tab stays");
    ok("Stop in the agent's menu stopped it");
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
    // Model and Effort are selects: the CLI's list, Default, and Custom… for anything else.
    const selOf = (label: string) =>
      `document.getElementById([...document.querySelectorAll('.agent-dlg label.field-label')]` +
      `.find((l) => l.textContent === '${label}').htmlFor)`;
    const [ms, es] = [selOf("Model"), selOf("Effort")];
    const choose = (sel: string, value: string) =>
      a.eval(`(() => { const s = ${sel}; s.value = ${JSON.stringify(value)};
        s.dispatchEvent(new Event('change', { bubbles: true })); })()`);
    await a.wait(`${ms}.options.length > 2`, 2000);
    const levels = await a.eval(`[...${es}.options].map((o) => o.textContent).join()`);
    check(
      levels === "Default (the CLI's choice),low,medium,high,xhigh,max,Custom…",
      `Effort lists ${levels}`,
    );
    await choose(ms, "opus");
    await choose(es, "xhigh");
    await shot(a, "10a-new-agent-models");
    await choose(ms, " custom");
    await a.wait(`document.activeElement?.getAttribute('aria-label') === 'Custom model'`, 1000);
    await a.type("claude-opus-5-5");
    check(
      await a.eval(`document.querySelector('[aria-label="Custom model"]').value === 'claude-opus-5-5'`),
      "the custom field started from opus, selected, so typing replaced it",
    );
    await shot(a, "10b-new-agent-custom-model");
    // Switching to Codex: the custom model goes back to Default; a level Codex also lists stays.
    const kindBtn = (k: string) =>
      `[...document.querySelectorAll('.agent-dlg .seg-btn')].find((b) => b.textContent.includes('${k}'))`;
    await a.eval(`${kindBtn("Codex")}.click()`);
    await a.sleep(100);
    check(
      await a.eval(`${ms}.value === '' && ${es}.value === 'xhigh'`) &&
        await a.eval(`!document.querySelector('[aria-label="Custom model"]')`),
      "switching to Codex reset the custom model and kept a level Codex has",
    );
    // Effort follows the model: gpt-6-luna has no "ultra", so picking it resets a picked ultra.
    await choose(ms, "gpt-6.1-sol");
    await choose(es, "ultra");
    await choose(ms, "gpt-6-luna");
    await a.sleep(100);
    check(
      await a.eval(`${es}.value === '' && ![...${es}.options].some((o) => o.value === 'ultra')`),
      "gpt-6-luna offers no ultra; the picked ultra went back to Default",
    );
    // Custom stays chosen with its field cleared, and clicking the current kind changes nothing.
    await choose(ms, " custom");
    await a.wait(`document.activeElement?.getAttribute('aria-label') === 'Custom model'`, 1000);
    await a.key("Backspace");
    await a.sleep(100);
    await a.eval(`${kindBtn("Codex")}.click()`);
    await a.sleep(100);
    check(
      await a.eval(
        `${ms}.value === ' custom' && document.querySelector('[aria-label="Custom model"]')?.value === ''`,
      ),
      "Custom stays chosen with an empty field, also after clicking the kind already chosen",
    );
    await a.type("gpt-6-luna-preview");
    await a.eval(`${kindBtn("Codex")}.click()`);
    await a.sleep(100);
    check(
      await a.eval(`document.querySelector('[aria-label="Custom model"]')?.value === 'gpt-6-luna-preview'`),
      "clicking the current kind keeps a custom model",
    );
    await a.eval(`${kindBtn("Claude")}.click()`);
    await a.sleep(100);
    await choose(ms, "sonnet");
    await choose(es, "");
    // An eval'd click doesn't move focus (the custom field it removed had it): Ctrl+Enter below
    // goes from the Model select, as after a real pick.
    await a.eval(`${ms}.focus()`);
    ok("Model and Effort: dropdowns per CLI, Custom… for other ids, a kind switch resets what doesn't fit");
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
      made && made.permissions.claude_mode === "plan" && made.accepts_origins.join() === "human" &&
        made.model === "sonnet" && made.effort === null,
      "created as the template says, with the chosen model and the default effort",
    );
    ok("New agent: the Researcher template prefilled the dialog; schedule on a Claude agent warns; created");

    // Provider: OpenRouter is off without the backend's key (the hint says how to set it); with
    // it, the Model field is a searchable list of OpenRouter's models grouped by vendor, and the
    // agent gets the pay-per-use badge.
    if (!real) {
      const newAgentDialog = async () => {
        await a.key("Ctrl+Shift+P");
        await a.wait(`!!document.querySelector('.switcher')`);
        await a.type("new agent");
        await a.sleep(150);
        await a.key("Enter");
        await a.wait(`!!document.querySelector('.agent-dlg [data-provider="openrouter"]')`, 3000);
      };
      await newAgentDialog();
      check(
        await a.eval(`document.querySelector('.agent-dlg [data-provider="openrouter"]').disabled`) &&
          await a.eval(`document.querySelector('.agent-dlg [data-provider="subscription"]').getAttribute('aria-checked') === 'true'`),
        "without the key OpenRouter is disabled and Subscription chosen",
      );
      check(
        await a.eval(`document.querySelector('.or-key-hint')?.textContent.includes('env OPENROUTER_API_KEY=… muster reload')`),
        "the hint says how to set the key and that the backend restarts",
      );
      await shot(a, "10c-new-agent-openrouter-off");
      await a.key("Escape");
      await a.wait(`!document.querySelector('.agent-dlg')`, 2000);
      await R(a, "mock.openrouter", { key_present: true });
      await R(a, "mock.drop_clients", {});
      await a.sleep(600);
      await a.wait(`${M}.rpc.isOpen && ${M}.openRouter.value?.key_present === true`, 10_000);
      await newAgentDialog();
      await a.type("or-coder");
      await a.click(`.agent-dlg [data-provider="openrouter"]`);
      await a.wait(`document.querySelectorAll('.or-opt').length > 10`, 3000);
      check(
        await a.eval(`document.querySelector('.or-vendor')?.textContent.startsWith('anthropic')`),
        "Anthropic's models come first for a Claude agent",
      );
      check(
        await a.eval(`document.querySelector('.or-warn')?.textContent.includes('pay per use')`),
        "the dialog says OpenRouter bills per token",
      );
      await a.click(`.or-opt[data-model="anthropic/claude-sonnet-5.5"]`);
      await a.wait(`document.querySelector('.or-opt.on')?.dataset.model === 'anthropic/claude-sonnet-5.5'`, 1000);
      await shot(a, "10d-new-agent-openrouter-models");
      // Tab stops at the chosen model once, then leaves the list (its other rows are ↑/↓ only).
      await a.eval(`document.querySelector('.or-search').focus()`);
      await a.key("Tab");
      check(
        await a.eval(`document.activeElement?.dataset.model === 'anthropic/claude-sonnet-5.5'`),
        "Tab from the search box reaches the chosen model",
      );
      await a.key("Tab");
      check(
        await a.eval(`!document.activeElement?.classList.contains('or-opt')`),
        "the next Tab leaves the model list instead of walking every row",
      );
      // Focused directly: the Tabs above may have scrolled the dialog past the box.
      await a.eval(`document.querySelector('.or-search').focus()`);
      await a.type("qwen coder");
      await a.sleep(150);
      check(
        await a.eval(`[...document.querySelectorAll('.or-opt')].map((o) => o.dataset.model).join()`) ===
          "qwen/qwen3.5-coder,custom",
        "search keeps only the matching model (and Custom…)",
      );
      await shot(a, "10e-new-agent-openrouter-search");
      await a.key("Enter");
      await a.wait(`document.querySelector('.or-opt.on')?.dataset.model === 'qwen/qwen3.5-coder'`, 1000);
      await a.key("Ctrl+Enter");
      await a.wait(`!document.querySelector('.agent-dlg') && !!document.querySelector('[data-agent="or-coder"] .or-badge')`, 3000);
      const orAgent: any = (await R(a, "agent.list", {})).find((x: any) => x.name === "or-coder");
      check(
        orAgent?.provider === "openrouter" && orAgent.model === "qwen/qwen3.5-coder",
        `created on OpenRouter with the picked model: ${JSON.stringify(orAgent)}`,
      );
      // Started, its tab carries the badge too.
      await a.click(`[data-agent="or-coder"]`);
      await a.wait(`!!document.querySelector('.tab.active .tab-or')`, 6000);
      await shot(a, "10f-agent-openrouter-badge");
      // Switched back to the subscription while running: it still bills OpenRouter until it
      // restarts, and both the row and the tab keep saying so.
      await R(a, "agent.update", { name: "or-coder", provider: "subscription" });
      await a.wait(`document.querySelector('[data-agent="or-coder"] .or-badge')?.textContent === 'OpenRouter · until restart'`, 3000);
      check(await a.eval(`!!document.querySelector('.tab.active .tab-or')`), "the tab keeps its badge until a restart");
      await R(a, "agent.remove", { name: "or-coder" });
      await a.wait(`!document.querySelector('.tab-or')`, 3000);
      ok("Provider: OpenRouter off without the key (with a hint), a searchable model list with it, the pay-per-use badge");
    }

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

    // Seeding an empty memory/: the intro explains it; Add files… and a drop copy files in.
    await a.click(`[data-agent="researcher"]`, { button: "right" });
    await a.wait(`!!document.querySelector('.menu')`);
    check(await menuItem(a, "Memory…"), "the new agent's menu has Memory…");
    await a.wait(`!!document.querySelector('.mem-intro')`, 3000);
    const intro: string = await a.eval(`document.querySelector('.mem-intro').textContent`);
    check(intro.includes("CLAUDE.md") && intro.includes("Add files…") && intro.includes("New note"), intro);
    await shot(a, "11a-agent-memory-empty");
    // Add files… is a real <input type=file multiple>: hand it a text file and a binary one.
    check(await a.eval(`document.querySelector('.mem input[type=file]')?.multiple === true`), "file picker");
    await a.eval(`(() => {
      const dt = new DataTransfer();
      dt.items.add(new File(["# Style guide\\nShort sentences.\\n"], "style-guide.md", { type: "text/markdown" }));
      dt.items.add(new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0xff, 0x00])], "logo.png"));
      const i = document.querySelector('.mem input[type=file]');
      i.files = dt.files;
      i.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await a.wait(`document.querySelector('.mem-text')?.value.startsWith('# Style guide')`, 3000);
    const note: string = await a.eval(`document.querySelector('.mem-added').textContent`);
    check(note.includes("Added style-guide.md") && note.includes("Skipped logo.png: not a text file"), note);
    await shot(a, "11b-agent-memory-added");
    // Dropping a file whose name is taken adds it under a numbered name; nothing is replaced.
    await a.eval(`(() => {
      const dt = new DataTransfer();
      dt.items.add(new File(["second copy"], "style-guide.md"));
      const t = document.querySelector('.mem-side');
      for (const type of ['dragenter', 'dragover', 'drop']) {
        t.dispatchEvent(new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true }));
      }
    })()`);
    await a.wait(`document.querySelector('.mem-text')?.value === 'second copy'`, 3000);
    check(
      (await a.eval(`document.querySelector('.mem-added').textContent`)).includes("already existed"),
      "the taken name is explained",
    );
    const seeded: any = await R(a, "agent.memory.list", { name: "researcher" });
    check(
      seeded.files.map((f: any) => f.path).join() === "style-guide-2.md,style-guide.md",
      JSON.stringify(seeded.files),
    );
    check(
      (await R(a, "agent.memory.read", { name: "researcher", path: "style-guide.md" }) as any).text
        .startsWith("# Style guide"),
      "the first copy is untouched",
    );
    check(await a.eval(`!document.querySelector('.mem-drop')`), "the drop outline is gone");
    await shot(a, "11c-agent-memory-dropped");
    // Delete asks in place (Esc keeps the file and the dialog), then removes the file.
    await a.click(".mem-del");
    await a.wait(`!!document.querySelector('.mem-del-yes')`, 1000);
    check(await a.eval(`document.activeElement?.textContent === 'Cancel'`), "Delete starts on Cancel");
    check(
      await a.eval(`(() => { const q = document.querySelector('.mem-pending[role=alertdialog]');
        return !!q && document.getElementById(q.getAttribute('aria-labelledby'))?.textContent.includes('style-guide-2.md')
          && document.activeElement.getAttribute('aria-describedby') === q.getAttribute('aria-labelledby'); })()`),
      "the delete question is an alertdialog naming the file, described on its buttons",
    );
    await shot(a, "11d-agent-memory-delete");
    await a.key("Escape");
    await a.wait(`!document.querySelector('.mem-del-yes') && !!document.querySelector('.agent-dlg')`, 1000);
    await a.wait(`document.activeElement?.classList.contains('mem-del')`, 1000);
    await a.click(".mem-del");
    await a.wait(`!!document.querySelector('.mem-del-yes')`, 1000);
    await a.click(".mem-del-yes");
    await a.wait(`document.querySelectorAll('.mem-file').length === 1`, 2000);
    await a.wait(`document.activeElement?.classList.contains('mem-add')`, 1000);
    const left: any = await R(a, "agent.memory.list", { name: "researcher" });
    check(left.files.map((f: any) => f.path).join() === "style-guide.md", JSON.stringify(left.files));

    // Two drops in a row: the second waits for the first batch instead of being lost.
    const drop = (names: string[]) =>
      `(() => { const dt = new DataTransfer();
        for (const n of ${JSON.stringify(names)}) dt.items.add(new File(["text of " + n], n));
        const t = document.querySelector('.mem-side');
        for (const type of ['dragenter', 'dragover', 'drop']) {
          t.dispatchEvent(new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true }));
        } })()`;
    await a.eval(`${drop(["q1.md"])}; ${drop(["q2.md"])}`);
    await a.wait(`document.querySelectorAll('.mem-file').length === 3`, 3000);
    await a.wait(`document.querySelector('.mem-text')?.value === 'text of q1.md'`, 2000);
    check(
      (await a.eval(`document.querySelector('.mem-added').textContent`)).includes("q1.md and q2.md"),
      "one note for both drops",
    );
    // Slow RPCs never overwrite what was done meanwhile (delayed in the page, mock untouched).
    await a.eval(`(() => { const r = ${M}.rpc; r.__call = r.call;
      r.call = (m, p) => ["agent.memory.delete", "agent.memory.read"].includes(m) && r.__slow?.includes(m)
        ? new Promise((ok) => setTimeout(ok, 700)).then(() => r.__call(m, p)) : r.__call(m, p); })()`);
    const fileBtn = (n: string) =>
      `[...document.querySelectorAll('.mem-file')].find((b) => b.textContent === ${JSON.stringify(n)})`;
    // A delete still running while another file is opened and edited: the edits stay.
    await a.eval(`${M}.rpc.__slow = ["agent.memory.delete"]`);
    await a.click(".mem-del");
    await a.click(".mem-del-yes");
    await a.eval(`${fileBtn("style-guide.md")}.click()`);
    await a.wait(`document.querySelector('.mem-text')?.value.startsWith('# Style guide')`, 2000);
    await a.click(".mem-text");
    await a.type(" typed during delete");
    await a.wait(`!${fileBtn("q1.md")}`, 3000);
    await a.sleep(200);
    check(
      await a.eval(`document.querySelector('.mem-head code').textContent === 'memory/style-guide.md'
        && document.querySelector('.mem-text').value.endsWith(' typed during delete')`),
      "a finished delete kept the other file's edits",
    );
    await a.click(".mem-save");
    await a.wait(`!document.querySelector('.mem-dirty')`, 2000);
    // An added file opening late: edits made while it was being read win.
    await a.eval(`${M}.rpc.__slow = ["agent.memory.read"]`);
    await a.eval(drop(["late.md"]));
    await a.wait(`!!${fileBtn("late.md")}`, 3000);
    await a.click(".mem-text");
    await a.type(" and while late.md opened");
    await a.sleep(1000);
    check(
      await a.eval(`document.querySelector('.mem-head code').textContent === 'memory/style-guide.md'
        && document.querySelector('.mem-text').value.endsWith(' and while late.md opened')`),
      "a late read didn't replace the edits",
    );
    await a.eval(`(() => { const r = ${M}.rpc; r.call = r.__call; delete r.__slow; })()`);
    await a.click(".mem-save");
    await a.wait(`!document.querySelector('.mem-dirty')`, 2000);
    await a.key("Escape");
    await a.wait(`!document.querySelector('.agent-dlg')`, 2000);
    ok("Memory: the empty state explains seeding; Add files… and drops copied text files in (binary skipped, a taken name numbered, a second drop queued); Delete asked first and gave focus back; slow RPCs kept later edits");
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

  // ---- schedules (dev/e2e_schedules.ts)
  if (!real) await scheduleChecks(a, { shots, ok, check });

  // ---- the cold prompt cache bar and its settings (dev/e2e_cache.ts)
  if (!real) await cacheChecks(a, { shots, ok, check });

  // ---- the update banner, the shutdown dialog, Ctrl+Shift+R (mock only: mock.updates)
  if (!real) {
    const banner = (kind: string) => a.wait(`!!document.querySelector('.update-banner.update-${kind}')`, 4000);
    await R(a, "mock.updates", { backend: "abc123def456", ui_release: "1-ff", host: null });
    await banner("backend");
    await shot(a, "12-update-backend");
    // Dismissing the backend update shows the next one (the UI); a different build shows again.
    await a.click(".update-banner .toast-x");
    await banner("ui");
    await shot(a, "12b-update-ui");
    await R(a, "mock.updates", { backend: "fedcba654321" });
    await banner("backend");
    ok("the update banner offers the backend first, then the UI; dismissing hides one build only");
    // A stale build id is refused (the backend restarts only the offered build).
    check(
      /changed since/.test(await a.eval(`${M}.rpc.call("daemon.reload", { expect: "abc123def456" }).then(() => "", (e) => e.message)`)),
      "daemon.reload refuses a build other than the one offered",
    );
    // Restart backend: every connection drops (as a real reload does) and the window reconnects.
    await a.click(".update-backend .btn-primary");
    await banner("ui");
    check(await a.eval(`!${M}.focusedSession.value || document.querySelectorAll('.tab').length > 0`), "tabs survived");
    ok("Restart backend reconnected the window and the banner moved on to the UI update");
    await a.click(".update-banner .toast-x");
    await R(a, "mock.updates", { host: "0123456789ab" });
    await banner("host");
    await shot(a, "12c-update-host");
    await a.click(".update-host .update-actions .btn");
    await a.wait(`!!document.querySelector('.modal[aria-label="Restart Muster"]')`, 2000);
    check(await a.eval(`!document.querySelector('.update-banner')`), "no banner above an open dialog");
    check(
      await a.eval(`document.activeElement?.textContent === 'Cancel'`),
      "Cancel is the default in the restart dialog",
    );
    const text: string = await a.eval(`document.querySelector('.modal .dlg-body').textContent`);
    check(/sessions? (is|are) running|No sessions/.test(text), `the dialog counts sessions: ${text}`);
    check(/reopen their conversation/.test(text), `the dialog says Claude tabs can be restarted: ${text}`);
    check(await a.eval(`!!document.querySelector('.modal .dlg-foot .btn-danger')`), "Restart is the red button");
    await shot(a, "13-restart-dialog");
    await a.key("Escape");
    await a.wait(`!document.querySelector('.modal')`, 2000);
    // A build the window wasn't shown is refused before anything stops.
    check(
      /nothing was stopped/.test(await a.eval(`${M}.rpc.call("daemon.restart", { backend: null, host: null }).then(() => "", (e) => e.message)`)),
      "daemon.restart refuses builds other than the ones shown",
    );
    await a.click(".update-banner .toast-x");
    ok("the terminal host banner opens the restart dialog (Cancel focused, Restart red); nothing was stopped");
    // The ⋯ menu carries the window items.
    await a.click(".win-more");
    await a.wait(`!!document.querySelector('.menu')`, 2000);
    const items: string = await a.eval(`[...document.querySelectorAll('.menu .menu-label')].map((e) => e.textContent).join("|")`);
    for (const want of ["Reload window", "Restart Muster…", "Shut down Muster…"]) {
      check(items.includes(want), `⋯ has ${want}: ${items}`);
    }
    // A browser lets a page close its window only when a script opened it or it has one history
    // entry (this test navigated, so it may have more).
    const closable = await a.eval(`history.length <= 1 || !!opener`);
    check(items.includes("Close window") === closable, `Close window only when it can work: ${items}`);
    check(!items.includes("System title bar"), "no title bar switch in a browser");
    await shot(a, "14-more-menu");
    await a.key("Escape");
    ok("⋯ has Reload window, Restart Muster… and Shut down Muster… (Close window when allowed), no title bar switch in a browser");
    // Ctrl+Shift+R reloads the page (a marker set now is gone afterwards).
    await a.eval(`globalThis.__beforeReload = true`);
    await a.click(".pane .xterm");
    await a.key("Ctrl+Shift+R");
    await a.wait(`!globalThis.__beforeReload && !!document.querySelector('.app')`, 8000);
    await guard(a);
    ok("Ctrl+Shift+R reloaded the window");
  }

  // ---- notes (dev/e2e_notes.ts)
  if (!real) await noteChecks(a, { shots, ok, check });

  // ---- agent panels (dev/e2e_panels.ts)
  if (!real) await panelChecks(a, { url, shots, hello, ok, check });

  // ---- built-in panel apps (dev/panel_apps_e2e.ts)
  if (!real) await panelApps({ a, repo, shots, ok, check });

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

  // ---- Restart Muster (mock only; last, since it ends every session): every window shows
  // "Restarting Muster…", reconnects by itself, and the terminal host update is gone.
  if (!real) {
    await R(a, "mock.updates", { host: "0123456789ac" });
    await a.wait(`!!document.querySelector('.update-banner.update-host')`, 4000);
    await a.click(".update-host .update-actions .btn");
    await a.wait(`!!document.querySelector('.modal[aria-label="Restart Muster"]')`, 2000);
    // Its checks take a while: once confirmed, the dialog says so and can't be dismissed.
    await R(a, "mock.restart_delay", { ms: 2500 });
    await a.click(".modal .dlg-foot .btn-danger");
    await a.wait(`/Checking that Muster can restart/.test(document.querySelector('.modal .dlg-body')?.textContent ?? "")`, 2000);
    check(await a.eval(`!document.querySelector('.modal .dlg-foot')`), "no Cancel once confirmed");
    await shot(a, "13b-restart-checking");
    await a.key("Escape");
    await a.sleep(200);
    check(await a.eval(`!!document.querySelector('.modal[aria-label="Restart Muster"]')`), "Escape doesn't dismiss a confirmed restart");
    await a.wait(`!!document.querySelector('#restarting-title')`, 5000);
    check(await a.eval(`document.activeElement?.getAttribute('aria-labelledby') === 'restarting-title'`), "the restarting screen takes the focus");
    await shot(a, "15-restarting");
    // Shortcuts do nothing while restarting (Alt+T would open a tab on the new backend).
    const tabsBefore = await a.eval(`${M}.sessions.value.size`);
    check(await a.eval(`!!document.querySelector('#restarting-title')`), "still restarting when the shortcut is pressed");
    await a.key("Alt+T");
    await a.wait(`!!document.querySelector('.app') && !document.querySelector('#restarting-title')`, 10000);
    await a.sleep(500);
    check((await a.eval(`${M}.sessions.value.size`)) === tabsBefore && (await a.eval(`!document.querySelector('.modal')`)),
      "a shortcut pressed while restarting did nothing, also after the reconnect");
    check(await a.eval(`!document.querySelector('.update-host')`), "no terminal host banner after the restart");
    check(
      await a.eval(`[...document.querySelectorAll('.toast-text')].some((t) => /Muster restarted/.test(t.textContent))`),
      "a toast says Muster restarted",
    );
    if (b2) await b2.wait(`!!document.querySelector('.app') && !document.querySelector('#restarting-title')`, 10000);
    ok("Restart Muster: Restarting Muster…, then every window reconnected by itself; the host update is gone");
  }

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
