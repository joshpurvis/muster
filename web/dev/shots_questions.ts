// Screenshots of the Questions tab (docs/UI.md "Questions") for docs/screenshots/questions-*.webp,
// against the isolated mock only. Every question, name and picture here is invented (a made-up
// bakery): this repo is public, so sample data never comes from real work.
//
//   deno task build:dev && MOCK_PORT=17796 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/shots_questions.ts http://127.0.0.1:17796/#token=dev <shots-dir>
//
// These are headless Chromium pictures. New pictures of the desktop window come from WebKitGTK
// (AGENTS.md, "Screenshots of the UI"): its steps can set up the same questions through
// `globalThis.__muster.rpc.call(...)`. Before a picture from this script goes in a pull request or
// docs/screenshots, retake it that way.
//
// It writes PNGs; convert them for the repo with
//   for f in <shots-dir>/questions-*.png; do magick "$f" -quality 80 "${f%.png}.webp"; done
import { type Browser, launch } from "./cdp.ts";

const [url, shots = "/tmp/muster-questions-shots"] = Deno.args;
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
await Deno.mkdir(shots, { recursive: true });
const M = "globalThis.__muster";
const check = (v: unknown, message: string) => { if (!v) throw new Error(message); };

const b: Browser = await launch(9373, await Deno.makeTempDir({ prefix: "muster-questions-shots-" }));
const R = (m: string, p: unknown = {}): Promise<any> => b.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
const theme = (value: "dark" | "light") =>
  b.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value }] });
const shot = async (name: string) => {
  await b.sleep(400);
  await b.shot(`${shots}/${name}.png`);
  console.log(`✓ ${name}`);
};
/** Click the first element matching `sel` whose text includes `text`. */
const clickText = async (sel: string, text: string) => {
  const find = `Array.from(document.querySelectorAll(${JSON.stringify(sel)})).find(el => el.textContent.includes(${
    JSON.stringify(text)
  }))`;
  await b.wait(`!!${find}`, 5000);
  await b.eval(`${find}.click()`);
  await b.sleep(200);
};
const row = (text: string) => clickText(".qv-row", text);
const filter = (name: "Open" | "Answered") => clickText(".qv-chip", name);

/** The questions, oldest first. Ages are minutes before now. */
const BREAD_BOX = {
  product: "Bakery",
  context: "Each recipe page ends with the same bread box card. You already approved one body text for all twelve " +
    "pages. The headings above it still differ from page to page.",
};
const QUESTIONS = [
  {
    asker: "writer", age_min: 160, product: "Bakery", kind: "pick_one",
    question: "Should the bread box card on the recipe pages show a price?",
    context: "The card ends each of the twelve recipe pages. It offers the weekly bread box; an extra loaf costs more.",
    example: "Under the Order a box button the reader would see either the short delivery line only, or that line " +
      "plus what the box costs.",
    options: [{ text: "No price" }, { text: "Show what the box costs" }], pick: ["a"],
    if_no_answer: "The card goes out without a price.",
  },
  {
    asker: "writer", age_min: 25, ...BREAD_BOX, kind: "pick_one",
    question: "Which heading goes on the bread box card at the end of the recipe pages?",
    example: "Someone reading the rye loaf recipe sees the card headed either “Fresh bread on Friday mornings” or " +
      "“Let us do the kneading”.",
    picture: "The card today on the rye loaf page, desktop, light. Only the heading line changes.",
    options: [
      { text: "“Fresh bread on Friday mornings” on all twelve" },
      { text: "“Let us do the kneading” on all twelve" },
      { text: "A heading per page" },
    ],
    pick: ["a"],
    if_no_answer: "Each page keeps its own heading for now.",
  },
  {
    asker: "release", age_min: 18, product: "Bakery", kind: "pick_one", area: "merge",
    question: "Merge the pull request that puts the winter opening hours on the live site?",
    context: "The winter hours you approved on Monday went into a test branch instead of the live one, so the site " +
      "still shows the summer hours. This pull request carries the same change and nothing new. Checks are green.",
    example: "Until it merges, the contact page says the shop opens at 6 on Sundays; in winter it opens at 8.",
    options: [
      { text: "Yes, merge it", why: "Same hours you already approved, now aimed at the live branch." },
      { text: "No, hold it", why: "The holiday menu page behind it waits too." },
    ],
    pick: ["a"],
    holding_up: "the holiday menu page queued behind it",
    if_no_answer: "Waits for you. A merge is never taken by default.",
    links: [{ label: "Open the pull request", url: "https://example.com/bakery/pull/12" }],
  },
  {
    asker: "lead", age_min: 75, product: "Recipes", kind: "pick_several",
    question: "Which of these five style rules go into the recipe guide?",
    context: "The writer drafted five rules from the twelve recipe pages. Ticked ones go into the guide every recipe " +
      "follows from now on.",
    example: "With the first rule, a recipe says “500 g flour (4 cups)” rather than “4 cups flour”.",
    options: [
      { text: "Weights in grams first, cups in brackets" },
      { text: "Oven temperatures in °C and °F" },
      { text: "One step per numbered line" },
      { text: "Name the flour by its type, not its brand" },
      { text: "Say how the dough should look, not only how long it takes" },
    ],
    pick: ["a", "c", "e"],
    if_no_answer: "The guide waits; nothing is added.",
  },
  {
    asker: "lead", age_min: 153, product: "Bakery", kind: "own_words",
    question: "Can you paste the oven settings from the kitchen notebook?",
    context: "The new recipe pages give a baking time for each loaf. To match the shop's own ovens I need the " +
      "settings the bakers really use: one line for the sourdough, one for the rye.",
    example: "Open the kitchen notebook at the bread page and copy the two lines, such as “sourdough: 240 °C, " +
      "steam, 35 min”.",
    holding_up: "the recipe pages build",
    if_no_answer: "The recipe pages aren't built. Wording picks you already made are kept.",
  },
];

// Only the mock: ask its health route before the window loads anything (a real daemon would hand
// this browser a real window).
const health = await fetch(new URL("/api/health", url)).then((r) => r.json()).catch(() => null);
if (!String(health?.version ?? "").includes("mock")) {
  await b.close();
  throw new Error("Only the mock daemon is allowed: /api/health doesn't say mock");
}

try {
  await b.size(1100, 728);
  await b.send("Emulation.setTimezoneOverride", { timezoneId: "UTC" });
  await theme("dark");
  await b.goto(url);
  await b.wait(`!!document.querySelector('.app')`, 10000);
  await b.eval(`document.fonts.ready`);
  check(String((await R("hello")).version).includes("mock"), "Only the mock daemon is allowed");

  // A "bakery" channel with three agent tabs (the New channel dialog, as e2e_channels.ts does, with
  // Claude: the first tab becomes "writer").
  await b.click('button[aria-label="New channel"]');
  await b.wait(`document.querySelector('.dialog h2')?.textContent === 'New channel'`);
  await b.eval(`(() => {
    const folder = document.querySelectorAll('.dialog input')[0];
    folder.value = '/tmp'; folder.dispatchEvent(new Event('input', {bubbles:true}));
    const name = document.querySelector('.dialog input[aria-label="Name (optional)"]');
    name.value = 'bakery'; name.dispatchEvent(new Event('input', {bubbles:true}));
    [...document.querySelectorAll('.dialog [role="radio"]')].find(b => b.textContent.trim() === 'Claude').click();
  })()`);
  await b.click('.dialog button[type="submit"]');
  await b.wait(`!document.querySelector('.dialog')`, 10000);
  await b.wait(`!!document.querySelector('.pane .xterm')`, 8000);
  const channel: number = await b.eval(`${M}.activeChannelId.value`);
  const first: number = await b.eval(
    `[...${M}.sessions.value.values()].find(s => s.channel_id === ${channel}).id`,
  );
  await R("session.rename", { id: first, name: "writer" });
  const ids: Record<string, number> = { writer: first };
  for (const name of ["lead", "release"]) {
    ids[name] = (await R("session.spawn", { channel_id: channel, kind: "claude", name })).id;
  }
  await b.wait(`document.querySelectorAll('.tab').length >= 4 && !Array.from(document.querySelectorAll('.tab'))
    .some(t => t.textContent.trim() === 'claude')`, 8000);
  // Hide the sidebar (Alt+G), so the shots show the tab strip and the Questions view only.
  await clickText(".pane .tab", "writer");
  await b.key("Alt+G");
  await b.wait(`${M}.channelLayout.value?.sidebar.hidden === true`, 5000);
  await b.sleep(1500);

  await R("mock.question_hold", { ms: 600_000 }); // the Undo window stays open for the shots
  for (const [i, q] of QUESTIONS.entries()) {
    await R("mock.question_post", { number: i + 1, asker_session: ids[q.asker], ...q });
  }
  // Q1 was answered in chat and recorded by the lead.
  await R("mock.question_record", { id: 1, pick: ["a"], words: "no price", by: "lead", ago_min: 150 });
  await b.wait(`document.querySelector('.questions-tab .tab-count')?.textContent === '4'`, 5000);

  // 01: a terminal in front; the Questions tab carries the count.
  await clickText(".pane .tab", "writer");
  await shot("questions-01-needs-you-count-dark");

  // 02, 03: the list and the oldest question holding up work.
  await b.click(".questions-tab");
  await b.wait(`!!document.querySelector('.qv-row')`, 5000);
  await row("oven settings");
  await shot("questions-02-list-and-question-dark");
  await theme("light");
  await shot("questions-03-list-and-question-light");
  await theme("dark");

  // 06: an answer sent (Undo offered for a while), the next question open.
  await row("Which heading goes");
  // a) is the writer's pick and already chosen, so Send answers it.
  await clickText(".qv-btn.primary", "Send");
  await b.wait(`!!Array.from(document.querySelectorAll('.qv-row')).find(r => r.textContent.includes('winter'))`);
  await row("winter opening hours");
  await shot("questions-06-after-send-dark");

  // 07: the sent answer under Answered, with its Undo.
  await filter("Answered");
  await row("Which heading goes");
  await shot("questions-07-sent-undo-offered-dark");

  // 09: an answer given in chat and recorded by the lead.
  await theme("light");
  await row("show a price");
  await shot("questions-09-answered-from-chat-light");

  // 11: a narrow window shows one question at a time.
  await b.size(420, 860);
  await b.sleep(300);
  if (await b.eval(`!!document.querySelector('.qv-back')`)) await b.click(".qv-back");
  await filter("Open");
  await row("winter opening hours");
  await shot("questions-11-narrow-question-light");
  console.log(`errors in console: ${b.console.filter((l) => /error/i.test(l)).length}`);
} catch (e) {
  await b.shot(`${shots}/failed.png`).catch(() => {});
  throw e;
} finally {
  await b.close();
}
