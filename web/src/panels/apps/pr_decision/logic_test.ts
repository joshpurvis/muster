import {
  answersEvent, askEvent, checksBlock, ConfirmGate, eventBytes, formsClosed, holdEvent, MAX_EVENT_BYTES, mergeBlocked, mergeCommand, mergeEvent, type Pr,
  joinAnd, readiness, requestChangesEvent, skipEvent, validatePr,
} from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";
import { missingRequired } from "../shared/questions.ts";

const sha = "0123456789abcdef0123456789abcdef01234567";
export const samplePr = {
  repo: "acme/api", number: 812, title: "Rate-limit login", author: "dana", url: "https://github.com/acme/api/pull/812",
  base: "master", head: "dana/login-limit", head_sha: sha, merge_method_default: "squash", delete_branch_default: true,
  summary: "Adds a per-IP limit to /login.", risk: { level: "medium", reasons: ["Touches auth"] },
  checks: { state: "passing", items: [{ name: "test", state: "pass", url: "https://github.com/acme/api/actions/runs/1" }] },
  changes: [{ file: "src/login.rs", note: "The limit", diff: "@@ -1 +1 @@\n-a\n+b" }],
};

function valid(data: unknown): Pr {
  const r = validatePr(data);
  if (!r.ok) throw new Error(r.errors.join("\n"));
  return r.value;
}

Deno.test("pr input: defaults and optional parts", () => {
  const pr = valid({ ...samplePr, merge_method_default: undefined, delete_branch_default: undefined });
  eq([pr.merge_method_default, pr.delete_branch_default, pr.status, pr.questions], ["squash", false, "open", []]);
  const full = valid({ ...samplePr, reviews: { decision: "APPROVED", items: [{ author: "lee", state: "APPROVED" }] }, questions: ["Why 5?", " "], answer: { question: "q", text: "a" }, status: "merged", status_detail: "Merged as abc" });
  eq([full.questions, full.answer, full.status], [[{ id: "q1", text: "Why 5?", kind: "text", options: [], required: false }], { question: "q", text: "a" }, "merged"]);
});

Deno.test("pr input: rejects what would make the merge command unsafe or wrong", () => {
  const bad = (patch: Record<string, unknown>, want: string) => {
    const e = errorsOf(validatePr({ ...samplePr, ...patch }));
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  };
  bad({ head_sha: "0123456" }, "head_sha: must be the full");
  bad({ head_sha: sha.toUpperCase() }, "head_sha: must be the full");
  bad({ head_sha: `${sha}; rm -rf /` }, "head_sha");
  bad({ head_sha: sha + "0".repeat(24) }, "head_sha");
  bad({ number: 8.5 }, "number: must be a positive whole number");
  bad({ number: "812" }, "number: must be a finite number");
  bad({ merge_method_default: "fast-forward" }, "merge_method_default: must be one of squash, merge, rebase");
  bad({ url: "javascript:alert(1)" }, "url: must be an https URL");
  bad({ repo: "acme" }, "repo: must look like owner/name");
  bad({ head: "x y" }, "head: is not a valid branch name");
  bad({ risk: { level: "extreme" } }, "risk.level: must be one of low, medium, high");
  bad({ checks: { state: "green" } }, "checks.state: must be one of passing, failing, pending, none");
  bad({ checks: { state: "passing", items: [{ name: "t", state: "SUCCESS" }] } }, "checks.items[0].state");
  bad({ changes: [{ file: "a", note: "n" }] }, "changes[0].diff: is required");
  bad({ status: "done" }, "status: must be one of open, merging, queued, merged, failed, requested_changes");
  bad({ title: undefined }, "title: is required");
  check(validatePr({ ...samplePr, extra: 1 }).ok, "unknown fields are warnings");
});

Deno.test("merge command is exactly the documented one, pinned to the repo", () => {
  eq(mergeCommand("acme/api", 812, "squash", true, sha), `gh pr merge 812 --repo acme/api --squash --delete-branch --match-head-commit ${sha}`);
  eq(mergeCommand("acme/api", 5, "rebase", false, sha), `gh pr merge 5 --repo acme/api --rebase --match-head-commit ${sha}`);
  for (const args of [["acme/api", 0, "squash", sha], ["acme/api", 5, "squash --admin", sha], ["acme/api", 5, "merge", "abc"],
    ["acme/api", 5, "merge", sha + "0".repeat(24)], ["acme", 5, "merge", sha], ["acme/api --admin", 5, "merge", sha]] as const) {
    let threw = false;
    try { mergeCommand(args[0], args[1], args[2] as "squash", false, args[3]); } catch { threw = true; }
    check(threw, JSON.stringify(args));
  }
});

Deno.test("events carry exactly what the panel showed: repo, base and head", () => {
  const pr = valid(samplePr);
  const m = mergeEvent(pr, "merge", false);
  eq(m.kind, "merge");
  eq(m.data, { action: "merge", repo: "acme/api", number: 812, base: "master", method: "merge", delete_branch: false, head_sha: sha, answers: {}, options: [] });
  check(m.text.endsWith(`gh pr merge 812 --repo acme/api --merge --match-head-commit ${sha}`));
  eq(requestChangesEvent(pr, "Add a test").data, { action: "request_changes", repo: "acme/api", number: 812, head_sha: sha, body: "Add a test", answers: {}, options: [] });
  eq(askEvent(pr, "Why?").data, { action: "ask", repo: "acme/api", number: 812, question: "Why?" });
});

Deno.test("comment size: the whole serialized event must fit the backend limit", () => {
  const pr = valid(samplePr);
  const ok = requestChangesEvent(pr, "x".repeat(7000));
  check(eventBytes(ok) <= MAX_EVENT_BYTES, String(eventBytes(ok)));
  // 8000 two-byte characters, counted twice (text and data): too big however it is typed.
  const big = requestChangesEvent(pr, "é".repeat(4000));
  check(eventBytes(big) > MAX_EVENT_BYTES, String(eventBytes(big)));
  check(eventBytes(askEvent(pr, "\u0001".repeat(1500))) > 1500 * 12, "control characters count as their JSON escapes");
  check(MAX_EVENT_BYTES < 16 * 1024);
});

Deno.test("checks badge comes from the items when there are any", () => {
  const r = validatePr({ ...samplePr, checks: { state: "passing", items: [{ name: "t", state: "fail" }, { name: "u", state: "pass" }] } });
  check(r.ok && r.value.checks.state === "failing" && r.warnings.some((w) => w.includes("says passing")), JSON.stringify(r));
  const p = valid({ ...samplePr, checks: { state: "failing", items: [{ name: "t", state: "pending" }, { name: "u", state: "skipping" }] } });
  eq(p.checks.state, "pending");
  eq(valid({ ...samplePr, checks: { state: "none", items: [] } }).checks.state, "none");
});

Deno.test("forms close for merged, queued and requested_changes", () => {
  eq(formsClosed(valid(samplePr)), null);
  eq(formsClosed(valid({ ...samplePr, status: "failed" })), null);
  for (const status of ["merged", "queued", "requested_changes"]) check(formsClosed(valid({ ...samplePr, status })), status);
  check(mergeBlocked(valid({ ...samplePr, status: "queued" }), true, false)!.includes("Queued"));
});

Deno.test("confirm gate: a held or repeated key never confirms; it arms after release", () => {
  const pr = valid(samplePr);
  const g = new ConfirmGate();
  // Enter on the focused Merge button opens the confirm while Enter is still down.
  g.keyDown("Enter", "Enter", false);
  g.open(pr);
  check(!g.canConfirm(pr), "not armed while the opening key is held");
  check(g.keyDown("Enter", "Enter", true), "auto-repeat is blocked while open");
  check(!g.canConfirm(pr), "still not armed");
  g.keyUp("Enter");
  check(g.canConfirm(pr), "armed after release");
  // Shift+M: the M keyup may report "m"; codes match, and a held Shift never blocks arming.
  const s = new ConfirmGate();
  s.keyDown("ShiftLeft", "Shift", false);
  s.keyDown("KeyM", "M", false);
  s.open(pr);
  check(!s.canConfirm(pr));
  s.keyUp("KeyM");
  check(s.canConfirm(pr), "armed with Shift still held");
  // Opened by a mouse click: nothing held, armed at once.
  const m = new ConfirmGate();
  m.open(pr);
  check(m.canConfirm(pr));
  check(!m.keyDown("Enter", "Enter", false), "a fresh press is not blocked");
  // Focus lost mid-press: keyups may never come.
  const b = new ConfirmGate();
  b.keyDown("KeyM", "m", false);
  b.open(pr);
  b.blur();
  check(b.canConfirm(pr));
});

Deno.test("confirm gate: invalid input, or a new head, base, repo, number, checks or default method, cancels it", () => {
  const pr = valid(samplePr);
  // Reproduced: confirm A, an invalid update, then valid data for head B.
  const g = new ConfirmGate();
  g.open(pr);
  check(g.input(null), "invalid input cancels");
  const b = valid({ ...samplePr, head_sha: "f".repeat(40) });
  check(!g.isOpen && !g.canConfirm(b), "B is never confirmed by A's step");
  // Reproduced: same head, base retargeted.
  for (const patch of [{ base: "production" }, { head_sha: "f".repeat(40) }, { repo: "acme/web" }, { number: 813 },
    { checks: { state: "failing", items: [{ name: "t", state: "fail" }] } }, { merge_method_default: "rebase" }]) {
    const c = new ConfirmGate();
    c.open(pr);
    const next = valid({ ...samplePr, ...patch });
    check(c.input(next) && !c.canConfirm(next), JSON.stringify(patch));
  }
  // Unrelated updates (an answer, a status note) keep it.
  const k = new ConfirmGate();
  k.open(pr);
  const same = valid({ ...samplePr, answer: { question: "q", text: "a" } });
  check(!k.input(same) && k.canConfirm(same));
});

Deno.test("failing checks block Merge unless Shift; status and pending sends block always", () => {
  const ok = valid(samplePr);
  eq(mergeBlocked(ok, false, false), null);
  const failing = valid({ ...samplePr, checks: { state: "passing", items: [{ name: "t", state: "fail" }] } });
  check(checksBlock(failing));
  check(mergeBlocked(failing, false, false)!.includes("Shift+M"));
  eq(mergeBlocked(failing, true, false), null);
  check(mergeBlocked(ok, true, true)!.includes("Waiting"));
  check(mergeBlocked(valid({ ...samplePr, status: "merging" }), true, false)!.includes("Waiting"));
  check(mergeBlocked(valid({ ...samplePr, status: "merged" }), true, false)!.includes("merged"));
  eq(mergeBlocked(valid({ ...samplePr, status: "failed" }), false, false), null);
});

const PNG_URI = "data:image/png;base64,iVBORw0KGgo=";
const chartSpec = { type: "bar", x: { values: ["src", "tests"] }, series: [{ name: "lines", values: [120, 64] }] };

Deno.test("media: pictures (as Muster read them) and charts, each checked by its own app's rules", () => {
  const pr = valid({ ...samplePr, media: [
    { kind: "image", path: "shots/login.png", src: PNG_URI, mime: "image/png", bytes: 8, caption: "After" },
    { kind: "chart", spec: chartSpec, caption: "Lines changed per area" },
  ] });
  eq(pr.media.length, 2);
  check(pr.media[0].kind === "image" && pr.media[0].image.src === PNG_URI && pr.media[0].image.alt === "After", JSON.stringify(pr.media[0]));
  check(pr.media[1].kind === "chart" && pr.media[1].spec.series[0].name === "lines" && pr.media[1].caption === "Lines changed per area");
  eq(valid(samplePr).media, []);
  const bad = (media: unknown, want: string) => {
    const e = errorsOf(validatePr({ ...samplePr, media }));
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  };
  bad([{ kind: "video" }], "media[0].kind: must be one of image, chart");
  bad([{ path: "a.png" }], "media[0].kind: is required");
  bad([{ kind: "image", path: "a.png" }], "media[0].path: the image file was not loaded");
  bad([{ kind: "image", url: "https://x.example/a.png" }], "media[0].url: pictures come from a path only");
  bad([{ kind: "image" }], "media[0]: needs path");
  bad([{ kind: "chart" }], "media[0].spec: is required");
  bad([{ kind: "chart", spec: { ...chartSpec, type: "pie" } }], "media[0].spec.type: must be one of bar");
  bad(Array.from({ length: 9 }, () => ({ kind: "chart", spec: chartSpec })), "media: has 9 items; at most 8");
  bad([1, 2].map(() => ({ kind: "image", path: "a.png", src: PNG_URI, bytes: 4 * 1024 * 1024 })), "media: images total");
  bad([{ kind: "chart", spec: chartSpec, extra: 1 }], "media[0].extra: unknown field");
  bad([{ kind: "image", path: "a.png", src: PNG_URI, extra: 1 }], "media[0].extra: unknown field");
});

Deno.test("skip and hold are one event each, about the PR shown", () => {
  const pr = valid(samplePr);
  eq(skipEvent(pr).kind, "skip");
  eq(skipEvent(pr).data, { action: "skip", repo: "acme/api", number: 812, head_sha: sha, answers: {}, options: [] });
  eq(holdEvent(pr).kind, "hold");
  eq(holdEvent(pr).data, { action: "hold", repo: "acme/api", number: 812, head_sha: sha, answers: {}, options: [] });
  check(holdEvent(pr).text.includes("come back to later"));
});

Deno.test("actions: the user's own buttons are validated with the PR", () => {
  const pr = valid({ ...samplePr, actions: [{ id: "needs-qa", label: "Needs QA", key: "q", description: "Add the needs-qa label on GitHub.", outward: true }] });
  eq(valid({ ...samplePr, actions: [{ id: "x", label: "X", description: "d" }] }).actions[0].outward, true, "a missing outward asks first");
  eq(pr.actions, [{ id: "needs-qa", label: "Needs QA", key: "q", description: "Add the needs-qa label on GitHub.", outward: true, mode: "toggle" }]);
  const e = errorsOf(validatePr({ ...samplePr, actions: [{ id: "x", label: "X", key: "m", description: "d" }] }));
  check(e.some((x) => x.startsWith("actions[0].key: \"m\" is a built-in key")), e.join("\n"));
});

Deno.test("verdict and undo: one plain line each, shown first", () => {
  const pr = valid({ ...samplePr, verdict: "Safe to merge: small bug fix, tests pass.", undo: "If this goes wrong: revert the merge commit." });
  eq([pr.verdict, pr.undo], ["Safe to merge: small bug fix, tests pass.", "If this goes wrong: revert the merge commit."]);
  eq([valid(samplePr).verdict, valid({ ...samplePr, verdict: " " }).verdict], [undefined, undefined]);
  for (const [patch, want] of [[{ verdict: "x".repeat(301) }, "verdict: is longer than 300"], [{ undo: "a\nb" }, "undo: must be one line"],
    [{ verdict: 5 }, "verdict: must be a string"]] as const) {
    const e = errorsOf(validatePr({ ...samplePr, ...patch }));
    check(e.some((x) => x.startsWith(want)), `${want}\n${e.join("\n")}`);
  }
});

const draftPr = () => valid({ ...samplePr, questions: [
  { id: "limit", text: "Keep 5 per 10 minutes?", kind: "yes_no", recommended: "yes" },
  { id: "note", text: "Anything for the author?", kind: "text" },
], actions: [{ id: "needs-qa", label: "Needs QA", key: "q", description: "Add the needs-qa label on GitHub.", outward: true }] });
const d = { answers: { limit: "no", note: "Use 10" }, options: ["needs-qa"] };

Deno.test("the draft rides along with Merge, Request changes, Hold and Skip; Send answers sends it alone", () => {
  const pr = draftPr();
  const m = mergeEvent(pr, "squash", true, d);
  eq(m.data, { action: "merge", repo: "acme/api", number: 812, base: "master", method: "squash", delete_branch: true, head_sha: sha, answers: d.answers, options: d.options });
  check(m.text.includes(`Answers: [limit] No; [note] "Use 10". Also do, for this PR: needs-qa (Add the needs-qa label on GitHub.).`), m.text);
  check(m.text.endsWith(`gh pr merge 812 --repo acme/api --squash --delete-branch --match-head-commit ${sha}`), "the command stays last");
  const r = requestChangesEvent(pr, "Please add a test", d);
  eq([r.data.answers, r.data.options, r.data.body], [d.answers, d.options, "Please add a test"]);
  check(r.text.endsWith("The review comment:\nPlease add a test"), r.text);
  for (const ev of [holdEvent(pr, d), skipEvent(pr, d)]) eq([ev.data.answers, ev.data.options, ev.data.head_sha], [d.answers, d.options, sha]);
  const a = answersEvent(pr, d);
  eq(a.kind, "answers");
  eq(a.data, { action: "answers", repo: "acme/api", number: 812, head_sha: sha, answers: d.answers, options: d.options });
  check(a.text.includes("nothing is decided yet"), a.text);
  eq(askEvent(pr, "Why?").data, { action: "ask", repo: "acme/api", number: 812, question: "Why?" }, "Ask stays a free-form question");
  // One message per send: everything must fit the event limit together.
  const long = { answers: Object.fromEntries(Array.from({ length: 12 }, (_, i) => [`t${i}`, "é".repeat(1000)])), options: [] };
  check(eventBytes(mergeEvent(pr, "squash", true, long)) > MAX_EVENT_BYTES, "too many long answers are refused before sending");
});

Deno.test("confirm gate: a changed answer or option while confirming cancels it", () => {
  const pr = draftPr();
  const g = new ConfirmGate();
  g.open(pr, d);
  check(g.canConfirm(pr, d));
  check(!g.canConfirm(pr, { ...d, options: [] }), "a confirm never sends a draft it did not show");
  check(g.input(pr, { ...d, answers: { ...d.answers, limit: "yes" } }) && !g.isOpen, "an answer changed: cancelled");
  g.open(pr, d);
  check(!g.input(pr, { answers: { note: "Use 10", limit: "no" }, options: ["needs-qa"] }), "the same draft keeps it");
});

Deno.test("confirm gate: a new required question, or any change to the questions, cancels it (reproduced)", () => {
  // Confirm with no questions, then the same PR gains an unanswered required question: the
  // draft is empty both times, yet the confirm must not survive.
  const before = valid(samplePr);
  const g = new ConfirmGate();
  g.open(before);
  const after = valid({ ...samplePr, questions: [{ id: "owner", text: "Who looks after this code now?", kind: "text", required: true }] });
  check(g.input(after) && !g.canConfirm(after), "cancelled");
  eq(missingRequired(after.questions, {}).map((q) => q.id), ["owner"], "and Merge then waits for the answer");
  const q = (patch: Record<string, unknown>) => valid({ ...samplePr, questions: [{ id: "limit", text: "Keep 5?", kind: "yes_no", recommended: "yes", ...patch }] });
  for (const patch of [{ required: true }, { kind: "text", recommended: undefined }, { text: "Keep 10?" }, { recommended: "no" }]) {
    const c = new ConfirmGate();
    const base = q({});
    c.open(base, { answers: { limit: "yes" }, options: [] });
    check(c.input(q(patch), { answers: { limit: "yes" }, options: [] }), JSON.stringify(patch));
  }
  // An option's action changing under the same id cancels it too.
  const acts = [{ id: "qa", label: "Needs QA", description: "Add the label.", outward: true }];
  const a1 = valid({ ...samplePr, actions: acts });
  const a2 = valid({ ...samplePr, actions: [{ ...acts[0], description: "Close the PR." }] });
  const c = new ConfirmGate();
  c.open(a1, { answers: {}, options: ["qa"] });
  check(c.input(a2, { answers: {}, options: ["qa"] }));
});

Deno.test("merge readiness: Muster's own check colours the verdict and says why Merge waits", () => {
  const base = valid({ ...samplePr, checks: { state: "passing", items: [] } });
  eq(readiness(base, {}), { ready: true, tone: "good", reasons: [], short: [] });
  const q = valid({ ...samplePr, checks: { state: "passing", items: [] }, reviews: { decision: "REVIEW_REQUIRED", items: [] }, questions: [
    { id: "a", text: "A?", kind: "yes_no" }, { id: "b", text: "B?", kind: "yes_no", required: true }] });
  const r = readiness(q, {});
  eq([r.ready, r.tone, r.reasons, r.short], [false, "warn",
    ["answer question 2", "an approving review is still required on GitHub"], ["question 2", "an approving review"]]);
  eq(readiness(q, { b: "yes" }).reasons, ["an approving review is still required on GitHub"]);
  const failing = valid({ ...samplePr, checks: { state: "failing", items: [] }, reviews: { decision: "CHANGES_REQUESTED", items: [] } });
  eq([readiness(failing, {}).tone, readiness(failing, {}).short], ["bad", ["passing checks", "the requested changes"]]);
  eq(readiness(valid({ ...samplePr, checks: { state: "pending", items: [] } }), {}).reasons, ["checks are still running"]);
  eq(readiness(valid({ ...samplePr, status: "merged", checks: { state: "failing", items: [] } }), {}).ready, true);
  eq([joinAnd([]), joinAnd(["a"]), joinAnd(["a", "b"]), joinAnd(["a", "b", "c"])], ["", "a", "a and b", "a, b and c"]);
});
