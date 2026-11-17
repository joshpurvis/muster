// The add-on bridge's complete input allowlist. No other window capabilities are exposed.
import { signal } from "@preact/signals";
export interface Addon {
  id: string;
  name: string;
  url: string;
  icon: string;
  agents: string[];
}
export const addons = signal<Addon[]>([]);
export const addonLabels = signal<Record<string, { title?: string; count?: number }>>({});
export const addonTitle = (a: Addon) => addonLabels.value[a.id]?.title || a.name;
export const THEME_TOKENS = [
  "--bg-0",
  "--bg-1",
  "--bg-2",
  "--border",
  "--text",
  "--text-muted",
  "--accent",
  "--font-ui",
  "--font-mono",
] as const;
export type AddonMessage =
  & { muster: 1 }
  & (
    | { type: "badge"; count: number }
    | { type: "title"; text: string }
    | { type: "open-link"; url: string }
    | { type: "message-agent"; agent: string; text: string }
  );
export function externalLink(value: unknown): string | null {
  if (typeof value !== "string" || value.length > 2048) return null;
  try {
    const u = new URL(value);
    return u.protocol === "https:" && !u.username && !u.password ? u.href : null;
  } catch {
    return null;
  }
}
export function addonMessage(
  e: Pick<MessageEvent, "source" | "origin" | "data">,
  source: Window | null,
  origin: string,
): AddonMessage | null {
  if (!source || e.source !== source || e.origin !== origin) return null;
  const d = e.data;
  if (!d || typeof d !== "object" || Array.isArray(d) || d.muster !== 1 || typeof d.type !== "string") {
    return null;
  }
  const shapes: Record<string, string[]> = {
    badge: ["count"],
    title: ["text"],
    "open-link": ["url"],
    "message-agent": ["agent", "text"],
  };
  if (!Object.hasOwn(shapes, d.type)) return null;
  const keys = ["muster", "type", ...shapes[d.type]];
  if (Object.keys(d).length !== keys.length || !keys.every((k) => Object.hasOwn(d, k))) return null;
  const text = (v: unknown, min: number, max: number) =>
    typeof v === "string" && [...v].length >= min && [...v].length <= max;
  switch (d.type) {
    case "badge":
      return Number.isInteger(d.count) && d.count >= 0 && d.count <= 999 ? d : null;
    case "title":
      return text(d.text, 0, 60) ? d : null;
    case "open-link":
      return externalLink(d.url) ? d : null;
    case "message-agent":
      return typeof d.agent === "string" && /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,39}$/.test(d.agent) &&
          text(d.text, 1, 300)
        ? d
        : null;
    default:
      return null;
  }
}
