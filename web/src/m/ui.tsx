import { useEffect, useRef, useState } from "preact/hooks";
import { type AgentState, type Decision, type SessionInfo, STATE_LABEL } from "../types.ts";
import { connStatus } from "../rpc.ts";
import { phoneGrid } from "./logic.ts";
import { cellSize } from "./term.ts";
import { approve, canInteract, errorText, logout, notify, prefs, setPrefs } from "./store.ts";

export function Icon({ name }: { name: "back" | "more" | "send" | "down" | "compose" }) {
  const paths = {
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
        onClick={() => setOpen(!open)}
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
          <button
            type="button"
            class="logout"
            onClick={() => {
              setOpen(false);
              logout();
            }}
          >
            Log out
          </button>
        </div>
      )}
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
