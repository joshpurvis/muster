import { diffStats, parseDiff } from "./diff.ts";
import { eq } from "./test_util.ts";

Deno.test("parseDiff numbers lines and skips the gh pr diff preamble", () => {
  const lines = parseDiff([
    "diff --git a/src/a.rs b/src/a.rs",
    "index 1111111..2222222 100644",
    "--- a/src/a.rs",
    "+++ b/src/a.rs",
    "@@ -10,3 +10,4 @@ fn main() {",
    " keep",
    "-old",
    "+new",
    "+added",
    "",
    "\\ No newline at end of file",
  ].join("\n"));
  eq(lines.map((l) => [l.kind, l.text, l.old ?? null, l.new ?? null]), [
    ["hunk", "@@ -10,3 +10,4 @@ fn main() {", null, null],
    ["ctx", "keep", 10, 10],
    ["del", "old", 11, null],
    ["add", "new", null, 11],
    ["add", "added", null, 12],
    ["ctx", "", 12, 13],
    ["note", "No newline at end of file", null, null],
  ]);
  eq(diffStats(lines), { added: 2, removed: 1 });
});

Deno.test("parseDiff keeps -- and ++ content lines inside a hunk and handles CRLF and several hunks", () => {
  const lines = parseDiff("@@ -1 +1 @@\r\n--- not a header\r\n+++ also content\r\n@@ -20,2 +20,2 @@\n a\n-b\n+c\n");
  eq(lines.map((l) => l.kind), ["hunk", "del", "add", "hunk", "ctx", "del", "add"]);
  eq(lines[1].text, "-- not a header");
  eq([lines[4].old, lines[6].new], [20, 21]);
});

Deno.test("parseDiff shows a bare hunk body without numbers", () => {
  const lines = parseDiff("+x\n-y\n z");
  eq(lines.map((l) => [l.kind, l.text, l.old ?? null]), [["add", "x", null], ["del", "y", null], ["ctx", "z", null]]);
});
