# Owner's open list

Things that need Josh, but not right now. Delete a line when it is done.

## Decisions
- Working dot animation adds ~8 ms to every echo while agents are busy (Chromium aligns
  WebSocket delivery to frames). Make it static? Numbers: scratchpad lag/lag-numbers.md of
  the 2026-10-18 session; summary in /tmp/muster-terminal-host/docs/TERMINAL_HOST.md.
- Backport the two master-side lag fixes (TCP_NODELAY on the UI socket, direct xterm
  writes) to master as a small branch, or wait for refactor/terminal-host?

## Whenever
- Roll out refactor/terminal-host: run `muster stop` on master first (rollback boundary
  in /tmp/muster-terminal-host/docs/TERMINAL_HOST.md).
