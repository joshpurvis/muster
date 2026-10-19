# Muster

A desktop app for running many coding agents (Claude Code, Codex CLI, plain shells) side by
side in real terminals. Each channel is a task backed by its own git worktree, with one or more
terminal sessions. A small terminal host owns the PTYs and processes. The application
backend handles agents, tasks and worktrees; windows render the UI. Closing a window or
restarting the backend leaves terminal processes running. The UI reconnects and replays
scrollback. See [the separation design](docs/TERMINAL_HOST.md) for recovery and rollout details.

Agents can show images, charts, tables and pull request decisions in panels beside their
terminal; `muster skills install` teaches Claude Code and Codex how ([docs/CLI.md](docs/CLI.md#agent-skills)).

See [PLAN.md](PLAN.md) for the roadmap, [docs/PROTOCOL.md](docs/PROTOCOL.md) for the client
protocol and [docs/UI.md](docs/UI.md) for the UI spec, including the keyboard shortcuts.

## Build

```sh
deno task build                      # web/dist, then cargo build --workspace --release (browser-window mode)
cargo build --workspace --release --features gui # native window (needs webkit2gtk-4.1 on Linux)
deno task build:all                  # web/dist, then the release build with the native window
```

**Native window renderer (Linux).** WebKitGTK's DMA-BUF renderer is much faster (typing
feels instant) but crashes at startup on many Wayland setups ("Error 71 (Protocol error)").
On Wayland with the NVIDIA driver, the window keeps it on and sets
`__NV_DISABLE_EXPLICIT_SYNC=1`, which makes it work there. Every other setup keeps it off.
While such a window starts it keeps a file in `<data>/webkit-dmabuf-launching/` (one per
window process), and deletes it a few seconds after the page loads or when the window
closes normally. If the next window finds one left by a process that is gone (the previous
window crashed during startup), or a window's web process crashes during startup, Muster
writes `<data>/webkit-dmabuf-failed` (usually `~/.local/share/muster/webkit-dmabuf-failed`),
keyed by the NVIDIA driver and WebKitGTK versions. From then on windows use
`WEBKIT_DISABLE_DMABUF_RENDERER=1` until one of those versions changes or you delete the file.
The window that crashed is not reopened for you; open a new one. Setting either variable
yourself always wins (`env WEBKIT_DISABLE_DMABUF_RENDERER=0 muster` forces the fast renderer
on, `=1` off). `muster status` shows what the next window would use, and each window logs its
choice and any fallback to the daemon log.

Build and distribute both `muster` and `muster-terminal-host` side by side. Debug builds
serve `web/dist` from disk. Release builds can also load an independent UI release:

```sh
deno task web
muster ui-install web/dist        # activate UI files, then refresh the window
muster reload                    # replace only the backend; terminal processes continue
```

UI installation switches complete releases atomically and keeps older bundles for open
windows. `MUSTER_WEB_DIR` can point directly to a built UI for development. An incompatible
UI is refused. Backend reload verifies host ownership and refuses legacy monolithic daemons;
existing terminals in those daemons need to finish before migrating.

On startup, the backend snapshots its executables under the data directory and publishes
`bin/muster` atomically. Agent hooks and `$MUSTER_BIN` use that stable path, so removing
an old build directory does not strand running sessions. Later backend-only builds can
reuse the saved host executable. Publishing a build never replaces a running host;
old executable releases are retained for rollback.

Downgrading to a monolithic build (one without `muster-terminal-host`) is an offline step:
run `muster stop` with this build first. That ends every terminal. An older daemon knows
nothing about host-owned terminals, so started beside them it would mark them exited and
autostart duplicate agents next to the originals. To prevent that, the terminal host holds
`daemon.lock` in the runtime directory while it lives, and an older daemon refuses to start
until the host is gone ("another muster daemon is already running").

## Run

```sh
muster                           # start the daemon if needed, then open a new window
muster open [path] [--window N]  # open window N, or a new window with a tab in path
muster status                    # is the daemon running? how many sessions?
muster url                       # print the UI URL (includes the access token)
muster reload                    # restart the backend, preserving terminal processes
muster stop                      # stop backend and host (terminates all sessions)
muster daemon [--foreground]
```

Each `muster` opens its own window and returns at once.

### Driving agents from a shell (or another agent)

```sh
muster ls                                # ID  WIN  TAB  NAME  KIND  STATE  FOR  DETAIL
muster new ~/src/app --prompt "fix the flaky test"   # prints the new session id
muster new . --kind codex -- -m gpt-6-sol            # args after -- go to the agent CLI
muster send fix-auth "run the tests" --wait          # prompt, then wait until it settles
muster read fix-auth -n 100              # last 100 lines of output, plain text
muster wait 12 --until needs_human       # block until it needs you (exit 124 on timeout)
muster approve 12                        # answer a permission prompt
muster keys 12 esc text:/compact enter
muster events --tab fix-auth             # one line per state change, until Ctrl-C (--json: every event)
```

Named agents are persistent agent sessions with their own folder, memory and resumed
conversation (PLAN.md §10): `muster agent new reviewer --persona "Reviews diffs"`, then
`muster ask reviewer "is fix-auth safe to merge?"` (or the `ask_agent` MCP tool from an agent).
In the UI, Alt+G opens the agents panel: start an agent with a click, create one from a
template, edit its persona and memory, and watch the messages agents send each other.

A target is a session id, `.` (the session you're in), a session or tab name, a unique
prefix, or `tab/name`. Exit codes: 0 ok, 1 error, 2 bad or ambiguous target, 124 timeout.
See [docs/CLI.md](docs/CLI.md) for every command.

Without the `gui` feature, the UI opens in the first browser found out of `$MUSTER_BROWSER`,
`chromium`, `google-chrome-stable` or `brave`, in `--app` mode with its own profile. If none
is found it falls back to `xdg-open`.

## Where things live

| What | Default | Override |
|---|---|---|
| Database, session logs, worktrees, agent homes (`agents/`) | `$XDG_DATA_HOME/muster` | `MUSTER_DATA_DIR` |
| Backend discovery/logs and terminal host socket/lock/log | `$XDG_RUNTIME_DIR/muster` | `MUSTER_RUNTIME_DIR` |
| Port | 7433 (a free port if busy on first launch; pinned thereafter) | `MUSTER_PORT` |
| Independent UI releases | `$MUSTER_DATA_DIR/ui/{releases,current}` | `MUSTER_WEB_DIR` (direct built UI override) |

Log verbosity: `MUSTER_LOG=debug`.

## Test

```sh
scripts/smoke.sh   # end-to-end: temp dirs, port 17433, scoped Deno permissions
```

## License

MIT, see [LICENSE](LICENSE).
