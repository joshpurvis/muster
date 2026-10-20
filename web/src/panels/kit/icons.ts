// Muster Kit icons: one stroke family on a 24 px grid. An unknown name draws "?" and logs the
// valid names once.

export const ICONS: Record<string, string> = {
  check: "M20 6 9 17l-5-5", x: "M18 6 6 18M6 6l12 12", alert: "M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
  info: "M12 16v-4m0-4h.01M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0z", clock: "M12 6v6l4 2M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0z",
  refresh: "M21 12a9 9 0 1 1-2.6-6.4L21 8M21 3v5h-5", external: "M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
  branch: "M6 3v12M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM18 9a9 9 0 0 1-9 9",
  merge: "M18 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM6 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM6 21V9a9 9 0 0 0 9 9",
  search: "m21 21-4.3-4.3M19 11a8 8 0 1 1-16 0 8 8 0 0 1 16 0z",
  rocket: "M4.5 16.5c-1.5 1.3-2 5-2 5s3.7-.5 5-2c.7-.8.7-2.1-.1-2.9a2.2 2.2 0 0 0-2.9-.1zM12 15l-3-3a22 22 0 0 1 2-4 12.9 12.9 0 0 1 11-6c0 2.7-.8 7.5-6 11a22 22 0 0 1-4 2zM9 12H4s.6-3 2-4c1.6-1.1 5 0 5 0M12 15v5s3-.6 4-2c1.1-1.6 0-5 0-5",
  undo: "M9 14 4 9l5-5M4 9h10.5a5.5 5.5 0 0 1 0 11H11", shield: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10zM9 12l2 2 4-4",
  zap: "M13 2 3 14h9l-1 8 10-12h-9l1-8z", play: "M6 3l14 9-14 9V3z", pause: "M6 4h4v16H6zM14 4h4v16h-4z", dot: "M12 12h.01",
  chart: "M3 3v18h18M7 15l4-4 3 3 6-6", table: "M3 3h18v18H3zM3 9h18M3 15h18M9 3v18",
  image: "M3 3h18v18H3zM8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3zM21 15l-5-5L5 21",
  doc: "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6M8 13h8M8 17h6",
  pr: "M18 15V8a3 3 0 0 0-3-3h-3M15 2l-3 3 3 3M6 9v12M6 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM18 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  globe: "M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0zM2 12h20M12 2a15 15 0 0 1 0 20M12 2a15 15 0 0 0 0 20",
  lock: "M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4", server: "M3 4h18v7H3zM3 13h18v7H3zM7 7.5h.01M7 16.5h.01",
  db: "M4 6c0 1.7 3.6 3 8 3s8-1.3 8-3-3.6-3-8-3-8 1.3-8 3zM4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3",
  dollar: "M12 2v20M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6", flask: "M9 3h6M10 3v6L4.5 19a1.5 1.5 0 0 0 1.3 2h12.4a1.5 1.5 0 0 0 1.3-2L14 9V3M7 15h10",
  sparkle: "M12 3l1.9 5.8L20 11l-6.1 2.2L12 19l-1.9-5.8L4 11l6.1-2.2z", plus: "M12 5v14M5 12h14", minus: "M5 12h14",
  arrowUp: "M12 19V5M5 12l7-7 7 7", arrowDown: "M12 5v14M19 12l-7 7-7-7", arrowRight: "M5 12h14M12 5l7 7-7 7", chevron: "m6 9 6 6 6-6",
  maximize: "M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7", eye: "M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12zM12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
  flag: "M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1zM4 22v-7",
  users: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8",
  terminal: "m4 17 6-6-6-6M12 19h8", copy: "M9 9h12v12H9zM5 15H3V3h12v2", filter: "M22 3H2l8 9.5V19l4 2v-8.5z", sort: "M3 6h18M6 12h12M10 18h4",
  help: "M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0zM9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3M12 17h.01",
};

const warned = new Set<string>();

/** An icon as SVG markup (aria-hidden: the text beside it carries the meaning). */
export function icon(name: string, cls = ""): string {
  if (!Object.hasOwn(ICONS, name) && !warned.has(name)) {
    warned.add(name);
    console.warn(`[mk] unknown icon "${name}"; known: ${Object.keys(ICONS).join(", ")}`);
  }
  const d = Object.hasOwn(ICONS, name) ? ICONS[name] : ICONS.help;
  return `<svg${cls ? ` class="${cls.replace(/[^\w -]/g, "")}"` : ""} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${d}"/></svg>`;
}

/** The same icon as a DOM node (for code that builds elements). */
export function iconEl(name: string, cls = ""): Element {
  const t = document.createElement("template");
  t.innerHTML = icon(name, cls);
  return t.content.firstElementChild!;
}

export const iconNames = () => Object.keys(ICONS);
