import { actionForKey, BUILTIN_IDS, customEvent, outwardTarget, RESERVED_KEYS, validateActions } from "./actions.ts";
import { Gate } from "./confirm.ts";
import { Problems } from "./validate.ts";
import { check, eq } from "./test_util.ts";

function run(actions: unknown) {
  const p = new Problems();
  const out = validateActions(p, { actions });
  return { out, errors: p.errors, warnings: p.warnings };
}

const qa = { id: "needs-qa", label: "Needs QA", key: "Q", description: "Add the needs-qa label on GitHub.", outward: true };
const later = { id: "later", label: "Label later", description: "Note this PR in your list to label later; nothing on GitHub.", outward: false };

Deno.test("actions: valid ones come back normalised", () => {
  const r = run([qa, later]);
  eq(r.errors, []);
  eq(r.out, [{ ...qa, key: "q", mode: "toggle" }, { ...later, key: undefined, mode: "toggle" }]);
  eq(run(undefined).out, []);
  // Muster refuses an action without `outward`; should one get here, it asks first.
  eq(run([{ id: "x", label: "X", description: "d" }]).out[0].outward, true);
  eq(run([{ id: "x", label: "X", description: "d", outward: null }]).out[0].outward, true);
});

Deno.test("actions: keys never clash with the apps' own keys or each other; ids are unique and not built-in", () => {
  const bad = (list: unknown, want: string) => {
    const r = run(list);
    check(r.errors.some((e) => e.startsWith(want)), `${want}\n${r.errors.join("\n")}`);
  };
  for (const k of RESERVED_KEYS) bad([{ ...later, key: k.toUpperCase() }], `actions[0].key: "${k}" is a built-in key`);
  bad([{ ...later, key: "ctrl+k" }], "actions[0].key: is longer than 1");
  bad([{ ...later, key: "/" }], "actions[0].key: must be one letter or digit");
  bad([{ ...later, key: "F" }, { ...qa, key: "f" }], `actions[1].key: "f" is used twice`);
  bad([later, { ...later }], `actions[1].id: "later" is used twice`);
  for (const id of BUILTIN_IDS) bad([{ ...later, id }], `actions[0].id: "${id}" is a built-in action`);
  bad([{ ...later, id: "Needs QA" }], "actions[0].id: must be lowercase");
  bad([{ ...later, description: " " }], "actions[0].description: must not be empty");
  bad([{ id: "x", label: "X" }], "actions[0].description: is required");
  bad([{ ...later, outward: "yes" }], "actions[0].outward: must be true or false");
  bad([{ ...later, mode: "later" }], "actions[0].mode: must be one of toggle, now");
  eq(run([{ ...later, mode: "now" }]).out[0].mode, "now");
  bad([{ ...later, id: "answers" }], `actions[0].id: "answers" is a built-in action`);
  bad([{ ...later, label: "Send answers" }], "actions[0].label:");
  for (const label of ["Hold", " merge ", "Open on  GitHub…", "Request changes.", "SKIP!", "Next", "Confirm merge"]) {
    bad([{ ...later, label }], "actions[0].label:");
  }
  eq(run([{ ...later, label: "Hold for QA" }]).errors, [], "only an exact look-alike is refused");
  // Lengths are characters (code points), as Muster's backend counts them.
  eq(run([{ ...later, label: "😀".repeat(40), description: "🚀".repeat(300) }]).errors, []);
  bad([{ ...later, label: "😀".repeat(41) }], "actions[0].label: is longer than 40");
  bad(Array.from({ length: 9 }, (_, i) => ({ ...later, id: `a${i}` })), "actions: has 9 items; at most 8");
  check(run([{ ...later, extra: 1 }]).warnings.some((w) => w.startsWith("actions[0].extra")));
});

Deno.test("actions: the custom event names the action and the PR, never a merge", () => {
  const [a] = run([qa]).out;
  const ev = customEvent(a, { repo: "acme/api", number: 812, head_sha: "f".repeat(40) });
  eq(ev.kind, "custom");
  eq(ev.data, { action: "custom", id: "needs-qa", repo: "acme/api", number: 812, head_sha: "f".repeat(40) });
  check(ev.text.includes("Add the needs-qa label on GitHub.") && ev.text.includes("nothing else"), ev.text);
  eq(customEvent(a, { repo: "acme/api", number: 9 }).data, { action: "custom", id: "needs-qa", repo: "acme/api", number: 9 });
  eq(actionForKey([a], "Q")?.id, "needs-qa");
  eq(actionForKey([a], "Enter"), undefined);
});

Deno.test("outward confirm: armed only after the opening key is released; a changed action or PR cancels it", () => {
  const [a] = run([qa]).out;
  const t = { repo: "acme/api", number: 812 };
  const g = new Gate();
  g.keyDown("KeyQ", "q", false);
  g.openTarget(outwardTarget(a, t));
  check(!g.canConfirmTarget(outwardTarget(a, t)), "not armed while Q is held");
  check(g.keyDown("KeyQ", "q", true), "repeat blocked");
  g.keyUp("KeyQ");
  check(g.canConfirmTarget(outwardTarget(a, t)), "armed after release");
  check(!g.inputTarget(outwardTarget(a, t)), "same action and PR keep it");
  check(g.inputTarget(outwardTarget({ ...a, description: "Close the PR." }, t)), "a new description cancels");
  check(!g.isOpen);
  g.openTarget(outwardTarget(a, t));
  check(g.inputTarget(outwardTarget(a, { ...t, number: 813 })), "another PR cancels");
  for (const change of [{ base: "release" }, { status: "merged" }, { head_sha: "e".repeat(40) }]) {
    const t2 = { ...t, base: "master", status: "open" };
    g.openTarget(outwardTarget(a, t2));
    check(g.inputTarget(outwardTarget(a, { ...t2, ...change })), `a changed ${Object.keys(change)[0]} cancels`);
  }
  g.openTarget(outwardTarget(a, t));
  check(g.inputTarget(null), "the action removed (or invalid input) cancels");
});
