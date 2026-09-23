// Small helpers: fuzzy matching, time formatting, slugs, clipboard, storage.

/**
 * Fuzzy subsequence score. Returns null when `q` isn't a subsequence of `s`.
 * Higher is better. Rewards consecutive runs, word starts and early matches.
 * `hits` receives matched character indices (for highlighting).
 */
export function fuzzy(q: string, s: string, hits?: number[]): number | null {
  if (!q) return 0;
  const ql = q.toLowerCase();
  const sl = s.toLowerCase();
  let score = 0;
  let si = 0;
  let prev = -2;
  for (let qi = 0; qi < ql.length; qi++) {
    const c = ql[qi];
    if (c === " ") continue;
    const idx = sl.indexOf(c, si);
    if (idx < 0) return null;
    let bonus = 1;
    if (idx === prev + 1) bonus += 4;
    const before = idx > 0 ? s[idx - 1] : "";
    if (idx === 0 || /[\s\-_/.#:>]/.test(before)) bonus += 6;
    else if (s[idx] !== sl[idx] && before === before.toLowerCase()) bonus += 3; // camelCase hump
    score += bonus;
    hits?.push(idx);
    prev = idx;
    si = idx + 1;
  }
  // Prefer shorter targets and exact prefixes.
  score -= s.length * 0.02;
  if (sl.startsWith(ql)) score += 8;
  return score;
}

export function timeAgo(ms: number, now = Date.now()): string {
  const d = Math.max(0, now - ms) / 1000;
  if (d < 45) return "now";
  if (d < 3600) return `${Math.round(d / 60)}m`;
  if (d < 86400) return `${Math.round(d / 3600)}h`;
  if (d < 86400 * 7) return `${Math.round(d / 86400)}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function dayLabel(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const yest = new Date(Date.now() - 86400000);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yest.toDateString()) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" });
}

/** Mirror of the daemon's slug rule (guess: lowercase, [a-z0-9-], collapse dashes). */
export function slugify(name: string): string {
  return name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
}

export function tildify(path: string, home: string): string {
  if (home && (path === home || path.startsWith(home + "/"))) return "~" + path.slice(home.length);
  return path;
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fallback for non-secure contexts.
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch { /* ignore */ }
    ta.remove();
    return ok;
  }
}

export function load<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem("muster." + key);
    if (raw === null) return fallback;
    return JSON.parse(raw) as T;
  } catch {
    return fallback;
  }
}

export function save(key: string, value: unknown) {
  try {
    localStorage.setItem("muster." + key, JSON.stringify(value));
  } catch { /* storage unavailable */ }
}

export function debounce<A extends unknown[]>(fn: (...a: A) => void, ms: number) {
  let t: number | undefined;
  const d = (...a: A) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...a), ms);
  };
  return d;
}

export function clamp(v: number, lo: number, hi: number) {
  return Math.max(lo, Math.min(hi, v));
}

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}
