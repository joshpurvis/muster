// Every select and text field in the dialogs, checked after it renders (docs/UI.md "Form controls").
// Run only against a fresh isolated mock:
//   deno run --config web/deno.json --allow-net=127.0.0.1 --allow-run=chromium \
//     --allow-env=MUSTER_FORM_SHOTS --allow-read=<shots> --allow-write=<shots> \
//     web/dev/e2e_form_controls.ts http://127.0.0.1:<port>/#token=dev
// MUSTER_FORM_SHOTS (optional) saves one picture per dialog. Chromium draws a native select
// fine, so the check asks what WebKitGTK (the desktop window) needs instead: every visible select
// draws its own control (appearance: none), and every field's text box holds one line of its text.
import { launch } from "./cdp.ts";
const url = Deno.args[0];
if (!url || new URL(url).hostname !== "127.0.0.1") throw new Error("Pass an isolated loopback mock URL");
const health = await (await fetch(new URL("/api/health", url))).json();
if (health.version !== "mock") throw new Error("Only the isolated mock daemon is allowed");
const shots = Deno.env.get("MUSTER_FORM_SHOTS");
const profile = await Deno.makeTempDir({ prefix: "muster-form-controls-" });
const b = await launch(9374, profile);
await b.size(1400, 900);
await b.goto(url);
await b.wait(`!!globalThis.__muster?.rpc && !!document.querySelector('.app')`, 15000);
const M = "globalThis.__muster";

// A dialog, never the command palette (also a modal).
const DIALOG = `document.querySelector('.modal[role="dialog"]:not(.switcher)')`;
async function palette(text: string) {
  await b.key("Ctrl+Shift+P");
  await b.wait(`!!document.querySelector('.switcher')`);
  await b.type(text);
  await b.sleep(200);
  await b.key("Enter");
}
async function closeDialog() {
  for (let i = 0; i < 3 && await b.eval(`!!document.querySelector(".modal")`); i++) {
    await b.key("Escape");
    await b.sleep(150);
  }
}
async function editAgent() {
  if (!(await b.eval(`${M}.agentsPanelOpen.value`))) await b.eval(`${M}.agentsPanelOpen.value = true`);
  await b.wait(`!!document.querySelector('.ap-row')`, 5000);
  await b.click(".ap-row", { button: "right" });
  const item = `Array.from(document.querySelectorAll('[role="menuitem"]')).find(e => e.querySelector('.menu-label')?.textContent.trim() === 'Edit…')`;
  await b.wait(`!!${item}`);
  await b.eval(`${item}.click()`);
}

// One entry per dialog with fields. `open` leaves the dialog on screen.
// Claude profiles is not here: the mock has no profiles (dev/account_profiles.ts covers it).
const dialogs: { name: string; open: () => Promise<void> }[] = [
  { name: "Add-on tabs", open: () => palette("Add-on tabs") },
  { name: "New agent", open: () => palette("New agent") },
  { name: "Edit agent", open: editAgent },
  { name: "New schedule", open: () => palette("New schedule") },
  { name: "Scheduling settings", open: () => palette("Scheduling settings") },
  { name: "Cache warning", open: () => palette("Cache warning") },
  { name: "Shell for new terminals", open: () => palette("Shell for new terminals") },
  { name: "New channel", open: () => palette("New channel") },
];

// In the page: every visible control in the dialog that shows text, and what is wrong with it.
const CHECK = `(() => {
  const dlg = ${DIALOG};
  const TEXTY = ["", "text", "search", "url", "email", "number", "password", "tel", "date", "time", "datetime-local"];
  const lineOf = (c) => {
    const n = parseFloat(c.lineHeight);
    if (!Number.isNaN(n)) return n;
    const s = document.createElement("span");
    s.style.cssText = "position:absolute;visibility:hidden;white-space:nowrap;line-height:normal";
    s.style.font = c.font || [c.fontStyle, c.fontWeight, c.fontSize, c.fontFamily].join(" ");
    s.textContent = "Agp";
    document.body.append(s);
    const h = s.getBoundingClientRect().height;
    s.remove();
    return h;
  };
  const out = { checked: 0, bad: [] };
  for (const e of dlg.querySelectorAll("select, input, textarea")) {
    if (e.tagName === "INPUT" && !TEXTY.includes((e.getAttribute("type") ?? "").toLowerCase())) continue;
    const r = e.getBoundingClientRect();
    const c = getComputedStyle(e);
    if (!r.width || !r.height || c.visibility === "hidden") continue;
    // A pill's select is a transparent overlay; the pill (a button) draws its text.
    if (parseFloat(c.opacity) === 0) continue;
    out.checked++;
    const what = e.id || e.name || e.getAttribute("aria-label") ||
      e.closest("label")?.querySelector("span")?.textContent?.trim() || e.tagName.toLowerCase();
    if (e.tagName === "SELECT" && (c.appearance !== "none" || (c.webkitAppearance ?? "none") !== "none")) {
      out.bad.push(what + ": select keeps its native look (appearance " + c.appearance + "); WebKitGTK pads it and cuts its text");
    }
    const box = e.clientHeight - parseFloat(c.paddingTop) - parseFloat(c.paddingBottom);
    const line = lineOf(c);
    // Selects get no slack (their text is laid out in the box); text inputs centre a line that may
    // overhang by a fraction of a pixel unseen.
    if (box + (e.tagName === "SELECT" ? 0.5 : 1) < line) {
      out.bad.push(what + ": text box " + box.toFixed(1) + "px is shorter than its line " + line.toFixed(1) + "px");
    }
  }
  return out;
})()`;

const failures: string[] = [];
let total = 0;
for (const d of dialogs) {
  await closeDialog();
  try {
    await d.open();
    await b.wait(`!!${DIALOG}`, 5000);
  } catch (e) {
    failures.push(`${d.name}: did not open (${(e as Error).message})`);
    continue;
  }
  // Open every "More options" so its fields are checked too.
  await b.eval(`${DIALOG}.querySelectorAll('.dlg-more[aria-expanded="false"]').forEach((m) => m.click())`);
  await b.sleep(250);
  const r: { checked: number; bad: string[] } = await b.eval(CHECK);
  total += r.checked;
  if (r.checked === 0) failures.push(`${d.name}: no fields found (has the dialog changed?)`);
  for (const bad of r.bad) failures.push(`${d.name}: ${bad}`);
  console.log(`${d.name}: ${r.checked} fields, ${r.bad.length} problems`);
  if (shots) {
    await Deno.mkdir(shots, { recursive: true });
    const rect = await b.rect('.modal[role="dialog"]:not(.switcher)');
    if (rect) await b.shot(`${shots}/${d.name.toLowerCase().replaceAll(" ", "-")}.png`, rect);
  }
}
await closeDialog();
await b.close();
await Deno.remove(profile, { recursive: true }).catch(() => {});
console.log(`form controls: ${total} fields in ${dialogs.length} dialogs`);
if (failures.length) {
  console.error(failures.map((f) => "FAIL " + f).join("\n"));
  Deno.exit(1);
}
console.log("form controls: all pass");
