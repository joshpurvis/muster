# muster CLI

Everything the UI can do to a running agent, from a shell or from another agent. The CLI talks
to the daemon over `POST /api/rpc` with the client token from the discovery file (see
docs/PROTOCOL.md). It starts the daemon when needed, except for `ls`/`status`, which just report
that nothing is running (`ls` prints `muster daemon: not running`, or `[]` with `--json`, and
exits 0). The design follows herdr's `agent list/read/prompt/wait` commands.

## Targets

Commands that act on a session take a **target**:

| Form | Meaning |
|---|---|
| `12` | session id 12 |
| `.` | this session (`$MUSTER_SESSION_ID`; error outside a muster session) |
| `fix-auth` | exact session name, else exact tab (channel) name, case-insensitive |
| `fix` | unique case-insensitive prefix of a session name or tab name |
| `tab/name` | session `name` inside tab `tab` (either part may be a prefix) |

A tab name resolves to that tab's agent session when it has exactly one live agent (claude or
codex), else its only live session. Archived tabs and removed sessions are never matched. An
exited session only matches by id or exact name, and an exact name matches exited sessions only
when no live session has it. All digits is always an id (never a name or prefix). Ambiguous targets fail with exit code 2 and
list the candidates (`id  tab/name  kind  state`).

## Commands

| Command | What it does |
|---|---|
| `muster` | Open a new window (reclaims a closed window with tabs first). The native window runs in its own detached process, so the command returns at once. |
| `muster open [path] [--window N]` | Open a window: window N, else a new one. With a path: add the folder (`workspace.add`), create a window unless `--window` is given (`window.create`), open a tab there without an agent (`channel.create`, the tab shows the launcher), then open the UI on that window. |
| `muster ls [--all] [--json]` (alias `list`) | Table of live sessions: `ID  WIN  TAB  NAME  KIND  STATE  FOR  DETAIL`. `FOR` is the time in state (`3m`). `--all` includes exited sessions. `--json` prints `SessionInfo[]` with `channel_name`, `window_id` added. |
| `muster read <target> [-n LINES] [--screen] [--json]` | Print the last LINES (default 50) lines of output as plain text. `--screen` prints the visible screen instead. Trailing blank lines are dropped. `--json` prints `{id, lines}`. |
| `muster send <target> [TEXT...] [--interrupt] [--wait] [--until STATE]... [--timeout SECS]` | Send a prompt through the injection queue (bracketed paste, then Enter; queued while the agent is busy). TEXT words are joined by spaces; with no TEXT or `-`, stdin is read. `--interrupt` interrupts a busy agent first. `--wait` blocks until the agent has started and then reached a settled state (the default `until` set, see `wait`), prints the final state line (`id  tab/name  state  detail`) and exits 0, or 124 on timeout. `--until` implies `--wait`; `--timeout` (default 600) applies to the whole wait. Without `--wait` it prints `tab/name: sent` or `tab/name: queued (N waiting)`. Text starting with `-` goes after `--`. |
| `muster wait <target> [--until STATE]... [--timeout SECS] [--json]` | Block until the session is in one of the states. Default: idle, needs_approval, asking_question, error, stuck or exited. `needs_human` is accepted as a group. Prints the state line and exits 0 when matched, 124 on timeout (default 600 s). `--json` prints the `session.wait` reply. |
| `muster keys <target> KEY...` | Send keys: `enter`, `esc`, `tab`, `space`, `backspace`, `up/down/left/right`, `ctrl+<x>`, `alt+<x>` (a character or key name), `shift+tab`, `home`, `end`, `delete`, `pageup`, `pagedown`, modified cursor keys (`ctrl+left`, `alt+up`, `ctrl+shift+end`: xterm `CSI 1;m X`), a single character (itself), or `text:<literal>`. Names are case-insensitive. All keys go in one `session.input`. |
| `muster approve <target> [--always \| --deny]` | Answer the permission prompt. |
| `muster interrupt <target>` | Interrupt (Esc for agents, Ctrl-C for shells). |
| `muster new [PATH] [--agent claude\|codex\|shell\|none] [--worktree] [--base REF] [--name NAME] [--window N] [--prompt TEXT] [--json]` | New tab in PATH (default: cwd), default agent claude, no worktree unless `--worktree` (`--base` needs it). No `--name`: auto-named. No `--window`: inside a muster session, that session's window, else the daemon's default. Prints the new session id; `--json` prints `{channel, session}`. |
| `muster rename <target> <name> [--tab]` | Rename the session, or its tab with `--tab`. |
| `muster kill <target>` / `muster restart <target>` | Kill or restart the session. |
| `muster status` / `stop` / `url` / `daemon` / `hook` | As before. |

Output: human tables go to stdout, errors to stderr as `muster: <message>`. Exit codes: 0 ok,
1 error, 2 bad or ambiguous target, 124 timeout.
