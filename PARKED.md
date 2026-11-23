# Parked review findings

Findings that missed the "fix now" bar (live and broken, wrong data a person relies on, or a
security or privacy problem). One line each: date, finding, file, why parked. Nobody starts work
from this list without the owner asking.

- 2026-11-23 · Archived sessions: archiving a channel while its first tab is still starting can list a tab whose launch then fails · src/hub.rs remove_session · needs a start that fails after the archive began; never seen.
- 2026-11-23 · Archived sessions: a named agent moved to another Claude profile whose next start fails, then removed, is listed under the new profile · src/hub.rs remove_session · removal now forgets the agent's rows, so it only shows until then; rare.
- 2026-11-23 · Archived sessions: day headings don't roll over at midnight while the list sits open untouched · web/src/components/EndedSessions.tsx · any key or reload fixes it.
- 2026-11-23 · Archived sessions: the mock archives a channel's tabs without adding rows and drops the chosen Claude profile on spawn · web/dev/mock.ts · dev only.
- 2026-11-23 · Archived sessions: empty and error states aren't announced to screen readers (no live region) · web/src/components/EndedSessions.tsx · polish; the Notes list behaves the same.
- 2026-11-23 · Archived sessions: in a 700 px WebKitGTK window a sidebar scrollbar line shows through the dialog backdrop · web/src/styles.css · existing for every dialog over the sidebar, not new here.
- 2026-11-23 · Archived sessions: the light theme's search placeholder is low contrast (about 2.35:1) · web/src/styles.css .sw-input · shared by every switcher dialog; polish.
- 2026-11-23 · Archived sessions: a tab recovered after a terminal host restart and then closed can leave its old output file on disk · src/hub.rs retirement · existing retirement behaviour, not new here; the archive itself keeps no output.
- 2026-11-23 · Archived sessions: rows that age out of the 1000 may linger in SQLite free pages or the WAL until reused · src/store.rs · same as every other deleted row in Muster's database; local file only.
- 2026-11-23 · Antigravity tools: An Antigravity tab's startup step goes on to `/clear` and releases queued prompts when "✓ muster" never shows within 15 seconds, so the first prompt can run without Muster's tools. · src/inject.rs open_conversation · holding the prompts forever is worse; the tools arrive on the next fresh start.
- 2026-11-23 · Antigravity tools: The automatic `/mcp` on a fresh Antigravity tab starts the folder's own MCP servers before the first prompt. · src/inject.rs open_conversation · they start on the first prompt anyway, and the folder is already trusted.
- 2026-11-23 · Antigravity tools: A fresh launch's initial prompt is now pasted into the Antigravity box instead of passed on the command line, without control-character cleaning. · src/hub.rs Antigravity launch · that path is the human's own prompt from the UI or CLI; prompts from MCP spawns are already cleaned.
- 2026-11-23 · Antigravity tools: A backend restart in the middle of the startup step (after `/mcp` or `/clear` is typed) can leave the item stuck or repeat it. · src/inject.rs open_conversation · rare window; a restart with no record of the step now drops it.
- 2026-11-23 · Antigravity tools: The hidden startup step is not counted as queued, so `wait_for_state(idle)` can return while `/mcp` is still open. · src/inject.rs waiting · about 2 seconds, once per fresh start.
- 2026-11-23 · Antigravity tools: A draft whose text is exactly the current mode's placeholder is read as an empty box. · src/adapters/antigravity.rs composer_text · needs a human to type the placeholder word for word.
- 2026-11-23 · Antigravity tools: The one-time Antigravity persona update recognises Muster's own text by a substring and the last "## Messages" heading, so a hand-edited persona quoting that text could be rewritten. · src/agents.rs update_antigravity_answer · needs a persona that copies Muster's generated wording.
- 2026-11-23 · Antigravity tools: An unreadable or over-4 MiB Antigravity settings file, or an unusual HOME, makes the session's allow list Muster's rule alone, dropping the user's own allow rules for that session. · src/adapters/antigravity.rs user_allow_rules · fails toward fewer grants, not more; deny and ask rules still apply.
- 2026-11-23 · Antigravity tools: A fresh start that prepares an Antigravity agent's first conversation still moves parked human prompts to the new run. · src/inject.rs prepare_initial · behaviour from before this change; the new startup step no longer does it.
