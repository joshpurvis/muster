// Entry point: wires rpc -> state -> terminals, renders the shell.
import "@xterm/xterm/css/xterm.css";
import "./styles.css";
import { render } from "preact";
import { effect, signal } from "@preact/signals";
import { useRef } from "preact/hooks";
import { connStatus, rpc } from "./rpc.ts";
import {
  activeChannel,
  activeChannelId,
  applyEvent,
  applyHello,
  helloDone,
  layoutOf,
  layouts,
  overlay,
  prefs,
  removedSessionListeners,
  rightSuppressed,
  rightVisible,
  sessions,
  setPref,
  updateLayout,
  view,
  workspaces,
} from "./state.ts";
import * as terms from "./terms.ts";
import * as A from "./actions.ts";
import { installKeys } from "./keys.ts";
import { Topbar } from "./components/Topbar.tsx";
import { Sidebar } from "./components/Sidebar.tsx";
import { ChannelHeader } from "./components/ChannelHeader.tsx";
import { Composer } from "./components/Composer.tsx";
import { RightPanel } from "./components/RightPanel.tsx";
import { Inbox } from "./components/Inbox.tsx";
import { Overlays, Toasts } from "./components/Overlays.tsx";
import { MenuHost } from "./components/Menu.tsx";
import { TooltipHost } from "./components/common.tsx";
import { SplitView } from "./layout/SplitView.tsx";
import { DragOverlay } from "./layout/dnd.tsx";
import { focusPane, paneOfSession } from "./layout/tree.ts";
import { IconFolder, IconPlus } from "./icons.tsx";
import { clamp, cx } from "./util.ts";
import { Kbd } from "./components/common.tsx";

// ---------- theme ----------
const mq = matchMedia("(prefers-color-scheme: light)");
function applyThemePref() {
  const t = prefs.value.theme;
  const eff = t === "system" ? (mq.matches ? "light" : "dark") : t;
  document.documentElement.dataset.theme = eff;
  // Terminal colours come from CSS vars; recompute after the attribute lands.
  terms.applyTheme();
}
effect(applyThemePref);
mq.addEventListener("change", applyThemePref);

// ---------- wiring ----------
terms.setFontSize(prefs.value.fontSize);

rpc.onHello = (h, reconnect) => {
  applyHello(h);
  if (reconnect) terms.reattachAll();
};
rpc.onEvent = applyEvent;

removedSessionListeners.push((id) => terms.dispose(id));
// Restarts need no client action: the daemon follows the session and sends a fresh replay.

terms.onTerminalFocus((sid) => {
  const s = sessions.value.get(sid);
  if (!s) return;
  const l = layouts.value.get(s.channel_id);
  if (!l) return;
  const p = paneOfSession(l.root, sid);
  if (p && l.focus !== p.id) updateLayout(s.channel_id, (x) => focusPane(x, p.id));
});

// Ensure a layout exists for the active channel.
effect(() => {
  const ch = activeChannelId.value;
  if (ch !== null && helloDone.value && activeChannel.value) layoutOf(ch);
});

installKeys();
rpc.start();

// Read-only debug handle (used by dev/e2e.ts; handy in devtools).
(globalThis as Record<string, unknown>).__muster = {
  rpc,
  layouts,
  sessions,
  activeChannelId,
  termText: (sid: number) => {
    const t = terms.terminal(sid);
    if (!t) return null;
    const b = t.buffer.active;
    const out: string[] = [];
    for (let i = 0; i < b.length; i++) out.push(b.getLine(i)?.translateToString(true) ?? "");
    return out.join("\n");
  },
};

// ---------- window width: side panels yield to the center ----------

/** The center (terminals) keeps at least this much width; side panels shrink first. */
const CENTER_MIN = 480;
/** Below this viewport width the right panel closes itself. */
const RIGHT_AUTO_CLOSE = 1100;
const SIDEBAR_MIN = 180;
const RIGHT_MIN = 260;

const winW = signal(innerWidth);
let lastW = 0; // 0 = first run: a narrow window at startup also closes the panel
function onWinResize() {
  const w = innerWidth;
  winW.value = w;
  // Crossing the threshold hides/restores the panel without touching the saved preference.
  if (w < RIGHT_AUTO_CLOSE && (lastW === 0 || lastW >= RIGHT_AUTO_CLOSE)) rightSuppressed.value = true;
  if (w >= RIGHT_AUTO_CLOSE && (lastW === 0 || lastW < RIGHT_AUTO_CLOSE)) rightSuppressed.value = false;
  lastW = w;
}
addEventListener("resize", onWinResize);
onWinResize();

/** Effective side panel widths for the current window (prefs are the user's wish). */
function panelWidths(showSidebar: boolean, showRight: boolean) {
  const p = prefs.value;
  const room = winW.value - CENTER_MIN - 2; // 2 = the two 1px handles
  let sw = showSidebar ? p.sidebarWidth : 0;
  let rw = showRight ? p.rightWidth : 0;
  if (sw + rw > room && showRight) rw = Math.max(RIGHT_MIN, room - sw);
  if (sw + rw > room && showSidebar) sw = Math.max(SIDEBAR_MIN, room - rw);
  return { sw, rw, room };
}

// ---------- components ----------

function ResizeHandle(
  { side, onDrag, onDone }: { side: "left" | "right"; onDrag: (dx: number) => void; onDone?: () => void },
) {
  return (
    <div
      class={cx("panel-handle", "handle-" + side)}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        const el = e.currentTarget as HTMLElement;
        el.setPointerCapture(e.pointerId);
        el.classList.add("dragging");
        document.body.classList.add("resizing-col");
        const sx = e.clientX;
        const move = (ev: PointerEvent) => onDrag(ev.clientX - sx);
        const up = () => {
          el.removeEventListener("pointermove", move);
          el.removeEventListener("pointerup", up);
          el.removeEventListener("pointercancel", up);
          el.removeEventListener("lostpointercapture", up);
          el.classList.remove("dragging");
          document.body.classList.remove("resizing-col");
          onDone?.();
        };
        el.addEventListener("pointermove", move);
        el.addEventListener("pointerup", up);
        el.addEventListener("pointercancel", up);
        el.addEventListener("lostpointercapture", up);
      }}
    />
  );
}

function ChannelView() {
  const c = activeChannel.value;
  if (!c) return <NoChannel />;
  const l = layouts.value.get(c.id);
  return (
    <div class="channel">
      <ChannelHeader c={c} />
      <div class="panes">{l ? <SplitView ch={c.id} layout={l} /> : null}</div>
      <Composer key={c.id} c={c} />
    </div>
  );
}

function NoChannel() {
  const ws = workspaces.value;
  if (!ws.length) {
    return (
      <div class="welcome">
        <div class="welcome-card">
          <div class="welcome-mark">m</div>
          <h1>Welcome to muster</h1>
          <p>
            Run coding agents and shells side by side. Each <b>channel</b>{" "}
            is a task with its own git worktree, terminals and timeline.
          </p>
          <button
            type="button"
            class="btn btn-primary btn-lg"
            onClick={() => (overlay.value = { kind: "add_workspace" })}
          >
            <IconFolder size={16} /> Add a workspace
          </button>
          <div class="welcome-keys">
            <span>
              <Kbd k="Ctrl+K" /> jump anywhere
            </span>
            <span>
              <Kbd k="Alt+/" /> all shortcuts
            </span>
          </div>
        </div>
      </div>
    );
  }
  return (
    <div class="welcome">
      <div class="welcome-card">
        <h1>No channel open</h1>
        <p>Create a channel to start an agent in its own worktree.</p>
        <button type="button" class="btn btn-primary btn-lg" onClick={() => A.newChannelDialog()}>
          <IconPlus size={16} /> New channel
        </button>
        <div class="welcome-keys">
          <span>
            <Kbd k="Alt+Shift+N" /> new channel
          </span>
          <span>
            <Kbd k="Ctrl+K" /> jump anywhere
          </span>
        </div>
      </div>
    </div>
  );
}

function Splash() {
  const s = connStatus.value;
  if (s === "no_token") {
    return (
      <div class="splash">
        <div class="welcome-card">
          <h1>Missing access token</h1>
          <p>
            Open the link printed by <code>muster url</code> (it ends in <code>#token=…</code>), or run{" "}
            <code>muster</code> to open the app. The token is kept for this tab only.
          </p>
        </div>
      </div>
    );
  }
  return (
    <div class="splash">
      <div class="splash-inner">
        <div class="welcome-mark pulse">m</div>
        <div class="muted">
          {s === "reconnecting" ? "Can't reach the muster daemon. Retrying…" : "Connecting…"}
        </div>
      </div>
    </div>
  );
}

function App() {
  const p = prefs.value;
  const startW = useRef(0);
  if (!helloDone.value) {
    return (
      <>
        <Splash />
        <Toasts />
      </>
    );
  }
  const c = activeChannel.value;
  const showRight = rightVisible.value && view.value === "channel" && !!c;
  const { sw, rw, room } = panelWidths(!p.sidebarCollapsed, showRight);
  return (
    <div class="app">
      <Topbar />
      <div class="main">
        {!p.sidebarCollapsed && (
          <>
            <div class="sidebar-wrap" style={{ width: sw + "px" }}>
              <Sidebar />
            </div>
            <ResizeHandle
              side="left"
              onDrag={(dx) => {
                if (!startW.current) startW.current = sw;
                const max = Math.max(SIDEBAR_MIN, Math.min(420, room - rw));
                setPref("sidebarWidth", clamp(startW.current + dx, SIDEBAR_MIN, max));
              }}
              onDone={() => (startW.current = 0)}
            />
          </>
        )}
        <main class="center">
          {view.value === "inbox" ? <Inbox /> : <ChannelView />}
        </main>
        {showRight && c && (
          <>
            <ResizeHandle
              side="right"
              onDrag={(dx) => {
                if (!startW.current) startW.current = rw;
                const max = Math.max(RIGHT_MIN, Math.min(720, innerWidth * 0.5, room - sw));
                setPref("rightWidth", clamp(startW.current - dx, RIGHT_MIN, max));
              }}
              onDone={() => (startW.current = 0)}
            />
            <div class="right-wrap" style={{ width: rw + "px" }}>
              <RightPanel c={c} />
            </div>
          </>
        )}
      </div>
      <Overlays />
      <MenuHost />
      <DragOverlay />
      <TooltipHost />
      <Toasts />
    </div>
  );
}

render(<App />, document.getElementById("app")!);
