// Modal overlays: Jump / command palette, New tab, Rename window, shortcuts sheet, confirm
// dialog, and the agent dialogs (Agents.tsx). Plus toasts.
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  agentBySession,
  agents,
  channelById,
  channels,
  dismissToast,
  hereIds,
  layoutOrder,
  meta,
  myWindow,
  overlay,
  prefs,
  sessions,
  toasts,
  windowById,
  windowOfSession,
  windows,
  workspaces,
} from "../state.ts";
import * as A from "../actions.ts";
import { type Command, commands } from "../keys.ts";
import { Kbd, Spinner, StateDot } from "./common.tsx";
import { IconBranch, IconCheck, IconCodex, IconFolder, IconSpark, IconTerminal, IconX } from "../icons.tsx";
import type { Permissions, SessionKind } from "../types.ts";
import { DEFAULT_PERMISSIONS, KIND_LABEL, needsHuman, STATE_LABEL, STATE_PRIORITY } from "../types.ts";
import { PermissionsDialog, PermissionsSection } from "./Permissions.tsx";
import { AgentDialog, MessageDialog } from "./Agents.tsx";
import { UpdateBanner } from "./UpdateBanner.tsx";
import { agentStatus } from "../agents.ts";
import { FolderInput } from "./FolderInput.tsx";
import { labelOf } from "../layout/Pane.tsx";
import { basename, type JumpCandidate, jumpOrder, rankJump, tabText, windowLabel } from "../model.ts";
import { cx, fuzzy, tildify } from "../util.ts";

const FOCUSABLE =
  'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [href], [tabindex]:not([tabindex="-1"])';

function close() {
  overlay.value = null;
  A.focusActiveTerminalSoon();
}

function Modal(
  { children, class: c, onClose = close, label, describedBy }: {
    children: ComponentChildren;
    class?: string;
    onClose?: () => void;
    label: string;
    /** Id of the text screen readers announce with the name (a confirm's question). */
    describedBy?: string;
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
      <div
        ref={ref}
        class={cx("modal", c)}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        aria-describedby={describedBy}
        tabIndex={-1}
      >
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
    case "shutdown":
      return <Shutdown />;
    case "shortcuts":
      return <Shortcuts />;
    case "confirm":
      return <Confirm {...o} />;
    case "permissions":
      return <PermissionsDialog key={o.channelId} channelId={o.channelId} Modal={Modal} close={close} />;
    case "agent":
      return <AgentDialog key={o.name ?? "new"} name={o.name} tab={o.tab} Modal={Modal} close={close} />;
    case "agent_message":
      return <MessageDialog key={o.id} id={o.id} Modal={Modal} close={close} />;
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

/** Every tab (session) of every window, and folders for a new tab. */
function jumpEntries(): Entry[] {
  const out: Entry[] = [];
  const home = meta.value.home;
  const here = hereIds.value;
  const winName = (id: number | null) => {
    const w = id !== null ? windowById.value.get(id) : undefined;
    return w ? windowLabel(w) : "no window";
  };
  // A named agent's session is listed once, as the agent (below).
  const live = [...sessions.value.values()].filter((s) => {
    const c = channelById.value.get(s.channel_id);
    return !!c && !c.archived && !agentBySession.value.has(s.id);
  });
  for (const s of jumpOrder(live, layoutOrder.value, windowOfSession)) {
    const c = channelById.value.get(s.channel_id)!;
    const label = tabText(labelOf(s));
    const isHere = here.has(s.id);
    const folder = tildify(c.cwd, home);
    out.push({
      key: "s:" + s.id,
      label,
      hay: `${label} ${basename(c.cwd)} ${c.branch ?? ""} ${KIND_LABEL[s.kind]}`,
      needs: needsHuman(s.state),
      pri: STATE_PRIORITY.indexOf(s.state),
      since: s.state_since,
      here: isHere,
      boost: 2,
      sub: isHere ? folder : `${winName(windowOfSession(s))} · ${folder}`,
      icon: <StateDot state={s.state} />,
      right: <span class="sw-detail">{s.state_detail || STATE_LABEL[s.state]}</span>,
      run: () => {
        overlay.value = null;
        A.gotoSession(s.id);
      },
    });
  }
  // Every named agent, stopped ones too: choosing one starts it and pulls its tab here.
  for (const a of agents.value) {
    const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
    const st = agentStatus(a, s);
    const isHere = !!s && here.has(s.id);
    out.push({
      key: "a:" + a.id,
      label: a.name,
      hay: `${a.name} agent ${KIND_LABEL[a.kind]}`,
      needs: !!s && needsHuman(s.state),
      pri: s ? STATE_PRIORITY.indexOf(s.state) : STATE_PRIORITY.length,
      since: st.since ?? 0,
      here: isHere,
      boost: 1,
      sub: `agent · ${KIND_LABEL[a.kind]}${
        s && !isHere && st.live ? ` · ${winName(windowOfSession(s))}` : ""
      }`,
      icon: <StateDot state={st.state} />,
      right: <span class="sw-detail">{st.live ? s?.state_detail || st.label : "start"}</span>,
      run: () => {
        overlay.value = null;
        A.openAgent(a.name);
      },
    });
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
        A.createTab({ path: f, agent: A.lastKind(), worktree: false })
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
  }, [q, sessions.value, channels.value, windows.value, layoutOrder.value, agents.value]);

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
          placeholder={cmdMode
            ? "Type a command…"
            : "Jump to a tab, agent or folder, in any window…   (> for commands)"}
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

const AGENTS: { k: SessionKind; label: string; icon: ComponentChildren }[] = [
  { k: "claude", label: "Claude", icon: <IconSpark size={14} /> },
  { k: "codex", label: "Codex", icon: <IconCodex size={14} /> },
  { k: "shell", label: "Shell", icon: <IconTerminal size={14} /> },
];

function NewTab({ worktree: wtInit, cwd }: { worktree?: boolean; cwd?: string }) {
  const home = meta.value.home;
  const [folder, setFolder] = useState(tildify(cwd ?? A.defaultFolder(), home));
  const [agent, setAgent] = useState<SessionKind>(A.lastKind());
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
        agent,
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
          <span class="muted">
            A folder and what to start in it, in the focused group. The tab names itself.
          </span>
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
  { title: "Focus group in direction (not in text fields)", keys: ["Alt+←↑→↓"], group: "Groups" },
  { title: "Resize focused group", keys: ["Alt+Shift+←↑→↓"], group: "Groups" },
  { title: "Copy / paste in terminal", keys: ["Ctrl+Shift+C", "Ctrl+Shift+V"], group: "View" },
  { title: "Close a tab", keys: ["Middle-click"], group: "Tab" },
  { title: "Rename (the names the tab shows)", keys: ["Double-click", "F2"], group: "Tab" },
  { title: "Reorder, move to a group, split or window", keys: ["Drag tab"], group: "Tab" },
  { title: "Group menu", keys: ["Right-click"], group: "Groups" },
  { title: "Equalize splits", keys: ["Double-click gutter"], group: "Groups" },
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
  props: {
    title: string;
    body: string | string[];
    confirmLabel: string;
    danger?: boolean;
    run: () => void;
    secondary?: { label: string; run: () => void };
  },
) {
  const ok = useRef<HTMLButtonElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  // A stray Enter must not destroy anything: destructive confirms start on Cancel.
  useLayoutEffect(() => (props.danger ? cancel : ok).current?.focus(), []);
  return (
    <Modal class="dialog confirm-dlg" label={props.title} describedBy="confirm-body">
      <div class="dlg-head">
        <h2>{props.title}</h2>
      </div>
      <div class="dlg-body" id="confirm-body">
        {(Array.isArray(props.body) ? props.body : [props.body]).map((text, i) => <p key={i}>{text}</p>)}
      </div>
      <div class="dlg-foot">
        <button ref={cancel} type="button" class="btn" onClick={close}>Cancel</button>
        {props.secondary && (
          <button
            type="button"
            class="btn"
            onClick={() => {
              // Back to the terminal, as Cancel does (the dialog's own focus memory pointed
              // at a button that is gone).
              close();
              props.secondary!.run();
            }}
          >
            {props.secondary.label}
          </button>
        )}
        <button
          ref={ok}
          type="button"
          class={cx("btn", props.danger ? "btn-danger" : "btn-primary")}
          onClick={() => {
            close();
            props.run();
          }}
        >
          {props.confirmLabel}
        </button>
      </div>
    </Modal>
  );
}

// ---------------- shut down ----------------

/** "Shut down Muster…": what `muster stop` does, from the window. Cancel is the default. */
function Shutdown() {
  const cancel = useRef<HTMLButtonElement>(null);
  const [busy, setBusy] = useState(false);
  useLayoutEffect(() => cancel.current?.focus(), []);
  const running = [...sessions.value.values()].filter((s) => s.state !== "exited");
  const agentCount = running.filter((s) => s.kind !== "shell").length;
  const n = running.length;
  return (
    <Modal class="dialog confirm-dlg" label="Shut down Muster" describedBy="shutdown-body">
      <div class="dlg-head">
        <h2>Shut down Muster?</h2>
      </div>
      <div class="dlg-body" id="shutdown-body">
        <p>
          This stops the Muster backend and its terminal host, in every window. Every terminal closes and every
          agent in them stops.
        </p>
        <p>
          {n === 0 ? "No sessions are running." : (
            <>
              <strong>{n === 1 ? "1 session is running" : `${n} sessions are running`}</strong>
              {agentCount === 0
                ? "."
                : agentCount === n
                ? (n === 1 ? " (an agent)." : " (all agents).")
                : ` (${agentCount} ${agentCount === 1 ? "agent" : "agents"}).`}
            </>
          )}
        </p>
        <p class="muted">Run <code>muster</code> to start it again.</p>
      </div>
      <div class="dlg-foot">
        <button ref={cancel} type="button" class="btn" onClick={close}>Cancel</button>
        <button
          type="button"
          class="btn btn-danger"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            A.shutDown().finally(() => (overlay.value = null));
          }}
        >
          {busy && <Spinner size={12} />}
          Shut down
        </button>
      </div>
    </Modal>
  );
}

// ---------------- toasts ----------------

export function Toasts() {
  const list = toasts.value;
  return (
    <div class="toasts">
      <UpdateBanner />
      <div class="toast-list" role="status" aria-live="polite">
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
    </div>
  );
}
