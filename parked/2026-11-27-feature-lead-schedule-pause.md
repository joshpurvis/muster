# Parked review findings: feature/lead-schedule-pause (issue #69)

One line each: date · finding · file · why parked.

- 2026-11-27 · Raw keys (`muster keys` / `session.input`) typed by scheduled or triggered work into a shell the human drives don't record that work's route on the shell, so the shell's next `muster schedule` command counts as the human's (pause any schedule, a narrowing edit, a held create or resume) · src/rpc.rs `session.input`, `agent_schedule_call` · pre-existing (this change doesn't widen it: such a shell already acted as the human), and closing it means recording provenance on raw input, a separate change to the provenance rules.
- 2026-11-27 · A proposal someone paused while it waited stays paused when approved, and the card doesn't say it is paused; with `replaces`, approving leaves both schedules paused while the approve toast names a next run · src/schedules/mod.rs `approve`, web/src/components/Schedules.tsx · needs a pause of a waiting proposal first (rare), nothing runs that shouldn't, and the owner sees both paused in the sidebar.
