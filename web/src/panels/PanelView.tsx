import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { signal } from "@preact/signals";
import { rpc } from "../rpc.ts";
import { layout, meta, panelOrigin, updateLayout } from "../state.ts";
import { focusGroup } from "../actions.ts";
import type { Panel, PanelResource } from "../types.ts";
import { PanelBridge } from "./bridge.ts";

export const panelReloads = signal<ReadonlyMap<number, number>>(new Map());
export function reloadPanel(id: number) {
  const next = new Map(panelReloads.value);
  next.set(id, (next.get(id) ?? 0) + 1);
  panelReloads.value = next;
}

export function PanelView({ panel, paneId, active }: { panel: Panel; paneId: string; active: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const bridge = useRef<PanelBridge | null>(null);
  const iframe = useRef<HTMLIFrameElement | null>(null);
  const htmlRevision = useRef(-1);
  const generation = useRef(0);
  const visible = useRef(active);
  visible.current = active;
  const [error, setError] = useState("");
  const [ready, setReady] = useState(false);
  let origin = "";
  try {
    const url = new URL(panelOrigin.value);
    if (url.protocol === "http:" && url.hostname === "127.0.0.1" && url.origin !== location.origin && url.port && url.port !== "0" && url.origin === panelOrigin.value) origin = url.origin;
  } catch { /* Older backends have no panel origin. */ }
  const reload = panelReloads.value.get(panel.id) ?? 0;
  const zoomed = layout.value?.zoom === paneId;

  const context = () => ({
    theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
    displayMode: layout.peek()?.zoom === paneId ? "fullscreen" : "inline",
    availableDisplayModes: ["inline", "fullscreen"],
    platform: "desktop",
    locale: navigator.language,
    userAgent: `Muster/${meta.peek().version}`,
    containerDimensions: { width: ref.current?.clientWidth ?? 0, height: ref.current?.clientHeight ?? 0 },
  });

  useEffect(() => {
    // Fail closed if an older backend has no panel origin, or it isn't a separate loopback origin.
    if (!origin) {
      setError("The backend did not provide a separate panel origin.");
      return;
    }
    const receive = (event: MessageEvent) => {
      if (event.source !== iframe.current?.contentWindow || event.origin !== origin) return;
      if (event.data?.method === "ui/notifications/initialized") setReady(true);
      void bridge.current?.receive(event.data);
    };
    const focus = () => {
      if (document.activeElement === iframe.current && visible.current) focusGroup(paneId);
    };
    const observer = new MutationObserver(() => bridge.current?.contextChanged());
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    const resize = new ResizeObserver(() => bridge.current?.contextChanged());
    if (ref.current) resize.observe(ref.current);
    addEventListener("message", receive);
    addEventListener("blur", focus);
    return () => {
      generation.current++;
      bridge.current?.teardown("panel removed or moved");
      bridge.current = null;
      iframe.current?.remove();
      iframe.current = null;
      htmlRevision.current = -1;
      removeEventListener("message", receive);
      removeEventListener("blur", focus);
      observer.disconnect();
      resize.disconnect();
    };
  }, [origin, panel.id, paneId, reload]);

  useEffect(() => {
    if (!origin) return;
    const run = ++generation.current;
    setError("");
    rpc.call<PanelResource>("panel.resource", { id: panel.id }).then((resource) => {
      if (run !== generation.current || !ref.current) return;
      if (bridge.current && htmlRevision.current === resource.panel.html_revision) {
        if (iframe.current) iframe.current.title = resource.panel.title;
        bridge.current.update(resource);
        return;
      }
      bridge.current?.teardown("panel HTML changed");
      iframe.current?.remove();
      setReady(false);
      const frame = document.createElement("iframe");
      frame.className = "panel-frame";
      frame.title = panel.title;
      frame.dataset.panelId = String(panel.id);
      frame.sandbox.value = "allow-scripts allow-same-origin";
      frame.referrerPolicy = "no-referrer";
      frame.src = `${origin}/proxy?csp=${encodeURIComponent(JSON.stringify(resource.panel.csp))}`;
      frame.addEventListener("focus", () => focusGroup(paneId));
      iframe.current = frame;
      htmlRevision.current = resource.panel.html_revision;
      bridge.current = new PanelBridge(resource, {
        send: (msg) => frame.contentWindow?.postMessage(msg, origin),
        call: (method, params) => rpc.call(method, params),
        context,
        userActive: () => visible.current && frame.getClientRects().length > 0 && document.activeElement === frame && navigator.userActivation?.isActive === true,
        openLink: (url) => { globalThis.open(url, "_blank", "noopener,noreferrer"); },
        displayMode: (mode) => {
          updateLayout((l) => ({ ...l, focus: paneId, zoom: mode === "fullscreen" ? paneId : null }));
          return mode;
        },
        // Tabs fill their group; app-requested sizes cannot cover the host's controls.
        resized: () => {},
      });
      ref.current.append(frame);
    }).catch((e) => { if (run === generation.current) setError(String(e.message ?? e)); });
  }, [panel.id, panel.revision, origin, reload]);

  useLayoutEffect(() => { bridge.current?.contextChanged(); }, [zoomed, active]);
  return (
    <div class="panel-view" hidden={!active} data-panel-view={panel.id}>
      <div class="panel-sandbox-label">Panel · {panel.app === "markdown" ? "Markdown" : "Custom app"}</div>
      <div class="panel-frame-host" ref={ref} />
      {error && <div class="panel-status" role="alert">{error}<button type="button" onClick={() => reloadPanel(panel.id)}>Reload panel</button></div>}
      {!error && !ready && <div class="panel-status" role="status">Loading panel…</div>}
    </div>
  );
}
