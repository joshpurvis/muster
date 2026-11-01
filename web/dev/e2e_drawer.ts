// Isolated mock only. No calls to the real daemon.
// deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-write=/tmp/muster-drawer dev/e2e_drawer.ts
import { launch } from "./cdp.ts";
const url = Deno.args[0] ?? "http://127.0.0.1:17789/#token=dev";
if (new URL(url).hostname !== "127.0.0.1") throw new Error("Use the isolated loopback mock");
const shots = "/tmp/muster-drawer";
await Deno.mkdir(shots, { recursive: true });
const b = await launch(9367, `${shots}/profile`);
const M = "globalThis.__muster";
const rpc = (method: string, params = {}): Promise<any> => b.eval(`${M}.rpc.call(${JSON.stringify(method)}, ${JSON.stringify(params)})`);
const check = (value: unknown, message: string) => { if (!value) throw new Error(message); console.log(`✓ ${message}`); };
const drawerSid = () => b.eval<number>(`${M}.channelLayout.value.drawers[${M}.activeChannelId.value].session`);
const select = async (id: number) => { await b.click(`[data-channel-id="${id}"] .channel-select`); await b.wait(`${M}.activeChannelId.value === ${id}`); };
try {
  await b.size(1400, 900);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`);
  check(String((await rpc("hello")).version).includes("mock"), "isolated mock verified");
  const workspace = await rpc("workspace.add", { path: "/tmp" });
  const window_id = await b.eval(`${M}.windowId.value`);
  const alpha = await rpc("channel.create", { workspace_id: workspace.id, name: "Drawer alpha", agent: "shell", window_id });
  await select(alpha.id);
  const originalTree = await b.eval(`JSON.stringify(${M}.layout.value)`);
  await b.key("Ctrl+`");
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  const sid = await drawerSid();
  check(await b.eval(`JSON.stringify(${M}.layout.value)`) === originalTree, "drawer does not alter tiled tabs");
  check((await rpc("hello")).sessions.find((s: any) => s.id === sid).cwd === "/tmp", "drawer starts in channel directory");
  await b.wait(`!!document.activeElement?.closest('.terminal-drawer')`);
  await b.type("export DRAWER_CHECK=kept; printf 'drawer-ready\\n'");
  await b.key("Enter");
  await b.wait(`${M}.termText(${sid})?.includes('drawer-ready')`);
  await b.key("Ctrl+`");
  await b.wait(`!document.querySelector('.terminal-drawer')`);
  await b.wait(`!!document.activeElement?.closest('.pane')`);
  check(true, "hiding restores main terminal focus");
  await b.click('[aria-label="Toggle channel terminal"]');
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  check(await drawerSid() === sid, "reopening reuses the shell");
  await b.wait(`!!document.activeElement?.closest('.terminal-drawer')`);
  await b.type("printf 'persist:%s\\n' \"$DRAWER_CHECK\"");
  await b.key("Enter");
  await b.wait(`${M}.termText(${sid})?.includes('persist:kept')`);
  const beta = await rpc("channel.create", { workspace_id: workspace.id, name: "Drawer beta", agent: null, window_id });
  await select(beta.id);
  check(await b.eval(`!${M}.windowSessions.value.some(s => s.channel_id === ${beta.id})`), "empty channel has no sessions before opening drawer");
  check(!await b.eval(`!!document.querySelector('.terminal-drawer')`), "another channel starts with drawer hidden");
  await b.key("Ctrl+`");
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  const other = await drawerSid();
  check(other !== sid, "empty channel gets its own shell");
  await select(alpha.id);
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  check(await drawerSid() === sid, "switching channels restores each drawer");
  await b.eval(`document.querySelector('.drawer-resize').focus()`);
  await b.key("Up");
  check(await b.eval(`${M}.channelLayout.value.drawers[${alpha.id}].height`) === 280, "keyboard resizing works");
  await b.wait(`document.querySelector('.terminal-drawer').getBoundingClientRect().height === 280`);
  await b.eval(`document.querySelector('.terminal-drawer .xterm textarea').focus()`);
  await b.sleep(200);
  await b.shot(`${shots}/desktop.png`);
  await b.size(800, 600);
  await b.sleep(300);
  await b.shot(`${shots}/compact.png`);
  await b.size(1400, 900);
  await b.eval(`${M}.prefs.value = {...${M}.prefs.value, theme: 'dark'}`);
  await b.sleep(300);
  await b.shot(`${shots}/dark.png`);
  await b.sleep(600);
  await b.goto(url);
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  check(await drawerSid() === sid, "reload restores drawer without another shell");
  check(await b.eval(`JSON.stringify(${M}.layout.value)`) === originalTree, "reload keeps drawer out of tabs");
  await b.click('.terminal-drawer .xterm');
  await b.type("exit");
  await b.key("Enter");
  await b.wait(`!!document.querySelector('[aria-label="Restart channel terminal"]')`);
  await b.click('[aria-label="Restart channel terminal"]');
  await b.wait(`${M}.sessions.value.get(${sid}).state !== 'exited'`);
  check(await drawerSid() === sid, "exited shell restarts in the same drawer");
  await b.eval(`document.querySelector('.terminal-drawer .xterm textarea').focus()`);
  await b.key("Alt+w");
  await b.wait(`!document.querySelector('.terminal-drawer')`);
  check(await b.eval(`JSON.stringify(${M}.layout.value)`) === originalTree, "close shortcut hides drawer and leaves main tab intact");
  // Two delayed spawns overlap: the first drawer must survive the other arrival's hold.
  const gamma = await rpc("channel.create", { workspace_id: workspace.id, name: "Drawer gamma", window_id });
  const delta = await rpc("channel.create", { workspace_id: workspace.id, name: "Drawer delta", window_id });
  await b.eval(`(() => {
    const rpc = ${M}.rpc, call = rpc.call.bind(rpc);
    globalThis.restoreDrawerRpc = () => { rpc.call = call; };
    rpc.call = async (method, params) => {
      if (method === 'session.spawn') {
        await new Promise(resolve => setTimeout(resolve, params.channel_id === ${gamma.id} ? 250 : 800));
      }
      return call(method, params);
    };
  })()`);
  await select(gamma.id);
  await b.key("Ctrl+`");
  await select(delta.id);
  await b.key("Ctrl+`");
  await b.key("Ctrl+`"); // hide while the launch is pending
  await b.wait(`${M}.channelLayout.value.drawers[${gamma.id}]?.session > 0 && ${M}.channelLayout.value.drawers[${delta.id}]?.session > 0`);
  check(!await b.eval(`!!document.querySelector('.terminal-drawer')`), "hiding a pending drawer stays hidden after launch");
  await b.eval(`globalThis.restoreDrawerRpc()`);
  const gammaSid = await b.eval<number>(`${M}.channelLayout.value.drawers[${gamma.id}].session`);
  await select(gamma.id);
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  check(await drawerSid() === gammaSid && await b.eval(`!document.querySelector('.pane .tab')`), "overlapping launches keep both shells out of the tile layout");

  const failure = await rpc("channel.create", { workspace_id: workspace.id, name: "Drawer retry", window_id });
  await select(failure.id);
  await b.eval(`(() => {
    const rpc = ${M}.rpc, call = rpc.call.bind(rpc);
    rpc.call = (method, params) => {
      if (method === 'session.spawn') { rpc.call = call; return Promise.reject(new Error('test launch failure')); }
      return call(method, params);
    };
  })()`);
  await b.key("Ctrl+`");
  await b.wait(`document.querySelector('.drawer-empty')?.textContent.includes('test launch failure')`);
  await b.click('.drawer-empty button');
  await b.wait(`!!document.querySelector('.terminal-drawer .xterm')`);
  check(true, "failed launches have a working retry");
  // The version check above limits cleanup to our mock's test channels.
  for (const c of (await rpc("hello")).channels.filter((c: any) => c.name.startsWith("Drawer "))) {
    await rpc("channel.archive", { id: c.id });
  }
} catch (error) {
  console.log(await b.eval(`${M}.termText(${M}.channelLayout.value.drawers[${M}.activeChannelId.value]?.session)`));
  throw error;
} finally { await b.close(); }
