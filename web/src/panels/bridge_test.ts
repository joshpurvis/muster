import { PanelBridge, type Host, type RpcMessage } from "./bridge.ts";
import type { PanelResource } from "../types.ts";

function check(value: unknown, message = "assertion failed") {
  if (!value) throw new Error(message);
}

function fixture() {
  const sent: RpcMessage[] = [];
  const calls: { method: string; params: Record<string, unknown> }[] = [];
  // `shown`: the host's own check (tab shown, frame focused); `verdict`: the sandbox proxy's.
  let shown = false;
  let verdict = false;
  const keys: unknown[] = [];
  const errors: string[] = [];
  let mode = "inline";
  const resource: PanelResource = {
    panel: { id: 7, channel_id: 2, session_id: 3, title: "Review", app: "markdown", csp: {}, window_id: 1,
      revision: 1, html_revision: 1, closed: false, created_at: 1, updated_at: 1 },
    html: "<h1>app</h1>", data: { markdown: "Hello" },
  };
  const host: Host = {
    send: (m) => sent.push(m),
    call: (method, params) => { calls.push({ method, params }); return Promise.resolve({ content: [] }); },
    context: () => ({ theme: "dark", displayMode: mode }),
    userActive: () => shown,
    openLink: (url) => { calls.push({ method: "link", params: { url } }); },
    displayMode: (next) => { mode = next; return next; },
    resized: () => {},
    keys: () => [{ ctrl: true, alt: false, shift: true, meta: false, code: "KeyP", editAlt: false }],
    key: (k) => keys.push(k),
    appError: (m) => errors.push(m),
  };
  const bridge = new PanelBridge(resource, host);
  let id = 0;
  const request = async (method: string, params: Record<string, unknown> = {}) => {
    const next = ++id;
    await bridge.receive({ jsonrpc: "2.0", id: next, method, params }, verdict);
    return sent.find((m) => m.id === next)!;
  };
  const init = async () => {
    await request("ui/initialize", { protocolVersion: "2026-01-26", appInfo: { name: "test", version: "1" }, appCapabilities: {} });
    await bridge.receive({ jsonrpc: "2.0", method: "ui/notifications/initialized" });
  };
  return {
    sent, calls, request, init, bridge, resource, keys, errors,
    activate: () => { shown = true; verdict = true; },
    set: (next: { shown?: boolean; verdict?: boolean }) => { shown = next.shown ?? shown; verdict = next.verdict ?? verdict; },
  };
}

Deno.test("panel bridge waits for initialized before sending input and result", async () => {
  const f = fixture();
  await f.request("ui/notifications/sandbox-proxy-ready");
  check(f.sent[0].method === "ui/notifications/sandbox-resource-ready");
  check(!f.sent.some((m) => m.method === "ui/notifications/tool-input"));
  const reply = await f.request("ui/initialize", { protocolVersion: "2026-01-26", appInfo: {}, appCapabilities: {} });
  check((reply.result as any).hostCapabilities.sandbox.csp.connectDomains.length === 0);
  check(!f.sent.some((m) => m.method === "ui/notifications/tool-input"));
  await f.bridge.receive({ jsonrpc: "2.0", method: "ui/notifications/initialized" });
  const last = f.sent.slice(-2);
  check(last[0].method === "ui/notifications/tool-input" && last[1].method === "ui/notifications/tool-result");
  check(last[0].params?.arguments === f.resource.data);
});

Deno.test("panel events require activation, text and user role", async () => {
  const f = fixture(); await f.init();
  const message = { role: "user", content: { type: "text", text: "Approve" } };
  check((await f.request("ui/message", message)).error?.code === -32000);
  check(f.calls.length === 0);
  f.activate();
  check((await f.request("ui/message", { ...message, role: "system" })).error?.code === -32602);
  check((await f.request("ui/message", message)).result !== undefined);
  check(f.calls.length === 1 && f.calls[0].method === "panel.event" && f.calls[0].params.panel_id === 7);
});

Deno.test("panel tools are allowlisted and unknown host methods fail closed", async () => {
  const f = fixture(); await f.init();
  for (const method of ["session.input", "resources/read", "sampling/createMessage", "ui/unknown"]) {
    check((await f.request(method)).error?.code === -32601);
  }
  check((await f.request("tools/call", { name: "send_message" })).error?.code === -32601);
  check(f.calls.length === 0);
  await f.request("tools/call", { name: "get_diff", arguments: {} });
  check(f.calls.length === 1 && f.calls[0].method === "panel.tool" && f.calls[0].params.panel_id === 7);
});

Deno.test("panel messages are rate limited even during one activation", async () => {
  const f = fixture(); await f.init(); f.activate();
  for (let i = 0; i < 30; i++) await f.request("ui/message", { role: "user", content: { type: "text", text: "click" } });
  check((await f.request("ui/message", { role: "user", content: { type: "text", text: "click" } })).error?.code === -32000);
  check(f.calls.length === 30);
});

Deno.test("panel context does not create an event; custom event kinds share the gesture gate", async () => {
  const f = fixture(); await f.init();
  await f.request("ui/update-model-context", { structuredContent: { selection: "hunk 2" } });
  check(f.calls[0].method === "panel.context");
  const message = { role: "user", content: { type: "text", text: "approve" }, _meta: { muster: { kind: "approve_hunk" } } };
  check((await f.request("ui/message", message)).error?.code === -32000);
  f.activate(); await f.request("ui/message", message);
  check(f.calls[1].params.kind === "approve_hunk");
});

Deno.test("panel links need a gesture and accept only safe URL schemes", async () => {
  const f = fixture(); await f.init();
  check(!!(await f.request("ui/open-link", { url: "https://example.com" })).error);
  f.activate();
  for (const url of ["javascript:alert(1)", "file:///etc/passwd", "data:text/html,hi", "https://user:pass@example.com"]) {
    check(!!(await f.request("ui/open-link", { url })).error);
  }
  await f.request("ui/open-link", { url: "https://example.com" });
  check(f.calls.length === 1 && f.calls[0].method === "link");
});

Deno.test("panel update, theme, fullscreen and teardown follow the lifecycle", async () => {
  const f = fixture(); await f.init();
  f.bridge.update({ ...f.resource, panel: { ...f.resource.panel, revision: 2 }, data: { markdown: "New" } });
  check((f.sent.at(-2)?.params?.arguments as any).markdown === "New");
  f.bridge.contextChanged();
  check(f.sent.at(-1)?.method === "ui/notifications/host-context-changed");
  check(((await f.request("ui/request-display-mode", { mode: "fullscreen" })).result as any).mode === "fullscreen");
  check(((await f.request("ui/request-display-mode", { mode: "pip" })).result as any).mode === "fullscreen");
  f.bridge.teardown("closed");
  check(f.sent.at(-1)?.method === "ui/resource-teardown");
  const count = f.sent.length;
  await f.request("ping");
  check(f.sent.length === count, "closed bridge is inert");
});

Deno.test("panel events need the proxy's activation verdict, not only a shown and focused frame", async () => {
  const f = fixture(); await f.init();
  const message = { role: "user", content: { type: "text", text: "Approve" } };
  // A click on the tab strip: the host is active and focuses the frame, the proxy saw nothing.
  f.set({ shown: true, verdict: false });
  check((await f.request("ui/message", message)).error?.message.includes("user action"));
  check(!!(await f.request("ui/open-link", { url: "https://example.com" })).error);
  // A real click inside a panel that is hidden (another tab of the group) still fails.
  f.set({ shown: false, verdict: true });
  check((await f.request("ui/message", message)).error?.code === -32000);
  check(f.calls.length === 0);
  f.set({ shown: true, verdict: true });
  check((await f.request("ui/message", message)).result !== undefined && f.calls.length === 1);
});

Deno.test("the sandbox hears Muster's chords; keys reach the host only while the panel has focus", async () => {
  const f = fixture();
  await f.request("ui/notifications/sandbox-proxy-ready");
  check((f.sent[0].params as any).keys[0].code === "KeyP", "chords travel with the resource");
  const key = { code: "KeyP", key: "P", ctrl: true, alt: false, shift: true, meta: false, inInput: false };
  await f.bridge.receive({ jsonrpc: "2.0", method: "muster/key", params: key });
  check(f.keys.length === 0, "not while hidden or unfocused");
  f.set({ shown: true });
  await f.bridge.receive({ jsonrpc: "2.0", method: "muster/key", params: key });
  check(f.keys.length === 1, "works before the app initializes");
  for (const bad of [{ ...key, ctrl: "yes" }, { ...key, code: "x".repeat(40) }, { code: "KeyP" }]) {
    await f.bridge.receive({ jsonrpc: "2.0", method: "muster/key", params: bad });
  }
  check(f.keys.length === 1, "malformed keys are dropped");
  await f.bridge.receive({ jsonrpc: "2.0", method: "muster/app-error", params: { message: "ReferenceError: x (line 3)" } });
  await f.bridge.receive({ jsonrpc: "2.0", method: "muster/app-error", params: { message: 42 } });
  check(f.errors.length === 1 && f.errors[0].startsWith("ReferenceError"));
  check(!f.bridge.ready);
  await f.init();
  check(f.bridge.ready);
});
