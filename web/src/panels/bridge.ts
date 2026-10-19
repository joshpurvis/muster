// MCP Apps 2026-01-26. This is the entire app-visible surface; daemon RPC stays behind it.
import type { PanelResource } from "../types.ts";

type ObjectValue = Record<string, unknown>;
export interface RpcMessage {
  jsonrpc: "2.0";
  id?: number | string;
  method?: string;
  params?: ObjectValue;
  result?: unknown;
  error?: { code: number; message: string };
}

export interface Host {
  send: (msg: RpcMessage) => void;
  call: (method: string, params: ObjectValue) => Promise<unknown>;
  context: () => ObjectValue;
  /** The panel's frame can take a user's action now (shown, focused). Read synchronously while
   * handling the postMessage, before any await; the sandbox proxy's activation verdict is
   * passed to `receive` separately. */
  userActive: () => boolean;
  openLink: (url: string) => void;
  /** Sandbox keys the host should hear (Muster's shortcuts), and one the user pressed. */
  keys: () => KeyChord[];
  key: (key: PanelKey) => void;
  /** The app reported a script, load or content-policy error. */
  appError: (message: string) => void;
  displayMode: (mode: "inline" | "fullscreen") => "inline" | "fullscreen";
  resized: (size: { width: number; height: number }) => void;
}

export interface KeyChord { ctrl: boolean; alt: boolean; shift: boolean; meta: boolean; code: string; editAlt: boolean }
export interface PanelKey { code: string; key: string; ctrl: boolean; alt: boolean; shift: boolean; meta: boolean; inInput: boolean }

/** Set only by the sandbox proxy (web/panels/proxy.html) on messages it forwards from the app. */
export const ACTIVATION = "muster_user_activation";

const object = (v: unknown): v is ObjectValue => !!v && typeof v === "object" && !Array.isArray(v);
const requestId = (v: unknown): v is string | number => typeof v === "string" || (typeof v === "number" && Number.isFinite(v));

class Failure extends Error {
  constructor(public code: number, text: string) { super(text); }
}

function requireParams(ok: unknown, text: string): asserts ok {
  if (!ok) throw new Failure(-32602, text);
}

export class PanelBridge {
  private initialized = false;
  private initializing = false;
  private disposed = false;
  private resource: PanelResource;
  private rates = new Map<string, number[]>();

  constructor(resource: PanelResource, private host: Host) { this.resource = resource; }

  /** The app finished the MCP Apps handshake. */
  get ready() { return this.initialized && !this.disposed; }

  private notify(method: string, params: ObjectValue) {
    if (!this.disposed) this.host.send({ jsonrpc: "2.0", method, params });
  }

  private limit(kind: string, maximum: number) {
    const now = Date.now();
    const calls = (this.rates.get(kind) ?? []).filter((t) => t > now - 60_000);
    if (calls.length >= maximum) throw new Failure(-32000, "Panel rate limit; try again shortly");
    calls.push(now);
    this.rates.set(kind, calls);
  }

  private input() {
    this.notify("ui/notifications/tool-input", { arguments: this.resource.data });
    this.notify("ui/notifications/tool-result", {
      content: [{ type: "text", text: `Panel ${this.resource.panel.title}` }],
      structuredContent: { panel_id: this.resource.panel.id, revision: this.resource.panel.revision },
    });
  }

  update(resource: PanelResource) {
    if (resource.panel.html_revision !== this.resource.panel.html_revision) throw new Error("HTML update needs a new frame");
    if (resource.panel.revision < this.resource.panel.revision) return;
    const changed = resource.panel.revision !== this.resource.panel.revision;
    this.resource = resource;
    if (this.initialized && changed) this.input();
  }

  contextChanged() {
    if (this.initialized) this.notify("ui/notifications/host-context-changed", this.host.context());
  }

  teardown(reason: string) {
    if (this.initialized && !this.disposed) {
      this.host.send({ jsonrpc: "2.0", id: "muster:teardown", method: "ui/resource-teardown", params: { reason } });
    }
    this.disposed = true;
  }

  /** `activated`: the sandbox proxy saw a fresh user action inside this panel's app and the
   * message is within that action's small budget (see proxy.html). */
  async receive(raw: unknown, activated = false) {
    if (this.disposed || !object(raw)) return;
    const id = requestId(raw.id) ? raw.id : undefined;
    const reply = (result: unknown) => {
      if (id !== undefined && !this.disposed) this.host.send({ jsonrpc: "2.0", id, result });
    };
    try {
      if (raw.jsonrpc !== "2.0" || (raw.id !== undefined && id === undefined)) throw new Failure(-32600, "Invalid JSON-RPC message");
      // Replies to host teardown have no method and need no further action.
      if (typeof raw.method !== "string") {
        if (id !== undefined && ("result" in raw || "error" in raw)) return;
        throw new Failure(-32600, "Missing method");
      }
      requireParams(raw.params === undefined || object(raw.params), "params must be an object");
      const p = (raw.params ?? {}) as ObjectValue;
      requireParams(JSON.stringify(p).length <= 32 * 1024, "App request is too large");
      const panel_id = this.resource.panel.id;
      if (raw.method === "ui/notifications/sandbox-proxy-ready") {
        this.notify("ui/notifications/sandbox-resource-ready", {
          html: this.resource.html, csp: this.resource.panel.csp, sandbox: "allow-scripts", keys: this.host.keys(),
        });
        return;
      }
      // Muster's own notifications from the sandbox helper; they work before initialize too.
      if (raw.method === "muster/key") {
        const flag = (v: unknown) => typeof v === "boolean";
        if (!(typeof p.code === "string" && typeof p.key === "string" && p.code.length <= 20 && p.key.length <= 20 &&
          flag(p.ctrl) && flag(p.alt) && flag(p.shift) && flag(p.meta) && flag(p.inInput))) return;
        if (!this.host.userActive()) return;
        this.limit("key", 240);
        this.host.key(p as unknown as PanelKey);
        return;
      }
      if (raw.method === "muster/app-error") {
        if (typeof p.message === "string" && p.message.trim()) {
          this.limit("app-error", 20);
          this.host.appError(p.message.slice(0, 300));
        }
        return;
      }
      if (raw.method === "ui/initialize") {
        requireParams(id !== undefined && p.protocolVersion === "2026-01-26" && object(p.appInfo) && object(p.appCapabilities), "Expected MCP Apps 2026-01-26 initialize");
        if (this.initializing) throw new Failure(-32600, "Already initialized");
        this.initializing = true;
        reply({
          protocolVersion: "2026-01-26", hostInfo: { name: "Muster", version: "0.1.0" },
          hostCapabilities: {
            openLinks: {}, serverTools: {},
            sandbox: { permissions: {}, csp: { resourceDomains: this.resource.panel.csp.resourceDomains ?? [], connectDomains: [], frameDomains: [], baseUriDomains: [] } },
          },
          hostContext: this.host.context(),
        });
        return;
      }
      if (raw.method === "ui/notifications/initialized") {
        if (!this.initializing || this.initialized) throw new Failure(-32600, "Unexpected initialized notification");
        this.initialized = true;
        this.input();
        return;
      }
      if (raw.method === "ping") { reply({}); return; }
      if (!this.initialized) throw new Failure(-32000, "Initialize the app first");
      switch (raw.method) {
        case "ui/notifications/size-changed":
          requireParams(typeof p.width === "number" && typeof p.height === "number" && Number.isFinite(p.width) && Number.isFinite(p.height) && p.width >= 0 && p.height >= 0, "Invalid app size");
          this.host.resized({ width: Math.min(p.width, 100_000), height: Math.min(p.height, 100_000) });
          return;
        case "ui/open-link": {
          if (!activated || !this.host.userActive()) throw new Failure(-32000, "A user action is required to open a link");
          requireParams(typeof p.url === "string", "url must be a string");
          let url: URL;
          try { url = new URL(p.url); } catch { throw new Failure(-32602, "Invalid URL"); }
          requireParams(["http:", "https:"].includes(url.protocol) && !url.username && !url.password, "Only http(s) links are allowed");
          this.limit("link", 30);
          this.host.openLink(url.href);
          reply({});
          return;
        }
        case "ui/message": {
          if (!activated || !this.host.userActive()) throw new Failure(-32000, "A real user action in this panel is required");
          requireParams(p.role === "user" && object(p.content) && p.content.type === "text" && typeof p.content.text === "string" && p.content.text.trim(), "Expected a user text message");
          let kind = "ui/message";
          if (object(p._meta) && object(p._meta.muster) && p._meta.muster.kind !== undefined) {
            requireParams(typeof p._meta.muster.kind === "string" && /^[a-zA-Z][\w/-]{0,79}$/.test(p._meta.muster.kind) && !["closed", "overflow"].includes(p._meta.muster.kind), "Invalid event kind");
            kind = p._meta.muster.kind;
          }
          this.limit("event", 30);
          await this.host.call("panel.event", { panel_id, kind, payload: p });
          reply({});
          return;
        }
        case "tools/call": {
          if (p.name !== "get_diff") throw new Failure(-32601, "Tool is not app-visible");
          requireParams(p.arguments === undefined || object(p.arguments), "arguments must be an object");
          this.limit("tool", 30);
          reply(await this.host.call("panel.tool", { panel_id, name: "get_diff", arguments: p.arguments ?? {} }));
          return;
        }
        case "ui/update-model-context":
          requireParams((p.content === undefined || Array.isArray(p.content)) && (p.structuredContent === undefined || object(p.structuredContent)), "Invalid model context");
          this.limit("context", 60);
          await this.host.call("panel.context", { panel_id, context: p });
          reply({});
          return;
        case "ui/request-display-mode": {
          requireParams(["inline", "fullscreen", "pip"].includes(String(p.mode)), "Invalid display mode");
          const mode = p.mode === "pip" ? this.host.context().displayMode : this.host.displayMode(p.mode as "inline" | "fullscreen");
          reply({ mode });
          this.contextChanged();
          return;
        }
        default: throw new Failure(-32601, "Method is not supported by this host");
      }
    } catch (error) {
      if (id !== undefined && !this.disposed) {
        this.host.send({ jsonrpc: "2.0", id, error: {
          code: error instanceof Failure ? error.code : -32000,
          message: error instanceof Error ? error.message : String(error),
        } });
      }
    }
  }
}
