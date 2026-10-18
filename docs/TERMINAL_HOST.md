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

## Verified checkpoints

- Extracted the existing PTY implementation into `muster-terminal-core`, removing
  its Hub and adapter dependencies. All original 212 Rust tests passed.
- Added a standalone `muster-terminal-host` and terminal-only protocol/client
  crate. Nine isolated process-boundary tests and two framing tests pass: client
  disconnect/replay, retry-safe spawn, generation fencing, descendant cleanup,
  singleton locks, protocol/instance rejection, explicit shutdown, autonomous
  terminal queries/log flushing, output floods, and host crash/restart.

### Application integration checkpoint

The backend now uses remote terminal handles and reconciles persisted launch
intents and credentials with the surviving host. Backend SIGTERM and SIGKILL no
longer terminate terminals. `muster reload` replaces only the backend; `muster stop`
explicitly stops both services. The initial application port is pinned across
backend restarts. Host logs are generation-specific and owned by the host.

Validation so far: 223 Rust tests, 11 focused backend survival checks in
`scripts/backend-smoke.py`, and all 154 checks in the updated full smoke suite.
The smoke suite exposed transient title loss with polling alone; the backend now
also consumes host title events. Run all checks through `scripts/isolated.sh`.

### Durable prompt recovery checkpoint

Agent provenance and active message attribution are saved before input is submitted.
Waiting prompts are saved before acknowledgement and restored for their original
terminal generation. A crash during typing or submission preserves an uncertain
record and pauses the queue. Desktop/mobile queue review and `muster queue TARGET`
show those records; explicit clearing never resends them or undoes terminal input.
Backend shutdown lets current submissions settle for up to eight seconds.

Validation: the Rust suite passed 227 tests, with one additional generation-fencing
unit test subsequently passing; all 154 full smoke checks, 18 focused backend crash
checks, and all 81 frontend tests pass. Frontend types and lint also pass.

Remaining before completion: hook spooling/deduplication; application/UI update compatibility and asset loading;
additional crash-window tests and code review, including retired-run cleanup,
slow/disconnected stream recovery, and bounded host resource retention. Existing
live terminals and the installed binary have not been changed or migrated.

### Restart safety checkpoint

Approval identities reserve durable sequence ranges and cannot be reused after a
backend restart. Delayed approvals remain bound to their original process generation.
Output subscriptions recover from socket resets or offset gaps by sending a fresh
reset replay before more live output. `muster reload` checks that the current backend
confirms the compatible host identity before it signals that backend; it refuses
legacy monolithic instances. The CLI reload preserves the terminal PID and host ID
in the fixture. Validation: 231 Rust tests and 21 focused backend survival checks.
