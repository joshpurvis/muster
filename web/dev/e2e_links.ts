// Links checks against the MOCK daemon (docs/UI.md "Links"): a shell prints a note's link as a
// hyperlink (OSC 8: the title only) and as Claude Code and Codex print Markdown links
// (`title (muster://…)`), then turns mouse reporting on. A click opens the closed note beside the
// terminal, a link to a deleted note says so, no click reaches the program, and the clicks make
// no call but the ones that show something.
//
//   deno task build:dev && MOCK_PORT=17761 deno task mock &
//   deno run --allow-net=127.0.0.1 --allow-run=chromium --allow-read=<shots> --allow-write=<shots>,/tmp \
//     dev/e2e_links.ts http://127.0.0.1:17761/#token=dev <shots-dir> <repo-path>
import { type Browser, launch } from "./cdp.ts";
import { noteTab } from "../src/layout/tree.ts";

const M = "globalThis.__muster";

export async function linkChecks(a: Browser, shots: string, ok: (m: string) => void, check: (c: unknown, m: string) => void) {
  // The test's own calls go around the recorder installed below.
  const R = (m: string, p: unknown): Promise<any> =>
    a.eval(`(globalThis.__rawCall ?? ${M}.rpc.call.bind(${M}.rpc))(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const shot = async (name: string) => {
    await a.sleep(400);
    await a.shot(`${shots}/${name}.png`);
  };
  const sid: number = await a.eval(`${M}.windowSessions.value.find((s) => s.kind === 'shell')?.id ?? null`);
  check(sid, "a shell tab to print the links");
  // Every call the page makes from here on is recorded.
  await a.eval(`(() => { const r = ${M}.rpc; globalThis.__calls = []; globalThis.__input = 0;
    const call = r.call.bind(r); globalThis.__rawCall = call; r.call = (m, p) => { globalThis.__calls.push(m); return call(m, p); };
    const send = r.sendInput.bind(r); r.sendInput = (sid, d) => { globalThis.__input++; return send(sid, d); }; })()`);
  const note = await R("note.create", { title: "Pairs to review", body: "1. login + signup\n2. billing + receipts\n3. docs + help\n" });
  const tab = `.note-tab[data-tab-sid="${noteTab(note.id)}"]`;
  check(note.window_id === null, "the note starts closed");
  // A program's own hyperlink (OSC 8) to the note, what Claude Code and Codex print for
  // `[Pairs to review](muster://note/N)`, and a link to a note that was deleted. Then mouse
  // reporting goes on (as a TUI's would): a click on a link must not reach the program.
  const cmd = `clear; printf 'Your three pairs are in \\e]8;;muster://note/${note.id}\\e\\\\Pairs to review\\e]8;;\\e\\\\.\\n` +
    `Codex: Pairs to review (muster://note/${note.id})\\nOld: muster://note/99999\\n\\e[?1000h\\e[?1006h'\r`;
  await R("session.input", { id: sid, data: cmd });
  await a.wait(`(${M}.termText(${sid}) ?? '').includes('Old: muster://note/99999')`, 6000);
  await a.wait(`${M}.term(${sid}).modes.mouseTrackingMode !== 'none'`, 3000);
  await a.eval(`globalThis.__calls.length = 0; globalThis.__input = 0`);

  /** Viewport pixel centre of the first `text` after `marker` on the row that holds `marker`. */
  const at = async (marker: string, text: string): Promise<[number, number]> =>
    await a.eval(`(() => {
      const t = ${M}.term(${sid}); const b = t.buffer.active;
      for (let y = 0; y < b.length; y++) {
        const line = b.getLine(y).translateToString(true);
        const m = line.indexOf(${JSON.stringify(marker)});
        if (m < 0) continue;
        const col = line.indexOf(${JSON.stringify(text)}, m) + 2;
        const scr = t.element.querySelector('.xterm-screen').getBoundingClientRect();
        const w = scr.width / t.cols, h = scr.height / t.rows;
        return [scr.x + (col + 0.5) * w, scr.y + (y - b.viewportY + 0.5) * h];
      }
      return null;
    })()`);
  const clickLink = async (marker: string, text: string, name?: string) => {
    const [x, y] = await at(marker, text);
    await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: x - 3, y });
    await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
    await a.sleep(300);
    if (name) await shot(name);
    await a.clickAt(x, y);
  };

  // ---- a hyperlink (OSC 8): the title alone; a plain click opens the closed note beside it
  await clickLink("Your three pairs", "Pairs to review", "links-1-hover-hyperlink");
  await a.wait(`${M}.notes.value.get(${note.id})?.window_id != null && !!document.querySelector('${tab}.active')`, 5000);
  check((await R("note.get", { id: note.id })).beside === sid, "opened beside the terminal that showed the link");
  await a.wait(`document.querySelector('textarea.note-text')?.value.includes('billing + receipts')`, 4000);
  await shot("links-2-note-opened");
  ok("a click on a hyperlinked title (OSC 8) opened the closed note beside the terminal; mouse reporting was on");

  // ---- the text form Claude Code and Codex print: the address in the text is a link too
  await R("note.close", { id: note.id });
  await a.wait(`${M}.notes.value.get(${note.id})?.window_id == null && !document.querySelector('${tab}')`, 4000);
  await clickLink("Codex:", "muster://note/", "links-1b-hover-text-link");
  await a.wait(`${M}.notes.value.get(${note.id})?.window_id != null && !!document.querySelector('${tab}.active')`, 4000);
  ok("a click on the address in `Pairs to review (muster://note/N)` opens the closed note again");

  // ---- a link to a note that is gone: a short message, nothing else. Pressed where the pointer
  // already is, with no move first (a link printed under a pointer that stays put).
  const [ox, oy] = await at("Old:", "muster://note/");
  await a.send("Input.dispatchMouseEvent", { type: "mousePressed", x: ox, y: oy, button: "left", clickCount: 1, buttons: 1 });
  await a.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: ox, y: oy, button: "left", clickCount: 1 });
  await a.wait(`[...document.querySelectorAll('.toast')].some((t) => t.textContent.includes('Note 99999 no longer exists'))`, 3000);
  await shot("links-3-gone");
  ok("a link to a deleted note shows a short message");

  // ---- the clicks made no call but showing the note
  const calls: string[] = await a.eval(`globalThis.__calls`);
  check((await a.eval(`globalThis.__input`)) === 0, "no click reached the program, though it asked for mouse reports");
  // Showing a note: open it (in this window, beside the terminal), save the layout, mark
  // the task read, fit the terminals. Nothing that types, approves, starts or stops.
  const showing = new Set([
    "note.open", "note.set_channel", "window.raise", "window.focused", "note.get",
    "window.set_layout", "channel.mark_read", "session.resize",
  ]);
  check(calls.every((m) => showing.has(m)), `only calls that show something: ${calls.join(", ")}`);
  check(calls.includes("note.open"), `the note was opened: ${calls.join(", ")}`);
  const after = await R("note.get", { id: note.id });
  check(after.body === note.body && after.rev === note.rev, "the note's text is unchanged");
  ok(`the clicks only showed things (${calls.join(", ")})`);
}

if (import.meta.main) {
  const [url, shots, repo] = Deno.args;
  if (!url || !shots || !repo) {
    console.error("usage: e2e_links.ts <url#token=..> <shots-dir> <repo-path>");
    Deno.exit(2);
  }
  let step = 0;
  const ok = (msg: string) => console.log(`✓ ${++step}. ${msg}`);
  const check = (cond: unknown, msg: string) => {
    if (!cond) throw new Error("FAILED: " + msg);
  };
  const a = await launch(9353, "/tmp/muster-cdp-links");
  try {
    await a.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
    await a.size(1440, 900);
    await a.goto(url);
    await a.wait(`!!document.querySelector('.app')`, 10000);
    const hello: any = await a.eval(`${M}.rpc.call("hello", {})`);
    if (!String(hello.version).includes("mock")) throw new Error("link checks run against the mock daemon");
    if (!(await a.eval(`${M}.windowSessions.value.length`))) {
      await a.wait(`!!document.querySelector('.empty-window')`);
      await a.eval(`(() => { const i = document.querySelector('.empty-window input');
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
        set.call(i, ${JSON.stringify(repo)}); i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
      await a.click(".empty-window .launch-btn:nth-child(3)");
      await a.wait(`${M}.windowSessions.value.length === 1 && !!document.querySelector('.pane .xterm')`, 8000);
    }
    await linkChecks(a, shots, ok, check);
    const errors = a.console.filter((l) => /^(error|exception)/.test(l));
    if (errors.length) throw new Error("console errors:\n  " + errors.join("\n  "));
    console.log("link checks passed");
  } catch (e) {
    await a.shot(`${shots}/links-zz-failure.png`).catch(() => {});
    console.error(a.console.slice(-20).join("\n"));
    throw e;
  } finally {
    await a.close();
  }
}
