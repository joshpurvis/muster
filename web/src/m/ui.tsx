import { useEffect, useRef, useState } from "preact/hooks";
import { type AgentState, type Decision, type SessionInfo, STATE_LABEL } from "../types.ts";
import { connStatus } from "../rpc.ts";
import { phoneGrid, relativeTime } from "./logic.ts";
import { cellSize } from "./term.ts";
import { approve, auth, canInteract, errorText, logout, notify, prefs, ready, setPrefs } from "./store.ts";
import {
  alerts,
  alertsBusy,
  clearAlert,
  forgetBrowserSubscription,
  refreshAlerts,
  sendTestAlert,
  testAlert,
  turnOffAlerts,
  turnOnAlerts,
} from "./alerts.ts";
import { load, save } from "../util.ts";

export function Icon({ name }: { name: "back" | "more" | "send" | "down" | "compose" | "plus" }) {
  const paths = {
    plus: "M12 5v14M5 12h14",
    back: "m14 6-6 6 6 6M8 12h12",
    more: "M5 12h.01M12 12h.01M19 12h.01",
    send: "m5 12 7-7 7 7M12 5v15",
    down: "m6 9 6 6 6-6",
    compose: "m15 5 4 4M5 19l4-1L20 7a2.8 2.8 0 0 0-4-4L5 14v5Z",
  };
  return (
    <svg
      width="20"
      height="20"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d={paths[name]} />
    </svg>
  );
}
export function StateDot({ state }: { state: AgentState }) {
  return <span class={`state-dot state-${state}`} role="img" aria-label={STATE_LABEL[state]} />;
}
export function Settings() {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const p = prefs.value;
  useEffect(() => {
    if (!open) return;
    const dismiss = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", key);
    };
  }, [open]);
  return (
    <div class="settings" ref={root}>
      <button
        type="button"
        class="icon-button"
        aria-label="Settings"
        aria-expanded={open}
        onClick={() => {
          // Opening the menu asks again how alerts stand (a delivery may have failed meanwhile).
          if (!open) refreshAlerts();
          setOpen(!open);
        }}
      >
        <Icon name="more" />
      </button>
      {open && (
        <div class="settings-panel">
          <label>
            Font size <output>{p.fontSize}px</output>
            <input
              aria-label="Terminal font size"
              type="range"
              min="7"
              max="20"
              step="0.5"
              value={p.fontSize}
              onInput={(e) => setPrefs({ fontSize: Number(e.currentTarget.value) })}
            />
          </label>
          <label>
            Terminal size
            <select
              value={p.termSize}
              onChange={(e) => setPrefs({ termSize: e.currentTarget.value as "follow" | "keep" })}
            >
              <option value="follow">Follow my actions</option>
              <option value="keep">Keep desktop size</option>
            </select>
          </label>
          <p>
            {p.termSize === "follow"
              ? "Fits your phone when you send or approve."
              : "Drag sideways to see wide output."}
          </p>
          <AlertSettings />
          <button
            type="button"
            class="logout"
            onClick={async () => {
              setOpen(false);
              await logout();
              if (auth.peek() === "unpaired") forgetBrowserSubscription();
            }}
          >
            Log out
          </button>
        </div>
      )}
    </div>
  );
}
function ago(t: number, now: number): string {
  const r = relativeTime(t, now);
  return r === "just now" ? r : `${r} ago`;
}

/** One line on how alerts stand, for the settings panel. */
function alertStatus(now: number): { text: string; tone: "ok" | "warn" | "muted" } {
  const a = alerts.value;
  switch (a.kind) {
    case "checking":
      return { text: "Checking…", tone: "muted" };
    case "unsupported":
      return { text: "This browser can't show alerts. On Android, open Muster in Chrome.", tone: "muted" };
    case "denied":
      return {
        text: "Notifications are blocked for this site. Allow them in Chrome's site settings, then come back.",
        tone: "warn",
      };
    case "unavailable":
      return { text: "This Muster doesn't support phone alerts. Update Muster on your computer.", tone: "muted" };
    case "outdated":
      return {
        text: "Muster updated and needs a reload before alerts can show on this phone.",
        tone: "warn",
      };
    case "off":
      return { text: "Off. Turn on to get an alert when an agent needs you, even with the screen off.", tone: "muted" };
    case "on": {
      const { info, failing } = a;
      if (failing) {
        return {
          text: `Last alert failed ${ago(info.last_error_at!, now)}: ${info.last_error}`,
          tone: "warn",
        };
      }
      return {
        text: info.last_ok_at === null
          ? "On. No alerts sent yet."
          : `On. Last alert sent ${ago(info.last_ok_at, now)}.`,
        tone: "ok",
      };
    }
  }
}

const TEST_TEXT = {
  idle: "",
  sending: "Sending a test alert…",
  sent: "Test sent. It should appear in a few seconds.",
  arrived: "Test alert arrived: alerts work on this phone.",
  late: "Not shown yet. Check that Chrome may show notifications and that battery saver isn't holding them.",
};

function AlertSettings() {
  const a = alerts.value;
  const status = alertStatus(Date.now());
  const busy = alertsBusy.value || !ready.value;
  const test = testAlert.value;
  return (
    <section class="alert-settings" aria-labelledby="alerts-heading">
      <h2 id="alerts-heading">Alerts on this phone</h2>
      <p class={`alert-status ${status.tone}`} role="status">{status.text}</p>
      {a.kind === "on" && test !== "idle" && (
        <p class={`alert-status ${test === "late" ? "warn" : test === "arrived" ? "ok" : "muted"}`} role="status">
          {TEST_TEXT[test]}
        </p>
      )}
      {a.kind === "outdated" && (
        <button type="button" class="alert-on" onClick={() => location.reload()}>Reload</button>
      )}
      {(a.kind === "off" || (a.kind === "on" && a.failing)) && (
        <button type="button" class="alert-on" disabled={busy} onClick={() => turnOnAlerts(a.kind === "on")}>
          {a.kind === "off" ? "Turn on alerts" : alertsBusy.value ? "Checking…" : "Turn on again"}
        </button>
      )}
      {a.kind === "on" && (
        <div class="alert-actions">
          <button type="button" disabled={busy || test === "sending"} onClick={sendTestAlert}>Send test</button>
          <button type="button" disabled={busy} onClick={turnOffAlerts}>Turn off</button>
        </div>
      )}
    </section>
  );
}

const HINT_KEY = "m.alerts_hint_dismissed";

/** On the sessions list: a nudge while alerts are off, a warning while they fail. */
export function AlertBanner() {
  const [dismissed, setDismissed] = useState(() => load<boolean>(HINT_KEY, false) === true);
  const a = alerts.value;
  if (a.kind === "on" && a.failing) {
    return (
      <div class="alert-banner warn" role="status">
        <p>Alerts to this phone are failing. Open the menu (⋯) to check.</p>
      </div>
    );
  }
  if (a.kind !== "off" || dismissed) return null;
  return (
    <div class="alert-banner" role="region" aria-label="Alerts">
      <p>Get an alert on this phone when an agent needs you.</p>
      <div class="alert-banner-actions">
        <button type="button" class="alert-on" disabled={alertsBusy.value || !ready.value} onClick={() => turnOnAlerts()}>
          Turn on
        </button>
        <button
          type="button"
          onClick={() => {
            save(HINT_KEY, true);
            setDismissed(true);
          }}
        >
          Not now
        </button>
      </div>
    </div>
  );
}

export function Approval({ session: s, compact = false }: { session: SessionInfo; compact?: boolean }) {
  const [busy, setBusy] = useState(false);
  const run = async (decision: Decision) => {
    if (busy) return;
    setBusy(true);
    try {
      const bar = document.querySelector(".topbar")?.getBoundingClientRect().height ?? 64;
      const grid = phoneGrid(innerWidth - 16, innerHeight - bar, cellSize(prefs.peek().fontSize));
      await approve(s, decision, grid);
      clearAlert(s.id);
      notify(decision === "deny" ? "Denied" : "Approved");
    } catch (e) {
      notify(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class="approval-actions">
      <button
        type="button"
        class="approve"
        disabled={!canInteract.value || busy}
        onClick={() => run("approve")}
      >
        Approve
      </button>
      {!compact && (
        <button
          type="button"
          disabled={!canInteract.value || busy}
          onClick={() => run("approve_always")}
        >
          Always
        </button>
      )}
      <button type="button" disabled={!canInteract.value || busy} onClick={() => run("deny")}>Deny</button>
    </div>
  );
}
export function ConnectionDot() {
  return (
    <span
      class={`connection-dot ${connStatus.value === "open" ? "connected" : ""}`}
      role="img"
      aria-label={connStatus.value === "open" ? "Connected" : "Reconnecting"}
    />
  );
}
