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
```

Debug builds serve `web/dist` from disk, so you can rebuild the frontend without recompiling.

## Run

```sh
muster              # start the daemon if needed, then open the UI
muster open [path]  # add a folder or repo as a workspace, then open the UI
muster status       # is the daemon running? how many sessions?
muster url          # print the UI URL (includes the access token)
muster stop         # stop the daemon (terminates all sessions)
muster daemon [--foreground]
```

Without the `gui` feature, the UI opens in the first browser found out of `$MUSTER_BROWSER`,
`chromium`, `google-chrome-stable` or `brave`, in `--app` mode with its own profile. If none
is found it falls back to `xdg-open`.

## Where things live

| What | Default | Override |
|---|---|---|
| Database, session logs, worktrees | `$XDG_DATA_HOME/muster` | `MUSTER_DATA_DIR` |
| `daemon.json`, lock, `daemon.log` | `$XDG_RUNTIME_DIR/muster` | `MUSTER_RUNTIME_DIR` |
| Port | 7433 (a free port if busy) | `MUSTER_PORT` |

Log verbosity: `MUSTER_LOG=debug`.

## Test

```sh
scripts/smoke.sh   # end-to-end: temp dirs, port 17433, scoped Deno permissions
```
