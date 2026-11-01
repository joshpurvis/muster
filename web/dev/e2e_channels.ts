// Channel workspace walkthrough. Only the isolated mock: real sessions must never be used.
// deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-write=/tmp/muster-channels \
//   dev/e2e_channels.ts http://127.0.0.1:17782/#token=dev /tmp/muster-channels
import { type Browser, launch } from "./cdp.ts";

const [url, shots = "/tmp/muster-channels"] = Deno.args;
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
await Deno.mkdir(shots, { recursive: true });
const M = "globalThis.__muster";
const check = (v: unknown, message: string) => { if (!v) throw new Error(message); };
const ok = (message: string) => console.log(`✓ ${message}`);
const r = (b: Browser, method: string, params: unknown = {}): Promise<any> =>
  b.eval(`${M}.rpc.call(${JSON.stringify(method)}, ${JSON.stringify(params)})`);
const current = (b: Browser): Promise<number> => b.eval(`${M}.activeChannelId.value`);
const selected = (b: Browser, id: number) => b.wait(`${M}.activeChannelId.value === ${id}`);
const select = async (b: Browser, id: number) => {
  await b.click(`[data-channel-id="${id}"] .channel-select`);
  await selected(b, id);
};
const tree = (b: Browser): Promise<string> => b.eval(`JSON.stringify(${M}.layout.value)`);
const activeSid = (b: Browser): Promise<number> => b.eval(`${M}.focusedSession.value?.id`);
const menuItem = async (b: Browser, label: string) => {
  await b.wait(`Array.from(document.querySelectorAll('[role="menuitem"]')).some(el => el.textContent.includes(${JSON.stringify(label)}))`);
  await b.eval(`Array.from(document.querySelectorAll('[role="menuitem"]')).find(el => el.textContent.includes(${JSON.stringify(label)})).click()`);
};

async function open(port: number, dir: string) {
  const b = await launch(port, dir);
  await b.size(1440, 900);
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`, 10000);
  const hello = await r(b, "hello");
  check(String(hello.version).includes("mock"), "Only the mock daemon is allowed");
  await b.eval(`(() => { const rpc = ${M}.rpc, call = rpc.call.bind(rpc);
    rpc.call = (m,p) => m === 'window.show' ? Promise.reject(new Error('blocked in e2e')) : call(m,p); })()`);
  return b;
}

async function create(b: Browser, name: string, kind = "Shell") {
  await b.click('button[aria-label="New channel"]');
  await b.wait(`document.querySelector('.dialog h2')?.textContent === 'New channel'`);
  await b.eval(`(() => {
    const inputs = document.querySelectorAll('.dialog input');
    const folder = inputs[0]; folder.value = '/tmp'; folder.dispatchEvent(new Event('input', {bubbles:true}));
    const name = document.querySelector('.dialog input[placeholder="Project folder name"]');
    name.value = ${JSON.stringify(name)}; name.dispatchEvent(new Event('input', {bubbles:true}));
    [...document.querySelectorAll('.dialog [role="radio"]')].find(b => b.textContent.trim() === ${JSON.stringify(kind)}).click();
  })()`);
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!document.querySelector('.dialog')`, 10000);
  const id = await current(b);
  if (kind !== "Empty") await b.wait(`!!document.querySelector('.pane .xterm')`, 8000);
  check((await r(b, "hello")).channels.find((c: any) => c.id === id)?.name === name, `created ${name}`);
  return id;
}

const a = await open(9356, `${shots}/profile-a`);
let b: Browser | undefined;
try {
  // Start each run from the same mock state, including after a failed assertion. open() checked
  // the mock version before any mutation; no real daemon is accepted by this walkthrough.
  const prior = await r(a, "hello");
  for (const n of prior.notes ?? []) await r(a, "note.delete", { id: n.id });
  for (const c of prior.channels) await r(a, "channel.archive", { id: c.id, remove_worktree: false });
  const alpha = await create(a, "Muster");
  const first = await activeSid(a);
  const renameTab = async (sid: number, name: string) => {
    await a.click(`[data-tab-sid="${sid}"] .tab-name`, { count: 2 });
    await a.wait(`document.querySelectorAll('.tab .inline-edit').length === 1`);
    check(await a.eval(`document.activeElement?.getAttribute('aria-label')`) === "Tab name", "only the tab name is editable");
    await a.key("Ctrl+a");
    await a.type(name);
    await a.key("Enter");
    await a.wait(`${M}.sessions.value.get(${sid}).name === ${JSON.stringify(name)}`);
    check(await a.eval(`document.querySelector('[data-tab-sid="${sid}"] .tab-name').textContent`) === name, "tab label is only its session name");
    check((await r(a, "hello")).channels.find((c: any) => c.id === alpha)?.name === "Muster", "tab rename leaves channel unchanged");
  };
  await renameTab(first, "Main terminal");
  await a.eval(`void (globalThis.firstTerminal = ${M}.term(${first}))`);
  await a.key("Alt+T");
  await a.wait(`${M}.windowSessions.value.filter(s => s.channel_id === ${alpha}).length === 2`);
  await renameTab(await activeSid(a), "Test logs");
  check(await a.eval(`document.querySelector('[data-tab-sid="${first}"] .tab-name').textContent`) === "Main terminal", "sibling arrival keeps the original tab name");
  check(await current(a) === alpha, "new tab stays in its channel");
  await a.key("Alt+D");
  await a.wait(`document.querySelectorAll('.pane').length === 2`);
  const split = await activeSid(a);
  const alphaTree = await tree(a);
  ok("New channel, multiple tabs and splits in one workspace");

  const beta = await create(a, "Website");
  await a.key("Alt+Shift+D");
  await a.wait(`document.querySelectorAll('.pane').length === 2`);
  const betaTree = await tree(a);
  await select(a, alpha);
  check(await tree(a) === alphaTree, "alpha layout restored exactly");
  check(await a.eval(`${M}.term(${first}) === globalThis.firstTerminal`), "terminal instance survives switching");
  await select(a, beta);
  check(await tree(a) === betaTree, "beta layout restored exactly");
  ok("Independent channel layouts, focus and terminal instances survive switching");

  await r(a, "session.spawn", { channel_id: alpha, kind: "shell", window_id: await a.eval(`${M}.windowId.value`) });
  await a.sleep(150);
  check(await tree(a) === betaTree, "background arrival does not alter active layout");
  check(await current(a) === beta, "background arrival does not switch channels");
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher')`);
  await a.type("Muster");
  await a.sleep(100);
  await a.key("Enter");
  await selected(a, alpha);
  ok("Background tabs stay in their channel; Jump reveals the right workspace");

  const panel = (await r(a, "panel.open", { from_session: first, arguments: {
    title: "Project plan", app: "markdown", data: { markdown: "# Muster\n\nChannels keep a project's work together." },
  } })).panel_id;
  await a.wait(`!!document.querySelector('[data-panel-view="${panel}"] iframe') && !document.querySelector('[data-panel-view="${panel}"] .panel-status')`, 12000);
  await a.eval(`void (globalThis.savedFrame = document.querySelector('[data-panel-view="${panel}"] iframe'))`);
  const note = await r(a, "note.create", { title: "Project notes", channel_id: alpha, window_id: await a.eval(`${M}.windowId.value`) });
  const nt = -(2 ** 40 + note.id);
  await a.wait(`!!document.querySelector('[data-tab-sid="${nt}"]')`);
  await a.click(`[data-tab-sid="${nt}"]`);
  await a.wait(`!!document.querySelector('textarea[data-note-id="${note.id}"]')`);
  await a.click(`textarea[data-note-id="${note.id}"]`);
  await a.type("Keep this draft while I switch projects.");
  await select(a, beta);
  await select(a, alpha);
  await a.wait(`document.querySelector('textarea[data-note-id="${note.id}"]')?.value.includes('Keep this draft')`);
  check(await a.eval(`document.querySelector('[data-panel-view="${panel}"] iframe') === globalThis.savedFrame`), "panel iframe survived channel switches");
  ok("Note drafts and panel iframe state survive switching");

  // Move by dropping a terminal onto another channel. Its owned panel follows it.
  const from = await a.rect(`[data-tab-sid="${first}"]`);
  const to = await a.rect(`[data-channel-id="${beta}"] .channel-select`);
  check(from && to, "drag targets exist");
  await a.drag([from!.x + 30, from!.y + 12], [to!.x + 50, to!.y + 15]);
  await selected(a, beta);
  await a.wait(`${M}.sessions.value.get(${first}).channel_id === ${beta} && ${M}.panels.value.get(${panel}).channel_id === ${beta}`);
  check(await a.eval(`document.querySelector('[data-panel-view="${panel}"] iframe') === globalThis.savedFrame`), "panel survives ownership move");
  ok("Dragging to a channel moves the terminal and its panels together");

  // Resize with the keyboard and restore both layouts on page reload.
  await a.eval(`document.querySelector('.sidebar-divider').focus()`);
  await a.key("Down");
  const ratio = await a.eval(`${M}.channelLayout.value.sidebar.split`);
  const layouts = await a.eval(`JSON.stringify(${M}.channelLayout.value.layouts)`);
  await a.sleep(650);
  await a.goto(url);
  await a.wait(`!!document.querySelector('.app')`, 10000);
  check(await current(a) === beta, "selected channel persisted");
  check(await a.eval(`JSON.stringify(${M}.channelLayout.value.layouts)`) === layouts, "all layouts persisted");
  check(await a.eval(`${M}.channelLayout.value.sidebar.split`) === ratio, "divider persisted");
  ok("Reload restores selection, every channel layout and sidebar divider");

  const empty = await create(a, "Scratch", "Empty");
  await a.wait(`!!document.querySelector('.empty-channel')`);
  await a.click('.empty-channel .launch-btn:nth-child(3)');
  await a.wait(`!!document.querySelector('.pane .xterm')`);
  const only = await activeSid(a);
  await r(a, "session.remove", { id: only });
  await a.wait(`!!document.querySelector('.empty-channel')`);
  check(await current(a) === empty, "closing last tab preserves the channel");
  ok("Empty channels can launch tabs and remain after the last tab closes");

  // Move one of alpha's sessions to another window; each window keeps its own alpha layout.
  b = await open(9357, `${shots}/profile-b`);
  const other = await b.eval(`${M}.windowId.value`);
  await r(a, "session.set_window", { id: split, window_id: other });
  await b.wait(`${M}.windowSessions.value.some(s => s.id === ${split})`);
  await select(b, alpha);
  check(await b.eval(`document.querySelectorAll('.tab[data-tab-sid]').length`) === 1, "other window shows only its own tab");
  await select(a, alpha);
  check(!await a.eval(`!!document.querySelector('[data-tab-sid="${split}"]')`), "moved tab gone from source layout");
  await b.key("Alt+T");
  await b.wait(`${M}.windowSessions.value.length === 2`);
  check(await a.eval(`${M}.windowSessions.value.every(s => s.id !== ${split})`), "source membership independent");
  ok("One channel in two windows has independent tabs and layouts");

  // Context menus on the heading, directory, channel, account and session all expose the view controls.
  await a.click('.channel-project-head', { button: "right" });
  await menuItem(a, "Hide channels");
  await a.wait(`!document.querySelector('.channels-scroll')`);
  await a.click('.sidebar-heading');
  await a.click(`[data-channel-id="${alpha}"]`, { button: "right" });
  await menuItem(a, "Hide sidebar");
  await a.wait(`getComputedStyle(document.querySelector('.sidebar')).display === 'none'`);
  await a.key("Alt+G");
  await a.wait(`getComputedStyle(document.querySelector('.sidebar')).display !== 'none'`);
  const claude = await r(a, "session.spawn", { channel_id: alpha, kind: "claude", window_id: await a.eval(`${M}.windowId.value`) });
  const codex = await r(a, "session.spawn", { channel_id: beta, kind: "codex", window_id: await a.eval(`${M}.windowId.value`) });
  await a.click('.channels-panel .ap-head', { button: "right" });
  await menuItem(a, "Group by account");
  await a.wait(`!!document.querySelector('[data-account-group="claude"]') && !!document.querySelector('[data-account-group="codex"]')`);
  await a.click(`[data-account-session="${codex.id}"] .channel-select`);
  await a.wait(`${M}.focusedSession.value?.id === ${codex.id} && ${M}.activeChannelId.value === ${beta}`);
  await a.click('[data-account-group="shell"] .channel-project-toggle');
  await a.wait(`document.querySelector('[data-account-group="shell"] .channel-project-toggle').getAttribute('aria-expanded') === 'false'`);
  await a.sleep(650);
  await a.goto(url);
  await a.wait(`!!document.querySelector('[data-account-group="shell"]')`, 10000);
  check(await a.eval(`${M}.channelLayout.value.sidebar.groupBy`) === "account", "account view persisted");
  check(await a.eval(`${M}.channelLayout.value.sidebar.collapsedAccounts.includes('shell')`), "account accordion persisted");
  check(!await a.eval(`!!document.querySelector('[data-account-session="${split}"]')`), "other window's session excluded from accounts");
  await a.click(`[data-account-session="${claude.id}"] .channel-select`);
  await a.wait(`${M}.focusedSession.value?.id === ${claude.id} && ${M}.activeChannelId.value === ${alpha}`);
  await a.click(`[data-account-session="${claude.id}"]`, { button: "right" });
  await menuItem(a, "Hide channels");
  await a.wait(`!document.querySelector('.channels-scroll')`);
  await a.click('.sidebar-heading');
  await a.shot(`${shots}/channels-accounts.png`);
  await a.click('[data-account-group="claude"] .channel-project-head', { button: "right" });
  await menuItem(a, "Group by directory");
  await a.wait(`!document.querySelector('[data-account-group]')`);
  ok("Context-menu hiding and account grouping, exact session selection and saved accordions");

  // Exercise the built result in both themes and a smaller desktop window in one QA batch.
  await select(a, beta);
  await a.shot(`${shots}/channels-dark.png`);
  await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
  await a.size(900, 640);
  await a.sleep(150);
  await a.shot(`${shots}/channels-light.png`);
  check(await a.eval(`document.documentElement.scrollWidth <= innerWidth`), "no horizontal page overflow");
  const errors = [...a.console, ...(b?.console ?? [])].filter((line) => line.startsWith("exception:"));
  check(!errors.length, `browser exceptions: ${errors.join("\n")}`);
  ok("Desktop layouts in dark and light themes; no browser exceptions");
  console.log(`Screenshots: ${shots}`);
} catch (e) {
  await a.shot(`${shots}/failure.png`).catch(() => {});
  console.error(await a.eval(`document.body.innerText`).catch(() => "unavailable"));
  console.error(a.console.join("\n"));
  throw e;
} finally {
  await b?.close();
  await a.close();
}
