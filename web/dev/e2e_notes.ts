// Notes checks for the mock e2e walkthrough (dev/e2e.ts calls `noteChecks`). Also runs alone
// against the mock, for quick iteration:
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_notes.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path>
import { type Browser, launch } from "./cdp.ts";
import { allPanes, type Layout, noteTab, paneOfSession } from "../src/layout/tree.ts";

export interface NoteCtx {
  shots: string;
  ok: (msg: string) => void;
  check: (cond: unknown, msg: string) => void;
}

const M = "globalThis.__muster";

export async function noteChecks(a: Browser, { shots, ok, check }: NoteCtx) {
  const R = (m: string, p: unknown): Promise<any> => a.eval(`${M}.rpc.call(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const shot = async (name: string) => {
    await a.sleep(300);
    await a.shot(`${shots}/${name}.png`);
  };
  const root = async (): Promise<Layout> => JSON.parse(await a.eval(`JSON.stringify(${M}.layout.value.root)`));
  const body = (id: number): Promise<string> => a.eval(`${M}.notes.value.get(${id})?.body ?? null`);
  const shown = (): Promise<string> => a.eval(`document.querySelector('textarea.note-text')?.value ?? null`);
  const palette = async (cmd: string) => {
    await a.key("Ctrl+Shift+P");
    await a.wait(`!!document.querySelector('.switcher')`);
    await a.type(cmd);
    await a.sleep(150);
    await a.key("Enter");
  };

  // Two groups side by side, terminals in both, so the note can move between them.
  const terms: number[] = await a.eval(`${M}.windowSessions.value.map((s) => s.id)`);
  check(terms.length >= 1, "a terminal to put the note beside");
  if (allPanes(await root()).length < 2) {
    await a.click(".pane .xterm");
    await a.key("Alt+D");
    await a.wait(`document.querySelectorAll('.pane').length >= 2`, 6000);
  }
  // The agent that writes to the note: a Claude tab (one is started when none runs).
  let agent: number | null = await a.eval(
    `[...${M}.sessions.value.values()].find((s) => s.kind === 'claude' && s.state !== 'exited')?.id ?? null`,
  );
  if (agent === null) {
    const ch: number = await a.eval(`${M}.sessions.value.get(${terms[0]}).channel_id`);
    agent = (await R("session.spawn", { channel_id: ch, kind: "claude" })).id as number;
  }

  // ---- New note from the palette: a tab in the focused group, the keyboard in its editor
  const before: number[] = await a.eval(`[...${M}.notes.value.keys()]`);
  await palette("new note");
  await a.wait(`!!document.querySelector('.note-tab.active') && document.activeElement?.matches('textarea.note-text')`, 5000);
  const id: number = await a.eval(`[...${M}.notes.value.keys()].find((k) => !${JSON.stringify(before)}.includes(k))`);
  check(id, "a new note exists");
  const tab = noteTab(id);
  check(paneOfSession(await root(), tab), "its tab is in the layout");
  await a.type("Plan for today\n- fix the login test");
  await a.wait(`${M}.notes.value.get(${id})?.body === "Plan for today\\n- fix the login test"`, 4000);
  await a.wait(`document.querySelector('.note-status .note-state')?.textContent === 'Saved'`, 3000);
  ok("New note (palette) opened an editor tab; typing saved itself");

  // ---- an agent appends while the human is typing: both land, live, no text lost
  await a.type("\n- review the PR");
  // Before the debounced save: the human's typing is unsaved when the agent's append arrives.
  await R("note.append", { id, text: "- agent: CI is green on master", from_session: agent });
  await a.wait(`${M}.notes.value.get(${id})?.body.includes("review the PR") && ${M}.notes.value.get(${id})?.body.includes("CI is green")`, 5000);
  const merged = await body(id);
  check(merged.startsWith("Plan for today\n- fix the login test"), `the start is kept: ${merged}`);
  check((await shown()) === merged, "the editor shows the merged text");
  check(await a.eval(`document.activeElement?.matches('textarea.note-text')`), "the keyboard stayed in the editor");
  ok(`an agent's note.append during unsaved typing merged with it (rev ${await a.eval(`${M}.notes.value.get(${id}).rev`)})`);

  // ---- an agent's append with nothing unsaved shows up live, and the status line names it
  await R("note.append", { id, text: "- agent: deploy at 15:00", from_session: agent });
  await a.wait(`document.querySelector('textarea.note-text')?.value.includes('deploy at 15:00')`, 3000);
  const by: string = await a.eval(`document.querySelector('.note-status .note-by')?.textContent ?? ''`);
  check(by.includes("edited by Claude ("), `provenance shown: ${by}`);
  // A full write from a stale revision is refused (agents re-read).
  const rev: number = await a.eval(`${M}.notes.value.get(${id}).rev`);
  const stale = await R("note.write", { id, body: "clobbered", base_rev: rev - 1, from_session: agent }).then(() => "written", (e: Error) => String(e));
  check(/changed since revision/.test(stale), `a stale write is refused: ${stale}`);
  check(!(await body(id)).includes("clobbered"), "nothing was clobbered");
  ok(`agent appends show live (${by.trim()}); a stale full write is refused`);

  // ---- switching tabs keeps the editor (and its undo); fullscreen has a way back
  const editor = `textarea.note-text[data-note-id="${id}"]`;
  await a.click(editor);
  await a.type(" UNDO-ME");
  const pane = paneOfSession(await root(), tab)!;
  const sibling = pane.tabs.find((t) => t !== tab);
  if (sibling !== undefined) {
    await a.click(`[data-tab-sid="${sibling}"]`);
    await a.wait(`document.querySelector('${editor}')?.closest('.note-view')?.hidden === true`, 2000);
    await a.click(`[data-tab-sid="${tab}"]`);
    await a.wait(`document.querySelector('${editor}')?.closest('.note-view')?.hidden === false`, 2000);
  }
  await a.click(editor);
  await a.key("Ctrl+z");
  await a.wait(`!document.querySelector('${editor}').value.includes('UNDO-ME')`, 2000);
  await a.wait(`!${M}.notes.value.get(${id}).body.includes('UNDO-ME')`, 4000);
  ok("switching away and back kept the editor mounted: Ctrl+Z still undid the typing");

  // ---- Ctrl+Z takes back only the human's typing: an agent's append meanwhile stays
  const value = `document.querySelector('${editor}').value`;
  await a.type(" MINE");
  await R("note.append", { id, text: "- agent: while you typed", from_session: agent });
  await a.wait(`${value}.includes('while you typed')`, 3000);
  await a.key("Ctrl+z");
  await a.wait(`!${value}.includes(' MINE') && ${value}.includes('while you typed')`, 2000);
  await a.wait(`!${M}.notes.value.get(${id}).body.includes(' MINE') && ${M}.notes.value.get(${id}).body.includes('while you typed')`, 4000);
  check(await a.eval(`document.activeElement?.matches('${editor}')`), "the keyboard stayed in the editor");
  await a.key("Ctrl+Shift+z");
  await a.wait(`${value}.includes(' MINE\\n- agent: while you typed')`, 2000);
  await a.key("Ctrl+z");
  await a.wait(`!${value}.includes(' MINE')`, 2000);
  await a.key("Ctrl+y");
  await a.wait(`${value}.includes(' MINE')`, 2000);
  await a.wait(`${M}.notes.value.get(${id}).body.includes(' MINE\\n- agent: while you typed')`, 4000);
  ok("Ctrl+Z undid the typing but kept an agent's append made meanwhile; Ctrl+Shift+Z and Ctrl+Y redid it, saved");
  await a.key("Alt+Shift+Z");
  await a.wait(`${M}.fullscreen.value !== null && !!document.querySelector('.fs-exit')`, 2000);
  await a.click(".fs-exit");
  await a.wait(`${M}.fullscreen.value === null`, 2000);
  ok("a note shown fullscreen has the Exit fullscreen button");

  // ---- typing still on its way when the page goes away is kept, and saved by the next page
  await a.click(editor);
  // Saves fail (as when the daemon can't be reached), so only the kept copy can bring it back.
  await a.eval(`(() => { const r = ${M}.rpc; const call = r.call.bind(r);
    r.call = (m, p) => m === "note.write" ? Promise.reject(new Error("Disconnected")) : call(m, p); })()`);
  await a.type("\n- typed just before a reload");
  await a.wait(`document.querySelector('.note-status .note-state')?.textContent.startsWith('Not saved yet')`, 3000);
  check(!(await body(id)).includes("typed just before a reload"), "the save really failed");
  await a.eval(`location.reload()`);
  await a.wait(`!!document.querySelector('.app') && ${M}.notes.value.size > 0`, 10000);
  await a.eval(`(() => { const r = ${M}.rpc; if (r.__guarded) return; const call = r.call.bind(r);
    r.call = (m, p) => m === "window.show" ? Promise.reject(new Error("e2e: window.show blocked")) : call(m, p);
    r.__guarded = true; })()`);
  await a.wait(`(${M}.notes.value.get(${id})?.body ?? '').includes('typed just before a reload')`, 6000);
  const afterReload = await R("note.get", { id });
  check(afterReload.body.includes("typed just before a reload") && afterReload.body.includes("deploy at 15:00"),
    `the reloaded window saved the typing: ${afterReload.body}`);
  await a.wait(`document.querySelector('${editor}')?.value.includes('typed just before a reload')`, 4000);
  ok("typing not saved before a reload came back and was saved");

  // ---- drag the note's tab into the other group: a note beside a terminal, text kept
  // Typing just before the drag stays undoable after it, though the editor is a new one there.
  const toEnd = `(() => { const t = document.querySelector('${editor}'); t.focus();
    t.setSelectionRange(t.value.length, t.value.length); })()`;
  await a.eval(toEnd);
  await a.type(" BEFORE-DRAG");
  await a.wait(`${M}.notes.value.get(${id}).body.endsWith(' BEFORE-DRAG')`, 4000);
  await a.eval(`document.querySelector('${editor}').__beforeDrag = true`);
  const from = paneOfSession(await root(), tab)!;
  const other = allPanes(await root()).find((p) => p.id !== from.id)!;
  const otherTab = other.tabs.find((t) => t > 0) ?? other.tabs[0];
  const target = await a.eval(`(() => { const t = document.querySelector('[data-tab-sid="${otherTab}"]');
    const r = t.closest('.pane').querySelector('.pane-body').getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
  const r = await a.rect(`[data-tab-sid="${tab}"]`);
  check(r, "the note's tab is on screen");
  await a.drag([r!.x + Math.min(40, r!.w / 2), r!.y + r!.h / 2], [target.x, target.y], 16);
  await a.wait(`(() => { const l = ${M}.layout.value.root; const find = (n) => n.type === 'pane' ? (n.tabs.includes(${tab}) ? n : null) : n.children.map(find).find(Boolean);
    const p = find(l); return !!p && p.id !== ${JSON.stringify(from.id)}; })()`, 4000);
  await a.wait(`document.querySelector('textarea.note-text')?.value.includes('deploy at 15:00')`, 3000);
  ok("dragging the note's tab into another group moved it there with its text");
  await a.click(`[data-tab-sid="${tab}"]`);
  await a.wait(`!!document.querySelector('.note-tab.active.focused')`, 2000);
  check(!(await a.eval(`!!document.querySelector('${editor}').__beforeDrag`)), "the drag remounted the editor");
  await a.eval(`document.querySelector('${editor}').focus()`);
  await a.key("Ctrl+z");
  await a.wait(`!${value}.includes('BEFORE-DRAG')`, 2000);
  const sel = `(() => { const t = document.querySelector('${editor}'); return [t.selectionStart, t.selectionEnd, t.value.length]; })()`;
  const [s0, s1, len] = await a.eval(sel);
  check(s0 === len && s1 === len, `the caret is where the undone typing began: ${s0}-${s1} of ${len}`);
  await a.wait(`!${M}.notes.value.get(${id}).body.includes('BEFORE-DRAG')`, 4000);
  await a.key("Ctrl+Shift+z");
  await a.wait(`${value}.endsWith(' BEFORE-DRAG')`, 2000);
  const [r0, r1, rlen] = await a.eval(sel);
  check(r0 === rlen && r1 === rlen, `redo leaves the caret after the text: ${r0}-${r1} of ${rlen}`);
  await a.wait(`${M}.notes.value.get(${id}).body.endsWith(' BEFORE-DRAG')`, 4000);
  ok("typing from before the drag was undone and redone in the moved (remounted) editor, and saved");
  await shot("notes-1-beside-terminal");

  // ---- close keeps the note; the notes list reopens it
  await a.click(`[data-tab-sid="${tab}"] .tab-close`);
  await a.wait(`!document.querySelector('[data-tab-sid="${tab}"]') && ${M}.notes.value.get(${id})?.window_id === null`, 3000);
  check((await body(id)).includes("deploy at 15:00"), "closing the tab kept the note");
  await R("note.create", { title: "Release checklist", body: "- [ ] changelog\n- [ ] tag v0.4\n" });
  await R("note.create", { title: "Scratch", body: "" });
  await palette("notes reopen");
  await a.wait(`!!document.querySelector('.notes-list .note-row')`, 3000);
  const rows: string = await a.eval(`[...document.querySelectorAll('.notes-list .note-row .sw-label')].map((e) => e.textContent).join('|')`);
  check(rows.includes("Release checklist") && rows.includes("Scratch"), `the list shows every note: ${rows}`);
  await shot("notes-2-list");
  // Find by text, then open.
  await a.type("CI is green");
  await a.wait(`document.querySelectorAll('.notes-list .note-row').length === 1`, 2000);
  await a.key("Enter");
  await a.wait(`!!document.querySelector('[data-tab-sid="${tab}"]') && ${M}.notes.value.get(${id})?.window_id !== null`, 3000);
  await a.wait(`document.querySelector('textarea.note-text')?.value.includes('deploy at 15:00')`, 3000);
  ok("closing a note's tab kept the note; Notes… found it by its text and reopened it");

  // ---- Jump finds a note by its text
  await a.key("Ctrl+Space");
  await a.wait(`!!document.querySelector('.switcher .sw-input')`);
  await a.type("tag v0.4");
  await a.wait(`[...document.querySelectorAll('.sw-item .sw-label')].some((e) => e.textContent === 'Release checklist')`, 2000);
  await a.key("Escape");
  ok("Jump lists notes by their text");

  // ---- deleting asks first, with the app's dialog
  const scratch: number = await a.eval(`[...${M}.notes.value.values()].find((n) => n.title === 'Scratch').id`);
  await palette("notes reopen");
  await a.wait(`!!document.querySelector('.notes-list .note-row')`, 3000);
  await a.type("Scratch");
  await a.wait(`document.querySelectorAll('.notes-list .note-row').length === 1`, 2000);
  await a.key("Shift+Delete");
  await a.wait(`!!document.querySelector('.confirm-dlg') && document.activeElement?.textContent === 'Cancel'`, 2000);
  await a.click(".confirm-dlg .btn-danger");
  await a.wait(`!${M}.notes.value.has(${scratch})`, 3000);
  ok("Shift+Delete in Notes… asked (Cancel focused) and deleted the note");
  await a.wait(`!!document.querySelector('.notes-list')`, 2000);
  await a.key("Escape");
  // Leave the window as it was: the note closed (it stays in Notes…).
  await R("note.close", { id });
  await a.wait(`!document.querySelector('[data-tab-sid="${tab}"]')`, 3000);
}

if (import.meta.main) {
  const [url, shots, repo] = Deno.args;
  if (!url || !shots || !repo) throw new Error("usage: e2e_notes.ts <mock url#token=dev> <shots-dir> <repo-path>");
  let step = 0;
  const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
  const check = (cond: unknown, msg: string) => {
    if (!cond) throw new Error("check failed: " + msg);
  };
  const a = await launch(9352, "/tmp/muster-cdp-notes");
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("note checks run against the mock daemon");
    if (!(await a.eval(`${M}.windowSessions.value.length`))) {
      await a.wait(`!!document.querySelector('.empty-window')`);
      await a.eval(`(() => { const i = document.querySelector('.empty-window input');
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
        set.call(i, ${JSON.stringify(repo)}); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
      await a.click(".empty-window .launch-btn:nth-child(3)");
      await a.wait(`${M}.windowSessions.value.length === 1 && !!document.querySelector('.pane .xterm')`, 8000);
    }
    await noteChecks(a, { shots, ok, check });
    const errors = a.console.filter((l) => /^(error|exception)/.test(l));
    if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
    console.log("note checks passed");
  } catch (e) {
    await a.shot(`${shots}/notes-zz-failure.png`).catch(() => {});
    console.error(a.console.slice(-20).join("\n"));
    throw e;
  } finally {
    await a.close();
  }
}
