// The update banner (docs/UI.md "Updates"): one waiting update at a time, above the toasts.
// Backend first (restart it; terminals keep running), then the UI (reload), then the terminal
// host (only a full restart installs it, which ends every session). Dismissing hides that
// update until a different one arrives. Hidden while a dialog is open (nothing behind a modal
// may be clicked) and while Muster stops or restarts, and announced once per update by its own
// live region, apart from the toasts' status region.
import { daemonRestarting, daemonStopped, dismissedUpdates, loadedRelease, overlay, updateStatus } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { pickUpdate } from "../updates.ts";
import { Kbd, Spinner } from "./common.tsx";
import { IconX } from "../icons.tsx";
import { cx } from "../util.ts";

/** What a screen reader hears when an update starts being offered (once per update). */
const ANNOUNCE = {
  backend: "Backend update ready. Restarting the backend keeps every terminal running.",
  ui: "UI update ready. Reload this window to use it.",
  host: "Terminal host update. Installing it needs a restart of Muster, which ends all sessions.",
} as const;

export function UpdateBanner() {
  const pending = pickUpdate(updateStatus.value, loadedRelease, dismissedUpdates.value);
  const u = daemonStopped.value || daemonRestarting.value ? null : pending;
  return (
    <>
      <div class="sr-only" aria-live="polite">{u ? ANNOUNCE[u.kind] : ""}</div>
      {u && !overlay.value && <Banner u={u} />}
    </>
  );
}

function Banner({ u }: { u: NonNullable<ReturnType<typeof pickUpdate>> }) {
  const restarting = A.restartingBackend.value;
  const offline = connStatus.value !== "open";
  const dismiss = () => (dismissedUpdates.value = { ...dismissedUpdates.value, [u.kind]: u.key });
  let title: string;
  let text: string;
  let action;
  if (u.kind === "backend") {
    title = "Backend update ready";
    text = "A new Muster build is waiting. Restarting the backend keeps every terminal running.";
    action = (
      <button
        type="button"
        class="btn btn-sm btn-primary"
        disabled={restarting || offline}
        onClick={() => A.restartBackend(u.key)}
      >
        {restarting && <Spinner size={11} />}
        {restarting ? "Restarting…" : "Restart backend"}
      </button>
    );
  } else if (u.kind === "ui") {
    title = "UI update ready";
    text = "A new interface is installed. Reload this window to use it; terminals keep running.";
    action = (
      <button type="button" class="btn btn-sm btn-primary" onClick={() => A.reloadWindow()}>
        Reload <Kbd k="Ctrl+Shift+R" />
      </button>
    );
  } else {
    title = "Terminal host update";
    text = "Installing it needs a restart of Muster, which ends all sessions.";
    action = (
      <button
        type="button"
        class="btn btn-sm"
        disabled={offline}
        onClick={() => (overlay.value = { kind: "restart" })}
      >
        Restart Muster…
      </button>
    );
  }
  return (
    <div class={cx("toast update-banner", "update-" + u.kind)} role="region" aria-label={title}>
      <span class="toast-dot" />
      <div class="update-text">
        <strong>{title}</strong>
        <span>{text}</span>
        <div class="update-actions">{action}</div>
      </div>
      <button type="button" class="toast-x" aria-label="Dismiss this update" data-tip="Dismiss" onClick={dismiss}>
        <IconX size={12} />
      </button>
    </div>
  );
}
