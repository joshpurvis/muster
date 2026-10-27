// Pure logic for the agents panel (docs/UI.md "Agents panel"): an agent's status from its
// session, the message log, and the New / Edit dialog's rules. No signals, no DOM: unit tested
// (agents_test.ts).

import type {
  Agent,
  AgentMessage,
  AgentState,
  LateAnswer,
  ModelCatalog,
  OpenRouterModel,
  Origin,
  Provider,
  SessionInfo,
} from "./types.ts";
import { STATE_LABEL } from "./types.ts";
import { idleText } from "./cache.ts";

/** What a row shows: the dot's state, whether a process runs, and the state's label and start. */
export interface AgentStatus {
  /** The dot (null: never started, no session). */
  state: AgentState | null;
  /** Its session is running (clicking it only pulls the tab). */
  live: boolean;
  /** Asleep: stopped while idle, the next message wakes it (src/sleep.rs). */
  asleep: boolean;
  label: string;
  /** Unix ms the state began (null: never started). */
  since: number | null;
}

export function agentStatus(a: Agent, s: SessionInfo | undefined): AgentStatus {
  // Asleep is the agent's own state: its session is exited (or gone), and it is not "stopped".
  if (isAsleep(a, s)) return { state: "exited", live: false, asleep: true, label: "asleep", since: a.asleep_at ?? null };
  if (!s || a.session_id === null) return { state: null, live: false, asleep: false, label: "stopped", since: null };
  if (s.state === "exited") return { state: "exited", live: false, asleep: false, label: "stopped", since: s.state_since };
  return { state: s.state, live: true, asleep: false, label: STATE_LABEL[s.state], since: s.state_since };
}

/** The agent is asleep (its record says so and no run of its session is going). */
export function isAsleep(a: Agent | undefined, s: SessionInfo | undefined): boolean {
  return !!a?.asleep && (!s || s.state === "exited");
}

/** The shortest and longest idle time an agent sleeps after (src/sleep.rs `AFTER_MIN`/`AFTER_MAX`). */
export const SLEEP_AFTER_MIN = 5;
export const SLEEP_AFTER_MAX = 7 * 24 * 60;
/** What the dialog suggests when sleep is switched on: about when a Claude subscription's cache
 * expires anyway, so sleeping then costs nothing extra on the next message. */
export const SLEEP_AFTER_DEFAULT = 60;

/** Parse the dialog's minutes field: a whole number in range, else null. */
export function parseSleepAfter(v: string): number | null {
  const t = v.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  return n >= SLEEP_AFTER_MIN && n <= SLEEP_AFTER_MAX ? n : null;
}

/** The asleep bar's sentence: how long it has slept, that the next message wakes it, and when
 * its prompt cache has likely expired, what that wake costs (`cold`). */
export function asleepText(name: string, asleepAt: number | null | undefined, cold: boolean, now: number): string {
  const lead = asleepAt != null && now - asleepAt >= 60_000
    ? `${name} has been asleep for ${idleText(now - asleepAt)}`
    : `${name} is asleep`;
  return `${lead}. The next message, or typing here, wakes it` +
    (cold
      ? `: it re-reads the whole conversation at full cost, since its prompt cache has likely expired.`
      : " into the same conversation.");
}

/** The asleep bar's line after a wake that failed (`wake_error`): still asleep, how to try again,
 * and what a wake costs once its prompt cache has likely expired (`cold`). */
export function wakeFailedText(name: string, error: string, cold: boolean): string {
  return `Waking ${name} failed: ${error.replace(/\.$/, "")}. It is still asleep: the next message, or typing here, tries again` +
    (cold ? ", and re-reads the whole conversation at full cost." : ".");
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

/** A tab bills OpenRouter while its run was started on it (the session's own provider, so a
 * changed agent keeps the badge until a restart applies the change). */
export function sessionOnOpenRouter(s: SessionInfo | undefined): boolean {
  return s?.provider === "openrouter" && s.state !== "exited";
}

/** An agent row's badge: shown while its live run is on OpenRouter or its saved provider is;
 * `note` says when the two differ (the change waits for a restart). */
export function agentBilling(
  a: Agent,
  s: SessionInfo | undefined,
): { badge: boolean; label: string; note: string | null } {
  const live = !!s && a.session_id === s.id && s.state !== "exited";
  const running = live && s!.provider === "openrouter";
  const saved = a.provider === "openrouter";
  const pending = live && s!.provider !== undefined && running !== saved;
  if (!pending) return { badge: running || saved, label: OPENROUTER_BADGE, note: null };
  return running
    ? {
      badge: true,
      label: "OpenRouter · until restart",
      note: "Still runs on OpenRouter, pay per use: its new provider applies when it restarts",
    }
    : { badge: true, label: "OpenRouter · from restart", note: "Switches to OpenRouter (pay per use) when it restarts" };
}

/** The message after showing a running agent whose saved provider its run doesn't have yet. */
export function providerPendingNote(a: Agent, s: SessionInfo | undefined): string | null {
  if (!s || s.provider === undefined || s.state === "exited") return null;
  const saved = a.provider ?? "subscription";
  if (s.provider === saved) return null;
  const name = (p: string) => (p === "openrouter" ? "OpenRouter" : "its subscription");
  return `${a.name} still runs on ${name(s.provider)}. ${name(saved)[0].toUpperCase()}${
    name(saved).slice(1)
  } applies when it restarts: Start fresh, or Stop and start it.`;
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

/** Did message `m` carry another message's late reply to its asker? Its text says so
 * (src/agent_reply.rs `delivered_text`), whatever part of the log is loaded. */
export function carriesLateReply(m: AgentMessage): boolean {
  return m.reply_to !== null && (m.text ?? "").startsWith(`Late reply to your message ${m.reply_to}`);
}

/** How an agent's late reply (`reply`) reached whoever asked, in a sentence. */
export function lateText(a: LateAnswer): string {
  switch (a.status) {
    case "pending":
    case "delivering":
      return "Sending it to whoever asked…";
    case "returned":
      return "The ask that was still waiting got it as its reply.";
    case "delivered":
      return a.note ? `${a.note[0].toUpperCase()}${a.note.slice(1)}.` : "Delivered to whoever asked.";
    case "shown":
      return "Shown here: the human (or a schedule) asked.";
    case "refused":
    case "undelivered":
      return `Kept here, not delivered${a.note ? `: ${a.note.replace(/^not delivered:? ?/, "")}` : ""}.`;
  }
}
