// Unified diff parsing for display. Tolerates a bare hunk body (no @@ header) and the
// `diff --git` / `index` / `---` / `+++` preamble that `gh pr diff` prints per file.

/** `meta`: a file-level change with no lines to show (rename, mode, binary, new or deleted file). */
export type DiffKind = "add" | "del" | "ctx" | "hunk" | "note" | "meta";
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
  const pending: Pending = {};
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
    if (!inHunk) {
      const meta = metaLine(line, pending);
      if (meta !== undefined) {
        if (meta) out.push({ kind: "meta", text: meta });
        continue;
      }
    }
    if (line.startsWith("\\")) { out.push({ kind: "note", text: line.slice(1).trim() }); continue; }
    if (inHunk && line.startsWith("diff --git ")) { inHunk = false; pending.from = pending.oldMode = undefined; continue; }
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

interface Pending { from?: string; oldMode?: string }

/**
 * A header line before a hunk: undefined when it is not one, "" when it is shown with a later
 * line (or not at all), else the text to show.
 */
function metaLine(line: string, pending: Pending): string | undefined {
  let m: RegExpExecArray | null;
  if ((m = /^rename from (.*)$/.exec(line))) { pending.from = m[1]; return ""; }
  if ((m = /^rename to (.*)$/.exec(line))) {
    const text = `renamed ${pending.from ?? "?"} → ${m[1]}`;
    pending.from = undefined;
    return text;
  }
  if ((m = /^copy from (.*)$/.exec(line))) { pending.from = m[1]; return ""; }
  if ((m = /^copy to (.*)$/.exec(line))) { const text = `copied ${pending.from ?? "?"} → ${m[1]}`; pending.from = undefined; return text; }
  if ((m = /^old mode (\d+)$/.exec(line))) { pending.oldMode = m[1]; return ""; }
  if ((m = /^new mode (\d+)$/.exec(line))) {
    const text = `mode ${pending.oldMode ?? "?"} → ${m[1]}`;
    pending.oldMode = undefined;
    return text;
  }
  if ((m = /^new file mode (\d+)$/.exec(line))) return `new file (mode ${m[1]})`;
  if ((m = /^deleted file mode (\d+)$/.exec(line))) return `file deleted (mode ${m[1]})`;
  if (/^Binary files .* differ$/.test(line) || line === "GIT binary patch") return "binary file changed";
  if (/^(diff --git |index |--- |\+\+\+ |similarity index |dissimilarity index )/.test(line)) return "";
  return undefined;
}

export function diffStats(lines: DiffLine[]): { added: number; removed: number } {
  let added = 0, removed = 0;
  for (const l of lines) {
    if (l.kind === "add") added++;
    else if (l.kind === "del") removed++;
  }
  return { added, removed };
}
