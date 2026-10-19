// Unified diff parsing for display. Tolerates a bare hunk body (no @@ header) and the
// `diff --git` / `index` / `---` / `+++` preamble that `gh pr diff` prints per file.

export type DiffKind = "add" | "del" | "ctx" | "hunk" | "note";
export interface DiffLine {
  kind: DiffKind;
  text: string;
  /** Line numbers in the old and new file, when known. */
  old?: number;
  new?: number;
}

const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/;

export function parseDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  let oldNo: number | undefined;
  let newNo: number | undefined;
  let inHunk = false;
  const lines = diff.replace(/\r\n?/g, "\n").split("\n");
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  for (const line of lines) {
    const hunk = HUNK.exec(line);
    if (hunk) {
      oldNo = Number(hunk[1]);
      newNo = Number(hunk[3]);
      inHunk = true;
      out.push({ kind: "hunk", text: line });
      continue;
    }
    if (!inHunk && /^(diff --git |index |--- |\+\+\+ |new file mode|deleted file mode|similarity index|rename (from|to) |old mode|new mode|Binary files)/.test(line)) {
      continue;
    }
    if (line.startsWith("\\")) { out.push({ kind: "note", text: line.slice(1).trim() }); continue; }
    if (inHunk && line.startsWith("diff --git ")) { inHunk = false; continue; }
    const sign = line[0];
    if (sign === "+") {
      out.push({ kind: "add", text: line.slice(1), new: newNo });
      if (newNo !== undefined) newNo++;
    } else if (sign === "-") {
      out.push({ kind: "del", text: line.slice(1), old: oldNo });
      if (oldNo !== undefined) oldNo++;
    } else {
      // A context line; an empty line inside a hunk is an empty context line.
      out.push({ kind: "ctx", text: sign === " " ? line.slice(1) : line, old: oldNo, new: newNo });
      if (oldNo !== undefined) oldNo++;
      if (newNo !== undefined) newNo++;
    }
  }
  return out;
}

export function diffStats(lines: DiffLine[]): { added: number; removed: number } {
  let added = 0, removed = 0;
  for (const l of lines) {
    if (l.kind === "add") added++;
    else if (l.kind === "del") removed++;
  }
  return { added, removed };
}
