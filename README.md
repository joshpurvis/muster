# Muster

A desktop app for running many coding agents (Claude Code, Codex CLI, plain shells) side by
side in real terminals. Each channel is a task backed by its own git worktree, with one or more
terminal sessions. A background daemon owns all the terminals, so closing the window never
kills an agent. The UI reconnects and replays scrollback.

See [PLAN.md](PLAN.md) for the roadmap, [docs/PROTOCOL.md](docs/PROTOCOL.md) for the client
protocol and [docs/UI.md](docs/UI.md) for the UI spec, including the keyboard shortcuts.

## Build

```sh
deno task build                      # web/dist, then cargo build --release (browser-window mode)
cargo build --release --features gui # native window (needs webkit2gtk-4.1 on Linux)
deno task build:all                  # web/dist, then the release build with the native window
```

Debug builds serve `web/dist` from disk, so you can rebuild the frontend without recompiling.

## Run

```sh
muster                           # start the daemon if needed, then open a new window
muster open [path] [--window N]  # open window N, or a new window with a tab in path
muster status                    # is the daemon running? how many sessions?
muster url                       # print the UI URL (includes the access token)
muster stop                      # stop the daemon (terminates all sessions)
muster daemon [--foreground]
```

Each `muster` opens its own window and returns at once.

### Driving agents from a shell (or another agent)

```sh
muster ls                                # ID  WIN  TAB  NAME  KIND  STATE  FOR  DETAIL
muster new ~/src/app --prompt "fix the flaky test"   # prints the new session id
muster new . --agent codex -- -m gpt-6-sol           # args after -- go to the agent CLI
muster send fix-auth "run the tests" --wait          # prompt, then wait until it settles
muster read fix-auth -n 100              # last 100 lines of output, plain text
muster wait 12 --until needs_human       # block until it needs you (exit 124 on timeout)
muster approve 12                        # answer a permission prompt
muster keys 12 esc text:/compact enter
muster events --tab fix-auth             # one line per state change, until Ctrl-C (--json: every event)
```

Bots are persistent named agents with their own folder, memory and resumed conversation
(PLAN.md §10): `muster bot new reviewer --persona "Reviews diffs"`, then
`muster ask reviewer "is fix-auth safe to merge?"` (or the `ask_bot` MCP tool from an agent).

A target is a session id, `.` (the session you're in), a session or tab name, a unique
prefix, or `tab/name`. Exit codes: 0 ok, 1 error, 2 bad or ambiguous target, 124 timeout.
See [docs/CLI.md](docs/CLI.md) for every command.

Without the `gui` feature, the UI opens in the first browser found out of `$MUSTER_BROWSER`,
`chromium`, `google-chrome-stable` or `brave`, in `--app` mode with its own profile. If none
is found it falls back to `xdg-open`.

## Where things live

| What | Default | Override |
|---|---|---|
| Database, session logs, worktrees, bot homes (`bots/`) | `$XDG_DATA_HOME/muster` | `MUSTER_DATA_DIR` |
| `daemon.json`, lock, `daemon.log` | `$XDG_RUNTIME_DIR/muster` | `MUSTER_RUNTIME_DIR` |
| Port | 7433 (a free port if busy) | `MUSTER_PORT` |

Log verbosity: `MUSTER_LOG=debug`.

## Test

```sh
scripts/smoke.sh   # end-to-end: temp dirs, port 17433, scoped Deno permissions
```
