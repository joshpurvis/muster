// Optional bottom line: the active tab's folder and branch (click copies), its session count,
// a "needs you" chip (same as Alt+J) and the connection dot.
import { activeChannel, meta, needsYou, sessionsByChannel } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { IconBranch, IconFolder } from "../icons.tsx";
import { cx, tildify } from "../util.ts";

const CONN_LABEL = {
  open: "connected",
  connecting: "connecting…",
  reconnecting: "reconnecting…",
  no_token: "not signed in",
} as const;

export function StatusLine() {
  const c = activeChannel.value;
  const n = needsYou.value.length;
  const status = connStatus.value;
  const count = c ? (sessionsByChannel.value.get(c.id) ?? []).length : 0;
  return (
    <footer class="statusline">
      {c && (
        <button
          type="button"
          class="sl-item sl-path"
          data-tip={`Copy path · ports ${c.port_base}–${c.port_base + 9}`}
          onClick={() => A.copy(c.cwd, "Path copied")}
        >
          <IconFolder size={12} />
          <span class="path-rtl">
            <bdi>{tildify(c.cwd, meta.value.home)}</bdi>
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
      {c && <span class="sl-item sl-count">{count === 1 ? "1 session" : `${count} sessions`}</span>}
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
