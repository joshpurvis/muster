import {
  answerLabel, answerOf, draftAnswers, draftEmpty, draftOptions, draftSig, draftText, isRecommended, missingRequired, pruneChoices, type Question,
  toggleSig, validateQuestions, valueForDigit,
} from "./questions.ts";
import { type CustomAction, validateActions } from "./actions.ts";
import { Problems } from "./validate.ts";
import { check, eq } from "./test_util.ts";

function run(questions: unknown) {
  const p = new Problems();
  const out = validateQuestions(p, { questions });
  return { out, errors: p.errors, warnings: p.warnings };
}
const ok = (questions: unknown): Question[] => {
  const r = run(questions);
  if (r.errors.length) throw new Error(r.errors.join("\n"));
  return r.out;
};

export const sampleQuestions = [
  { id: "limit", text: "Keep the limit at 5 failed logins per 10 minutes?", why: "Support asked for 10 per hour last year.", kind: "yes_no", recommended: "yes", required: true },
  { id: "proxy", text: "Which proxies should be trusted?", kind: "choice", options: [{ label: "Only ours", value: "ours" }, { label: "Any", value: "any" }, { label: "None", value: "none" }], recommended: "ours" },
  { id: "note", text: "Anything to tell the author?", kind: "text" },
];

Deno.test("questions: objects and the old plain strings (kind text, ids by place)", () => {
  const qs = ok([...sampleQuestions, "Why 5?"]);
  eq(qs.map((q) => [q.id, q.kind, q.required]), [["limit", "yes_no", true], ["proxy", "choice", false], ["note", "text", false], ["q4", "text", false]]);
  eq(qs[0].why, "Support asked for 10 per hour last year.");
  eq(ok(["  ", "Real one"]).map((q) => q.id), ["q2"], "blank strings are skipped, ids stay by place");
  eq(ok(undefined), []);
});

Deno.test("questions: every bad field is named", () => {
  const bad = (questions: unknown, want: string) => {
    const r = run(questions);
    check(r.errors.some((e) => e.startsWith(want)), `${want}\n${r.errors.join("\n")}`);
  };
  const q = (patch: Record<string, unknown>) => [{ ...sampleQuestions[1], ...patch }];
  bad([1], "questions[0]: must be a string or an object");
  bad(q({ id: "Bad Id" }), "questions[0].id: must be lowercase");
  bad([sampleQuestions[0], sampleQuestions[0]], `questions[1].id: "limit" is used twice`);
  bad(["x", { ...sampleQuestions[2], id: "q1" }], `questions[1].id: "q1" is used twice`);
  bad(q({ kind: "multi" }), "questions[0].kind: must be one of yes_no, choice, text");
  bad(q({ kind: undefined }), "questions[0].kind: is required");
  bad(q({ text: "x".repeat(501) }), "questions[0].text: is longer than 500");
  bad(["x".repeat(2001)], "questions[0]: is longer than 2000");
  bad(q({ why: "x".repeat(301) }), "questions[0].why: is longer than 300");
  bad(q({ options: [{ label: "A", value: "a" }] }), "questions[0].options: needs at least 2");
  bad(q({ options: undefined }), "questions[0].options: is required");
  bad(q({ options: Array.from({ length: 10 }, (_, i) => ({ label: `L${i}`, value: `v${i}` })) }), "questions[0].options: has 10 items; at most 9");
  bad(q({ options: [{ label: "A", value: "a" }, { label: "B", value: "a" }] }), `questions[0].options[1].value: "a" is used twice`);
  bad(q({ options: [{ label: "x".repeat(81), value: "a" }, { label: "B", value: "b" }] }), "questions[0].options[0].label: is longer than 80");
  bad(q({ recommended: "maybe" }), "questions[0].recommended: must be one of the option values");
  bad([{ ...sampleQuestions[0], recommended: "Yes" }], "questions[0].recommended: must be one of the option values (\"yes\", \"no\")");
  bad([{ ...sampleQuestions[0], options: [{ label: "A", value: "a" }] }], "questions[0].options: a yes_no question has its own Yes and No");
  bad([{ ...sampleQuestions[2], options: [] }], "questions[0].options: a text question takes no options");
  bad([{ ...sampleQuestions[2], recommended: "x".repeat(1001) }], "questions[0].recommended: is longer than 1000");
  bad(q({ required: "yes" }), "questions[0].required: must be true or false");
  bad(Array.from({ length: 21 }, (_, i) => `q${i}`), "questions: has 21 items; at most 20");
  check(run([{ ...sampleQuestions[2], extra: 1 }]).warnings.some((w) => w.startsWith("questions[0].extra")), "unknown fields are warnings");
});

Deno.test("draft: the recommended answer is picked until the user picks; text needs typing", () => {
  const qs = ok(sampleQuestions);
  eq(draftAnswers(qs, {}), { limit: "yes", proxy: "ours" });
  check(isRecommended(qs[0], {}) && !isRecommended(qs[0], { limit: "no" }));
  eq(missingRequired(qs, {}), [], "a required question with a recommendation is answered");
  eq(draftAnswers(qs, { limit: "no", note: "  Thanks!  " }), { limit: "no", proxy: "ours", note: "Thanks!" });
  eq(draftAnswers(qs, { proxy: "gone" }).proxy, "ours", "a pick that is no longer an option falls back to the recommendation");
  const noRec = ok([{ ...sampleQuestions[0], recommended: undefined }]);
  eq(missingRequired(noRec, {}).map((q) => q.id), ["limit"], "a required question without an answer holds Merge");
  eq(missingRequired(noRec, { limit: "no" }), []);
  const suggested = ok([{ ...sampleQuestions[2], recommended: "Nice work" }]);
  eq(answerOf(suggested[0], {}), "Nice work", "a text question's suggestion counts until changed");
  eq(answerOf(suggested[0], { note: "" }), undefined, "cleared by the user: unanswered, not the suggestion");
  eq(answerLabel(qs[1], "ours"), "Only ours");
  eq(answerLabel(qs[0], "no"), "No");
});

Deno.test("draft: number keys pick the options of the focused question; old picks are pruned", () => {
  const qs = ok(sampleQuestions);
  eq([valueForDigit(qs[0], "1"), valueForDigit(qs[0], "2"), valueForDigit(qs[0], "3")], ["yes", "no", undefined]);
  eq([valueForDigit(qs[1], "3"), valueForDigit(qs[1], "0"), valueForDigit(qs[2], "1")], ["none", undefined, undefined]);
  eq(pruneChoices(qs, { limit: "no", proxy: "gone", note: "x", old: "yes" }), { limit: "no", note: "x" });
});

Deno.test("draft: toggles ride along; the text names answers and what each option will do", () => {
  const p = new Problems();
  const actions: CustomAction[] = validateActions(p, { actions: [
    { id: "needs-qa", label: "Needs QA", key: "q", description: "Add the needs-qa label on GitHub.", outward: true },
    { id: "ping", label: "Ping now", description: "Tell the author in chat.", outward: true, mode: "now" },
    { id: "later", label: "Label later", description: "Note it locally.", outward: false, mode: "toggle" },
  ] });
  eq(p.errors, []);
  eq(actions.map((a) => a.mode), ["toggle", "now", "toggle"]);
  eq(draftOptions(actions, new Set(["later", "ping", "needs-qa"])), ["needs-qa", "later"], "only toggles, in the panel's order");
  const qs = ok(sampleQuestions);
  const d = { answers: draftAnswers(qs, { note: "Thanks" }), options: ["needs-qa"] };
  const text = draftText(qs, actions, d);
  check(text === ` Answers: [limit] Yes; [proxy] Only ours; [note] "Thanks". Also do, for this PR: needs-qa (Add the needs-qa label on GitHub.).`, text);
  eq(draftText(qs, actions, { answers: {}, options: [] }), "");
  check(draftEmpty({ answers: {}, options: [] }) && !draftEmpty(d));
  check(draftSig(d) === draftSig({ answers: { note: "Thanks", proxy: "ours", limit: "yes" }, options: ["needs-qa"] }), "order of answers does not matter");
  check(draftSig(d) !== draftSig({ ...d, options: [] }));
  check(toggleSig(actions[0]) !== toggleSig({ ...actions[0], description: "Close the PR." }), "a changed description is a different toggle");
});
