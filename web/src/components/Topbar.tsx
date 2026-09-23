import { inboxItems, overlay, prefs, view } from "../state.ts";
import { connStatus } from "../rpc.ts";
import * as A from "../actions.ts";
import { Kbd } from "./common.tsx";
import {
  IconInbox,
  IconKeyboard,
  IconMonitor,
  IconMoon,
  IconSearch,
  IconSidebar,
  IconSun,
} from "../icons.tsx";
import { cx } from "../util.ts";

export function Topbar() {
  const n = inboxItems.value.length;
  const theme = prefs.value.theme;
  const status = connStatus.value;
  return (
    <header class="topbar">
      <div class="topbar-left">
        <button
          type="button"
          class={cx("icon-btn", !prefs.value.sidebarCollapsed && "active-soft")}
          data-tip="Toggle sidebar"
          data-keys="Alt+B"
          aria-label="Toggle sidebar"
          onClick={A.toggleSidebar}
        >
          <IconSidebar size={16} />
        </button>
        <span class="brand">muster</span>
      </div>
      <button
        type="button"
        class="search-btn"
        onClick={() => (overlay.value = { kind: "switcher", initial: "" })}
      >
        <IconSearch size={14} />
        <span>Search or jump to…</span>
        <Kbd k="Ctrl+K" />
      </button>
      <div class="topbar-right">
        {status === "reconnecting" && (
          <span class="reconnect-pill" role="status">
            <span class="conn-dot" /> Reconnecting…
          </span>
        )}
        <button
          type="button"
          class={cx("icon-btn inbox-btn", view.value === "inbox" && "active", n > 0 && "attn")}
          data-tip={n ? `${n} need${n === 1 ? "s" : ""} you` : "Inbox"}
          data-keys="Alt+I"
          aria-label="Inbox"
          onClick={A.openInbox}
        >
          <IconInbox size={16} />
          {n > 0 && <span class="bell-count">{n}</span>}
        </button>
        <button
          type="button"
          class="icon-btn"
          data-tip={`Theme: ${theme}`}
          aria-label="Cycle theme"
          onClick={A.cycleTheme}
        >
          {theme === "light"
            ? <IconSun size={16} />
            : theme === "dark"
            ? <IconMoon size={16} />
            : <IconMonitor size={16} />}
        </button>
        <button
          type="button"
          class="icon-btn"
          data-tip="Keyboard shortcuts"
          data-keys="Alt+/"
          aria-label="Keyboard shortcuts"
          onClick={() => (overlay.value = { kind: "shortcuts" })}
        >
          <IconKeyboard size={16} />
        </button>
      </div>
    </header>
  );
}
