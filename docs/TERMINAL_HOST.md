# Terminal host separation

Work in progress on `refactor/terminal-host`. The live installation is not part of
this migration or its tests. Builds use this worktree's `target/` directory.

## Ownership

- Terminal core/host: PTYs, process ownership and cleanup, raw input/output,
  terminal emulation, replay, output logs, and generic process lifecycle facts.
- Application: agents, adapters, tasks, worktrees, detection, prompt delivery,
  MCP, hooks, authorization policy, application persistence and window records.
- UI: windows, tabs, menus, layout interactions and terminal rendering.

The host must not import application types or access the application database.
Backend disconnect, restart or crash must not terminate or stop draining PTYs.
Host shutdown remains explicitly destructive. A host crash is a separate failure
domain; this work does not promise recovery of open PTYs after host failure.

## Delivery sequence

1. Extract application-independent terminal core and generic lifecycle events.
2. Add versioned private host protocol, standalone host and client, with bounded
   streams, process generations and retry-safe spawn identities.
3. Replace application-owned PTYs with host handles; reconcile inventory before
   autostart, restore credentials and avoid startup reaping of surviving PTYs.
4. Preserve queued prompts, attribution and hook delivery through backend outages;
   report ambiguous delivery rather than replaying potentially executed commands.
5. Separate backend restart from full shutdown, keep endpoint/helper paths stable,
   and support independently updated frontend assets.
6. Exercise crash/reconnect, spawn races, exited leaders with live descendants,
   replay overflow, terminal modes, authorization and protocol compatibility.

The first transport keeps the existing UI protocol and proxies terminal traffic
through the backend. Processes and output capture survive, while input/rendering
pause during reconnect. A direct UI-to-host stream is a subsequent step if fully
uninterrupted interaction is required; MCP availability is independent of that.

## Safety and rollout

Use `bash scripts/isolated.sh <command>` for checks. Every test invocation of a
Muster executable must specify temporary `MUSTER_RUNTIME_DIR`, `MUSTER_DATA_DIR`
and `MUSTER_PORT`, including helper commands and cleanup. Tests own child process
handles; never signal a PID based only on a discovery file. Never invoke the live
Muster CLI or install over the live executable during development.

Existing terminals in the original daemon require a scheduled migration after
they finish, or an explicitly authorized restart. They are not adopted by the new
host. Host compatibility is negotiated separately from application releases;
application updates must never automatically replace an incompatible live host.
