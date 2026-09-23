// Decision inbox: every session that needs the human, across channels.
import { useEffect, useRef, useState } from "preact/hooks";
import type { SessionInfo } from "../types.ts";
import { channelById, inboxItems, overlay, toast, view } from "../state.ts";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { StatePill } from "./common.tsx";
import { IconArrowUpRight, IconCheck, KindIcon } from "../icons.tsx";
import { cx, timeAgo } from "../util.ts";

export function Inbox() {
  const items = inboxItems.value;
  const [sel, setSel] = useState(0);
  const [replying, setReplying] = useState<number | null>(null);
  const [, tick] = useState(0);
  const ref = useRef<HTMLDivElement>(null);

  // Re-render for "2m ago" labels.
  useEffect(() => {
    const t = setInterval(() => tick((x) => x + 1), 30_000);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    if (sel >= items.length) setSel(Math.max(0, items.length - 1));
  }, [items.length]);

  useEffect(() => {
    ref.current?.focus();
  }, []);

  useEffect(() => {
    ref.current?.querySelector(`[data-idx="${sel}"]`)?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  const onKey = (e: KeyboardEvent) => {
    if (overlay.value || e.altKey || e.ctrlKey || e.metaKey) return;
    if ((e.target as HTMLElement).tagName === "INPUT") return;
    const s = items[sel];
    switch (e.key) {
      case "j":
      case "ArrowDown":
        setSel(Math.min(items.length - 1, sel + 1));
        break;
      case "k":
      case "ArrowUp":
        setSel(Math.max(0, sel - 1));
        break;
      case "a":
        if (s?.state === "needs_approval") A.approve(s.id, "approve");
        break;
      case "d":
        if (s?.state === "needs_approval") A.approve(s.id, "deny");
        break;
      case "r":
        if (s) setReplying(s.id);
        break;
      case "Enter":
        if (s) A.revealSession(s.id);
        break;
      case "Escape":
        view.value = "channel";
        A.focusActiveTerminalSoon();
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  return (
    <div class="inbox" tabIndex={0} ref={ref} onKeyDown={onKey}>
      <div class="inbox-head">
        <h1>Inbox</h1>
        <span class="muted">{items.length ? `${items.length} waiting on you` : ""}</span>
        <span class="inbox-keys">
          <kbd>j</kbd>/<kbd>k</kbd> move · <kbd>a</kbd> approve · <kbd>d</kbd> deny · <kbd>r</kbd> reply ·
          {" "}
          <kbd>⏎</kbd> open
        </span>
      </div>
      {items.length === 0
        ? (
          <div class="inbox-empty">
            <div class="inbox-empty-check">
              <IconCheck size={22} />
            </div>
            <div class="inbox-empty-title">Nothing needs you.</div>
            <div class="muted">Approvals, questions and errors from every channel land here.</div>
          </div>
        )
        : (
          <div class="inbox-list">
            {items.map((s, i) => (
              <InboxCard
                key={s.id}
                s={s}
                idx={i}
                selected={i === sel}
                replying={replying === s.id}
                onSelect={() => setSel(i)}
                onReply={() => {
                  setSel(i);
                  setReplying(s.id);
                }}
                onReplyDone={() => {
                  setReplying(null);
                  ref.current?.focus();
                }}
              />
            ))}
          </div>
        )}
    </div>
  );
}

function InboxCard(
  { s, idx, selected, replying, onSelect, onReply, onReplyDone }: {
    s: SessionInfo;
    idx: number;
    selected: boolean;
    replying: boolean;
    onSelect: () => void;
    onReply: () => void;
    onReplyDone: () => void;
  },
) {
  const c = channelById.value.get(s.channel_id);
  const [lines, setLines] = useState<string[] | null>(null);
  const [reply, setReply] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let live = true;
    rpc.call<{ lines: string[] }>("session.screen", { id: s.id })
      .then((r) => {
        if (!live) return;
        const ls = [...r.lines];
        while (ls.length && !ls[ls.length - 1].trim()) ls.pop();
        setLines(ls.slice(-8));
      })
      .catch(() => live && setLines(null));
    return () => {
      live = false;
    };
  }, [s.id, s.state, s.state_detail]);

  useEffect(() => {
    if (replying) inputRef.current?.focus();
  }, [replying]);

  const sendReply = async () => {
    const t = reply.trim();
    if (!t) return;
    try {
      await A.send(s.id, t);
      setReply("");
      onReplyDone();
      toast("success", `Sent to ${s.name}`, 1800);
    } catch (e) {
      toast("error", `Send failed: ${e instanceof Error ? e.message : e}`);
    }
  };

  return (
    <div
      class={cx("inbox-card", selected && "selected", "st-" + s.state)}
      data-idx={idx}
      onClick={onSelect}
    >
      <div class="ic-head">
        <span class="ic-channel">#{c?.name ?? "?"}</span>
        <span class="ic-sep">·</span>
        <KindIcon kind={s.kind} size={13} />
        <span class="ic-session">{s.name}</span>
        <StatePill state={s.state} />
        <span class="ic-age">{timeAgo(s.state_since)}</span>
      </div>
      {s.state_detail && <div class="ic-detail">{s.state_detail}</div>}
      {lines && lines.length > 0 && <pre class="ic-preview">{lines.join("\n")}</pre>}
      <div class="ic-actions">
        {s.state === "needs_approval" && (
          <>
            <button type="button" class="btn btn-sm btn-approve" onClick={() => A.approve(s.id, "approve")}>
              <IconCheck size={13} /> Approve <kbd>a</kbd>
            </button>
            <button type="button" class="btn btn-sm" onClick={() => A.approve(s.id, "approve_always")}>
              Always
            </button>
            <button type="button" class="btn btn-sm btn-deny" onClick={() => A.approve(s.id, "deny")}>
              Deny <kbd>d</kbd>
            </button>
          </>
        )}
        {replying
          ? (
            <input
              ref={inputRef}
              class="ic-reply"
              value={reply}
              placeholder={`Reply to ${s.name}…`}
              onInput={(e) => setReply((e.target as HTMLInputElement).value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  sendReply();
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  onReplyDone();
                }
              }}
            />
          )
          : (
            <button
              type="button"
              class="btn btn-sm"
              onClick={(e) => {
                e.stopPropagation();
                setReply("");
                onReply();
              }}
            >
              Reply <kbd>r</kbd>
            </button>
          )}
        <span class="grow" />
        <button type="button" class="btn btn-sm btn-ghost" onClick={() => A.revealSession(s.id)}>
          Open <IconArrowUpRight size={13} />
        </button>
      </div>
    </div>
  );
}
