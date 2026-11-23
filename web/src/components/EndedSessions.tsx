// "Archived sessions" (docs/UI.md): every closed tab, newest first, all channels and accounts in
// one list, as the Notes list looks. A row opens what Muster still has of it: the facts below,
// never its screen or output (Muster doesn't keep those).
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { accountProfiles, meta, overlay } from "../state.ts";
import { profileName } from "../account_profiles.ts";
import {
  byDay, channelColour, clock, conversationKeeper, duration, ENDED_KEPT, endedMatches, endedRevision, endText, fullTime,
} from "../ended.ts";
import { ago } from "../agents.ts";
import { IconArchive, IconChevron, IconCopy, KindIcon } from "../icons.tsx";
import { KIND_LABEL, type EndedSession } from "../types.ts";
import { cx, tildify } from "../util.ts";
import { Spinner } from "./common.tsx";

type ModalT = (p: { children: ComponentChildren; class?: string; label: string; onClose?: () => void }) =>
  ComponentChildren;

/** The account a row names: Claude's profile; other kinds have none here. */
function account(e: EndedSession): string | null {
  return e.kind === "claude" ? profileName(accountProfiles.value, e.account_profile_id) : null;
}

/** The channel it was in: its name with the channel's own colour, marked when archived. */
export function ChannelTag({ e }: { e: EndedSession }) {
  return (
    <span class={cx("ended-tag", e.channel_archived && "archived")} style={{ "--tag": channelColour(e.channel_id) }}
      data-tip={e.channel_archived ? `Channel ${e.channel_name} (archived)` : `Channel ${e.channel_name}`}>
      <span class="ended-tag-dot" aria-hidden="true" />
      <span class="ended-tag-name">{e.channel_name || "unnamed channel"}</span>
      {e.channel_archived && <span class="ended-tag-note">archived</span>}
    </span>
  );
}

export function EndedSessionsDialog({ Modal }: { Modal: ModalT }) {
  // Opened from the sidebar: Esc goes back there, as the Notes list does.
  const [opener] = useState(() => {
    const el = document.activeElement as HTMLElement | null;
    return el?.closest(".channels-panel") ? el : null;
  });
  // Focus goes back once the dialog (and its focus trap) is gone; opened elsewhere (the
  // palette), the dialog's own close returns to the active tab.
  const cancel = opener
    ? () => {
      overlay.value = null;
      requestAnimationFrame(() => { if (opener.isConnected) opener.focus(); });
    }
    : undefined;
  const [list, setList] = useState<EndedSession[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [q, setQ] = useState("");
  // The selected and the opened row, by session id: a tab closing meanwhile (a new row on top)
  // never moves the selection to another session.
  const [selId, setSelId] = useState<number | null>(null);
  const [openId, setOpenId] = useState<number | null>(null);
  const [kept, setKept] = useState<EndedSession | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const backRef = useRef<HTMLButtonElement>(null);
  const revision = endedRevision.value;

  useEffect(() => {
    let live = true;
    rpc.call<EndedSession[]>("session.ended_list", { limit: ENDED_KEPT })
      .then((l) => { if (live) { setList(l); setErr(null); } })
      .catch((e) => { if (live) setErr(e instanceof Error ? e.message : String(e)); });
    return () => { live = false; };
  }, [revision]);
  // The opened row as the latest list has it (a renamed or archived channel shows at once),
  // else as it was when opened.
  const open = openId === null ? null : list?.find((x) => x.id === openId) ?? kept;
  useLayoutEffect(() => (openId !== null ? backRef.current : inputRef.current)?.focus(), [openId]);

  const now = Date.now();
  const rows = useMemo(() => (list ?? []).filter((e) => endedMatches(q, e)), [list, q]);
  const groups = useMemo(() => byDay(rows, now), [rows, Math.floor(now / 60_000)]);
  const found = rows.findIndex((x) => x.id === selId);
  const sel = found < 0 ? 0 : found;
  const select = (i: number) => setSelId(rows[i]?.id ?? null);
  useEffect(() => {
    listRef.current?.querySelector(".sw-item.sel")?.scrollIntoView({ block: "nearest" });
  }, [sel, openId]);
  const optionId = (id: number) => `ended-opt-${id}`;
  const show = (e: EndedSession) => {
    setSelId(e.id);
    setKept(e);
    setOpenId(e.id);
  };

  const onKey = (e: KeyboardEvent) => {
    const n = Math.max(1, rows.length);
    if (e.key === "ArrowDown" || (e.key === "n" && e.ctrlKey)) {
      e.preventDefault();
      select((sel + 1) % n);
    } else if (e.key === "ArrowUp" || (e.key === "p" && e.ctrlKey)) {
      e.preventDefault();
      select((sel - 1 + n) % n);
    } else if (e.key === "Home" && e.ctrlKey) {
      e.preventDefault();
      select(0);
    } else if (e.key === "End" && e.ctrlKey) {
      e.preventDefault();
      select(n - 1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (rows[sel]) show(rows[sel]);
    }
  };

  if (open) {
    return (
      <Modal class="switcher notes-list ended-list" label={`Archived session ${open.name}`} onClose={cancel}>
        <Detail e={open} now={now} backRef={backRef} back={() => setOpenId(null)} />
      </Modal>
    );
  }

  let index = -1;
  return (
    <Modal class="switcher notes-list ended-list" label="Archived sessions" onClose={cancel}>
      <div class="sw-input-row">
        <span class="sw-mode">Archived</span>
        <input
          ref={inputRef}
          class="sw-input"
          value={q}
          placeholder="Find a closed session by name, channel or folder…"
          spellcheck={false}
          aria-label="Find a closed session"
          role="combobox"
          aria-expanded="true"
          aria-autocomplete="list"
          aria-controls="ended-listbox"
          aria-activedescendant={rows[sel] ? optionId(rows[sel].id) : undefined}
          onInput={(e) => {
            // A new filter starts at its first match.
            setQ((e.target as HTMLInputElement).value);
            setSelId(null);
          }}
          onKeyDown={onKey}
        />
      </div>
      <div class="sw-list" ref={listRef} role="listbox" id="ended-listbox" aria-label="Archived sessions">
        {err && <div class="sw-empty form-error">Couldn't load closed sessions: {err}</div>}
        {!err && list === null && <div class="sw-empty"><Spinner size={12} /> Loading…</div>}
        {!err && list !== null && rows.length === 0 && (
          <div class="sw-empty">
            {list.length === 0
              ? "No closed sessions yet. Each tab you close, or that closes with its channel, shows here, newest first."
              : "No matches"}
          </div>
        )}
        {groups.map((g) => (
          <div role="group" aria-label={g.day} key={g.day}>
            <div class="ended-day" aria-hidden="true">{g.day}</div>
            {g.rows.map((e) => {
              const i = ++index;
              const acct = account(e);
              const sub = e.title && e.title !== e.name ? e.title : tildify(e.cwd, meta.value.home);
              const when = clock(e.ended_at);
              return (
                <div
                  key={e.id}
                  id={optionId(e.id)}
                  role="option"
                  aria-selected={i === sel}
                  aria-label={[
                    e.name,
                    `${KIND_LABEL[e.kind] ?? e.kind}${e.agent ? `, agent ${e.agent}` : ""}`,
                    `channel ${e.channel_name}${e.channel_archived ? " (archived)" : ""}`,
                    acct ? `account ${acct}` : null,
                    `closed ${g.day}, ${when}`,
                  ].filter(Boolean).join(", ")}
                  class={cx("sw-item", "ended-row", i === sel && "sel")}
                  onMouseMove={() => i !== sel && setSelId(e.id)}
                  onClick={() => show(e)}
                >
                  <span class="sw-icon"><KindIcon kind={e.kind} size={14} /></span>
                  <span class="sw-label">{e.name}</span>
                  <ChannelTag e={e} />
                  <span class="sw-sub">{sub}</span>
                  <span class="sw-right">
                    <span class="sw-detail">
                      {[e.agent, acct].filter(Boolean).join(" · ")}
                    </span>
                    <span class="ended-time" data-tip={fullTime(e.ended_at)}>{when}</span>
                  </span>
                </div>
              );
            })}
          </div>
        ))}
      </div>
      <div class="sw-foot">
        <span><kbd>⏎</kbd> details</span>
        <span><kbd>esc</kbd> close</span>
        <span class="sw-foot-note">Muster keeps the newest {ENDED_KEPT} closed tabs, never their output</span>
      </div>
    </Modal>
  );
}

/** What Muster still has for one closed tab. */
function Detail({ e, now, back, backRef }: {
  e: EndedSession;
  now: number;
  back: () => void;
  backRef: { current: HTMLButtonElement | null };
}) {
  const acct = account(e);
  const keeper = conversationKeeper(e);
  const folder = tildify(e.cwd, meta.value.home);
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape" || (ev.key === "ArrowLeft" && (ev.target as HTMLElement).tagName === "BUTTON") ||
      (ev.key === "Backspace" && ev.target === backRef.current)) {
      // Back to the list, not closed: the dialog's own Esc never sees it.
      ev.preventDefault();
      ev.stopPropagation();
      back();
    }
  };
  return (
    <div class="ended-detail" onKeyDown={onKey}>
      <div class="ended-head">
        <button ref={backRef} type="button" class="btn btn-sm ended-back" onClick={back}
          aria-label="Back to all archived sessions" data-tip="Back" data-keys="Esc">
          <IconChevron size={12} class="back" /> All
        </button>
        <span class="sw-icon"><KindIcon kind={e.kind} size={15} /></span>
        <h2 class="ended-title">{e.name}</h2>
        <ChannelTag e={e} />
      </div>
      <dl class="ended-facts">
        <Fact k="Channel">{e.channel_name || "unnamed channel"}{e.channel_archived ? " (archived)" : ""}</Fact>
        {e.agent && <Fact k="Agent">{e.agent}</Fact>}
        <Fact k="Kind">{KIND_LABEL[e.kind] ?? e.kind}</Fact>
        {acct && <Fact k="Account">{acct}</Fact>}
        {e.title && e.title !== e.name && <Fact k="Last title">{e.title}</Fact>}
        <Fact k="Folder">
          <span class="ended-mono">{folder}</span>
          <CopyButton text={e.cwd} what="Folder path copied" label="Copy folder path" />
        </Fact>
        <Fact k="Opened">{fullTime(e.created_at)}</Fact>
        <Fact k="Closed">{fullTime(e.ended_at)} ({ago(e.ended_at, now) === "now" ? "just now" : `${ago(e.ended_at, now)} ago`})</Fact>
        <Fact k="Tab open for">{duration(e.ended_at - e.created_at)}</Fact>
        <Fact k="How it ended">{endText(e)}</Fact>
        {e.conversation && (
          <Fact k="Conversation">
            <span class="ended-mono">{e.conversation}</span>
            <CopyButton text={e.conversation} what="Conversation id copied" label="Copy conversation id" />
          </Fact>
        )}
      </dl>
      <div class="dlg-note ended-kept" role="note">
        <IconArchive size={13} />
        <span>
          Muster keeps only these details: a closed tab's screen and output aren't kept, so they can't be shown here.
          {keeper && ` ${keeper} keeps its own copy of the conversation under the id above.`}
        </span>
      </div>
      <div class="sw-foot">
        <span><kbd>esc</kbd> back to the list</span>
      </div>
    </div>
  );
}

function Fact({ k, children }: { k: string; children: ComponentChildren }) {
  return (
    <>
      <dt>{k}</dt>
      <dd>{children}</dd>
    </>
  );
}

function CopyButton({ text, what, label }: { text: string; what: string; label: string }) {
  return (
    <button type="button" class="ap-icon-btn ended-copy" aria-label={label} data-tip={label}
      onClick={() => A.copy(text, what)}>
      <IconCopy size={12} />
    </button>
  );
}
