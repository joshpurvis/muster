// Panel apps live in one layer per window, positioned over their tab's slot in the layout.
// Moving a tab between groups, zooming or splitting only moves the slot: the app's frame
// stays in the document, so it keeps its state (a removed or re-parented iframe reloads).
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { signal } from "@preact/signals";
import { rpc } from "../rpc.ts";
import { focusedTabId, helloEpoch, layout, meta, panelOrigin, updateLayout, windowPanels } from "../state.ts";
import { focusActiveTerminalSoon, focusGroup } from "../actions.ts";
import { panelChords, runPanelKey } from "../keys.ts";
import { paneOfSession, panelTab } from "../layout/tree.ts";
import type { Panel, PanelResource } from "../types.ts";
import { ACTIVATION, PanelBridge } from "./bridge.ts";

const APP_NAMES: Record<string, string> = { markdown: "Markdown", image: "Image", chart: "Chart", table: "Table", pr_decision: "PR decision" };

export const panelReloads = signal<ReadonlyMap<number, number>>(new Map());
export function reloadPanel(id: number) {
  const next = new Map(panelReloads.value);
  next.set(id, (next.get(id) ?? 0) + 1);
  panelReloads.value = next;
}

/** An app that has not finished the MCP Apps handshake by then gets an error and Reload. */
export const LOAD_TIMEOUT_MS = 10_000;

const slots = new Map<number, HTMLElement>();
const slotsChanged = signal(0);

/** Where the active panel tab of a group shows (its app is drawn over it by PanelLayer). */
export function PanelSlot({ id }: { id: number }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current!;
    slots.set(id, el);
    slotsChanged.value++;
    return () => {
      if (slots.get(id) === el) slots.delete(id);
      slotsChanged.value++;
    };
  }, [id]);
  return <div class="panel-slot" ref={ref} data-panel-slot={id} />;
}

/** Every panel of this window, alive whichever group (if any) shows it. */
export function PanelLayer() {
  return (
    <div class="panel-layer">
      {windowPanels.value.map((p) => <PanelView key={p.id} panel={p} />)}
    </div>
  );
}

type Stage = "resource" | "proxy" | "app" | "init" | "ready";

const STALLED: Record<Exclude<Stage, "ready">, string> = {
  resource: "The panel's content did not arrive from Muster.",
  proxy: "The panel sandbox did not load: the panel server is unreachable or this window blocked it.",
  app: "The app did not start: it never sent ui/initialize.",
  init: "The app did not finish starting: it sent ui/initialize but not ui/notifications/initialized.",
};

function clip(r: DOMRect, by: DOMRect) {
  const left = Math.max(r.left, by.left), top = Math.max(r.top, by.top);
  return new DOMRect(left, top, Math.min(r.right, by.right) - left, Math.min(r.bottom, by.bottom) - top);
}

function PanelView({ panel }: { panel: Panel }) {
  const ref = useRef<HTMLDivElement>(null);
  const frameHost = useRef<HTMLDivElement>(null);
  const bridge = useRef<PanelBridge | null>(null);
  const iframe = useRef<HTMLIFrameElement | null>(null);
  const htmlRevision = useRef(-1);
  const applied = useRef(-1);
  const generation = useRef(0);
  const visible = useRef(false);
  const stageRef = useRef<Stage>("resource");
  const timer = useRef(0);
  // A failed resource fetch, and an app that stalled during its handshake.
  const [error, setError] = useState("");
  const [stall, setStall] = useState("");
  const [stage, setStageState] = useState<Stage>("resource");
  const [appError, setAppError] = useState("");
  const appErrorRef = useRef("");

  const tab = panelTab(panel.id);
  const l = layout.value;
  const pane = l ? paneOfSession(l.root, tab) : null;
  const paneId = useRef<string | null>(null);
  paneId.current = pane?.id ?? null;
  const active = !!l && !!pane && pane.active === tab && (!l.zoom || l.zoom === pane.id);
  const zoomed = !!pane && l?.zoom === pane.id;
  const split = !!l && l.root.type !== "pane" && !l.zoom;
  const dim = split && l.focus !== pane?.id;
  const activeRef = useRef(active);
  activeRef.current = active;
  void slotsChanged.value;
  const slot = slots.get(panel.id);

  let origin = "";
  try {
    const url = new URL(panelOrigin.value);
    if (url.protocol === "http:" && url.hostname === "127.0.0.1" && url.origin !== location.origin && url.port && url.port !== "0" && url.origin === panelOrigin.value) origin = url.origin;
  } catch { /* Older backends have no panel origin. */ }
  const reload = panelReloads.value.get(panel.id) ?? 0;
  const epoch = helloEpoch.value;

  const setStage = (next: Stage) => {
    stageRef.current = next;
    setStageState(next);
    if (next === "ready") {
      clearTimeout(timer.current);
      setStall("");
    }
  };

  /** Draw the app over its slot, or hide it. Focus inside a frame being hidden or removed
   * would fall to <body>; hand it to the tab that took its place. */
  const place = () => {
    const host = ref.current;
    const layer = host?.parentElement;
    if (!host || !layer) return;
    const el = slots.get(panel.id);
    let r: DOMRect | null = el?.isConnected && activeRef.current ? el.getBoundingClientRect() : null;
    // Split children clip panes squeezed below their floor; clip the app the same way.
    for (let a = el?.parentElement; r && a && a !== layer.parentElement; a = a.parentElement) {
      if (a.classList.contains("split-child") || a.classList.contains("split-root")) r = clip(r, a.getBoundingClientRect());
    }
    const show = !!r && r.width > 0 && r.height > 0;
    if (show) {
      const base = layer.getBoundingClientRect();
      host.style.left = `${r!.left - base.left}px`;
      host.style.top = `${r!.top - base.top}px`;
      host.style.width = `${r!.width}px`;
      host.style.height = `${r!.height}px`;
      host.dataset.placed = "";
    } else if (host.dataset.placed !== undefined) {
      const focused = !!iframe.current && document.activeElement === iframe.current;
      delete host.dataset.placed;
      if (focused) focusActiveTerminalSoon();
    }
    visible.current = show;
  };

  const context = () => ({
    theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
    displayMode: paneId.current !== null && layout.peek()?.zoom === paneId.current ? "fullscreen" : "inline",
    availableDisplayModes: ["inline", "fullscreen"],
    platform: "desktop",
    locale: navigator.language,
    userAgent: `Muster/${meta.peek().version}`,
    containerDimensions: { width: frameHost.current?.clientWidth ?? 0, height: frameHost.current?.clientHeight ?? 0 },
  });

  // Layout changes reach every group in the same render pass; measure once they are all in.
  useLayoutEffect(() => { queueMicrotask(place); });
  useEffect(() => {
    const resize = new ResizeObserver(() => place());
    if (slot) resize.observe(slot);
    if (ref.current?.parentElement) resize.observe(ref.current.parentElement);
    addEventListener("resize", place);
    return () => {
      resize.disconnect();
      removeEventListener("resize", place);
    };
  }, [slot]);

  useEffect(() => {
    // Fail closed if an older backend has no panel origin, or it isn't a separate loopback origin.
    if (!origin) {
      setError("The backend did not provide a separate panel origin.");
      return;
    }
    const receive = (event: MessageEvent) => {
      if (event.source !== iframe.current?.contentWindow || event.origin !== origin) return;
      const data = event.data;
      const b = bridge.current;
      if (!b) return;
      if (data?.method === "ui/notifications/sandbox-proxy-ready" && stageRef.current === "proxy") setStage("app");
      if (data?.method === "ui/initialize" && stageRef.current === "app") setStage("init");
      void b.receive(data, data?.[ACTIVATION] === true).then(() => {
        if (b === bridge.current && b.ready && stageRef.current !== "ready") setStage("ready");
      });
    };
    const focus = () => {
      if (document.activeElement === iframe.current && visible.current && paneId.current) focusGroup(paneId.current);
    };
    const observer = new MutationObserver(() => bridge.current?.contextChanged());
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    const resize = new ResizeObserver(() => bridge.current?.contextChanged());
    if (frameHost.current) resize.observe(frameHost.current);
    addEventListener("message", receive);
    addEventListener("blur", focus);
    return () => {
      generation.current++;
      clearTimeout(timer.current);
      bridge.current?.teardown("panel removed or reloaded");
      bridge.current = null;
      const focused = !!iframe.current && document.activeElement === iframe.current;
      iframe.current?.remove();
      iframe.current = null;
      if (focused) focusActiveTerminalSoon();
      htmlRevision.current = -1;
      applied.current = -1;
      removeEventListener("message", receive);
      removeEventListener("blur", focus);
      observer.disconnect();
      resize.disconnect();
    };
  }, [origin, panel.id, reload]);

  useEffect(() => {
    if (!origin) return;
    // A reconnect re-runs this: fetch again only if the current revision never got applied.
    if (bridge.current && applied.current === panel.revision) return;
    const run = ++generation.current;
    if (!bridge.current) {
      stageRef.current = "resource";
      setStageState("resource");
    }
    rpc.call<PanelResource>("panel.resource", { id: panel.id }).then((resource) => {
      if (run !== generation.current || !frameHost.current) return;
      setError("");
      if (bridge.current && htmlRevision.current === resource.panel.html_revision) {
        if (iframe.current) iframe.current.title = resource.panel.title;
        bridge.current.update(resource);
        applied.current = resource.panel.revision;
        return;
      }
      const hadFocus = !!iframe.current && document.activeElement === iframe.current;
      bridge.current?.teardown("panel HTML changed");
      iframe.current?.remove();
      clearTimeout(timer.current);
      appErrorRef.current = "";
      setAppError("");
      setStall("");
      setStage("proxy");
      const frame = document.createElement("iframe");
      frame.className = "panel-frame";
      frame.title = resource.panel.title;
      frame.dataset.panelId = String(panel.id);
      // Names the frame for devtools and tests; sandboxed apps cannot target other frames.
      frame.name = `muster-panel-${panel.id}`;
      frame.sandbox.value = "allow-scripts allow-same-origin";
      frame.referrerPolicy = "no-referrer";
      frame.src = `${origin}/proxy?csp=${encodeURIComponent(JSON.stringify(resource.panel.csp))}`;
      frame.addEventListener("focus", () => { if (paneId.current) focusGroup(paneId.current); });
      iframe.current = frame;
      htmlRevision.current = resource.panel.html_revision;
      applied.current = resource.panel.revision;
      bridge.current = new PanelBridge(resource, {
        send: (msg) => frame.contentWindow?.postMessage(msg, origin),
        call: (method, params) => rpc.call(method, params),
        context,
        userActive: () => visible.current && frame.getClientRects().length > 0 && document.activeElement === frame,
        openLink: (url) => { globalThis.open(url, "_blank", "noopener,noreferrer"); },
        displayMode: (mode) => {
          const id = paneId.current;
          if (id) updateLayout((l) => ({ ...l, focus: id, zoom: mode === "fullscreen" ? id : null }));
          return mode;
        },
        // Tabs fill their group; app-requested sizes cannot cover the host's controls.
        resized: () => {},
        keys: panelChords,
        key: (k) => { if (focusedTabId.peek() === tab) runPanelKey(tab, k); },
        appError: (message) => {
          appErrorRef.current = message;
          setAppError(message);
        },
      });
      timer.current = setTimeout(() => {
        if (run !== generation.current || stageRef.current === "ready") return;
        const stalled = STALLED[stageRef.current];
        setStall(appErrorRef.current ? `${stalled} The app reported: ${appErrorRef.current}` : stalled);
      }, LOAD_TIMEOUT_MS);
      frameHost.current.append(frame);
      if (hadFocus) frame.focus();
    }).catch((e) => {
      if (run === generation.current) setError(`Could not load the panel: ${String(e.message ?? e)}`);
    });
  }, [panel.id, panel.revision, origin, reload, epoch]);

  useLayoutEffect(() => { bridge.current?.contextChanged(); }, [zoomed, active]);
  return (
    <div
      ref={ref}
      class={dim ? "panel-view dim" : split ? "panel-view ring" : "panel-view"}
      data-panel-view={panel.id}
      onPointerDown={() => { if (paneId.current) focusGroup(paneId.current); }}
    >
      <div class="panel-sandbox-label">Panel · {APP_NAMES[panel.app] ?? "Custom app"}</div>
      <div class="panel-body">
        <div class="panel-frame-host" ref={frameHost} />
        {error || stall
        ? (
          <div class="panel-status" role="alert">
            <p>{error || stall}</p>
            <button type="button" class="btn" onClick={() => reloadPanel(panel.id)}>Reload panel</button>
          </div>
        )
        : stage !== "ready" && (
          <div class="panel-status" role="status">
            <p>Loading panel…</p>
            {appError && <p class="panel-status-detail">The app reported: {appError}</p>}
          </div>
        )}
      </div>
    </div>
  );
}
