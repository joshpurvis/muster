// Panel apps live in one layer per window, positioned over their tab's slot in the layout
// (placement.ts). A web preview is drawn by WebPreview.tsx instead of the sandbox proxy.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { channels, focusedTabId, helloEpoch, layout, meta, sessions, updateLayout, windowPanels } from "../state.ts";
import { closeTab, focusActiveTerminalSoon, focusGroup } from "../actions.ts";
import { panelTab } from "../layout/tree.ts";
import { ago, APP_KIND, KitIcon } from "./KitIcon.tsx";
import { panelChords, runPanelKey } from "../keys.ts";
import type { Panel, PanelResource } from "../types.ts";
import { ACTIVATION, PanelBridge } from "./bridge.ts";
import { panelOriginUrl, panelReloads, reloadPanel, slots, slotsChanged, usePanelPlacement } from "./placement.ts";
import { WebPreview } from "./WebPreview.tsx";

export { reloadPanel };

/** An app that has not finished the MCP Apps handshake by then gets an error and Reload. */
export const LOAD_TIMEOUT_MS = 10_000;

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
      {windowPanels.value.map((p) => p.app === "web_preview" ? <WebPreview key={p.id} panel={p} /> : <PanelView key={p.id} panel={p} />)}
    </div>
  );
}

type Stage = "resource" | "proxy" | "app" | "init" | "ready";

/** Where a panel stalled, as the error's evidence line. */
const STAGE_STEP: Record<Exclude<Stage, "ready">, string> = {
  resource: "panel.resource", proxy: "the sandbox proxy", app: "ui/initialize", init: "ui/notifications/initialized",
};
/** The same, in plain words for the error's sentence (STALLED below carries the technical one too). */
const STALLED_TEXT: Record<Exclude<Stage, "ready">, string> = {
  resource: "Muster could not hand this panel its content.",
  proxy: "The panel's sandbox did not load, so it can't show anything.",
  app: "The agent's page never answered Muster's hello, so it can't show anything.",
  init: "The agent's page answered Muster's hello but never said it was ready.",
};

const STALLED: Record<Exclude<Stage, "ready">, string> = {
  resource: "The panel's content did not arrive from Muster.",
  proxy: "The panel sandbox did not load: the panel server is unreachable or this window blocked it.",
  app: "The app did not start: it never sent ui/initialize.",
  init: "The app did not finish starting: it sent ui/initialize but not ui/notifications/initialized.",
};

function PanelView({ panel }: { panel: Panel }) {
  const frameHost = useRef<HTMLDivElement>(null);
  const bridge = useRef<PanelBridge | null>(null);
  const iframe = useRef<HTMLIFrameElement | null>(null);
  const htmlRevision = useRef(-1);
  const applied = useRef(-1);
  const generation = useRef(0);
  const { ref, visible, paneId, tab, active, zoomed, split, dim } = usePanelPlacement(panel.id, iframe);
  const stageRef = useRef<Stage>("resource");
  const stallAt = useRef<Exclude<Stage, "ready">>("resource");
  const timer = useRef(0);
  // A failed resource fetch, and an app that stalled during its handshake.
  const [error, setError] = useState("");
  const [stall, setStall] = useState("");
  const [stage, setStageState] = useState<Stage>("resource");
  const [appError, setAppError] = useState("");
  const appErrorRef = useRef("");

  const origin = panelOriginUrl();
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

  const context = () => ({
    theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
    displayMode: paneId.current !== null && layout.peek()?.zoom === paneId.current ? "fullscreen" : "inline",
    availableDisplayModes: ["inline", "fullscreen"],
    platform: "desktop",
    locale: navigator.language,
    userAgent: `Muster/${meta.peek().version}`,
    containerDimensions: { width: frameHost.current?.clientWidth ?? 0, height: frameHost.current?.clientHeight ?? 0 },
  });

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
        stallAt.current = stageRef.current as Exclude<Stage, "ready">;
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
      // Host controls (Reload panel) select the group when tabbed to, as the app frame does.
      onFocusCapture={() => { if (paneId.current) focusGroup(paneId.current); }}
    >
      <PanelStrip panel={panel} state={error || stall ? "err" : stage !== "ready" ? "loading" : "live"} />
      <div class="panel-body">
        <div class="panel-frame-host" ref={frameHost} />
        {error || stall
        ? (
          <div class="panel-status failed" role="alert">
            <div class="ps-card">
              <span class="ps-orb bad"><KitIcon name="alert" size={20} /></span>
              <h3>{error ? "This panel could not be loaded" : "This panel didn't finish starting"}</h3>
              <p>{error ? "Muster could not load this panel. Reload tries again." : STALLED_TEXT[stallAt.current]}</p>
              <pre class="panel-status-detail">{error ? error : [
                `Stalled at: ${STAGE_STEP[stallAt.current]}`,
                stall,
                `Waited: ${LOAD_TIMEOUT_MS / 1000} s · revision ${panel.revision}`,
              ].join("\n")}</pre>
              <div class="ps-acts">
                <button type="button" class="btn btn-primary" onClick={() => reloadPanel(panel.id)}><KitIcon name="refresh" /> Reload</button>
                <button type="button" class="btn btn-ghost" onClick={() => closeTab(panelTab(panel.id))}>Close panel</button>
              </div>
            </div>
          </div>
        )
        : stage !== "ready" && (
          <div class="panel-status loading" role="status">
            <Skeleton app={panel.app} />
            <p class="ps-loading">Loading panel…</p>
            {appError && <p class="panel-status-detail">The app reported: {appError}</p>}
          </div>
        )}
      </div>
    </div>
  );
}

/** The strip above a panel: what it is, who opened it, how fresh it is, and that it is sandboxed. */
function PanelStrip({ panel, state }: { panel: Panel; state: "live" | "loading" | "err" }) {
  const [, tick] = useState(0);
  useEffect(() => {
    // "updated 14 s ago" keeps itself current.
    const t = setInterval(() => tick((n) => n + 1), 15_000);
    return () => clearInterval(t);
  }, []);
  const [icon, name] = APP_KIND[panel.app] ?? APP_KIND.custom;
  const s = sessions.value.get(panel.session_id);
  const ch = channels.value.find((c) => c.id === panel.channel_id);
  const who = s ? (ch && ch.name !== s.name ? `${s.name} in ${ch.name}` : s.name) : ch?.name;
  const live = state === "err" ? "stopped starting" : state === "loading" ? "loading…" : `updated ${ago(panel.updated_at)}`;
  return (
    <div class="panel-strip panel-sandbox-label">
      <span class="ps-kind"><KitIcon name={icon} />{name}</span>
      {who && <span class="ps-sep" aria-hidden="true">·</span>}
      {who && <span class="ps-who">opened by <b>{who}</b></span>}
      <span class="ps-r">
        <span class={`ps-live ${state}`}>{live}</span>
        <span class="ps-lock" title="Runs in a sandbox on its own origin, with no network"><KitIcon name="lock" size={12} />sandboxed</span>
      </span>
    </div>
  );
}

/** A skeleton of the panel the app will become, while it loads. */
function Skeleton({ app }: { app: string }) {
  const bars = app === "chart" || app === "board";
  return (
    <div class="ps-skel" aria-hidden="true">
      <div class="skel" style="height:12px;width:34%" />
      <div class="skel" style="height:22px;width:62%" />
      <div class="skel" style="height:12px;width:84%" />
      {bars
        ? <div class="skel-chart">{[46, 70, 40, 82, 100, 22, 14].map((h, i) => <div key={i} class="skel" style={`height:${h}%`} />)}</div>
        : <div class="skel-rows">{[1, 2, 3, 4, 5].map((i) => <div key={i} class="skel" style="height:14px" />)}</div>}
    </div>
  );
}
