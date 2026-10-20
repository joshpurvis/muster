// The kit's stroke icons (panels/kit/icons.ts) in Muster's own window, so the panel strip and the
// web preview bar use the same icon family as the panels themselves.
import { ICONS } from "./kit/icons.ts";

export function KitIcon({ name, size = 13, class: cls }: { name: string; size?: number; class?: string }) {
  return (
    <svg class={cls} width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
      stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d={ICONS[name] ?? ICONS.help} />
    </svg>
  );
}

/** Each panel app's icon and name, for the strip above it and its tab. */
export const APP_KIND: Record<string, [string, string]> = {
  markdown: ["doc", "Markdown"], image: ["image", "Image"], chart: ["chart", "Chart"], table: ["table", "Table"],
  pr_decision: ["pr", "PR decision"], pr_overview: ["list", "Pull requests"], board: ["sparkle", "Board"],
  web_preview: ["globe", "Web preview"], custom: ["sparkle", "Custom app"],
};

/** "just now", "14 s ago", "3 min ago", "2 h ago", "4 d ago". */
export function ago(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 10) return "just now";
  if (s < 60) return `${s} s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h} h ago`;
  return `${Math.floor(h / 24)} d ago`;
}
