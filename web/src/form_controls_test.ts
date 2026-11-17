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
// stylesheet, so it holds for any select a later dialog adds, wrapped or not. A rule that reaches a
// select only through a class (`.foo { height: 20px }` on a <select class="foo">) can't be seen
// here; web/dev/e2e_form_controls.ts checks the selects the dialogs really render.

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
      // The value that wins inside the rule: the last one, unless an earlier one is !important.
      const decls = new Map<string, string>();
      const important = new Set<string>();
      for (const d of text.slice(i + 1, close).split(";")) {
        const k = d.indexOf(":");
        if (k <= 0) continue;
        const prop = d.slice(0, k).trim().toLowerCase();
        let value = d.slice(k + 1).trim().toLowerCase();
        const imp = /!\s*important$/.test(value);
        value = value.replace(/\s*!\s*important$/, "");
        if (important.has(prop) && !imp) continue;
        if (imp) important.add(prop);
        decls.set(prop, value);
      }
      out.push({
        selectors: splitTop(prelude, ",").map((s) => s.trim().replace(/\s+/g, " ")).filter(Boolean),
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

/** `text` split at `sep` outside brackets and parentheses. */
function splitTop(text: string, sep: string): string[] {
  const out: string[] = [];
  let depth = 0, from = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "(" || c === "[") depth++;
    else if (c === ")" || c === "]") depth--;
    else if (depth === 0 && c === sep) {
      out.push(text.slice(from, i));
      from = i + 1;
    }
  }
  out.push(text.slice(from));
  return out;
}

/** The selector's subject (its last compound, outside brackets) names the select element, also
 * through :is()/:where()/:has-free forms such as `.a :is(select)`. `:not(select)` doesn't count. */
function targetsSelect(sel: string): boolean {
  const s = sel.toLowerCase().trim();
  let depth = 0, from = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === "(" || c === "[") depth++;
    else if (c === ")" || c === "]") depth--;
    else if (depth === 0 && /[\s>+~]/.test(c)) from = i + 1;
  }
  const last = s.slice(from).replace(/:not\([^()]*\)/g, "").replace(/\[[^\]]*\]/g, "");
  return /(^|[\s(,>+~*|])select(?![-\w])/.test(last);
}

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
    :is(.a, select) { height: 16px; }
    .a :where(select:focus) { appearance: auto !important; appearance: none; }
    SELECT.big { HEIGHT: 40PX; }
    .a select:not(.b) { color: red; }
    .a :not(select) { color: red; }
    [data-kind="select"] { color: red; }
  `);
  const hits = sample.filter((r) => r.selectors.some(targetsSelect)).map((r) => r.line);
  const want = "[3,4,8,9,10,11]";
  if (JSON.stringify(hits) !== want) throw new Error(`select rules found on lines ${JSON.stringify(hits)}, expected ${want}`);
  if (sample[1].decls.get("-webkit-appearance") !== "menulist") throw new Error("missed a declaration inside @media");
  if (sample[6].decls.get("appearance") !== "auto") throw new Error("a later plain value beat an earlier !important one");
  if (sample[7].decls.get("height") !== "40px") throw new Error("upper-case property or value missed");
});
