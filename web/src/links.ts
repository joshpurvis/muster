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
 * and no text, so columns after one differ from string offsets. `before` is the character just
 * before `cells` and `next` the text just after them, when they may belong to the same text (a
 * wrap, or rows a replay redrew at another width): a link that `next` would turn into another
 * link (`muster://note/9` then `1`) is none, never a link to the wrong id. */
export function linkColumns(
  cells: { chars: string; width: number }[],
  edges: { before?: string; next?: string } = {},
): { start: number; end: number; uri: string }[] {
  const before = edges.before ?? "", next = edges.next ?? "";
  let text = "";
  const col: number[] = [];
  cells.forEach((c, x) => {
    if (c.width === 0) return;
    const s = c.chars || " ";
    for (let i = 0; i < s.length; i++) col.push(x);
    text += s;
  });
  const n = before.length;
  const joined = next ? findMusterLinks(before + text + next) : [];
  return findMusterLinks(before + text)
    .filter((l) => l.start >= n && !joined.some((j) => j.start === l.start && j.uri !== l.uri))
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
  edges: { before?: string; next?: string } = {},
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
        return gone(d, `${a.name} has no tab yet: start it from its row in the sidebar.`);
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

/** A link in a note's text (docs/UI.md "Notes"): `[start, end)` offsets into the text. `web` is an
 * http or https address, opened in the browser; `muster` an address above, shown in Muster. */
export interface TextLink {
  start: number;
  end: number;
  uri: string;
  kind: "web" | "muster";
}

/** Where a web address starts: `http://` or `https://`, not glued to a word before it
 * (`xhttp://` is none). */
const WEB_START = /https?:\/\//gi;
/** Characters no address holds unescaped: it ends before them. */
const WEB_STOP = /[\s<>"`]/;
/** Characters that end a sentence or a quote rather than an address: `see https://a.org/x.`
 * is a link to `https://a.org/x`. */
const TRAILING = ".,;:!?'";
/** Markup around an address (`**https://a.org/x**`): trimmed only as far as the same run before
 * it, since an address may end in these (`?token=abc_`). */
const WRAP = "*_~";
/** Another address starting right after a comma or semicolon (`https://a.org,muster://note/7`). */
const NEXT_ADDRESS = /^(?:https?:\/\/|muster:\/\/)/i;
const OPEN: Record<string, string> = { "(": ")", "[": "]", "{": "}" };
const CLOSE: Record<string, string> = { ")": "(", "]": "[", "}": "{" };

/** Where the web address starting at `from` (just after `://`) ends. A closing bracket the address
 * didn't open ends it (`(https://a.org/x)`, Markdown `[t](https://a.org/x)[u](…)`), one it opened
 * stays (`https://en.wikipedia.org/wiki/Rust_(language)`), and so does a comma or semicolon right
 * before another address, web or Muster (`https://a.org,https://b.org` is two). One pass, so a long run of
 * brackets costs no more than its length. */
function webEnd(text: string, start: number, from: number): number {
  const depth: Record<string, number> = { "(": 0, "[": 0, "{": 0 };
  let i = from;
  for (; i < text.length; i++) {
    const c = text[i];
    if (WEB_STOP.test(c)) break;
    if (c in OPEN) depth[c]++;
    else if (c in CLOSE) {
      if (depth[CLOSE[c]] === 0) break;
      depth[CLOSE[c]]--;
    } else if ((c === "," || c === ";") && NEXT_ADDRESS.test(text.slice(i + 1, i + 10))) break;
  }
  // Sentence ends and quotes after it; then markup as far as the same run before it.
  let end = i;
  while (end > from && TRAILING.includes(text[end - 1])) end--;
  let before = start - 1;
  while (end > from && before >= 0 && WRAP.includes(text[end - 1]) && text[before] === text[end - 1]) {
    end--;
    before--;
    while (end > from && TRAILING.includes(text[end - 1])) end--;
  }
  return end;
}

/** Every link in `text`, in order: web addresses (http, https) and Muster addresses. Nothing else
 * (`file://`, `ftp://`, `javascript:`) is a link. A Muster address inside a web address is part of
 * that address, not a link of its own. */
export function findTextLinks(text: string): TextLink[] {
  const web: TextLink[] = [];
  if (!text.includes("://")) return web;
  const re = new RegExp(WEB_START.source, WEB_START.flags);
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const start = m.index;
    if (start > 0 && /[A-Za-z0-9]/.test(text[start - 1])) continue;
    const end = webEnd(text, start, start + m[0].length);
    re.lastIndex = Math.max(re.lastIndex, end);
    const uri = text.slice(start, end);
    // A host must follow the slashes: `https://` alone, or `https://.`, is none.
    if (!/^https?:\/\/[^/?#]/i.test(uri)) continue;
    web.push({ start, end, uri, kind: "web" });
  }
  // Both lists are in order, so one walk drops the Muster addresses inside web ones.
  const out: TextLink[] = [];
  let w = 0;
  for (const l of findMusterLinks(text)) {
    while (w < web.length && web[w].end <= l.start) out.push(web[w++]);
    if (w < web.length && web[w].start < l.end) continue;
    out.push({ ...l, kind: "muster" });
  }
  while (w < web.length) out.push(web[w++]);
  return out;
}

/** The link the caret is in (`from` to `to`, a selection inside one link counts), looked for on the
 * caret's line only: an address never spans lines. A caret right after an address's last
 * character is in it. */
export function linkAt(text: string, from: number, to = from): TextLink | null {
  const start = text.lastIndexOf("\n", from - 1) + 1;
  const nl = text.indexOf("\n", from);
  const line = text.slice(start, nl < 0 ? text.length : nl);
  for (const l of findTextLinks(line)) {
    if (l.start + start <= from && to <= l.end + start) {
      return { ...l, start: l.start + start, end: l.end + start };
    }
  }
  return null;
}

/** A piece of text with its `[label](url)` links found: plain text, or a link (`muster` set for a
 * Muster link, null for a web one). */
export type TextPart = string | { label: string; url: string; muster: MusterLink | null };

/** `[label](url)`: a label with `\`-escapes (as `src/links.rs` `mention` writes them) and no bare
 * brackets or line break; a url without spaces, with at most one level of balanced parentheses
 * (`https://en.wikipedia.org/wiki/Bread_(food)`). */
const MD_LINK = /\[((?:\\[^\n]|[^\\\[\]\n])+)\]\(((?:[^\s()]|\([^\s()]*\))+)\)/g;

/** Unescape a Markdown label: a backslash before any ASCII punctuation (`\[`, `\&`, `\\`). */
function unescapeLabel(s: string): string {
  return s.replace(/\\([!-\/:-@\[-`{-~])/g, "$1");
}

/** An http(s) address a card may open: parses, has a host, carries no user name or password
 * (`https://docs.example@other.example/` reads as the first and goes to the second) and no
 * backslash (a Markdown escape, `report\)`, would make it another address than the one written). */
export function webAddress(url: string): boolean {
  if (url.length > 2000 || /[\s\\]/.test(url) || !/^https?:\/\//.test(url)) return false;
  try {
    const u = new URL(url);
    return (u.protocol === "https:" || u.protocol === "http:") && !!u.hostname && !u.username && !u.password;
  } catch {
    return false;
  }
}

/** `text` as it reads on a card: each link as its label (for a plain-text line such as an answer
 * preview). */
export function linkLabels(text: string): string {
  return markdownLinks(text).map((p) => (typeof p === "string" ? p : p.label)).join("");
}

/** Find the `[label](url)` links in `text` whose url is a Muster link or an http(s) address, for
 * the Questions tab (and anywhere else text from an agent shows). Nothing else from Markdown is
 * read: any other link (`javascript:`, `file:`, a relative path) and all other syntax stay as they
 * were written. Adjacent plain text comes back as one string. */
export function markdownLinks(text: string): TextPart[] {
  const out: TextPart[] = [];
  let plain = "";
  let at = 0;
  for (const m of text.matchAll(MD_LINK)) {
    const url = m[2];
    const start = m.index ?? 0;
    // An image (`![alt](url)`) or an escaped bracket (`\[x](url)`, an odd run of backslashes)
    // is not a link.
    const slashes = /\\*$/.exec(text.slice(0, start))![0].length;
    if (text[start - 1] === "!" || slashes % 2 === 1) continue;
    const muster = parseMusterLink(url);
    const label = unescapeLabel(m[1]).trim();
    if ((!muster && !webAddress(url)) || !label) continue;
    plain += text.slice(at, m.index);
    if (plain) out.push(plain);
    plain = "";
    out.push({ label, url, muster });
    at = (m.index ?? 0) + m[0].length;
  }
  plain += text.slice(at);
  if (plain) out.push(plain);
  return out;
}
