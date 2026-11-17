// Run only against a fresh isolated mock. See docs/ADDONS.md for permissions and commands.
import { launch } from "./cdp.ts";
import { allPanes, type Layout } from "../src/layout/tree.ts";
import type { Addon } from "../src/addons.ts";
const url = Deno.args[0];
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
const health = await (await fetch(new URL("/api/health", url))).json();
if (health.version !== "mock") throw new Error("Only the isolated mock daemon is allowed");
const shots = Deno.env.get("MUSTER_ADDON_SHOTS");
if (!shots) throw new Error("Set MUSTER_ADDON_SHOTS to a screenshot directory");
await Deno.mkdir(shots, { recursive: true });
const b = await launch(9373, await Deno.makeTempDir({ dir: shots, prefix: "profile-" }));
const M = "globalThis.__muster";
const check = (v: unknown, message: string) => {
  if (!v) throw new Error(message);
};
const rpc = <T>(m: string, p = {}): Promise<T> =>
  b.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
const root = (): Promise<Layout> => b.eval(`${M}.layout.value.root`);
async function palette(text: string) {
  await b.key("Ctrl+Shift+P");
  await b.wait(`!!document.querySelector('.switcher')`);
  await b.type(text);
  await b.sleep(180);
  await b.key("Enter");
}
async function dialog() {
  await palette("Add-on tabs");
  await b.wait(`document.querySelector('[role="dialog"]')?.getAttribute('aria-label') === 'Add-on tabs'`);
}
async function fill(name: string, value: string) {
  await b.eval(
    `(() => { const e = document.querySelector('[name="${name}"]'); e.value = ${
      JSON.stringify(value)
    }; e.dispatchEvent(new Event('input', {bubbles:true})); })()`,
  );
}
async function shot(name: string, selector: string) {
  await b.sleep(300);
  const r = await b.rect(selector);
  check(r, "Screenshot target exists");
  await b.shot(`${shots}/${name}.png`, r!);
}
async function menu(label: string) {
  await b.click("[data-addon]", { button: "right" });
  const find =
    `Array.from(document.querySelectorAll('[role="menuitem"]')).find(e => e.textContent.trim() === ${
      JSON.stringify(label)
    })`;
  await b.wait(`!!${find}`);
  await b.eval(`${find}.click()`);
}
async function createChannel(name: string): Promise<number> {
  await b.click('button[aria-label="New channel"]');
  await b.wait(`document.querySelector('.dialog h2')?.textContent === 'New channel'`);
  await b.eval(`(() => {
    const folder = document.querySelectorAll('.dialog input')[0]; folder.value = '/tmp'; folder.dispatchEvent(new Event('input', {bubbles:true}));
    const name = document.querySelector('.dialog input[aria-label="Name (optional)"]'); name.value = ${
    JSON.stringify(name)
  }; name.dispatchEvent(new Event('input', {bubbles:true}));
  })()`);
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!document.querySelector('.dialog') && !!document.querySelector('.pane .xterm')`, 10000);
  return b.eval(`${M}.activeChannelId.value`);
}
async function selectChannel(id: number) {
  await b.click(`[data-channel-id="${id}"] .channel-select`);
  await b.wait(`${M}.activeChannelId.value === ${id}`);
  await b.click("[data-addon]");
}
try {
  await b.size(1440, 900);
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await b.goto(url);
  await b.wait(`!!${M}?.rpc && !!document.querySelector('.app')`, 10000);
  check(
    (await rpc<{ version: string }>("hello")).version === "0.0.0-mock",
    "Only the mock daemon is allowed",
  );
  await b.eval(`${M}.prefs.value = {...${M}.prefs.value, theme:'dark'}`);
  const demo = await rpc<Addon>("mock.addon_demo");
  check(
    (await rpc<Addon[]>("addon.list")).some((a) => a.id === "counter"),
    "Start a fresh mock with MOCK_ADDONS=1",
  );
  await rpc("agent.create", { name: "alpha" });
  await rpc("agent.create", { name: "beta" });
  const first = await createChannel("First");
  const second = await createChannel("Second");
  // Remove the mock's initial registration using the same dialog, then register it afresh.
  await dialog();
  await b.eval(
    `(() => { const e = document.querySelector('.dialog select'); e.value = 'counter'; e.dispatchEvent(new Event('change', {bubbles:true})); })()`,
  );
  await b.eval(
    `Array.from(document.querySelectorAll('.dialog button')).find(e => e.textContent === 'Remove').click()`,
  );
  await b.wait(`!document.querySelector('.dialog') && !!${M}?.helloEpoch?.value`, 10000);
  await dialog();
  await fill("addon-name", "Counter");
  await fill("addon-url", demo.url);
  await b.click(".dialog fieldset input");
  await shot("settings-dark", ".modal");
  await fill("addon-url", "https://example.test:5173/");
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!!document.querySelector('#addon-error')`);
  await shot("validation-dark", ".modal");
  await fill("addon-url", demo.url);
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!document.querySelector('.dialog') && !!document.querySelector('[data-addon]')`, 10000);
  await b.click("[data-addon]");
  await b.wait(`!!document.querySelector('iframe[data-addon-id]')?.closest('[data-placed]')`);
  const iframe = await b.rect("iframe[data-addon-id]");
  check(iframe, "Counter frame is visible");
  const tree = await b.send<{ frameTree: FrameTree }>("Page.getFrameTree");
  type FrameTree = { frame: { id: string; url: string }; childFrames?: FrameTree[] };
  const frames = (t: FrameTree): FrameTree[] => [t, ...(t.childFrames ?? []).flatMap(frames)];
  const child = frames(tree.frameTree).find((t) => t.frame.url === demo.url);
  check(child, "Counter loaded from its registered URL");
  const context = await b.send<{ executionContextId: number }>("Page.createIsolatedWorld", {
    frameId: child!.frame.id,
    worldName: "counter-check",
  });
  const result = await b.send<{ result: { value: { x: number; y: number } } }>("Runtime.evaluate", {
    contextId: context.executionContextId,
    returnByValue: true,
    expression:
      "(() => { const r = document.querySelector('#plus').getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; })()",
  });
  await b.clickAt(iframe!.x + result.result.value.x, iframe!.y + result.result.value.y);
  await b.wait(`document.querySelector('[data-addon] .tab-count')?.textContent === '1'`);
  await b.eval(`globalThis.__addonFrame = document.querySelector('iframe[data-addon-id]')`);
  await b.eval(
    `dispatchEvent(new MessageEvent('message', {source:globalThis.__addonFrame.contentWindow, origin:location.origin, data:{muster:1,type:'badge',count:999}}))`,
  );
  await b.sleep(150);
  check(
    await b.eval(`document.querySelector('[data-addon] .tab-count')?.textContent === '1'`),
    "Wrong-origin message was ignored",
  );
  await selectChannel(first);
  await menu("Split right");
  const shape = (l: Layout): string =>
    l.type === "pane" ? JSON.stringify(l.tabs) : `${l.dir}(${l.children.map(shape).join(",")})`;
  const firstShape = shape(await root());
  await selectChannel(second);
  const target = allPanes(await root()).find((p) => p.tabs.some((id) => id > 0))!;
  const tab = (await b.rect("[data-addon]"))!,
    strip = (await b.rect(`[data-tabstrip="${target.id}"] .tabs`))!;
  await b.drag([tab.x + 24, tab.y + tab.h / 2], [strip.x + 10, strip.y + strip.h / 2]);
  await b.sleep(200);
  const secondShape = shape(await root());
  await selectChannel(first);
  check(shape(await root()) === firstShape, "First channel keeps its place");
  await selectChannel(second);
  const back = shape(await root());
  check(back === secondShape, `Second channel keeps its place: ${back} != ${secondShape}`);
  await menu("Hide");
  await b.wait(`!document.querySelector('[data-addon]')`);
  await palette("Show Counter");
  await b.wait(`!!document.querySelector('[data-addon].active')`);
  check(shape(await root()) === secondShape, "Hide/show keeps the place");
  check(
    await b.eval(`globalThis.__addonFrame === document.querySelector('iframe[data-addon-id]')`),
    "Frame node survived moves and hide/show",
  );
  check(
    await b.eval(`document.querySelector('[data-addon] .tab-count')?.textContent === '1'`),
    "Counter did not reload",
  );
  await shot("counter-tab-dark", ".split-root");
  console.log(
    "passed: dialog registration, validation, counter badge, wrong origin, per-channel places, hide/show, frame lifetime; 3 screenshots",
  );
} finally {
  await b.close();
}
