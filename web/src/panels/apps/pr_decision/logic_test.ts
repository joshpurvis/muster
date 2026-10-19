import {
  askEvent, checksBlock, ConfirmGate, eventBytes, formsClosed, MAX_EVENT_BYTES, mergeBlocked, mergeCommand, mergeEvent, type Pr,
  requestChangesEvent, validatePr,
} from "./logic.ts";
import { check, eq, errorsOf } from "../shared/test_util.ts";

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
  eq([full.questions, full.answer, full.status], [["Why 5?"], { question: "q", text: "a" }, "merged"]);
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
  eq(m.data, { action: "merge", repo: "acme/api", number: 812, base: "master", method: "merge", delete_branch: false, head_sha: sha });
  check(m.text.endsWith(`gh pr merge 812 --repo acme/api --merge --match-head-commit ${sha}`));
  eq(requestChangesEvent(pr, "Add a test").data, { action: "request_changes", repo: "acme/api", number: 812, head_sha: sha, body: "Add a test" });
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
