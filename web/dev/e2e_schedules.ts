// Schedules checks for the mock e2e walkthrough (dev/e2e.ts calls `scheduleChecks`): the agents
// panel's Schedules sections with their history, the New / Edit schedule dialog, Run now, pause,
// Pause all and Scheduling settings. Also runs alone against the mock, for quick iteration:
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_schedules.ts http://127.0.0.1:17761/#token=dev <shots-dir>
import { type Browser, launch } from "./cdp.ts";

export interface ScheduleCtx {
  shots: string;
  ok: (msg: string) => void;
  check: (cond: unknown, msg: string) => void;
}

const M = "globalThis.__muster";

export async function scheduleChecks(a: Browser, { shots, ok, check }: ScheduleCtx) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const js = JSON.stringify;
  /** A screenshot of one element (with a margin), or of the window. */
  const shot = async (name: string, sel?: string, pad = 0) => {
    await a.sleep(300);
    const r = sel ? await a.rect(sel) : null;
    await a.shot(
      `${shots}/${name}.png`,
      r ? { x: Math.max(0, r.x - pad), y: Math.max(0, r.y - pad), w: r.w + 2 * pad, h: r.h + 2 * pad } : undefined,
    );
  };
  const menuItem = (label: string) =>
    a.eval(`(() => { const it = [...document.querySelectorAll('.menu .menu-item')]
      .find((e) => e.querySelector('.menu-label')?.textContent === ${js(label)});
      if (!it) return false; it.click(); return true; })()`);
  const palette = async (cmd: string) => {
    await a.key("Ctrl+Shift+P");
    await a.wait(`!!document.querySelector('.switcher')`);
    await a.type(cmd);
    await a.sleep(150);
    await a.key("Enter");
  };
  const row = (name: string) => `[...document.querySelectorAll('.sch-row')].find((r) => r.querySelector('.sch-name')?.textContent === ${js(name)})`;
  const rowSel = async (name: string) => `.sch-row[data-schedule="${await a.eval(`${row(name)}?.dataset.schedule`)}"]`;
  const openMenu = async (name: string) => {
    await a.click(`${await rowSel(name)} .sch-more`);
    await a.wait(`!!document.querySelector('.menu')`, 2000);
  };
  const setValue = (sel: string, v: string, ev = "input") =>
    a.eval(`(() => { const i = document.querySelector(${js(sel)});
      const proto = i instanceof HTMLSelectElement ? HTMLSelectElement : i instanceof HTMLTextAreaElement ? HTMLTextAreaElement : HTMLInputElement;
      Object.getOwnPropertyDescriptor(proto.prototype, 'value').set.call(i, ${js(v)});
      i.dispatchEvent(new Event(${js(ev)}, { bubbles: true })); })()`);

  if (!(await a.eval(`${M}.agentsPanelOpen.value`))) await a.key("Alt+G");
  await a.wait(`!!document.querySelector('.agents-panel') && document.querySelectorAll('.sch-row').length >= 3`, 5000);

  // ---- the panel: each agent's schedules under its row
  check(
    await a.eval(`document.querySelector('[data-agent="worker"]').nextElementSibling?.matches('.sch-list')`),
    "the worker's schedules sit right under its row",
  );
  const daily = row("Daily PR summary");
  check(/^next: /.test(await a.eval(`${daily}.querySelector('.sch-next').textContent`)), "the next run shows");
  check(await a.eval(`${daily}.querySelector('.sch-chip').textContent`) === "replied", "the last run's outcome");
  check(await a.eval(`${row("CI check")}.querySelector('.sch-badge')?.textContent`) === "paused", "a paused badge");
  check(await a.eval(`${row("CI check")}.querySelector('.sch-chip.sch-bad')?.textContent`) === "blocked", "a failure in red");
  check(
    /^expires (today|tomorrow)/.test(await a.eval(`${row("Morning digest")}.querySelector('.sch-expires')?.textContent ?? ''`)),
    "a Claude subscription agent's schedule says when it expires, with Renew",
  );
  await shot("agents-panel-schedules", ".agents-panel");
  ok("the agents panel lists each agent's schedules: next run, last outcome, paused badge, expiry");

  // ---- history: click a schedule; failures red, skips grey, a catch-up chip; a run opens its message
  await a.click(await rowSel("Daily PR summary"));
  await a.wait(`document.querySelectorAll('.sch-runs .sch-run').length >= 5`, 3000);
  const lines: string[] = await a.eval(`[...document.querySelectorAll('.sch-runs .sch-run')].map((r) => r.className + '|' + r.textContent)`);
  check(/^\S+ \d\d:\d\d · replied · 4m/.test(lines[0].split("|")[1]), `newest first, "Mon 09:00 · replied · 4m": ${lines[0]}`);
  check(lines.some((l) => l.includes("sch-run-skip") && l.includes("busy")), "a skipped run is grey, with why");
  check(lines.some((l) => l.includes("sch-run-bad") && l.includes("refused")), "a refused run is red, with why");
  check(lines.some((l) => l.includes("catch-up")), "a catch-up run has its chip");
  await shot("schedule-history", ".agents-panel");
  await a.click(`.sch-runs .sch-run.has-msg`);
  await a.wait(`!!document.querySelector('.msg-dlg')`, 2000);
  check(
    (await a.eval(`[...document.querySelectorAll('.msg-dlg .msg-pre')].map((p) => p.textContent).join('|')`)).includes("PRs opened"),
    "the message overlay shows the run's reply",
  );
  await a.key("Escape");
  await a.wait(`!document.querySelector('.msg-dlg')`, 2000);
  await a.click(await rowSel("Daily PR summary"));
  await a.wait(`!document.querySelector('.sch-runs')`, 2000);
  ok("clicking a schedule shows its runs (failures red, skips grey, catch-up chip); a run opens its message and reply");

  // ---- New schedule from an agent's menu preselects that agent
  await a.click(`[data-agent="digest"]`, { button: "right" });
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  check(await menuItem("New schedule…"), "an agent's menu has New schedule…");
  await a.wait(`!!document.querySelector('.sch-dlg .sch-agent')`, 2000);
  check(await a.eval(`document.querySelector('.sch-agent').value`) === "digest", "it preselects that agent");
  await a.wait(`!!document.querySelector('.sch-claude-warn')`, 2000);
  check(
    (await a.eval(`document.querySelector('.sch-claude-warn').textContent`)).includes("7 days"),
    "a Claude subscription agent: the amber warning and the 7-day rule",
  );
  await a.wait(`document.querySelector('.sch-quiet-toggle')?.checked === true`, 2000);
  await shot("new-schedule-claude", ".sch-dlg", 8);
  await a.key("Escape");
  await a.wait(`!document.querySelector('.sch-dlg')`, 2000);
  ok("New schedule… in an agent's menu preselects it; a Claude subscription agent shows the warning, quiet hours on");

  // ---- New schedule from the palette: Codex/OpenRouter first and preselected, Claude without the origin not selectable
  await palette("new schedule");
  await a.wait(`!!document.querySelector('.sch-dlg .sch-agent')`, 3000);
  const opts: string[] = await a.eval(
    `[...document.querySelectorAll('.sch-agent option')].map((o) => o.value + ':' + (o.disabled ? 'off' : 'on'))`,
  );
  check(opts[0] === "worker:on", `the Codex agent comes first: ${opts}`);
  check(opts.indexOf("digest:on") > 0, `then the Claude agent that opted in: ${opts}`);
  check(opts.includes("orchestrator:off") && opts.includes("reviewer:off"), `Claude agents without Schedule can't be picked: ${opts}`);
  check(await a.eval(`document.querySelector('.sch-agent').value`) === "worker", "the Codex agent is preselected");
  check(await a.eval(`!document.querySelector('.sch-claude-warn')`), "no Claude warning for Codex");
  // A cron line faster than every 15 minutes is refused as you type.
  await setValue(".sch-preset select", "custom", "change");
  await a.wait(`!!document.querySelector('.sch-cron')`, 1000);
  await setValue(".sch-cron", "*/5 * * * *");
  await a.wait(`document.querySelector('.sch-preview-err')?.textContent.includes('at most every 15 minutes')`, 3000);
  await setValue(".sch-preset select", "weekdays", "change");
  await setValue(".sch-time", "09:00");
  await a.wait(`document.querySelector('.sch-reading')?.textContent === 'At 09:00, Monday to Friday'`, 3000);
  await a.wait(`document.querySelectorAll('.sch-next-run').length === 3`, 2000);
  await a.click(".sch-prompt");
  await a.type("Check the open issues and post the three oldest.\nKeep it short.");
  await a.sleep(150);
  await shot("new-schedule-dialog", ".sch-dlg", 8);
  // Once at…: a date and time; the reading says when.
  await setValue(".sch-preset select", "once", "change");
  await a.wait(`document.querySelector('.sch-reading')?.textContent.startsWith('Once, ')`, 3000);
  await shot("new-schedule-once", ".sch-dlg", 8);
  await setValue(".sch-preset select", "weekdays", "change");
  await a.wait(`document.querySelector('.sch-reading')?.textContent === 'At 09:00, Monday to Friday'`, 3000);
  await a.eval(`document.querySelector('.sch-more-toggle').scrollIntoView({ block: 'center' })`);
  await a.click(".sch-more-toggle");
  await a.wait(`!!document.querySelector('.sch-tz')`, 1000);
  check((await a.eval(`document.querySelector('.sch-tz').value`)).length > 0, "the time zone defaults to this browser's");
  await a.eval(`document.querySelector('.sch-dlg .dlg-body').scrollTop = 1e6`);
  await shot("new-schedule-dialog-options", ".sch-dlg", 8);
  const before: number = await a.eval(`${M}.schedules.value.length`);
  await a.click(".sch-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-dlg') && ${M}.schedules.value.length === ${before + 1}`, 3000);
  const made: any = await a.eval(`${M}.schedules.value.find((s) => s.name === 'Check the open issues and post the three oldest.')`);
  check(made && made.agent === "worker" && made.when.cron === "0 9 * * 1-5", `created as typed: ${JSON.stringify(made?.when)}`);
  await a.wait(`!!${row(made.name)}`, 2000);
  ok("New schedule: Codex first and preselected, Claude without Schedule disabled; Weekdays 09:00 reads and previews; created");

  // ---- Run now from the menu: a run in the history, then its reply opens in the message overlay
  await openMenu(made.name);
  check(await menuItem("Run now"), "Run now in the schedule's menu");
  await a.wait(`!!document.querySelector('.sch-runs[aria-label=${js(`Runs of ${made.name}`)}] .sch-run')`, 3000);
  await a.wait(`${row(made.name)}.querySelector('.sch-chip')?.textContent === 'replied'`, 4000);
  const runSel = `.sch-runs[aria-label="Runs of ${made.name}"] .sch-run.has-msg`;
  await a.wait(`document.querySelector(${js(runSel)})?.textContent.includes('run now')`, 2000);
  await a.click(runSel);
  await a.wait(`!!document.querySelector('.msg-dlg')`, 2000);
  check(
    (await a.eval(`document.querySelector('.msg-dlg').textContent`)).includes("Done:"),
    "the run's reply shows in the message overlay",
  );
  await a.key("Escape");
  await a.wait(`!document.querySelector('.msg-dlg')`, 2000);
  ok("Run now added a run to the history (marked run now); clicking it showed the reply");

  // ---- Pause / Resume toggles the badge
  await openMenu(made.name);
  check(await menuItem("Pause"), "Pause in the menu");
  await a.wait(`${row(made.name)}.querySelector('.sch-badge')?.textContent === 'paused'`, 2000);
  await openMenu(made.name);
  check(await menuItem("Resume"), "Resume in the menu");
  await a.wait(`!${row(made.name)}.querySelector('.sch-badge') && /^next: /.test(${row(made.name)}.querySelector('.sch-next')?.textContent)`, 2000);
  // Skip next run, then Don't skip.
  await openMenu(made.name);
  check(await menuItem("Skip next run"), "Skip next run in the menu");
  await a.wait(`${row(made.name)}.querySelector('.sch-next')?.textContent.startsWith('skips next: ')`, 2000);
  await openMenu(made.name);
  check(await menuItem("Don't skip next run"), "Don't skip in the menu");
  await a.wait(`${row(made.name)}.querySelector('.sch-next')?.textContent.startsWith('next: ')`, 2000);
  ok("Pause / Resume toggled the paused badge; Skip next run showed what it skips, and undid");

  // ---- Pause all: a banner with Resume; the palette offers the opposite
  await a.click(".ap-head-more");
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  check(await menuItem("Pause all schedules"), "Pause all in the panel header's menu");
  await a.wait(`!!document.querySelector('.sch-paused-note')`, 2000);
  check((await R("schedule.settings.get", {})).paused === true, "the backend has it");
  check(await a.eval(`!!document.querySelector('.sch-next.held')`), "next runs show as held");
  await shot("agents-panel-paused", ".agents-panel");
  await a.click(".sch-paused-note .sch-link");
  await a.wait(`!document.querySelector('.sch-paused-note')`, 2000);
  await palette("pause all schedules");
  await a.wait(`!!document.querySelector('.sch-paused-note')`, 2000);
  await palette("resume all schedules");
  await a.wait(`!document.querySelector('.sch-paused-note')`, 2000);
  ok("Pause all showed the banner (Resume in it works); the palette pauses and resumes all");

  // ---- Scheduling settings
  await palette("scheduling settings");
  await a.wait(`!!document.querySelector('.sch-settings-dlg')`, 2000);
  check(
    await a.eval(`[document.querySelector('.sch-quiet-from').value, document.querySelector('.sch-quiet-to').value, document.querySelector('.sch-unattended').value].join()`) ===
      "23:00,07:00,24",
    "quiet hours and the unattended cap",
  );
  await shot("scheduling-settings", ".sch-settings-dlg", 8);
  await setValue(".sch-unattended", "30");
  await a.click(".sch-settings-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-settings-dlg')`, 2000);
  check((await R("schedule.settings.get", {})).unattended_cap === 30, "saved");
  await R("schedule.settings.set", { unattended_cap: 24 });
  ok("Scheduling settings shows quiet hours and the unattended cap, and saves a change");

  // ---- Edit maps the stored cron back to its preset; Remove asks first
  await openMenu(made.name);
  check(await menuItem("Edit…"), "Edit… in the menu");
  await a.wait(`!!document.querySelector('.sch-dlg')`, 2000);
  check(await a.eval(`document.querySelector('.sch-preset select').value`) === "weekdays", "Weekdays again");
  check(await a.eval(`document.querySelector('.sch-agent-fixed')?.textContent.includes('worker')`), "the agent is fixed");
  await setValue(".sch-time", "10:30");
  await a.wait(`document.querySelector('.sch-reading')?.textContent === 'At 10:30, Monday to Friday'`, 3000);
  await a.click(".sch-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-dlg')`, 2000);
  check(
    (await R("schedule.list", {})).schedules.find((s: any) => s.id === made.id).when.cron === "30 10 * * 1-5",
    "Save sent the new time",
  );
  await openMenu(made.name);
  check(await menuItem("Remove…"), "Remove… in the menu");
  await a.wait(`!!document.querySelector('.confirm-dlg')`, 2000);
  await a.click(".confirm-dlg .btn-danger");
  await a.wait(`!${row(made.name)}`, 2000);
  ok("Edit showed Weekdays and saved a new time; Remove… asked, then removed it");
}

if (import.meta.main) {
  const [url, shots] = Deno.args;
  const a = await launch(9345, "/tmp/muster-cdp-sched");
  let step = 0;
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("runs against the mock daemon only");
    await scheduleChecks(a, {
      shots,
      ok: (m) => console.log(`✓ ${++step}. ${m}`),
      check: (c, m) => {
        if (!c) throw new Error("check failed: " + m);
      },
    });
    const errors = a.console.filter((l) => /^(error|exception)/.test(l));
    if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
    console.log(`✓ ${++step}. no console errors`);
  } catch (e) {
    console.error("✗", e instanceof Error ? e.message : e);
    await a.shot(`${shots}/zz-sched-failure.png`).catch(() => {});
    Deno.exitCode = 1;
  } finally {
    await a.close();
  }
}
