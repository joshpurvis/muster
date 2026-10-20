// Entry point: wires rpc -> state -> terminals, renders the shell.
import "@xterm/xterm/css/xterm.css";
import "./styles.css";
import { render } from "preact";
import { effect } from "@preact/signals";
import { useEffect, useRef, useState } from "preact/hooks";
import { connStatus, rpc } from "./rpc.ts";
import {
  agentMessages,
  agents,
  agentsPanelOpen,
  fullscreen,
  applyEvent,
  applyHello,
  daemonStopped,
  focusedSession,
  focusedTabId,
  helloDone,
  hereIds,
  layout,
  meta,
  prefs,
  removedSessionListeners,
  sessions,
  panels,
  toast,
  updateLayout,
  windowId,
  windowLostListeners,
  windowPrefs,
  windowSessions,
  windowsSupported,
  workspaces,
} from "./state.ts";
import { canCloseWindow, isNative, setNativeDecorations } from "./native.ts";
import * as terms from "./terms.ts";
import * as A from "./actions.ts";
import { installKeys } from "./keys.ts";
import { notes } from "./notes.ts";
import { WindowControls } from "./components/WindowControls.tsx";
import { StatusLine } from "./components/StatusLine.tsx";
import { Overlays, Toasts } from "./components/Overlays.tsx";
import { MenuHost } from "./components/Menu.tsx";
import { AgentsPanel } from "./components/Agents.tsx";
import { Kbd, TooltipHost } from "./components/common.tsx";
import { FolderInput } from "./components/FolderInput.tsx";
import { SplitView } from "./layout/SplitView.tsx";
import { PanelLayer } from "./panels/PanelView.tsx";
import { DragOverlay } from "./layout/dnd.tsx";
import { LaunchButtons } from "./layout/Pane.tsx";
import { focusPane, paneOfSession } from "./layout/tree.ts";
import { windowParam } from "./model.ts";
import type { SessionKind } from "./types.ts";
import { tildify } from "./util.ts";

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

// ---------- which window this client shows ----------
// A launcher may ask for a window with `#window=<id>` (`muster open --window N`). It wins over
// the one this tab had; keep it in sessionStorage (a reload reclaims it) and drop it from the URL.
{
  const w = windowParam(location.hash);
  if (w !== null) {
    A.rememberWindow(w);
    const params = new URLSearchParams(location.hash.slice(1));
    params.delete("window");
    const rest = params.toString();
    history.replaceState(null, "", location.pathname + location.search + (rest ? "#" + rest : ""));
  }
}

// ---------- wiring ----------
terms.setFontSize(prefs.value.fontSize);
terms.setInputPaused((sid) => !!sessions.value.get(sid)?.input_interrupted);
terms.onClipboardFailed((text) => toast("error", text));

rpc.onHello = async (h, reconnect) => {
  applyHello(h);
  A.deliveriesNeedReview(h.paused_deliveries ?? []);
  if (reconnect) A.backendRestarted();
  // Re-claim on every hello: after a reconnect the daemon released our window.
  await A.claimWindow();
  if (reconnect) terms.reattachAll();
};
rpc.onEvent = applyEvent;

// Muster is shutting down (this window's "Shut down Muster", or another's): don't reconnect;
// the stopped screen says how to start it again.
effect(() => {
  if (daemonStopped.value) rpc.suspend();
});

// The system title bar follows the shared preference in every native window (new windows are
// built with it already).
effect(() => {
  const p = windowPrefs.value;
  if (p && isNative) setNativeDecorations(p.decorations);
});

windowLostListeners.push(() => A.claimWindow());
addEventListener("focus", () => A.windowFocused());

removedSessionListeners.push((id) => terms.dispose(id));
// Restarts need no client action: the daemon follows the session and sends a fresh replay.

// Only this window's tabs stream here: detach terminals whose tab moved to another window (they
// stay cached for a while in case it comes back).
effect(() => {
  const here = hereIds.value;
  const all = sessions.value;
  if (!helloDone.value || (windowsSupported.value && windowId.value === null)) return;
  for (const sid of terms.attachedSids()) {
    if (all.has(sid) && !here.has(sid)) terms.detach(sid);
  }
});

terms.onTerminalFocus((sid) => {
  const l = layout.value;
  const p = l ? paneOfSession(l.root, sid) : null;
  if (l && p && l.focus !== p.id) updateLayout((x) => focusPane(x, p.id));
});

// The focused tab closed or moved to another window while its terminal had focus: focus falls to
// <body>, so hand it to the tab that took its place (never steal it from a dialog or field).
effect(() => {
  void focusedTabId.value;
  const a = document.activeElement;
  if (!a || a === document.body) A.focusActiveTerminalSoon();
});

installKeys();
rpc.start();

// Read-only debug handle (used by dev/e2e.ts; handy in devtools).
(globalThis as Record<string, unknown>).__muster = {
  rpc,
  layout,
  sessions,
  panels,
  notes,
  windowSessions,
  windowId,
  focusedSession,
  agents,
  agentMessages,
  agentsPanelOpen,
  fullscreen,
  prefs,
  termText: (sid: number) => {
    const t = terms.terminal(sid);
    if (!t) return null;
    const b = t.buffer.active;
    const out: string[] = [];
    for (let i = 0; i < b.length; i++) out.push(b.getLine(i)?.translateToString(true) ?? "");
    return out.join("\n");
  },
  attached: () => terms.attachedSids(),
  /** The live (mutable) xterm instance of a session, for fixtures and latency probes. */
  term: (sid: number) => terms.terminal(sid),
};

// ---------- components ----------

const KINDS: SessionKind[] = ["claude", "codex", "shell"];

/** A window with no tabs: pick a folder and what to run (keys 1 2 3). The first tab starts a
 * group. */
function EmptyWindow() {
  const home = meta.value.home;
  const [folder, setFolder] = useState(() => tildify(A.defaultFolder(), home));
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const first = !workspaces.value.length;
  useEffect(() => {
    if (document.activeElement === document.body || !document.activeElement) ref.current?.focus();
  }, []);
  const pick = async (k: SessionKind) => {
    if (busy || !folder.trim()) return;
    setBusy(true);
    try {
      await A.createTab({ path: folder.trim(), agent: k, worktree: false });
    } catch (e) {
      toast("error", `Couldn't open a tab: ${e instanceof Error ? e.message : e}`);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div
      class="welcome empty"
      ref={ref}
      tabIndex={-1}
      onKeyDown={(e) => {
        if (e.altKey || e.ctrlKey || e.metaKey) return;
        if ((e.target as HTMLElement).closest("input")) return;
        const i = ["1", "2", "3"].indexOf(e.key);
        if (i >= 0) {
          e.preventDefault();
          pick(KINDS[i]);
        }
      }}
    >
      <div class="empty-controls">
        <WindowControls empty />
      </div>
      <div class="welcome-card empty-window">
        <div class="welcome-mark">m</div>
        <h1>{first ? "Welcome to muster" : "No tabs in this window"}</h1>
        <p>
          {first
            ? "Run coding agents and shells in tabs and split groups. Pick a folder and what to start there."
            : "Pick a folder and what to start there."}
        </p>
        <label class="field empty-folder">
          <span class="field-label">Folder</span>
          <FolderInput value={folder} onChange={setFolder} />
        </label>
        <LaunchButtons pick={pick} last={prefs.value.lastAgent} />
        <div class="welcome-keys">
          <span>
            <Kbd k="Ctrl+Space" /> jump
          </span>
          <span>
            <Kbd k="Alt+Shift+N" /> more options
          </span>
          <span>
            <Kbd k="Alt+/" /> shortcuts
          </span>
        </div>
      </div>
    </div>
  );
}

/** After `daemon_stopping`: replaces whatever had focus (maybe a terminal), so it takes the
 * focus and is announced as an alert dialog. */
function Stopped() {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => ref.current?.focus(), []);
  return (
    <div class="splash">
      <div
        ref={ref}
        class="welcome-card"
        role="alertdialog"
        aria-labelledby="stopped-title"
        aria-describedby="stopped-text"
        tabIndex={-1}
      >
        <h1 id="stopped-title">Muster is shut down</h1>
        <p id="stopped-text">
          Every session has ended. Run <code>muster</code> to start it again, then reconnect.
        </p>
        <div class="splash-actions">
          {canCloseWindow() && <button type="button" class="btn" onClick={() => A.closeWindow()}>Close window</button>}
          <button type="button" class="btn btn-primary" onClick={() => location.reload()}>Reconnect</button>
        </div>
      </div>
    </div>
  );
}

function Splash() {
  const s = connStatus.value;
  if (daemonStopped.value) return <Stopped />;
  if (s === "incompatible") {
    return <div class="splash"><div class="welcome-card"><h1>UI update required</h1>
      <p>Install a UI compatible with this backend, then refresh. Your terminals remain running.</p>
    </div></div>;
  }
  if (s === "no_token") {
    return (
      <div class="splash">
        <div class="welcome-card">
          <h1>Not signed in</h1>
          <p>
            This launch link has expired or was already used. Run <code>muster</code>{" "}
            again, or open the link printed by <code>muster url</code>. Access is kept for this tab only.
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
  const ready = helloDone.value && (!windowsSupported.value || windowId.value !== null);
  if (!ready || connStatus.value === "incompatible" || daemonStopped.value) {
    return (
      <>
        <Splash />
        <Toasts />
      </>
    );
  }
  const l = layout.value;
  return (
    <div class={fullscreen.value === null ? "app" : "app fullscreen"}>
      <div class="app-body">
        {agentsPanelOpen.value && <AgentsPanel />}
        <main class="content">
          {l && hereIds.value.size
            ? (
              <div class="panes">
                <SplitView layout={l} />
                <PanelLayer />
              </div>
            )
            : <EmptyWindow />}
        </main>
      </div>
      {prefs.value.statusLine && <StatusLine />}
      <Overlays />
      <MenuHost />
      <DragOverlay />
      <TooltipHost />
      <Toasts />
    </div>
  );
}

render(<App />, document.getElementById("app")!);
