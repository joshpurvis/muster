// Dropdowns in the desktop window must never cut their text off (docs/UI.md "Form controls").
//
// The desktop window is WebKitGTK. It paints a select that keeps its native look with GTK
// theme padding that CSS can neither see nor size, and it forces that select's line-height to
// "normal". Put such a select in a fixed-height field and the bottom half of its text is cut off.
// Headless Chromium (our e2e and screenshot runs) draws the same select fine, so nothing caught
// it: Claude profiles (Oct 2026), the agent dialog's dropdowns (11 Nov) and Settings > Add-on
// tabs (17 Nov) each shipped it, and each fix covered only the dialog it was found in.
//
// The rule now lives once, in styles.css: every select draws its own closed control
// (appearance: none, our chevron) and no rule gives a select a fixed height. This test reads the
// stylesheet, so it holds for any select a later dialog adds, wrapped or not.

const css = await Deno.readTextFile(new URL("./styles.css", import.meta.url));

interface Rule {
  selectors: string[];
  decls: Map<string, string>;
  line: number;
}

/** Style rules (also those inside @media/@supports), comments removed. Enough for our own sheet. */
function rules(src: string): Rule[] {
  const text = src.replace(/\/\*[\s\S]*?\*\//g, (c) => c.replace(/[^\n]/g, " "));
  const out: Rule[] = [];
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (ch === "{") {
      const prelude = text.slice(start, i).trim();
      const close = text.indexOf("}", i);
      const nextOpen = text.indexOf("{", i + 1);
      if (prelude.startsWith("@") || (nextOpen !== -1 && nextOpen < close)) {
        start = i + 1; // an at-rule block: its rules follow
        continue;
      }
      const decls = new Map<string, string>();
      for (const d of text.slice(i + 1, close).split(";")) {
        const k = d.indexOf(":");
        if (k > 0) decls.set(d.slice(0, k).trim().toLowerCase(), d.slice(k + 1).trim().toLowerCase());
      }
      out.push({
        selectors: prelude.split(",").map((s) => s.trim().replace(/\s+/g, " ")).filter(Boolean),
        decls,
        line: text.slice(0, i).split("\n").length,
      });
      i = close;
      start = close + 1;
    } else if (ch === "}") {
      start = i + 1;
    }
  }
  return out;
}

/** The selector's subject (its last compound) is a select element. */
const targetsSelect = (sel: string) => /^select(?=$|[.:#[])/.test(sel.split(/\s*[>+~]\s*|\s+/).at(-1)!);

const all = rules(css);
const selectRules = all.filter((r) => r.selectors.some(targetsSelect));

Deno.test("form controls: the stylesheet parses into rules", () => {
  if (all.length < 500) throw new Error(`only ${all.length} rules parsed from styles.css; the parser is broken`);
  if (selectRules.length < 3) throw new Error(`only ${selectRules.length} rules target a select; the parser is broken`);
});

Deno.test("form controls: every select draws its own closed control (no native WebKitGTK select)", () => {
  const base = all.find((r) => r.selectors.includes("select") && r.decls.get("appearance") === "none");
  if (!base) {
    throw new Error(
      "styles.css has no plain `select { appearance: none }` rule. Without it a select keeps WebKitGTK's native " +
        "look, whose theme padding cuts its text off in the desktop window. Add it to the shared form-control rules.",
    );
  }
  if (base.decls.get("-webkit-appearance") !== "none") {
    throw new Error(`styles.css:${base.line}: the base select rule needs -webkit-appearance: none as well`);
  }
  for (const r of selectRules) {
    for (const p of ["appearance", "-webkit-appearance"]) {
      const v = r.decls.get(p);
      if (v !== undefined && v !== "none") {
        throw new Error(
          `styles.css:${r.line} (${r.selectors.join(", ")}): ${p}: ${v} gives a select back WebKitGTK's native look, ` +
            "which cuts its text off. Keep appearance: none.",
        );
      }
    }
  }
});

Deno.test("form controls: no rule gives a select a fixed height", () => {
  for (const r of selectRules) {
    const h = r.decls.get("height");
    if (h === undefined || h === "auto" || h.endsWith("%")) continue;
    throw new Error(
      `styles.css:${r.line} (${r.selectors.join(", ")}): height: ${h} on a select. A select's text box must be ` +
        "free to be as tall as its line; use min-height instead (and keep it at least the line plus padding).",
    );
  }
});

Deno.test("form controls: the select chevron is defined in both themes", () => {
  const themed = (sel: string) => all.find((r) => r.selectors.includes(sel))?.decls.get("--select-chevron");
  for (const sel of [':root[data-theme="dark"]', ':root[data-theme="light"]']) {
    if (!themed(sel)?.startsWith("url(")) throw new Error(`${sel} needs --select-chevron (the select's arrow)`);
  }
});

Deno.test("form controls: the parser sees the cases it must", () => {
  const sample = rules(`
    /* select { appearance: auto } */
    .a select, .b input { height: 32px; }
    @media (max-width: 10px) { .c > select:focus { -webkit-appearance: menulist; } }
    .pill .pill-native { height: 100%; }
    .select-wrap { display: block; }
    .x select-wrap { color: red; }
  `);
  const hits = sample.filter((r) => r.selectors.some(targetsSelect)).map((r) => r.line);
  if (JSON.stringify(hits) !== "[3,4]") throw new Error(`select rules found on lines ${JSON.stringify(hits)}, expected [3,4]`);
  if (sample[1].decls.get("-webkit-appearance") !== "menulist") throw new Error("missed a declaration inside @media");
});
