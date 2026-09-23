// Entry point: wires rpc -> state -> terminals, renders the shell.
import "@xterm/xterm/css/xterm.css";
import "./styles.css";
import { render } from "preact";
import { effect } from "@preact/signals";
import { useEffect, useRef, useState } from "preact/hooks";
import { connStatus, rpc } from "./rpc.ts";
import {
  activeChannel,
  applyEvent,
  applyHello,
  helloDone,
  layoutOf,
  layouts,
  meta,
  prefs,
  removedSessionListeners,
  sessions,
  tabIdSet,
  tabs,
  toast,
  updateLayout,
  windowId,
  windowLostListeners,
  windowsSupported,
  workspaces,
} from "./state.ts";
import * as terms from "./terms.ts";
import * as A from "./actions.ts";
import { installKeys } from "./keys.ts";
import { TabBar, TabDragOverlay } from "./components/TabBar.tsx";
import { StatusLine } from "./components/StatusLine.tsx";
import { Overlays, Toasts } from "./components/Overlays.tsx";
import { MenuHost } from "./components/Menu.tsx";
import { Kbd, TooltipHost } from "./components/common.tsx";
import { FolderInput } from "./components/FolderInput.tsx";
import { SplitView } from "./layout/SplitView.tsx";
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

rpc.onHello = async (h, reconnect) => {
  applyHello(h);
  // Re-claim on every hello: after a reconnect the daemon released our window.
  await A.claimWindow();
  if (reconnect) terms.reattachAll();
};
rpc.onEvent = applyEvent;

windowLostListeners.push(() => A.claimWindow());
addEventListener("focus", () => A.windowFocused());

removedSessionListeners.push((id) => terms.dispose(id));
// Restarts need no client action: the daemon follows the session and sends a fresh replay.

// Only sessions of this window's tabs stream here: detach terminals whose tab moved away (they
// stay cached for a while in case it comes back).
effect(() => {
  const here = tabIdSet.value;
  const all = sessions.value;
  if (!helloDone.value || (windowsSupported.value && windowId.value === null)) return;
  for (const sid of terms.attachedSids()) {
    const s = all.get(sid);
    if (s && !here.has(s.channel_id)) terms.detach(sid);
  }
});

terms.onTerminalFocus((sid) => {
  const s = sessions.value.get(sid);
  if (!s) return;
  const l = layouts.value.get(s.channel_id);
  if (!l) return;
  const p = paneOfSession(l.root, sid);
  if (p && l.focus !== p.id) updateLayout(s.channel_id, (x) => focusPane(x, p.id));
});

// Ensure a layout exists for the active tab.
effect(() => {
  const c = activeChannel.value;
  if (c) layoutOf(c.id);
});

installKeys();
rpc.start();

// Read-only debug handle (used by dev/e2e.ts; handy in devtools).
(globalThis as Record<string, unknown>).__muster = {
  rpc,
  layouts,
  sessions,
  tabs,
  windowId,
  activeChannel,
  termText: (sid: number) => {
    const t = terms.terminal(sid);
    if (!t) return null;
    const b = t.buffer.active;
    const out: string[] = [];
    for (let i = 0; i < b.length; i++) out.push(b.getLine(i)?.translateToString(true) ?? "");
    return out.join("\n");
  },
  attached: () => terms.attachedSids(),
};

// ---------- components ----------

const KINDS: SessionKind[] = ["claude", "codex", "shell"];

/** A window with no tabs: pick a folder and what to run (keys 1 2 3). */
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
      setBusy(false);
    }
  };
  return (
    <div
      class="welcome"
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
      <div class="welcome-card empty-window">
        <div class="welcome-mark">m</div>
        <h1>{first ? "Welcome to muster" : "No tabs in this window"}</h1>
        <p>
          {first
            ? "Run coding agents and shells in tabs and splits. Pick a folder and what to start there."
            : "Pick a folder and what to start there."}
        </p>
        <label class="field empty-folder">
          <span class="field-label">Folder</span>
          <FolderInput value={folder} onChange={setFolder} />
        </label>
        <LaunchButtons pick={pick} last={prefs.value.lastAgent} />
        <div class="welcome-keys">
          <span>
            <Kbd k="Ctrl+K" /> jump
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

function Splash() {
  const s = connStatus.value;
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
  if (!ready) {
    return (
      <>
        <Splash />
        <Toasts />
      </>
    );
  }
  const c = activeChannel.value;
  const l = c ? layouts.value.get(c.id) : undefined;
  return (
    <div class="app">
      <TabBar />
      <main class="content">
        {c ? <div class="panes">{l ? <SplitView ch={c.id} layout={l} /> : null}</div> : <EmptyWindow />}
      </main>
      {prefs.value.statusLine && <StatusLine />}
      <Overlays />
      <MenuHost />
      <DragOverlay />
      <TabDragOverlay />
      <TooltipHost />
      <Toasts />
    </div>
  );
}

render(<App />, document.getElementById("app")!);
