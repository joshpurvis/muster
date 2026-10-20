// Optional bottom line: the focused tab's task name, folder and branch (click copies), the number
// of tabs in this window, a "needs you" chip (same as Alt+J) and the connection dot.
import {
  accountProfiles,
  focusedSession,
  focusedTask,
  meta,
  needsYou,
  windowNotes,
  windowPanels,
  windowSessions,
  workspaceById,
} from "../state.ts";
import { profileName } from "../account_profiles.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { IconBranch, IconFolder } from "../icons.tsx";
import { cx, tildify } from "../util.ts";
import { taskLabel } from "../model.ts";

const CONN_LABEL = {
  open: "connected",
  connecting: "connecting…",
  reconnecting: "reconnecting…",
  no_token: "not signed in",
  incompatible: "UI update required",
} as const;

export function StatusLine() {
  const c = focusedTask.value;
  const n = needsYou.value.length;
  const status = connStatus.value;
  // Terminal, panel and note tabs alike.
  const count = windowSessions.value.length + windowPanels.value.length + windowNotes.value.length;
  const s = focusedSession.value;
  // Where the focused tab is now: a shell's `cd` or Claude's `/cd` moves it off the task folder.
  const live = s && c && s.channel_id === c.id ? s.live_cwd : undefined;
  const cwd = live ?? c?.cwd ?? "";
  const moved = !!c && cwd !== c.cwd;
  // Left the task's project altogether: its branch says nothing about this folder.
  const away = !!c && !!A.movedFolder(s ?? undefined, c, workspaceById.value.get(c.workspace_id));
  const taskFolder = c ? tildify(c.cwd, meta.value.home) : "";
  // Only a real name: until one is derived the task is named after its folder, shown next to it.
  const name = c ? taskLabel(c, workspaceById.value.get(c.workspace_id)) : null;
  return (
    <footer class="statusline">
      {c && name && !name.muted && (
        <button
          type="button"
          class="sl-item sl-task"
          data-tip={`Copy task name · ${name.text}`}
          onClick={() => A.copy(name.text, "Task name copied")}
        >
          <span>{name.text}</span>
        </button>
      )}
      {c && (
        <button
          type="button"
          class="sl-item sl-path"
          data-tip={`Copy path${moved ? ` · task folder ${taskFolder}` : ""} · ports ${
            c.port_base
          }–${c.port_base + 9}`}
          aria-label={`Copy path ${tildify(cwd, meta.value.home)}${moved ? `, task folder ${taskFolder}` : ""}`}
          onClick={() => A.copy(cwd, "Path copied")}
        >
          <IconFolder size={12} />
          <span class="path-rtl">
            <bdi>{tildify(cwd, meta.value.home)}</bdi>
          </span>
        </button>
      )}
      {c?.branch && !away && (
        <button
          type="button"
          class="sl-item sl-branch"
          data-tip={`Copy branch${c.base_branch ? ` · based on ${c.base_branch}` : ""}`}
          onClick={() => A.copy(c.branch!, "Branch copied")}
        >
          <IconBranch size={12} />
          <span>{c.branch}</span>
        </button>
      )}
      {count > 0 && <span class="sl-item sl-count">{count === 1 ? "1 tab" : `${count} tabs`}</span>}
      {focusedSession.value?.kind === "claude" && focusedSession.value.account_profile_id != null && (
        <span class="sl-item" data-tip="Claude configuration profile · verify the account with /status in Claude Code">
          {profileName(accountProfiles.value, focusedSession.value.account_profile_id)}
        </span>
      )}
      <span class="sl-space" />
      {n > 0 && (
        <button
          type="button"
          class="sl-needs"
          data-tip="Go to the next session that needs you"
          data-keys="Alt+J"
          onClick={() => A.nextNeedsYou()}
        >
          needs you: {n}
        </button>
      )}
      <span class={cx("sl-item sl-conn", "conn-" + status)} role="status">
        <span class="conn-dot" />
        {CONN_LABEL[status]}
      </span>
    </footer>
  );
}
