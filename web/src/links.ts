// Links that bring a note, panel, tab or agent to the front (docs/UI.md "Links"). Agents write
// them for the human as Markdown, `[Pairs to review](muster://note/91)`, which Claude Code and
// Codex print as `Pairs to review (muster://note/91)`. Muster's terminal makes the address
// clickable, and a program's own hyperlink (OSC 8) to one too (terms.ts).
//
// A link names a kind and an id (an agent: its name), nothing else: no token, no query, no
// fragment. Text in a terminal may come from anywhere (a web page an agent printed), so a link
// only ever shows what it names: `followLink` can select a tab, open a note's tab or raise a
// window, the same as Jump, and never sends a prompt, approves, starts or stops anything or
// changes a note's text. It is pure: everything it may do comes in through `LinkDeps`.

export type MusterLink =
  | { kind: "note" | "panel" | "tab"; id: number }
  | { kind: "agent"; name: string };

/** The whole address, exactly: `muster://note/91`, `muster://panel/3`, `muster://tab/12`
 * (a session's tab), `muster://agent/reviewer`. Ids are positive integers without leading zeros;
 * agent names as `src/agents.rs` `check_name` allows (letters, digits, `-`, `_`; at most 40). */
const LINK = /^muster:\/\/(?:(note|panel|tab)\/([1-9][0-9]{0,14})|agent\/([A-Za-z0-9][A-Za-z0-9_-]{0,39}))$/;

/** The same shapes inside text. A link ends where the id or name does, so `(muster://note/9).`
 * yields `muster://note/9`; one that goes on (`muster://note/9x`, `/9/2`, `/9?a=b`) is none. */
const IN_TEXT =
  /muster:\/\/(?:(?:note|panel|tab)\/[1-9][0-9]{0,14}|agent\/[A-Za-z0-9][A-Za-z0-9_-]{0,39})(?![A-Za-z0-9_\/?#-])/g;

export function parseMusterLink(uri: string): MusterLink | null {
  const m = LINK.exec(uri);
  if (!m) return null;
  if (m[3]) return { kind: "agent", name: m[3] };
  return { kind: m[1] as "note" | "panel" | "tab", id: Number(m[2]) };
}

/** Where links are in one line of plain text: `[start, end)` offsets into `text`. A link must
 * not be glued to a word before it either (`xmuster://note/1` is none). */
export function findMusterLinks(text: string): { start: number; end: number; uri: string }[] {
  const out: { start: number; end: number; uri: string }[] = [];
  if (!text.includes("muster://")) return out;
  for (const m of text.matchAll(IN_TEXT)) {
    const start = m.index ?? 0;
    if (start > 0 && /[A-Za-z0-9_]/.test(text[start - 1])) continue;
    out.push({ start, end: start + m[0].length, uri: m[0] });
  }
  return out;
}

/** Links in one terminal row as 1-based, inclusive cell columns (xterm's `IBufferRange.x`).
 * `cells` are the row's cells left to right; a wide character takes two, the second with width 0
 * and no text, so columns after one differ from string offsets. `before` and `after` are the
 * characters just outside `cells`, if any may belong to the same text (a row wrapped or drawn
 * full to its edge): a link they would continue (`muster://note/9` then `1`) is none, never a link
 * to the wrong id. */
export function linkColumns(
  cells: { chars: string; width: number }[],
  edges: { before?: string; after?: string } = {},
): { start: number; end: number; uri: string }[] {
  const before = edges.before ?? "", after = edges.after ?? "";
  let text = "";
  const col: number[] = [];
  cells.forEach((c, x) => {
    if (c.width === 0) return;
    const s = c.chars || " ";
    for (let i = 0; i < s.length; i++) col.push(x);
    text += s;
  });
  const n = before.length;
  return findMusterLinks(before + text + after)
    .filter((l) => l.start >= n && l.end <= n + text.length)
    .map((l) => ({ start: col[l.start - n] + 1, end: col[l.end - n - 1] + 1, uri: l.uri }));
}

/** Links in a logical line (a row and the rows wrapped from it) as xterm buffer ranges, 1-based
 * and inclusive: `cells` are the rows' cells in order, `cols` per row, the first row being buffer
 * row `top` (0-based). A link split by the wrap is one range over both rows. `edges` as for
 * `linkColumns`. */
export function linkRanges(
  cells: { chars: string; width: number }[],
  cols: number,
  top: number,
  edges: { before?: string; after?: string } = {},
) {
  const at = (col: number) => ({ x: ((col - 1) % cols) + 1, y: top + 1 + Math.floor((col - 1) / cols) });
  return linkColumns(cells, edges).map((l) => ({ start: at(l.start), end: at(l.end), uri: l.uri }));
}

/** The longest address, in characters (`muster://agent/` and a 40-character name). */
export const MAX_LINK = 55;

/** What following a link may use: lookups, the three "show it" actions and a short message. */
export interface LinkDeps {
  hasNote(id: number): boolean;
  /** Open (not closed) panel. */
  hasPanel(id: number): boolean;
  hasSession(id: number): boolean;
  /** The agent with this name (case-insensitive): its display name and current session, if
   * any; null when there is no such agent. */
  agent(name: string): { name: string; session: number | null } | null;
  gotoNote(id: number): void;
  gotoPanel(id: number): void;
  gotoSession(id: number): void;
  /** A short message for the human (a link to something that is gone). */
  notify(text: string): void;
}

/** Show what `link` names, or say in a short message that it no longer exists. True when it was
 * shown. */
export function followLink(link: MusterLink, d: LinkDeps): boolean {
  switch (link.kind) {
    case "note":
      if (!d.hasNote(link.id)) return gone(d, `Note ${link.id} no longer exists.`);
      d.gotoNote(link.id);
      return true;
    case "panel":
      if (!d.hasPanel(link.id)) return gone(d, `Panel ${link.id} is closed.`);
      d.gotoPanel(link.id);
      return true;
    case "tab":
      if (!d.hasSession(link.id)) return gone(d, `Tab ${link.id} no longer exists.`);
      d.gotoSession(link.id);
      return true;
    case "agent": {
      const a = d.agent(link.name);
      if (!a) return gone(d, `There is no agent named ${link.name}.`);
      if (a.session === null || !d.hasSession(a.session)) {
        return gone(d, `${a.name} has no tab yet: start it from the Agents panel.`);
      }
      d.gotoSession(a.session);
      return true;
    }
  }
}

function gone(d: LinkDeps, text: string): false {
  d.notify(text);
  return false;
}
