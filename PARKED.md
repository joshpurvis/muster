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
