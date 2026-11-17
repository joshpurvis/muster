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
export interface AddonRegistry {
  addons: Addon[];
  revision: string;
}
export const addonRevision = signal("");
export const ADDON_SANDBOX = "allow-scripts allow-same-origin allow-forms";
const MESSAGE_ERRORS = [
  "Not sent: this agent isn't allowed.",
  "Not sent: limit of 30 an hour.",
  "Not sent: message must be 1-300 characters.",
  "Not sent.",
] as const;
export function addonMessageError(error: unknown): string {
  const text = error instanceof Error ? error.message : error;
  return MESSAGE_ERRORS.find((allowed) => allowed === text) ?? "Not sent.";
}
/** Retry the existing node: moving/replacing it would lose the frame's placement. */
export function reloadAddonFrame(frame: Pick<HTMLIFrameElement, "src"> | null) {
  // The DOM setter navigates even when the address is unchanged.
  // deno-lint-ignore no-self-assign
  if (frame) frame.src = frame.src;
}
export function pinnedLabel(title: string, count: number, addon: boolean): string {
  return addon ? `${title}${count ? `, ${count}` : ""}` : `Questions, ${count} open`;
}
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
