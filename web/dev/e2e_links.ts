// Links checks against the MOCK daemon (docs/UI.md "Links"): a shell prints a note's link as a
// hyperlink (OSC 8: the title only) and as Claude Code and Codex print Markdown links
// (`title (muster://…)`), then turns mouse reporting on. A click opens the closed note beside the
// terminal, a link to a deleted note says so, no click reaches the program, and the clicks make
// no call but the ones that show something. Then a note's own links (`noteLinkChecks`): web and
// Muster addresses open on Ctrl+click, a plain click places the caret.
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

/** Links in a note's text: hovering one says what Ctrl+click opens, Ctrl underlines it, Ctrl+click
 * opens it (a web address through `window.open`, recorded here; a Muster address in Muster), and a
 * plain click only places the caret. The note's font is monospace and its lines don't wrap here,
 * so a character's place is its line and column times the cell size. */
export async function noteLinkChecks(a: Browser, shots: string, ok: (m: string) => void, check: (c: unknown, m: string) => void) {
  const R = (m: string, p: unknown): Promise<any> =>
    a.eval(`(globalThis.__rawCall ?? ${M}.rpc.call.bind(${M}.rpc))(${JSON.stringify(m)}, ${JSON.stringify(p)})`);
  const shot = async (name: string) => {
    await a.sleep(300);
    await a.shot(`${shots}/${name}.png`);
  };
  const target = await R("note.create", { title: "Release checklist", body: "1. tag\n2. notes\n" });
  const body = [
    "Links for the release:",
    "- Guide: https://example.org/guide.",
    "- Board (https://example.org/board?tab=open) and muster://note/" + target.id,
    "- Old: muster://note/99999",
    "Plain text after the links.",
  ].join("\n");
  const note = await R("note.create", { title: "Release links", body });
  const here = await a.eval(`${M}.windowId.value`);
  await R("note.open", { id: note.id, window_id: here });
  const tab = `.note-tab[data-tab-sid="${noteTab(note.id)}"]`;
  await a.wait(`!!document.querySelector('${tab}')`, 4000);
  await a.click(tab);
  const ta = `textarea.note-text[data-note-id="${note.id}"]`;
  await a.wait(`document.querySelector('${ta}')?.value.includes('Plain text after')`, 4000);
  await a.eval(`globalThis.__opened = []; globalThis.open = (...args) => { globalThis.__opened.push(args); return null; }`);

  /** Viewport centre of character `col` (0-based) on line `line` of the note. */
  const at = async (line: number, col: number): Promise<[number, number]> =>
    await a.eval(`(() => {
      const el = document.querySelector('${ta}'); const cs = getComputedStyle(el);
      const m = document.createElement('span'); m.style.whiteSpace = 'pre';
      for (const p of ['fontFamily', 'fontSize', 'fontWeight', 'letterSpacing']) m.style[p] = cs[p];
      m.textContent = '0'.repeat(100); document.body.append(m);
      const w = m.getBoundingClientRect().width / 100, h = parseFloat(cs.lineHeight); m.remove();
      const r = el.getBoundingClientRect();
      return [r.left + el.clientLeft + parseFloat(cs.paddingLeft) + (${col} + 0.5) * w,
        r.top + el.clientTop + parseFloat(cs.paddingTop) + (${line} + 0.5) * h - el.scrollTop];
    })()`);
  const lines = body.split("\n");
  const colOf = (line: number, text: string) => lines[line].indexOf(text);
  const CTRL = 2;
  const move = async (x: number, y: number, modifiers = 0) => {
    await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, modifiers });
    await a.sleep(120);
  };
  const ctrlKey = async (down: boolean) => {
    await a.send("Input.dispatchKeyEvent", {
      type: down ? "rawKeyDown" : "keyUp", key: "Control", code: "ControlLeft", windowsVirtualKeyCode: 17,
      modifiers: down ? CTRL : 0,
    });
    await a.sleep(120);
  };
  const press = async (x: number, y: number, modifiers = 0) => {
    await a.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1, buttons: 1, modifiers });
    await a.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1, modifiers });
    await a.sleep(150);
  };
  const hint = () => a.eval(`document.querySelector('.note-link-hint')?.textContent ?? ''`);
  const lineCount = () => a.eval(`document.querySelectorAll('.note-link-lines > div').length`);
  const opened = (): Promise<unknown[][]> => a.eval(`globalThis.__opened`);
  const sel = (): Promise<[number, number]> => a.eval(`(() => { const el = document.querySelector('${ta}'); return [el.selectionStart, el.selectionEnd]; })()`);

  // ---- hover without Ctrl: the status line says what Ctrl+click opens; nothing is underlined
  const [gx, gy] = await at(1, colOf(1, "example.org/guide"));
  await move(gx - 4, gy);
  await move(gx, gy);
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').includes('https://example.org/guide')`, 2000);
  check((await hint()).startsWith("Ctrl+click to open https://example.org/guide"), `hint: ${await hint()}`);
  check(!(await hint()).includes("guide."), "the full stop after the address isn't part of it");
  check((await lineCount()) === 0, "no underline without Ctrl");
  await shot("note-links-1-hover");
  ok("hovering an address in a note says Ctrl+click opens it (the full stop after it left out)");

  // ---- Ctrl down where the pointer rests: underlined, pointer cursor
  await ctrlKey(true);
  await a.wait(`document.querySelectorAll('.note-link-lines > div').length === 1`, 2000);
  check(await a.eval(`document.querySelector('${ta}').classList.contains('on-link')`), "pointer cursor over the link with Ctrl down");
  check((await hint()).startsWith("Click to open https://example.org/guide"), `hint with Ctrl: ${await hint()}`);
  await shot("note-links-2-ctrl-hover");
  ok("pressing Ctrl over the address underlines it and shows the pointer");

  // ---- Ctrl+click opens it in the browser; the caret stays where it was and the text is unchanged
  await a.eval(`(() => { const el = document.querySelector('${ta}'); el.focus(); el.setSelectionRange(3, 3); })()`);
  await press(gx, gy, CTRL);
  let o = await opened();
  check(o.length === 1 && o[0][0] === "https://example.org/guide" && o[0][1] === "_blank", `opened: ${JSON.stringify(o)}`);
  check(String(o[0][2]).includes("noopener"), "opened without a way back to Muster (noopener)");
  check(JSON.stringify(await sel()) === "[3,3]", `caret kept: ${JSON.stringify(await sel())}`);
  await ctrlKey(false);
  await a.wait(`document.querySelectorAll('.note-link-lines > div').length === 0`, 2000);
  ok("Ctrl+click on a web address opens it in the browser; the caret doesn't move");

  // ---- the full stop after it, with Ctrl: no link
  const [dx, dy] = await at(1, colOf(1, "guide.") + 5);
  await press(dx, dy, CTRL);
  check((await opened()).length === 1, "Ctrl+click on the full stop after an address opens nothing");
  // ---- past the end of the line beside it: no link either
  const [ex, ey] = await at(1, lines[1].length + 6);
  await press(ex, ey, CTRL);
  check((await opened()).length === 1, "Ctrl+click past the line's end opens nothing");
  ok("Ctrl+click beside an address (its full stop, past the line's end) opens nothing");

  // ---- a plain click on the address places the caret in it and opens nothing
  await press(gx, gy);
  const [s0, s1] = await sel();
  const start = body.indexOf("https://example.org/guide");
  check(s0 === s1 && s0 > start && s0 < start + 25, `caret in the address: ${s0},${s1}`);
  check((await opened()).length === 1, "a plain click opens nothing");
  ok("a plain click on an address places the caret in it, as in any text");

  // ---- an address in brackets, with a query: the bracket isn't part of it
  const [bx, by] = await at(2, colOf(2, "example.org/board") + 3);
  await press(bx, by, CTRL);
  o = await opened();
  check(o.length === 2 && o[1][0] === "https://example.org/board?tab=open", `opened: ${JSON.stringify(o)}`);
  ok("Ctrl+click on `(https://…?tab=open)` opens the address without the brackets");

  // ---- a Muster address opens what it names, in Muster (no browser)
  await a.eval(`globalThis.__calls = []`);
  const [mx, my] = await at(2, colOf(2, "muster://note/") + 4);
  await move(mx, my, CTRL);
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').includes('Release checklist')`, 2000);
  await shot("note-links-3-muster-hover");
  await press(mx, my, CTRL);
  const ttab = `.note-tab[data-tab-sid="${noteTab(target.id)}"]`;
  await a.wait(`!!document.querySelector('${ttab}.active')`, 4000);
  check((await opened()).length === 2, "a Muster address doesn't go to the browser");
  ok("Ctrl+click on muster://note/N opens that note in Muster (status line named it by title)");

  // ---- a link to a note that is gone: the short message
  await a.click(tab);
  await a.wait(`!!document.querySelector('${tab}.active')`, 3000);
  const [qx, qy] = await at(3, colOf(3, "muster://note/") + 4);
  await move(qx, qy, CTRL);
  await press(qx, qy, CTRL);
  await a.wait(`[...document.querySelectorAll('.toast')].some((t) => t.textContent.includes('Note 99999 no longer exists'))`, 3000);
  ok("Ctrl+click on a link to a deleted note says it no longer exists");

  const after = await R("note.get", { id: note.id });
  check(after.body === body, "the note's text is unchanged by the clicks");
  ok("the note's text is unchanged");

  // ---- a Ctrl press that turns into a drag, even one back onto the link, opens nothing
  await a.send("Input.dispatchMouseEvent", { type: "mousePressed", x: gx, y: gy, button: "left", clickCount: 1, buttons: 1, modifiers: CTRL });
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: gx + 40, y: gy, buttons: 1, modifiers: CTRL });
  await a.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: gx + 2, y: gy, buttons: 1, modifiers: CTRL });
  await a.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: gx + 2, y: gy, button: "left", clickCount: 1, modifiers: CTRL });
  await a.sleep(150);
  check((await opened()).length === 2, "a Ctrl press dragged away and back opens nothing");
  ok("a Ctrl press that becomes a drag opens nothing");

  // ---- the keyboard: the caret in an address names it in the status line; Ctrl+Enter opens it
  await a.eval(`(() => { const el = document.querySelector('${ta}'); el.focus();
    const at = el.value.indexOf('example.org/board'); el.setSelectionRange(at, at); })()`);
  await a.key("Right");
  await move(10, 10);
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').startsWith('Ctrl+Enter to open https://example.org/board?tab=open')`, 2000);
  await shot("note-links-4-caret");
  await a.key("Ctrl+Enter");
  o = await opened();
  check(o.length === 3 && o[2][0] === "https://example.org/board?tab=open", `Ctrl+Enter opened: ${JSON.stringify(o)}`);
  check((await a.eval(`document.querySelector('${ta}').value`)) === body, "Ctrl+Enter typed no new line");
  check(
    (await a.eval(`document.getElementById(document.querySelector('${ta}').getAttribute('aria-describedby'))?.textContent ?? ''`))
      .includes("Ctrl+Enter or Ctrl+click"),
    "the editor tells screen readers how links open",
  );
  ok("with the caret in an address, the status line names it and Ctrl+Enter opens it");

  // ---- an agent changes the address under a resting pointer with Ctrl down: the status line follows
  await move(gx, gy, CTRL);
  await ctrlKey(true);
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').includes('https://example.org/guide')`, 2000);
  const cur = await R("note.get", { id: note.id });
  await R("note.write", { id: note.id, base_rev: cur.rev, body: body.replace("https://example.org/guide", "https://example.net/other") });
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').includes('https://example.net/other')`, 3000);
  await ctrlKey(false);
  ok("an agent's change under the pointer updates what the status line says opens");

  // ---- a narrow window: the instruction stays whole, the address is cut short
  await a.size(960, 700);
  await a.sleep(400);
  const [nx, ny] = await at(1, "- Guide: https://".length + 2);
  await move(nx - 4, ny);
  await move(nx, ny);
  await a.wait(`(document.querySelector('.note-link-hint')?.textContent ?? '').startsWith('Ctrl+click to open')`, 2000);
  const how = await a.eval(`(() => { const h = document.querySelector('.note-link-hint .how'); return h.scrollWidth <= h.clientWidth + 1 && h.getBoundingClientRect().width > 0; })()`);
  check(how, "the instruction fits in a narrow note");
  check(await a.eval(`document.querySelector('${ta}').clientWidth < 420`), "the note is narrow");
  await shot("note-links-5-narrow");
  await a.size(1440, 900);
  ok("in a narrow note the status line keeps “Ctrl+click to open” whole");
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
    await noteLinkChecks(a, shots, ok, check);
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
