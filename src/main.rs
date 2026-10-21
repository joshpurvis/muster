mod account_profiles;
mod adapters;
mod agents;
mod assets;
mod cache;
mod cli;
mod client;
mod detect;
mod devices;
mod executables;
mod gui;
mod hookcmd;
mod hook_inbox;
mod hub;
mod inject;
mod mcp;
mod model;
mod notes;
mod panels;
mod panel_apps;
mod panel_kit;
mod web_preview;
mod skills;
mod notify;
mod openrouter;
mod paths;
mod pr;
mod renderer;
mod pty;
mod rpc;
mod schedules;
mod server;
mod store;
mod terminal;
mod updates;
mod wm;
mod worktree;


use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};
#[cfg(test)]
use clap::CommandFactory;

use crate::paths::Discovery;

#[derive(Parser)]
#[command(
    name = "muster",
    version,
    about = "Run many coding agents side by side, in tabs and splits.",
    after_help = "With no command: open a new window (starting the daemon if needed).\n\
Session commands take a TARGET: an id, `.` (this session), a session or tab name, a unique\n\
prefix, or tab/name. Exit codes: 0 ok, 1 error, 2 bad or ambiguous target, 124 timeout."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open and manage agent panels in this session's task.
    Panel {
        #[command(subcommand)]
        action: cli::PanelCmd,
    },
    /// Plain-text notes shared by you and agents: list, read, write, append, open.
    Note {
        #[command(subcommand)]
        action: cli::NoteCmd,
    },
    /// Schedules: recurring or one-off messages to a named agent (add, ls, rm, pause, resume,
    /// run, skip, renew, history).
    Schedule {
        #[command(subcommand)]
        action: cli::ScheduleCmd,
    },
    /// Read-only GitHub PR fetches for reviews: the open PR list, or one PR's snapshot (JSON).
    Pr {
        #[command(subcommand)]
        action: pr::PrCmd,
    },
    /// Install Muster's agent skills (PR review, panels) for Claude Code and Codex.
    Skills {
        #[command(subcommand)]
        action: skills::SkillsCmd,
    },
    /// Run the daemon (detaches into the background unless --foreground).
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    /// Forward an agent hook callback to the daemon (used inside agent CLIs).
    Hook {
        agent: String,
        event: String,
        payload: Option<String>,
    },
    /// Show whether the daemon is running.
    Status,
    /// Stop the daemon (running sessions are terminated).
    Stop,
    /// Restart only the application backend; host-owned terminal processes continue.
    Reload,
    /// Activate a built UI directory without restarting backend or terminals.
    UiInstall {
        /// Built UI directory (for example web/dist).
        path: PathBuf,
    },
    /// Print the UI URL (with its access token).
    Url,
    /// Open a UI window; with PATH, add the folder and open a shell tab there.
    Open {
        /// Folder or repo to open a shell tab in (added as a workspace if new).
        path: Option<PathBuf>,
        /// Window to open (and to put the new tab in).
        #[arg(long, value_name = "N")]
        window: Option<i64>,
    },
    /// List live sessions (never starts the daemon).
    #[command(visible_alias = "list")]
    Ls {
        /// Include exited sessions.
        #[arg(long)]
        all: bool,
        /// Print SessionInfo[] (with channel_name and window_id) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a session's recent output as plain text.
    Read {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// How many lines (at most 10000).
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: u32,
        /// Print the visible screen instead.
        #[arg(long)]
        screen: bool,
        /// Print {id, lines} as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Send a prompt (bracketed paste + Enter; queued while the agent is busy).
    #[command(after_help = "Examples:\n  muster send fix-auth run the tests --wait\n  git diff | muster send 12 -\n  muster send 12 -- -1 is not a flag here\n\nText starting with `-` goes after `--`.")]
    Send {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Prompt words, joined by spaces. None or `-`: read stdin.
        text: Vec<String>,
        /// Interrupt a busy agent first.
        #[arg(long)]
        interrupt: bool,
        /// Wait until the agent has started and settled again; print its state.
        #[arg(long)]
        wait: bool,
        /// State to wait for (repeatable; implies --wait). Default: any settled state.
        #[arg(long, value_name = "STATE", value_parser = cli::STATES)]
        until: Vec<String>,
        /// With --wait: give up after SECS (exit 124).
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        timeout: u64,
    },
    /// Inspect queued prompts and any delivery paused for review after a backend crash.
    Queue {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Discard waiting/uncertain delivery records after inspecting the terminal.
        #[arg(long, conflicts_with = "resume")]
        clear: bool,
        /// Mark the uncertain delivery as reviewed and resume the prompts waiting behind it
        /// (the uncertain prompt is not resent).
        #[arg(long)]
        resume: bool,
    },
    /// Block until a session reaches a state (exit 124 on timeout).
    Wait {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// State to wait for (repeatable). Default: idle, needs_approval, asking_question,
        /// error, stuck, exited.
        #[arg(long, value_name = "STATE", value_parser = cli::STATES)]
        until: Vec<String>,
        /// Give up after SECS (exit 124).
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        timeout: u64,
        /// Print {session, matched, timed_out} as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Send keys to a session's terminal.
    #[command(after_help = "Keys: enter, esc, tab, shift+tab, space, backspace, up, down, left, right, \
home, end, delete, pageup, pagedown, ctrl+<x>, alt+<x>, ctrl+left (etc.), a single character, or text:<literal>.\n\
Example: muster keys 12 esc text:/compact enter")]
    Keys {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Keys, sent in order (see below).
        #[arg(required = true, allow_hyphen_values = true)]
        keys: Vec<String>,
    },
    /// Answer a session's permission prompt (approve by default).
    Approve {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Approve and don't ask again for this kind of action.
        #[arg(long, conflicts_with = "deny")]
        always: bool,
        /// Deny instead.
        #[arg(long)]
        deny: bool,
    },
    /// Interrupt a session (Esc for agents, Ctrl-C for shells).
    Interrupt {
        #[arg(help = TARGET_HELP)]
        target: String,
    },
    /// Open a new tab and start an agent in it; prints the new session id.
    #[command(after_help = "Arguments after `--` go to the agent CLI, after muster's permission flags:\n  \
muster new . --kind codex --prompt \"hi\" -- -m gpt-6-sol -c model_reasoning_effort=xhigh\n  \
muster new --kind claude -- --model haiku")]
    New {
        /// Folder or repo (default: current directory).
        path: Option<PathBuf>,
        /// Claude profile name/id, or "default" for ordinary inherited login behavior.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// What to start in the tab (none: an empty tab). `--agent` is an older alias.
        #[arg(long, alias = "agent", default_value = "claude", value_parser = ["claude", "codex", "shell", "none"])]
        kind: String,
        /// Work in a new worktree on branch muster/<tab>.
        #[arg(long)]
        worktree: bool,
        /// Branch or commit the worktree starts from.
        #[arg(long, value_name = "REF", requires = "worktree")]
        base: Option<String>,
        /// Tab name (default: named automatically).
        #[arg(long)]
        name: Option<String>,
        /// Window for the tab (default: this session's window, else the last focused one).
        #[arg(long, value_name = "N")]
        window: Option<i64>,
        /// First prompt for the agent.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Print {channel, session} as JSON.
        #[arg(long)]
        json: bool,
        /// Extra arguments for the agent CLI (not with --kind none).
        #[arg(last = true, value_name = "AGENT_ARGS")]
        agent_args: Vec<String>,
    },
    /// Rename a session, or its tab.
    Rename {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// The new name.
        name: String,
        /// Rename the session's tab instead.
        #[arg(long)]
        tab: bool,
    },
    /// Move a session (tab) to another window.
    Move {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// The window to show it in (must exist).
        #[arg(long, value_name = "N")]
        window: i64,
    },
    /// Kill a session.
    Kill {
        #[arg(help = TARGET_HELP)]
        target: String,
    },
    /// Restart a session (same id, new process).
    Restart {
        #[arg(help = TARGET_HELP)]
        target: String,
    },
    /// Stream daemon events: one line per session state change, or every event as JSON.
    #[command(after_help = "Human lines: HH:MM:SS  ID  TAB/NAME  FROM -> TO  DETAIL\n\
--json prints the session_*, channel_* and window_* events as the daemon sends them, one per line.\n\
Runs until Ctrl-C or the daemon stops (exit 0). Never starts the daemon (exit 1 when none runs).")]
    Events {
        /// Print every event as one JSON object per line.
        #[arg(long)]
        json: bool,
        /// Only this session's events.
        #[arg(long, value_name = "TARGET")]
        session: Option<String>,
        /// Only this tab's events (exact name, else unique prefix).
        #[arg(long, value_name = "NAME")]
        tab: Option<String>,
    },
    /// Pair a phone: prints a QR code and a short code for the phone UI (over Tailscale).
    #[command(after_help = "The phone opens https://<this machine>.<tailnet>.ts.net/m/ through `tailscale serve`\n\
(the commands are printed when it isn't set up). Scan the QR code with the phone's camera, or open\n\
that address and type the short code. The code works once, for --ttl seconds; three wrong codes burn it.\n\
See docs/ANDROID.md.")]
    Pair {
        /// Device name (default: what the phone reports).
        #[arg(long)]
        name: Option<String>,
        /// What the phone may do: view, interact (approve, prompt, keys), or manage (also open
        /// and close tabs).
        #[arg(long, default_value = "interact", value_parser = ["view", "interact", "manage"])]
        scope: String,
        /// Seconds the code stays valid.
        #[arg(long, default_value_t = 120)]
        ttl: u64,
        /// Address the phone opens (default: https://<this machine's Tailscale name>).
        #[arg(long)]
        url: Option<String>,
    },
    /// List paired devices, or revoke one.
    Devices {
        #[command(subcommand)]
        action: Option<DevicesCmd>,
        /// Print Device[] as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage named agents: persistent agents with a home folder that others can ask things
    /// (see `muster ask`).
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Manage Muster's references to existing Claude configuration directories. Never logs in.
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
    },
    /// Ask an agent something and print its reply (starts the agent if needed).
    #[command(after_help = "Examples:\n  muster ask reviewer is the diff in fix-auth safe to merge?\n  git log -5 | muster ask scout -\n\n\
Exit codes: 0 replied, 1 error or refused, 124 timeout. Inside a muster session the message\n\
carries that session's origin (docs/MCP.md, \"Agents\").")]
    Ask {
        /// Agent name (case-insensitive).
        agent: String,
        /// Words, joined by spaces. None or `-`: read stdin.
        text: Vec<String>,
        /// Give up waiting after SECS (exit 124; the message stays delivered).
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        timeout: u64,
        /// Print the agent.ask reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a native UI window in this process (started by `muster` / `muster open`).
    #[command(hide = true)]
    NativeWindow {
        #[arg(long)]
        window: Option<i64>,
    },
}

#[derive(Subcommand)]
enum DevicesCmd {
    /// Revoke a device (by id or name): its connections close and it must pair again.
    Revoke {
        /// Device id or exact name.
        device: Option<String>,
        /// Revoke every device.
        #[arg(long, conflicts_with = "device")]
        all: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List registered directories. Account identity is checked in Claude Code with /status.
    #[command(visible_alias = "list")]
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Register a directory already set up through the official Claude Code CLI.
    Add { name: String, directory: PathBuf },
    /// Change only the display name; the directory binding cannot change.
    Rename { profile: String, name: String },
    /// Allow new starts on this profile again.
    Enable { profile: String },
    /// Prevent new starts, including restarts. Running processes continue normally.
    Disable { profile: String },
    /// Select a workspace's default for new Claude sessions. PROFILE may be "default".
    Default { path: PathBuf, profile: String },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Create an agent: its home folder (persona file, memory/) under the data dir.
    New {
        /// Agent name: letters, digits, `-`, `_` (unique, case-insensitive).
        name: String,
        /// Pin this Claude agent to a profile name/id, or "default". Fixed after creation.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        #[arg(long, default_value = "claude", value_parser = ["claude", "codex"])]
        kind: String,
        /// Where the model runs: subscription (the CLI's own login) or openrouter (pay per
        /// token with the backend's OPENROUTER_API_KEY; a Codex agent there needs --model).
        #[arg(long, value_name = "P", value_parser = ["subscription", "openrouter"])]
        provider: Option<String>,
        /// Model for the agent CLI.
        #[arg(long, value_name = "M")]
        model: Option<String>,
        /// Reasoning effort (claude --effort, codex model_reasoning_effort).
        #[arg(long, value_name = "E")]
        effort: Option<String>,
        /// Who the agent is and what it does (written to its CLAUDE.md / AGENTS.md).
        #[arg(long, value_name = "TEXT", conflicts_with = "persona_file")]
        persona: Option<String>,
        /// Read the persona from a file.
        #[arg(long, value_name = "F")]
        persona_file: Option<PathBuf>,
        /// Message origins it accepts (default: human for claude, human,schedule,webhook for codex).
        #[arg(long, value_name = "ORIGINS")]
        accepts: Option<String>,
        /// Start it whenever the daemon starts.
        #[arg(long)]
        autostart: bool,
        #[command(flatten)]
        perms: AgentPerms,
    },
    /// Change an agent: persona, model, effort, accepted origins, autostart, permissions
    /// (model, effort and permissions apply from its next start).
    Edit {
        name: String,
        /// Where the model runs: subscription (the CLI's own login) or openrouter (pay per
        /// token with the backend's OPENROUTER_API_KEY; a Codex agent there needs --model).
        #[arg(long, value_name = "P", value_parser = ["subscription", "openrouter"])]
        provider: Option<String>,
        /// Model for the agent CLI (empty: the CLI's default).
        #[arg(long, value_name = "M")]
        model: Option<String>,
        /// Reasoning effort (empty: the CLI's default).
        #[arg(long, value_name = "E")]
        effort: Option<String>,
        /// New persona (rewrites its CLAUDE.md / AGENTS.md; memory/ is untouched).
        #[arg(long, value_name = "TEXT", conflicts_with = "persona_file")]
        persona: Option<String>,
        /// Read the new persona from a file.
        #[arg(long, value_name = "F")]
        persona_file: Option<PathBuf>,
        /// Message origins it accepts, e.g. human,schedule.
        #[arg(long, value_name = "ORIGINS")]
        accepts: Option<String>,
        /// Start it whenever the daemon starts.
        #[arg(long, value_name = "on|off", value_parser = ["on", "off"])]
        autostart: Option<String>,
        #[command(flatten)]
        perms: AgentPerms,
    },
    /// List agents.
    #[command(visible_alias = "list")]
    Ls {
        /// Print the agents as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove an agent and its session (its home folder stays on disk).
    Rm { name: String },
    /// Start the agent's session (or report the running one); prints its session id.
    Start {
        name: String,
        /// Start a new conversation instead of resuming the last one.
        #[arg(long)]
        fresh: bool,
    },
}

/// An agent's tab profile (docs/PROTOCOL.md "Permissions").
#[derive(clap::Args)]
struct AgentPerms {
    /// Claude permission mode of its tab.
    #[arg(long, value_name = "MODE", value_parser = ["plan", "default", "accept_edits", "bypass"])]
    claude_mode: Option<String>,
    /// Codex sandbox of its tab.
    #[arg(long, value_name = "SANDBOX", value_parser = ["read_only", "workspace_write", "workspace_write_network", "full_access"])]
    codex_sandbox: Option<String>,
}

fn read_persona(persona: Option<String>, file: Option<PathBuf>) -> anyhow::Result<Option<String>> {
    match file {
        Some(f) => Ok(Some(std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?)),
        None => Ok(persona),
    }
}

const TARGET_HELP: &str = "Session: id, `.` (this session), session or tab name, unique prefix, or tab/name";

fn main() -> ExitCode {
    // Hook callbacks run inside agent CLIs on every event: dispatch before anything else
    // (no clap, no runtime) so they stay fast.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook") {
        let code = hookcmd::run(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
            args.get(4).cloned(),
        );
        return ExitCode::from(code.clamp(0, 255) as u8);
    }

    let cli = Cli::parse();
    // Printing commands die quietly on a closed pipe (`muster ls | head`) instead of panicking.
    // Not the daemon or a window: they write to sockets and must keep ignoring SIGPIPE (a Rust
    // binary starts with it ignored, so processes they spawn are unaffected either way).
    if !matches!(cli.cmd, None | Some(Cmd::Daemon { foreground: true }) | Some(Cmd::NativeWindow { .. })) {
        // SAFETY: setting a signal disposition before any threads exist.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let r = match cli.cmd {
        None => ensure_daemon().and_then(|d| open_ui(&d, None)),
        Some(Cmd::Daemon { foreground: true }) => run_daemon(),
        Some(Cmd::Daemon { foreground: false }) => start_daemon_detached().map(|d| println!("{}", d.ui_url())),
        Some(Cmd::Hook { .. }) => unreachable!("handled above"),
        Some(Cmd::Status) => status(),
        Some(Cmd::Stop) => stop(),
        Some(Cmd::Reload) => reload(),
        Some(Cmd::UiInstall { path }) => {
            if std::env::var_os("MUSTER_WEB_DIR").is_some() {
                Err(anyhow::anyhow!("MUSTER_WEB_DIR overrides installed UI releases; update that directory or unset the override on the backend"))
            } else {
                assets::install(&path, &paths::data_dir()).map(|release| println!("UI activated at {}; reload the window (open windows offer it)", release.display()))
            }
        },
        Some(Cmd::Skills { action }) => skills::run(action),
        Some(Cmd::Pr { action }) => pr::run(action),
        Some(Cmd::Url) => running().map(|d| println!("{}", d.ui_url())),
        Some(Cmd::Open { path, window }) => open(path, window),
        Some(Cmd::NativeWindow { window }) => {
            #[cfg(all(feature = "gui", target_os = "linux"))]
            gui::linux::take_reexec_flag();
            ensure_daemon().and_then(|d| gui::run_window(&d, window, || browser_url(&d, window)))
        }
        Some(Cmd::Ls { all, json }) => cli::ls(find_daemon().as_ref(), all, json),
        Some(Cmd::Pair { name, scope, ttl, url }) => {
            ensure_daemon().and_then(|d| cli::pair(&d, name.as_deref(), &scope, ttl, url.as_deref()))
        }
        Some(Cmd::Devices { action, json }) => running().and_then(|d| match action {
            None => cli::devices(&d, json),
            Some(DevicesCmd::Revoke { device, all }) => cli::revoke_device(&d, device.as_deref(), all),
        }),
        Some(Cmd::Events { json, session, tab }) => {
            running().and_then(|d| cli::events(&d, json, session.as_deref(), tab.as_deref()))
        }
        Some(cmd) => ensure_daemon().and_then(|d| agent_cmd(&d, cmd)),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("muster: {e:#}");
            ExitCode::from(e.downcast_ref::<cli::Exit>().map_or(1, |x| x.code))
        }
    }
}

/// Commands that act on sessions (docs/CLI.md).
fn agent_cmd(d: &Discovery, cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Panel { action } => cli::panel(d, action),
        Cmd::Note { action } => cli::note(d, action),
        Cmd::Schedule { action } => cli::schedule(d, action),
        Cmd::Read {
            target,
            lines,
            screen,
            json,
        } => cli::read(d, &target, lines, screen, json),
        Cmd::Send {
            target,
            text,
            interrupt,
            wait,
            until,
            timeout,
        } => cli::send(
            d,
            &target,
            &text,
            cli::SendOpts {
                interrupt,
                wait,
                until,
                timeout,
            },
        ),
        Cmd::Wait {
            target,
            until,
            timeout,
            json,
        } => cli::wait(d, &target, until, timeout, json),
        Cmd::Queue { target, clear, resume } => cli::queue(d, &target, clear, resume),
        Cmd::Keys { target, keys } => cli::keys(d, &target, &keys),
        Cmd::Approve { target, always, deny } => cli::approve(d, &target, always, deny),
        Cmd::Interrupt { target } => cli::by_id(d, &target, "session.interrupt"),
        Cmd::Kill { target } => cli::by_id(d, &target, "session.kill"),
        Cmd::Restart { target } => cli::by_id(d, &target, "session.restart"),
        Cmd::Rename { target, name, tab } => cli::rename(d, &target, &name, tab),
        Cmd::Move { target, window } => cli::move_to_window(d, &target, window),
        Cmd::New {
            path,
            profile,
            kind,
            worktree,
            base,
            name,
            window,
            prompt,
            json,
            agent_args,
        } => cli::new(
            d,
            cli::NewOpts {
                path: path.unwrap_or_else(|| PathBuf::from(".")),
                profile,
                kind,
                worktree,
                base,
                name,
                window,
                prompt,
                json,
                agent_args,
            },
        ),
        Cmd::Agent { action } => match action {
            AgentCmd::New {
                name,
                profile,
                kind,
                provider,
                model,
                effort,
                persona,
                persona_file,
                accepts,
                autostart,
                perms,
            } => cli::agent_new(
                d,
                cli::AgentOpts {
                    name,
                    profile,
                    kind: Some(kind),
                    provider,
                    model,
                    effort,
                    persona: read_persona(persona, persona_file)?,
                    accepts,
                    autostart: Some(autostart),
                    claude_mode: perms.claude_mode,
                    codex_sandbox: perms.codex_sandbox,
                },
            ),
            AgentCmd::Edit {
                name,
                provider,
                model,
                effort,
                persona,
                persona_file,
                accepts,
                autostart,
                perms,
            } => cli::agent_edit(
                d,
                cli::AgentOpts {
                    name,
                    kind: None,
                    profile: None,
                    provider,
                    model,
                    effort,
                    persona: read_persona(persona, persona_file)?,
                    accepts,
                    autostart: autostart.map(|a| a == "on"),
                    claude_mode: perms.claude_mode,
                    codex_sandbox: perms.codex_sandbox,
                },
            ),
            AgentCmd::Ls { json } => cli::agent_ls(d, json),
            AgentCmd::Rm { name } => cli::agent_rm(d, &name),
            AgentCmd::Start { name, fresh } => cli::agent_start(d, &name, fresh),
        },
        Cmd::Profile { action } => match action {
            ProfileCmd::Ls { json } => cli::profile_list(d, json),
            ProfileCmd::Add { name, directory } => cli::profile_add(d, &name, &directory),
            ProfileCmd::Rename { profile, name } => cli::profile_update(d, &profile, Some(name), None),
            ProfileCmd::Enable { profile } => cli::profile_update(d, &profile, None, Some(true)),
            ProfileCmd::Disable { profile } => cli::profile_update(d, &profile, None, Some(false)),
            ProfileCmd::Default { path, profile } => cli::profile_default(d, &path, &profile),
        },
        Cmd::Ask {
            agent,
            text,
            timeout,
            json,
        } => cli::ask(d, &agent, &text, timeout, json),
        _ => unreachable!("dispatched in main"),
    }
}

// ----- CLI side -----

/// The running daemon, if its discovery file is fresh and it answers.
fn find_daemon() -> Option<Discovery> {
    let d = paths::read_discovery()?;
    (backend_alive(&d) && client::healthy(d.port)).then_some(d)
}

/// Is the backend `d` names running? By its pid where this process can see it. A command run in
/// its own PID namespace can't (Claude Code's Bash under `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, in
/// an OpenRouter session): there the backend must answer its token-authenticated `hello` with
/// that pid, which a stale discovery file or a recycled pid can't fake.
fn backend_alive(d: &Discovery) -> bool {
    if paths::pid_alive(d.pid) {
        return is_muster_process(d.pid);
    }
    backend_answers_as(d)
}

/// The backend at `d.port` knows `d.token` and reports `d.pid` as its own.
fn backend_answers_as(d: &Discovery) -> bool {
    client::rpc_with_timeout(d, "hello", serde_json::json!({}), Duration::from_secs(5))
        .is_ok_and(|h| h["pid"].as_u64() == Some(d.pid as u64))
}

/// The backend runs but this process can't see (or signal) it: see [`backend_alive`].
fn backend_hidden(d: &Discovery) -> bool {
    !paths::pid_alive(d.pid) && backend_answers_as(d)
}

fn running() -> anyhow::Result<Discovery> {
    find_daemon().context("daemon is not running (start it with `muster daemon`)")
}

fn ensure_daemon() -> anyhow::Result<Discovery> {
    match find_daemon() {
        Some(d) => Ok(d),
        None => start_daemon_detached(),
    }
}

/// Re-exec `muster daemon --foreground` in its own session, logging to the runtime dir,
/// and wait until it answers.
fn start_daemon_detached() -> anyhow::Result<Discovery> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    if let Some(d) = find_daemon() {
        return Ok(d);
    }
    let rt = paths::runtime_dir();
    paths::ensure_private_dir(&rt)?;
    let log_path = paths::log_path();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    paths::make_private_file(&log_path);
    let exe = std::env::current_exe().context("locating the muster binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the daemon from our terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting daemon")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    loop {
        if let Some(d) = find_daemon() {
            // No need to reap: the daemon outlives us and is reparented to init.
            return Ok(d);
        }
        if exited.is_none()
            && let Ok(Some(st)) = child.try_wait()
        {
            exited = Some((st, Instant::now()));
        }
        // Our child may have lost a startup race to another daemon: give that one a moment.
        if let Some((st, at)) = exited
            && at.elapsed() > Duration::from_secs(3)
        {
            anyhow::bail!("daemon exited during startup ({st}); see {}", log_path.display());
        }
        if Instant::now() > deadline {
            anyhow::bail!("daemon did not come up within 15s; see {}", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn status() -> anyhow::Result<()> {
    let Some(d) = find_daemon() else {
        println!("muster daemon: not running");
        std::process::exit(1);
    };
    println!("muster daemon: running (pid {}, version {})", d.pid, d.version);
    println!("url: {}", d.base_url());
    if let Ok(h) = client::rpc(&d, "hello", serde_json::json!({})) {
        let sessions = h["sessions"].as_array().cloned().unwrap_or_default();
        let live = sessions.iter().filter(|s| s["state"] != "exited").count();
        println!(
            "workspaces: {}  channels: {}  sessions: {} running / {} total",
            h["workspaces"].as_array().map_or(0, |a| a.len()),
            h["channels"].as_array().map_or(0, |a| a.len()),
            live,
            sessions.len()
        );
    }
    println!("data: {}", paths::data_dir().display());
    #[cfg(all(feature = "gui", target_os = "linux"))]
    if std::env::var_os("MUSTER_BROWSER").is_none() {
        println!("next native window: {}", gui::linux::status_line());
    }
    Ok(())
}

fn stop() -> anyhow::Result<()> {
    stop_backend(None)?;
    let socket = paths::runtime_dir().join("terminal.sock");
    match muster_terminal_protocol::client::Client::connect(&socket) {
        Ok(host) => {
            anyhow::ensure!(host.hello.data_dir == std::fs::canonicalize(paths::data_dir().join("terminal"))?,
                "terminal host belongs to another data directory");
            host.call(muster_terminal_protocol::Command::Shutdown)?;
            let end = Instant::now() + Duration::from_secs(10);
            while socket.exists() {
                anyhow::ensure!(Instant::now() < end, "terminal host did not stop within 10s");
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("muster terminal host stopped");
        }
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e|
            matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => {},
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Replacing a legacy monolithic daemon would kill its PTYs. Require a live,
/// compatible host and the same backend identity immediately before signalling.
fn reload() -> anyhow::Result<()> {
    let discovery = paths::read_discovery().filter(backend_alive);
    if let Some(d) = &discovery {
        let hello = client::rpc(d, "hello", serde_json::json!({}))
            .context("cannot verify restart safety; backend left running")?;
        let host = terminal::TerminalService::connect(
            &paths::runtime_dir().join("terminal.sock"), &paths::data_dir().join("terminal"),
        ).context("cannot verify a compatible terminal host; backend left running")?;
        check_reload(&hello, &host.client.hello, &paths::lock_path())?;
        // From a PID namespace this command can't stop the backend (nor see its replacement's
        // process), so the backend restarts itself, keeping its environment.
        if backend_hidden(d) {
            anyhow::ensure!(
                std::env::var_os(openrouter::ENV_KEY).is_none(),
                "this command runs where Muster's backend process isn't visible (a sandboxed command), so the backend \
                 has to restart itself with its own environment; {} set here can't be applied. Run it outside the sandbox",
                openrouter::ENV_KEY
            );
            reload_handoff(d)?;
            println!("muster backend restarted by the backend itself, keeping its environment and OpenRouter key; terminals preserved");
            return Ok(());
        }
        // The replacement gets this command's environment. When that has no OpenRouter key and
        // the backend has one (only in its memory), the backend restarts itself instead, so the
        // key carries over. OPENROUTER_API_KEY set here, even empty, replaces (or clears) it.
        if std::env::var_os(openrouter::ENV_KEY).is_none() && hello["openrouter"]["key_present"] == true {
            match reload_handoff(d) {
                Ok(()) => {
                    println!("muster backend restarted by the backend itself, keeping its environment and OpenRouter key; terminals preserved");
                    return Ok(());
                }
                Err(error) if error.to_string().contains(crate::updates::HANDOFF_REFUSED) => {
                    eprintln!("note: the backend can't restart itself here ({error:#}); reloading from this shell instead");
                }
                Err(error) => return Err(error),
            }
        }
        warn_openrouter_key_lost(&hello);
        stop_backend(Some(d))?;
    }
    start_daemon_detached()?;
    println!("muster backend restarted; terminals preserved");
    Ok(())
}

/// Ask the backend to run `reload` itself (`daemon.reload_handoff`), then wait for its
/// replacement. A refusal (a message with `HANDOFF_REFUSED`) is returned as an error so the
/// caller reloads the usual way; any other failure may have started the reload already, so it
/// waits for a replacement rather than starting a second one.
fn reload_handoff(d: &Discovery) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating the muster binary")?;
    let sent = client::rpc(d, "daemon.reload_handoff", serde_json::json!({ "exe": exe }));
    if let Err(error) = &sent
        && error.to_string().contains(crate::updates::HANDOFF_REFUSED)
    {
        return sent.map(|_| ());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(next) = paths::read_discovery().filter(|n| n.pid != d.pid && backend_alive(n))
            && client::rpc(&next, "hello", serde_json::json!({})).is_ok()
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(match sent {
                Err(error) => error.context("the backend did not restart"),
                Ok(_) => anyhow::anyhow!("the backend did not come back within 30 s; see {}", paths::log_path().display()),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The replacement backend gets this command's environment, so an OpenRouter key the running
/// backend has and this shell doesn't is gone after the reload: say so (never refuse).
fn warn_openrouter_key_lost(hello: &serde_json::Value) {
    let here = std::env::var(openrouter::ENV_KEY).ok().and_then(|v| openrouter::ApiKey::new(&v)).is_some();
    if hello["openrouter"]["key_present"] == true && !here {
        eprintln!(
            "note: the running backend has an OpenRouter key and this command's {key} is empty or unusable, so the \
             restarted backend won't: OpenRouter agents won't start until it is restarted with one \
             (`env {key}=… muster reload`).",
            key = openrouter::ENV_KEY
        );
    }
}

/// Everything the replacement backend will require of the running host, checked before
/// the working backend is stopped: a refused reload leaves it running.
fn check_reload(backend: &serde_json::Value, host: &muster_terminal_protocol::Hello, lock: &std::path::Path)
    -> anyhow::Result<()> {
    validate_reload_host(backend, host)?;
    // The replacement refuses a host whose fence it cannot verify (`terminal::ensure_host`).
    terminal::verify_fence(host, lock)
        .context("the replacement backend would refuse this terminal host; backend left running")
}

fn validate_reload_host(backend: &serde_json::Value, host: &muster_terminal_protocol::Hello) -> anyhow::Result<()> {
    anyhow::ensure!(backend["terminal_host"]["protocol"].as_u64() == Some(host.protocol as u64)
        && backend["terminal_host"]["instance"].as_str() == Some(host.instance.as_str()),
        "backend does not confirm ownership by this terminal host; reload refused to preserve existing terminals");
    Ok(())
}

fn stop_backend(expected: Option<&Discovery>) -> anyhow::Result<()> {
    if let Some(d) = paths::read_discovery().filter(backend_hidden) {
        anyhow::bail!("Muster's backend (pid {}) runs where this command can't signal it (a sandboxed command); stop it from outside", d.pid);
    }
    let Some(d) = paths::read_discovery().filter(|d| paths::pid_alive(d.pid)) else {
        println!("muster daemon: not running");
        return Ok(());
    };
    if let Some(expected) = expected {
        anyhow::ensure!(d.pid == expected.pid && d.token == expected.token && d.port == expected.port,
            "backend changed while preparing reload; nothing was stopped");
    }
    // Guard against a stale daemon.json whose pid now belongs to an unrelated process.
    if !is_muster_process(d.pid) {
        let _ = std::fs::remove_file(paths::discovery_path());
        println!("muster daemon: not running (removed stale daemon.json)");
        return Ok(());
    }
    // SAFETY: plain kill(2).
    unsafe {
        libc::kill(d.pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while paths::pid_alive(d.pid) && is_muster_process(d.pid) {
        if Instant::now() > deadline {
            anyhow::bail!("daemon (pid {}) did not stop within 10s", d.pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("muster daemon stopped");
    Ok(())
}

/// Best effort: does `pid` look like a `muster daemon` process?
fn is_muster_process(pid: u32) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => {
            let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            let exe_is_muster = args
                .first()
                .is_some_and(|a| String::from_utf8_lossy(a).rsplit('/').next() == Some("muster"));
            exe_is_muster && args.iter().any(|a| *a == b"daemon")
        }
        // No procfs (macOS): fall back to trusting the pid.
        Err(_) => !std::path::Path::new("/proc/self").exists(),
    }
}

/// `muster open [PATH] [--window N]`: with PATH, add the folder (`workspace.add`, idempotent)
/// and open a new shell tab there (`channel.create` with a shell: a task without a session
/// would have no tab to show) in window N or a new window (`window.create`); then open the UI
/// on that window.
fn open(path: Option<PathBuf>, window: Option<i64>) -> anyhow::Result<()> {
    let d = ensure_daemon()?;
    let window = match path {
        None => window,
        Some(path) => {
            let abs = std::fs::canonicalize(&path).with_context(|| format!("{}", path.display()))?;
            let ws = client::rpc(&d, "workspace.add", serde_json::json!({ "path": abs }))?;
            let ws_id = ws["id"].as_i64().context("workspace.add returned no id")?;
            let win = match window {
                Some(w) => w,
                None => client::rpc(&d, "window.create", serde_json::json!({}))?["id"]
                    .as_i64()
                    .context("window.create returned no id")?,
            };
            let ch = client::rpc(
                &d,
                "channel.create",
                serde_json::json!({ "workspace_id": ws_id, "window_id": win, "worktree": false, "agent": "shell" }),
            )?;
            println!(
                "{}: tab {} in window {win}",
                abs.display(),
                ch["name"].as_str().unwrap_or("?")
            );
            Some(win)
        }
    };
    open_ui(&d, window)
}

/// Open the UI (on `window`, else a window the page claims): the native window gets the
/// client token in-process; a browser gets a single-use launch code, so the token never
/// appears on its command line (/proc/<pid>/cmdline is world-readable).
fn open_ui(d: &Discovery, window: Option<i64>) -> anyhow::Result<()> {
    gui::open_ui(d, window, || browser_url(d, window))
}

/// URL for opening the UI in a browser: a launch-code URL, or the token URL for an old daemon
/// without launch codes.
fn browser_url(d: &Discovery, window: Option<i64>) -> anyhow::Result<String> {
    match client::launch_code(d).context("getting a launch code from the daemon")? {
        Some(code) => Ok(d.launch_url(&code, window)),
        None => {
            eprintln!(
                "muster: the running daemon (version {}) has no launch codes; opening with the token URL. \
                 Restart it (`muster stop`) to stop passing the token on the browser command line.",
                d.version
            );
            Ok(d.ui_url())
        }
    }
}

// ----- daemon side -----

/// Held for the daemon's lifetime; a second daemon for the same runtime dir fails to lock.
struct DaemonLock(#[allow(dead_code)] std::fs::File);

fn acquire_lock() -> anyhow::Result<DaemonLock> {
    use std::os::fd::AsRawFd;
    let rt = paths::runtime_dir();
    paths::ensure_private_dir(&rt)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::backend_lock_path())?;
    // SAFETY: flock on an fd we own.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        anyhow::bail!("another muster daemon is already running for {}", rt.display());
    }
    Ok(DaemonLock(f))
}

fn load_client_token() -> anyhow::Result<String> {
    let p = paths::client_token_path();
    if let Ok(t) = std::fs::read_to_string(&p) {
        let t = t.trim().to_string();
        if t.len() >= 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(t);
        }
    }
    let t = paths::random_token();
    paths::write_private(&p, t.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    Ok(t)
}

async fn bind() -> anyhow::Result<tokio::net::TcpListener> {
    use tokio::net::TcpListener;
    let saved_path = paths::runtime_dir().join("application-port");
    let saved: Option<u16> = match std::fs::read_to_string(&saved_path) {
        Ok(port) => Some(port.trim().parse().context("invalid saved application port")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let requested = paths::port_override()?;
    if let (Some(saved), Some(requested)) = (saved, requested.filter(|p| *p != 0)) {
        anyhow::ensure!(saved == requested, "application port is pinned to {saved} for surviving terminals; cannot switch to {requested}");
    }
    let port = saved.or(requested).unwrap_or(paths::DEFAULT_PORT);
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) if saved.is_none() && requested.is_none() => {
            tracing::info!("port {port} unavailable ({error}); assigning the initial application port");
            TcpListener::bind(("127.0.0.1", 0)).await?
        }
        Err(error) => return Err(error).with_context(|| format!("binding pinned application port {port}")),
    };
    paths::write_private(&saved_path, listener.local_addr()?.port().to_string().as_bytes())?;
    Ok(listener)
}

fn run_daemon() -> anyhow::Result<()> {
    // First, while this is the only thread: the key leaves the process environment.
    let openrouter_key = openrouter::take_key_from_env();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUSTER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    if let Some(d) = find_daemon() {
        println!("muster daemon already running (pid {}): {}", d.pid, d.base_url());
        return Ok(());
    }
    let _lock = acquire_lock()?;
    paths::ensure_private_dir(&paths::data_dir())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(daemon_main(openrouter_key));
    rt.shutdown_timeout(Duration::from_secs(2));
    paths::remove_discovery_if_ours();
    r
}

async fn daemon_main(openrouter_key: Option<openrouter::ApiKey>) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    // The terminal host and its rollback fence first: an older, monolithic daemon holding
    // daemon.lock must stop this backend before it opens the shared database.
    let launch_exe = std::env::current_exe()?;
    // Snapshot (and so hash) the build that is executing, not whatever the launch path holds
    // by now: a rebuild in between must still show as an update.
    let proc_exe = std::path::Path::new("/proc/self/exe");
    let running = if proc_exe.exists() { proc_exe } else { launch_exe.as_path() };
    let release = executables::prepare_running(&paths::data_dir(), &launch_exe, running)?;
    let terminal_host = terminal::ensure_host(&release.host())?;
    let host_pid = terminal_host.client.hello.pid;
    let listener = bind().await?;
    let port = listener.local_addr()?.port();
    let panel_listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let panel_port = panel_listener.local_addr()?.port();
    let client_token = load_client_token()?;
    let store = store::Store::open(&paths::db_path())?;
    let hook_inbox = hook_inbox::Inbox::open(&hook_inbox::path())?;
    // Never move a directory beneath a surviving process during a backend update.
    if !terminal_host.inventory()?.iter().any(|run| run.has_processes) {
        if let Err(e) = agents::relocate_homes(&store, &paths::data_dir()) {
            tracing::warn!("moving agent homes to {}: {e:#}", paths::agents_dir().display());
        }
    }
    let muster_bin = release.command();
    let hub = hub::Hub::new(
        hub::HubConfig {
            port,
            client_token: client_token.clone(),
            muster_bin,
            terminal_host: Some(terminal_host),
            openrouter_key,
        },
        store,
    )?;

    // Before the server answers any hello, so the first one names the active UI release.
    updates::spawn(&hub, updates::Launch {
        exe: launch_exe,
        app_hash: release.app_hash().to_string(),
        host_pid: Some(host_pid),
    });
    hook_inbox::spawn(&hub, hook_inbox)?;
    hub::spawn_input_attribution(&hub)?;
    hub.panels.port.store(panel_port, std::sync::atomic::Ordering::Relaxed);
    // Finish interrupted archive cleanup and sweep stale files before publishing any panel.
    panels::recover(&hub)?;
    let app = server::router(hub.clone())?;
    // Publish the helper only after application recovery succeeds. Existing
    // terminals keep this path even when the original build directory is removed.
    release.activate()?;
    inject::spawn_tasks(hub.clone());
    panels::spawn(hub.clone());
    detect::spawn(hub.clone());
    notify::spawn(hub.clone());
    // Agents with autostart on (their homes were moved above, before anything could start).
    {
        let hub = hub.clone();
        tokio::spawn(async move { agents::autostart(&hub).await });
    }
    // Schedules: finish runs a previous backend left, then fire on time (src/schedules/).
    schedules::spawn(hub.clone());

    // Event log retention (also applied at startup).
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(3600));
            t.tick().await;
            loop {
                t.tick().await;
                let h = hub.clone();
                let _ = tokio::task::spawn_blocking(move || h.prune_events()).await;
            }
        });
    }

    // Retry application-owned removal intents after process exit and drain, and adopt
    // launches whose acknowledgement was lost.
    // Terminal logs are flushed autonomously by the host.
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(3));
            loop {
                t.tick().await;
                let h = hub.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(error) = h.reconcile_pending() {
                        tracing::warn!("reconciling unconfirmed launches: {error:#}");
                    }
                    if let Err(error) = h.reconcile_retired() {
                        tracing::warn!("completing terminal retirement: {error:#}");
                    }
                }).await;
            }
        });
    }

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;

    // Peer addresses let `/api/exchange` check which local user owns the connecting socket.
    // TCP_NODELAY: terminal echo is a stream of small WebSocket frames. With Nagle's algorithm
    // a frame written while the previous one is unacknowledged waits for the peer's delayed
    // ACK (about 40 ms on Linux), which showed up as echo stalls during fast typing.
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        if let Err(error) = tcp.set_nodelay(true) {
            tracing::debug!("setting TCP_NODELAY: {error}");
        }
    });
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    );

    paths::write_discovery(&Discovery {
        pid: std::process::id(),
        port,
        panel_port: Some(panel_port),
        token: client_token,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .context("writing daemon.json")?;
    tracing::info!(
        "muster daemon listening on http://127.0.0.1:{port} (data: {})",
        paths::data_dir().display()
    );

    let r: anyhow::Result<()> = async {
        tokio::select! {
            r = serve => r.context("server failed"),
            r = axum::serve(panel_listener, server::panel_router(port, panel_port)) => r.context("panel server failed"),
            _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); Ok(()) }
            _ = int.recv() => { tracing::info!("SIGINT: shutting down"); Ok(()) }
            _ = async { loop { hup.recv().await; tracing::info!("ignoring SIGHUP"); } } => Ok(()),
        }
    }
    .await;

    shutdown(&hub).await;
    // Removed only after teardown, so a new daemon doesn't start while sessions are still
    // being killed (it would fail to take the lock anyway).
    paths::remove_discovery_if_ours();
    r
}

async fn shutdown(hub: &Arc<hub::Hub>) {
    hub.shutdown().await;
    tracing::info!("muster daemon stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cmd, clap::Error> {
        Cli::try_parse_from(std::iter::once("muster").chain(args.iter().copied())).map(|c| c.cmd.expect("a command"))
    }

    #[test]
    fn ask_has_no_origin_override() {
        assert!(parse(&["ask", "ada", "hi"]).is_ok());
        // Provenance comes from the calling session, never from a flag.
        assert!(parse(&["ask", "ada", "hi", "--origin", "human"]).is_err());
        assert!(matches!(parse(&["agent", "start", "ada", "--fresh"]), Ok(Cmd::Agent { .. })));
    }

    #[test]
    fn agent_commands_and_kind_flag() {
        assert!(parse(&["agent", "new", "ada", "--kind", "codex", "--autostart", "--codex-sandbox", "read_only"]).is_ok());
        assert!(parse(&["agent", "edit", "ada", "--persona", "x", "--autostart", "off", "--claude-mode", "plan"]).is_ok());
        assert!(parse(&["agent", "edit", "ada", "--autostart", "maybe"]).is_err());
        assert!(parse(&["agent", "edit", "ada", "--claude-mode", "yolo"]).is_err());
        assert!(parse(&["bot", "ls"]).is_err());
        // `muster new --kind`, with `--agent` still accepted.
        for flag in ["--kind", "--agent"] {
            match parse(&["new", ".", flag, "codex"]).unwrap() {
                Cmd::New { kind, .. } => assert_eq!(kind, "codex"),
                _ => panic!("not `new`"),
            }
        }
    }

    fn agent_args(args: &[&str]) -> (Option<PathBuf>, Option<String>, Vec<String>) {
        match parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}")) {
            Cmd::New {
                path,
                prompt,
                agent_args,
                ..
            } => (path, prompt, agent_args),
            _ => panic!("not `new`"),
        }
    }

    #[test]
    fn new_passes_args_after_double_dash() {
        let (path, prompt, args) = agent_args(&[
            "new", ".", "--kind", "codex", "--prompt", "hi", "--", "-m", "gpt-6-sol", "-c",
            "model_reasoning_effort=xhigh",
        ]);
        assert_eq!(path, Some(PathBuf::from(".")));
        assert_eq!(prompt.as_deref(), Some("hi"));
        assert_eq!(args, ["-m", "gpt-6-sol", "-c", "model_reasoning_effort=xhigh"]);
        // Without a path, and flags after `--` are never muster's own.
        let (path, _, args) = agent_args(&["new", "--agent", "shell", "--", "--json", "-c", "echo hi"]);
        assert_eq!(path, None);
        assert_eq!(args, ["--json", "-c", "echo hi"]);
        let (_, _, args) = agent_args(&["new"]);
        assert!(args.is_empty());
        // Extra words without `--` are still an error, not agent arguments.
        assert!(parse(&["new", ".", "-m"]).is_err());
        assert!(parse(&["new", ".", "extra"]).is_err());
    }

    #[test]
    fn new_help_shows_agent_args() {
        let help = Cli::command().find_subcommand_mut("new").unwrap().render_help().to_string();
        assert!(help.contains("[-- <AGENT_ARGS>...]"), "{help}");
        assert!(help.contains("gpt-6-sol"), "{help}");
    }

    #[test]
    fn events_flags() {
        match parse(&["events", "--json", "--session", "12", "--tab", "fix"]).unwrap() {
            Cmd::Events { json, session, tab } => {
                assert!(json);
                assert_eq!(session.as_deref(), Some("12"));
                assert_eq!(tab.as_deref(), Some("fix"));
            }
            _ => panic!("not `events`"),
        }
        assert!(matches!(
            parse(&["events"]).unwrap(),
            Cmd::Events { json: false, session: None, tab: None }
        ));
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    #[test]
    fn reload_refuses_legacy_and_different_host_backends() {
        let host = muster_terminal_protocol::Hello { protocol: 1, instance: "one".into(),
            pid: 1, data_dir: PathBuf::from("/unused"), fence: None };
        assert!(validate_reload_host(&serde_json::json!({"version": "0.1.0"}), &host).is_err());
        assert!(validate_reload_host(&serde_json::json!({"terminal_host": {"protocol": 1, "instance": "other"}}), &host).is_err());
        assert!(validate_reload_host(&serde_json::json!({"terminal_host": host}), &host).is_ok());
        // A host the replacement would refuse (no fence it can verify) stops the reload
        // before the working backend is signalled.
        let lock = std::env::temp_dir().join(format!("muster-reload-fence-{}", rand::random::<u64>()));
        std::fs::write(&lock, b"").unwrap();
        let backend = serde_json::json!({"terminal_host": host});
        let error = check_reload(&backend, &host, &lock).unwrap_err();
        assert!(format!("{error:#}").contains("backend left running"), "{error:#}");
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&lock).unwrap();
        let fenced = muster_terminal_protocol::Hello { fence: Some(muster_terminal_protocol::FenceLock {
            path: lock.clone(), dev: metadata.dev(), ino: metadata.ino() }), ..host.clone() };
        check_reload(&serde_json::json!({"terminal_host": fenced}), &fenced, &lock).unwrap();
        std::fs::remove_file(&lock).unwrap();
    }
}
