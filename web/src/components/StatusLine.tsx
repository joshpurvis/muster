// Optional bottom line: the focused tab's task folder and branch (click copies), the number of
// tabs in this window, a "needs you" chip (same as Alt+J) and the connection dot.
import { focusedTask, meta, needsYou, windowSessions } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { IconBranch, IconFolder } from "../icons.tsx";
import { cx, tildify } from "../util.ts";

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
  const count = windowSessions.value.length;
  const cwd = c?.cwd ?? "";
  return (
    <footer class="statusline">
      {c && (
        <button
          type="button"
          class="sl-item sl-path"
          data-tip={`Copy path · ports ${c.port_base}–${c.port_base + 9}`}
          onClick={() => A.copy(cwd, "Path copied")}
        >
          <IconFolder size={12} />
          <span class="path-rtl">
            <bdi>{tildify(cwd, meta.value.home)}</bdi>
          </span>
        </button>
      )}
      {c?.branch && (
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
