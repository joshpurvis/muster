// The sidebar's one channel list with agents in their channels' rows (docs/UI.md "Channels
// sidebar", "Agents in the sidebar"), checked with real keys and pointer moves in headless
// Chromium. Run only against a fresh isolated mock:
//   deno run --config web/deno.json --allow-net=127.0.0.1 --allow-run=chromium \
//     --allow-read=<shots> --allow-write=<shots>,/tmp web/dev/e2e_sidebar.ts http://127.0.0.1:<port>/#token=dev <shots>
// /tmp (Deno's temp dir) holds the browser profile. Its pictures show that a step happened; the
// desktop window's look is checked in WebKitGTK (web/dev/webkit_shots/sidebar.json).
import { launch } from "./cdp.ts";

const [url, shots] = Deno.args;
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
const health = await (await fetch(new URL("/api/health", url))).json();
if (health.version !== "mock") throw new Error("Only the isolated mock daemon is allowed");
const M = "globalThis.__muster";
const js = JSON.stringify;
const profile = await Deno.makeTempDir({ prefix: "muster-sidebar-" });
const a = await launch(9376, profile);
let step = 0;
const ok = (m: string) => console.log(`✓ ${++step}. ${m}`);
const check = (c: unknown, m: string) => {
  if (!c) throw new Error("check failed: " + m);
};
const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${js(m)}, ${js(p)})`);
const shot = async (name: string) => {
  if (shots) await a.shot(`${shots}/${name}.png`);
};
const row = (agent: string) => `.channel-row[data-agent=${js(agent)}]`;
const sel = (agent: string) => `${row(agent)} .channel-select`;
const focused = (q: string) => a.eval(`document.activeElement?.matches(${js(q)}) ?? false`);
const menuLabels = (): Promise<string[]> =>
  a.eval(`[...document.querySelectorAll('.menu .menu-label')].map((e) => e.textContent)`);
const menuItem = (label: string) =>
  a.eval(`(() => { const it = [...document.querySelectorAll('.menu .menu-item')]
    .find((e) => e.querySelector('.menu-label')?.textContent === ${js(label)});
    if (!it) return false; it.click(); return true; })()`);
const move = (x: number, y: number) => a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });

try {
  await a.size(1400, 900);
  await a.goto(url);
  await a.wait(`!!${M}?.rpc && ${M}.windowId.value != null && ${M}.agents.value.length >= 4`, 15000);

  // Two agents with channels here: one renamed, so its row shows both names.
  const w = await a.eval(`${M}.windowId.value`);
  await R("agent.start", { name: "orchestrator", window_id: w });
  await R("agent.start", { name: "reviewer", window_id: w });
  await a.wait(`!!${M}.agents.value.find((x) => x.name === 'orchestrator')?.channel_id`, 5000);
  const orch = await a.eval(`${M}.agents.value.find((x) => x.name === 'orchestrator')`);
  await R("channel.rename", { id: orch.channel_id, name: "planning" });
  await R("mock.propose", {
    agent: "worker", by: "orchestrator", name: "Weekly dependency check",
    prompt: "List the dependency updates waiting in the open pull requests.", reason: "Once a week is enough.",
  });

  // ---- rows: one list; the renamed channel shows the agent's name faint beside it
  await a.wait(`document.querySelector(${js(`${row("orchestrator")} .channel-agent`)})?.textContent.trim() === 'orchestrator'`, 4000);
  check(await a.eval(`document.querySelector(${js(`${row("orchestrator")} .channel-name`)}).firstChild.textContent`) === "planning", "the channel's name leads");
  check(await a.eval(`!document.querySelector(${js(`${row("reviewer")} .channel-agent`)})`), "same names: shown once");
  check(await a.eval(`!document.querySelector('.agents-panel, .ap-row')`), "no Agents section");
  const order: string[] = await a.eval(`[...document.querySelectorAll('nav[aria-label=Channels] .channel-row')].map((r) => r.dataset.agent ?? 'task:' + r.dataset.channelId)`);
  check(
    order.indexOf("digest") > order.indexOf("orchestrator") && order.indexOf("worker") > order.indexOf("reviewer"),
    `agents without a channel here come after the channels: ${order}`,
  );
  check(
    (await a.eval(`document.querySelector(${js(sel("orchestrator"))}).getAttribute('aria-label')`)).startsWith("planning, agent orchestrator, "),
    "the row's spoken name has both names and its state",
  );
  ok(`one list: ${order.join(", ")}; the renamed channel shows both names`);

  // ---- Needs you: the proposal and the blocked run; the worker's row flags them
  await a.wait(`document.querySelectorAll('.needs-you .ny-item').length === 2`, 3000);
  check(await a.eval(`document.querySelector('.needs-you .ny-prop .ny-name').textContent`) === "Weekly dependency check", "the proposal is first");
  check(await a.eval(`document.querySelector('.needs-you .ny-run .ny-name').textContent`) === "CI check", "then the blocked run");
  check(await a.eval(`document.querySelector(${js(`${row("worker")} .row-chip-bad`)})?.textContent`) === "2", "the worker's row has a red chip: 2");
  await shot("sidebar-needs-you");
  ok("Needs you holds the proposal and the blocked run; the worker's row has the red chip");

  // ---- the card by keyboard: → opens and focuses it, Tab walks its buttons and leaves it, Esc and ← close
  await a.eval(`document.querySelector(${js(sel("orchestrator"))}).focus()`);
  await a.key("Right");
  await a.wait(`document.activeElement?.closest('.agent-card') && document.activeElement.textContent === 'Show'`, 2000);
  check(await a.eval(`document.querySelector(${js(sel("orchestrator"))}).getAttribute('aria-expanded')`) === "true", "the row says its card is open");
  const tabs: string[] = [];
  for (let i = 0; i < 5; i++) {
    tabs.push(await a.eval(`document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.textContent`));
    await a.key("Tab");
  }
  check(tabs.join("|") === "Show|New tab|Memory…|Start fresh|More for orchestrator", `Tab walks the card's buttons: ${tabs}`);
  // Past its last button (no schedules here) focus leaves the card: it closes, no trap.
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  check(!(await a.eval(`!!document.activeElement?.closest('.agent-card')`)), "focus moved on");
  await a.eval(`document.querySelector(${js(sel("orchestrator"))}).focus()`);
  await a.key("Right");
  await a.wait(`!!document.activeElement?.closest('.agent-card')`, 2000);
  await shot("sidebar-card-keyboard");
  await a.key("Escape");
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  check(await focused(sel("orchestrator")), "Esc puts focus back on the row");
  await a.key("Right");
  await a.wait(`!!document.activeElement?.closest('.agent-card')`, 2000);
  await a.key("Left");
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  check(await focused(sel("orchestrator")), "← does too");
  // → again on a row whose card is already open moves focus back in.
  await a.key("Right");
  await a.wait(`!!document.activeElement?.closest('.agent-card')`, 2000);
  await a.key("Shift+Tab");
  await a.key("Shift+Tab");
  check(await focused(sel("orchestrator")), "Shift+Tab goes back to the row");
  check(await a.eval(`!!document.querySelector('.agent-card')`), "the card stays while focus is in its row");
  await a.key("Right");
  await a.wait(`!!document.activeElement?.closest('.agent-card')`, 2000);
  // A dialog opened from the card gives focus back to the card's button.
  for (let i = 0; i < 2; i++) await a.key("Tab");
  check(await a.eval(`document.activeElement?.textContent`) === "Memory…", "Tab reaches Memory…");
  await a.key("Enter");
  await a.wait(`!!document.activeElement?.closest('.agent-dlg')`, 3000);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.agent-dlg')`, 2000);
  await a.wait(`document.activeElement?.textContent === 'Memory…' && !!document.activeElement.closest('.agent-card')`, 2000);
  await a.sleep(150);
  check(await a.eval(`document.activeElement?.textContent === 'Memory…'`), "and keeps it (the terminal doesn't take it)");
  await a.key("Escape");
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  // ↑/↓, Home and End move between rows.
  await a.key("Down");
  check(await focused(sel("reviewer")), "↓ moves to the next row");
  await a.key("Up");
  check(await focused(sel("orchestrator")), "↑ back");
  await a.key("End");
  check(await focused(".channel-fallback"), "End: the last row (Notes)");
  await a.key("Home");
  check(await a.eval(`document.activeElement === document.querySelector('nav[aria-label=Channels] .channel-select')`), "Home: the first row");
  // Shift+F10 opens the row's menu with its first action chosen (not the heading), spoken as such.
  await a.eval(`document.querySelector(${js(sel("orchestrator"))}).focus()`);
  await a.key("Shift+F10");
  await a.wait(`!!document.querySelector('.menu .menu-item.sel')`, 2000);
  check(await a.eval(`document.querySelector('.menu .menu-item.sel .menu-label').textContent`) === "Edit…", "the first action is chosen");
  check(
    await a.eval(`document.querySelector('.menu').getAttribute('aria-activedescendant') === document.querySelector('.menu .menu-item.sel').id`),
    "the menu tells screen readers which item is chosen",
  );
  check(await a.eval(`[...document.querySelectorAll('.menu [role=menuitemcheckbox]')].some((e) => e.textContent.includes('Autostart'))`), "Autostart is a checkbox item");
  await a.key("Escape");
  ok("→ opens the card with focus in it (again after Shift+Tab); Tab walks it and leaves; Esc and ← close it; a dialog gives focus back; rows take ↑/↓/Home/End; Shift+F10 picks the first action");

  // ---- the card by hover: opens after a pause, stays while the pointer moves into it, closes after
  const r = (await a.rect(row("worker")))!;
  await move(r.x + 40, r.y + r.h / 2);
  await a.wait(`!!document.querySelector('#agent-card-' + ${M}.agents.value.find((x) => x.name === 'worker').id)`, 2000);
  const c = (await a.rect(".agent-card"))!;
  check(c.x >= r.x + r.w, "the card sits beside the sidebar");
  await move(r.x + r.w + 3, r.y + r.h / 2);
  await move(c.x + 20, c.y + 20);
  await a.sleep(500);
  check(await a.eval(`!!document.querySelector('.agent-card')`), "moving into the card keeps it open");
  check(await a.eval(`document.querySelectorAll('.agent-card .sch-prop, .agent-card .sch-row').length`) === 3, "its schedules: the proposal card and two rows");
  await shot("sidebar-card-hover");
  // Shift+F10 on a schedule in the card opens the schedule's menu, not the row's.
  await a.eval(`document.querySelector('.agent-card .sch-row').focus()`);
  await a.key("Shift+F10");
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  check(
    (await menuLabels()).includes("Run now") && !(await menuLabels()).includes("Archive channel…"),
    `the schedule's own menu: ${await menuLabels()}`,
  );
  await a.key("Escape");
  await move(c.x + c.w + 200, c.y + 20);
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  // A hover card closes on Esc wherever focus is.
  await a.eval(`document.activeElement?.blur()`);
  await move(r.x + 40, r.y + r.h / 2);
  await a.wait(`!!document.querySelector('.agent-card')`, 2000);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.agent-card')`, 2000);
  ok("hover opens the card beside the row; it stays while the pointer is in it and closes after it leaves");

  // ---- one menu: the agent's items, then the channel's
  await a.click(sel("orchestrator"), { button: "right" });
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  const labels = await menuLabels();
  for (const l of ["Edit…", "Memory…", "New schedule…", "Start fresh", "Stop", "Autostart", "Remove…", "Rename", "New tab", "Permissions…", "Archive channel…", "Hide sidebar"]) {
    check(labels.includes(l), `the row's menu has ${l}: ${labels}`);
  }
  check(labels.indexOf("Remove…") < labels.indexOf("Rename"), "the agent's part comes first");
  check(await a.eval(`[...document.querySelectorAll('.menu .menu-header')].map((h) => h.textContent).join('|')`) === "planning · agent orchestrator|Channel", "headed with both names, then Channel");
  await a.key("Escape");
  // In a short window the long menu scrolls, so its last actions stay reachable.
  await a.size(1400, 480);
  await a.eval(`document.querySelector(${js(sel("orchestrator"))}).scrollIntoView({ block: 'center' })`);
  await a.click(sel("orchestrator"), { button: "right" });
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  await a.sleep(200);
  const fit = await a.eval(`(() => { const m = document.querySelector('.menu'), r = m.getBoundingClientRect();
    return { top: r.top, bottom: r.bottom, h: innerHeight, sh: m.scrollHeight, ch: m.clientHeight }; })()`);
  check(fit.bottom <= fit.h && fit.sh > fit.ch, `the menu fits the window and scrolls: ${JSON.stringify(fit)}`);
  await a.key("Up");
  await a.wait(`['Hide sidebar', 'Hide channels'].includes(document.querySelector('.menu .menu-item.sel .menu-label')?.textContent)`, 1000);
  check(await a.eval(`(() => { const r = document.querySelector('.menu .menu-item.sel').getBoundingClientRect(); return r.bottom <= innerHeight; })()`), "the chosen last item is in view");
  await a.key("Escape");
  // In a narrow window the card stays inside it.
  await a.size(560, 800);
  await a.eval(`document.querySelector(${js(sel("orchestrator"))}).focus()`);
  await a.key("Right");
  await a.wait(`!!document.querySelector('.agent-card')`, 2000);
  check(await a.eval(`document.querySelector('.agent-card').getBoundingClientRect().right <= innerWidth`), "the card fits a narrow window");
  await a.key("Escape");
  await a.size(1400, 900);
  // Double-click renames a stopped agent's channel without starting the agent.
  const rev = await a.eval(`${M}.agents.value.find((x) => x.name === 'reviewer')`);
  await R("session.kill", { id: rev.session_id });
  await a.wait(`(document.querySelector(${js(sel("reviewer"))})?.getAttribute('aria-label') ?? '').includes(', stopped')`, 4000);
  const rr = (await a.rect(sel("reviewer")))!;
  await a.clickAt(rr.x + 40, rr.y + rr.h / 2, { count: 2 });
  await a.wait(`!!document.querySelector(${js(`${row("reviewer")} .channel-rename input`)})`, 2000);
  await a.sleep(500);
  check(await a.eval(`${M}.sessions.value.get(${rev.session_id})?.state === 'exited'`), "the agent stayed stopped");
  await a.key("Escape");
  ok("right-click on an agent's row: one menu, its agent's actions then its channel's; it scrolls in a short window; the card fits a narrow one; double-click renames without starting");

  // ---- the Channels header's ⋯: new agent and schedule, pause all, settings, session communication
  await a.click(".channels-panel .ap-head-more");
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  const head = await menuLabels();
  for (const l of ["New channel…", "New agent…", "New schedule…", "Pause all schedules", "Scheduling settings…", "Session communication…"]) {
    check(head.includes(l), `the header menu has ${l}: ${head}`);
  }
  check(await menuItem("Session communication…"), "Session communication…");
  await a.wait(`!!document.activeElement?.closest('.modal[aria-label="Session communication"]')`, 2000);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.modal')`, 2000);
  ok("the Channels ⋯ has what the Agents header had; Session communication opens as a dialog");

  // ---- Messages follow the open channel's agent
  await a.click(sel("orchestrator"));
  await a.wait(`document.querySelector('.ap-filter')?.textContent.includes('orchestrator')`, 3000);
  await a.click(".ap-filter");
  await a.wait(`!document.querySelector('.ap-filter')`, 2000);
  await a.click(sel("reviewer"));
  await a.wait(`document.querySelector('.ap-filter')?.textContent.includes('reviewer')`, 3000);
  // It was stopped above: the click started it (as Show does) and shows its tab when it is up.
  await a.wait(`!(document.querySelector(${js(sel("reviewer"))})?.getAttribute('aria-label') ?? ', stopped').includes(', stopped')`, 5000);
  await a.sleep(400);
  ok("opening an agent's channel filters Messages to it; × shows all until another channel opens");

  // ---- Dismiss and Approve from Needs you (focus moves to the next item); Hide a failed run
  await R("mock.propose", { agent: "worker", by: "orchestrator", name: "Second idea", reason: "Try it." });
  await a.wait(`document.querySelectorAll('.needs-you .ny-prop').length === 2`, 3000);
  await a.eval(`[...document.querySelectorAll('.needs-you .ny-prop')].find((p) => p.textContent.includes('Second idea')).querySelector('.sch-dismiss').focus()`);
  await a.key("Enter");
  await a.wait(`document.querySelectorAll('.needs-you .ny-prop').length === 1`, 3000);
  await a.wait(`!!document.activeElement?.closest('.needs-you') && document.activeElement.matches('.ny-name')`, 2000);
  await a.click(".needs-you .ny-prop .sch-approve");
  await a.wait(`!document.querySelector('.needs-you .ny-prop')`, 3000);
  check(await a.eval(`document.querySelector(${js(`${row("worker")} .row-chip-bad`)})?.textContent`) === "1", "the red chip now counts the failed run only");
  await a.click(".needs-you .ny-hide");
  await a.wait(`!document.querySelector('.needs-you')`, 2000);
  check(await a.eval(`!!document.querySelector(${js(`${row("worker")} .row-chip-plain`)})`), "the row's chip is back to the plain count");
  ok("Approve took the proposal out of Needs you; Hide took the blocked run out; the box went away");

  // ---- a wider sidebar shows state, time and the schedules' chip
  check(await a.eval(`getComputedStyle(document.querySelector(${js(`${row("reviewer")} .row-state`)})).display`) === "none", "narrow: no state text");
  await a.eval(`document.querySelector('.sidebar-edge').focus()`);
  for (let i = 0; i < 5; i++) await a.key("Right");
  await a.wait(`document.querySelector('.sidebar').classList.contains('wide')`, 2000);
  check(await a.eval(`getComputedStyle(document.querySelector(${js(`${row("reviewer")} .row-state`)})).display`) !== "none", "wide: the state shows");
  check((await a.eval(`document.querySelector('.sidebar').getBoundingClientRect().width`)) >= 330, "the edge widened it");
  await shot("sidebar-wide");
  ok("the sidebar's edge widens it from the keyboard; wide rows show each agent's state");

  const errors = a.console.filter((l) => /^(error|exception)/.test(l));
  if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
  ok("no console errors");
} catch (e) {
  console.error("✗", e instanceof Error ? e.message : e);
  if (shots) await a.shot(`${shots}/zz-sidebar-failure.png`).catch(() => {});
  Deno.exitCode = 1;
} finally {
  await a.close();
}
