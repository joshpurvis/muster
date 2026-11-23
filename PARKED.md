# Parked review findings

Findings that missed the fix bar (live and broken now, wrong data, or a security or privacy problem). One line each:
date, finding, file, why parked. Nobody starts work from this list without the owner's ask.

- 2026-11-23: An Antigravity tab's startup step goes on to `/clear` and releases queued prompts when "✓ muster" never
  shows within 15 seconds, so the first prompt can run without Muster's tools. `src/inject.rs` (`open_conversation`).
  Parked: holding the prompts forever is worse; the tools arrive on the next fresh start.
- 2026-11-23: The automatic `/mcp` on a fresh Antigravity tab starts the folder's own MCP servers before the first
  prompt. `src/inject.rs` (`open_conversation`). Parked: they start on the first prompt anyway, and the folder is
  already trusted.
- 2026-11-23: A fresh launch's initial prompt is now pasted into the Antigravity box instead of passed on the command
  line, without control-character cleaning. `src/hub.rs` (Antigravity launch). Parked: that path is the human's own
  prompt from the UI or CLI; prompts from MCP spawns are already cleaned.
- 2026-11-23: A backend restart in the middle of the startup step (after `/mcp` or `/clear` is typed) can leave the
  item stuck or repeat it. `src/inject.rs` (`open_conversation`). Parked: rare window; a restart with no record of
  the step now drops it.
- 2026-11-23: The hidden startup step is not counted as queued, so `wait_for_state(idle)` can return while `/mcp` is
  still open. `src/inject.rs` (`waiting`). Parked: about 2 seconds, once per fresh start.
- 2026-11-23: A draft whose text is exactly the current mode's placeholder is read as an empty box.
  `src/adapters/antigravity.rs` (`composer_text`). Parked: needs a human to type the placeholder word for word.
- 2026-11-23: The one-time Antigravity persona update recognises Muster's own text by a substring and the last
  "## Messages" heading, so a hand-edited persona quoting that text could be rewritten. `src/agents.rs`
  (`update_antigravity_answer`). Parked: needs a persona that copies Muster's generated wording.
- 2026-11-23: An unreadable or over-4 MiB Antigravity settings file, or an unusual HOME, makes the session's allow
  list Muster's rule alone, dropping the user's own allow rules for that session. `src/adapters/antigravity.rs`
  (`user_allow_rules`). Parked: fails toward fewer grants, not more; deny and ask rules still apply.
- 2026-11-23: A fresh start that prepares an Antigravity agent's first conversation still moves parked human prompts
  to the new run. `src/inject.rs` (`prepare_initial`). Parked: behaviour from before this change; the new startup
  step no longer does it.
