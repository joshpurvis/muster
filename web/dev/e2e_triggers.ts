// Scheduling Phase 2 checks for the mock e2e walkthrough (dev/e2e_schedules.ts calls
// `triggerChecks` after its own): the schedule dialog's "When something happens" with the event
// picker, a note trigger and the note's "Allow triggers" switch (in the dialog, the note's status
// line and its tab), "Then…" steps and a chain run in the history, a GitHub trigger whose gh isn't
// signed in, a URL trigger's menu, and today's OpenRouter spend in Scheduling settings.
import type { Browser } from "./cdp.ts";

export interface TriggerCtx {
  shots: string;
  ok: (msg: string) => void;
  check: (cond: unknown, msg: string) => void;
}

const M = "globalThis.__muster";

export async function triggerChecks(a: Browser, { shots, ok, check }: TriggerCtx) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const js = JSON.stringify;
  const shot = async (name: string, sel?: string, pad = 0) => {
    await a.sleep(300);
    const r = sel ? await a.rect(sel) : null;
    await a.shot(
      `${shots}/${name}.png`,
      r ? { x: Math.max(0, r.x - pad), y: Math.max(0, r.y - pad), w: r.w + 2 * pad, h: r.h + 2 * pad } : undefined,
    );
  };
  const palette = async (cmd: string) => {
    await a.key("Ctrl+Shift+P");
    await a.wait(`!!document.querySelector('.switcher')`);
    await a.type(cmd);
    await a.sleep(150);
    await a.key("Enter");
  };
  const setValue = (sel: string, v: string, ev = "input") =>
    a.eval(`(() => { const i = document.querySelector(${js(sel)});
      const proto = i instanceof HTMLSelectElement ? HTMLSelectElement : i instanceof HTMLTextAreaElement ? HTMLTextAreaElement : HTMLInputElement;
      Object.getOwnPropertyDescriptor(proto.prototype, 'value').set.call(i, ${js(v)});
      i.dispatchEvent(new Event(${js(ev)}, { bubbles: true })); })()`);
  const row = (name: string) =>
    `[...document.querySelectorAll('.sch-row')].find((r) => r.querySelector('.sch-name')?.textContent === ${js(name)})`;
  const rowSel = async (name: string) => `.sch-row[data-schedule="${await a.eval(`${row(name)}?.dataset.schedule`)}"]`;
  const menuItem = (label: string) =>
    a.eval(`(() => { const it = [...document.querySelectorAll('.menu .menu-item')]
      .find((e) => e.querySelector('.menu-label')?.textContent === ${js(label)});
      if (!it) return false; it.click(); return true; })()`);
  const menuLabels = (): Promise<string[]> => a.eval(`[...document.querySelectorAll('.menu .menu-label')].map((e) => e.textContent)`);

  // An agent for the "Then…" step (a Codex agent takes triggered work by default).
  await R("agent.create", { name: "triage", kind: "codex", persona: "You look into failures and file notes." });

  // ---- A note, and its "Allow triggers" switch in the status line: off by default
  await palette("new note");
  await a.wait(`!!document.querySelector('.note-tab.active') && document.activeElement?.matches('textarea.note-text')`, 5000);
  const noteId: number = await a.eval(`Number(document.querySelector('textarea.note-text').dataset.noteId)`);
  await a.type("Ideas\n- a calmer cold-cache bar");
  await R("note.rename", { id: noteId, name: "Ideas" });
  await a.wait(`document.querySelector('.note-status .note-trig')?.getAttribute('aria-checked') === 'false'`, 3000);
  check((await a.eval(`document.querySelector('.note-trig').textContent`)).includes("triggers off"), "the switch reads off");
  // An agent can't switch it (the backend refuses a call on an agent session's behalf).
  const agentSid: number | null = await a.eval(
    `[...${M}.sessions.value.values()].find((s) => (s.kind === 'claude' || s.kind === 'codex') && s.state !== 'exited')?.id ?? null`,
  );
  if (agentSid !== null) {
    const refused: string = await a.eval(
      `${M}.rpc.call("note.set_triggers", { id: ${noteId}, on: true, from_session: ${agentSid} }).then(() => "allowed", (e) => String(e))`,
    );
    check(refused.includes("only the human"), `an agent's session can't allow triggers: ${refused}`);
  }
  ok("a note's status line has an 'Allow triggers' switch, off by default; an agent's session can't switch it");

  // ---- New schedule → When something happens → A note changes
  await palette("new schedule");
  await a.wait(`!!document.querySelector('.sch-dlg .sch-agent')`, 3000);
  await a.click(`.sch-dlg .sch-seg [data-value="event"]`);
  await setValue(".sch-agent", "worker", "change");
  await a.wait(`!!document.querySelector('.sch-trig-kind')`, 2000);
  const kinds: string[] = await a.eval(`[...document.querySelectorAll('.sch-trig-kind option')].map((o) => o.textContent)`);
  check(kinds.length === 5 && kinds[0] === "A note changes", `the event picker: ${kinds}`);
  const opts: string[] = await a.eval(`[...document.querySelectorAll('.sch-agent option')].map((o) => o.value + ':' + (o.disabled ? 'off' : 'on'))`);
  check(opts.includes("worker:on") && opts.includes("digest:off"), `agents need the webhook origin: ${opts}`);
  check(await a.eval(`!document.querySelector('.sch-dlg [aria-label="If missed (Muster was off or asleep)"]')`), "no If missed for a trigger");
  await setValue(".sch-trig-note", String(noteId), "change");
  await a.wait(`document.querySelector('.sch-reading')?.textContent.includes('“Ideas” changes (after 2 minutes of quiet)')`, 3000);
  await a.wait(`!!document.querySelector('.sch-trig-off')`, 2000);
  await a.click(".sch-prompt");
  await a.type("Expand the new items in the Ideas note into a short plan each.");
  await shot("trigger-dialog-note-off", ".sch-dlg", 8);
  // Allow triggers right from the dialog (the human's click).
  await a.click(".sch-trig-off .sch-link");
  await a.wait(`!document.querySelector('.sch-trig-off')`, 3000);
  check((await R("note.get", { id: noteId })).triggers === true, "the note now allows triggers");
  // Then…: ask triage on a reply; notify on failure.
  await a.eval(`document.querySelector('.sch-then').scrollIntoView({ block: 'center' })`);
  await setValue(`.sch-then select[aria-label="Agent to ask when it replies"]`, "triage", "change");
  await a.wait(`!!document.querySelector('.sch-then-ok')`, 1000);
  await setValue(".sch-then-ok", "Here is the plan: {reply} Turn the first item into a task list.");
  await a.click(`.sch-then .sch-seg [data-value="notify"]`);
  await a.wait(`document.querySelector('.sch-then .field-hint')?.textContent.includes('desktop notification')`, 1000);
  await shot("trigger-dialog-then", ".sch-dlg", 8);
  const before: number = await a.eval(`${M}.schedules.value.length`);
  await a.click(".sch-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-dlg') && ${M}.schedules.value.length === ${before + 1}`, 3000);
  const made: any = await a.eval(`${M}.schedules.value.find((s) => s.name.startsWith('Expand the new items'))`);
  check(made.trigger && made.when.kind === "note" && made.when.note_id === noteId && made.daily_cap === 10, `a note trigger: ${js(made.when)}`);
  check(made.then?.on_success?.agent === "triage" && made.then.on_failure?.notify, `its steps: ${js(made.then)}`);
  await a.wait(`!!${row(made.name)}?.querySelector('.sch-icon-trig')`, 2000);
  check(await a.eval(`${row(made.name)}.querySelector('.sch-next').textContent`) === "waiting", "the row waits for a change");
  ok("New schedule → When something happens → A note changes: the note's switch is offered, Then… steps saved");

  // ---- The note's switch now reads on (with the trigger count) and its tab has the marker
  await a.wait(`document.querySelector('.note-trig')?.getAttribute('aria-checked') === 'true'`, 3000);
  check((await a.eval(`document.querySelector('.note-trig').textContent`)).includes("triggers on · 1"), "on, 1 trigger watches it");
  check(await a.eval(`!!document.querySelector('.note-tab .tab-trig')`), "the note's tab shows the trigger marker");
  // An edit by you holds an event: the row says when it runs (after 2 quiet minutes).
  await R("note.append", { id: noteId, text: "- a quieter status line" });
  await a.wait(`${row(made.name)}.querySelector('.sch-next').textContent.startsWith('runs: ')`, 3000);
  await shot("note-triggers-on", ".note-view", 0);
  // Off again from the status line: the held event goes.
  await a.click(".note-trig");
  await a.wait(`document.querySelector('.note-trig')?.getAttribute('aria-checked') === 'false'`, 3000);
  await a.wait(`${row(made.name)}.querySelector('.sch-next').textContent === 'triggers off for its note'`, 3000);
  await a.click(".note-trig");
  await a.wait(`document.querySelector('.note-trig')?.getAttribute('aria-checked') === 'true'`, 3000);
  ok("the note's switch reads on with its trigger count, its tab is marked; an edit shows when the run comes; off drops it");

  // ---- Run now: the run, then its "Then…" step on triage, indented under it
  await a.click(await rowSel(made.name));
  await a.wait(`!!document.querySelector('.sch-runs[aria-label=${js(`Runs of ${made.name}`)}]')`, 2000);
  await R("schedule.run", { id: made.id });
  await a.wait(`!!document.querySelector('.sch-run.sch-run-step')`, 6000);
  await a.wait(`document.querySelector('.sch-run.sch-run-step')?.textContent.includes('then: triage')`, 4000);
  await shot("trigger-history-chain", ".agents-panel");
  await a.click(await rowSel(made.name));
  await a.wait(`!document.querySelector('.sch-runs')`, 2000);
  ok("Run now on a trigger: the run, and its Then… step on triage, shown under it");

  // ---- GitHub: a PR opened; the dialog reads it, and a watcher whose gh isn't signed in says so
  await palette("new schedule");
  await a.wait(`!!document.querySelector('.sch-dlg .sch-agent')`, 3000);
  await a.click(`.sch-dlg .sch-seg [data-value="event"]`);
  await setValue(".sch-agent", "worker", "change");
  await a.wait(`!!document.querySelector('.sch-trig-kind')`, 2000);
  await setValue(".sch-trig-kind", "pr_opened", "change");
  await a.wait(`!!document.querySelector('.sch-trig-gh')`, 1000);
  await setValue(".sch-trig-gh", "octocat/hello-world");
  await a.wait(`document.querySelector('.sch-reading')?.textContent === 'When a pull request is opened on octocat/hello-world (checked every 5 minutes)'`, 3000);
  await a.click(".sch-prompt");
  await a.type("Review the new pull request and post a summary.");
  await shot("trigger-dialog-github", ".sch-dlg", 8);
  await a.click(".sch-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-dlg')`, 3000);
  const gh: any = await a.eval(`${M}.schedules.value.find((s) => s.name.startsWith('Review the new pull request'))`);
  await R("schedule.mock_watch", { id: gh.id, state: "gh_not_signed_in", detail: "gh isn't signed in to GitHub: run `gh auth login`" });
  await a.wait(`${row(gh.name)}?.querySelector('.sch-watch')?.textContent === 'gh not signed in'`, 3000);
  // A URL trigger: its menu copies the command (with the secret); Skip next isn't offered.
  const url: any = await R("schedule.create", { agent: "worker", prompt: "The deploy finished: check the site.", when: { kind: "url", name: "deploy-done" } });
  await a.wait(`!!${row(url.name)}`, 2000);
  await a.eval(`${row(url.name)}.scrollIntoView({ block: 'center' })`);
  await a.click(`${await rowSel(url.name)} .sch-more`);
  await a.wait(`!!document.querySelector('.menu')`, 2000);
  const labels = await menuLabels();
  check(labels.includes("Copy trigger command") && labels.includes("New secret…") && !labels.includes("Skip next run"), `the URL trigger's menu: ${labels}`);
  check(await menuItem("Copy trigger command"), "Copy trigger command");
  await a.wait(`[...document.querySelectorAll('.toast')].some((t) => t.textContent.includes('keep it private') || t.textContent.includes('muster trigger --url deploy-done'))`, 3000);
  await shot("agents-panel-triggers", ".agents-panel");
  ok("a GitHub trigger reads in the dialog and its row says gh isn't signed in; a URL trigger's menu copies its command");

  // ---- Scheduling settings: today's OpenRouter spend and the caps
  await R("mock.openrouter", { key_present: true });
  await R("agent.create", { name: "cheap", kind: "claude", provider: "openrouter", model: "anthropic/claude-sonnet-5.5", accepts: ["human", "schedule", "webhook"] });
  await palette("scheduling settings");
  await a.wait(`!!document.querySelector('.sch-settings-dlg .sch-spend')`, 3000);
  check((await a.eval(`document.querySelector('.sch-spend-today').textContent`)).includes("$1.84 today of $5.00"), "today's spend of the cap");
  await a.wait(`!!document.querySelector('.sch-spend-agent')`, 3000);
  await setValue(`.sch-spend-agent input`, "0.50");
  await a.eval(`document.querySelector('.sch-settings-dlg .dlg-body').scrollTop = 1e6`);
  await shot("scheduling-settings-spend", ".sch-settings-dlg", 8);
  await a.click(".sch-settings-dlg button[type=submit]");
  await a.wait(`!document.querySelector('.sch-settings-dlg')`, 2000);
  const sp = (await R("schedule.settings.get", {})).spend;
  check(sp.agents.find((x: any) => x.name === "cheap")?.cap === 0.5, `the per-agent cap saved: ${js(sp.agents)}`);
  await R("mock.openrouter", { key_present: false });
  ok("Scheduling settings shows today's OpenRouter spend against the $5 cap and saves a per-agent cap");

  for (const s of [made, gh, url]) await R("schedule.remove", { id: s.id });
  await R("note.delete", { id: noteId });
  await a.wait(`!document.querySelector('.note-tab')`, 3000);
}
