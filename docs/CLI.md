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
| `fix src/main.rs` | a string that is exactly a session or tab name is taken whole, even with a `/` in it |

A tab name resolves to that tab's agent session when it has exactly one live agent (claude or
codex), else its only live session. Archived tabs and removed sessions are never matched. An
exited session only matches by id or exact name, and an exact name matches exited sessions only
when no live session has it. All digits is always an id (never a name or prefix). Ambiguous targets fail with exit code 2 and
list the candidates (`id  tab/name  kind  state`).

## Commands

| Command | What it does |
|---|---|
| `muster` | Open a new window (reclaims a closed window with tabs first). The native window runs in its own detached process, so the command returns at once. |
| `muster open [path] [--window N]` | Open a window: window N, else a new one. With a path: add the folder (`workspace.add`), create a window unless `--window` is given (`window.create`), open a shell tab there (`channel.create` with `agent: "shell"`), then open the UI on that window. A shell rather than an empty task: since v2 a tab is a session, so a task without one has no tab and no window would show it. |
| `muster ls [--all] [--json]` (alias `list`) | Table of live sessions: `ID  WIN  TAB  NAME  KIND  STATE  FOR  DETAIL`. `FOR` is the time in state (`3m`). `--all` includes exited sessions. `WIN` is the session's own window (a tab is a session, so a task's sessions may be in different windows). `--json` prints `SessionInfo[]` (which carry `window_id`) with `channel_name` added. |
| `muster read <target> [-n LINES] [--screen] [--json]` | Print the last LINES (default 50) lines of output as plain text. `--screen` prints the visible screen instead. Trailing blank lines are dropped. `--json` prints `{id, lines}`. |
| `muster send <target> [TEXT...] [--interrupt] [--wait] [--until STATE]... [--timeout SECS]` | Send a prompt through the injection queue (bracketed paste, then Enter; queued while the agent is busy). TEXT words are joined by spaces; with no TEXT or `-`, stdin is read. `--interrupt` interrupts a busy agent first. `--wait` blocks until the agent has started and then reached a settled state (the default `until` set, see `wait`), prints the final state line (`id  tab/name  state  detail`) and exits 0, or 124 on timeout. `--until` implies `--wait`; `--timeout` (default 600) applies to the whole wait. Without `--wait` it prints `tab/name: sent` or `tab/name: queued (N waiting)`. Text starting with `-` goes after `--`. |
| `muster wait <target> [--until STATE]... [--timeout SECS] [--json]` | Block until the session is in one of the states. Default: idle, needs_approval, asking_question, error, stuck or exited. `needs_human` is accepted as a group. Prints the state line and exits 0 when matched, 124 on timeout (default 600 s). `--json` prints the `session.wait` reply. |
| `muster keys <target> KEY...` | Send keys: `enter`, `esc`, `tab`, `space`, `backspace`, `up/down/left/right`, `ctrl+<x>`, `alt+<x>` (a character or key name), `shift+tab`, `home`, `end`, `delete`, `pageup`, `pagedown`, modified cursor keys (`ctrl+left`, `alt+up`, `ctrl+shift+end`: xterm `CSI 1;m X`), a single character (itself), or `text:<literal>`. Names are case-insensitive. All keys go in one `session.input` with `keys: true`, so unmodified cursor keys follow the terminal's cursor mode. |
| `muster approve <target> [--always \| --deny]` | Answer the permission prompt. |
| `muster interrupt <target>` | Interrupt (Esc for agents, Ctrl-C for shells). |
| `muster new [PATH] [--kind claude\|codex\|shell\|none] [--worktree] [--base REF] [--name NAME] [--window N] [--prompt TEXT] [--json] [-- AGENT_ARGS...]` | New tab in PATH (default: cwd), default kind claude (`--agent` is the flag's old name, still accepted), no worktree unless `--worktree` (`--base` needs it). No `--name`: auto-named. `--window N` puts the tab (the task's first session) in window N, which must exist. No `--window`: inside a muster session, that session's window, else the daemon's default. `--kind none` creates a task without a session: it has no tab, so no window shows it until a session starts in it; the daemon closes a task when its last session is removed, never one that never had one. Arguments after `--` go to the agent CLI of the tab's first session, after muster's permission flags (`channel.create` `extra_args`), and again on its restarts: `muster new . --kind codex --prompt "hi" -- -m gpt-6-sol -c model_reasoning_effort=xhigh`. They need an agent (not `--kind none`), and only an allowlist of flags is accepted (model, effort, resume and similar; see docs/PROTOCOL.md "Channel permissions"): anything that could change the agent's permissions is refused, set the tab's permissions instead. Prints the new session id; `--json` prints `{channel, session}`. |
| `muster move <target> --window N` | Show the session (tab) in window N (`session.set_window`, with `from_window_id` so a concurrent move isn't overridden). Window N must exist. The session stays in its tab (task); the task's window follows once all its sessions are in N (or none is left in the task's window). Prints `id  tab/name: window A -> N`. The window isn't opened or raised. |
| `muster rename <target> <name> [--tab]` | Rename the session, or its tab with `--tab`. |
| `muster kill <target>` / `muster restart <target>` | Kill or restart the session. |
| `muster events [--json] [--session TARGET] [--tab NAME]` | Stream daemon events (`GET /api/events`) until Ctrl-C, so scripts don't poll. Default: one line per session state change, `HH:MM:SS  ID  TAB/NAME  FROM -> TO  DETAIL` (local time of the change; `FROM` is `new` for a new session, `TO` is `removed` for a removed one). Each message to a named agent adds a line `HH:MM:SS  msg ID  FROM -> TO  origin=O hop=N  FIRST LINE` (with `refused: WHY` or `replied`). `--json`: every `session_upserted`, `session_removed`, `channel_upserted`, `channel_removed`, `window_upserted`, `window_removed`, `agent_upserted`, `agent_removed` and `agent_message` event, one JSON object per line exactly as the daemon sent it, flushed per line (plus `lagged`, below). `--session` keeps one session's events (and its tab's, its tab's window's and its own window's); `--tab` (exact tab name, else unique prefix) keeps that tab's, including a session moving out of it. Both are resolved once, at the start. Never starts the daemon: exit 1 when none runs. When the daemon stops, it prints `muster: the daemon stopped` to stderr and exits 0. After `lagged` (the reader fell behind) it re-reads the state and says so on stderr. |
| `muster pair [--name N] [--scope view\|interact\|manage] [--ttl SECS] [--url URL]` | Pair a phone (docs/ANDROID.md). Prints the `tailscale serve` commands when tailscale doesn't already proxy to this daemon's port, a QR code of `https://<host>/m/#pair=<code>` (needs `qrencode`), and the short code, then waits: `paired: <name> (device N)` and exit 0, or exit 1 when the code is burnt (three wrong codes), replaced or expired. Default scope `interact` (approve, prompt, keys); `manage` also opens and closes tabs. `--url` overrides the tailscale name. |
| `muster devices [--json]` | Paired devices: `ID  NAME  ACCESS  LAST`. Never starts the daemon. |
| `muster devices revoke <id\|name>` / `--all` | Revoke: the device's connections close at once and it must pair again. |
| `muster agent new <name> [--kind claude\|codex] [--model M] [--effort E] [--persona TEXT \| --persona-file F] [--accepts ORIGINS] [--autostart] [--claude-mode MODE] [--codex-sandbox SANDBOX]` | Create an agent (PLAN.md §10, `agent.create`): its home dir `<data>/agents/<name>/` with `CLAUDE.md` / `AGENTS.md` from the persona and `memory/`. Default kind claude. `--accepts` is a comma list of `human`, `schedule`, `webhook` (default: `human` for claude, all three for codex). `--autostart`: start it whenever the daemon starts. `--claude-mode plan\|default\|accept_edits\|bypass` / `--codex-sandbox read_only\|workspace_write\|workspace_write_network\|full_access`: its tab's profile (the other half keeps its default). Prints `agent NAME (KIND, accepts …): home DIR`. |
| `muster agent edit <name> [--model M] [--effort E] [--persona TEXT \| --persona-file F] [--accepts ORIGINS] [--autostart on\|off] [--claude-mode MODE] [--codex-sandbox SANDBOX]` | Change an agent (`agent.update`): only the given fields change; an empty `--model ""` / `--effort ""` clears it. A new persona rewrites its persona file (memory/ untouched). One permission flag keeps the other half as it is. Model, effort and permissions apply from its next start. Prints `updated NAME (…)`. |
| `muster agent ls [--json]` (alias `list`) | `NAME  KIND  MODEL  ACCEPTS  SESSION  STATE`. `--json` prints `agent.list`. |
| `muster agent rm <name>` | Remove the agent and its session; the home dir (persona, memory) stays. |
| `muster agent start <name> [--fresh]` | Start the agent's session (cwd = its home dir, in the agent's own tab in the "agents" workspace), restart it if it exited, or report the running one. Claude agents resume their last conversation; `--fresh` starts a new one (restarting a running agent). Prints the session id (`… (already running)`). |
| `muster ask <agent> [TEXT...] [--timeout SECS] [--json]` | Ask an agent and print its reply (`agent.ask`): starts it if needed, delivers with an envelope, waits for this message's turn to end (default 600 s, max 1800). TEXT as for `send` (none or `-`: stdin). Inside a muster session (`$MUSTER_SESSION_ID`) the ask is that session's: an agent's runs MCP `ask_agent` (its origin and hop, permission check, rate limit); outside one it is the human's (`human`, hop 0). There is no flag to set the origin. Exit 0 with a reply, 124 on timeout (the message stays delivered), 1 when refused or the agent stopped without a reply (a prompt for the human, an error). If the agent reported no final message for this message, the end of its terminal is printed after a note on stderr that it may belong to another prompt (exit 0). `--json` prints the `agent.ask` reply. |
| `muster queue TARGET [--clear]` | Inspect waiting prompts and interrupted delivery records as JSON. After inspecting the terminal, `--clear` discards the queue records; it does not undo commands or erase terminal input. |
| `muster ui-install PATH` | Stage and activate a complete built UI directory, without restarting either service. Refresh the window to load it. Previous release directories are retained for rollback and existing pages. Refuses incompatible manifests and symlinked content. `MUSTER_WEB_DIR` on the backend overrides installed releases. |
| `muster reload` | Replace the application backend while its terminal host and terminal processes keep running. Clients reconnect to the same endpoint. |
| `muster stop` | Explicitly stop both backend and terminal host, terminating sessions. |
| `muster status` / `url` / `daemon` / `hook` | As before. |

New sessions receive `$MUSTER_BIN` pointing to `<data>/bin/muster`; agent hooks use the
same stable executable path. Successful backend startup atomically switches it to a
private copy of that build, so callers survive deletion of the original checkout.
The first startup requires both executables side by side; backend-only updates can
reuse the saved host binary. This never restarts a running host. Executable snapshots
under `<data>/application-releases` are retained rather than pruned during updates.

Inside a muster session, `send`, `keys`, `ask` and `new` pass `$MUSTER_SESSION_ID` to the daemon
as `from_session`, so what an agent does through the CLI is attributed to it: `send` and `ask` run
as its MCP `send_message` / `ask_agent` (same guards and rate limits), and `keys` and `new` carry its
provenance (docs/MCP.md "Agents"). From a shell tab they carry that shell's provenance, which is the
human's unless an agent typed into it.

Output: human tables go to stdout, errors to stderr as `muster: <message>`. Exit codes: 0 ok,
1 error (including a `wait`/`send --wait` whose session exited before reaching the state),
2 bad or ambiguous target, 124 timeout.

`send --wait` on a shell: shells only show as working while a foreground job runs (sampled
every 250 ms), so a command that finishes at once is taken as started after about 1.5 s
(agents: 10 s).
