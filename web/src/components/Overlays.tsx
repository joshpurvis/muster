// Modal overlays: quick switcher / command palette, new channel, add workspace,
// shortcuts sheet, confirm dialog. Plus toasts.
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  activeChannel,
  channelById,
  channelOrder,
  dismissToast,
  meta,
  overlay,
  prefs,
  sessions,
  toast,
  toasts,
  workspaceById,
  workspaces,
} from "../state.ts";
import { rpc } from "../rpc.ts";
import * as A from "../actions.ts";
import { type Command, commands } from "../keys.ts";
import { Kbd, Spinner, StateDot } from "./common.tsx";
import {
  IconCheck,
  IconCodex,
  IconFolder,
  IconHash,
  IconInbox,
  IconSpark,
  IconTerminal,
  IconX,
  KindIcon,
} from "../icons.tsx";
import type { SessionKind } from "../types.ts";
import { STATE_LABEL } from "../types.ts";
import { cx, fuzzy, slugify, tildify } from "../util.ts";

const FOCUSABLE =
  'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [href], [tabindex]:not([tabindex="-1"])';

function close() {
  overlay.value = null;
  A.focusActiveTerminalSoon();
}

function Modal(
  { children, class: c, onClose = close, label }: {
    children: ComponentChildren;
    class?: string;
    onClose?: () => void;
    label: string;
  },
) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    const focusables = () =>
      [...(ref.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? [])].filter((el) =>
        el.offsetParent !== null || el === document.activeElement
      );
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        e.stopPropagation();
        onClose();
        return;
      }
      // Trap Tab inside the dialog (fields that use Tab themselves prevent it first).
      if (e.key === "Tab" && !e.defaultPrevented && !e.ctrlKey && !e.altKey && !e.metaKey) {
        const list = focusables();
        if (!list.length) {
          e.preventDefault();
          ref.current?.focus();
          return;
        }
        const i = list.indexOf(document.activeElement as HTMLElement);
        const next = e.shiftKey
          ? (i <= 0 ? list.length - 1 : i - 1)
          : (i < 0 || i === list.length - 1 ? 0 : i + 1);
        e.preventDefault();
        list[next].focus();
      }
    };
    // Focus can't leave the dialog for the terminal (or anything else) behind it; menus opened
    // from the dialog are fine.
    const focusIn = (e: FocusEvent) => {
      const t = e.target as HTMLElement | null;
      if (!ref.current || !t || ref.current.contains(t) || t.closest?.(".menu")) return;
      (focusables()[0] ?? ref.current).focus();
    };
    addEventListener("keydown", key);
    document.addEventListener("focusin", focusIn);
    // Dialogs without an autofocused field still take focus off the terminal behind them.
    if (ref.current && !ref.current.contains(document.activeElement)) ref.current.focus();
    return () => {
      removeEventListener("keydown", key);
      document.removeEventListener("focusin", focusIn);
      if (document.activeElement === document.body && prev?.isConnected && !overlay.value) prev.focus();
    };
  }, []);
  return (
    <div
      class="modal-backdrop"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div ref={ref} class={cx("modal", c)} role="dialog" aria-modal="true" aria-label={label} tabIndex={-1}>
        {children}
      </div>
    </div>
  );
}

export function Overlays() {
  const o = overlay.value;
  if (!o) return null;
  switch (o.kind) {
    case "switcher":
      return <Switcher key={o.initial} initial={o.initial} />;
    case "new_channel":
      return <NewChannel workspaceId={o.workspaceId} />;
    case "add_workspace":
      return <AddWorkspace />;
    case "shortcuts":
      return <Shortcuts />;
    case "confirm":
      return <Confirm {...o} />;
  }
}

// ---------------- quick switcher / command palette ----------------

interface Entry {
  key: string;
  label: string;
  sub?: string;
  icon: ComponentChildren;
  right?: ComponentChildren;
  score: number;
  hits: number[];
  run: () => void;
}

function Highlight({ text, hits }: { text: string; hits: number[] }) {
  if (!hits.length) return <>{text}</>;
  const set = new Set(hits);
  const out: ComponentChildren[] = [];
  let buf = "";
  let on = false;
  for (let i = 0; i < text.length; i++) {
    const h = set.has(i);
    if (h !== on && buf) {
      out.push(on ? <mark>{buf}</mark> : buf);
      buf = "";
    }
    on = h;
    buf += text[i];
  }
  if (buf) out.push(on ? <mark>{buf}</mark> : buf);
  return <>{out}</>;
}

function commandEntries(q: string): Entry[] {
  const out: Entry[] = [];
  for (const c of commands) {
    if (c.hidden) continue;
    if (c.enabled && !c.enabled()) continue;
    const hits: number[] = [];
    const s = fuzzy(q, c.title, hits);
    if (s === null) continue;
    out.push({
      key: "cmd:" + c.id,
      label: c.title,
      sub: c.group,
      icon: <span class="cmd-icon">›</span>,
      right: c.keys?.[0] ? <Kbd k={c.keys[0]} /> : null,
      score: s,
      hits,
      run: () => runCommand(c),
    });
  }
  return q ? out.sort((a, b) => b.score - a.score) : out;
}

function runCommand(c: Command) {
  overlay.value = null;
  c.run();
  if (!overlay.value && !A.findOpen.value && c.id !== "composer") A.focusActiveTerminalSoon();
}

function Switcher({ initial }: { initial: string }) {
  const [q, setQ] = useState(initial);
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const cmdMode = q.startsWith(">");

  useLayoutEffect(() => {
    const el = inputRef.current!;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);

  const entries = useMemo((): Entry[] => {
    if (cmdMode) return commandEntries(q.slice(1).trim());
    const query = q.trim();
    const recent = prefs.value.recent;
    const out: Entry[] = [];
    const push = (e: Omit<Entry, "score" | "hits">, hay: string, boost = 0) => {
      const hits: number[] = [];
      const s = fuzzy(query, hay, hits);
      if (s === null) return;
      // Hits index into `hay`; we highlight the label, which is hay's prefix.
      const ri = recent.indexOf(e.key);
      out.push({
        ...e,
        score: s + boost + (ri >= 0 ? 6 - ri * 0.3 : 0),
        hits: hits.filter((i) => i < e.label.length),
      });
    };
    for (const c of channelOrder.value) {
      const w = workspaceById.value.get(c.workspace_id);
      push(
        {
          key: "c:" + c.id,
          label: c.name,
          sub: w?.name,
          icon: <IconHash size={14} />,
          right: c.id === activeChannel.value?.id ? <span class="muted">current</span> : null,
          run: () => {
            overlay.value = null;
            A.selectChannel(c.id);
          },
        },
        c.name + " " + (w?.name ?? ""),
        2,
      );
    }
    for (const s of sessions.value.values()) {
      const c = channelById.value.get(s.channel_id);
      if (!c) continue;
      push({
        key: "s:" + s.id,
        label: s.name,
        sub: `#${c.name}`,
        icon: <KindIcon kind={s.kind} size={14} />,
        right: (
          <span class="sw-state">
            <StateDot state={s.state} /> {STATE_LABEL[s.state]}
          </span>
        ),
        run: () => {
          overlay.value = null;
          A.revealSession(s.id);
        },
      }, s.name + " " + c.name);
    }
    for (const w of workspaces.value) {
      push(
        {
          key: "w:" + w.id,
          label: w.name,
          sub: "New channel in workspace",
          icon: <IconFolder size={14} />,
          run: () => (overlay.value = { kind: "new_channel", workspaceId: w.id }),
        },
        w.name,
        -2,
      );
    }
    push(
      {
        key: "inbox",
        label: "Inbox",
        sub: "Everything that needs you",
        icon: <IconInbox size={14} />,
        right: <Kbd k="Alt+I" />,
        run: () => {
          overlay.value = null;
          A.openInbox();
        },
      },
      "Inbox",
      -1,
    );
    if (query) {
      for (const e of commandEntries(query)) out.push({ ...e, score: e.score - 4 });
    }
    if (!query) {
      // Recent first, then sidebar order.
      const rank = (e: Entry) => {
        const i = recent.indexOf(e.key);
        return i >= 0 ? i : 100 + out.indexOf(e);
      };
      return [...out].sort((a, b) => rank(a) - rank(b)).slice(0, 50);
    }
    return out.sort((a, b) => b.score - a.score).slice(0, 50);
  }, [q]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    listRef.current?.querySelector(".sw-item.sel")?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown" || (e.key === "n" && e.ctrlKey) || (e.key === "Tab" && !e.shiftKey)) {
      e.preventDefault();
      setSel((sel + 1) % Math.max(1, entries.length));
    } else if (e.key === "ArrowUp" || (e.key === "p" && e.ctrlKey) || (e.key === "Tab" && e.shiftKey)) {
      e.preventDefault();
      setSel((sel - 1 + entries.length) % Math.max(1, entries.length));
    } else if (e.key === "Enter") {
      e.preventDefault();
      entries[sel]?.run();
    } else if (e.key === "Backspace" && q === ">" && initial !== ">") {
      // fallthrough: removing '>' returns to switcher mode naturally
    }
  };

  return (
    <Modal class="switcher" label={cmdMode ? "Command palette" : "Quick switcher"}>
      <div class="sw-input-row">
        {cmdMode ? <span class="sw-mode">Commands</span> : <IconHashOrSearch />}
        <input
          ref={inputRef}
          class="sw-input"
          value={q}
          placeholder={cmdMode ? "Type a command…" : "Jump to a channel or session…   (type > for commands)"}
          spellcheck={false}
          onInput={(e) => setQ((e.target as HTMLInputElement).value)}
          onKeyDown={onKey}
        />
      </div>
      <div class="sw-list" ref={listRef} role="listbox">
        {entries.length === 0 && <div class="sw-empty">No matches</div>}
        {entries.map((e, i) => (
          <div
            key={e.key}
            role="option"
            aria-selected={i === sel}
            class={cx("sw-item", i === sel && "sel")}
            onMouseMove={() => i !== sel && setSel(i)}
            onClick={() => e.run()}
          >
            <span class="sw-icon">{e.icon}</span>
            <span class="sw-label">
              <Highlight text={e.label} hits={e.hits} />
            </span>
            {e.sub && <span class="sw-sub">{e.sub}</span>}
            <span class="sw-right">{e.right}</span>
          </div>
        ))}
      </div>
      <div class="sw-foot">
        <span>
          <kbd>↑</kbd>
          <kbd>↓</kbd> navigate
        </span>
        <span>
          <kbd>⏎</kbd> open
        </span>
        <span>
          <kbd>esc</kbd> close
        </span>
        {!cmdMode && (
          <span>
            <kbd>&gt;</kbd> commands
          </span>
        )}
      </div>
    </Modal>
  );
}

function IconHashOrSearch() {
  return (
    <span class="sw-mode-icon">
      <svg
        width="15"
        height="15"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="1.6"
        stroke-linecap="round"
      >
        <circle cx="11" cy="11" r="6.5" />
        <path d="m20 20-4.2-4.2" />
      </svg>
    </span>
  );
}

// ---------------- new channel ----------------

const AGENTS: { k: SessionKind | "none"; label: string; icon: ComponentChildren }[] = [
  { k: "claude", label: "Claude", icon: <IconSpark size={14} /> },
  { k: "codex", label: "Codex", icon: <IconCodex size={14} /> },
  { k: "shell", label: "Shell", icon: <IconTerminal size={14} /> },
  { k: "none", label: "None", icon: <IconX size={14} /> },
];

function NewChannel({ workspaceId }: { workspaceId?: number }) {
  const ws = workspaces.value;
  const [wid, setWid] = useState<number | undefined>(
    workspaceId ?? activeChannel.value?.workspace_id ?? ws[0]?.id,
  );
  const w = wid !== undefined ? workspaceById.value.get(wid) : undefined;
  const [name, setName] = useState("");
  const [agent, setAgent] = useState<SessionKind | "none">(prefs.value.lastAgent);
  const [worktree, setWorktree] = useState(true);
  const [base, setBase] = useState("");
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => nameRef.current?.focus(), [ws.length > 0]);
  useEffect(() => {
    if (wid === undefined && ws.length) setWid(ws[0].id);
  }, [ws.length]);

  const slug = slugify(name);
  const git = !!w?.is_git;
  const canCreate = !!slug && wid !== undefined && !busy;

  const create = async () => {
    if (!canCreate) return;
    setBusy(true);
    setErr(null);
    try {
      await A.createChannel({
        workspace_id: wid!,
        name: name.trim(),
        worktree: git && worktree,
        base: base.trim() || undefined,
        agent: agent === "none" ? null : agent,
        prompt: agent !== "none" && agent !== "shell" ? prompt.trim() || undefined : undefined,
      });
      overlay.value = null;
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  if (!ws.length) {
    return (
      <Modal class="dialog" label="New channel">
        <div class="dlg-head">
          <h2>New channel</h2>
        </div>
        <div class="dlg-body">
          <p class="muted">Channels live in a workspace (a repo or folder). Add one first.</p>
        </div>
        <div class="dlg-foot">
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button
            type="button"
            class="btn btn-primary"
            onClick={() => (overlay.value = { kind: "add_workspace" })}
          >
            Add workspace
          </button>
        </div>
      </Modal>
    );
  }

  return (
    <Modal class="dialog" label="New channel">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          create();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            create();
          }
        }}
      >
        <div class="dlg-head">
          <h2>New channel</h2>
          <span class="muted">A task with its own worktree and sessions</span>
        </div>
        <div class="dlg-body">
          <label class="field">
            <span class="field-label">Name</span>
            <div class="input-prefix">
              <span>#</span>
              <input
                ref={nameRef}
                value={name}
                placeholder="fix-auth-redirect"
                spellcheck={false}
                onInput={(e) => setName((e.target as HTMLInputElement).value)}
              />
            </div>
            <span class="field-hint">
              {slug
                ? git && worktree
                  ? (
                    <>
                      Branch <code>muster/{slug}</code> in a new worktree
                    </>
                  )
                  : (
                    <>
                      Runs in <code>{w ? tildify(w.path, meta.value.home) : "…"}</code>
                    </>
                  )
                : "Lowercase words joined by dashes work best"}
            </span>
          </label>
          <div class="field-row">
            <label class="field grow">
              <span class="field-label">Workspace</span>
              <select value={wid} onChange={(e) => setWid(Number((e.target as HTMLSelectElement).value))}>
                {ws.map((x) => <option key={x.id} value={x.id}>{x.name}</option>)}
              </select>
            </label>
            {git && (
              <label class="field grow">
                <span class="field-label">
                  Base branch <span class="muted">(optional)</span>
                </span>
                <input
                  value={base}
                  placeholder="current HEAD"
                  spellcheck={false}
                  disabled={!worktree}
                  onInput={(e) => setBase((e.target as HTMLInputElement).value)}
                />
              </label>
            )}
          </div>
          <div class="field">
            <span class="field-label">Start with</span>
            <div class="seg seg-lg" role="radiogroup">
              {AGENTS.map((a) => (
                <button
                  type="button"
                  role="radio"
                  aria-checked={agent === a.k}
                  class={cx("seg-btn", agent === a.k && "on")}
                  onClick={() => setAgent(a.k)}
                >
                  {a.icon} {a.label}
                </button>
              ))}
            </div>
          </div>
          {git && (
            <label class="toggle">
              <input
                type="checkbox"
                checked={worktree}
                onChange={(e) => setWorktree((e.target as HTMLInputElement).checked)}
              />
              <span class="toggle-track">
                <span class="toggle-thumb" />
              </span>
              <span>
                Create a git worktree <span class="muted">(isolated checkout on its own branch)</span>
              </span>
            </label>
          )}
          {(agent === "claude" || agent === "codex") && (
            <label class="field">
              <span class="field-label">
                Initial prompt <span class="muted">(optional)</span>
              </span>
              <textarea
                rows={3}
                value={prompt}
                placeholder="What should the agent do?"
                onInput={(e) => setPrompt((e.target as HTMLTextAreaElement).value)}
              />
            </label>
          )}
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          <span class="muted dlg-foot-hint">
            <Kbd k="Ctrl+Enter" /> create
          </span>
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!canCreate}>
            {busy ? <Spinner size={12} /> : null} Create channel
          </button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- add workspace ----------------

function AddWorkspace() {
  const [path, setPath] = useState("");
  const [name, setName] = useState("");
  const [sugg, setSugg] = useState<string[]>([]);
  const [sel, setSel] = useState(-1);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const ref = useRef<HTMLInputElement>(null);
  const seq = useRef(0);
  useLayoutEffect(() => ref.current?.focus(), []);

  useEffect(() => {
    const prefix = path || "~/";
    const my = ++seq.current;
    const t = setTimeout(() => {
      rpc.call<{ entries: string[] }>("fs.complete", { prefix })
        .then((r) => {
          if (my !== seq.current) return;
          setSugg(r.entries.slice(0, 50));
          setSel(-1);
        })
        .catch(() => my === seq.current && setSugg([]));
    }, 90);
    return () => clearTimeout(t);
  }, [path]);

  const complete = (v: string) => {
    const withSlash = v.endsWith("/") ? v : v + "/";
    setPath(withSlash);
    ref.current?.focus();
  };

  const submit = async () => {
    const p = path.trim().replace(/\/+$/, "") || path.trim();
    if (!p || busy) return;
    setBusy(true);
    setErr(null);
    try {
      const w = await A.addWorkspace(p, name.trim() || undefined);
      toast("success", `Added ${w.name}`);
      overlay.value = { kind: "new_channel", workspaceId: w.id };
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  // Common prefix of suggestions for Tab completion.
  const common = (() => {
    if (!sugg.length) return "";
    let p = sugg[0];
    for (const s of sugg) while (!s.startsWith(p)) p = p.slice(0, -1);
    return p;
  })();

  return (
    <Modal class="dialog" label="Add workspace">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <div class="dlg-head">
          <h2>Add workspace</h2>
          <span class="muted">A git repo or any folder. Channels get worktrees inside git repos.</span>
        </div>
        <div class="dlg-body">
          <label class="field">
            <span class="field-label">Path</span>
            <div class="path-box">
              <input
                ref={ref}
                value={path}
                placeholder="~/projects/my-repo"
                spellcheck={false}
                autoComplete="off"
                onInput={(e) => setPath((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => {
                  if (e.key === "Tab" && !e.shiftKey) {
                    e.preventDefault();
                    if (sel >= 0 && sugg[sel]) complete(sugg[sel]);
                    else if (sugg.length === 1) complete(sugg[0]);
                    else if (common && common.length > path.length) setPath(common);
                    else if (sugg.length) setSel(0);
                  } else if (e.key === "ArrowDown" && sugg.length) {
                    e.preventDefault();
                    setSel((sel + 1) % sugg.length);
                  } else if (e.key === "ArrowUp" && sugg.length) {
                    e.preventDefault();
                    setSel((sel - 1 + sugg.length) % sugg.length);
                  } else if (e.key === "Enter" && sel >= 0 && sugg[sel]) {
                    e.preventDefault();
                    complete(sugg[sel]);
                  }
                }}
              />
              {sugg.length > 0 &&
                !(sugg.length === 1 && sugg[0].replace(/\/$/, "") === path.replace(/\/$/, "")) && (
                <div class="path-sugg">
                  {sugg.map((s, i) => (
                    <div
                      key={s}
                      class={cx("path-item", i === sel && "sel")}
                      onMouseDown={(e) => {
                        e.preventDefault();
                        complete(s);
                      }}
                    >
                      <IconFolder size={13} />
                      <span>{s}</span>
                    </div>
                  ))}
                </div>
              )}
            </div>
            <span class="field-hint">
              <kbd>Tab</kbd> completes · <kbd>↑</kbd>
              <kbd>↓</kbd> pick
            </span>
          </label>
          <label class="field">
            <span class="field-label">
              Name <span class="muted">(optional)</span>
            </span>
            <input
              value={name}
              placeholder={path.replace(/\/+$/, "").split("/").pop() || "defaults to the folder name"}
              onInput={(e) => setName((e.target as HTMLInputElement).value)}
            />
          </label>
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!path.trim() || busy}>
            {busy ? <Spinner size={12} /> : null} Add workspace
          </button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- shortcuts ----------------

const EXTRA: { title: string; keys: string[]; group: string }[] = [
  { title: "Focus pane in direction (not in text fields)", keys: ["Alt+←↑→↓"], group: "Panes" },
  { title: "Resize focused pane", keys: ["Alt+Shift+←↑→↓"], group: "Panes" },
  { title: "Jump to channel N", keys: ["Alt+1", "Alt+9"], group: "Navigate" },
  { title: "Copy / paste in terminal", keys: ["Ctrl+Shift+C", "Ctrl+Shift+V"], group: "View" },
  { title: "Send & interrupt (composer)", keys: ["Ctrl+Enter"], group: "Session" },
  { title: "Close tab", keys: ["Middle-click"], group: "Panes" },
  { title: "Rename tab or channel", keys: ["Double-click"], group: "Panes" },
  { title: "Move tab / new split", keys: ["Drag tab"], group: "Panes" },
  { title: "Equalize splits", keys: ["Double-click gutter"], group: "Panes" },
];

type ScGroup = [string, { title: string; keys: string[] }[]];

/** Split groups into two columns of about the same height (rows + a header each). */
function balance(groups: ScGroup[]): [ScGroup[], ScGroup[]] {
  const weight = (g: ScGroup) => g[1].length + 1.5;
  const cols: [ScGroup[], ScGroup[]] = [[], []];
  const h = [0, 0];
  // Biggest groups first into the shorter column, then restore sheet order in each column.
  for (const g of [...groups].sort((a, b) => weight(b) - weight(a))) {
    const i = h[0] <= h[1] ? 0 : 1;
    cols[i].push(g);
    h[i] += weight(g);
  }
  const order = (c: ScGroup[]) => c.sort((a, b) => groups.indexOf(a) - groups.indexOf(b));
  return [order(cols[0]), order(cols[1])];
}

function Shortcuts() {
  const groups = new Map<string, { title: string; keys: string[] }[]>();
  for (const c of commands) {
    if (c.hidden || !c.keys?.length) continue;
    const g = groups.get(c.group) ?? [];
    g.push({ title: c.title, keys: c.keys });
    groups.set(c.group, g);
  }
  for (const e of EXTRA) {
    const g = groups.get(e.group) ?? [];
    g.push(e);
    groups.set(e.group, g);
  }
  const altTerm = prefs.value.altToTerminal;
  const cols = balance([...groups]);
  return (
    <Modal class="shortcuts" label="Keyboard shortcuts">
      <div class="dlg-head">
        <h2>Keyboard shortcuts</h2>
        <span class="muted">
          {altTerm
            ? "Terminal gets Alt keys is on: in a terminal only Alt+Shift+… and Ctrl+Shift+… shortcuts apply."
            : "App shortcuts work even while a terminal has focus. Text fields keep Alt+letters and Alt+arrows."}
        </span>
      </div>
      <div class="sc-grid">
        {cols.map((col, ci) => (
          <div class="sc-col" key={ci}>
            {col.map(([g, list]) => (
              <section class="sc-group" key={g}>
                <h3>{g}</h3>
                {list.map((it) => (
                  <div class="sc-row" key={it.title}>
                    <span>{it.title}</span>
                    <span class="sc-keys">
                      {it.keys.map((k, i) => (
                        <>
                          {i > 0 && <span class="sc-or">{it.title === "Jump to channel N" ? "…" : "or"}
                          </span>}
                          <Kbd k={k} />
                        </>
                      ))}
                    </span>
                  </div>
                ))}
              </section>
            ))}
          </div>
        ))}
      </div>
      <div class="sc-foot">
        <span>
          Shells and agents use Alt for word motion. Command palette → <b>Terminal gets Alt keys</b>{" "}
          hands them over ({altTerm ? "on" : "off"}).
        </span>
      </div>
    </Modal>
  );
}

// ---------------- confirm ----------------

function Confirm(
  props: { title: string; body: string; confirmLabel: string; danger?: boolean; run: () => void },
) {
  const ok = useRef<HTMLButtonElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  // A stray Enter must not destroy anything: destructive confirms start on Cancel.
  useLayoutEffect(() => (props.danger ? cancel : ok).current?.focus(), []);
  return (
    <Modal class="dialog confirm-dlg" label={props.title}>
      <div class="dlg-head">
        <h2>{props.title}</h2>
      </div>
      <div class="dlg-body">
        <p>{props.body}</p>
      </div>
      <div class="dlg-foot">
        <button ref={cancel} type="button" class="btn" onClick={close}>Cancel</button>
        <button
          ref={ok}
          type="button"
          class={cx("btn", props.danger ? "btn-danger" : "btn-primary")}
          onClick={() => {
            overlay.value = null;
            props.run();
          }}
        >
          {props.confirmLabel}
        </button>
      </div>
    </Modal>
  );
}

// ---------------- toasts ----------------

export function Toasts() {
  const list = toasts.value;
  return (
    <div class="toasts" role="status" aria-live="polite">
      {list.map((t) => (
        <div key={t.id} class={cx("toast", "toast-" + t.level)}>
          {t.level === "success" ? <IconCheck size={14} /> : <span class="toast-dot" />}
          <span class="toast-text">{t.text}</span>
          <button type="button" class="toast-x" aria-label="Dismiss" onClick={() => dismissToast(t.id)}>
            <IconX size={12} />
          </button>
        </div>
      ))}
    </div>
  );
}
