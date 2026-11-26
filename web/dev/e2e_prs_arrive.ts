// A pull request card posted while the window is open gets noticed (docs/UI.md "PRs to merge",
// "A new card"): the tab showing a merged card moves to it, the tab showing a waiting card gets a
// cue in its bar and scrolls the strip to the new pill, and a tab in the background or hidden gets
// one toast with Show. Checked in headless Chromium against a fresh isolated mock:
//   deno run --config web/deno.json --allow-net=127.0.0.1 --allow-run=chromium \
//     --allow-read=<shots> --allow-write=<shots>,/tmp web/dev/e2e_prs_arrive.ts http://127.0.0.1:<port>/#token=dev <shots> [before]
// `before` runs the same steps without failing (for pictures of the old behaviour). /tmp (Deno's
// temp dir) holds the browser profile. Its pictures show that a step happened; the window's look is
// checked in WebKitGTK.
import { launch } from "./cdp.ts";

const [url, shots, mode] = Deno.args;
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
const health = await (await fetch(new URL("/api/health", url))).json();
if (health.version !== "mock") throw new Error("Only the isolated mock daemon is allowed");
const before = mode === "before";
const M = "globalThis.__muster";
const js = JSON.stringify;
const profile = await Deno.makeTempDir({ prefix: "muster-prs-arrive-" });
const a = await launch(9381, profile);
let step = 0;
const failed: string[] = [];
const ok = (m: string) => console.log(`✓ ${++step}. ${m}`);
const check = (c: unknown, m: string) => {
  if (c) return;
  if (!before) throw new Error("check failed: " + m);
  failed.push(m);
  console.log(`  (before) not so: ${m}`);
};
const waitFor = async (cond: string, m: string, ms = 4000) => {
  try {
    await a.wait(cond, ms);
  } catch {
    check(false, m);
  }
};
const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${js(m)}, ${js(p)})`);
const shot = async (name: string) => {
  if (shots) await a.shot(`${shots}/${name}${before ? "-before" : ""}.png`);
};
const title = (): Promise<string | null> => a.eval(`document.querySelector('.prs-view h2.qv-q')?.textContent ?? null`);
const prToasts = (): Promise<string[]> =>
  a.eval(`[...document.querySelectorAll('.toast .toast-text')].map((e) => e.textContent).filter((t) => /PRs? to merge:/.test(t))`);
const tabShown = (): Promise<boolean> => a.eval(`!!document.querySelector('.prs-tab')`);
const tabActive = (): Promise<boolean> => a.eval(`!!document.querySelector('.prs-tab.active')`);
const NB = (t: string) => t.replaceAll("-", "‑");
/** The pill of #n lies inside the strip's visible part. */
const pillVisible = (n: number): Promise<boolean> =>
  a.eval(`(() => { const s = document.querySelector('.pb-strip'); const p = [...document.querySelectorAll('.pb-pill')]
    .find((e) => e.querySelector('.qv-qn')?.textContent === '#${n}'); if (!s || !p) return false;
    const a = s.getBoundingClientRect(), b = p.getBoundingClientRect(); return b.left >= a.left - 1 && b.right <= a.right + 1; })()`);
const clickToastShow = () =>
  a.eval(`(() => { const t = [...document.querySelectorAll('.toast')].find((e) => /PRs? to merge:/.test(e.textContent));
    const b = t?.querySelector('.toast-action'); if (!b) return false; b.click(); return true; })()`);
const pickPill = async (n: number) => {
  await a.eval(`[...document.querySelectorAll('.pb-pill')].find((e) => e.querySelector('.qv-qn')?.textContent === '#${n}')?.click()`);
  await a.sleep(400);
};
const shellTab = `.tab[data-tab-sid]:not(.prs-tab):not(.questions-tab):not(.addon-tab)`;

try {
  await a.size(1100, 820);
  await a.goto(url);
  await a.wait(`!!${M}?.rpc && ${M}.windowId.value != null`, 15000);
  // One tab in this window, so the PRs to merge tab has a strip to sit in.
  if (!(await a.eval(`${M}.windowSessions.value.length`))) {
    await a.wait(`!!document.querySelector('.empty-window input')`, 8000);
    await a.eval(`(() => { const i = document.querySelector('.empty-window input');
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      set.call(i, '/tmp'); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
    await a.click(".empty-window .launch-btn:nth-child(3)");
    await a.wait(`${M}.windowSessions.value.length === 1`, 8000);
  }
  // The sample cards were there before the window opened: no toast for them.
  await R("mock.pr_seed", {});
  await a.goto("about:blank");
  await a.goto(url);
  await a.wait(`!!document.querySelector('.prs-tab')`, 15000);
  await a.sleep(800);
  check((await prToasts()).length === 0, "no toast for the cards the window found on opening");
  check(!(await tabActive()), "the PRs tab starts in the background");
  ok("window opened on 8 sample cards: the tab in the background, no toast");

  // ---- 1. tab in the background: one toast per post, not per refresh
  const n240 = await R("mock.pr_post", { n: 240, title: "Pin a note to the top of the list" });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === 'New PR to merge: acme/notes-app #240')`,
    "a toast says: New PR to merge: acme/notes-app #240");
  await shot("prs-arrive-1-toast");
  // Dismissed, then read twice (its first read comes now): no toast comes back.
  await a.eval(`document.querySelectorAll('.toast .toast-x').forEach((b) => b.click())`);
  await R("pr.refresh", {});
  await R("pr.refresh", { id: n240.id, force: true });
  await a.sleep(600);
  check((await prToasts()).length === 0, `no toast again after two refreshes: ${js(await prToasts())}`);
  await R("mock.pr_post", { again: n240.id });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === 'New PR to merge: acme/notes-app #240')`,
    "posted again: a toast again");
  check(await a.eval(`!!document.querySelector('.prs-tab .tab-count')`), "the tab's badge counts it");
  check(await clickToastShow(), "the toast has Show");
  await waitFor(`!!document.querySelector('.prs-tab.active') && !!document.querySelector('.prs-view h2.qv-q')`, "Show opens the tab");
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Pin a note to the top of the list"))}`,
    `Show opens it on the new card, read from GitHub: ${await title()}`);
  if (before) {
    // The old window had no Show: open the tab and the card by hand.
    await a.click(".prs-tab");
    await a.wait(`!!document.querySelector('.pb-strip')`, 4000);
    await pickPill(240);
  }
  ok("tab in the background: one toast per post, Show opens the tab on the new card");

  // ---- 2. tab showing a card just merged from it: the new card takes its place
  check(await a.eval(`!document.querySelector('.pm-merge')?.disabled`), "the new card is ready");
  await a.sleep(700);
  await a.click(".pm-merge");
  await waitFor(`document.querySelector('.prs-view .qv-ah')?.textContent === 'Merged'`, "merged from the tab", 12000);
  await a.sleep(300);
  await shot("prs-arrive-2a-merged");
  const n241 = await R("mock.pr_post", { n: 241, title: "Sort notes by last edit" });
  // Posted unread (no title yet): the tab asks for its read at once, not at the next 2 minute one.
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Sort notes by last edit"))}`,
    "the stage moves from the merged card to the new one, read at once");
  await waitFor(`!!document.querySelector('.pm-merge:not(:disabled)')`, "its Merge is on");
  check((await prToasts()).length === 0, "no toast while the tab is in front");
  await a.sleep(400);
  await shot("prs-arrive-2b-moved");
  ok("tab showing a just-merged card: the new card replaces it");

  // ---- 2c. a merged card with the pointer on its dock: no move, the cue instead
  await a.click(".qv-chip:nth-of-type(2)"); // Merged
  await a.wait(`document.querySelector('.prs-view .qv-ah')?.textContent === 'Merged'`, 4000);
  const dockBtn = await a.rect(".pb-dock .qv-btn, .pb-dock .qv-link");
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: dockBtn!.x + 5, y: dockBtn!.y + 5 });
  await a.sleep(200);
  const mergedTitle = await title();
  await R("mock.pr_post", { n: 244, title: "Undo a deleted note for a minute" });
  await waitFor(`!!document.querySelector('.pb-new')`, "the cue shows while the pointer is on the dock");
  check((await title()) === mergedTitle, "the stage stays while the pointer is on its dock");
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 5, y: 400 });
  if (before) await pickPill(244);
  else await a.click(".pb-new");
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Undo a deleted note for a minute"))}`,
    "the cue shows the new card");
  ok("pointer on the dock of a merged card: the cue, not a move");

  // ---- 3. tab showing a waiting card: the cue in the bar, the strip scrolled to the new pill
  await a.click(".qv-chip:nth-of-type(1)"); // Waiting
  await a.sleep(300);
  await a.eval(`document.querySelector('.pb-pill')?.click()`);
  await a.eval(`document.querySelector('.pb-strip').scrollLeft = 0`);
  await a.sleep(800);
  const waitingTitle = await title();
  await shot("prs-arrive-3a-waiting");
  await R("mock.pr_post", { n: 242, title: "Show the note's last edit time on hover" });
  await waitFor(`document.querySelector('.pb-new')?.textContent === "1 new: Show the note's last edit time on hover"`,
    `the cue reads "1 new: <title>": ${await a.eval(`document.querySelector('.pb-new')?.textContent ?? ''`)}`);
  check((await title()) === waitingTitle, "the waiting card stays shown");
  await a.sleep(700);
  check(await pillVisible(242), "the strip scrolled the new pill into view");
  check(await a.eval(`!!document.querySelector('.pb-pill.new')`), "the new pill is marked");
  check((await prToasts()).length === 0, "no toast while the tab is in front");
  await a.sleep(200);
  await shot("prs-arrive-3b-cue");
  if (before) await pickPill(242);
  else await a.click(".pb-new");
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Show the note's last edit time on hover"))}`,
    "the cue shows the new card");
  check(await a.eval(`!document.querySelector('.pb-new')`), "the cue goes once it is shown");
  ok("tab showing a waiting card: the cue, the pill in view; the cue shows the card");

  // ---- 3a. the card shown posted again: the bar says so until it is looked at
  const n242 = await a.eval(`${M}.rpc.call('pr.list', {}).then((r) => r.prs.find((p) => p.number === 242).id)`);
  await R("mock.pr_post", { again: n242 });
  await waitFor(`document.querySelector('.pb-new')?.textContent === "Posted again: Show the note's last edit time on hover"`,
    "the card shown, posted again: the cue says so");
  await a.click(".pb-new");
  await waitFor(`!document.querySelector('.pb-new')`, "looked at: the cue goes");
  ok("the card shown posted again: Posted again in the bar until clicked");

  // ---- 3b. a withdrawn arrival takes its cue with it
  const n245 = await R("mock.pr_post", { n: 245, title: "Archive old notes" });
  await waitFor(`!!document.querySelector('.pb-new')`, "the cue for #245");
  await R("mock.pr_withdraw", { id: n245.id });
  await waitFor(`!document.querySelector('.pb-new')`, "a withdrawn card's cue goes");
  ok("a card withdrawn before it was seen leaves no cue");

  // ---- 4. a card posted again while the tab is in the background: a toast
  await a.click(shellTab);
  await a.sleep(500);
  check(!(await tabActive()), "the shell tab is in front");
  await R("mock.pr_post", { again: n241.id });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === 'New PR to merge: acme/notes-app #241')`,
    "a toast for a card posted again");
  ok("a card posted again: a toast");
  await a.eval(`document.querySelectorAll('.toast .toast-x').forEach((b) => b.click())`);

  // ---- 5. the tab hidden: a toast, the tab stays hidden until Show
  await a.click(".prs-tab");
  await a.sleep(300);
  // Back in the tab, the card posted again waits in the cue: look at it.
  await waitFor(`document.querySelector('.pb-new')?.textContent === '1 new: Sort notes by last edit'`,
    "back in the tab, the cue names the card posted again");
  if (before) await pickPill(241);
  else await a.click(".pb-new");
  await a.sleep(300);
  await a.key("Alt+W");
  await a.wait(`!document.querySelector('.prs-tab')`, 4000);
  await a.sleep(400);
  await a.eval(`document.querySelectorAll('.toast .toast-x').forEach((b) => b.click())`);
  await a.sleep(200);
  await R("mock.pr_post", { n: 243, title: "Export a note as Markdown" });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === 'New PR to merge: acme/notes-app #243')`,
    "a toast for the hidden tab");
  await a.sleep(1500);
  check(!(await tabShown()), "the tab stays hidden by itself");
  await shot("prs-arrive-5-hidden-toast");
  check(await clickToastShow(), "the toast has Show");
  if (before) {
    await a.key("Ctrl+Shift+P");
    await a.type("PRs to merge");
    await a.key("Enter");
  }
  await waitFor(`!!document.querySelector('.prs-tab.active')`, "Show brings the hidden tab back");
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Export a note as Markdown"))}`,
    `on the new card: ${await title()}`);
  ok("hidden tab: a toast, the tab stays hidden until Show");

  // ---- 6. several at once in the background: one toast that counts them
  await a.click(shellTab);
  await a.sleep(400);
  // A toast dismissed is not named again by the next one.
  await R("mock.pr_post", { n: 249, title: "Print a note" });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === 'New PR to merge: acme/notes-app #249')`,
    "a toast for #249");
  await a.eval(`document.querySelectorAll('.toast .toast-x').forEach((b) => b.click())`);
  const n250 = await R("mock.pr_post", { n: 250, title: "Tags on notes" });
  await R("mock.pr_post", { n: 251, title: "Colour a tag" });
  await R("mock.pr_post", { n: 252, title: "Filter by tag" });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === '3 new PRs to merge: acme/notes-app #250 and 2 more')`,
    `one toast counts the three new cards, not the dismissed one: ${js(await prToasts())}`);
  check((await prToasts()).length === 1, `one toast for three: ${js(await prToasts())}`);
  await shot("prs-arrive-6-three");
  // The first withdrawn: the toast names, and Show opens, the next.
  await R("mock.pr_withdraw", { id: n250.id });
  await waitFor(`[...document.querySelectorAll('.toast .toast-text')].some((e) => e.textContent === '2 new PRs to merge: acme/notes-app #251 and 1 more')`,
    `a withdrawn card leaves the toast: ${js(await prToasts())}`);
  check(await clickToastShow(), "the toast has Show");
  await waitFor(`document.querySelector('.prs-view h2.qv-q')?.textContent === ${js(NB("Colour a tag"))}`, "Show opens the card it names");
  ok("three at once: one toast; a dismissed one isn't named again; a withdrawn one leaves it");

  // narrow: the cue still fits the bar
  await a.size(640, 820);
  await a.sleep(500);
  await R("mock.pr_post", { n: 253, title: "A much longer title that has to be cut short to fit a narrow bar" });
  await waitFor(`!!document.querySelector('.pb-new')`, "the cue at 640 px");
  check(await a.eval(`(() => { const b = document.querySelector('.prs-view .qv-bar'); return b.scrollWidth <= b.clientWidth + 1; })()`),
    "the bar doesn't overflow at 640 px");
  await shot("prs-arrive-7-narrow");
  ok("narrow window: the cue fits");

  console.log(before ? `before: ${failed.length} checks did not hold` : `all ${step} steps passed`);
} finally {
  await a.close();
}
