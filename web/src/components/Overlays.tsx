// Modal overlays: Jump / command palette, New tab, Rename window, shortcuts sheet, confirm
// dialog. Plus toasts.
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  channels,
  dismissToast,
  meta,
  myWindow,
  overlay,
  prefs,
  sessions,
  sessionsByChannel,
  tabIdSet,
  tabs,
  toasts,
  windowById,
  windowOf,
  windows,
  workspaceById,
  workspaces,
} from "../state.ts";
import * as A from "../actions.ts";
import { type Command, commands } from "../keys.ts";
import { Kbd, Spinner, StateDot } from "./common.tsx";
import {
  IconBranch,
  IconCheck,
  IconCodex,
  IconFolder,
  IconSpark,
  IconTerminal,
  IconX,
  KindIcon,
} from "../icons.tsx";
import type { Permissions, SessionKind } from "../types.ts";
import { DEFAULT_PERMISSIONS, needsHuman, STATE_LABEL, STATE_PRIORITY } from "../types.ts";
import { PermissionsDialog, PermissionsSection } from "./Permissions.tsx";
import { FolderInput } from "./FolderInput.tsx";
import { aggregateState, basename, type JumpCandidate, rankJump, tabLabel, windowLabel } from "../model.ts";
import { cx, fuzzy, tildify } from "../util.ts";

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
    case "jump":
      return <Jump key={o.initial} initial={o.initial} />;
    case "new_tab":
      return <NewTab worktree={o.worktree} cwd={o.cwd} />;
    case "rename_window":
      return <RenameWindow />;
    case "shortcuts":
      return <Shortcuts />;
    case "confirm":
      return <Confirm {...o} />;
    case "permissions":
      return <PermissionsDialog key={o.channelId} channelId={o.channelId} Modal={Modal} close={close} />;
  }
}

// ---------------- Jump / command palette ----------------

interface Entry extends JumpCandidate {
  sub?: string;
  icon: ComponentChildren;
  right?: ComponentChildren;
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

function runCommand(c: Command) {
  overlay.value = null;
  c.run();
  if (!overlay.value && !A.findOpen.value) A.focusActiveTerminalSoon();
}

function commandEntries(): Entry[] {
  const out: Entry[] = [];
  for (const c of commands) {
    if (c.hidden) continue;
    if (c.enabled && !c.enabled()) continue;
    out.push({
      key: "cmd:" + c.id,
      label: c.title,
      hay: c.title,
      needs: false,
      sub: c.group,
      icon: <span class="cmd-icon">›</span>,
      right: c.keys?.[0] ? <Kbd k={c.keys[0]} /> : null,
      run: () => runCommand(c),
    });
  }
  return out;
}

/** Tabs and sessions of every window, and folders for a new tab. */
function jumpEntries(): Entry[] {
  const out: Entry[] = [];
  const home = meta.value.home;
  const here = tabIdSet.value;
  const winName = (id: number | null) => {
    const w = id !== null ? windowById.value.get(id) : undefined;
    return w ? windowLabel(w) : "no window";
  };
  const live = channels.value.filter((c) => !c.archived);
  // This window's tabs first, in tab order, then other windows'.
  // This window's tabs in tab order, then other windows' (by window, then age).
  const tabPos = new Map(tabs.value.map((c, i) => [c.id, i]));
  const ordered = [...live].sort((a, b) =>
    Number(here.has(b.id)) - Number(here.has(a.id)) ||
    (tabPos.get(a.id) ?? 0) - (tabPos.get(b.id) ?? 0) ||
    (windowOf(a) ?? 0) - (windowOf(b) ?? 0) || a.id - b.id
  );
  const tabOf = new Map<number, string>();
  for (const c of ordered) {
    const list = sessionsByChannel.value.get(c.id) ?? [];
    const label = tabLabel(c, workspaceById.value.get(c.workspace_id)).text;
    tabOf.set(c.id, label);
    const isHere = here.has(c.id);
    const where = isHere ? tildify(c.cwd, home) : `${winName(windowOf(c))} · ${tildify(c.cwd, home)}`;
    // A tab with one session is that session: one row, which carries its urgency.
    const only = list.length === 1 ? list[0] : null;
    const agg = aggregateState(list);
    const detail = only
      ? only.state_detail ?? (agg ? STATE_LABEL[agg] : "")
      : agg
      ? STATE_LABEL[agg]
      : "empty";
    out.push({
      key: "c:" + c.id,
      label,
      hay: `${label} ${basename(c.cwd)} ${c.branch ?? ""}`,
      needs: !!only && needsHuman(only.state),
      pri: only ? STATE_PRIORITY.indexOf(only.state) : undefined,
      since: only?.state_since,
      here: isHere,
      boost: 2,
      sub: where,
      icon: <StateDot state={agg} />,
      right: <span class="sw-detail">{detail}</span>,
      run: () => {
        overlay.value = null;
        A.gotoChannel(c.id, only?.id);
      },
    });
  }
  for (const c of ordered) {
    const list = sessionsByChannel.value.get(c.id) ?? [];
    if (list.length < 2) continue;
    const isHere = here.has(c.id);
    for (const s of list) {
      out.push({
        key: "s:" + s.id,
        label: s.name,
        hay: `${s.name} ${tabOf.get(c.id)}`,
        needs: needsHuman(s.state),
        pri: STATE_PRIORITY.indexOf(s.state),
        since: s.state_since,
        here: isHere,
        sub: isHere ? tabOf.get(c.id) : `${tabOf.get(c.id)} · ${winName(windowOf(c))}`,
        icon: <KindIcon kind={s.kind} size={14} />,
        right: (
          <span class="sw-state">
            <StateDot state={s.state} /> {s.state_detail || STATE_LABEL[s.state]}
          </span>
        ),
        run: () => {
          overlay.value = null;
          A.gotoChannel(c.id, s.id);
        },
      });
    }
  }
  const folders = [...prefs.value.recentFolders];
  for (const w of workspaces.value) if (!folders.includes(w.path)) folders.push(w.path);
  for (const f of folders) {
    const name = basename(f);
    out.push({
      key: "f:" + f,
      label: name,
      hay: `${name} ${f}`,
      needs: false,
      boost: -3,
      sub: `New tab in ${tildify(f, home)}`,
      icon: <IconFolder size={14} />,
      right: <span class="sw-detail">new tab</span>,
      run: () => {
        overlay.value = null;
        A.createTab({
          path: f,
          agent: prefs.value.lastAgent === "none" ? null : prefs.value.lastAgent,
          worktree: false,
        })
          .catch((e) => A.fail("Couldn't open a tab")(e)).catch(() => {});
      },
    });
  }
  return out;
}

function Jump({ initial }: { initial: string }) {
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

  const entries = useMemo((): { e: Entry; hits: number[] }[] => {
    if (cmdMode) {
      const query = q.slice(1).trim();
      const list = commandEntries();
      if (!query) return list.map((e) => ({ e, hits: [] }));
      return list
        .map((e) => {
          const hits: number[] = [];
          const s = fuzzy(query, e.hay, hits);
          return s === null ? null : { e, hits, s };
        })
        .filter((x): x is { e: Entry; hits: number[]; s: number } => !!x)
        .sort((a, b) => b.s - a.s);
    }
    const query = q.trim();
    const items = jumpEntries();
    const ranked = rankJump(items, query, prefs.value.recent).map((r) => ({ e: r.item, hits: r.hits }));
    // Commands also match plain queries, always after everything else.
    if (!query) return ranked;
    const cmds = rankJump(commandEntries(), query, [], 10).map((r) => ({ e: r.item, hits: r.hits }));
    return [...ranked, ...cmds];
    // Live: states and tabs change while Jump is open.
  }, [q, sessions.value, channels.value, windows.value]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    listRef.current?.querySelector(".sw-item.sel")?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  const onKey = (e: KeyboardEvent) => {
    const n = Math.max(1, entries.length);
    if (e.key === "ArrowDown" || (e.key === "n" && e.ctrlKey) || (e.key === "Tab" && !e.shiftKey)) {
      e.preventDefault();
      setSel((sel + 1) % n);
    } else if (e.key === "ArrowUp" || (e.key === "p" && e.ctrlKey) || (e.key === "Tab" && e.shiftKey)) {
      e.preventDefault();
      setSel((sel - 1 + n) % n);
    } else if (e.key === "Enter") {
      e.preventDefault();
      entries[sel]?.e.run();
    }
  };

  return (
    <Modal class="switcher" label={cmdMode ? "Command palette" : "Jump"}>
      <div class="sw-input-row">
        {cmdMode ? <span class="sw-mode">Commands</span> : <SearchGlyph />}
        <input
          ref={inputRef}
          class="sw-input"
          value={q}
          placeholder={cmdMode ? "Type a command…" : "Jump to a tab, session or folder…   (> for commands)"}
          spellcheck={false}
          aria-label={cmdMode ? "Command" : "Jump to"}
          onInput={(e) => setQ((e.target as HTMLInputElement).value)}
          onKeyDown={onKey}
        />
      </div>
      <div class="sw-list" ref={listRef} role="listbox">
        {entries.length === 0 && <div class="sw-empty">No matches</div>}
        {entries.map(({ e, hits }, i) => (
          <div
            key={e.key}
            role="option"
            aria-selected={i === sel}
            class={cx("sw-item", i === sel && "sel", e.needs && "needs")}
            onMouseMove={() => i !== sel && setSel(i)}
            onClick={() => e.run()}
          >
            <span class="sw-icon">{e.icon}</span>
            <span class="sw-label">
              <Highlight text={e.label} hits={hits} />
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
        {!cmdMode && myWindow.value && (
          <span class="sw-foot-note">
            {prefs.value.jumpRaises ? "Tabs elsewhere: focus their window" : "Tabs elsewhere move here"}
          </span>
        )}
      </div>
    </Modal>
  );
}

function SearchGlyph() {
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

// ---------------- new tab ----------------

const AGENTS: { k: SessionKind | "none"; label: string; icon: ComponentChildren }[] = [
  { k: "claude", label: "Claude", icon: <IconSpark size={14} /> },
  { k: "codex", label: "Codex", icon: <IconCodex size={14} /> },
  { k: "shell", label: "Shell", icon: <IconTerminal size={14} /> },
  { k: "none", label: "None", icon: <IconX size={14} /> },
];

function NewTab({ worktree: wtInit, cwd }: { worktree?: boolean; cwd?: string }) {
  const home = meta.value.home;
  const [folder, setFolder] = useState(tildify(cwd ?? A.defaultFolder(), home));
  const [agent, setAgent] = useState<SessionKind | "none">(prefs.value.lastAgent);
  const [worktree, setWorktree] = useState(!!wtInit);
  const [base, setBase] = useState("");
  const [name, setName] = useState("");
  const [prompt, setPrompt] = useState("");
  const [permissions, setPermissions] = useState<Permissions>({ ...DEFAULT_PERMISSIONS });
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const folderRef = useRef<HTMLInputElement>(null);

  const expand = (p: string) => (home ? p.trim().replace(/^~(?=\/|$)/, home) : p.trim()).replace(/\/+$/, "");
  const known = workspaces.value.find((w) => w.path.replace(/\/+$/, "") === expand(folder));
  const notGit = !!known && !known.is_git;
  const recents = prefs.value.recentFolders.filter((f) => f !== expand(folder)).slice(0, 5);
  const canCreate = !!folder.trim() && !busy;

  const create = async () => {
    if (!canCreate) return;
    setBusy(true);
    setErr(null);
    try {
      await A.createTab({
        path: folder.trim(),
        workspace_id: known?.id,
        name: name.trim() || undefined,
        worktree: worktree && !notGit,
        base: worktree ? base.trim() || undefined : undefined,
        agent: agent === "none" ? null : agent,
        prompt: agent === "claude" || agent === "codex" ? prompt.trim() || undefined : undefined,
        permissions,
      });
      overlay.value = null;
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  return (
    <Modal class="dialog dialog-scroll" label="New tab">
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
          <h2>New tab</h2>
          <span class="muted">A folder and what to start in it. The tab names itself.</span>
        </div>
        <div class="dlg-body">
          <label class="field">
            <span class="field-label">Folder</span>
            <FolderInput value={folder} onChange={setFolder} autoFocus inputRef={folderRef} />
            {recents.length > 0 && (
              <span class="recent-folders">
                {recents.map((f) => (
                  <button
                    type="button"
                    key={f}
                    class="recent-folder"
                    data-tip={tildify(f, home)}
                    onClick={() => {
                      setFolder(tildify(f, home));
                      folderRef.current?.focus();
                    }}
                  >
                    <IconFolder size={12} /> {basename(f)}
                  </button>
                ))}
              </span>
            )}
          </label>
          <div class="field">
            <span class="field-label">Start with</span>
            <div class="seg seg-lg" role="radiogroup" aria-label="Start with">
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
          <label class={cx("toggle", notGit && "disabled")}>
            <input
              type="checkbox"
              checked={worktree && !notGit}
              disabled={notGit}
              onChange={(e) => setWorktree((e.target as HTMLInputElement).checked)}
            />
            <span class="toggle-track">
              <span class="toggle-thumb" />
            </span>
            <span>
              Git worktree{" "}
              <span class="muted">
                {notGit ? "(this folder isn't a git repository)" : "(an isolated checkout on its own branch)"}
              </span>
            </span>
          </label>
          {worktree && !notGit && (
            <label class="field">
              <span class="field-label">
                Base branch <span class="muted">(optional)</span>
              </span>
              <input
                value={base}
                placeholder="current HEAD"
                spellcheck={false}
                onInput={(e) => setBase((e.target as HTMLInputElement).value)}
              />
            </label>
          )}
          <label class="field">
            <span class="field-label">
              Name <span class="muted">(optional)</span>
            </span>
            <input
              value={name}
              placeholder="Automatic: from the agent's task or first prompt"
              spellcheck={false}
              onInput={(e) => setName((e.target as HTMLInputElement).value)}
            />
          </label>
          {(agent === "claude" || agent === "codex") && (
            <label class="field">
              <span class="field-label">
                First prompt <span class="muted">(optional)</span>
              </span>
              <textarea
                rows={3}
                value={prompt}
                placeholder="What should the agent do?"
                onInput={(e) => setPrompt((e.target as HTMLTextAreaElement).value)}
              />
            </label>
          )}
          <PermissionsSection value={permissions} onChange={setPermissions} />
          {err && <div class="form-error">{err}</div>}
        </div>
        <div class="dlg-foot">
          <span class="muted dlg-foot-hint">
            <Kbd k="Ctrl+Enter" /> create
          </span>
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!canCreate}>
            {busy ? <Spinner size={12} /> : worktree && !notGit ? <IconBranch size={13} /> : null} Open tab
          </button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- rename window ----------------

function RenameWindow() {
  const w = myWindow.value;
  const [name, setName] = useState(w?.name ?? "");
  const ref = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  if (!w) return null;
  return (
    <Modal class="dialog confirm-dlg" label="Rename window">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          A.renameWindow(name);
          close();
        }}
      >
        <div class="dlg-head">
          <h2>Rename window</h2>
          <span class="muted">
            The title becomes <code>muster · {name.trim() || w.name}</code>, which window managers can match.
          </span>
        </div>
        <div class="dlg-body">
          <label class="field">
            <span class="field-label">Name</span>
            <input
              ref={ref}
              value={name}
              spellcheck={false}
              onInput={(e) => setName((e.target as HTMLInputElement).value)}
            />
          </label>
        </div>
        <div class="dlg-foot">
          <button type="button" class="btn" onClick={close}>Cancel</button>
          <button type="submit" class="btn btn-primary" disabled={!name.trim()}>Rename</button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- shortcuts ----------------

const EXTRA: { title: string; keys: string[]; group: string }[] = [
  { title: "Go to tab N", keys: ["Alt+1", "Alt+9"], group: "Navigate" },
  { title: "Focus pane in direction (not in text fields)", keys: ["Alt+←↑→↓"], group: "Panes" },
  { title: "Resize focused pane", keys: ["Alt+Shift+←↑→↓"], group: "Panes" },
  { title: "Copy / paste in terminal", keys: ["Ctrl+Shift+C", "Ctrl+Shift+V"], group: "View" },
  { title: "Close a tab or pane-tab", keys: ["Middle-click"], group: "Tab" },
  { title: "Rename a tab or session", keys: ["Double-click"], group: "Tab" },
  { title: "Reorder, or move to another window", keys: ["Drag tab"], group: "Tab" },
  { title: "Move a session / new split", keys: ["Drag pane header"], group: "Panes" },
  { title: "Pane menu (Shift for programs using the mouse)", keys: ["Right-click"], group: "Panes" },
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
                          {i > 0 && <span class="sc-or">{it.title === "Go to tab N" ? "…" : "or"}</span>}
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
          hands them over ({altTerm ? "on" : "off"}). Ctrl+Shift+T/N may be taken by the browser: Alt+T and
          Alt+Shift+N always work.
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
