// Right panel: channel timeline (Slack-like feed) and worktree diff.
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import type { Channel, TimelineEvent } from "../types.ts";
import { STATE_LABEL } from "../types.ts";
import { loadTimeline, prefs, sessions, setPref, timelineOf } from "../state.ts";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { IconButton, Spinner, StateDot } from "./common.tsx";
import { IconChevron, IconDiff, IconRefresh, IconTimeline, IconX, KindIcon } from "../icons.tsx";
import { clockTime, cx, dayLabel } from "../util.ts";

export function RightPanel({ c }: { c: Channel }) {
  const tab = prefs.value.rightTab;
  return (
    <aside class="right-panel" aria-label={tab === "timeline" ? "Timeline" : "Diff"}>
      <div class="rp-head">
        <div class="seg">
          <button
            type="button"
            class={cx("seg-btn", tab === "timeline" && "on")}
            onClick={() => setPref("rightTab", "timeline")}
            data-tip="Timeline"
            data-keys="Alt+L"
          >
            <IconTimeline size={14} /> Timeline
          </button>
          <button
            type="button"
            class={cx("seg-btn", tab === "diff" && "on")}
            onClick={() => setPref("rightTab", "diff")}
            data-tip="Diff"
            data-keys="Alt+G"
          >
            <IconDiff size={14} /> Diff
          </button>
        </div>
        <IconButton
          icon={<IconX size={15} />}
          tip="Close panel"
          onClick={() => setPref("rightOpen", false)}
        />
      </div>
      {tab === "timeline" ? <Timeline key={c.id} c={c} /> : <Diff key={c.id} c={c} />}
    </aside>
  );
}

// ---------------- timeline ----------------

type Item =
  | { type: "day"; key: string; label: string }
  | {
    type: "msg";
    key: string;
    author: string;
    authorKind: "you" | "agent" | "system";
    sid?: number;
    to?: string;
    ts: number;
    parts: { id: number; text: string; queued?: boolean }[];
  }
  | { type: "sys"; key: string; events: TimelineEvent[] };

function sessName(id: number | null | undefined, e?: TimelineEvent): string {
  if (id === null || id === undefined) return "session";
  const s = sessions.value.get(id);
  if (s) return s.name;
  if (e?.data?.name) return e.data.name;
  return `session ${id}`;
}

function isMessage(e: TimelineEvent) {
  return e.kind === "note" || e.kind === "prompt" || e.kind === "agent_post";
}

function build(events: TimelineEvent[]): Item[] {
  const out: Item[] = [];
  let lastDay = "";
  for (const e of events) {
    if (e.kind === "hook") continue; // raw hooks are noise in the feed
    const day = new Date(e.ts).toDateString();
    if (day !== lastDay) {
      out.push({ type: "day", key: "d" + e.id, label: dayLabel(e.ts) });
      lastDay = day;
    }
    const prev = out[out.length - 1];
    if (isMessage(e)) {
      let author = "You";
      let authorKind: "you" | "agent" | "system" = "you";
      let sid: number | undefined;
      let to: string | undefined;
      if (e.kind === "agent_post") {
        sid = e.session_id ?? e.data?.session_id;
        author = sessName(sid);
        authorKind = "agent";
      } else if (e.kind === "prompt") {
        const target = e.data?.session_id ?? e.session_id;
        to = sessName(target);
        const from = e.data?.from;
        if (typeof from === "number" || (typeof from === "string" && /^\d+$/.test(from))) {
          sid = Number(from);
          author = sessName(sid);
          authorKind = "agent";
        } else if (from && !["human", "user", "ui"].includes(String(from))) {
          author = String(from);
          authorKind = "system";
        }
      }
      const text = String(e.data?.text ?? "");
      if (
        prev && prev.type === "msg" && prev.author === author && prev.to === to &&
        e.ts - prev.ts < 5 * 60_000
      ) {
        prev.parts.push({ id: e.id, text, queued: !!e.data?.queued });
        prev.ts = e.ts;
      } else {
        out.push({
          type: "msg",
          key: "m" + e.id,
          author,
          authorKind,
          sid,
          to,
          ts: e.ts,
          parts: [{ id: e.id, text, queued: !!e.data?.queued }],
        });
      }
    } else {
      if (prev && prev.type === "sys") prev.events.push(e);
      else out.push({ type: "sys", key: "s" + e.id, events: [e] });
    }
  }
  return out;
}

function sysText(e: TimelineEvent): preact.ComponentChildren {
  const d = e.data ?? {};
  const who = <b>{sessName(e.session_id ?? d.session_id, e)}</b>;
  switch (e.kind) {
    case "state_change":
      return (
        <>
          {who} <StateDot state={d.to} /> {STATE_LABEL[d.to as keyof typeof STATE_LABEL] ?? d.to}
          {d.detail ? <span class="sys-detail">· {d.detail}</span> : null}
        </>
      );
    case "session_started":
      return (
        <>{who} started{d.kind ? ` (${d.kind})` : ""}{d.parent_id ? ` by ${sessName(d.parent_id)}` : ""}</>
      );
    case "session_exited":
      return (
        <>
          {who} exited{d.exit_code !== undefined && d.exit_code !== null ? ` with code ${d.exit_code}` : ""}
        </>
      );
    case "mcp_call":
      return (
        <>
          {who} called <code>{d.tool}</code>
          {d.ok === false ? <span class="sys-bad">failed</span> : null}
        </>
      );
    case "approval":
      return (
        <>
          {who} {d.decision === "deny"
            ? "denied"
            : d.decision === "approve_always"
            ? "approved (always)"
            : "approved"}
        </>
      );
    case "system":
      return (
        <span class={cx(d.level === "error" && "sys-bad", d.level === "warn" && "sys-warn")}>{d.text}</span>
      );
    default:
      return <>{e.kind}</>;
  }
}

function SysRun({ events }: { events: TimelineEvent[] }) {
  const [open, setOpen] = useState(false);
  const collapsible = events.length > 3;
  const shown = collapsible && !open ? events.slice(-2) : events;
  return (
    <div class="sys-run">
      {collapsible && (
        <button type="button" class="sys-toggle" onClick={() => setOpen(!open)}>
          <IconChevron size={11} class={cx(open && "open")} />
          {open ? "Hide" : `${events.length - 2} more events`}
        </button>
      )}
      {shown.map((e) => (
        <div key={e.id} class="sys-line">
          <span class="sys-time">{clockTime(e.ts)}</span>
          <span class="sys-text">{sysText(e)}</span>
        </div>
      ))}
    </div>
  );
}

function Timeline({ c }: { c: Channel }) {
  const t = timelineOf(c.id).value;
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const prevHeight = useRef(0);
  const loadingOlder = useRef(false);

  useEffect(() => {
    if (!t.loaded) loadTimeline(c.id);
  }, [c.id]);

  const items = useMemo(() => build(t.events), [t.events, sessions.value]);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (loadingOlder.current) {
      // Keep the viewport anchored when older events are prepended.
      el.scrollTop = el.scrollHeight - prevHeight.current + el.scrollTop;
      loadingOlder.current = false;
    } else if (stick.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [items]);

  const onScroll = () => {
    const el = scroller.current!;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    if (el.scrollTop < 60 && t.hasMore && !t.loading && t.loaded) {
      prevHeight.current = el.scrollHeight;
      loadingOlder.current = true;
      loadTimeline(c.id, true);
    }
  };

  return (
    <div class="timeline" ref={scroller} onScroll={onScroll}>
      {t.loading && !t.loaded && (
        <div class="rp-empty">
          <Spinner /> Loading…
        </div>
      )}
      {t.loaded && t.hasMore && t.loading && (
        <div class="tl-more">
          <Spinner size={10} />
        </div>
      )}
      {t.loaded && !t.hasMore && t.events.length > 0 && (
        <div class="tl-start">
          Start of <b>#{c.name}</b>
        </div>
      )}
      {t.loaded && t.events.length === 0 && (
        <div class="rp-empty">
          <div class="rp-empty-title">No activity yet</div>
          <div>
            Prompts, agent posts and state changes show up here. Type <kbd>/note</kbd>{" "}
            in the composer to leave a note.
          </div>
        </div>
      )}
      {items.map((it) => {
        if (it.type === "day") {
          return (
            <div key={it.key} class="tl-day">
              <span>{it.label}</span>
            </div>
          );
        }
        if (it.type === "sys") return <SysRun key={it.key} events={it.events} />;
        const s = it.sid !== undefined ? sessions.value.get(it.sid) : undefined;
        return (
          <div key={it.key} class={cx("tl-msg", "from-" + it.authorKind)}>
            <div class="tl-avatar">
              {it.authorKind === "agent" && s
                ? <KindIcon kind={s.kind} size={14} />
                : it.author.slice(0, 1).toUpperCase()}
            </div>
            <div class="tl-body">
              <div class="tl-head">
                {it.sid !== undefined
                  ? (
                    <button type="button" class="tl-author link" onClick={() => A.revealSession(it.sid!)}>
                      {it.author}
                    </button>
                  )
                  : <span class="tl-author">{it.author}</span>}
                {it.to && <span class="tl-to">→ {it.to}</span>}
                <span class="tl-time">{clockTime(it.ts)}</span>
              </div>
              {it.parts.map((p) => (
                <div key={p.id} class="tl-text">
                  {p.text}
                  {p.queued && <span class="tl-queued">queued</span>}
                </div>
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}

// ---------------- diff ----------------

interface DiffFile {
  path: string;
  oldPath?: string;
  status: "modified" | "added" | "deleted" | "renamed" | "binary";
  add: number;
  del: number;
  lines: string[];
}

export function parseDiff(text: string): DiffFile[] {
  const files: DiffFile[] = [];
  let cur: DiffFile | null = null;
  let inHunk = false;
  for (const line of text.split("\n")) {
    if (line.startsWith("diff --git ")) {
      const m = line.match(/^diff --git a\/(.*) b\/(.*)$/);
      cur = { path: m ? m[2] : line.slice(11), status: "modified", add: 0, del: 0, lines: [] };
      files.push(cur);
      inHunk = false;
      continue;
    }
    if (!cur) {
      // Untracked files may be appended without a git header; ignore preamble.
      continue;
    }
    if (!inHunk) {
      if (line.startsWith("new file")) cur.status = "added";
      else if (line.startsWith("deleted file")) cur.status = "deleted";
      else if (line.startsWith("rename from ")) {
        cur.status = "renamed";
        cur.oldPath = line.slice(12);
      } else if (line.startsWith("Binary files")) cur.status = "binary";
      if (line.startsWith("@@")) inHunk = true;
      else continue;
    }
    if (line.startsWith("+") && !line.startsWith("+++")) cur.add++;
    else if (line.startsWith("-") && !line.startsWith("---")) cur.del++;
    cur.lines.push(line);
  }
  return files;
}

function Diff({ c }: { c: Channel }) {
  const [state, setState] = useState<
    { loading: boolean; files: DiffFile[] | null; error?: string; raw?: string }
  >({
    loading: true,
    files: null,
  });
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const bodyRef = useRef<HTMLDivElement>(null);

  const load = () => {
    setState((s) => ({ ...s, loading: true }));
    rpc.call<{ diff: string }>("channel.diff", { id: c.id })
      .then((r) => setState({ loading: false, files: parseDiff(r.diff ?? ""), raw: r.diff }))
      .catch((e) =>
        setState({ loading: false, files: null, error: e instanceof Error ? e.message : String(e) })
      );
  };
  useEffect(load, [c.id]);

  const files = state.files ?? [];
  const tot = files.reduce((a, f) => ({ add: a.add + f.add, del: a.del + f.del }), { add: 0, del: 0 });

  return (
    <div class="diff">
      <div class="diff-bar">
        <span class="diff-summary">
          {state.files && files.length > 0
            ? (
              <>
                {files.length} file{files.length === 1 ? "" : "s"}
                <span class="add">+{tot.add}</span>
                <span class="del">−{tot.del}</span>
              </>
            )
            : "Changes"}
          {c.base_branch && <span class="muted">vs {c.base_branch}</span>}
        </span>
        <IconButton
          icon={state.loading ? <Spinner size={12} /> : <IconRefresh size={14} />}
          tip="Refresh"
          onClick={load}
        />
      </div>
      <div class="diff-body" ref={bodyRef}>
        {state.error && <div class="rp-empty err">Couldn't load diff: {state.error}</div>}
        {state.files && files.length === 0 && !state.loading && (
          <div class="rp-empty">
            <div class="rp-empty-title">No changes</div>
            <div>The worktree matches its base.</div>
          </div>
        )}
        {files.length > 1 && (
          <div class="diff-files">
            {files.map((f) => (
              <button
                type="button"
                class="diff-file-link"
                onClick={() =>
                  bodyRef.current?.querySelector(`[data-file="${CSS.escape(f.path)}"]`)?.scrollIntoView({
                    block: "start",
                    behavior: "smooth",
                  })}
              >
                <span class={cx("fstat", "fs-" + f.status)}>{f.status[0].toUpperCase()}</span>
                <span class="diff-path">{f.path}</span>
                <span class="add">+{f.add}</span>
                <span class="del">−{f.del}</span>
              </button>
            ))}
          </div>
        )}
        {files.map((f) => {
          const isCollapsed = collapsed.has(f.path);
          return (
            <div class="diff-file" data-file={f.path} key={f.path}>
              <button
                type="button"
                class="diff-file-head"
                onClick={() => {
                  const n = new Set(collapsed);
                  if (isCollapsed) n.delete(f.path);
                  else n.add(f.path);
                  setCollapsed(n);
                }}
              >
                <IconChevron size={12} class={cx(!isCollapsed && "open")} />
                <span class="diff-path">
                  {f.oldPath ? <>{f.oldPath} →</> : null}
                  {f.path}
                </span>
                <span class="add">+{f.add}</span>
                <span class="del">−{f.del}</span>
              </button>
              {!isCollapsed && (
                <pre class="diff-code">
                  {f.status === "binary" && <div class="dl dl-meta">Binary file</div>}
                  {f.lines.slice(0, 4000).map((l, i) => (
                    <div
                      key={i}
                      class={cx(
                        "dl",
                        l.startsWith("@@") ? "dl-hunk" : l.startsWith("+") ? "dl-add" : l.startsWith("-") ? "dl-del" : l.startsWith("\\") ? "dl-meta" : "",
                      )}
                    >
                      {l || " "}
                    </div>
                  ))}
                  {f.lines.length > 4000 && <div class="dl dl-meta">… {f.lines.length - 4000} more lines</div>}
                </pre>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
