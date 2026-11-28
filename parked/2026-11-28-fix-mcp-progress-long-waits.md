# Parked: fix/mcp-progress-long-waits (2026-11-28)

One Sol high review (correctness + reliability): P1 0, P2 2, P3 3. Fixed: reachability recheck during waits. Parked:

- 2026-11-28 · mcp-progress: a client that keeps the SSE socket open without reading can stall a wait_for_panel_event past its deadline, holding matching panel events back · src/mcp/mod.rs ~260, src/panels.rs ~898 · needs a client that stops reading; Claude Code reads the stream
- 2026-11-28 · mcp-progress: an ask_agent cancelled while waiting over 30 s for the start lock keeps its send charge though nothing was queued · src/mcp/tools.rs ~1737 · one send slot, only near the rate limit
- 2026-11-28 · mcp-progress: a 2025-11-25-era client that drops its SSE connection mid-wait gets the wait cancelled, which that revision advises against · src/mcp/mod.rs ~243 · deliberate: one rule for both protocol eras
- 2026-11-28 · mcp-progress: a panel click acknowledged just before the connection drops is gone on retry · src/panels.rs ~807 · existing panel delivery trade-off, not new
