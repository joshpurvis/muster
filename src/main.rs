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
mod spend;
mod statusline;
mod notify;
mod openrouter;
mod paths;
mod pidfd;
mod pr;
mod renderer;
mod pty;
mod rpc;
mod schedules;
mod server;
mod store;
mod terminal;
mod updates;
mod usage;
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
    /// Fire a trigger (a schedule made with `--on url:<name>`) now, with DATA (words joined by
    /// spaces, `-` reads stdin) handed to its agent as quoted untrusted data. `--url` prints its
    /// loopback URL and secret instead (`--rotate`: a new secret).
    Trigger {
        name: String,
        data: Vec<String>,
        #[arg(long)]
        url: bool,
        #[arg(long, requires = "url")]
        rotate: bool,
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
        /// Where this build was installed, when it runs from a release snapshot (`restart-all`
        /// starts the checked snapshot): updates are looked for there.
        #[arg(long, hide = true, value_name = "PATH", requires = "foreground")]
        launch: Option<PathBuf>,
        /// Started by a command that holds the lifecycle lock for it (see `start_daemon_from`),
        /// so this start doesn't wait for that lock itself.
        #[arg(long, hide = true, requires = "foreground")]
        lifecycle_locked: bool,
    },
    /// Forward an agent hook callback to the daemon (used inside agent CLIs).
    Hook {
        agent: String,
        event: String,
        payload: Option<String>,
    },
    /// Claude sessions' usage reporter: reads the status-line JSON on stdin and reports the usage
    /// windows to the daemon, printing nothing (src/statusline.rs).
    #[command(hide = true)]
    Statusline {
        agent: String,
    },
    /// Show whether the daemon is running.
    Status,
    /// Stop the daemon (running sessions are terminated).
    Stop,
    /// Restart only the application backend; host-owned terminal processes continue.
    Reload,
    /// Stop Muster, ending every session, then start it again from this build, as `muster stop`
    /// then `muster` would. A window's "Restart Muster…" runs it (`daemon.restart`).
    #[command(hide = true)]
    RestartAll {
        /// Report the checks' verdict on this descriptor and stop nothing until the backend
        /// answers "go" there.
        #[arg(long, value_name = "FD")]
        report_fd: Option<i32>,
        /// The path this build was installed at (default: this binary's).
        #[arg(long, value_name = "PATH")]
        launch: Option<PathBuf>,
        /// Refuse unless this build is the one with this SHA-256.
        #[arg(long, value_name = "SHA256")]
        app: Option<String>,
        /// Refuse unless the terminal host beside the launch path has this SHA-256.
        #[arg(long, value_name = "SHA256")]
        host: Option<String>,
    },
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
    /// Each account's last reported 5-hour and weekly usage, and where a new task has the most
    /// known room. Unknown or stale usage is shown as such, never as free.
    Usage {
        #[arg(long)]
        json: bool,
    },
    /// Declare that PROFILE signs in to the same Claude account as OTHER, so they share one set
    /// of limits. Without OTHER, PROFILE is its own account again.
    SameAccount { profile: String, other: Option<String> },
    /// You signed in to a different account in PROFILE (or "default"): forget its usage and
    /// its same-account declaration.
    LoginChanged { profile: String },
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
    if args.get(1).map(String::as_str) == Some("statusline") {
        let code = statusline::run(args.get(2).map(String::as_str).unwrap_or(""));
        return ExitCode::from(code.clamp(0, 255) as u8);
    }

    let cli = Cli::parse();
    // Printing commands die quietly on a closed pipe (`muster ls | head`) instead of panicking.
    // Not the daemon or a window: they write to sockets and must keep ignoring SIGPIPE (a Rust
    // binary starts with it ignored, so processes they spawn are unaffected either way).
    // Nor `restart-all`: it writes to the backend's report socket, which may close first.
    if !matches!(cli.cmd, None | Some(Cmd::Daemon { foreground: true, .. }) | Some(Cmd::NativeWindow { .. })
        | Some(Cmd::RestartAll { .. })) {
        // SAFETY: setting a signal disposition before any threads exist.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let r = match cli.cmd {
        None => ensure_daemon().and_then(|d| open_ui(&d, None)),
        Some(Cmd::Daemon { foreground: true, launch, lifecycle_locked }) => run_daemon(launch, lifecycle_locked),
        Some(Cmd::Daemon { foreground: false, .. }) => start_daemon_detached().map(|d| println!("{}", d.ui_url())),
        Some(Cmd::Hook { .. } | Cmd::Statusline { .. }) => unreachable!("handled above"),
        Some(Cmd::Status) => status(),
        Some(Cmd::Stop) => stop(),
        Some(Cmd::Reload) => reload(),
        Some(Cmd::RestartAll { report_fd, launch, app, host }) => restart_all(RestartOpts { report_fd, launch, app, host }),
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
        Cmd::Trigger { name, data, url, rotate } => cli::trigger(d, &name, &data, url, rotate),
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
            ProfileCmd::Usage { json } => cli::profile_usage(d, json),
            ProfileCmd::SameAccount { profile, other } => cli::profile_same_account(d, &profile, other.as_deref()),
            ProfileCmd::LoginChanged { profile } => cli::profile_login_changed(d, &profile),
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
        return is_our_backend(d.pid);
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
/// and wait until it answers. While a restart, reload or stop holds the lifecycle lock, it
/// waits for that to finish first (a restart's own new backend is then the one found). A
/// restart that didn't finish is reported here (see [`take_restart_failure`]).
fn start_daemon_detached() -> anyhow::Result<Discovery> {
    let lock = wait_lifecycle_lock()?;
    let exe = std::env::current_exe().context("locating the muster binary")?;
    let failure = take_restart_failure();
    let (d, _) = start_daemon_from(&exe, None, &lock)?;
    if let Some(failure) = failure {
        eprintln!("muster: note: {failure}");
    }
    Ok(d)
}

/// [`start_daemon_detached`] from `exe` (`launch`: see `daemon --launch`), for a caller that
/// holds the lifecycle lock (`_held`). Also says whether the backend that answers is the one
/// this started (false: one was already running, or another won the start).
fn start_daemon_from(exe: &std::path::Path, launch: Option<&std::path::Path>, _held: &LifecycleLock)
    -> anyhow::Result<(Discovery, bool)> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    if let Some(d) = find_daemon() {
        return Ok((d, false));
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
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "--foreground", "--lifecycle-locked"]);
    if let Some(launch) = launch {
        cmd.arg("--launch").arg(launch);
    }
    cmd
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
            let ours = d.pid == child.id();
            return Ok((d, ours));
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
    if let Some(failure) = restart_failure() {
        println!("{failure}");
    }
    #[cfg(all(feature = "gui", target_os = "linux"))]
    if std::env::var_os("MUSTER_BROWSER").is_none() {
        println!("next native window: {}", gui::linux::status_line());
    }
    Ok(())
}

/// How long `stop` waits for the backend to exit.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long `restart-all` waits for the old backend to exit: it goes on (stopping the host,
/// starting Muster) only once the backend has really gone, so it waits long.
const RESTART_STOP_WAIT: Duration = Duration::from_secs(120);
/// How long a terminal host may take to stop.
const HOST_STOP_WAIT: Duration = Duration::from_secs(30);

/// What a `stop`, `reload` or `restart-all` is refused with while another holds the lifecycle
/// lock.
const LIFECYCLE_BUSY: &str = "another Muster restart, reload or stop is under way (a window's Restart Muster, say)";

/// `<runtime>/lifecycle.lock`, held by `restart-all`, `stop` and `reload` while they stop or
/// replace Muster, so one started from another shell refuses instead of racing it (a reload
/// starting a backend on a host the restart is about to stop). Close-on-exec, so a daemon
/// started meanwhile never holds it.
struct LifecycleLock(#[allow(dead_code)] std::fs::File);

/// The lifecycle lock, or None while another command holds it.
fn try_lifecycle_lock() -> anyhow::Result<Option<LifecycleLock>> {
    use std::os::fd::AsRawFd;
    let rt = paths::runtime_dir();
    paths::ensure_private_dir(&rt)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(rt.join("lifecycle.lock"))?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    Ok(Some(LifecycleLock(f)))
}

fn lifecycle_lock(what: &str) -> anyhow::Result<LifecycleLock> {
    try_lifecycle_lock()?.with_context(||
        format!("{LIFECYCLE_BUSY}, so {what} was refused and nothing was changed; try again once it has finished"))
}

/// How long an ordinary start waits for a restart, reload or stop under way: longer than a
/// restart can take (its checks, the old backend's 120 s, the host's 30 s, the new start).
const LIFECYCLE_WAIT: Duration = Duration::from_secs(240);

/// The lifecycle lock for an ordinary start (`muster`, `muster daemon`, a command that starts
/// the daemon): while a restart, reload or stop holds it, wait for that to finish instead of
/// starting a backend in the middle of it (on a terminal host about to be stopped, or beside
/// the one a restart starts).
fn wait_lifecycle_lock() -> anyhow::Result<LifecycleLock> {
    let end = Instant::now() + LIFECYCLE_WAIT;
    let mut said = false;
    loop {
        if let Some(lock) = try_lifecycle_lock()? {
            return Ok(lock);
        }
        if !said {
            eprintln!("muster: a Muster restart, reload or stop is under way; waiting for it to finish…");
            said = true;
        }
        anyhow::ensure!(Instant::now() < end,
            "{LIFECYCLE_BUSY} and hasn't finished within {} s, so Muster wasn't started; see {}",
            LIFECYCLE_WAIT.as_secs(), paths::log_path().display());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Where a restart that went ahead but didn't finish says why (`<runtime>/restart-failed`):
/// the windows waiting for it can't hear it, so the next `muster` (and `muster status`) do.
fn restart_failure_path() -> PathBuf {
    paths::runtime_dir().join("restart-failed")
}

fn record_restart_failure(error: &anyhow::Error) {
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let why = format!("{error:#}").replace('\n', " ");
    let _ = paths::write_private(&restart_failure_path(), format!("{at}\n{why}\n").as_bytes());
}

/// The last restart's failure, as a sentence, if one is recorded.
fn restart_failure() -> Option<String> {
    let raw = std::fs::read_to_string(restart_failure_path()).ok()?;
    let (at, why) = raw.split_once('\n')?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let minutes = now.saturating_sub(at.trim().parse().unwrap_or(now)) / 60;
    let ago = match minutes {
        0 => "less than a minute ago".to_string(),
        1 => "1 minute ago".to_string(),
        m if m < 120 => format!("{m} minutes ago"),
        m => format!("{} hours ago", m / 60),
    };
    Some(format!("the last Restart Muster ({ago}) didn't finish: {}", why.trim()))
}

/// [`restart_failure`], removed once told.
fn take_restart_failure() -> Option<String> {
    let failure = restart_failure();
    let _ = std::fs::remove_file(restart_failure_path());
    failure
}

fn stop() -> anyhow::Result<()> {
    let _lock = lifecycle_lock("`muster stop`")?;
    stop_all(STOP_WAIT)
}

/// Stop the backend (waiting up to `wait`), then the terminal host, ending every session.
fn stop_all(wait: Duration) -> anyhow::Result<()> {
    stop_backend_within(None, wait)?;
    if let Some(host) = running_host(&HostPaths::current())? {
        host.stop()?;
        println!("muster terminal host stopped");
    }
    Ok(())
}

/// Where the terminal host of this runtime and data directory answers and describes itself.
#[derive(Clone)]
struct HostPaths {
    socket: PathBuf,
    /// `terminal.json`: the host's `Hello`, written when it starts.
    discovery: PathBuf,
    /// The host's data directory (`<data>/terminal`).
    data: PathBuf,
}

impl HostPaths {
    fn current() -> Self {
        let rt = paths::runtime_dir();
        Self { socket: rt.join("terminal.sock"), discovery: rt.join("terminal.json"), data: paths::data_dir().join("terminal") }
    }
}

/// The running terminal host of [`HostPaths`], checked to be this data directory's: the one
/// `stop` shuts down.
struct RunningHost {
    instance: String,
    pid: u32,
    how: HostStop,
    paths: HostPaths,
}

enum HostStop {
    /// It speaks this build's protocol: asked to shut down.
    Protocol(muster_terminal_protocol::client::Client),
    /// Another protocol version (a build that raised `VERSION`, stopping the host an older one
    /// started): sent SIGTERM, which it handles as a shutdown. Only after its identity is
    /// confirmed without the protocol (see [`identify_host`]).
    Signal,
}

/// The host answering at `paths.socket`, if one does (None: no socket, or nothing listening).
fn running_host(paths: &HostPaths) -> anyhow::Result<Option<RunningHost>> {
    let data = std::fs::canonicalize(&paths.data);
    match muster_terminal_protocol::client::Client::connect(&paths.socket) {
        Ok(host) => {
            anyhow::ensure!(data.is_ok_and(|d| host.hello.data_dir == d), "terminal host belongs to another data directory");
            let (instance, pid) = (host.hello.instance.clone(), host.hello.pid);
            Ok(Some(RunningHost { instance, pid, how: HostStop::Protocol(host), paths: paths.clone() }))
        }
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e|
            matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => Ok(None),
        // Something answers but not in this protocol (or not at all): find out who, without it.
        Err(error) => match identify_host(paths) {
            Ok((instance, pid)) => Ok(Some(RunningHost { instance, pid, how: HostStop::Signal, paths: paths.clone() })),
            Err(why) => Err(error.context(format!("the terminal host can't be identified ({why:#})"))),
        },
    }
}

/// Who the host at `paths.socket` is, without its protocol: the process listening on the socket
/// (by its peer credentials, this user's) must be the pid its `terminal.json` names, for this
/// data directory. Read as plain JSON, so any version's file does.
fn identify_host(paths: &HostPaths) -> anyhow::Result<(String, u32)> {
    let raw = std::fs::read(&paths.discovery).with_context(|| format!("reading {}", paths.discovery.display()))?;
    let hello: serde_json::Value = serde_json::from_slice(&raw).context("terminal.json is not JSON")?;
    let pid = hello["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).context("terminal.json names no pid")?;
    let instance = hello["instance"].as_str().unwrap_or_default().to_string();
    let data = std::fs::canonicalize(&paths.data)?;
    anyhow::ensure!(hello["data_dir"].as_str().map(PathBuf::from) == Some(data), "terminal host belongs to another data directory");
    let stream = std::os::unix::net::UnixStream::connect(&paths.socket)?;
    let (peer, uid) = peer_cred(&stream)?;
    // SAFETY: geteuid has no preconditions.
    anyhow::ensure!(uid == unsafe { libc::geteuid() }, "the terminal host socket belongs to another user");
    anyhow::ensure!(peer == pid, "the process on the terminal host socket (pid {peer}) isn't the one terminal.json names (pid {pid})");
    Ok((instance, pid))
}

/// The pid and uid of the process at the other end of `stream`.
#[cfg(target_os = "linux")]
fn peer_cred(stream: &std::os::unix::net::UnixStream) -> anyhow::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    let r = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
    };
    anyhow::ensure!(r == 0, "reading the terminal host's credentials: {}", std::io::Error::last_os_error());
    Ok((cred.pid as u32, cred.uid))
}

#[cfg(not(target_os = "linux"))]
fn peer_cred(_: &std::os::unix::net::UnixStream) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("a terminal host of another protocol version can only be identified on Linux")
}

impl RunningHost {
    /// Shut it down (every session ends) and wait until it has gone.
    fn stop(self) -> anyhow::Result<()> {
        // Without the protocol it is signalled through a handle opened before its identity is
        // confirmed again (in `stop_with`), so the signal reaches the process just confirmed to
        // own the socket, never one that took its pid since.
        let process = match self.how {
            HostStop::Signal => pidfd::Process::open(self.pid).ok(),
            HostStop::Protocol(_) => None,
        };
        self.stop_with(
            |_| {
                if let Some(process) = &process {
                    let _ = process.signal(libc::SIGTERM);
                }
            },
            |_| process.as_ref().is_some_and(|p| !p.exited()),
        )
    }

    /// [`RunningHost::stop`] with how to signal it and how to tell it still runs.
    fn stop_with(self, signal: impl FnOnce(u32), alive: impl Fn(u32) -> bool) -> anyhow::Result<()> {
        match &self.how {
            HostStop::Protocol(host) => {
                host.call(muster_terminal_protocol::Command::Shutdown)?;
            }
            HostStop::Signal => {
                // Nothing listens any more: it has gone by itself.
                if let Err(e) = std::os::unix::net::UnixStream::connect(&self.paths.socket)
                    && matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
                {
                    return Ok(());
                }
                // Confirmed again right before the signal, so a pid freed since isn't hit.
                let (instance, pid) = identify_host(&self.paths)?;
                anyhow::ensure!(pid == self.pid && instance == self.instance, "the terminal host changed while it was being stopped");
                signal(pid);
            }
        }
        let end = Instant::now() + HOST_STOP_WAIT;
        // Asked through the protocol, it is gone once its socket is; signalled, once it exited.
        while match self.how { HostStop::Protocol(_) => self.paths.socket.exists(), HostStop::Signal => alive(self.pid) } {
            anyhow::ensure!(Instant::now() < end, "terminal host did not stop within {}s", HOST_STOP_WAIT.as_secs());
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
}

/// `muster restart-all`'s options (see `Cmd::RestartAll`).
struct RestartOpts {
    report_fd: Option<i32>,
    launch: Option<PathBuf>,
    app: Option<String>,
    host: Option<String>,
}

/// `muster restart-all`: `stop`, then start Muster again as `muster` does, without opening a
/// window (the open ones reconnect). Everything that can refuse is checked before anything
/// stops, including the release the new daemon will run: this build and the terminal host
/// beside its launch path are snapshotted and checked first (`executables::prepare_running`,
/// as a daemon start does), both copies are run once (the host on scratch directories, answering
/// this build's protocol), and the daemon is then started from that snapshot, so a rebuild
/// meanwhile can neither fail nor change the start. With `report_fd` (the backend's
/// `daemon.restart`), the verdict goes there ("ready" or "refused: <why>") and nothing stops
/// until the backend answers "go", so windows hear of the restart first. Holds the lifecycle
/// lock throughout (ordinary starts wait for it). Once anything has stopped, it goes on only
/// when each step has really finished (the backend exited, the host gone); if one doesn't, it
/// stops there and says so, and the windows' wait ends in "run muster". A failure after "go"
/// is also recorded for the next `muster` to tell ([`record_restart_failure`]).
fn restart_all(opts: RestartOpts) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    use std::os::fd::FromRawFd;
    // SAFETY: the backend hands this process its end of a socket pair at this descriptor, which
    // nothing else here uses.
    let report = opts.report_fd.map(|fd| unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) });
    let checked = lifecycle_lock("the restart").and_then(|lock| restart_checks(&opts).map(|c| (lock, c)));
    let (lock, plan) = match checked {
        Ok(c) => c,
        Err(error) => {
            if let Some(mut r) = report {
                let _ = writeln!(r, "refused: {}", format!("{error:#}").replace('\n', " "));
            }
            return Err(error);
        }
    };
    if let Some(mut r) = report {
        r.write_all(b"ready\n").context("reporting to the backend; nothing was stopped")?;
        r.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        let _ = std::io::BufReader::new(&r).read_line(&mut line);
        anyhow::ensure!(line.trim_end() == "go", "the backend did not confirm the restart; nothing was stopped");
    }
    let _ = std::fs::remove_file(restart_failure_path());
    let restarted = restart_go(&lock, plan);
    if let Err(error) = &restarted {
        record_restart_failure(error);
    }
    restarted
}

/// What `restart-all` stops and starts, once checked ([`restart_checks`]).
struct RestartPlan {
    backend: Option<Backend>,
    host: Option<RunningHost>,
    release: executables::Release,
    launch: PathBuf,
}

/// `restart-all` past the point of no return: stop the backend, then the host, then start the
/// checked release and confirm it is what answers.
fn restart_go(lock: &LifecycleLock, plan: RestartPlan) -> anyhow::Result<()> {
    println!("muster: restarting; every session ends");
    stop_backend_within(plan.backend.as_ref(), RESTART_STOP_WAIT).context(
        "the old backend did not stop, so the terminal host was left running (sessions may still run) and Muster \
         was not started again; once it has stopped, run `muster stop`, then `muster`")?;
    let old_instance = plan.host.as_ref().map(|h| h.instance.clone());
    if let Some(host) = plan.host {
        host.stop().context("the terminal host did not stop, so Muster was not started again; run `muster stop`, then `muster`")?;
        println!("muster terminal host stopped");
    }
    let (d, started) = start_daemon_from(&plan.release.app(), Some(&plan.launch), lock)
        .context("Muster was stopped but did not start again; start it with `muster`")?;
    anyhow::ensure!(started && plan.backend.as_ref().is_none_or(|o| o.d.pid != d.pid),
        "Muster was stopped, but the backend that answers (pid {}) isn't the one this restart started; \
         run `muster stop`, then `muster`", d.pid);
    let hello = client::rpc(&d, "hello", serde_json::json!({})).context("the new backend does not answer")?;
    check_restarted(&hello, &plan.release, old_instance.as_deref())
        .map_err(|e| anyhow::anyhow!("{e:#}; run `muster stop`, then `muster`"))?;
    println!("muster restarted (pid {}); every session ended, open windows reconnect", d.pid);
    Ok(())
}

/// Is the backend that answers (`hello`) the release this restart checked and started, on a new
/// terminal host started from that release's host?
fn check_restarted(hello: &serde_json::Value, release: &executables::Release, old_instance: Option<&str>)
    -> anyhow::Result<()> {
    let build = hello["build"].as_str().unwrap_or_default();
    anyhow::ensure!(!build.is_empty() && release.app_hash().starts_with(build),
        "the new backend runs another build ({}) than the one this restart checked", if build.is_empty() { "unknown" } else { build });
    let instance = hello["terminal_host"]["instance"].as_str().unwrap_or_default();
    anyhow::ensure!(!instance.is_empty() && old_instance != Some(instance), "the new backend has no new terminal host");
    // By content, where /proc shows it (it keeps the executable even if its file is replaced).
    if let Some(pid) = hello["terminal_host"]["pid"].as_u64()
        && let Ok(hash) = executables::hash_file(std::path::Path::new(&format!("/proc/{pid}/exe")))
    {
        anyhow::ensure!(hash == release.host_hash(), "the new terminal host (pid {pid}) isn't the one this restart checked");
    }
    Ok(())
}

/// The least free space the data directory's file system must have left once the release is
/// prepared, for the new backend's database, logs and state.
const RESTART_MIN_FREE: u64 = 64 * 1024 * 1024;

/// What `restart-all` checks before anything stops: this command can signal the backend (a
/// handle on its process is taken now, see [`open_backend`]); the terminal host is one it can
/// stop (in this protocol or not); and the release the new daemon will run is prepared, is the
/// build the backend offered (`opts.app`, `opts.host`), and runs: its backend answers
/// `--version` as this build, its host starts on scratch directories and answers this build's
/// protocol ([`terminal::probe_host`]). There must also be room left to start.
fn restart_checks(opts: &RestartOpts) -> anyhow::Result<RestartPlan> {
    let d = paths::read_discovery().filter(backend_alive);
    if let Some(d) = &d {
        anyhow::ensure!(!backend_hidden(d),
            "Muster's backend (pid {}) runs where this command can't signal it (a sandboxed command); restart it from outside", d.pid);
    }
    let backend = match &d {
        Some(d) => Some(open_backend(d)
            .with_context(|| format!("Muster's backend (pid {}) stopped while the restart was being checked; nothing was stopped", d.pid))?),
        None => None,
    };
    let host = running_host(&HostPaths::current()).context("the terminal host can't be stopped from here; nothing was stopped")?;
    let launch = match &opts.launch {
        Some(launch) => launch.clone(),
        None => std::env::current_exe().context("locating the muster binary")?,
    };
    // What executes is this build; the host comes from beside the launch path, as at a start.
    let proc_exe = std::path::Path::new("/proc/self/exe");
    let running = if proc_exe.exists() { proc_exe.to_path_buf() } else { launch.clone() };
    let release = executables::prepare_running(&paths::data_dir(), &launch, &running)
        .context("the new Muster build can't be started (nothing was stopped)")?;
    check_restart_release(&release, opts.app.as_deref(), opts.host.as_deref(), &launch)?;
    check_release_runs(&release).map_err(|e| anyhow::anyhow!("{e:#}; nothing was stopped"))?;
    let free = free_space(&paths::data_dir()).unwrap_or(u64::MAX);
    anyhow::ensure!(free >= RESTART_MIN_FREE,
        "only {} MB are free where Muster keeps its data ({}), too little to start it again; nothing was stopped. Free some space, then try again",
        free / (1024 * 1024), paths::data_dir().display());
    Ok(RestartPlan { backend, host, release, launch })
}

/// Run the prepared release once, as the new daemon will: its backend copy answers
/// `--version` as this build (the copy executes where it lies: no `noexec` mount, nothing
/// missing), and its terminal host copy starts and speaks this build's protocol.
fn check_release_runs(release: &executables::Release) -> anyhow::Result<()> {
    let app = release.app();
    // The OpenRouter key this command was handed is for the new daemon only.
    let mut child = Command::new(&app).arg("--version").env_remove(openrouter::ENV_KEY)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().with_context(|| format!("the new Muster build {} can't be run", app.display()))?;
    let end = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the new Muster build {} did not answer `--version` within 10 s", app.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let want = format!("muster {}", env!("CARGO_PKG_VERSION"));
    anyhow::ensure!(output.status.success() && said == want,
        "the new Muster build {} doesn't run as this build (it says {said:?})", app.display());
    // Beside the real host's socket, unless a socket there would be too long a path for one.
    let name = format!(".restart-check-{}", &paths::random_token()[..12]);
    let mut scratch = paths::runtime_dir().join(&name);
    if scratch.join("run/terminal.sock").as_os_str().len() > 100 {
        scratch = std::env::temp_dir().join(name);
    }
    terminal::probe_host(&release.host(), &scratch)
}

/// Bytes free to this user on the file system holding `path`.
fn free_space(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct; the path is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Is the prepared `release` the build the backend offered?
fn check_restart_release(release: &executables::Release, app: Option<&str>, host: Option<&str>, launch: &std::path::Path)
    -> anyhow::Result<()> {
    anyhow::ensure!(app.is_none_or(|a| a == release.app_hash()),
        "the Muster build at {} changed after the restart was asked for; nothing was stopped", launch.display());
    anyhow::ensure!(host.is_none_or(|h| h == release.host_hash()),
        "the terminal host beside {} changed after the restart was asked for; nothing was stopped", launch.display());
    Ok(())
}

/// Replacing a legacy monolithic daemon would kill its PTYs. Require a live,
/// compatible host and the same backend identity immediately before signalling.
fn reload() -> anyhow::Result<()> {
    // Released before a handoff: the backend's own `reload` takes it then.
    let mut lock = Some(lifecycle_lock("`muster reload`")?);
    let discovery = paths::read_discovery().filter(backend_alive);
    // A handle on its process now, so the one checked is the one stopped (see `open_backend`).
    let backend = discovery.as_ref().and_then(open_backend);
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
            drop(lock.take());
            reload_handoff(d)?;
            println!("muster backend restarted by the backend itself, keeping its environment and OpenRouter key; terminals preserved");
            return Ok(());
        }
        // The replacement gets this command's environment. When that has no OpenRouter key and
        // the backend has one (only in its memory), the backend restarts itself instead, so the
        // key carries over. OPENROUTER_API_KEY set here, even empty, replaces (or clears) it.
        if std::env::var_os(openrouter::ENV_KEY).is_none() && hello["openrouter"]["key_present"] == true {
            drop(lock.take());
            match reload_handoff(d) {
                Ok(()) => {
                    println!("muster backend restarted by the backend itself, keeping its environment and OpenRouter key; terminals preserved");
                    return Ok(());
                }
                Err(error) if error.to_string().contains(crate::updates::HANDOFF_REFUSED) => {
                    eprintln!("note: the backend can't restart itself here ({error:#}); reloading from this shell instead");
                    lock = Some(lifecycle_lock("`muster reload`")?);
                }
                Err(error) => return Err(error),
            }
        }
        warn_openrouter_key_lost(&hello);
        // Gone meanwhile (nothing to stop): the start below finds whatever runs now.
        if let Some(backend) = &backend {
            stop_backend(Some(backend))?;
        }
    }
    let held = lock.as_ref().context("the lifecycle lock is held across the reload")?;
    start_daemon_from(&std::env::current_exe().context("locating the muster binary")?, None, held)?;
    drop(lock);
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
    // Refused, or another restart or shutdown is under way (nothing to wait for here).
    if let Err(error) = &sent
        && [crate::updates::HANDOFF_REFUSED, crate::updates::BUSY].iter().any(|m| error.to_string().contains(m))
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

fn stop_backend(expected: Option<&Backend>) -> anyhow::Result<()> {
    stop_backend_within(expected, STOP_WAIT)
}

/// A running backend: its discovery file, and a handle on its process ([`open_backend`]).
struct Backend {
    d: Discovery,
    process: pidfd::Process,
}

/// The backend `d` names, if it runs. The handle on its process is opened first, then the
/// process is confirmed to be this runtime directory's backend ([`is_our_backend`]), so a pid
/// that was freed and reused (by anything, or by another runtime directory's daemon) is never
/// taken for it, and everything later signalled or waited for through the handle is that one.
fn open_backend(d: &Discovery) -> Option<Backend> {
    let process = pidfd::Process::open(d.pid).ok()?;
    is_our_backend(d.pid).then(|| Backend { d: d.clone(), process })
}

/// Stop the backend: `expected` (the one a reload or restart checked), else the one the discovery
/// file names. The expected one is stopped even when its discovery file has gone meanwhile (it is
/// removed only at the end of a backend's teardown, or by something else): reading the file again
/// would say "not running" while it still runs, and the next backend would fail to start beside it.
/// Signalled and waited for through its process handle.
fn stop_backend_within(expected: Option<&Backend>, wait: Duration) -> anyhow::Result<()> {
    let current = paths::read_discovery();
    if let Some(d) = current.as_ref().filter(|d| backend_hidden(d)) {
        anyhow::bail!("Muster's backend (pid {}) runs where this command can't signal it (a sandboxed command); stop it from outside", d.pid);
    }
    let opened;
    let backend = match expected {
        Some(expected) => {
            if let Some(d) = &current {
                let e = &expected.d;
                anyhow::ensure!(d.pid == e.pid && d.token == e.token && d.port == e.port,
                    "backend changed while preparing the reload or restart; nothing was stopped");
            }
            expected
        }
        None => {
            let Some(d) = current else {
                println!("muster daemon: not running");
                return Ok(());
            };
            match open_backend(&d) {
                Some(backend) => {
                    opened = backend;
                    &opened
                }
                // Guard against a stale daemon.json whose pid now belongs to another process.
                None if paths::pid_alive(d.pid) => {
                    let _ = std::fs::remove_file(paths::discovery_path());
                    println!("muster daemon: not running (removed stale daemon.json)");
                    return Ok(());
                }
                None => {
                    println!("muster daemon: not running");
                    return Ok(());
                }
            }
        }
    };
    if backend.process.exited() {
        println!("muster daemon: not running");
        return Ok(());
    }
    backend.process.signal(libc::SIGTERM).with_context(|| format!("signalling the backend (pid {})", backend.d.pid))?;
    let deadline = Instant::now() + wait;
    while !backend.process.exited() {
        if Instant::now() > deadline {
            anyhow::bail!("daemon (pid {}) did not stop within {}s", backend.d.pid, wait.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("muster daemon stopped");
    Ok(())
}

/// Is `pid` this runtime directory's backend: a `muster daemon` ([`is_muster_process`]) that
/// holds its `backend.lock` open (or `daemon.lock`, an older monolithic daemon's)? A daemon of
/// another runtime directory holds other files. Where its open files can't be read (no procfs,
/// or not allowed), the cmdline check's answer.
fn is_our_backend(pid: u32) -> bool {
    if !is_muster_process(pid) {
        return false;
    }
    let held = [paths::backend_lock_path(), paths::lock_path()].map(|lock| pidfd::holds_open(pid, &lock));
    held.contains(&Some(true)) || held.iter().all(Option::is_none)
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

fn run_daemon(launch: Option<PathBuf>, lifecycle_locked: bool) -> anyhow::Result<()> {
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
    // Run by hand (not by a command holding the lifecycle lock for it): wait out a restart,
    // reload or stop under way first, as `muster` does.
    if !lifecycle_locked {
        drop(wait_lifecycle_lock()?);
    }
    if let Some(d) = find_daemon() {
        println!("muster daemon already running (pid {}): {}", d.pid, d.base_url());
        return Ok(());
    }
    let _lock = acquire_lock()?;
    paths::ensure_private_dir(&paths::data_dir())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(daemon_main(openrouter_key, launch));
    rt.shutdown_timeout(Duration::from_secs(2));
    paths::remove_discovery_if_ours();
    r
}

async fn daemon_main(openrouter_key: Option<openrouter::ApiKey>, launch: Option<PathBuf>) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    // The terminal host and its rollback fence first: an older, monolithic daemon holding
    // daemon.lock must stop this backend before it opens the shared database.
    let launch_exe = std::env::current_exe()?;
    // Snapshot (and so hash) the build that is executing, not whatever the launch path holds
    // by now: a rebuild in between must still show as an update.
    let proc_exe = std::path::Path::new("/proc/self/exe");
    let running = if proc_exe.exists() { proc_exe } else { launch_exe.as_path() };
    // Started from a release snapshot (`restart-all`, `--launch`), its host is the snapshot's
    // copy beside it, checked before anything stopped; updates are looked for at `launch`.
    let release = executables::prepare_running(&paths::data_dir(), &launch_exe, running)?;
    let launch_exe = launch.unwrap_or(launch_exe);
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
    // Schedules: finish runs a previous backend left, then fire on time (src/schedules/); the
    // trigger watchers (git refs, GitHub through gh) and the OpenRouter spend sampler.
    schedules::spawn(hub.clone());
    spend::spawn(hub.clone());

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
mod restart_tests {
    use super::*;
    use muster_terminal_protocol::wire;

    /// A terminal host of an older protocol version at `dir`: it answers every request with the
    /// error such a host gives a newer client, and its `terminal.json` names this test process.
    fn old_host(dir: &std::path::Path, pid: u32) -> HostPaths {
        let data = dir.join("terminal");
        std::fs::create_dir_all(&data).unwrap();
        let paths = HostPaths { socket: dir.join("terminal.sock"), discovery: dir.join("terminal.json"), data: data.clone() };
        let hello = serde_json::json!({"protocol": 0, "instance": "old-host", "pid": pid,
            "data_dir": std::fs::canonicalize(&data).unwrap()});
        std::fs::write(&paths.discovery, serde_json::to_vec(&hello).unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let Ok(frame) = wire::read(&mut stream) else { continue };
                let request: muster_terminal_protocol::Request = frame.decode().unwrap();
                assert_ne!(request.version, 0, "a newer client");
                let error = muster_terminal_protocol::Response::Error {
                    code: "protocol".into(), message: "unsupported terminal protocol".into() };
                let _ = wire::write(&mut stream, &wire::Frame::json(&error).unwrap());
            }
        });
        paths
    }

    fn temp() -> PathBuf {
        // Short: the host checks put a socket in here.
        let dir = std::env::temp_dir().join(format!("muster-restart-host-{}", &paths::random_token()[..12]));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn a_host_of_an_older_protocol_is_stopped_by_signal_after_its_identity_is_confirmed() {
        let dir = temp();
        let paths = old_host(&dir, std::process::id());
        let host = running_host(&paths).unwrap().expect("a host answers");
        assert!(matches!(host.how, HostStop::Signal), "not through the protocol it doesn't speak");
        assert_eq!((host.instance.as_str(), host.pid), ("old-host", std::process::id()));
        let socket = paths.socket.clone();
        let signalled = std::cell::Cell::new(None);
        host.stop_with(|pid| {
            signalled.set(Some(pid));
            // The host exits on SIGTERM: its socket goes.
            std::fs::remove_file(&socket).unwrap();
        }, |_| signalled.get().is_none()).unwrap();
        assert_eq!(signalled.get(), Some(std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_host_whose_identity_doesnt_match_is_never_signalled() {
        // terminal.json names another process than the one on the socket.
        let dir = temp();
        let paths = old_host(&dir, std::process::id() + 1);
        let e = running_host(&paths).err().expect("refused").to_string();
        assert!(e.contains("can't be identified"), "{e}");
        let e = format!("{:#}", running_host(&paths).err().unwrap());
        assert!(e.contains("isn't the one terminal.json names"), "{e}");
        // Another data directory.
        let other = temp();
        let mut wrong = old_host(&other, std::process::id());
        wrong.data = dir.join("terminal");
        let e = format!("{:#}", running_host(&wrong).err().unwrap());
        assert!(e.contains("another data directory"), "{e}");
        // No host at all: nothing to stop.
        let none = temp();
        let paths = HostPaths { socket: none.join("terminal.sock"), discovery: none.join("terminal.json"), data: none.clone() };
        assert!(running_host(&paths).unwrap().is_none());
        for d in [dir, other, none] {
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn the_release_must_be_the_offered_build() {
        let dir = temp();
        let bin = dir.join("build");
        std::fs::create_dir(&bin).unwrap();
        for (name, body) in [("muster", "app"), (executables::HOST, "host")] {
            std::fs::write(bin.join(name), body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let launch = bin.join("muster");
        let release = executables::prepare_running(&dir.join("data"), &launch, &launch).unwrap();
        let (app, host) = (release.app_hash().to_string(), release.host_hash().to_string());
        check_restart_release(&release, Some(&app), Some(&host), &launch).unwrap();
        check_restart_release(&release, None, None, &launch).unwrap();
        let e = check_restart_release(&release, Some("other"), Some(&host), &launch).unwrap_err().to_string();
        assert!(e.contains("Muster build at") && e.contains("nothing was stopped"), "{e}");
        let e = check_restart_release(&release, Some(&app), Some("other"), &launch).unwrap_err().to_string();
        assert!(e.contains("terminal host beside"), "{e}");
        // The new daemon starts from the release's own read-only copy, not the build output.
        assert_eq!(std::fs::read(release.app()).unwrap(), b"app");
        std::fs::write(&launch, b"rebuilt").unwrap();
        assert_eq!(std::fs::read(release.app()).unwrap(), b"app");
        // A host the start would refuse (not executable) fails the preparation, before anything stops.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join(executables::HOST), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(bin.join(executables::HOST), b"new host").unwrap();
        assert!(executables::prepare_running(&dir.join("data"), &launch, &launch).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_lifecycle_command_across_processes() {
        // Runtime dir is the isolated one in `deno task test`.
        if std::env::var_os("MUSTER_RUNTIME_DIR").is_none() {
            return;
        }
        let first = lifecycle_lock("the restart").unwrap();
        let e = lifecycle_lock("`muster stop`").err().unwrap().to_string();
        assert!(e.contains(LIFECYCLE_BUSY) && e.contains("`muster stop` was refused"), "{e}");
        // An ordinary start waits for it instead (`wait_lifecycle_lock`): it sees it held.
        assert!(try_lifecycle_lock().unwrap().is_none());
        let waiter = std::thread::spawn(|| {
            let started = Instant::now();
            let lock = wait_lifecycle_lock().unwrap();
            (started.elapsed(), lock)
        });
        std::thread::sleep(Duration::from_millis(500));
        drop(first);
        let (waited, lock) = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(400), "it waited for the restart: {waited:?}");
        drop(lock);
        lifecycle_lock("`muster stop`").unwrap();
    }

    #[test]
    fn a_restart_that_went_ahead_and_failed_is_told_by_the_next_start() {
        if std::env::var_os("MUSTER_RUNTIME_DIR").is_none() {
            return;
        }
        record_restart_failure(&anyhow::anyhow!("daemon (pid 7) did not stop within 120s")
            .context("the old backend did not stop"));
        let told = restart_failure().unwrap();
        assert!(told.starts_with("the last Restart Muster (less than a minute ago) didn't finish: the old backend did not stop: daemon (pid 7)"), "{told}");
        assert_eq!(take_restart_failure().as_deref(), Some(told.as_str()));
        assert!(restart_failure().is_none(), "told once");
    }

    /// A release of stand-in executables, as `prepare_running` publishes it.
    fn release(dir: &std::path::Path, host: &[u8]) -> executables::Release {
        let bin = dir.join("build");
        std::fs::create_dir_all(&bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for (name, body) in [("muster", &b"app"[..]), (executables::HOST, host)] {
            std::fs::write(bin.join(name), body).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        executables::prepare_running(&dir.join("data"), &bin.join("muster"), &bin.join("muster")).unwrap()
    }

    #[test]
    fn a_host_that_doesnt_run_or_speak_this_protocol_fails_the_check() {
        let dir = temp();
        // Truncated, but still executable: it can't even start.
        let truncated = release(&dir.join("a"), b"\x7fELF\x02\x01\x01");
        let e = format!("{:#}", terminal::probe_host(&truncated.host(), &dir.join("probe-a")).unwrap_err());
        // (Or run by the shell fallback for an unknown executable format, which then fails.)
        assert!(e.contains("running the terminal host") || e.contains("exited when started"), "{e}");
        // Starts, then exits without answering.
        let exits = release(&dir.join("b"), b"#!/bin/sh\necho 'muster-terminal-host: broken' >&2\nexit 3\n");
        let e = format!("{:#}", terminal::probe_host(&exits.host(), &dir.join("probe-b")).unwrap_err());
        assert!(e.contains("exited when started") && e.contains("broken"), "{e}");
        assert!(!dir.join("probe-b").exists(), "its scratch directories are removed");
        // Answers, but in another protocol version (a backend rebuilt after a protocol bump).
        let older = release(&dir.join("c"), b"#!/bin/sh\nsleep 20\n");
        let probe = dir.join("probe-c");
        old_host(&probe.join("run"), std::process::id());
        let e = format!("{:#}", terminal::probe_host(&older.host(), &probe).unwrap_err());
        assert!(e.contains("doesn't speak this build's protocol"), "{e}");
        // This build's own host passes (when the workspace build left it beside the tests).
        let built = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(executables::HOST);
        if built.exists() {
            terminal::probe_host(&built, &dir.join("probe-d")).unwrap();
            assert!(!dir.join("probe-d").exists());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_restart_is_done_only_when_its_own_release_answers_on_a_new_host() {
        let dir = temp();
        let release = release(&dir, b"host");
        let build: String = release.app_hash().chars().take(12).collect();
        let hello = |build: &str, instance: &str| serde_json::json!({"build": build, "terminal_host": {"instance": instance}});
        check_restarted(&hello(&build, "new"), &release, Some("old")).unwrap();
        let e = check_restarted(&hello("0123456789ab", "new"), &release, Some("old")).unwrap_err().to_string();
        assert!(e.contains("another build (0123456789ab)"), "{e}");
        let e = check_restarted(&serde_json::json!({"terminal_host": {"instance": "new"}}), &release, None).unwrap_err().to_string();
        assert!(e.contains("another build (unknown)"), "{e}");
        let e = check_restarted(&hello(&build, "old"), &release, Some("old")).unwrap_err().to_string();
        assert!(e.contains("no new terminal host"), "{e}");
        // The host it runs on is checked by content: this test process is no such host.
        let mut wrong = hello(&build, "new");
        wrong["terminal_host"]["pid"] = std::process::id().into();
        let e = check_restarted(&wrong, &release, Some("old")).unwrap_err().to_string();
        assert!(e.contains("isn't the one this restart checked"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
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
