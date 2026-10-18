# Private terminal protocol v1

This is the local application-to-host protocol, distinct from the public UI/CLI
protocol in PROTOCOL.md. It is defined by `muster-terminal-protocol` and does not
contain agents, worktrees, tasks, window records, or application permissions.

The host requires explicit `--runtime-dir` and `--data-dir`. It owns
`terminal.sock`, `terminal.lock` and `terminal.json` in its runtime directory, and
`host.lock` plus `runs/<id>-<generation>/` in its data directory. Both directories
are private. A lock on both directories prevents a differently configured second
host from reaping a live host's children. The socket is mode 0600 and checks the
connecting UID. Client disconnect never implies shutdown.

## Handshake and identity

A request contains `version`, `instance` and `command`. `hello` returns the
protocol version, random host-instance identifier, PID and canonical data path.
Every other command must name that exact host instance. A stale client cannot
mutate a replacement host even if its socket path and terminal IDs are reused.
Incompatible versions fail; clients must not automatically upgrade/restart a host.

A terminal run is `{id, generation}`. Generations increase for successive runs of
one ID. Spawning an identical specification for the same run returns its existing
status, including after exit. A conflicting specification or a retired generation
is refused. The next generation cannot start while any prior run owns processes.
Clients do not automatically retry arbitrary mutations after losing a response.

Spawn specifies a complete environment, cwd, executable, arguments and initial
size. The host replaces its process ownership markers with host-generated values;
it does not inherit the environment of the long-lived host into children.

## Framing

Each frame is `[u8 type][u32 payload_length, big endian][payload]`. The maximum
payload is 16 MiB, checked before allocation. Unknown types and malformed headers
are rejected. Frames are:

- 0: JSON request or response.
- 1: replay, `[u64 next_raw_output_offset][replay bytes]`.
- 2: live output, `[u64 raw_output_offset][output bytes]`.
- 3: heartbeat with an empty payload.

Offsets count raw PTY bytes; synthetic mode prefixes in replay do not advance
those offsets. Attach captures the replay, next offset and live subscription
atomically. A lagged output subscriber receives a replacement replay and offset.
The UI must reset on replay. Slow socket writes time out rather than blocking PTY
readers; clients can reattach. Replay has the existing 2 MiB retention bound, not
unlimited historical output.

## Commands

Control connections carry one request and one response. Each output/event stream
uses a separate connection. `attach` and `events` acknowledge `attached` before
streaming. Subscribe to events before reading inventory; lifecycle events may
repeat information already present in inventory. Event stream overflow sends
`resync`, which requires another inventory read.

Commands: `hello`, `inventory`, `spawn`, `status`, `input`, `resize`, `screen`,
`recent_text`, `terminate`, `forget`, `attach`, `events`, `flush`, `shutdown`.
`input` is bounded to 1 MiB per request; larger input must be split deliberately.
`forget` is refused until processes and pending output are gone. Shutdown is an
explicit operation that terminates all host-owned processes. SIGTERM/SIGINT have
that same host-only meaning. SIGHUP is ignored.

Inventory retains exited runs until explicitly forgotten, including exit codes
and access to replay. Original process exit and terminal drain are separate facts:
background children can remain after the leader exits.

After exit, drain and descendant cleanup, the host saves an immutable final replay,
screen and plain-text scrollback checkpoint. It releases the PTY/parser/ring only
after the log and checkpoint are durably written. A write failure retains the live
object for a later retry. Attached streams do not pin that object; they receive
the remaining output and then heartbeats. New readers load the final state from
disk. Resizing an archived run is acknowledged without changing its final snapshot;
input is refused. `forget` removes its checkpoint and log as well as inventory.
Small lifecycle metadata and a launch-specification digest remain until forgetting;
retired generations use one watermark per terminal ID.

## Failure domains

The host flushes output periodically without an application connection. It records
process ownership separately from the application database. Only host startup
reaps verified leftovers of a previous host instance. Restarting a backend must
instead reconnect and reconcile inventory.

Open PTYs are not recovered after host crash. The application must distinguish a
new host instance from a surviving one. Previous output files remain on disk.
