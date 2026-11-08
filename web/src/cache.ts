// Cold prompt cache (src/cache.rs, docs/UI.md "Cold cache bar"): an agent idle past its
// provider's cache lifetime re-reads its whole conversation uncached on the next message.
// Pure helpers; the clock and the dismissals live in state.ts.
import type { CacheTtl, SessionInfo } from "./types.ts";

/** The backend's defaults (src/cache.rs `CacheTtl::default`), in minutes; 0 never goes cold. */
export const DEFAULT_CACHE_TTL: CacheTtl = { claude: 60, claude_openrouter: 5, codex_chatgpt: 0, codex_openrouter: 10 };
export const CACHE_TTL_MAX = 24 * 60;

/** The settings dialog's rows: which field, and what it covers. */
export const CACHE_TTL_ROWS: { key: keyof CacheTtl; label: string; hint: string }[] = [
  { key: "claude", label: "Claude on a subscription", hint: "Claude Code asks for a 1-hour cache" },
  {
    key: "claude_openrouter",
    label: "Claude on OpenRouter or an API key",
    hint: "Anthropic's default, unless Claude Code asks for 1 hour",
  },
  { key: "codex_chatgpt", label: "Codex logged in with ChatGPT", hint: "OpenAI keeps these conversations cached" },
  { key: "codex_openrouter", label: "Codex on OpenRouter", hint: "OpenRouter may switch provider after 10 idle minutes" },
];

/** An idle agent's dot: green while its cache is warm, dim green in its last `COOLING_MS`,
 * grey once cold. Idle shells (no cache) are grey too. */
export type CacheDot = "warm" | "cooling" | "cold";
export const COOLING_MS = 5 * 60_000;

/** `s`'s cache, for its dot; null unless it is idle. Without a cold time (it never goes cold,
 * has had no turn yet, or the warning is off: `warn`) an idle agent counts as warm. */
export function cacheDot(s: SessionInfo, now: number, warn = true): CacheDot | null {
  if (s.state !== "idle") return null;
  if (s.kind === "shell") return "cold";
  if (!warn || s.cache_cold_at == null) return "warm";
  if (now >= s.cache_cold_at) return "cold";
  return now >= s.cache_cold_at - COOLING_MS ? "cooling" : "warm";
}

/** A channel's dot when it is idle: green if any agent in it is warm, else dim green if one is
 * cooling, else grey. */
export function channelCacheDot(list: SessionInfo[], now: number, warn = true): CacheDot {
  const dots = list.map((s) => cacheDot(s, now, warn));
  return dots.includes("warm") ? "warm" : dots.includes("cooling") ? "cooling" : "cold";
}

/** Words for the dot's tooltip and accessible name. */
export const CACHE_DOT_LABEL: Record<CacheDot, string> = {
  warm: "cache warm",
  cooling: "cache going cold soon",
  cold: "cache cold",
};

const WAITING_NOT = new Set(["working", "starting", "exited"]);

/** Its next message would re-read the conversation uncached: an agent past `cache_cold_at`,
 * waiting for someone (not working, starting or exited). */
export function isCold(s: SessionInfo, now: number): boolean {
  return s.kind !== "shell" && !WAITING_NOT.has(s.state) && s.cache_cold_at != null && now >= s.cache_cold_at;
}

/** Waking it now would re-read the conversation uncached: its cache has likely expired, whatever
 * its state (an asleep agent's session is exited; src/sleep.rs `wake_cost`). */
export function wakeCold(s: SessionInfo | undefined, now: number): boolean {
  return !!s && s.kind !== "shell" && s.cache_cold_at != null && now >= s.cache_cold_at;
}

/** How long since its last model request, or null. */
export function idleFor(s: SessionInfo, now: number): number | null {
  return s.last_turn_at == null ? null : Math.max(0, now - s.last_turn_at);
}

/** "1 h 20 m", "45 m", "2 h" (as src/cache.rs `idle_text`). */
export function idleText(ms: number): string {
  const min = Math.floor(Math.max(0, ms) / 60_000);
  const h = Math.floor(min / 60), m = min % 60;
  if (!h) return `${m} m`;
  return m ? `${h} h ${m} m` : `${h} h`;
}

/** A lifetime for people: "1 h", "5 min", "1 h 30 min", "0, never cold" for 0. */
export function ttlText(min: number): string {
  if (min === 0) return "0, never cold";
  const h = Math.floor(min / 60), m = min % 60;
  if (!h) return `${m} min`;
  return m ? `${h} h ${m} min` : `${h} h`;
}

/** One idle period of one session: dismissing the bar lasts until the next turn ends. */
export function coldKey(s: SessionInfo): string {
  return `${s.id}:${s.last_turn_at ?? 0}`;
}

/** The bar's sentence. */
export function coldText(s: SessionInfo, now: number): string {
  const idle = idleFor(s, now);
  return `${idle === null ? "Idle" : `Idle ${idleText(idle)}`}. The prompt cache has likely expired, so your ` +
    `next message re-reads the whole conversation at full cost.`;
}

/** The lifetime that applies to `s` (minutes), for tooltips. */
export function ttlFor(s: SessionInfo, ttl: CacheTtl): number | null {
  if (s.kind === "shell") return null;
  const key: keyof CacheTtl = s.kind === "claude"
    ? (s.provider === "openrouter" ? "claude_openrouter" : "claude")
    : s.provider === "openrouter" ? "codex_openrouter" : "codex_chatgpt";
  return ttl[key];
}

/** Parse a minutes field: a whole number 0 (never cold) to CACHE_TTL_MAX, else null. */
export function parseTtl(v: string): number | null {
  const t = v.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  return n <= CACHE_TTL_MAX ? n : null;
}

/** Keep the newest `max` dismissals (insertion order). */
export function addDismissal(list: string[], key: string, max = 200): string[] {
  const out = list.filter((k) => k !== key);
  out.push(key);
  return out.slice(-max);
}
