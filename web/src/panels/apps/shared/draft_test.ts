import { DraftState } from "./draft.ts";
import { type CustomAction, validateActions } from "./actions.ts";
import { type Question, validateQuestions } from "./questions.ts";
import { Problems } from "./validate.ts";
import { check, eq } from "./test_util.ts";

function actions(list: unknown[]): CustomAction[] {
  const p = new Problems();
  const out = validateActions(p, { actions: list });
  if (p.errors.length) throw new Error(p.errors.join("\n"));
  return out;
}
function questions(list: unknown[]): Question[] {
  const p = new Problems();
  return validateQuestions(p, { questions: list });
}
const qa = { id: "qa", label: "Needs QA", description: "Add the needs-qa label on GitHub.", outward: true };
const later = { id: "later", label: "Label later", description: "Note it locally.", outward: false };
const qs = questions([{ id: "limit", text: "Keep 5?", kind: "yes_no", recommended: "yes" }]);

Deno.test("draft: a refused send puts back what it spent, when nothing happened since", () => {
  const acts = actions([qa, later]);
  const ds = new DraftState();
  ds.input("acme/api#812", qs, acts);
  ds.switchOn(acts[0]);
  const d = ds.draft(qs, acts);
  eq(d, { answers: { limit: "yes" }, options: ["qa"] });
  const rollback = ds.spend(d);
  eq(ds.draft(qs, acts).options, [], "options are one-shot");
  check(ds.sentAnswers !== null);
  check(rollback(acts), "nothing changed: restored");
  eq(ds.draft(qs, acts).options, ["qa"]);
  eq(ds.sentAnswers, null);
});

Deno.test("draft: a late refusal never re-enables an option whose action changed in an update (reproduced)", () => {
  const acts = actions([qa]);
  const ds = new DraftState();
  ds.input("acme/api#812", qs, acts);
  ds.switchOn(acts[0]); // confirmed as "Add the needs-qa label"
  const rollback = ds.spend(ds.draft(qs, acts));
  // The agent changes qa to another outward action; then the first send is refused.
  const changed = actions([{ ...qa, description: "Close the PR on GitHub." }]);
  ds.input("acme/api#812", qs, changed);
  check(!rollback(changed), "the rollback is stale");
  eq(ds.draft(qs, changed).options, [], "the changed action stays off: it was never confirmed");
  // Nor onto another PR.
  const ds2 = new DraftState();
  ds2.input("acme/api#812", qs, acts);
  ds2.switchOn(acts[0]);
  const back2 = ds2.spend(ds2.draft(qs, acts));
  ds2.input("acme/api#813", qs, acts);
  check(!back2(acts));
  eq(ds2.draft(qs, acts).options, [], "never carried onto another PR");
});

Deno.test("draft: a late refusal keeps the user's newer edits (reproduced)", () => {
  const acts = actions([qa, later]);
  const ds = new DraftState();
  ds.input("acme/api#812", qs, acts);
  ds.switchOn(acts[0]);
  const rollback = ds.spend(ds.draft(qs, acts));
  ds.switchOn(acts[1]); // the user goes on: switches on Label later
  check(!rollback(acts), "an edit since makes it stale");
  eq(ds.draft(qs, acts).options, ["later"], "Later stays on; QA is not switched back on");
  // Answers edited since are kept too.
  const ds3 = new DraftState();
  ds3.input("acme/api#812", qs, acts);
  const r3 = ds3.spend(ds3.draft(qs, acts));
  ds3.setChoice("limit", "no");
  check(!r3(acts));
  eq(ds3.draft(qs, acts).answers, { limit: "no" });
  check(ds3.sentAnswers !== null, "the sent mark is not rolled back over a newer edit");
});

Deno.test("draft: options count only while the action is the one switched on", () => {
  const acts = actions([qa, { ...later, mode: "now" }]);
  const ds = new DraftState();
  ds.input("acme/api#812", qs, acts);
  ds.switchOn(acts[0]);
  ds.switchOn(acts[1]);
  eq(ds.draft(qs, acts).options, ["qa"], "a now action is never an option");
  const relabelled = actions([{ ...qa, label: "QA please" }]);
  eq(ds.draft(qs, relabelled).options, [], "draft() checks the saved signature, not just the id");
  ds.input("acme/api#812", qs, relabelled);
  eq(ds.draft(qs, acts).options, [], "an update drops it for good");
  const t = ds.token();
  check(ds.fresh(t));
  ds.touch();
  check(!ds.fresh(t), "invalid input invalidates earlier sends");
});
