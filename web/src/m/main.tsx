import { connStatus } from "../rpc.ts";
import { effect } from "@preact/signals";
import { render } from "preact";
import { listenToServiceWorker, refreshAlerts } from "./alerts.ts";
import "@xterm/xterm/css/xterm.css";
import "./mobile.css";
import { Pair } from "./Pair.tsx";
import { Sessions } from "./Sessions.tsx";
import { SessionView } from "./SessionView.tsx";
import {
  auth,
  closeSession,
  online,
  openSessionId,
  pairBusy,
  ready,
  reconnect,
  selected,
  sessions,
  start,
  toast,
} from "./store.ts";

function App() {
  const sid = selected.value;
  const session = sid === null ? null : sessions.value.get(sid);
  return (
    <div class="mobile-app">
      {auth.value === "paired" && !ready.value && (
        <div class="connection-banner" role="status">
          {connStatus.value === "incompatible" ? "UI update required. Install a compatible UI and refresh; terminals remain running." : online.value ? "Reconnecting…" : "Offline — waiting for a connection"}
        </div>
      )}
      {auth.value === "unpaired" || pairBusy.value ? <Pair /> : auth.value === "paired"
        ? (
          session ? <SessionView key={session.id} session={session} /> : sid && ready.value
            ? (
              <main class="empty">
                <h1>Session unavailable</h1>
                <p>It may have been removed on your computer.</p>
                <button type="button" onClick={closeSession}>Back to sessions</button>
              </main>
            )
            : <Sessions />
        )
        : (
          <main class="empty">
            <img src="icon.svg" width="40" height="40" alt="" />
            <h1>{auth.value === "offline" ? "You’re offline" : "Connecting to Muster…"}</h1>
            <p>
              {auth.value === "offline"
                ? "Check your connection and Tailscale, then try again."
                : "Finding your sessions."}
            </p>
            {auth.value === "offline" && <button type="button" onClick={reconnect}>Try again</button>}
          </main>
        )}
      <div class={`toast ${toast.value ? "visible" : ""}`} role="status" aria-live="polite">
        {toast.value}
      </div>
    </div>
  );
}

const viewport = () => {
  const vv = visualViewport;
  document.documentElement.style.setProperty("--view-height", `${vv?.height ?? innerHeight}px`);
  document.documentElement.style.setProperty("--view-top", `${vv?.offsetTop ?? 0}px`);
};
visualViewport?.addEventListener("resize", viewport);
visualViewport?.addEventListener("scroll", viewport);
addEventListener("resize", viewport);
viewport();
start();
listenToServiceWorker(openSessionId);
// Each (re)connection asks how alerts stand: the daemon may have dropped the subscription.
effect(() => {
  if (ready.value) refreshAlerts();
});
render(<App />, document.getElementById("app")!);
if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js", { scope: "/m/" }).catch(() => {});
