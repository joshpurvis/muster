import { useLayoutEffect, useRef } from "preact/hooks";
import { activeChannel, activeDrawer, fullscreen, meta, sessions } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import * as terms from "../terms.ts";
import { IconChevronDown, IconRestart, IconTerminal } from "../icons.tsx";
import { tildify } from "../util.ts";

function DrawerTerminal({ sid }: { sid: number }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const host = ref.current!;
    terms.mount(sid, host);
    return () => terms.unmount(sid, host);
  }, [sid]);
  return <div class="term-host" ref={ref} />;
}

export function TerminalDrawer() {
  const c = activeChannel.value;
  const d = activeDrawer.value;
  if (!c || !d?.open || fullscreen.value !== null) return null;
  const session = d.session === null ? null : sessions.value.get(d.session);
  const starting = A.drawerStarting.value.has(c.id);
  const error = A.drawerErrors.value[c.id];
  return (
    <section id="channel-terminal-drawer" class="terminal-drawer" aria-label="Channel terminal"
      style={{ height: `${d.height}px` }}>
      <div class="drawer-resize" role="separator" tabIndex={0} aria-label="Resize channel terminal"
        aria-orientation="horizontal" aria-valuemin={140} aria-valuemax={700} aria-valuenow={d.height}
        onKeyDown={(e) => {
          if (!["ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
          e.preventDefault();
          A.setTerminalDrawer(c.id, { height: e.key === "Home" ? 140 : e.key === "End" ? 700 :
            d.height + (e.key === "ArrowUp" ? 20 : -20) });
        }}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          e.preventDefault();
          const handle = e.currentTarget;
          const y = e.clientY, height = handle.parentElement!.getBoundingClientRect().height;
          handle.setPointerCapture(e.pointerId);
          handle.onpointermove = (event) => A.setTerminalDrawer(c.id, { height: height + y - event.clientY });
          handle.onlostpointercapture = () => { handle.onpointermove = null; };
        }} />
      <header class="drawer-header">
        <IconTerminal size={14} />
        <span class="drawer-title">Terminal</span>
        <span class="drawer-path" title={session?.live_cwd ?? c.cwd}>
          {tildify(session?.live_cwd ?? c.cwd, meta.value.home)}
        </span>
        <span class="sl-space" />
        {session?.state === "exited" && <span class="drawer-state">Exited{session.exit_code != null ? ` · ${session.exit_code}` : ""}</span>}
        {session?.state === "exited" && (
          <button type="button" class="drawer-button" aria-label="Restart channel terminal"
            data-tip="Restart terminal" onClick={() => A.restartSession(session.id)}>
            <IconRestart size={14} />
          </button>
        )}
        <button type="button" class="drawer-button" aria-label="Hide channel terminal"
          data-tip="Hide terminal · shell keeps running" data-keys="Ctrl+`" onClick={A.hideTerminalDrawer}>
          <IconChevronDown size={15} />
        </button>
      </header>
      {session ? <DrawerTerminal sid={session.id} /> : (
        <div class="drawer-empty" role="status">
          <span>{starting ? "Starting terminal…" : error ? `Couldn't start terminal: ${error}` : "Start a shell in this channel’s directory."}</span>
          {!starting && <button type="button" class="btn" onClick={() => void A.startTerminalDrawer()}>
            {error ? "Try again" : "Start terminal"}
          </button>}
        </div>
      )}
      {session?.input_interrupted && session.state !== "exited" && (
        <div class="drawer-notice" role="status">
          Typing paused after reconnecting. Check the terminal before continuing.{" "}
          <button type="button" class="btn" onClick={() => A.continueInput(session.id)}>Continue typing</button>
        </div>
      )}
      {connStatus.value !== "open" && <div class="drawer-notice" role="status">Disconnected · reconnecting. Typing is paused.</div>}
    </section>
  );
}
