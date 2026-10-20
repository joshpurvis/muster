// Pure logic for the agents panel (docs/UI.md "Agents panel"): an agent's status from its
// session, the message log, and the New / Edit dialog's rules. No signals, no DOM: unit tested
// (agents_test.ts).

import type {
  Agent,
  AgentMessage,
  AgentState,
  ModelCatalog,
  OpenRouterModel,
  Origin,
  Provider,
  SessionId,
  SessionInfo,
} from "./types.ts";
import { STATE_LABEL } from "./types.ts";

/** What a row shows: the dot's state, whether a process runs, and the state's label and start. */
export interface AgentStatus {
  /** The dot (null: never started, no session). */
  state: AgentState | null;
  /** Its session is running (clicking it only pulls the tab). */
  live: boolean;
  label: string;
  /** Unix ms the state began (null: never started). */
  since: number | null;
}

export function agentStatus(a: Agent, s: SessionInfo | undefined): AgentStatus {
  if (!s || a.session_id === null) return { state: null, live: false, label: "stopped", since: null };
  if (s.state === "exited") return { state: "exited", live: false, label: "stopped", since: s.state_since };
  return { state: s.state, live: true, label: STATE_LABEL[s.state], since: s.state_since };
}

/** "now", "45s", "12m", "3h", "2d". */
export function ago(ms: number, now: number): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 10) return "now";
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** The first non-blank line, trimmed (persona hover, message preview). */
export function firstLine(text: string | null | undefined): string {
  return (text ?? "").split("\n").map((l) => l.trim()).find((l) => l) ?? "";
}

/** Merge a message (new, refused or just replied) into the log, newest first, at most `cap`. */
export function upsertMessage(list: AgentMessage[], m: AgentMessage, cap = 300): AgentMessage[] {
  const out = list.filter((x) => x.id !== m.id);
  const i = out.findIndex((x) => x.id < m.id);
  out.splice(i < 0 ? out.length : i, 0, m);
  return out.slice(0, cap);
}

/** The log as the panel shows it: messages to agent `id` (or all), newest first. */
export function feedFor(list: AgentMessage[], id: number | null): AgentMessage[] {
  return id === null ? list : list.filter((m) => m.to_agent === id);
}

/** Origins a Claude agent shouldn't take unattended (the dialog's one-line warning). */
export function unattendedWarning(kind: Agent["kind"], accepts: Origin[]): boolean {
  return kind === "claude" && accepts.some((o) => o === "schedule" || o === "webhook");
}

export const UNATTENDED_WARNING =
  "Anthropic's consumer terms are unclear about unattended use; scheduled work is safer on a Codex agent.";

/** Agent names: a letter or digit, then letters, digits, `-` or `_` (src/agents.rs `check_name`). */
export function nameError(name: string, taken: string[]): string | null {
  const n = name.trim();
  if (!n) return "Give it a name";
  if (n.length > 40 || !/^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(n)) {
    return "Letters, digits, - and _ only, starting with a letter or digit";
  }
  if (taken.some((t) => t.toLowerCase() === n.toLowerCase())) return `An agent named ${n} already exists`;
  return null;
}

/** The Model / Effort selects' entry for a typed value (a custom model id, an unknown level). Never
 * a real value: those are trimmed and have no spaces (src/agents.rs `agent_args`). */
export const CUSTOM = " custom";

/** Which entry a Model or Effort select shows. `custom` is latched: set when the person picks
 * "Custom…" or edits its text field, cleared only when they pick another entry, so clearing the
 * field or typing `opus[1m]` through `opus` stays in Custom. Unlatched: "" (Default) for no
 * value, the value itself when listed, else CUSTOM (a stored value the list doesn't know is kept
 * and shown as custom, never dropped). */
export function choiceOf(value: string, listed: string[], custom: boolean): string {
  const v = value.trim();
  if (custom) return CUSTOM;
  if (!v) return "";
  return listed.includes(v) ? v : CUSTOM;
}

/** Switching an agent's kind keeps a model or effort only when the new kind lists it. */
export function keepForKind(value: string, listed: string[]): string {
  return listed.includes(value.trim()) ? value : "";
}

/** The effort levels on offer for `model` of `kind`: the model's own list where it differs from
 * the CLI's (src/agents.rs `model_catalog`), else the CLI's. Empty without a catalog. */
export function effortsFor(c: ModelCatalog | null, kind: Agent["kind"], model: string): string[] {
  const k = c?.[kind];
  if (!k) return [];
  return k.models.find((m) => m.id === model.trim())?.efforts ?? k.efforts;
}

/** The effort after the model changes: a level picked from the old model's list that the new
 * model doesn't offer goes back to Default; a custom effort (`custom`, or one the old list
 * didn't have) stays as typed. */
export function effortAfterModel(effort: string, before: string[], after: string[], custom: boolean): string {
  if (choiceOf(effort, before, custom) !== effort.trim()) return effort;
  return after.includes(effort.trim()) ? effort : "";
}

/** A memory file path the daemon accepts for a new note (typed in the name field; surrounding
 * spaces are trimmed off what was typed): relative, plain parts (no `..`, `.` or empty parts),
 * each one reading as what it is (`hiddenCharsOrSpaces`). */
export function memoryPathError(path: string): string | null {
  const p = path.trim();
  if (!p) return "Name the file";
  if (p.startsWith("/") || p.endsWith("/")) return "A file name inside memory/, like notes.md";
  const parts = p.split("/");
  if (parts.some((part) => !part || part === "." || part === "..")) return "No .. or empty parts";
  if (parts.some(hiddenCharsOrSpaces)) return "No invisible characters, or spaces around a folder name";
  return null;
}

/** Largest memory file the daemon reads, writes or adds (`MEMORY_FILE_MAX`). */
export const MEMORY_FILE_MAX = 256 * 1024;
/** Longest name `agent.memory.add` takes (`MEMORY_NAME_MAX`, in UTF-8 bytes). */
const MEMORY_NAME_MAX = 200;
/** Unicode's Default_Ignorable_Code_Point ranges, as the daemon's `default_ignorable` (kept as
 * numbers so no invisible character is ever written into this file). */
const IGNORABLE: [number, number][] = [
  [0x00ad, 0x00ad],
  [0x034f, 0x034f],
  [0x061c, 0x061c],
  [0x115f, 0x1160],
  [0x17b4, 0x17b5],
  [0x180b, 0x180f],
  [0x200b, 0x200f],
  [0x202a, 0x202e],
  [0x2060, 0x206f],
  [0x3164, 0x3164],
  [0xfe00, 0xfe0f],
  [0xfeff, 0xfeff],
  [0xffa0, 0xffa0],
  [0xfff0, 0xfff8],
  [0x1bca0, 0x1bca3],
  [0x1d173, 0x1d17a],
  [0xe0000, 0xe0fff],
];

/** A name part the daemon refuses to create (`agents::memory_new_part`): a control character
 * (Cc), a default-ignorable one, or spaces before or after it. */
export function hiddenCharsOrSpaces(part: string): boolean {
  if (part !== part.trim()) return true;
  for (const ch of part) {
    const c = ch.codePointAt(0)!;
    if (c < 0x20 || (c >= 0x7f && c <= 0x9f)) return true;
    if (IGNORABLE.some(([lo, hi]) => c >= lo && c <= hi)) return true;
  }
  return false;
}

/** Why the daemon would refuse a picked or dropped file's name for `agent.memory.add`, or null.
 * The same rules as `agents::memory_file_name`: one visible name exactly as it is (never
 * trimmed), no folders, not hidden. */
export function memoryFileNameError(name: string): string | null {
  if (!name.trim()) return "it has no name";
  if (name !== name.trim()) return "its name starts or ends with a space";
  if (new TextEncoder().encode(name).length > MEMORY_NAME_MAX) return "its name is too long";
  if (name.includes("/") || name.includes("\\")) return "its name is a path";
  if (name.startsWith(".")) return "hidden files (names starting with a dot) aren't added";
  if (hiddenCharsOrSpaces(name)) return "its name has control or invisible characters";
  return null;
}

/** A picked or dropped file as text for memory/: UTF-8 (a BOM is dropped), at most
 * `MEMORY_FILE_MAX` bytes, no NUL characters. Otherwise why it is skipped. */
export async function readMemoryFile(
  f: Blob & { name: string },
): Promise<{ text: string } | { skip: string }> {
  const bad = memoryFileNameError(f.name);
  if (bad) return { skip: bad };
  if (f.size > MEMORY_FILE_MAX) return { skip: `larger than ${MEMORY_FILE_MAX / 1024} KB` };
  let bytes: ArrayBuffer;
  try {
    bytes = await f.arrayBuffer();
  } catch {
    return { skip: "couldn't be read (a folder?)" };
  }
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return { skip: "not a text file" };
  }
  if (text.includes("\0")) return { skip: "not a text file" };
  return { text };
}

// ---------------- OpenRouter (src/openrouter.rs) ----------------

/** The badge on agents and tabs that run on OpenRouter: billed per token, not a subscription. */
export const OPENROUTER_BADGE = "OpenRouter · pay per use";

/** Sessions of agents that run on OpenRouter (their tabs get the badge). */
export function openRouterSessions(list: Agent[]): Set<SessionId> {
  return new Set(
    list.filter((a) => a.provider === "openrouter" && a.session_id !== null).map((a) => a.session_id!),
  );
}

/** Each CLI's own vendor comes first: Claude Code is built for Anthropic's models, Codex for
 * OpenAI's (others work through OpenRouter on a best-effort basis). */
export const PREFERRED_VENDOR: Record<Agent["kind"], string> = { claude: "anthropic", codex: "openai" };

export interface VendorGroup {
  vendor: string;
  models: OpenRouterModel[];
}

/** The models matching `query` (every word, in the id or the name, any case), grouped by
 * vendor: `prefer` first, then A to Z; within a vendor, the daemon's order (newest first). */
export function vendorGroups(models: OpenRouterModel[], query: string, prefer: string): VendorGroup[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const groups = new Map<string, OpenRouterModel[]>();
  for (const m of models) {
    const hay = `${m.id} ${m.name}`.toLowerCase();
    if (!words.every((w) => hay.includes(w))) continue;
    const g = groups.get(m.vendor);
    if (g) g.push(m);
    else groups.set(m.vendor, [m]);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => (a === prefer ? -1 : b === prefer ? 1 : a.localeCompare(b)))
    .map(([vendor, models]) => ({ vendor, models }));
}

/** `$0.25`, `$1.50`, `$3`, `<$0.01`. */
function dollars(n: number): string {
  if (n > 0 && n < 0.01) return "<$0.01";
  const cents = Math.round(n * 100);
  return cents % 100 === 0 ? `$${cents / 100}` : `$${(cents / 100).toFixed(2)}`;
}

/** What a model costs per million tokens, in and out: `$3 / $15`, `free`, or "" when unknown. */
export function priceLabel(m: OpenRouterModel): string {
  const { prompt_per_m: p, completion_per_m: c } = m;
  if (p === null || c === null) return "";
  if (p === 0 && c === 0) return "free";
  return `${dollars(p)} / ${dollars(c)}`;
}

/** `1M`, `200k`, "" when unknown. */
export function contextLabel(n: number | null): string {
  if (!n) return "";
  if (n >= 1_000_000) return `${Number((n / 1_000_000).toFixed(1))}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/** The values the Model field lists for `provider`: the CLI's curated ids, or OpenRouter's. */
export function listedModels(
  provider: Provider,
  kind: Agent["kind"],
  catalog: ModelCatalog | null,
  openrouter: OpenRouterModel[],
): string[] {
  return provider === "openrouter"
    ? openrouter.map((m) => m.id)
    : (catalog?.[kind].models ?? []).map((m) => m.id);
}

/** Codex on OpenRouter has no usable default (its own model names lack the vendor prefix). */
export function needsModel(provider: Provider, kind: Agent["kind"], model: string): boolean {
  return provider === "openrouter" && kind === "codex" && !model.trim();
}
