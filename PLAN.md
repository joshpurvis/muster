# Agent Multiplexer: Project Plan

> Working name: TBD (shortlist: Muster, Drove, Switchyard, Corral, Tessel).
> Status: personal tool first. Distribution, teams, and monetization are explicitly deferred until the concept is proven.

## 0. Direction change (2026-09-23)

The Slack-style UI (sidebar of channels, inbox, timeline, composer, diff panel) is replaced by a
slim tiling-terminal UI: tabs with splits, one fuzzy jump across everything, and several client
windows at once so the window manager can tile them. Tabs are unnamed at creation and named
from what the agent does. Worktrees are opt-in (Ctrl+Shift+N). A CLI (`muster ls/read/send/wait/
keys/approve/new`, see docs/CLI.md) drives running agents from a shell or another agent. The
daemon, PTY handling, state detection, injection and MCP server stay; "channel" remains the API
name for a tab. Specs: docs/UI.md, docs/PROTOCOL.md (Windows, tabs and naming), docs/CLI.md.
Sections below describe the original v0 plan; where they mention the channel sidebar, inbox,
timeline or composer, section 0 wins.

## 1. What this is

A desktop app for running many coding agents (Claude Code, Codex CLI, plain shells) side by side. It is a terminal multiplexer rebuilt around agents: a Slack-like interface where each channel is a task backed by real terminals, with the app understanding what each agent is doing and letting agents control the environment themselves via MCP.

**Core premise:** agents are cheap and parallel; the scarce resource is human attention. Every feature should reduce how often the human has to look at a raw terminal.

## 2. Scope

### v0 (build now)
1. Multiplexing: spawn, arrange, persist, and reattach terminal sessions.
2. Agent state awareness: know when each agent is working, waiting, done, or stuck.
3. Intelligent injection: reliably send prompts, approvals, and interrupts to agent CLIs.
4. MCP server: agents can list, spawn, message, and read other sessions.

### Deferred (do not build yet, but don't design them out)
Quota-aware scheduling, team/multiplayer, mobile client, tournament mode, playbooks, time-travel recording, distribution/packaging for others. See section 8.

## 3. Hard constraints

- **Run the official, unmodified CLIs** (`claude`, `codex`) inside PTYs. The user logs in through each CLI's own login flow.
- **Never read, copy, store, or proxy CLI credential files or tokens.** The app only ever talks to agents through their terminal and their documented hooks/config.
- **No Anthropic/Claude Code/OpenAI names or logos in the product name or branding.**
- Anthropic's Claude Code legal page (code.claude.com/docs/en/legal-and-compliance) says subscription limits assume "ordinary, individual usage." Keep this in mind before building any scheduling or usage-maximizing feature later.
- Target macOS + Linux first. Windows (ConPTY) later.

## 4. Architecture

```
┌─────────────── single binary ───────────────┐
│                                              │
│  GUI mode (Tauri window)     daemon mode     │
│  web frontend + xterm.js ◄──► (--daemon)     │
│        WebSocket / UDS       │               │
│                              ├─ PTY manager  │
│                              ├─ VT state     │
│                              ├─ Agent adapters│
│                              ├─ Event bus    │
│                              ├─ MCP server   │
│                              └─ Store (SQLite)│
└──────────────────────────────────────────────┘
```

- **One Rust codebase, one binary.** Launched normally it opens the Tauri GUI; it starts itself with `--daemon` if no daemon is running (tmux model). No sidecar.
- **The daemon owns everything stateful:** PTYs, worktrees, agent state, event log. The GUI is a thin client. Closing the GUI must not kill agents.
- **Client protocol:** JSON (or MessagePack) over a Unix domain socket or localhost WebSocket with a local auth token. Design it so a future TUI, web, or mobile client can speak it too.
- **Suggested crates:** `tauri` v2, `portable-pty` (PTYs), `alacritty_terminal` or `vt100` for headless screen state (evaluate `libghostty-vt` later; it's alpha), `tokio`, `axum` (WS + MCP HTTP), `rusqlite` or `sqlx`, `git2` or shelling out to `git` for worktrees, `notify-rust` for OS notifications.
- **Frontend:** framework open (Svelte, Solid, or React). xterm.js for rendering terminals.

## 5. Core data model

- **Workspace**: a repo (or folder) the user is working in.
- **Channel**: a unit of work. Owns a git worktree, one or more sessions, a message timeline, and optional acceptance criteria. This is the "Slack channel."
- **Session**: one PTY process. Has a kind (`claude`, `codex`, `shell`), cwd, env, current agent state, scrollback, and headless screen state.
- **Event**: append-only log entry (output chunk, state change, hook event, injected input, MCP call, human message). Everything else is derived from events, which keeps later features (summaries, replay, time travel) cheap.

## 6. Features by phase

### Phase 0: Skeleton
- Daemon spawns a PTY, streams output to the GUI, accepts input.
- GUI renders it with xterm.js; resize propagates to the PTY.
- Close GUI, reopen, reattach with scrollback replay.
- **Done when:** you can run `claude` in the app, quit the window, reopen it, and the session is still alive.

### Phase 1: Multiplexing and the channel UI
- Sidebar of channels (Slack-style), each with its sessions as tabs/splits.
- "New channel" creates a git worktree + branch and starts a chosen agent in it.
- Per-channel environment isolation: allocate unique ports (`PORT`, etc.) per worktree so parallel dev servers don't collide.
- Channel timeline showing human notes and agent state changes alongside the terminal.
- **Done when:** 5+ agents in separate worktrees can be run and navigated comfortably.

### Phase 2: Agent state detection
- Normalized states: `starting`, `working`, `needs_approval`, `asking_question`, `idle/done`, `error`, `stuck`.
- Two signal sources, combined per adapter:
  1. **Structured hooks** (preferred): Claude Code hooks and Codex's notify config call back into the daemon (e.g., a tiny `yourapp hook <event>` subcommand that posts to the daemon socket). The implementing agent must check both CLIs' current docs; these APIs change.
  2. **Screen parsing** (fallback): match patterns on the headless VT screen state (approval prompts, input box idle, spinners).
- **Decision inbox:** one ranked queue of everything that needs the human (approvals, questions, errors), across all channels. Approve/deny/reply inline.
- OS notifications when something enters the inbox.
- Basic stuck detection: no meaningful output or the same failing command repeating for N minutes.

### Phase 3: Intelligent injection
- Per-agent **adapter** trait: how to detect readiness, submit a prompt, approve/deny, interrupt, and exit for each CLI.
- Reliable prompt submission: wait until the agent is ready, send text via bracketed paste for multi-line, send Enter as a separate write after a short delay, then verify the agent started working.
- **Input queue** per session: messages sent while an agent is busy are queued and delivered when it's ready (or optionally interrupt).
- Batch actions: "approve all similar" across sessions.
- **Done when:** sending a multi-line prompt to a busy agent reliably delivers once it's idle, with no half-typed input left in the box.

### Phase 4: MCP server
The daemon exposes an MCP server (localhost HTTP) that agents in any session can connect to, so agents can orchestrate the environment.

Initial tools:
- `list_sessions` / `list_channels`: what's running and in which state.
- `spawn_agent(channel, kind, prompt)`: start a new agent, optionally in a new worktree/channel.
- `send_message(session, text)`: goes through the Phase 3 injection queue.
- `read_screen(session)` / `get_recent_output(session, n)`: current screen or tail as plain text.
- `wait_for_state(session, state, timeout)`: block until an agent finishes or needs help.
- `post_to_channel(channel, text)`: write to the human-visible timeline.
- `get_diff(channel)`: worktree diff against its base branch.

Design requirements:
- Each session gets its own MCP identity/token (injected via env or config at spawn) so the daemon knows *which* agent is calling.
- Guardrails: max spawn depth and count, rate limits on `send_message`, no agent may approve its own or another agent's permission prompts unless the human enables it, and all MCP actions are logged as events and visible in the channel timeline.
- Auto-register the MCP server in each spawned agent's config for that session only (don't modify the user's global config without asking).

### Phase 5: Review and trust (after v0 is usable)
- Per-channel diff view.
- "Catch me up": short summary of what happened in a channel since you last looked, built from the event log.
- Acceptance criteria per channel (test/lint commands) run on demand or on agent `done`, shown as green/red.
- Link diff hunks to the transcript moment that produced them (needs event log + hook data on file edits).

## 7. Suggested order of work
Phase 0 → 1 → 2 → 3 → 4, but build the Phase 3 adapter trait early (Phase 1) since injection and state detection share it. Dogfood from Phase 1 onward: use the tool to build the tool.

## 8. Parked ideas (from brainstorming)
- **Quota-aware scheduler:** track subscription usage windows, queue work for resets, fall back between providers. Revisit against provider terms first.
- **Tournament mode:** same task to multiple agents in separate worktrees, compare/rank results.
- **File leases / collision detection:** agents declare files they're touching; warn on overlap.
- **Agent-to-agent review:** reviewer agent watches an implementer's channel (largely enabled by the MCP server).
- **Time-travel scrubber:** record panes + filesystem snapshots, scrub back, fork from a point.
- **Agent blame:** which channel/prompt/model wrote each line.
- **Hibernate/resume:** suspend a session with worktree + conversation (via CLI resume flags) and restore later.
- **Playbooks:** save a multi-agent workflow as a reusable template.
- **Multiplayer + mobile approvals:** relay server, teammates share a workspace, approve from phone. Each user must use their own CLI login.
- **Distribution:** packaging, auto-update, possible paid tier (team/relay features).

## 9. Open questions
- Headless VT backend: `alacritty_terminal` vs `vt100` vs `libghostty-vt`. Pick the one that best supports reliable screen-state matching for the agent CLIs.
- Frontend framework.
- How "catch me up" summaries get generated (e.g., the user's own `claude -p` in a session vs. an API key).
- Product name.

## 10. Bots (first pass, 2026-10-11)

A **bot** is a persistent, named agent that other bots (or the human) can ask things and get
replies from. Code: `src/bots.rs`; tools: docs/MCP.md "Bots"; commands: docs/CLI.md.

| Part | What |
|---|---|
| Record | `bots` table: name (unique, case-insensitive), kind (`claude` \| `codex`), model, effort, persona, home dir, permission profile, accepted origins, current session, conversation id. |
| Home | `<data>/bots/<name>/` with `CLAUDE.md` (Claude) or `AGENTS.md` (Codex) written from the persona, and `memory/` for durable notes the persona tells the bot to keep. |
| Session | At most one at a time: the agent runs in its home dir, in a tab named after the bot in the "bots" workspace (`<data>/bots`). `muster bot start`, or any message, starts it, restarts it if it exited, or reuses it. |
| Continuity | Claude: the SessionStart hook's `session_id` is saved, and every (re)start passes `--resume <id>` (`--fresh` forgets it). Codex: the notify `thread-id` is saved but not used yet (Codex resumes with a `codex resume <id>` subcommand the adapter doesn't build; TODO). |
| Asking | MCP `ask_bot` / `muster ask`: deliver through the injection queue, wait for the turn to end, return the Stop hook's final message (Codex: notify's), else the end of the terminal. |
| Messages | Each delivery gets a `bot_messages` row (from, to, origin, hop, reply_to, reply) and a one-line envelope before the text: `[muster msg <id> from <sender> origin=<origin> hop=<n>]`. |

**Origin rule.** Every message carries the origin of the chain it belongs to: `human`,
`schedule` or `webhook`. A message a session sends while handling message M inherits M's origin
at hop+1 (the "current inbound message", set from the UserPromptSubmit hook's envelope, and
inherited down `parent_id`, so a helper a bot spawned carries it too). A prompt the human types,
or sends from the UI or CLI outside a muster session, is `human`, hop 0. Delivery is refused
when the bot doesn't accept the origin, past hop 4, or past 200 messages to the bot in 24 hours
(constants in `src/mcp/guard.rs`). Non-bot Claude agents follow the Claude default: no
`send_message` or `spawn_agent` of a Claude agent from a non-human chain.

Defaults: Claude bots accept `human` only; Codex bots accept `human`, `schedule` and `webhook`,
so scheduled and triggered work routes to Codex. Why: Anthropic's consumer terms are unclear
about unattended use of a subscription (§3: limits assume "ordinary, individual usage"). They
don't forbid scheduling as such: Claude Code itself has `/loop` and `CronCreate` (at least a
1-minute interval; recurring loops expire after 7 days; code.claude.com/docs/en/scheduled-tasks),
and the stated risk is round-the-clock background use, which is what the weekly caps are for. So
a Claude bot may opt in to `schedule` per bot (`muster bot new --accepts human,schedule`); the
default stays human-only.

**Deferred** (don't design them out):

- Hibernate and wake: stop an idle bot's process, resume it on the next message.
- Schedules and webhooks: the origins and the policy exist; the triggers don't (an internal
  `origin` parameter on `bot.ask` stands in for tests). Rules for when they're built: a Claude
  bot's schedules mirror Anthropic's bounds (at least a 1-minute interval, a recurring schedule
  expires after 7 days unless renewed, quiet hours supported); webhook triggers are never
  exposed to anyone but the user.
- A phone chat view per bot.
- An async `reply(msg_id)` tool, so a bot can answer later instead of in the same turn.
