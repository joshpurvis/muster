// Undo through a note's draft (notes.ts) against a stand-in daemon: an agent's change arriving
// while the human's typing is unsaved, the merge, Ctrl+Z, and the save that follows.
import type { Note } from "./types.ts";
import { CONFLICT_END, CONFLICT_START } from "./note_merge.ts";

// rpc.ts reads the page's location as it loads.
Object.defineProperty(globalThis, "location", {
  value: { pathname: "/", protocol: "http:", host: "muster.test", hash: "", search: "" },
  configurable: true,
});
const { rpc } = await import("./rpc.ts");
const { applyNoteRemoved, applyNoteUpsert, draft, draftOf, editDraft, flushDraft, notes, undoDraft } = await import(
  "./notes.ts"
);

function eq<T>(actual: T, expected: T, msg = "") {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}\n  actual:   ${a}\n  expected: ${e}`);
}

const agent = { kind: "agent" as const, name: "Claude", session_id: 7 };
let server: Note;

/** The daemon's `note.write` and `note.get`, for note `server`. */
(rpc as unknown as { call: (m: string, p: any) => Promise<unknown> }).call = (method, p) => {
  if (method === "note.get") return Promise.resolve({ ...server });
  if (method === "note.write") {
    if (p.base_rev !== server.rev) return Promise.reject(new Error(`note changed since revision ${p.base_rev}`));
    server = { ...server, body: p.body, rev: server.rev + 1, version: server.version + 1, updated_by: { kind: "human" } };
    return Promise.resolve({ ...server });
  }
  return Promise.reject(new Error(`unexpected ${method}`));
};

/** Note 1 opened in an editor with `body`. */
function open(body: string) {
  server = {
    id: 1, title: "Plan", body, rev: 1, created_at: 0, updated_at: 0, updated_by: { kind: "human" }, window_id: 1,
    beside: null, version: 1,
  };
  applyNoteUpsert({ ...server });
  return draftOf(notes.value.get(1)!)!;
}

/** An agent writes `body` (the daemon's event reaches the window). */
function agentWrites(body: string) {
  server = { ...server, body, rev: server.rev + 1, version: server.version + 1, updated_by: agent };
  applyNoteUpsert({ ...server });
}

/** The human types `inserted` over [at, end) of the editor's text, one step. */
function humanEdits(at: number, end: number, inserted: string) {
  const d = draft(1)!;
  editDraft(1, d.text.slice(0, at) + inserted + d.text.slice(end), { from: at, caret: at + inserted.length, kind: "other" });
}

async function close() {
  await flushDraft(1);
  applyNoteRemoved(1);
}

Deno.test("unsaved typing over the line an agent changed: undo keeps the agent's version, and saves", async () => {
  open("a\nline\nz\n");
  humanEdits(2, 6, "LINE");
  agentWrites("a\nlane\nz\n");
  const merged = `a\nLINE\n${CONFLICT_START("Claude")}\nlane\n${CONFLICT_END}\nz\n`;
  eq(draft(1)!.text, merged, "both versions kept");
  const c = undoDraft(1);
  const text = draft(1)!.text;
  if (!text.includes("lane")) throw new Error(`the agent's version was undone: ${text}`);
  eq(text, `a\nline\n${CONFLICT_START("Claude")}\nlane\n${CONFLICT_END}\nz\n`, "only the human's LINE went back");
  eq(c?.sel, [2, 6], "the line that came back is selected");
  await flushDraft(1);
  eq(server.body, text, "the undo was saved");
  await close();
});

Deno.test("the agent's line below stays when the human's typing above is undone", async () => {
  open("# Plan\n");
  humanEdits(7, 7, "- mine");
  agentWrites("# Plan\n- agent\n");
  eq(draft(1)!.text, "# Plan\n- mine- agent\n", "merged (the human's typing first)");
  undoDraft(1);
  eq(draft(1)!.text, "# Plan\n- agent\n");
  await flushDraft(1);
  eq(server.body, "# Plan\n- agent\n");
  undoDraft(1, true);
  await flushDraft(1);
  eq(server.body, "# Plan\n- mine- agent\n", "redo saved too");
  await close();
});

Deno.test("an agent making the same change before it was saved: undo doesn't take theirs back", async () => {
  open("Plan\n");
  humanEdits(5, 5, "- agree\n");
  agentWrites("Plan\n- agree\n");
  eq(draft(1)!.text, "Plan\n- agree\n");
  eq(undoDraft(1), null, "nothing of the human's own is left to undo");
  await flushDraft(1);
  eq(server.body, "Plan\n- agree\n");
  await close();
});
