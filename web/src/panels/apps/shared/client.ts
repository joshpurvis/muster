// The app side of the MCP Apps 2026-01-26 bridge, shared by Muster's built-in panel apps.
// Every call that reaches the agent (`sendEvent`, `openLink`) posts synchronously, so callers
// must invoke it straight from a click or key handler: the host only accepts it during a real
// user action.

type Json = Record<string, unknown>;

let sequence = 0;
const pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

function post(message: Json) {
  parent.postMessage({ jsonrpc: "2.0", ...message }, "*");
}

function request(method: string, params: Json): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const id = ++sequence;
    pending.set(id, { resolve, reject });
    post({ id, method, params });
  });
}

/** Send a user action to the agent as `kind`, with `text` for people and `data` for the agent. */
export function sendEvent(kind: string, text: string, data: object): Promise<void> {
  return request("ui/message", {
    role: "user",
    content: { type: "text", text },
    _meta: { muster: { kind, data } },
  }).then(() => {});
}

/** Send the user's own words to the agent as a plain message (the event kind is `ui/message`). */
export function sendText(text: string): Promise<void> {
  return request("ui/message", { role: "user", content: { type: "text", text } }).then(() => {});
}

/** Open an http(s) link in the system browser. */
export function openLink(url: string): Promise<void> {
  return request("ui/open-link", { url }).then(() => {});
}

function applyContext(ctx: unknown) {
  if (!ctx || typeof ctx !== "object") return;
  const c = ctx as Json;
  if (c.theme === "light" || c.theme === "dark") document.documentElement.dataset.theme = c.theme;
  // Hosts may pass the spec's standard CSS variables; take only well-formed custom properties.
  const vars = (c.styles as Json | undefined)?.variables;
  if (vars && typeof vars === "object") {
    for (const [k, v] of Object.entries(vars as Json)) {
      if (/^--[a-z0-9-]{1,60}$/.test(k) && typeof v === "string" && v.length < 300) {
        document.documentElement.style.setProperty(k, v);
      }
    }
  }
}

export interface AppOptions {
  name: string;
  /** Called with each new input (`tool-input` arguments): the first one and every update. */
  input: (data: unknown) => void;
  /** Called after the theme or other host context changed. */
  context?: () => void;
}

export function startApp(options: AppOptions) {
  // Until the host says which theme, follow the system's.
  if (!document.documentElement.dataset.theme) {
    document.documentElement.dataset.theme = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  addEventListener("message", (event) => {
    if (event.source !== parent || event.data?.jsonrpc !== "2.0") return;
    const msg = event.data;
    if (typeof msg.id === "number" && pending.has(msg.id) && !msg.method) {
      const p = pending.get(msg.id)!;
      pending.delete(msg.id);
      if (msg.error) p.reject(new Error(String(msg.error.message ?? "Request failed")));
      else p.resolve(msg.result);
      return;
    }
    if (msg.method === "ui/notifications/tool-input") options.input(msg.params?.arguments ?? {});
    else if (msg.method === "ui/notifications/host-context-changed") {
      applyContext(msg.params);
      options.context?.();
    } else if (msg.method === "ui/resource-teardown") post({ id: msg.id, result: {} });
  });
  request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: options.name, version: "1.0.0" },
    appCapabilities: { availableDisplayModes: ["inline", "fullscreen"] },
  }).then((result) => {
    applyContext((result as Json | undefined)?.hostContext);
    options.context?.();
    post({ method: "ui/notifications/initialized", params: {} });
    new ResizeObserver(() => {
      post({
        method: "ui/notifications/size-changed",
        params: { width: innerWidth, height: document.documentElement.scrollHeight },
      });
    }).observe(document.body);
  }).catch(() => {});
}

/** True when a key event is typing into a field, so single-key shortcuts must not fire. */
export function typing(event: KeyboardEvent): boolean {
  // A field inside a shadow root is retargeted to its host: look along the whole path.
  const path = typeof event.composedPath === "function" ? event.composedPath() : [event.target];
  return path.some((n) => {
    const t = n as HTMLElement | null;
    if (!t || typeof t.tagName !== "string") return false;
    if (t.isContentEditable || t.tagName === "TEXTAREA" || t.tagName === "SELECT") return true;
    return t.tagName === "INPUT" && !["checkbox", "radio", "button", "submit", "reset", "range", "color"].includes((t as HTMLInputElement).type);
  });
}
