import { useEffect, useState } from "preact/hooks";
import { needsHuman, type SessionInfo, STATE_LABEL } from "../types.ts";
import { relativeTime, sessionOrder } from "./logic.ts";
import { channels, openSession, ready, sessions } from "./store.ts";
import { NewTabButton, NewTabLayer } from "./NewTab.tsx";
import { Approval, ConnectionDot, Settings, StateDot } from "./ui.tsx";

function SessionRow({ session: s, now }: { session: SessionInfo; now: number }) {
  return (
    <li class={`session-row ${needsHuman(s.state) ? "needs-human" : ""}`} data-session={s.id}>
      <button type="button" class="session-open" onClick={() => openSession(s)}>
        <StateDot state={s.state} />
        <span class="session-summary">
          <span class="session-line">
            <strong>{s.name}</strong>
            <time>{relativeTime(s.state_since, now)}</time>
          </span>
          <span class="channel-name">{channels.value.get(s.channel_id)?.name ?? "Session"}</span>
          <span class="session-detail">{s.state_detail || STATE_LABEL[s.state]}</span>
        </span>
        <span class="row-arrow" aria-hidden="true">›</span>
      </button>
      {s.state === "needs_approval" && <Approval session={s} compact />}
    </li>
  );
}
export function Sessions() {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 30000);
    return () => clearInterval(timer);
  }, []);
  const ordered = [...sessions.value.values()].sort(sessionOrder);
  const live = ordered.filter((s) => s.state !== "exited");
  const exited = ordered.filter((s) => s.state === "exited");
  const attention = live.filter((s) => needsHuman(s.state)).length;
  return (
    <main class="sessions-screen">
      <header class="topbar">
        <img src="icon.svg" width="28" height="28" alt="" />
        <h1>Muster</h1>
        <ConnectionDot />
        <NewTabButton />
        <Settings />
      </header>
      <div class="session-list">
        <div class="list-heading">
          <h2>
            Sessions <span>{live.length}</span>
          </h2>
          {attention > 0 && <span class="attention-count">{attention} need you</span>}
        </div>
        {!ordered.length && (
          <div class="empty">
            <h2>{ready.value ? "All quiet here" : "Connecting…"}</h2>
            <p>Sessions you start on your computer will appear here.</p>
          </div>
        )}
        <ul>{live.map((s) => <SessionRow key={s.id} session={s} now={now} />)}</ul>
        {exited.length > 0 && (
          <details class="exited">
            <summary>Exited ({exited.length})</summary>
            <ul>{exited.map((s) => <SessionRow key={s.id} session={s} now={now} />)}</ul>
          </details>
        )}
      </div>
      <NewTabLayer />
    </main>
  );
}
