import { useRef } from "preact/hooks";
import type { Channel } from "../types.ts";
import { layouts, meta, prefs, renaming, rightVisible, sessionsByChannel, workspaceById } from "../state.ts";
import * as A from "../actions.ts";
import { IconButton, InlineEdit } from "./common.tsx";
import { type MenuItem, openMenuAt } from "./Menu.tsx";
import { spawnMenuItems } from "../layout/Pane.tsx";
import { confirmArchive } from "./Sidebar.tsx";
import {
  IconArchive,
  IconBranch,
  IconChevronDown,
  IconCopy,
  IconDiff,
  IconFolder,
  IconMaximize,
  IconMore,
  IconPencil,
  IconPlus,
  IconRestart,
  IconSplitDown,
  IconSplitRight,
  IconTimeline,
  KindIcon,
} from "../icons.tsx";
import { cx, tildify } from "../util.ts";
import { KIND_LABEL } from "../types.ts";

export function ChannelHeader({ c }: { c: Channel }) {
  const r = renaming.value;
  const isRenaming = r?.kind === "channel" && r.id === c.id && r.where === "header";
  const ws = workspaceById.value.get(c.workspace_id);
  const p = prefs.value;
  const moreRef = useRef<HTMLButtonElement>(null);
  const newRef = useRef<HTMLDivElement>(null);
  const l = layouts.value.get(c.id);
  const last = p.lastAgent === "none" ? "shell" : p.lastAgent;
  const exited = (sessionsByChannel.value.get(c.id) ?? []).filter((s) => s.state === "exited");

  const moreItems = (): MenuItem[] => [
    {
      label: "Rename channel",
      icon: <IconPencil size={14} />,
      run: () => (renaming.value = { kind: "channel", id: c.id, where: "header" }),
    },
    { label: "Copy path", icon: <IconCopy size={14} />, run: () => A.copy(c.cwd, "Path copied") },
    ...(c.branch
      ? [{
        label: "Copy branch name",
        icon: <IconBranch size={14} />,
        run: () => A.copy(c.branch!, "Branch copied"),
      }]
      : []),
    {
      label: "Restart exited sessions",
      icon: <IconRestart size={14} />,
      disabled: !exited.length,
      hint: exited.length ? String(exited.length) : undefined,
      run: () => exited.forEach((s) => A.restartSession(s.id)),
    },
    "sep",
    {
      label: "Zoom pane",
      keys: "Alt+Z",
      icon: <IconMaximize size={14} />,
      run: A.zoomToggle,
      disabled: !l || l.root.type !== "split",
    },
    { label: "Equalize splits", run: A.equalizeAll, disabled: !l || l.root.type !== "split" },
    "sep",
    { label: "Archive…", icon: <IconArchive size={14} />, danger: true, run: () => confirmArchive(c, false) },
    ...(c.owns_worktree
      ? [{ label: "Archive + remove worktree…", danger: true, run: () => confirmArchive(c, true) }]
      : []),
  ];

  return (
    <div class="ch-header">
      <div class="ch-title">
        <span class="ch-title-hash">#</span>
        {isRenaming
          ? (
            <InlineEdit
              value={c.name}
              class="ch-title-edit"
              onCommit={(v) => {
                renaming.value = null;
                A.renameChannel(c.id, v);
              }}
              onCancel={() => (renaming.value = null)}
            />
          )
          : (
            <h1
              class="ch-name"
              onDblClick={() => (renaming.value = { kind: "channel", id: c.id, where: "header" })}
              data-tip="Double-click to rename"
            >
              {c.name}
            </h1>
          )}
      </div>
      <div class="ch-meta">
        {ws && <span class="chip chip-ws" data-tip={ws.path}>{ws.name}</span>}
        {c.branch && (
          <button
            type="button"
            class="chip chip-branch"
            data-tip={`Copy branch${c.base_branch ? ` · based on ${c.base_branch}` : ""}`}
            onClick={() => A.copy(c.branch!, "Branch copied")}
          >
            <IconBranch size={12} />
            <span>{c.branch}</span>
          </button>
        )}
        <button
          type="button"
          class="chip chip-path"
          data-tip={`Copy path · ports ${c.port_base}–${c.port_base + 9}`}
          onClick={() => A.copy(c.cwd, "Path copied")}
        >
          <IconFolder size={12} />
          <span class="path-rtl">
            <bdi>{tildify(c.cwd, meta.value.home)}</bdi>
          </span>
        </button>
      </div>
      <div class="ch-actions">
        <div class="split-btn" ref={newRef}>
          <button
            type="button"
            class="split-btn-main"
            data-tip={`New ${KIND_LABEL[last]} session`}
            onClick={() => A.spawnHere(last)}
          >
            <IconPlus size={14} />
            <KindIcon kind={last} size={13} />
          </button>
          <button
            type="button"
            class="split-btn-more"
            aria-label="Choose session type"
            data-tip="Choose session type"
            onClick={() =>
              openMenuAt(newRef.current!, [
                { header: "New session" },
                ...spawnMenuItems((k) => A.spawnHere(k)),
                "sep",
                { header: "In a new split" },
                {
                  label: "Split right…",
                  icon: <IconSplitRight size={14} />,
                  submenu: spawnMenuItems((k) => A.splitFocused("right", k)),
                },
                {
                  label: "Split down…",
                  icon: <IconSplitDown size={14} />,
                  submenu: spawnMenuItems((k) => A.splitFocused("bottom", k)),
                },
              ], { alignRight: true, minWidth: 200 })}
          >
            <IconChevronDown size={12} />
          </button>
        </div>
        <IconButton
          icon={<IconSplitRight size={16} />}
          tip="Split right"
          keys="Alt+D"
          onClick={() => A.splitFocused("right")}
        />
        <IconButton
          icon={<IconSplitDown size={16} />}
          tip="Split down"
          keys="Alt+Shift+D"
          onClick={() => A.splitFocused("bottom")}
        />
        <span class="divider-v" />
        <IconButton
          icon={<IconTimeline size={16} />}
          tip="Timeline"
          keys="Alt+L"
          active={rightVisible.value && p.rightTab === "timeline"}
          onClick={() => A.toggleRight("timeline")}
        />
        <IconButton
          icon={<IconDiff size={16} />}
          tip="Diff"
          keys="Alt+G"
          active={rightVisible.value && p.rightTab === "diff"}
          onClick={() => A.toggleRight("diff")}
        />
        <button
          type="button"
          ref={moreRef}
          class={cx("icon-btn")}
          aria-label="Channel menu"
          data-tip="More"
          onClick={() => openMenuAt(moreRef.current!, moreItems(), { alignRight: true, minWidth: 220 })}
        >
          <IconMore size={16} />
        </button>
      </div>
    </div>
  );
}
