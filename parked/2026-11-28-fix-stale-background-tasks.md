# Parked: fix/stale-background-tasks (2026-11-28)

One Sol high review (correctness + reliability): P1 3, P2 3, P3 1. Fixed: the three P1s (quoted notices, resumed sub-agents, delayed Stop snapshots) and the P3 (paths kept under the run's Claude projects folder). Parked, all keep a task counted rather than release it:

- 2026-11-28 · stale-bg: a Stop hook replayed after a backend outage stamps a sub-agent with restart time, so its real earlier completion notice is rejected and it stays counted until the process restarts · src/sleep.rs ~507/~549 · fails closed; needs an outage across a sub-agent's end
- 2026-11-28 · stale-bg: a background launch whose PostToolUse hook was lost, later stopped with TaskStop, stays counted (the stop names the task id the lost hook would have linked) · src/sleep.rs ~490/~651 · fails closed; needs a lost hook
- 2026-11-28 · stale-bg: a full scan of a large saved conversation (largest seen 201 MiB) runs under the agent start lock for a manual sleep or `--now` · src/sleep.rs ~625/~879 · no stall observed; only rows saved before this change take the full-scan path
- 2026-11-28 · stale-bg: a task missing from a Stop hook's background_tasks list is no longer treated as finished (no way to know the list postdates the launch), so a task whose end notice never arrives stays counted until restart · src/sleep.rs · decided on the safe side; completion comes from notices, TaskStop and SubagentStop
