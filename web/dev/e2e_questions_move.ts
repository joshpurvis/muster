// Moving the Questions tab (docs/UI.md "Questions"), against the isolated mock only: drag it into
// the right group, into a split of its own on the right, reload, switch channels, hide and show.
// Then Josh's story (16 Nov): each channel keeps its own place. Set it in one channel, set it
// somewhere else in the other, and switching back and forth (and hide/show, reload) shows each
// channel's own place every time.
//
//   deno task build:dev && MOCK_PORT=17795 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_questions_move.ts http://127.0.0.1:17795/#token=dev <shots-dir>
import { type Browser, launch } from "./cdp.ts";
import { allPanes, type Layout, QUESTIONS_TAB as Q } from "../src/layout/tree.ts";

/** A layout's shape: groups (Questions as Q, terminals as t), splits with their sizes. */
function shape(n: Layout): string {
  if (n.type === "pane") return `[${n.tabs.map((t) => t === Q ? "Q" : "t").join(",")}]`;
  return `${n.dir}(${n.children.map((c, i) => `${shape(c)}:${n.sizes[i].toFixed(3)}`).join(" ")})`;
}

const [url, shots = "/tmp/muster-questions-move"] = Deno.args;
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
await Deno.mkdir(shots, { recursive: true });
const M = "globalThis.__muster";
const check = (v: unknown, message: string) => { if (!v) throw new Error(message); };
const ok = (message: string) => console.log(`✓ ${message}`);

const b: Browser = await launch(9371, await Deno.makeTempDir({ prefix: "muster-questions-move-" }));
const R = (m: string, p: unknown = {}): Promise<any> => b.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
const root = async (): Promise<Layout> => JSON.parse(await b.eval(`JSON.stringify(${M}.layout.value.root)`));
const qGroup = async () => {
  const panes = allPanes(await root());
  return { panes, at: panes.findIndex((p) => p.tabs.includes(Q)) };
};
const shot = async (name: string) => {
  await b.sleep(350);
  await b.shot(`${shots}/${name}.png`);
};
const load = async (questions = true) => {
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`, 10000);
  if (questions) await b.wait(`!!document.querySelector('.questions-tab')`, 10000);
  await b.eval(`document.fonts.ready`);
};
/** A channel in /tmp with one shell (the New channel dialog, as e2e_channels.ts does). */
async function create(name: string): Promise<number> {
  await b.click('button[aria-label="New channel"]');
  await b.wait(`document.querySelector('.dialog h2')?.textContent === 'New channel'`);
  await b.eval(`(() => {
    const folder = document.querySelectorAll('.dialog input')[0];
    folder.value = '/tmp'; folder.dispatchEvent(new Event('input', {bubbles:true}));
    const name = document.querySelector('.dialog input[aria-label="Name (optional)"]');
    name.value = ${JSON.stringify(name)}; name.dispatchEvent(new Event('input', {bubbles:true}));
    [...document.querySelectorAll('.dialog [role="radio"]')].find(b => b.textContent.trim() === 'Shell').click();
  })()`);
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!document.querySelector('.dialog')`, 10000);
  await b.wait(`!!document.querySelector('.pane .xterm')`, 8000);
  return await b.eval(`${M}.activeChannelId.value`);
}
const menuItem = async (label: string) => {
  const find = `Array.from(document.querySelectorAll('[role="menuitem"]')).find(el => el.textContent.includes(${JSON.stringify(label)}))`;
  await b.wait(`!!${find}`);
  await b.eval(`${find}.click()`);
};

try {
  await b.size(1440, 900);
  await b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  await load(false);
  check(String((await R("hello")).version).includes("mock"), "Only the mock daemon is allowed");
  // Two channels (a fresh mock window has none); the first one shown.
  const other = await create("Beta");
  const first = await create("Alpha");
  await b.wait(`!!document.querySelector('.questions-tab')`, 10000);
  // Two open questions, so the tab carries its count.
  const asker: number = await b.eval(`${M}.focusedSession.value.id`);
  for (const [question, holding_up] of [
    ["Keep the Questions tab on the right in every channel?", true],
    ["Show answered questions for a week instead of a day?", false],
  ] as const) {
    await R("mock.question_post", { asker_session: asker, asker: "builder", product: "Muster", question, holding_up,
      context: "You moved the Questions tab.", options: [{ text: "Yes" }, { text: "No" }], pick: ["a"],
      if_no_answer: "I keep it as it is." });
  }
  await b.wait(`document.querySelector('.questions-tab .tab-count')?.textContent === '2'`);
  // Two groups side by side; Questions first in the left one, as before.
  if (allPanes(await root()).length < 2) {
    await b.click(".pane .xterm");
    await b.key("Alt+D");
    await b.wait(`document.querySelectorAll('.pane').length >= 2`, 6000);
  }
  let g = await qGroup();
  check(g.at === 0 && g.panes[0].tabs[0] === Q, "Questions starts first in the first group");
  await shot("01-before-first-in-left-group-dark");
  ok("Questions starts first in the left group");

  // Drag it into the right group's strip, after its tabs.
  const right = g.panes[g.panes.length - 1].id;
  const from = await b.rect(".questions-tab");
  const strip = await b.rect(`[data-tabstrip="${right}"] .tab-new`);
  check(from && strip, "drag targets exist");
  await b.drag([from!.x + 30, from!.y + from!.h / 2], [strip!.x - 6, strip!.y + strip!.h / 2]);
  await b.wait(`${M}.layout.value && JSON.stringify(${M}.layout.value.root).length > 0`);
  g = await qGroup();
  check(g.at === g.panes.length - 1, `Questions is in the right group (${g.at})`);
  check(g.panes[g.at].active === Q, "and shown there");
  check((await b.eval(`JSON.stringify(${M}.channelLayout.value.questionsAt)`)) === JSON.stringify({ group: -1, index: -1 }),
    "the window remembers the right group");
  await shot("02-dragged-into-right-group-dark");
  ok("Dragged into the right group, it stays there and is remembered");

  // Onto the right edge of the right group: a split of its own along the window's right edge.
  const body = await b.rect(`[data-pane-body="${right}"]`);
  const tab = await b.rect(".questions-tab");
  await b.drag([tab!.x + 30, tab!.y + tab!.h / 2], [body!.x + body!.w - 12, body!.y + body!.h / 2]);
  await b.wait(`JSON.parse(JSON.stringify(${M}.channelLayout.value.questionsAt ?? {})).alone === 'right'`);
  g = await qGroup();
  check(g.panes.length === 3 && g.at === 2 && g.panes[2].tabs.length === 1, "Questions has the right split to itself");
  await shot("03-own-split-on-the-right-dark");
  ok("Dropped on the right edge, it gets a split of its own");

  // Reload: the same place.
  await b.sleep(700); // the layout save is debounced (400ms)
  await load();
  g = await qGroup();
  check(g.panes.length === 3 && g.at === 2 && g.panes[2].tabs.length === 1, "after reload, still in its own right split");
  await b.click(".questions-tab");
  await b.wait(`document.querySelector('.questions-tab')?.classList.contains('active')`);
  await shot("04-after-reload-right-split-open-dark");
  ok("After a reload it is still in its right split");

  // Another channel: it arrives in a right split of its own there too, and leaves no empty
  // group behind.
  check((await b.eval(`${M}.activeChannelId.value`)) === first, "still on the first channel");
  const firstGroups = g.panes.length;
  await b.click(`[data-channel-id="${other}"] .channel-select`);
  await b.wait(`${M}.activeChannelId.value === ${other}`);
  await b.wait(`!!document.querySelector('.questions-tab')`);
  g = await qGroup();
  check(g.at === g.panes.length - 1 && g.panes[g.at].tabs.length === 1 && g.panes.length >= 2,
    "in the other channel, a right split of its own");
  const left = JSON.parse(await b.eval(`JSON.stringify(${M}.channelLayout.value.layouts[${first}].root)`)) as Layout;
  check(allPanes(left).length === firstGroups - 1 && !allPanes(left).some((p) => p.tabs.includes(Q)),
    "the first channel lost its Questions split and has no copy");
  await shot("05-other-channel-right-split-dark");
  ok("Another channel gets it in a right split too");

  // Hide it from its menu, then show it from the palette: back on the right.
  const t = await b.rect(".questions-tab");
  await b.clickAt(t!.x + 20, t!.y + t!.h / 2, { button: "right" });
  await menuItem("Hide Questions tab");
  await b.wait(`!document.querySelector('.questions-tab')`);
  await b.key("Ctrl+Shift+P");
  await b.wait(`!!document.querySelector('.switcher')`);
  await b.type("Questions (what");
  await b.sleep(150);
  await b.key("Enter");
  await b.wait(`!!document.querySelector('.questions-tab.active')`);
  g = await qGroup();
  check(g.at === g.panes.length - 1 && g.panes[g.at].tabs.length === 1, "shown again in its right split");
  check((await b.eval(`${M}.layout.value.focus`)) === g.panes[g.at].id, "and focused");
  ok("Hidden and shown again from the palette, it is back in its right split, focused");

  // ---- Josh's story: one place per channel. Beta ("marketer", one terminal) and Alpha ("otis",
  // two terminals side by side) get different places, and each keeps its own.
  const shown = async (id: number) => {
    await b.click(`[data-channel-id="${id}"] .channel-select`);
    await b.wait(`${M}.activeChannelId.value === ${id}`);
    await b.wait(`!!document.querySelector('.questions-tab')`);
    await b.sleep(150);
  };
  const now = async () => shape(await root());
  const log = async (what: string) => console.log(`  ${what}: ${await now()}`);
  // In Beta, put it first in the terminal's group.
  g = await qGroup();
  const betaTerm = g.panes.find((p) => !p.tabs.includes(Q))!.id;
  const q1 = await b.rect(".questions-tab");
  const firstTab = await b.rect(`[data-tabstrip="${betaTerm}"] .tab`);
  await b.drag([q1!.x + 30, q1!.y + q1!.h / 2], [firstTab!.x + 4, firstTab!.y + firstTab!.h / 2]);
  await b.wait(`${M}.layout.value.root.type === 'pane'`);
  const betaPlace = await now();
  check(betaPlace === "[Q,t]", `in Beta it is first in the terminal's group (${betaPlace})`);
  await log("Beta, set first in its group");
  // Back to Alpha: still in its right split of its own, where it was put there.
  await shown(first);
  await log("Alpha, switched back");
  g = await qGroup();
  check(g.panes.length === 3 && g.at === 2 && g.panes[2].tabs.length === 1,
    `Alpha still has it in its own right split (${await now()})`);
  ok("Setting it in Beta left Alpha's place alone");
  // In Alpha, move it under the right terminal: a split of its own inside that column.
  const alphaRight = g.panes[1].id;
  const q2 = await b.rect(".questions-tab");
  const rb = await b.rect(`[data-pane-body="${alphaRight}"]`);
  await b.drag([q2!.x + 30, q2!.y + q2!.h / 2], [rb!.x + rb!.w / 2, rb!.y + rb!.h - 12]);
  await b.wait(`JSON.stringify(${M}.layout.value.root).includes('"col"')`);
  const alphaPlace = await now();
  check(/^row\(\[t\]:[\d.]+ col\(\[t\]:[\d.]+ \[Q\]:[\d.]+\):[\d.]+\)$/.test(alphaPlace),
    `in Alpha it is under the right terminal (${alphaPlace})`);
  await log("Alpha, set under the right terminal");
  // Back and forth: each channel shows its own place every time.
  for (let round = 1; round <= 3; round++) {
    // The screenshots without the earlier "hidden" toast over them.
    if (round === 3) await b.wait(`!document.querySelector('.toast-list .toast')`, 8000);
    await shown(other);
    check(await now() === betaPlace, `round ${round}: Beta shows its own place (${await now()})`);
    if (round === 3) await shot("06-beta-first-in-its-group-after-switching-dark");
    await shown(first);
    check(await now() === alphaPlace, `round ${round}: Alpha shows its own place (${await now()})`);
    if (round === 3) await shot("07-alpha-under-right-terminal-after-switching-dark");
  }
  ok("Switching Alpha → Beta → Alpha three times kept each channel's own place, at its size");
  // Hide and show in Alpha: back under the right terminal.
  const q3 = await b.rect(".questions-tab");
  await b.clickAt(q3!.x + 20, q3!.y + q3!.h / 2, { button: "right" });
  await menuItem("Hide Questions tab");
  await b.wait(`!document.querySelector('.questions-tab')`);
  await b.key("Ctrl+Shift+P");
  await b.wait(`!!document.querySelector('.switcher')`);
  await b.type("Questions (what");
  await b.sleep(150);
  await b.key("Enter");
  await b.wait(`!!document.querySelector('.questions-tab.active')`);
  check(await now() === alphaPlace, `hidden and shown, Alpha has it under the right terminal (${await now()})`);
  ok("Hide then show in Alpha brought it back under the right terminal");
  // Reload: both channels still keep their own places.
  await b.sleep(700);
  await load();
  check(await now() === alphaPlace, `after reload, Alpha's place (${await now()})`);
  await shown(other);
  check(await now() === betaPlace, `after reload, Beta's place (${await now()})`);
  await shown(first);
  check(await now() === alphaPlace, `after reload and a switch, Alpha's place (${await now()})`);
  ok("After a reload, each channel still shows its own place");
  console.log(`errors in console: ${b.console.filter((l) => /error/i.test(l)).length}`);
} catch (e) {
  await b.shot(`${shots}/failed.png`).catch(() => {});
  throw e;
} finally {
  await b.close();
}
