import { askEvent, checksBlock, mergeBlocked, mergeCommand, mergeEvent, type Pr, requestChangesEvent, validatePr } from "./logic.ts";
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
  bad({ status: "done" }, "status: must be one of open, merging, merged, failed, requested_changes");
  bad({ title: undefined }, "title: is required");
  check(validatePr({ ...samplePr, extra: 1 }).ok, "unknown fields are warnings");
});

Deno.test("merge command is exactly the documented one", () => {
  eq(mergeCommand(812, "squash", true, sha), `gh pr merge 812 --squash --delete-branch --match-head-commit ${sha}`);
  eq(mergeCommand(5, "rebase", false, sha), `gh pr merge 5 --rebase --match-head-commit ${sha}`);
  for (const args of [[0, "squash", false, sha], [5, "squash --admin", false, sha], [5, "merge", false, "abc"]] as const) {
    let threw = false;
    try { mergeCommand(args[0], args[1] as "squash", args[2], args[3]); } catch { threw = true; }
    check(threw, JSON.stringify(args));
  }
});

Deno.test("events carry exactly what the panel showed", () => {
  const pr = valid(samplePr);
  const m = mergeEvent(pr, "merge", false);
  eq(m.kind, "merge");
  eq(m.data, { action: "merge", number: 812, method: "merge", delete_branch: false, head_sha: sha });
  check(m.text.endsWith(`gh pr merge 812 --merge --match-head-commit ${sha}`));
  eq(requestChangesEvent(pr, "Add a test").data, { action: "request_changes", number: 812, head_sha: sha, body: "Add a test" });
  eq(askEvent(pr, "Why?").data, { action: "ask", number: 812, question: "Why?" });
});

Deno.test("failing checks block Merge unless Shift; status and pending sends block always", () => {
  const ok = valid(samplePr);
  eq(mergeBlocked(ok, false, false), null);
  const failing = valid({ ...samplePr, checks: { state: "passing", items: [{ name: "t", state: "fail" }] } });
  check(checksBlock(failing));
  check(mergeBlocked(failing, false, false)!.includes("Shift"));
  eq(mergeBlocked(failing, true, false), null);
  check(mergeBlocked(ok, true, true)!.includes("Waiting"));
  check(mergeBlocked(valid({ ...samplePr, status: "merging" }), true, false)!.includes("Waiting"));
  check(mergeBlocked(valid({ ...samplePr, status: "merged" }), true, false)!.includes("merged"));
  eq(mergeBlocked(valid({ ...samplePr, status: "failed" }), false, false), null);
});
