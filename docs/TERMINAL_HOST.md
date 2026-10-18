# Terminal host separation

Implemented on `refactor/terminal-host` in `/tmp/muster-terminal-host`. The live
installation is not part of this migration or its tests. Builds use this worktree's
`target/` directory. The chronological checkpoints below describe intermediate
states; the completion audit at the end records the final verification.

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

Downgrade boundary: a monolithic (pre-host) build cannot see host-owned terminals.
Rolling back requires `muster stop` on this build first, which ends every terminal;
the application database stays readable by the older build. As a guard, the backend
starts the host with `MUSTER_TERMINAL_FENCE_LOCK=<runtime>/daemon.lock`, and the host
keeps that lock for its whole life. Older daemons take the same lock to start, so they
refuse to run while a host owns terminals instead of autostarting duplicates. This
build's backend uses `backend.lock` for its own single-instance check.

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

Remaining before completion: additional crash-window tests and code review, including
the host's spawn/ownership-journal window and concurrent hook/prompt ordering.
Existing
live terminals and the installed binary have not been changed or migrated.

### Restart safety checkpoint

Approval identities reserve durable sequence ranges and cannot be reused after a
backend restart. Delayed approvals remain bound to their original process generation.
Output subscriptions recover from socket resets or offset gaps by sending a fresh
reset replay before more live output. `muster reload` checks that the current backend
confirms the compatible host identity before it signals that backend; it refuses
legacy monolithic instances. The CLI reload preserves the terminal PID and host ID
in the fixture. Validation: 231 Rust tests and 21 focused backend survival checks.

### Independent UI checkpoint

`muster ui-install web/dist` stages a compatible release and atomically activates it.
Installed HTML pins JavaScript/CSS to its release; old bundles remain available to
open pages. Release validation and the UI/application handshake fail closed for
incompatible versions. No backend or host restart is needed for UI-only updates.
`MUSTER_WEB_DIR` remains a direct built-directory override for development.

Validation: 27 focused backend/UI survival checks; external asset isolation/cache/
activation unit test; frontend type/lint checks, all 82 frontend tests, and both
production bundles. Remaining work above is still open; no live install was performed.

### Durable callback checkpoint

New terminal runs receive `MUSTER_HOOK_DB`, an application-owned SQLite inbox. Hook
helpers append callbacks even while no backend is listening and keep a two-second
write deadline. The backend processes the inbox in insertion order before starting
detectors, prompt queue workers or autostart. Reply attribution, resume IDs and the
inbox checkpoint commit in one transaction; deleting the acknowledged inbox row is
safe to retry. Callbacks from retired or superseded runs are discarded. A run that
exited during the outage can still report its final reply.

Pending callback payloads are capped at 64 MiB and 100,000 events. Overflow is counted
persistently, logged and reported by the backend. Legacy runs without the new inbox
variable keep the existing HTTP callback path. The inbox is not part of the terminal
host and is never needed to keep terminal processes or output alive.

Validation: 234 Rust tests, all 154 full smoke checks, and 31 focused survival checks,
including helper invocation during an outage, a lost inbox acknowledgement, and old
run callback rejection. Further review still needs to cover concurrent prompt/hook
ordering and reconstruction of transient detector/subagent state.

### MCP recovery checkpoint

MCP rate reservations, send/delivery markers and remembered spawn ancestry now persist
before agent work. MCP-created channel origin commits with its channel row, closing
the crash window before the guard snapshot is updated. Constructor recovery restores
missing child depths from their persisted parent records. Persistence failures refuse
new work. Errors after durable prompt acceptance retain the message attribution and
rate reservation, because they do not prove that no input ran.

Validation: 237 Rust tests, including replacement with exhausted rate limits, a removed
parent and lost child acknowledgement, persistence failure, and pending-delivery errors.
The focused process-boundary suite also passes all 32 checks, including a real backend
crash with an exhausted MCP rate window. The original live installation remains untouched.

### Stable executable checkpoint

Backend startup copies complete application/host executables into private content-addressed
releases, then atomically publishes `<data>/bin/muster` after application recovery succeeds.
Hooks and `$MUSTER_BIN` use this path. Copies are independent of build-output inodes;
corrupt or incomplete releases are refused. A backend-only build can reuse the saved host
binary, and no executable publication signals or replaces a running terminal host.
Old executable releases remain available for rollback.

Validation: two executable-publication unit tests and all 34 focused process checks pass.
The process fixture now deletes its original build directory, then invokes its saved CLI,
delivers hooks during downtime and performs subsequent backend restarts/reload from the
stable path. Its terminal PID and host identity survive. No live files were changed.

### Exited terminal resource checkpoint

The host now archives a run after exit, terminal drain and descendant cleanup. It
persists exact replay modes/bytes, final screen and text before releasing the PTY,
VT parser and ring buffer. Attached streams hold receivers rather than PTY owners,
so an open exited tab cannot keep those resources alive. Archived reads load from
disk; small lifecycle metadata and a launch digest remain in memory until Forget.
Forgotten generations use one watermark per session instead of one entry per run.

Validation: all 241 Rust tests pass. New checks prove that PTY strong references
reach zero with a subscriber still present, failed checkpoint writes retain state
for retry, replay and screen survive archival, background descendants prevent
archival, and a pre-exit stream keeps receiving heartbeats after archival. All 154
full application smoke checks and all 34 focused backend checks also pass with this change.

### Retirement recovery checkpoint

Session removal now commits the deleted session/events and retired launch in one
transaction before publishing removal or signaling its process. Reconciliation at
startup and periodically thereafter completes termination, drain, Forget and record
deletion. Failed persistence keeps the original session live; failed cleanup retains
the intent for retry. The application no longer has no-op PTY flushing/release calls
or a second process-reaping thread for each removal.

Forget also fences an unknown prepared run against a delayed spawn. Terminal generation
allocation is durable independently of deletable launch records. A second start cannot
overwrite an unconfirmed launch; late acknowledgements preserve retirement and cannot
replace a newer generation's record. Restart preserves the prior handle and launch
record until the host confirms cleanup. Backend shutdown permits an already-started
launch to finish instead of terminating it solely because the backend is stopping.

Validation: all 245 Rust tests, all 154 full smoke checks and all 36 focused process checks
pass. The new process case resumes a persisted removal whose process ignores SIGHUP,
waits for exit/drain, and verifies both host state and application record are removed
while an unrelated terminal keeps its PID. Unit coverage includes transactional removal
failure, generation allocation across reopen, unconfirmed-launch preservation, and
Forget-before-Spawn fencing.

Deterministic fault injection now also passes: a private socket proxy delays Spawn
before host creation or after creation but before acknowledgement. Concurrent removal
wins in both cases, and killing the backend before acknowledgement adopts exactly the
original process on replacement. All 40 focused checks pass, with unrelated terminal
PIDs and host identity preserved throughout. The proxy operates only on fixture paths.

The next audit covers the host's spawn/ownership-journal crash window and transient
detector/subagent recovery, plus final UI recovery verification. No live installation
or existing legacy session has been migrated.

### Callback/input ordering checkpoint

The backend serializes inbox consumption with prompt attribution and human-input
markers. Before changing attribution it drains already committed callbacks, then
briefly holds the inbox writer lock while saving the new context. If callbacks remain
after the bounded drain, input attribution is deferred instead of overtaking them.
The background consumer and legacy HTTP hook path share the same serialization.
This fixes a queued Stop callback being assigned to the next injected message and
an older prompt callback consuming a later human-input marker. It orders observed
callback inserts; it cannot infer an agent's event time before its helper enqueues it.

Validation: all 247 Rust tests, all 154 full smoke checks, and all 40 focused survival
checks pass. New unit tests cover reply association across the input boundary,
provenance retention, stale-generation refusal, backlog deferral and concurrent inbox
writes. This coordination remains entirely in the application layer.

### Launch ownership checkpoint

The host writes an ownership intent before spawning its early exec mode. That child
waits until the host publishes its exact PID and markers, then executes the requested
program with the same PID and PTY. If the parent exits before publication, the child
exits without running the target. This closes the spawn-before-journal crash window.
New launches reject missing executables or working directories; retries of existing
runs remain valid even after their original launch files have disappeared.

Validation: all 250 Rust tests passed, followed by all 16 host tests after adding
the deleted-launch-files retry regression. All 154 full application smoke checks
and all 40 focused backend survival checks passed. Process tests cover parent death
before ownership publication, PID preservation, vanished cwd, and host replacement.
Detector/subagent recovery and final UI recovery verification remain under review.

### Detector recovery checkpoint

The application saves meaningful screen hashes, progress timestamps and observed
states for each terminal generation. Backend replacement restores that progress
clock; transient poll counters restart. Stable SHA-256 hashes permit comparison
across backend builds. A new terminal generation gets a fresh clock, and detector
verdicts are applied only to the exact terminal generation whose screen was read.
Unchanged screens and spinner-only animation do not trigger checkpoint writes.

Validation: all 253 Rust tests pass, including application database reopen with a
preserved stuck timer, a fresh timer for a replacement generation, and refusal of
an old generation's detector verdict. Subagent/hook state recovery and final UI
recovery verification remain open.

### Hook session recovery checkpoint

Hook-derived session state, subagent counts and the hook-precedence timestamp now
commit in the same transaction as attribution and the inbox cursor. Memory is
published only after that commit. Startup restores the count and grace timestamp
for the surviving generation; new or exited runs begin with zero subagents.
A run's initial session state is saved with its launch record, before Spawn, so
callbacks arriving before the acknowledgement are applied at once (later revised:
they used to wait at the head of the inbox, which stalled every other session). Removal deletes the saved hook runtime with the session.

Validation: all 255 Rust tests, all 154 full smoke checks and all 43 focused backend
checks pass. The tests include transaction rollback, duplicate count prevention,
callback deferral during launch, real backend replacement with a running subagent,
and an explicit terminal restart clearing the old count. All 82 frontend tests pass.

The optional `scripts/backend-smoke.py --browser` mode uses a fresh Chromium profile
and captures screenshots under its private fixture directory. Its rendered desktop
check confirms that an open page reconnects after backend SIGKILL, replays captured
output, and accepts keyboard input into the original terminal PID. The screenshots
have been inspected and all 44 checks in the desktop browser-enabled suite passed.

The browser fixture also exercises the mobile UI with a private paired device.
During backend downtime it verifies that sending is disabled; after replacement it
verifies that pairing and the unsent draft survive, captured output is replayed, and
the same terminal PID remains. Offline/reconnected screenshots have been inspected.
All 45 checks in the extended browser fixture passed, including its owned-process
cleanup. No browser console exceptions were reported.

## Review fix round

- A launch's initial state (Starting, no pid, zero subagents) and any unconsumed
  initial prompt are saved before Spawn; the acknowledgement only adds pid and title.
  Recovery treats a live run as authoritative over a saved Exited, and turns a saved
  approval or question into Working so the detector re-reads the dialog on screen.
- A launch whose acknowledgement was lost is adopted (or, if the host rejects it,
  retired) by the running backend every 3 s. A definite rejection at startup retires
  only that launch. `spawn_session` reports `SpawnRetained`; MCP keeps the channel,
  worktree and rate reservation, and named agents are linked before Spawn.
- Keystrokes are written even when input attribution cannot be saved.
- Worktree and port users count only runs that are starting, alive, leaving
  processes behind or unconfirmed, not archived replay handles. Channel archival
  stops on a failed session retirement and waits for retired runs to exit before
  deleting the worktree.
- `session.resume_queue` (`muster queue --resume`, desktop and mobile review) clears
  only the uncertain record. Queue counts include the prompt being delivered, `busy`
  changes are not written, and a draft or dialog backs delivery off for 1 s.
- Titles are capped at 1 KiB; the host has separate admission for streams (256) and
  status reads no longer render a screen.

Open after the fix round (not yet fixed):

- The backend now locks `backend.lock` and the host holds `daemon.lock` as a rollback
  fence, so a legacy monolithic daemon no longer excludes a new backend by flock. Only
  the `find_daemon()` discovery check stops a new backend starting beside it; a missing
  or racing `daemon.json` lets both run on one database. The backend should refuse when
  `daemon.lock` is held and no compatible host answers.
- A dialog answered by keyboard and replaced before the detector sees the change can
  pair the new dialog with the old description (also on master). Bind approvals to a
  dialog fingerprint.
- vte buffers an oversized OSC title before the 1 KiB cap; a huge `session.output`
  `lines` request can exceed the frame limit (fails only that request).
- A host started before this build does not hold the fence lock.
- The desktop/mobile Resume buttons are type- and lint-checked but not yet rendered in
  the browser fixture.

## Completion audit

The scope is independent terminal lifetime during application/UI iteration. Host
replacement, machine reboot and migration of legacy daemon-owned PTYs are not
promised to preserve open terminals. The proxy transport may pause interaction
while the application reconnects; the host keeps processes and output capture alive.

| Requirement | Current evidence | Status |
| --- | --- | --- |
| Small host independent of application policy | Host/core/protocol Cargo manifests contain no application, database, agent or MCP dependency. Generic PTY ownership lives in the host/core crates. | Implemented; dependency boundary inspected |
| Backend crash/reload preserves terminal identity and output | Host survival tests; backend smoke checks for SIGKILL, SIGTERM, reload, delayed spawn acknowledgement and output during downtime. | Verified |
| Reconnect cannot accidentally duplicate or kill a run | Instance/protocol checks, monotonic generations, retry-safe Spawn, durable retirement and delayed-ack fault proxy cases. | Verified |
| Prompt and agent recovery is explicit and safe | Durable waiting/uncertain queues, generation-bound input, atomic hook attribution/state/count/cursor, MCP reservations and detector progress checkpoints. Recovery runs before queue workers and autostart. | Verified by Rust/process checks and lifecycle review |
| Upper-layer executable updates preserve helper availability | Immutable copied executable releases and stable command link; fixture deletes its original build before hook/reload checks. | Verified |
| UI releases activate independently | Staged compatible releases, pinned asset URLs and retained old bundles; fixture verifies unchanged backend/terminal PIDs during UI installation. Complete staged assets are checked before publication. | Verified with asset and process tests |
| Actual UI reconnects to the surviving terminal | Desktop verifies replay and post-reconnect keyboard input; mobile verifies pairing, draft retention, offline input disabling and replay. Screenshots inspected. | Verified; all 45 extended fixture checks passed |
| Host behaves correctly without clients | Query replies, periodic log flush, flood bounds, exited-run archival and background descendant tests. | Verified |
| Work remains isolated from the live instance | Separate `/tmp/muster-terminal-host` worktree; explicit private runtime/data/port on all fixtures; original checkout remains at `9e67a6d`. No installation or migration performed. | Verified development boundary |
| Reviewable delivery and rollout guidance | Committed branch, README build/reload/UI commands, private protocol specification and this design/checkpoint document. | Requirements and rollout guidance reviewed; no live rollout performed |

Final validation: 255 Rust tests, 154 full application smoke checks, 45 backend/browser
survival checks, 82 frontend tests, frontend type/lint checks, the workspace build,
the staged UI asset regression and `cargo check --workspace --features gui --offline`.
The native GUI feature was compile-checked; the interactive recovery checks used
Chromium against the actual desktop and mobile bundles.

Lifecycle review confirmed that application shutdown only settles its queues;
terminal ownership has no backend-drop cleanup path. Host startup is attempted only
when the private endpoint is absent/refused, and compatibility errors preserve the
existing host. Executable publication follows successful application recovery, while
surviving terminals retain their stable helper path. The original checkout remains
at `9e67a6d` with its pre-existing `.claude/` entry. Installing this branch and retiring
legacy sessions remain separate user-controlled rollout actions.
