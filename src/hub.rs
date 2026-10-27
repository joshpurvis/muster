//! The Hub is the daemon's single source of truth: workspaces, channels, live sessions,
//! the event log, and the broadcast bus that clients listen to.
//!
//! CONTRACT: the public method signatures below are used by `agents/*`, `detect.rs`,
//! `inject.rs`, `notify.rs`, `mcp.rs` and `rpc.rs`. The core implementer owns the bodies and
//! private fields; other modules must only use this public surface. Add methods freely;
//! don't change existing signatures without updating every caller.
//!
//! Locking rule: never hold a Hub lock across an `.await` or while calling into another
//! module's code.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::adapters::{ScreenSnapshot, SpawnCtx, adapter_for};
use crate::model::*;
use crate::paths;
use crate::pty;
use crate::terminal::{Terminal as PtySession, TerminalService, Output};
use muster_terminal_protocol::{Command as HostCommand, RunId, SpawnSpec};
use crate::store::{Store, StoredSession, StoredTerminal};
use crate::worktree;

/// Pushed to every connected client (and to in-daemon listeners such as the injector,
/// detector and notifier). Wire form: `{"event": "<snake_case variant>", "data": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ServerEvent {
    PanelUpserted(Panel),
    PanelRemoved { id: PanelId },
    /// A note was created or changed (text, title, window), with its full text.
    NoteUpserted(Note),
    NoteRemoved { id: NoteId },
    AccountProfilesChanged(Vec<crate::account_profiles::AccountProfile>),
    /// Claude usage observations or profile accounts changed (`account_usage.get`).
    AccountUsageChanged(crate::usage::UsageSnapshot),
    WorkspaceUpserted(Workspace),
    WorkspaceRemoved {
        id: WorkspaceId,
    },
    ChannelUpserted(Channel),
    ChannelRemoved {
        id: ChannelId,
    },
    /// A window was created or changed (tabs, order, name, open, focus time).
    WindowUpserted(Window),
    WindowRemoved {
        id: WindowId,
    },
    /// Sent on create and on ANY change (state, title, queued count, exit, resize).
    SessionUpserted(SessionInfo),
    SessionRemoved {
        id: SessionId,
    },
    /// A new timeline/event-log entry.
    Event(Event),
    /// Transient toast for the UI. level: "info" | "warn" | "error"
    Notice {
        level: String,
        text: String,
    },
    /// A named agent was created or its record changed (`agents::agent_json`).
    AgentUpserted(serde_json::Value),
    AgentRemoved {
        id: i64,
        name: String,
    },
    /// A message to an agent was recorded, refused or replied to (`agents::message_json`).
    AgentMessage(serde_json::Value),
    /// A schedule was created or changed, or its next run moved (`schedules::schedule_json`).
    ScheduleUpserted(serde_json::Value),
    ScheduleRemoved {
        id: i64,
        agent_id: i64,
    },
    /// A schedule run was recorded or changed (`schedules::run_json`).
    ScheduleRun(serde_json::Value),
    /// The scheduling settings changed (`schedules::settings_json`).
    ScheduleSettings(serde_json::Value),
    /// What `hello.updates` says changed (`updates::Status`).
    UpdatesChanged(crate::updates::Status),
    /// The shared window preferences changed (`prefs.set`).
    PrefsChanged(UiPrefs),
    /// The backend and terminal host are about to stop (`daemon.shutdown`); clients should not
    /// reconnect on their own.
    DaemonStopping {
        reason: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannel {
    /// Omitted: inherit from the calling Claude session, then workspace. "default": opt out.
    #[serde(default)]
    pub account_profile: Option<crate::account_profiles::ProfileChoice>,
    pub workspace_id: WorkspaceId,
    /// None or blank: auto-named (see `derive_name`).
    #[serde(default)]
    pub name: Option<String>,
    /// Create a git worktree at `<root>/<channel id>` on branch `muster/<channel id>` for this
    /// channel. Ignored for non-git workspaces (cwd is then the workspace path).
    #[serde(default)]
    pub worktree: bool,
    /// Branch/commit to base the worktree on. Default: the workspace's current HEAD.
    pub base: Option<String>,
    /// Start an agent right away (usual case). None = empty channel.
    pub agent: Option<SessionKind>,
    /// Initial prompt for that agent.
    pub prompt: Option<String>,
    /// Extra CLI arguments for that agent (after the permission flags, like
    /// `SpawnSession::extra_args`). Needs `agent`. Kept for restarts of that session only.
    #[serde(default, alias = "args")]
    pub extra_args: Vec<String>,
    /// Agent permission profile. Default: the CLIs' own defaults (see `Permissions`).
    #[serde(default, deserialize_with = "de_permissions")]
    pub permissions: Option<Permissions>,
    /// Window to put the tab in (must exist). Default: see `place_locked`.
    #[serde(default)]
    pub window_id: Option<WindowId>,
    /// Used when it exists, else the default placement (MCP: the caller's channel's window).
    #[serde(skip)]
    pub prefer_window: Option<WindowId>,
    /// Terminal size for that first agent (what the client will show it at).
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// The muster session a CLI call comes from (`$MUSTER_SESSION_ID`); rpc.rs applies its
    /// provenance (and, for an agent, the Claude origin rule) and sets `prov`.
    #[serde(default)]
    pub from_session: Option<SessionId>,
    /// Provenance the new session starts with (agents.rs): the spawning agent's, so a helper
    /// keeps it until it gets another attributed prompt. None: human-rooted.
    #[serde(skip)]
    pub prov: Option<crate::agents::Prov>,
}

fn de_permissions<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Permissions>, D::Error> {
    Ok(Option::<PermissionsInput>::deserialize(d)?.map(Into::into))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnSession {
    #[serde(default)]
    pub account_profile: Option<crate::account_profiles::ProfileChoice>,
    pub channel_id: ChannelId,
    pub kind: SessionKind,
    pub name: Option<String>,
    /// Defaults to the channel cwd.
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Set when an agent spawns another agent through MCP.
    #[serde(default)]
    pub parent_id: Option<SessionId>,
    /// Window to show the session in (must exist). Default: the parent session's window, else
    /// the channel's, else where a new channel would go (see `session_window_locked`).
    #[serde(default)]
    pub window_id: Option<WindowId>,
    /// The muster session a CLI call comes from (`$MUSTER_SESSION_ID`); rpc.rs applies its
    /// provenance (and, for an agent, the Claude origin rule) and sets `prov`.
    #[serde(default)]
    pub from_session: Option<SessionId>,
    /// Provenance the new session starts with (agents.rs): the spawning agent's, so a helper
    /// keeps it until it gets another attributed prompt. None: human-rooted.
    #[serde(skip)]
    pub prov: Option<crate::agents::Prov>,
    /// The named agent this session is started for (agents.rs), linked before launch.
    #[serde(skip)]
    pub agent_id: Option<i64>,
}

/// `spawn_session` failed with an unconfirmed launch outcome. The session and its prepared
/// launch are kept and reconciled in the background; callers must neither clean up its
/// channel or worktree nor retry the spawn (that could run the task twice).
#[derive(Debug)]
pub struct SpawnRetained(pub SessionId);
impl std::fmt::Display for SpawnRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session {} was kept; its launch is being confirmed", self.0)
    }
}

pub fn default_cols() -> u16 {
    120
}
pub fn default_rows() -> u16 {
    32
}

/// Daemon-wide settings the Hub needs.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub port: u16,
    /// Token clients (UI, CLI) authenticate with.
    pub client_token: String,
    /// Absolute path of the muster binary (exported as MUSTER_BIN).
    pub muster_bin: PathBuf,
    /// Absent only in pure application fixtures that never launch processes.
    pub terminal_host: Option<Arc<TerminalService>>,
    /// The owner's OpenRouter key, read from the daemon's environment at start
    /// (src/openrouter.rs). Memory only: never stored, logged or sent to a client.
    pub openrouter_key: Option<crate::openrouter::ApiKey>,
}

/// What `attach` hands to a client connection.
pub enum Attach {
    /// Running (or exited-this-run) session: replay bytes + live output receiver + the
    /// generation of the process they belong to (taken together, so a restart can't
    /// mismatch them; see `pty_generation`) + the raw output offset the replay ends at (the
    /// first live byte's; docs/PROTOCOL.md `attach_info`).
    Live { replay: Vec<u8>, rx: broadcast::Receiver<Output>, generation: u64, offset: u64 },
    /// Session from a previous daemon run: replay only.
    Replay(Vec<u8>),
}

/// Env vars that mark "running inside another agent session" and must not leak from the
/// daemon's own environment into sessions (configuration vars are kept).
const INHERITED_SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SSE_PORT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

/// Result of `start_process`.
struct Started {
    info: SessionInfo,
    consumed: bool,
    generation: u64,
    early_exit: Option<i32>,
}

/// A Screen update can't override a Hook update younger than this.
const HOOK_GRACE_MS: i64 = 1500;
const EVENT_BUS_CAP: usize = 4096;
/// Event log retention: rows older than this, or beyond the newest `EVENTS_PER_CHANNEL` of a
/// channel, are pruned at startup and hourly.
pub const EVENT_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const EVENTS_PER_CHANNEL: usize = 5000;
/// Launch codes (`POST /api/launch-code`) are single use and expire after this.
const LAUNCH_CODE_TTL: Duration = Duration::from_secs(60);
const MAX_LAUNCH_CODES: usize = 64;

struct SessEntry {
    info: SessionInfo,
    extra_args: Vec<String>,
    token: Option<String>,
    pty: Option<Arc<PtySession>>,
    /// Generation of the process being started / running.
    generation: u64,
    /// Exit reported before the PTY was installed (process died instantly).
    early_exit: Option<i32>,
    last_hook_at: i64,
    /// Unix ms of the last delivery out of the injection queue (queue length shrank), or 0.
    last_dequeue_at: i64,
    /// A spawn or restart is in progress (lifecycle ops on one session are serialized).
    busy: bool,
    /// `kill_session` arrived while a process was being started.
    kill_requested: bool,
    /// The run a restart killed to replace it: its exit is no failed start (`pty_exited`).
    /// The replacement's own exit, also one while the restart is still in progress (a CLI
    /// that can't reopen its conversation exits at once), is.
    killed_generation: Option<u64>,
    /// The last title this session set that was usable as a channel name (`clean_title`).
    /// Later unusable titles (a shell prompt, the program name) don't clear it.
    usable_title: Option<String>,
    /// Port block (`PORT`) the current process was started with; it stays reserved while that
    /// process runs, also after the session moved to another channel. 0: none yet.
    port_base: u16,
    /// Its `Working` was put there by backend recovery, not seen: leaving it is no sign of a
    /// model request (src/cache.rs). Cleared by the next state change.
    unverified_working: bool,
    /// The run's conversation (`conversation_after_hook`): Claude's latest reported id; Codex's
    /// top-level thread (the one it resumed, its SessionStart's, else the first to finish a
    /// turn). None until the run reports one.
    conversation: Option<String>,
    /// Codex: `conversation` is known to be the top-level thread (resumed, or named by
    /// SessionStart), so a notify from another thread is a sub-agent's.
    conversation_sure: bool,
    /// The current run sent a hook, so its CLI opened its conversation: its exit is never a
    /// failed resume (`conversation_missing`), whatever its output quotes. Recovered from the
    /// run's saved hook state (`store::HookSession`, written with every hook).
    opened: bool,
    /// A Codex tab's saved thread (`store::CodexThread`; agents keep theirs on their record).
    codex_thread: Option<crate::store::CodexThread>,
    /// A run whose hooks no longer change the conversation its agent resumes (Start fresh is
    /// replacing it): `fence_conversation`.
    fenced_generation: Option<u64>,
}

struct WinEntry {
    name: String,
    last_focused_at: i64,
    created_at: i64,
    /// Client connection showing this window (`window.claim`).
    holder: Option<u64>,
    /// The frontend's split layout (`window.set_layout`), stored verbatim.
    layout: Option<serde_json::Value>,
}

/// Latest usable (cleaned) terminal titles of a channel's sessions: auto-naming inputs.
#[derive(Default)]
struct Titles {
    agent: Option<String>,
    shell: Option<String>,
}

#[derive(Default)]
struct Inner {
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    channels: BTreeMap<ChannelId, Channel>,
    windows: BTreeMap<WindowId, WinEntry>,
    titles: HashMap<ChannelId, Titles>,
    /// Per channel, the source of the derived name its branch was last renamed after this run.
    /// A better source (agent title > first prompt > shell title) renames it again.
    branch_named_from: HashMap<ChannelId, NameSource>,
    /// Worktree directories being deleted right now: no process may start inside them.
    /// Windows a client window was just launched for (`window.show`), until one claims them.
    launching: HashMap<WindowId, std::time::Instant>,
    /// Counted: overlapping removals of one directory each hold it until they finish.
    removing_worktrees: HashMap<PathBuf, u32>,
    sessions: BTreeMap<SessionId, SessEntry>,
    tokens: HashMap<String, SessionId>,
    /// One-time launch code -> expiry.
    launch_codes: HashMap<String, std::time::Instant>,
}

/// Runtime key of the runs whose input is interrupted (`Hub::on_input_interrupted`).
const INPUT_INTERRUPTED_KEY: &str = "input-interrupted";

/// Human-input markers waiting for the callback boundary (see `Hub::note_human_input`).
/// Each keystroke takes a sequence number, so a callback can tell input that came before it
/// arrived from input that came after (`Hub::handle_hook`).
#[derive(Default)]
pub(crate) struct HumanInputQueue {
    /// The last sequence number handed out (0: none yet).
    seq: std::sync::atomic::AtomicU64,
    /// Per session, the first and last sequence number of its uncommitted keystrokes.
    pending: Mutex<BTreeMap<SessionId, (u64, u64)>>,
    wake: parking_lot::Condvar,
    /// Per session, the sequence number and time of its newest keystroke (memory only): a
    /// schedule's fresh restart never wipes what the human is typing (`human_input_of`).
    latest: Mutex<HashMap<SessionId, (u64, std::time::Instant)>>,
}

impl HumanInputQueue {
    fn note(&self, id: SessionId) {
        let mut pending = self.pending.lock();
        // Numbered under the lock, so a later number is never recorded before an earlier one.
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.latest.lock().insert(id, (seq, std::time::Instant::now()));
        match pending.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((seq, seq));
                self.wake.notify_one();
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => entry.get_mut().1 = seq,
        }
    }
    /// The newest sequence number: input numbered after this came later.
    pub(crate) fn now(&self) -> u64 { self.seq.load(Ordering::SeqCst) }
    fn is_empty(&self) -> bool { self.pending.lock().is_empty() }
    fn take(&self) -> BTreeMap<SessionId, (u64, u64)> { std::mem::take(&mut *self.pending.lock()) }
    fn restore(&self, marks: BTreeMap<SessionId, (u64, u64)>) {
        let mut pending = self.pending.lock();
        for (id, (first, last)) in marks {
            let entry = pending.entry(id).or_insert((first, last));
            *entry = (entry.0.min(first), entry.1.max(last));
        }
    }
    /// Wait until a marker is pending (or the timeout passes).
    fn wait(&self, timeout: Duration) {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            self.wake.wait_for(&mut pending, timeout);
        }
    }
}

/// Commit human-input markers off the input path, promptly after each keystroke. A boundary
/// that cannot be established yet (callbacks still arriving) keeps the markers and retries.
/// Stops when the backend shuts down; `Hub::shutdown` commits what is left and joins it.
pub fn spawn_input_attribution(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let queue = hub.human_input.clone();
    let weak = Arc::downgrade(hub);
    let worker = std::thread::Builder::new().name("input-attribution".into()).spawn(move || {
        let mut failing = false;
        loop {
            queue.wait(Duration::from_millis(500));
            let Some(hub) = weak.upgrade() else { break };
            if hub.is_shutting_down() {
                break;
            }
            match hub.flush_human_input() {
                Ok(()) => failing = false,
                Err(error) => {
                    if !failing {
                        tracing::debug!("human-input attribution waiting for callbacks: {error:#}");
                    }
                    failing = true;
                    drop(hub);
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    })?;
    *hub.workers.lock() = vec![worker];
    Ok(())
}

/// Tell the user when a terminal's input was interrupted (`terminal::InputLink`), off the
/// input path: the link's hook only queues the run here.
pub fn spawn_input_interruptions(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let Some(host) = hub.cfg.terminal_host.clone() else { return Ok(()) };
    let (tx, rx) = std::sync::mpsc::channel::<RunId>();
    let queued = hub.interruptions_pending.clone();
    host.input().on_interrupt(Arc::new(move |run| {
        queued.fetch_add(1, Ordering::SeqCst);
        if tx.send(run).is_err() { queued.fetch_sub(1, Ordering::SeqCst); }
    }));
    let weak = Arc::downgrade(hub);
    let queued = hub.interruptions_pending.clone();
    std::thread::Builder::new().name("input-interrupted".into()).spawn(move || {
        while let Ok(run) = rx.recv() {
            if let Some(hub) = weak.upgrade() { hub.on_input_interrupted(run); }
            queued.fetch_sub(1, Ordering::SeqCst);
        }
    })?;
    Ok(())
}

pub struct Hub {
    pub panels: crate::panels::Panels,
    pub notes: crate::notes::Notes,
    /// Schedules and their runs (src/schedules/).
    pub schedules: crate::schedules::Schedules,
    /// Trigger watchers' live state (src/schedules/triggers.rs).
    pub triggers: crate::schedules::triggers::Watchers,
    /// OpenRouter spend and the daily dollar caps (src/spend.rs).
    pub spend: crate::spend::Spend,
    cfg: HubConfig,
    store: Store,
    tx: broadcast::Sender<ServerEvent>,
    inner: Mutex<Inner>,
    generation_floor: u64,
    terminal_records: Mutex<BTreeMap<SessionId, StoredTerminal>>,
    retire_signalled: Mutex<BTreeSet<RunId>>,
    /// Serializes inbox consumption and input attribution. The host never sees it.
    pub(crate) callback_inbox: Mutex<Option<crate::hook_inbox::Inbox>>,
    /// Sessions that received human keystrokes whose attribution marker is not committed
    /// yet (`note_human_input`). Keystrokes only add to it; markers are taken and committed
    /// under the callback boundary, never on the input path.
    human_input: Arc<HumanInputQueue>,
    /// Each session's current terminal handle, for the input path: the same handle as
    /// `SessEntry::pty` (updated together, under `inner`), but behind a lock that is never
    /// held across SQLite I/O, so a keystroke never waits for a callback's commit.
    input_handles: Mutex<HashMap<SessionId, Arc<PtySession>>>,
    /// Interruption announcements not yet handled (`spawn_input_interruptions`).
    interruptions_pending: Arc<std::sync::atomic::AtomicUsize>,
    /// Background threads `shutdown` joins (the attribution worker).
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    shutting_down: AtomicBool,
    /// Client replays of exited sessions rebuilt from output.log (a vt100 pass over up to
    /// 2 MiB), keyed by session and the log's (length, mtime).
    replay_cache: Mutex<HashMap<SessionId, (u64, std::time::SystemTime, Bytes)>>,
    /// Raw output tails for phones' `delta` replays (src/replay_tail.rs).
    tails: Arc<crate::replay_tail::Tails>,
    /// Per session, how many times its terminal was resized this daemon run: a client's offset
    /// from an earlier size revision is never resumed with a `delta` (docs/PROTOCOL.md).
    /// The value is (revision, resizes in progress).
    size_revs: Mutex<HashMap<SessionId, (u64, u32)>>,
    /// Paired remote devices (docs/ANDROID.md).
    devices: crate::devices::Registry,
    /// Agents (PLAN.md §10): records, and every session's provenance and active agent message.
    agents: crate::agents::Registry,
    pub(crate) injection: crate::inject::Queues,
    /// The MCP server (weak: it holds the hub), set by `mcp::router`.
    #[allow(clippy::type_complexity)]
    mcp: Mutex<Option<Weak<crate::mcp::McpState<crate::mcp::backend::HubBackend>>>>,
    /// Source of `SessionInfo.state_seq` values.
    state_seq: crate::store::StateSequences,
    /// Serializes branch renames (git calls) so they see each other's results.
    branch_lock: tokio::sync::Mutex<()>,
    /// For work started from PTY threads (outside the runtime).
    rt: Option<tokio::runtime::Handle>,
    /// Waiting UI, backend and terminal host updates (`updates::spawn` fills it).
    updates: crate::updates::Updates,
    /// Serializes `set_ui_prefs`.
    prefs_lock: Mutex<()>,
    /// The cold-cache rules from the shared preferences (`UiPrefs.cache_*`, src/cache.rs).
    cache_rules: Mutex<crate::cache::CacheRules>,
    /// OpenRouter's model list (`openrouter.models`).
    openrouter_models: Arc<crate::openrouter::ModelCache>,
    weak: Weak<Hub>,
}

const REPLAY_CACHE_MAX: usize = 8;

impl Hub {
    /// Load persisted state. Sessions that were running when the previous daemon died are
    /// marked Exited ("daemon restarted").
    /// Decide, under one lock, whether `window.show` should launch a client for `id`: not when a
    /// connection holds it, nor while an earlier launch (under 15 s ago) hasn't been claimed;
    /// otherwise the launch is recorded (so concurrent calls can't both launch).
    pub fn window_launch(&self, id: WindowId) -> anyhow::Result<WindowLaunch> {
        let mut g = self.inner.lock();
        let w = g.windows.get(&id).ok_or_else(|| anyhow::anyhow!("no such window: {id}"))?;
        if w.holder.is_some() {
            return Ok(WindowLaunch::Held(w.name.clone()));
        }
        let now = std::time::Instant::now();
        if g.launching.get(&id).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(15)) {
            return Ok(WindowLaunch::Launching);
        }
        g.launching.insert(id, now);
        Ok(WindowLaunch::Go)
    }

    #[cfg(test)]
    pub fn begin_window_launch(&self, id: WindowId) -> bool {
        matches!(self.window_launch(id), Ok(WindowLaunch::Go))
    }

    /// A launch failed: allow the next `window.show` to try again.
    pub fn end_window_launch(&self, id: WindowId) {
        self.inner.lock().launching.remove(&id);
    }

    /// The muster binary the daemon runs as (for launching client windows).
    pub fn muster_bin(&self) -> &Path {
        &self.cfg.muster_bin
    }

    pub fn updates(&self) -> &crate::updates::Updates {
        &self.updates
    }

    pub fn openrouter_key(&self) -> Option<&crate::openrouter::ApiKey> {
        self.cfg.openrouter_key.as_ref()
    }

    /// The fingerprint of the OpenRouter key session `id`'s current run was launched with
    /// (None: not an OpenRouter launch, or one from before fingerprints were kept).
    pub fn run_key_fingerprint(&self, id: SessionId) -> Option<String> {
        self.terminal_records.lock().get(&id).and_then(|r| r.key_fp.clone())
    }

    /// This hub as an `Arc` (None only while it is being dropped).
    pub fn arc(&self) -> Option<Arc<Hub>> {
        self.weak.upgrade()
    }

    pub fn openrouter_models(&self) -> &Arc<crate::openrouter::ModelCache> {
        &self.openrouter_models
    }

    /// The provider session `id` runs on: its named agent's (read at each start, so an edit
    /// applies from the next one); every other session uses its CLI's own login.
    pub fn session_provider(&self, id: SessionId) -> crate::openrouter::Provider {
        self.agents.by_session(id).map(|a| a.provider).unwrap_or_default()
    }

    /// Fail before a (re)start when the session's provider can't run (no key).
    pub fn check_session_provider(&self, id: SessionId) -> anyhow::Result<()> {
        if self.session_provider(id) == crate::openrouter::Provider::OpenRouter && self.openrouter_key().is_none() {
            return Err(crate::openrouter::missing_key_error());
        }
        Ok(())
    }

    /// Window preferences shared by every window (`prefs.get`).
    pub fn ui_prefs(&self) -> UiPrefs {
        match self.store.load_runtime::<UiPrefs>(UI_PREFS_KEY) {
            Ok(p) => p.unwrap_or_default(),
            Err(error) => {
                tracing::warn!("reading window preferences: {error:#}");
                UiPrefs::default()
            }
        }
    }

    /// Change the given window preferences, save them and tell every client. Writes are
    /// serialized from read to broadcast, so the last write is what is stored and what every
    /// client hears last.
    pub fn set_ui_prefs(&self, change: UiPrefsChange) -> anyhow::Result<UiPrefs> {
        let _one_at_a_time = self.prefs_lock.lock();
        let mut p = self.ui_prefs();
        if let Some(d) = change.decorations {
            p.decorations = d;
        }
        if let Some(shell) = change.shell {
            p.shell = match shell.trim() {
                "" => None,
                s => Some(crate::adapters::shell::resolve_shell(s)?),
            };
        }
        if let Some(on) = change.cache_warn {
            p.cache_warn = on;
        }
        if let Some(ttl) = change.cache_ttl {
            p.cache_ttl.apply(ttl)?;
        }
        p.revision += 1;
        self.store.save_runtime(UI_PREFS_KEY, &p)?;
        crate::adapters::shell::set_configured_shell(p.shell.clone());
        self.emit(ServerEvent::PrefsChanged(p.clone()));
        self.set_cache_rules(crate::cache::CacheRules::from_prefs(&p));
        Ok(p)
    }

    /// New cold-cache rules: every session's `cache_cold_at` follows (derived, not stored).
    fn set_cache_rules(&self, rules: crate::cache::CacheRules) {
        let mut g = self.inner.lock();
        {
            let mut current = self.cache_rules.lock();
            if *current == rules {
                return;
            }
            *current = rules;
        }
        for e in g.sessions.values_mut() {
            if rules.refresh(&mut e.info) {
                self.publish_session(&e.info);
            }
        }
    }

    /// Record that Muster typed a prompt (`prov`; `panel`: panel events) into session `id`
    /// after it had been idle `idle_ms`, past its cache lifetime: the agent message gets
    /// `cold_idle_ms` (so do the schedule runs that sent it), and the timeline a notice.
    /// Never fails a delivery.
    /// `delivery`: what was typed (a message id, a panel batch, the text): a retry of the same
    /// delivery is recorded once.
    pub(crate) fn note_cold_send(&self, id: SessionId, prov: crate::agents::Prov, panel: bool, delivery: &str,
        idle_ms: i64) {
        let Some(info) = self.session(id) else { return };
        let key = format!("cold-send:{id}");
        if self.store.load_runtime::<String>(&key).ok().flatten().as_deref() == Some(delivery) {
            return;
        }
        if let Err(error) = self.store.save_runtime(&key, &delivery.to_string()) {
            tracing::warn!(session = id, "recording a cold start: {error:#}");
        }
        if let Some(msg) = prov.msg_id {
            match self.store.set_agent_message_cold(msg, idle_ms) {
                Ok(true) => crate::agents::publish_message(self, msg),
                // Recorded already: the same prompt submitted again.
                Ok(false) => return,
                Err(error) => tracing::warn!(session = id, msg, "marking a cold start: {error:#}"),
            }
        }
        let what = if panel {
            "panel events"
        } else {
            match prov.origin {
                crate::agents::Origin::Schedule => "a scheduled message",
                crate::agents::Origin::Webhook => "a webhook message",
                crate::agents::Origin::Human => "a message",
            }
        };
        let text = format!(
            "Cold start: Muster sent {what} to {} after {} idle. Its prompt cache had likely expired, so it re-read the whole conversation at full cost.",
            info.name,
            crate::cache::idle_text(idle_ms)
        );
        tracing::info!(session = id, idle_ms, "{text}");
        self.log_event(Some(info.channel_id), Some(id), EventKind::System, serde_json::json!({
            "text": text, "level": "info",
            "cold_start": {"session_id": id, "idle_ms": idle_ms, "msg_id": prov.msg_id, "panel": panel},
        }));
    }

    /// A named agent's session is going away (its task closed): keep its conversation's cache
    /// clock with the agent, for a later resume of the same conversation.
    fn save_conversation_turn(&self, info: &SessionInfo) {
        let Some(agent) = self.agents.by_session(info.id) else { return };
        let (Some(resume), Some(at)) = (agent.resume_id, info.last_turn_at) else { return };
        if let Err(error) = self.store.save_runtime(&format!("agent-turn:{}", agent.id), &(resume, at)) {
            tracing::warn!(session = info.id, "keeping the conversation's cache clock: {error:#}");
        }
    }

    /// The cache clock saved for the conversation session `id`'s agent resumes, if it is the same.
    fn saved_conversation_turn(&self, id: SessionId) -> Option<i64> {
        let agent = self.agents.by_session(id)?;
        let resume = agent.resume_id?;
        let saved: (String, i64) = self.store.load_runtime(&format!("agent-turn:{}", agent.id)).ok().flatten()?;
        (saved.0 == resume).then_some(saved.1)
    }

    /// Session `id`'s next message would re-read its whole conversation uncached (src/cache.rs):
    /// how long it has been idle, else None.
    pub fn cache_cold(&self, id: SessionId, now: i64) -> Option<i64> {
        let g = self.inner.lock();
        let info = &g.sessions.get(&id)?.info;
        crate::cache::is_cold(info, now).then(|| now - info.last_turn_at.unwrap_or(now))
    }

    pub fn terminal_host_info(&self) -> Option<muster_terminal_protocol::Hello> {
        self.cfg.terminal_host.as_ref().map(|host| host.client.hello.clone())
    }

    pub fn new(cfg: HubConfig, store: Store) -> anyhow::Result<Arc<Hub>> {
        let state_seq = crate::store::StateSequences::new(&store);
        let mut terminal_records = store.load_terminals()?;
        let mut inventory = match &cfg.terminal_host {
            Some(host) => host.inventory()?,
            None => Vec::new(),
        };
        for run in inventory.iter().filter(|run| run.has_processes) {
            anyhow::ensure!(terminal_records.get(&run.run.id).is_some_and(|record| record.spec.run == run.run),
                "host owns unrecognized terminal {:?} in {}; preserving it and refusing application mutations until its record is recovered",
                run.run, run.cwd.display());
        }
        // Complete prepared launch intents using the same run identity. The host
        // serializes duplicate spawns and returns the original process after a lost reply.
        if let Some(host) = &cfg.terminal_host {
            for record in terminal_records.values_mut() {
                if record.host_instance != host.client.hello.instance { continue }
                if !record.retired && record.pending {
                    // The host already has this run: it started, so adopt it below. Re-sending
                    // could differ from what was launched (the OpenRouter key changed or is gone
                    // since), which the host refuses, and a refused run would be retired: its
                    // live process killed.
                    if inventory.iter().any(|run| run.run == record.spec.run) {
                        record.pending = false;
                        store.save_terminal(record.spec.run.id, record)?;
                        continue;
                    }
                    // Never started, and it needs a key this backend doesn't have: keep it
                    // pending (nothing runs) until a backend with the key recovers it.
                    if !record.key_refs.is_empty() && cfg.openrouter_key.is_none() {
                        tracing::warn!(run = ?record.spec.run, "an OpenRouter launch waits for {}", crate::openrouter::ENV_KEY);
                        continue;
                    }
                    match host.spawn(crate::openrouter::restore_spec(record.spec.clone(), &record.key_refs, cfg.openrouter_key.as_ref())) {
                        Ok(_) => {
                            record.pending = false;
                            // Sent with this backend's key: that is the key it bills.
                            if !record.key_refs.is_empty() {
                                record.key_fp = cfg.openrouter_key.as_ref().map(|k| k.fingerprint());
                            }
                        }
                        // A definite per-run rejection (its program or cwd vanished during
                        // the outage) retires only that launch; the others still recover.
                        // Transport and identity uncertainty still stops startup.
                        Err(error) if is_definite_spawn_failure(&error) => {
                            tracing::warn!(run = ?record.spec.run, "prepared launch was rejected: {error:#}");
                            record.retired = true;
                        }
                        Err(error) => return Err(error),
                    }
                    store.save_terminal(record.spec.run.id, record)?;
                }
            }
            inventory = host.inventory()?;
        }
        let generation_floor = terminal_records.values().map(|r| r.spec.run.generation)
            .chain(inventory.iter().map(|s| s.run.generation)).max().unwrap_or(0);
        // Persist this floor before any retired records can be deleted. Even a
        // restart with no new spawns must remember the host's older generations.
        let generation_floor = store.next_terminal_generation(generation_floor)?;
        match store.prune_events(now_ms() - EVENT_MAX_AGE_MS, EVENTS_PER_CHANNEL) {
            Ok(n) if n > 0 => tracing::info!("pruned {n} old events"),
            Ok(_) => {}
            Err(e) => tracing::warn!("pruning events: {e}"),
        }
        let mut inner = Inner::default();
        let cache_rules = crate::cache::CacheRules::from_prefs(
            &store.load_runtime::<UiPrefs>(UI_PREFS_KEY).ok().flatten().unwrap_or_default());
        for w in store.load_workspaces()? {
            inner.workspaces.insert(w.id, w);
        }
        for c in store.load_channels()? {
            inner.channels.insert(c.id, c);
        }
        for s in store.load_sessions()? {
            let StoredSession { mut info, extra_args } = s;
            info.state_seq = state_seq.next()?;
            let record = terminal_records.get(&info.id).filter(|r| !r.retired);
            let status = record.and_then(|r| cfg.terminal_host.as_ref()
                .filter(|h| h.client.hello.instance == r.host_instance)
                .and_then(|_| inventory.iter().find(|s| s.run == r.spec.run)));
            let pty = status.and_then(|s| cfg.terminal_host.as_ref().map(|h| h.adopt(s.clone())));
            let generation = record.map_or(0, |r| r.spec.run.generation);
            let token = record.filter(|_| status.is_some_and(|s| s.alive)).map(|r| r.token.clone());
            let port_base = record.map_or(0, |r| r.port_base);
            // Saved before the cache clock existed: estimate it from the saved row.
            let estimated_turn = crate::cache::estimated_turn(&info);
            let unverified_working = recover_session_state(&mut info, status, record.is_some(), now_ms());
            if info.state != AgentState::Exited && info.last_turn_at.is_none() {
                info.last_turn_at = estimated_turn;
            }
            // What the run's saved launch says about its cache (credentials: presence only).
            info.cache_basis = record.map_or_else(Default::default, |r| {
                crate::cache::basis_from_env(info.kind, |k| r.spec.env.get(k).map(String::as_str))
            });
            let saved_hook = store.load_hook_session(info.id, generation)?;
            // Every hook saves it: the run's CLI sent one, so it opened its conversation.
            let opened = saved_hook.is_some();
            let hook = saved_hook.filter(|_| info.state != AgentState::Exited);
            info.subagents = hook.as_ref().map_or(0, |saved| saved.subagents);
            let last_hook_at = hook.as_ref().map_or(0, |saved| saved.last_hook_at);
            let (conversation, conversation_sure) = match hook {
                Some(saved) => (saved.conversation, saved.conversation_sure),
                // No hook from this run yet: a Codex run that resumed a thread has that one as
                // its top-level conversation from the start (as `start_process` set it), read
                // from its launch, so a sub-agent's first turn can't replace it.
                None => recovered_conversation(info.kind, record.map(|r| r.spec.args.as_slice())),
            };
            let codex_thread = if info.kind == SessionKind::Codex {
                tab_thread_at_load(store.codex_thread(info.id)?,
                    terminal_records.get(&info.id).map(|r| r.spec.run.generation), generation)
            } else { None };
            cache_rules.refresh(&mut info);
            store.update_session(&info)?;
            if let Some(token) = &token { inner.tokens.insert(token.clone(), info.id); }
            let usable_title = info.title.as_deref().and_then(|t| clean_title(t, info.kind));
            inner.sessions.insert(
                info.id,
                SessEntry {
                    info,
                    extra_args,
                    token,
                    pty,
                    generation,
                    early_exit: None,
                    last_hook_at,
                    last_dequeue_at: 0,
                    busy: false,
                    kill_requested: false,
                    killed_generation: None,
                    usable_title,
                    port_base,
                    unverified_working,
                    conversation,
                    conversation_sure,
                    opened,
                    codex_thread,
                    fenced_generation: None,
                },
            );
        }
        let panels = store.load_panels()?;
        for w in store.load_windows()? {
            // Nothing holds a window at startup: ones without tabs or tasks are gone for good.
            if window_unused(&inner, w.id) && !panels.iter().any(|p| !p.closed && p.window_id == Some(w.id)) {
                if let Err(e) = store.delete_window(w.id) {
                    tracing::warn!(window = w.id, "deleting empty window: {e}");
                }
                continue;
            }
            inner.windows.insert(
                w.id,
                WinEntry {
                    name: w.name,
                    last_focused_at: w.last_focused_at,
                    created_at: w.created_at,
                    holder: None,
                    layout: w.layout,
                },
            );
        }
        // Notes whose window is gone are closed for good (window ids are reused).
        if let Err(e) = store.close_orphan_notes() {
            tracing::warn!("closing notes of deleted windows: {e}");
        }
        // Typing stays paused for a run whose input was interrupted before a restart, until
        // the user continues. The input path finds each session's handle without `inner`.
        let interrupted: Vec<(RunId, String)> = store.load_runtime(INPUT_INTERRUPTED_KEY)?.unwrap_or_default();
        let mut input_handles = HashMap::new();
        for (id, entry) in inner.sessions.iter_mut() {
            let Some(pty) = &entry.pty else { continue };
            if let Some((run, reason)) = interrupted.iter().find(|(run, _)| *run == pty.run() && pty.is_alive()) {
                if let Some(host) = &cfg.terminal_host { host.input().restore_interrupted(*run, reason); }
                entry.info.input_interrupted = true;
            }
            input_handles.insert(*id, pty.clone());
        }
        let devices = crate::devices::Registry::load(&store)?;
        let agents = crate::agents::Registry::load(&store)?;
        let schedules = crate::schedules::Schedules::load(&store, Arc::new(crate::schedules::SystemClock::from_env()))?;
        let injection = crate::inject::Queues::load(&store)?;
        let (tx, _) = broadcast::channel(EVENT_BUS_CAP);
        let spend = crate::spend::Spend::load(&store, cfg.openrouter_key.as_ref());
        let hub = Arc::new_cyclic(|weak| Hub {
            panels: crate::panels::Panels::default(),
            notes: crate::notes::Notes::default(),
            schedules,
            triggers: Default::default(),
            spend,
            cfg,
            devices,
            agents,
            injection,
            mcp: Mutex::new(None),
            state_seq,
            store,
            tx,
            inner: Mutex::new(inner),
            generation_floor,
            terminal_records: Mutex::new(terminal_records),
            retire_signalled: Mutex::new(BTreeSet::new()),
            callback_inbox: Mutex::new(None),
            human_input: Arc::default(),
            input_handles: Mutex::new(input_handles),
            interruptions_pending: Arc::default(),
            workers: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
            replay_cache: Mutex::new(HashMap::new()),
            tails: Arc::default(),
            size_revs: Mutex::new(HashMap::new()),
            branch_lock: tokio::sync::Mutex::new(()),
            rt: tokio::runtime::Handle::try_current().ok(),
            updates: Default::default(),
            prefs_lock: Mutex::new(()),
            cache_rules: Mutex::new(cache_rules),
            openrouter_models: Arc::new(crate::openrouter::ModelCache::new(paths::data_dir().join("openrouter-models.json"))),
            weak: weak.clone(),
        });
        if let Some(host) = &hub.cfg.terminal_host {
            let weak = Arc::downgrade(&hub);
            let unavailable = weak.clone();
            host.watch(Arc::new(move |event| {
                let Some(hub) = weak.upgrade() else { return };
                use muster_terminal_core::TerminalEvent;
                match event {
                    TerminalEvent::Title { id, generation, title } => hub.pty_title_changed(id, generation, title),
                    TerminalEvent::Exited { id, generation, code } => hub.pty_exited(id, generation, code),
                    TerminalEvent::Drained { .. } => {}, // Host releases final terminal resources.
                }
            }), Arc::new(move |message| {
                if let Some(hub) = unavailable.upgrade() { hub.notice("error", message); }
            }))?;
        }
        // Before any input can flow (queue recovery below may type).
        spawn_input_interruptions(&hub)?;
        if let Err(error) = hub.reconcile_retired() {
            tracing::warn!("terminal retirement will retry after startup: {error:#}");
        }
        crate::inject::restore(&hub)?;
        crate::adapters::shell::set_configured_shell(hub.ui_prefs().shell);
        Ok(hub)
    }

    /// Base URL of this daemon, e.g. "http://127.0.0.1:7433".
    pub fn daemon_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.cfg.port)
    }

    pub fn devices(&self) -> &crate::devices::Registry {
        &self.devices
    }

    pub fn agents(&self) -> &crate::agents::Registry {
        &self.agents
    }

    /// The MCP server's state, so CLI calls of agent sessions run through the same tools.
    pub fn set_mcp(&self, st: Weak<crate::mcp::McpState<crate::mcp::backend::HubBackend>>) {
        *self.mcp.lock() = Some(st);
    }

    pub fn mcp(&self) -> Option<Arc<crate::mcp::McpState<crate::mcp::backend::HubBackend>>> {
        self.mcp.lock().as_ref().and_then(Weak::upgrade)
    }

    /// The store, for registries that write through to it (devices).
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn port(&self) -> u16 {
        self.cfg.port
    }

    pub fn check_client_token(&self, token: &str) -> bool {
        paths::token_eq(token, &self.cfg.client_token)
    }

    /// Mint a single-use launch code (60 s) the UI trades for the client token, so the token
    /// never shows up on a browser command line.
    pub fn mint_launch_code(&self) -> String {
        let now = std::time::Instant::now();
        let code = paths::random_token();
        let mut g = self.inner.lock();
        g.launch_codes.retain(|_, exp| *exp > now);
        if g.launch_codes.len() >= MAX_LAUNCH_CODES
            && let Some(oldest) = g.launch_codes.iter().min_by_key(|(_, e)| **e).map(|(c, _)| c.clone())
        {
            g.launch_codes.remove(&oldest);
        }
        g.launch_codes.insert(code.clone(), now + LAUNCH_CODE_TTL);
        code
    }

    /// Consume a launch code; returns the client token if it was valid and unexpired.
    pub fn exchange_launch_code(&self, code: &str) -> Option<String> {
        let now = std::time::Instant::now();
        let mut g = self.inner.lock();
        g.launch_codes.retain(|_, exp| *exp > now);
        let key = g.launch_codes.keys().find(|k| paths::token_eq(k, code)).cloned()?;
        g.launch_codes.remove(&key);
        Some(self.cfg.client_token.clone())
    }

    /// Apply event-log retention (see `EVENT_MAX_AGE_MS`, `EVENTS_PER_CHANNEL`).
    pub fn prune_events(&self) {
        match self.store.prune_events(now_ms() - EVENT_MAX_AGE_MS, EVENTS_PER_CHANNEL) {
            Ok(n) if n > 0 => tracing::info!("pruned {n} old events"),
            Ok(_) => {}
            Err(e) => tracing::warn!("pruning events: {e}"),
        }
        let refused_before = now_ms() - crate::agents::REFUSED_MAX_AGE_MS;
        if let Err(e) = self.store.prune_refused_agent_messages(refused_before, crate::agents::REFUSED_KEEP) {
            tracing::warn!("pruning refused agent messages: {e}");
        }
    }

    // ----- event bus -----

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.tx.subscribe()
    }

    pub fn emit(&self, ev: ServerEvent) {
        let _ = self.tx.send(ev);
    }

    pub fn notice(&self, level: &str, text: impl Into<String>) {
        self.emit(ServerEvent::Notice {
            level: level.to_string(),
            text: text.into(),
        });
    }

    /// Persist an event-log entry and broadcast it as `ServerEvent::Event`.
    pub fn log_event(
        &self,
        channel_id: Option<ChannelId>,
        session_id: Option<SessionId>,
        kind: EventKind,
        data: serde_json::Value,
    ) -> Event {
        let ts = now_ms();
        let id = match self.store.insert_event(channel_id, session_id, kind, &data, ts) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("persisting event: {e}");
                0
            }
        };
        let ev = Event {
            id,
            channel_id,
            session_id,
            kind,
            data,
            ts,
        };
        self.emit(ServerEvent::Event(ev.clone()));
        ev
    }

    /// Timeline for a channel, newest last. `before` = event id for paging back.
    pub fn timeline(&self, channel_id: ChannelId, before: Option<i64>, limit: usize) -> Vec<Event> {
        self.store
            .timeline(channel_id, before, limit.clamp(1, 1000))
            .unwrap_or_else(|e| {
                tracing::warn!("reading timeline: {e}");
                Vec::new()
            })
    }

    // ----- queries -----

    pub fn account_profiles(&self) -> anyhow::Result<Vec<crate::account_profiles::AccountProfile>> {
        self.store.account_profiles()
    }

    pub fn account_profile(&self, id: i64) -> anyhow::Result<crate::account_profiles::AccountProfile> {
        self.account_profiles()?.into_iter().find(|p| p.id == id)
            .ok_or_else(|| anyhow::anyhow!("no such Claude profile: {id}; choose an available profile explicitly"))
    }

    pub fn add_account_profile(&self, name: &str, directory: &str) -> anyhow::Result<crate::account_profiles::AccountProfile> {
        let name = crate::account_profiles::clean_name(name)?;
        let directory = crate::account_profiles::config_directory(directory)?;
        let id = self.store.insert_account_profile(&name, &directory)?;
        self.emit(ServerEvent::AccountProfilesChanged(self.account_profiles()?));
        self.usage_changed();
        self.account_profile(id)
    }

    pub fn update_account_profile(&self, id: i64, name: Option<String>, enabled: Option<bool>) -> anyhow::Result<crate::account_profiles::AccountProfile> {
        let name = name.as_deref().map(crate::account_profiles::clean_name).transpose()?;
        self.store.update_account_profile(id, name.as_deref(), enabled)?;
        self.emit(ServerEvent::AccountProfilesChanged(self.account_profiles()?));
        self.usage_changed();
        self.account_profile(id)
    }

    pub fn apply_account_profile(&self, id: Option<i64>, env: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
        if let Some(id) = id { crate::account_profiles::apply_environment(&self.account_profile(id)?, env)?; }
        Ok(())
    }

    pub fn check_account_profile(&self, id: Option<i64>) -> anyhow::Result<()> {
        self.apply_account_profile(id, &mut std::env::vars().collect())
    }

    /// Each account's last reported usage and a placement suggestion (src/usage.rs).
    pub fn account_usage(&self) -> anyhow::Result<crate::usage::UsageSnapshot> {
        let profiles = self.account_profiles()?;
        let observations = self.store.usage_observations()?;
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let default_dir = crate::usage::effective_config_dir(&env);
        Ok(crate::usage::snapshot(&profiles, &observations,
            |id| self.check_account_profile(id).map_err(|e| format!("{e:#}")), default_dir.as_deref(), now_ms()))
    }

    fn usage_changed(&self) {
        match self.account_usage() {
            Ok(snapshot) => self.emit(ServerEvent::AccountUsageChanged(snapshot)),
            Err(e) => tracing::warn!("reading Claude usage: {e:#}"),
        }
    }

    /// A status-line report (`POST /usage/claude`), attributed only by its run's session token:
    /// to that session, its current run and the configuration folder that run was launched with
    /// (its pinned profile's, or Default's at the time).
    pub fn record_usage(&self, token: &str, body: &serde_json::Value) -> anyhow::Result<()> {
        let now = now_ms();
        let report = crate::usage::parse_report(body, now);
        let (session, generation) = {
            let g = self.inner.lock();
            let id = *g.tokens.get(token).context("unknown session token")?;
            let e = g.sessions.get(&id).context("unknown session token")?;
            anyhow::ensure!(e.info.kind == SessionKind::Claude, "usage reports come from Claude sessions");
            (id, e.generation)
        };
        // A report with only the API time (before the run's first response) is the baseline
        // the next one is measured against.
        if report.windows.is_empty() && report.api_ms.is_none() {
            return Ok(());
        }
        if self.store.apply_usage(session, generation, &report, now)? {
            self.usage_changed();
        }
        Ok(())
    }

    /// The human says profile `id` signs in to the same account as `with` (None: its own).
    pub fn set_same_account(&self, id: i64, with: Option<i64>) -> anyhow::Result<crate::account_profiles::AccountProfile> {
        if let Some(with) = with { self.account_profile(with)?; }
        self.store.set_same_account(id, with)?;
        self.emit(ServerEvent::AccountProfilesChanged(self.account_profiles()?));
        self.usage_changed();
        self.account_profile(id)
    }

    /// The human signed in to another account in this profile (or Default): the login lives in
    /// its folder, so the folder's observations and the same-account declaration of every
    /// profile on that folder no longer apply (Default and a profile there are the same login).
    pub fn account_login_changed(&self, choice: crate::account_profiles::ProfileChoice) -> anyhow::Result<()> {
        let folder = match choice.id() {
            Some(id) => Some(self.account_profile(id)?.config_dir),
            None => crate::usage::effective_config_dir(&std::env::vars().collect())
                .map(|d| d.to_string_lossy().into_owned()),
        };
        let Some(folder) = folder else {
            // Default's folder doesn't exist, so nothing was ever filed under it.
            return Ok(());
        };
        let profiles = self.account_profiles()?;
        for p in profiles.iter().filter(|p| p.config_dir == folder && p.account_group.is_some()) {
            self.store.set_same_account(p.id, None)?;
        }
        self.store.clear_usage(&crate::usage::folder_key(&folder))?;
        self.emit(ServerEvent::AccountProfilesChanged(self.account_profiles()?));
        self.usage_changed();
        Ok(())
    }

    /// Resolve exactly once at creation. Restarts use SessionInfo's pinned ID; no fallback.
    /// Named agents pass their own explicit pinned choice, including Default.
    pub fn resolve_account_profile(&self, kind: SessionKind,
        choice: Option<crate::account_profiles::ProfileChoice>, parent: Option<SessionId>, workspace: WorkspaceId,
    ) -> anyhow::Result<Option<i64>> {
        if kind != SessionKind::Claude {
            anyhow::ensure!(choice.is_none(), "account_profile is only supported for Claude sessions");
            return Ok(None);
        }
        let inherited = parent.and_then(|id| self.session(id))
            .filter(|s| s.kind == SessionKind::Claude)
            .map(|s| s.account_profile_id);
        let id = choice.map(|c| c.id()).or(inherited)
            .unwrap_or_else(|| self.workspace(workspace).and_then(|w| w.account_profile_id));
        self.check_account_profile(id)?;
        Ok(id)
    }

    pub fn set_workspace_account_profile(&self, id: WorkspaceId, profile: Option<i64>) -> anyhow::Result<Workspace> {
        // Default selection may be saved while the profile needs login, but it must exist
        // and be enabled. Directory metadata and launch environment are checked at launch.
        if let Some(profile) = profile {
            anyhow::ensure!(self.account_profile(profile)?.enabled, "Claude profile is disabled");
        }
        let ws = {
            let mut inner = self.inner.lock();
            let ws = inner.workspaces.get_mut(&id).ok_or_else(|| anyhow::anyhow!("no such workspace: {id}"))?;
            self.store.set_workspace_account_profile(id, profile)?;
            ws.account_profile_id = profile;
            ws.clone()
        };
        self.emit(ServerEvent::WorkspaceUpserted(ws.clone()));
        Ok(ws)
    }

    pub fn workspaces(&self) -> Vec<Workspace> {
        self.inner.lock().workspaces.values().cloned().collect()
    }
    /// Active (non-archived) channels, in sidebar order.
    pub fn channels(&self) -> Vec<Channel> {
        let mut v: Vec<Channel> = self
            .inner
            .lock()
            .channels
            .values()
            .filter(|c| !c.archived)
            .cloned()
            .collect();
        v.sort_by_key(|c| (c.workspace_id, c.sort_order, c.id));
        v
    }
    /// Any channel, including archived ones.
    pub fn channel(&self, id: ChannelId) -> Option<Channel> {
        self.inner.lock().channels.get(&id).cloned()
    }
    pub fn workspace(&self, id: WorkspaceId) -> Option<Workspace> {
        self.inner.lock().workspaces.get(&id).cloned()
    }
    /// All sessions, live and exited (exited ones remain until removed).
    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.inner.lock().sessions.values().map(|e| e.info.clone()).collect()
    }
    pub fn session(&self, id: SessionId) -> Option<SessionInfo> {
        self.inner.lock().sessions.get(&id).map(|e| e.info.clone())
    }
    /// Resolve a per-session token (hooks, MCP) to its session.
    pub fn session_by_token(&self, token: &str) -> Option<SessionId> {
        if token.is_empty() {
            return None;
        }
        self.inner.lock().tokens.get(token).copied()
    }
    /// Live (running) session ids.
    pub fn live_session_ids(&self) -> Vec<SessionId> {
        self.inner
            .lock()
            .sessions
            .values()
            .filter(|e| e.pty.as_ref().is_some_and(|p| p.is_alive()))
            .map(|e| e.info.id)
            .collect()
    }

    /// A prompt queue for this session can still be delivered: its run is alive, or its
    /// launch is prepared and not yet acknowledged (one lock, so an acknowledgement in
    /// between can't make a live run look gone).
    pub(crate) fn may_deliver(&self, id: SessionId) -> bool {
        let g = self.inner.lock();
        let Some(e) = g.sessions.get(&id) else { return false };
        if e.info.state == AgentState::Exited { return false }
        if e.pty.as_ref().is_some_and(|p| p.is_alive()) { return true }
        self.terminal_records.lock().get(&id)
            .is_some_and(|r| r.pending && !r.retired && r.spec.run.generation == e.generation)
    }

    fn pty(&self, id: SessionId) -> Option<Arc<PtySession>> {
        self.inner.lock().sessions.get(&id).and_then(|e| e.pty.clone())
    }

    /// The program run `generation` of session `id` was launched with (None without a
    /// terminal host record, as in some tests).
    pub(crate) fn launched_program(&self, id: SessionId, generation: u64) -> Option<String> {
        self.terminal_records.lock().get(&id)
            .filter(|r| !r.retired && r.spec.run.generation == generation)
            .map(|r| r.spec.program.clone())
    }

    /// The `MUSTER_RUN` marker in run `generation`'s environment: tells its process apart
    /// from an unrelated one that got the same pid later. The terminal host sets it, as
    /// `<host instance>-<session id>-<generation>` (crates/terminal-host `spawn`).
    pub(crate) fn run_marker(&self, id: SessionId, generation: u64) -> Option<String> {
        self.terminal_records.lock().get(&id)
            .filter(|r| !r.retired && r.spec.run.generation == generation)
            .map(|r| format!("{}-{id}-{generation}", r.host_instance))
    }

    /// Record the folder session `id`'s process is in now (`SessionInfo::live_cwd`; None:
    /// unknown, e.g. the folder was deleted), for the run `generation`; tells clients only when
    /// it changed. Not persisted.
    pub(crate) fn note_live_cwd(&self, id: SessionId, generation: u64, cwd: Option<String>) {
        let mut g = self.inner.lock();
        let Some(e) = g.sessions.get_mut(&id).filter(|e| e.generation == generation) else { return };
        if e.info.live_cwd == cwd {
            return;
        }
        e.info.live_cwd = cwd;
        self.publish_session(&e.info);
    }

    /// Broadcast a session's info. Call with `inner` held, right after the change: snapshots
    /// are then published in the order they were made, so a slower writer can never publish
    /// an older snapshot over a newer one (e.g. a hook's state change re-showing a tab
    /// `hide_session` just hid). `emit` never takes a lock.
    fn publish_session(&self, info: &SessionInfo) {
        self.emit(ServerEvent::SessionUpserted(info.clone()));
    }

    /// Capture application state and its exact terminal together. A detector can
    /// query this handle without accidentally reading a replacement run's screen.
    pub(crate) fn detection_terminal(&self, id: SessionId) -> Option<(SessionInfo, Arc<PtySession>)> {
        let g = self.inner.lock();
        let entry = g.sessions.get(&id)?;
        let terminal = entry.pty.as_ref()?;
        (terminal.generation == entry.generation && terminal.is_alive())
            .then(|| (entry.info.clone(), terminal.clone()))
    }

    // ----- session I/O -----

    /// Attribution changes that must be ordered against agent callbacks. Under the callback
    /// lock: drain committed callbacks, then (with hook inserts blocked) commit the human-input
    /// markers recorded so far and run `action`. Pending markers are taken only here, so a
    /// prompt delivery can never clear `human_seen` before a keystroke that preceded it is
    /// marked, and the marker worker can never re-set it afterwards.
    pub(crate) fn input_boundary<T>(&self, action: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut inbox = self.callback_inbox.lock();
        let action = || {
            self.commit_human_input()?;
            action()
        };
        match inbox.as_mut() {
            Some(inbox) => inbox.input_boundary(self, action),
            None => action(),
        }
    }

    /// Keystrokes from a client: the human's input. Records the marker and returns at once;
    /// the keystroke itself never waits on SQLite or the callback lock. The marker is
    /// committed by the next input boundary (the attribution worker runs one promptly, and
    /// every prompt delivery or legacy hook runs one first). Until then, callbacks drained
    /// by the background consumer do not see it: a callback that might predate the keystroke
    /// can never consume it, and one that reacts to it at worst keeps the session's current,
    /// stricter provenance. Each marker carries a sequence number, so a legacy HTTP callback
    /// that arrived before the keystroke can never consume it either (`handle_hook`).
    pub fn note_human_input(&self, id: SessionId) {
        self.human_input.note(id);
    }

    /// Commit every pending human-input marker now (tests, the attribution worker, shutdown).
    pub(crate) fn flush_human_input(&self) -> anyhow::Result<()> {
        if self.human_input.is_empty() {
            return Ok(());
        }
        self.input_boundary(|| Ok(()))
    }

    /// Only call inside `input_boundary`.
    fn commit_human_input(&self) -> anyhow::Result<()> {
        let pending = self.human_input.take();
        if pending.is_empty() {
            return Ok(());
        }
        let live: BTreeMap<SessionId, (u64, u64)> = {
            let g = self.inner.lock();
            pending.iter().filter(|(id, _)| g.sessions.contains_key(id)).map(|(id, marks)| (*id, *marks)).collect()
        };
        if let Err(error) = self.agents.note_human_input_all(&live) {
            self.human_input.restore(pending);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn note_prompt_delivery(&self, id: SessionId, generation: Option<u64>,
        source: crate::agents::Source, now: i64) -> anyhow::Result<()> {
        self.input_boundary(|| {
            let g = self.inner.lock();
            anyhow::ensure!(g.sessions.get(&id).is_some_and(|entry| Some(entry.generation) == generation),
                "terminal generation changed before prompt attribution");
            self.agents.on_delivered(id, source, now)
        })
    }

    /// The session's current terminal handle for input. Never takes `inner` (held across
    /// SQLite commits by hook processing and session updates).
    fn input_handle(&self, id: SessionId) -> Option<Arc<PtySession>> {
        self.input_handles.lock().get(&id).cloned()
    }

    /// Change a session's terminal handle. Call with `inner` held, wherever `SessEntry::pty`
    /// changes, so the input path's lookup always names the same run. A new run starts with
    /// its input open.
    fn set_pty(&self, id: SessionId, entry: &mut SessEntry, pty: Option<Arc<PtySession>>) {
        let mut handles = self.input_handles.lock();
        match &pty {
            Some(pty) => { handles.insert(id, pty.clone()); }
            None => { handles.remove(&id); }
        }
        entry.info.input_interrupted = pty.as_ref().is_some_and(|pty| pty.input_interrupted().is_some());
        entry.pty = pty;
    }

    fn not_running(&self, id: SessionId) -> anyhow::Error {
        // Only on the error path: nothing reaches a terminal here, so waiting is harmless.
        match self.session(id) {
            Some(_) => anyhow::anyhow!("session {id} is not running"),
            None => anyhow::anyhow!("no such session: {id}"),
        }
    }

    /// Raw bytes to the PTY.
    pub fn write_input(&self, id: SessionId, bytes: &[u8]) -> anyhow::Result<()> {
        match self.input_handle(id) {
            Some(p) => p.write(bytes),
            None => Err(self.not_running(id)),
        }
    }

    /// Keystrokes from a client: queued to the PTY in order without waiting on the host.
    pub fn send_keys(&self, id: SessionId, bytes: &[u8]) -> anyhow::Result<()> {
        match self.input_handle(id) {
            Some(p) => p.send_keys(bytes),
            None => Err(self.not_running(id)),
        }
    }

    /// Bind delayed input to the process it was prepared for, even across a concurrent restart.
    pub(crate) fn write_input_generation(&self, id: SessionId, generation: Option<u64>, bytes: &[u8]) -> anyhow::Result<()> {
        let terminal = self.input_handle(id).filter(|terminal| Some(terminal.generation) == generation)
            .context("the terminal run changed before input delivery")?;
        terminal.write(bytes)
    }

    /// Typing into the session's current run is paused (`terminal::InputInterrupted`).
    pub fn input_interrupted(&self, id: SessionId) -> bool {
        self.input_handle(id).is_some_and(|p| p.input_interrupted().is_some())
    }

    /// The backend is stopping and refuses input.
    pub fn input_closed(&self) -> bool {
        self.cfg.terminal_host.as_ref().is_some_and(|host| host.input().is_closed())
    }

    fn save_input_interrupted(&self) {
        let Some(host) = &self.cfg.terminal_host else { return };
        if let Err(error) = self.store.save_runtime(INPUT_INTERRUPTED_KEY, &host.input().interrupted_runs()) {
            tracing::warn!("saving interrupted terminal input: {error:#}");
        }
    }

    /// A run's input was interrupted (lost, refused or dropped input): later input for it is
    /// refused. Show it on the session and tell the user, unless the run is gone anyway.
    pub(crate) fn on_input_interrupted(&self, run: RunId) {
        self.save_input_interrupted();
        let info = {
            let mut g = self.inner.lock();
            let Some(entry) = g.sessions.get_mut(&run.id) else { return };
            let current = entry.pty.as_ref()
                .is_some_and(|p| p.run() == run && p.is_alive() && p.input_interrupted().is_some());
            if !current || entry.info.input_interrupted { return }
            entry.info.input_interrupted = true;
            self.publish_session(&entry.info);
            entry.info.clone()
        };
        tracing::warn!(session = run.id, generation = run.generation, "terminal input interrupted");
        self.notice("error", format!(
            "Some input to \u{201c}{}\u{201d} may not have reached its terminal, so typing there is paused. \
            Check the terminal, then choose Continue typing.", info.name));
    }

    /// The user has checked the terminal after an interruption: accept its input again.
    /// Nothing that was lost is resent.
    pub fn continue_input(self: &Arc<Self>, id: SessionId) -> anyhow::Result<()> {
        anyhow::ensure!(self.session(id).is_some(), "no such session: {id}");
        if let Some(host) = &self.cfg.terminal_host {
            host.input().resume(id);
        }
        self.save_input_interrupted();
        self.update_session_info(id, |info| info.input_interrupted = false)?;
        // Prompts that waited while typing was paused.
        crate::inject::kick_initial(self, id);
        Ok(())
    }

    /// Is the session's terminal in application cursor mode (DECCKM)?
    pub fn application_cursor(&self, id: SessionId) -> bool {
        self.input_handle(id).is_some_and(|p| p.application_cursor())
    }

    /// Generation of the session's current process (changes on every restart), if it has one
    /// this daemon run.
    pub fn pty_generation(&self, id: SessionId) -> Option<u64> {
        self.pty(id).map(|p| p.generation)
    }

    /// Current screen as text, from the headless VT.
    pub fn screen(&self, id: SessionId) -> Option<ScreenSnapshot> {
        if let Some(p) = self.pty(id) {
            return p.screen();
        }
        let info = self.session(id)?;
        pty::log_screen(&self.session_log_path(id), info.cols, info.rows)
    }

    /// Last `lines` lines of output as plain text (scrollback + screen, ANSI stripped).
    pub fn recent_output_text(&self, id: SessionId, lines: usize) -> Option<String> {
        if let Some(p) = self.pty(id) {
            return p.recent_text(lines);
        }
        let info = self.session(id)?;
        pty::log_tail_text(&self.session_log_path(id), info.cols, info.rows, lines)
    }

    fn session_log_path(&self, id: SessionId) -> PathBuf {
        match self.terminal_records.lock().get(&id) {
            Some(record) => crate::terminal::host_log_path(id, record.spec.run.generation),
            None => paths::session_dir(id).join("output.log"),
        }
    }

    /// Unix ms of the last PTY output for a session (0 if none).
    #[allow(dead_code)] // part of the hub's public surface
    pub fn last_output_at(&self, id: SessionId) -> i64 {
        self.pty(id).map(|p| p.last_output_at()).unwrap_or(0)
    }

    /// Increments on every PTY output chunk. Lets pollers skip unchanged sessions.
    pub fn output_seq(&self, id: SessionId) -> u64 {
        self.pty(id).map(|p| p.seq()).unwrap_or(0)
    }

    /// Replay + live subscription for a client attach. For a session without a PTY the replay
    /// is rebuilt from output.log off the async workers, and cached while the log is unchanged.
    /// `max_body`: replay at most the last this many bytes of output (phones, see
    /// `session.attach`'s `replay_bytes`); None = the whole ring.
    pub async fn attach(&self, id: SessionId, max_body: Option<usize>) -> anyhow::Result<Attach> {
        if let Some(p) = self.pty(id) {
            let (replay, offset, rx) = p.attach(max_body)?;
            return Ok(Attach::Live { replay, rx, generation: p.generation, offset });
        }
        let info = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        let path = self.session_log_path(id);
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
        // The whole replay is cached; a client with a limit gets its last lines.
        if let Some((len, mtime)) = stamp
            && let Some((l, m, b)) = self.replay_cache.lock().get(&id)
            && *l == len
            && *m == mtime
        {
            return Ok(Attach::Replay(pty::trim_replay(b, max_body)));
        }
        let (cols, rows) = (info.cols, info.rows);
        let p2 = path.clone();
        let data = tokio::task::spawn_blocking(move || pty::log_replay(&p2, cols, rows)).await?;
        if let Some((len, mtime)) = stamp {
            let mut c = self.replay_cache.lock();
            if c.len() >= REPLAY_CACHE_MAX && !c.contains_key(&id) {
                if let Some(k) = c.keys().next().copied() {
                    c.remove(&k);
                }
            }
            c.insert(id, (len, mtime, Bytes::from(data.clone())));
        }
        Ok(Attach::Replay(pty::trim_replay(&data, max_body)))
    }

    /// This daemon run's replay epoch (docs/PROTOCOL.md `attach_info`).
    pub fn replay_epoch(&self) -> &str {
        self.tails.epoch()
    }

    /// Session `id`'s terminal size revision (see `size_revs`); None while a resize is on its
    /// way to the terminal host (the size a replay is drawn at is unknown then).
    pub fn size_rev(&self, id: SessionId) -> Option<u64> {
        match self.size_revs.lock().get(&id).copied() {
            None => Some(0),
            Some((_, busy)) if busy > 0 => None,
            Some((rev, _)) => Some(rev),
        }
    }

    /// Whether session `id`'s current run is still running, asked of the terminal host (the
    /// cached status can lag an exit). A run that ended replays differently (an ended
    /// full-screen view moves into history), so it never gets a `delta`.
    pub fn pty_alive(&self, id: SessionId) -> bool {
        self.pty(id).is_some_and(|p| p.alive_now())
    }

    /// Keep a raw output tail of session `id`'s current run for a `delta` replay later, and
    /// count a client as attached through it until the returned use is dropped (src/replay_tail.rs).
    /// Call before taking the client's replay. None without a live run or a free tail.
    pub fn join_tail(&self, id: SessionId) -> Option<crate::replay_tail::TailUse> {
        let p = self.pty(id)?;
        self.tails.join(id, p.generation, 0, move || {
            // The host's smallest replay: the tail only needs the stream and its offset.
            let (_, offset, rx) = p.attach(Some(4096))?;
            Ok((offset, rx))
        })
    }

    /// Apply a state change with precedence rules:
    /// - `Exited` is terminal.
    /// - A `Screen` update doesn't override a `Hook` update less than 1.5s old, unless it
    ///   reports NeedsApproval/AskingQuestion (prompts are visible before hooks may fire).
    /// - No-op if state and detail are unchanged.
    /// On change: updates SessionInfo, broadcasts SessionUpserted, logs a StateChange event.
    /// Returns true if the state changed.
    /// Optionally require the process generation under the same lock as the mutation.
    pub(crate) fn set_state_checked(
        &self,
        id: SessionId,
        update: StateUpdate,
        source: StateSource,
        generation: Option<u64>,
    ) -> bool {
        let now = now_ms();
        let (from, info) = {
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id) else {
                return false;
            };
            if generation.is_some_and(|gen_| gen_ != e.generation) {
                return false;
            }
            if e.info.state == AgentState::Exited {
                return false;
            }
            if source == StateSource::Hook {
                e.last_hook_at = now;
            }
            if source == StateSource::Screen
                && now - e.last_hook_at < HOOK_GRACE_MS
                && !matches!(update.state, AgentState::NeedsApproval | AgentState::AskingQuestion)
            {
                return false;
            }
            if e.info.state == update.state && e.info.state_detail == update.detail {
                return false;
            }
            let next_seq = match self.state_seq.next() {
                Ok(seq) => seq,
                Err(error) => {
                    tracing::error!(session = id, "cannot reserve prompt identity: {error:#}");
                    return false;
                }
            };
            let from = e.info.state;
            if from != update.state {
                e.info.state_since = now;
            }
            e.info.state_seq = next_seq;
            e.info.state = update.state;
            e.info.state_detail = update.detail.clone();
            // Only what the screen really showed moves the cache clock, and only for a CLI that
            // sends no hooks (they say when a request succeeded): never a timer's Stuck, a
            // process exit, or leaving the Working that recovery put there.
            let seen = source == StateSource::Screen && e.last_hook_at == 0
                && !(from == AgentState::Working && e.unverified_working);
            if from != update.state {
                e.unverified_working = false;
            }
            if seen && crate::cache::note_screen(&mut e.info, from, update.state, now) {
                self.cache_rules.lock().refresh(&mut e.info);
            }
            if let Err(err) = self.store.update_session(&e.info) {
                tracing::warn!(session = id, "persisting session: {err}");
            }
            self.publish_session(&e.info);
            (from, e.info.clone())
        };
        let channel_id = info.channel_id;
        if from != update.state {
            self.log_event(
                Some(channel_id),
                Some(id),
                EventKind::StateChange,
                serde_json::json!({
                    "session_id": id,
                    "from": from,
                    "to": update.state,
                    "detail": update.detail,
                    "source": source,
                }),
            );
        }
        true
    }

    /// Take the right to answer the prompt a caller saw: refused unless the session's
    /// `state_seq` (and `state_since`, when given) still match; on success `state_seq` moves on,
    /// so a second answer to the same prompt is refused too. Checked and bumped under one lock.
    pub fn claim_prompt(&self, id: SessionId, seq: Option<u64>, since: Option<i64>) -> anyhow::Result<()> {
        {
            let mut g = self.inner.lock();
            let e = g
                .sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            if seq.is_some_and(|s| s != e.info.state_seq) || since.is_some_and(|s| s != e.info.state_since) {
                anyhow::bail!("stale: session {id}'s prompt changed since you looked; check it again");
            }
            if seq.is_none() {
                return Ok(());
            }
            e.info.state_seq = self.state_seq.next()?;
            self.publish_session(&e.info);
        }
        Ok(())
    }

    /// Unix ms of the last delivery out of a session's injection queue (for `session.wait`).
    pub fn last_dequeue_at(&self, id: SessionId) -> Option<i64> {
        self.inner
            .lock()
            .sessions
            .get(&id)
            .map(|e| e.last_dequeue_at)
            .filter(|t| *t > 0)
    }

    /// Update the injection-queue length shown in the UI.
    pub fn set_queued(&self, id: SessionId, queued: u32) {
        {
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id) else {
                return;
            };
            if e.info.queued == queued {
                return;
            }
            if queued < e.info.queued {
                e.last_dequeue_at = now_ms();
            }
            e.info.queued = queued;
            self.publish_session(&e.info);
        }
    }

    /// Tests: put a restored (exited) session in another state without a terminal.
    #[cfg(test)]
    pub(crate) fn set_state_for_test(&self, id: SessionId, state: AgentState) {
        if let Some(e) = self.inner.lock().sessions.get_mut(&id) { e.info.state = state; }
    }

    /// Mutate a session's info under the lock, persist, broadcast.
    fn update_session_info(&self, id: SessionId, f: impl FnOnce(&mut SessionInfo)) -> anyhow::Result<SessionInfo> {
        self.update_session_info_checked(id, None, f)
    }

    fn update_session_info_checked(
        &self,
        id: SessionId,
        generation: Option<u64>,
        f: impl FnOnce(&mut SessionInfo),
    ) -> anyhow::Result<SessionInfo> {
        let info = {
            let mut g = self.inner.lock();
            let e = g
                .sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            if generation.is_some_and(|gen_| gen_ != e.generation) {
                anyhow::bail!("session {id} was restarted");
            }
            f(&mut e.info);
            if let Err(err) = self.store.update_session(&e.info) {
                tracing::warn!(session = id, "persisting session: {err}");
            }
            self.publish_session(&e.info);
            e.info.clone()
        };
        Ok(info)
    }

    /// Mutate a channel under the lock, persist, optionally broadcast.
    fn update_channel(&self, id: ChannelId, emit: bool, f: impl FnOnce(&mut Channel)) -> anyhow::Result<Channel> {
        let ch = {
            let mut g = self.inner.lock();
            let c = g
                .channels
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such channel: {id}"))?;
            let mut next = c.clone();
            f(&mut next);
            self.store.update_channel(&next)?;
            *c = next.clone();
            next
        };
        if emit {
            self.emit(ServerEvent::ChannelUpserted(ch.clone()));
        }
        Ok(ch)
    }

    // ----- PTY callbacks -----

    pub(crate) fn pty_title_changed(&self, id: SessionId, generation: u64, title: Option<String>) {
        let Ok(info) = self.update_session_info_checked(id, Some(generation), |i| i.title = title) else {
            return;
        };
        let Some(clean) = info.title.as_deref().and_then(|t| clean_title(t, info.kind)) else {
            return;
        };
        // The channel as of this lock: the session may have moved since `info` was read.
        let cid = {
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id) else {
                return;
            };
            e.usable_title = Some(clean.clone());
            let cid = e.info.channel_id;
            let t = g.titles.entry(cid).or_default();
            let slot = match info.kind {
                SessionKind::Shell => &mut t.shell,
                SessionKind::Claude | SessionKind::Codex => &mut t.agent,
            };
            if slot.as_deref() == Some(clean.as_str()) {
                return;
            }
            *slot = Some(clean);
            cid
        };
        self.rederive_name(cid);
    }

    pub(crate) fn pty_exited(&self, id: SessionId, generation: u64, code: i32) {
        let (channel_id, kind, opened, resumed) = {
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id) else {
                return;
            };
            if e.generation != generation {
                return;
            }
            if e.pty.is_none() {
                e.early_exit = Some(code);
                return;
            }
            e.info.exit_code = Some(code);
            e.info.pid = None;
            e.info.queued = 0;
            e.info.subagents = 0;
            // The conversation this run was launched to reopen; not for a kill (the run a
            // restart replaces, or a shutdown).
            let resumed = (code != 0 && e.killed_generation != Some(generation) && !e.kill_requested
                && !self.shutting_down.load(Ordering::SeqCst))
                .then(|| self.launched_resume(id, e.info.kind)).flatten();
            let token = e.token.take();
            let (ch, kind, opened) = (e.info.channel_id, e.info.kind, e.opened);
            if let Some(t) = token {
                g.tokens.remove(&t);
            }
            (ch, kind, opened, resumed)
        };
        // Read outside the lock: the run's last output says whether the conversation is gone.
        let failed_resume = resumed.is_some_and(|resumed| {
            conversation_missing(kind, code, opened, resumed.as_deref(), self.recent_output_text(id, 40).as_deref())
                && self.forget_failed_resume(id, generation, resumed.as_deref())
        });
        crate::inject::drop_queue(self, id);
        let detail = if self.shutting_down.load(Ordering::SeqCst) {
            "daemon stopped".to_string()
        } else if failed_resume {
            // Said in the tab: the CLI printed why (e.g. "No conversation found") above.
            failed_resume_detail(code)
        } else {
            format!("exit code {code}")
        };
        if !self.set_state_checked(
            id,
            StateUpdate::with(AgentState::Exited, detail),
            StateSource::Process,
            Some(generation),
        ) {
            // Still persist exit code / pid even if the state didn't change.
            if self.update_session_info_checked(id, Some(generation), |_| {}).is_err() {
                return;
            }
        }
        self.log_event(
            Some(channel_id),
            Some(id),
            EventKind::SessionExited,
            serde_json::json!({ "session_id": id, "exit_code": code }),
        );
    }

    // ----- mutations -----

    pub async fn add_workspace(self: &Arc<Self>, path: String, name: Option<String>) -> anyhow::Result<Workspace> {
        let raw = path.trim();
        if raw.is_empty() {
            anyhow::bail!("path is empty");
        }
        let expanded = paths::expand_tilde(raw);
        let canon = tokio::fs::canonicalize(&expanded)
            .await
            .map_err(|e| anyhow::anyhow!("{}: {e}", expanded.display()))?;
        if !canon.is_dir() {
            anyhow::bail!("{} is not a directory", canon.display());
        }
        let path_s = canon.to_string_lossy().to_string();
        if let Some(w) = self.inner.lock().workspaces.values().find(|w| w.path == path_s) {
            return Ok(w.clone());
        }
        let is_git = worktree::is_git_repo(&canon).await;
        let name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| canon.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| path_s.clone());
        let created_at = now_ms();
        let ws = {
            let mut g = self.inner.lock();
            if let Some(w) = g.workspaces.values().find(|w| w.path == path_s) {
                return Ok(w.clone());
            }
            let id = match self.store.workspace_by_path(&path_s)? {
                Some(id) => id,
                None => self.store.insert_workspace(&name, &path_s, is_git, created_at)?,
            };
            let ws = Workspace {
                account_profile_id: None,
                id,
                name,
                path: path_s,
                is_git,
                created_at,
            };
            g.workspaces.insert(id, ws.clone());
            ws
        };
        self.emit(ServerEvent::WorkspaceUpserted(ws.clone()));
        Ok(ws)
    }

    /// Archives the workspace's channels (killing their sessions, keeping worktrees) and
    /// forgets the workspace.
    pub async fn remove_workspace(self: &Arc<Self>, id: WorkspaceId) -> anyhow::Result<()> {
        let chans: Vec<ChannelId> = {
            let g = self.inner.lock();
            if !g.workspaces.contains_key(&id) {
                anyhow::bail!("no such workspace: {id}");
            }
            g.channels
                .values()
                .filter(|c| c.workspace_id == id && !c.archived)
                .map(|c| c.id)
                .collect()
        };
        for c in chans {
            if let Err(e) = self.archive_channel(c, false).await {
                tracing::warn!(channel = c, "archiving: {e}");
            }
        }
        self.inner.lock().workspaces.remove(&id);
        self.store.delete_workspace(id)?;
        self.emit(ServerEvent::WorkspaceRemoved { id });
        Ok(())
    }

    pub async fn create_channel(self: &Arc<Self>, req: CreateChannel) -> anyhow::Result<Channel> {
        self.create_channel_with_origin(req, false).await
    }

    pub(crate) async fn create_mcp_channel(self: &Arc<Self>, req: CreateChannel) -> anyhow::Result<Channel> {
        self.create_channel_with_origin(req, true).await
    }

    async fn create_channel_with_origin(self: &Arc<Self>, req: CreateChannel, mcp: bool) -> anyhow::Result<Channel> {
        self.ensure_not_shutting_down()?;
        let ws = self
            .workspace(req.workspace_id)
            .ok_or_else(|| anyhow::anyhow!("no such workspace: {}", req.workspace_id))?;
        // Validate and pin before creating any channel or worktree.
        let account_profile = match req.agent {
            Some(kind) => self.resolve_account_profile(kind, req.account_profile, req.from_session, ws.id)?,
            None => {
                anyhow::ensure!(req.account_profile.is_none(), "account_profile needs a Claude agent");
                None
            }
        };
        if let Some(w) = req.window_id
            && !self.inner.lock().windows.contains_key(&w)
        {
            anyhow::bail!("no such window: {w}");
        }
        match req.agent {
            None if !req.extra_args.is_empty() => anyhow::bail!("extra_args needs an agent"),
            Some(kind) => crate::adapters::check_extra_args(kind, &req.extra_args)?,
            None => {}
        }
        let explicit = req
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let prompt = req.prompt.clone().filter(|p| !p.trim().is_empty());
        let first_prompt = match explicit {
            Some(_) => None,
            None => prompt.as_deref().and_then(prompt_name),
        };
        let (name, derived) = match &explicit {
            Some(n) => (n.clone(), NameSource::Folder),
            None => derive_name(None, first_prompt.as_deref(), None, &folder_name(&ws)),
        };
        let repo = PathBuf::from(&ws.path);
        let now = now_ms();
        // The row goes in first (archived until it's ready) because the worktree is keyed by
        // the channel id.
        let mut ch = Channel {
            id: 0,
            workspace_id: ws.id,
            name,
            cwd: ws.path.clone(),
            branch: None,
            base_branch: None,
            owns_worktree: false,
            port_base: 0,
            archived: true,
            layout: None,
            created_at: now,
            last_read_at: now,
            sort_order: 0,
            permissions: req.permissions.unwrap_or_default(),
            window_id: None,
            auto_named: explicit.is_none(),
            first_prompt,
        };
        ch.id = self.store.insert_channel_with_origin(&ch, mcp)?;
        if ws.is_git && req.worktree {
            let root = paths::worktrees_dir().join(format!("{}-{}", worktree::slugify(&ws.name), ws.id));
            match worktree::create(&repo, &root, &ch.id.to_string(), req.base.as_deref()).await {
                Ok(wt) => {
                    ch.cwd = wt.path.to_string_lossy().to_string();
                    ch.base_branch = Some(wt.base);
                    ch.owns_worktree = true;
                    // Named up front (explicitly, or from the prompt): the branch gets that name
                    // right away, which counts as its one derived-name rename.
                    let mut branch = wt.branch;
                    if explicit.is_some() || derived != NameSource::Folder {
                        match worktree::rename_branch(&wt.path, &branch, &worktree::slugify(&ch.name)).await {
                            Ok(Some(n)) => branch = n,
                            Ok(None) => {}
                            Err(e) => tracing::warn!(channel = ch.id, "renaming branch {branch}: {e:#}"),
                        }
                    }
                    ch.branch = Some(branch);
                }
                Err(e) => {
                    let _ = self.store.delete_channel(ch.id);
                    return Err(e);
                }
            }
        } else if ws.is_git {
            ch.branch = worktree::current_head(&repo).await.ok();
        }
        let mut events = Vec::new();
        let inserted = (|| -> anyhow::Result<Channel> {
            let mut g = self.inner.lock();
            if !g.workspaces.contains_key(&ws.id) {
                anyhow::bail!("workspace {} was removed", ws.id);
            }
            let wid = self.place_locked(&mut g, req.window_id, req.prefer_window, &mut events)?;
            ch.window_id = Some(wid);
            ch.sort_order = next_tab_order(&g, wid);
            ch.archived = false;
            // Preferred block derives from the id; if an active channel already uses it
            // (ids 1000 apart), take the next free block.
            // Blocks of live channels, and of running processes (a session moved out of a closed
            // channel still uses the block it started with).
            let used: HashSet<u16> = g
                .channels
                .values()
                .filter(|c| !c.archived)
                .map(|c| c.port_base)
                .chain({
                    let records = self.terminal_records.lock();
                    let used: Vec<u16> = g.sessions.values().filter(|e| e.port_base != 0 && occupies(e, &records))
                        .map(|e| e.port_base)
                        // A removed session's run may still be exiting.
                        .chain(records.values().filter(|r| r.retired && r.port_base != 0).map(|r| r.port_base))
                        .collect();
                    used
                })
                .collect();
            let pref = port_base_for(ch.id);
            ch.port_base = (0..1000u16)
                .map(|k| 20000 + ((pref - 20000) / 10 + k) % 1000 * 10)
                .find(|b| !used.contains(b))
                .unwrap_or(pref);
            self.store.update_channel(&ch)?;
            g.channels.insert(ch.id, ch.clone());
            if derived != NameSource::Folder {
                g.branch_named_from.insert(ch.id, derived);
            }
            events.push(ServerEvent::ChannelUpserted(ch.clone()));
            if let Some(w) = window_view(&g, wid) {
                events.push(ServerEvent::WindowUpserted(w));
            }
            Ok(ch.clone())
        })();
        let ch = match inserted {
            Ok(ch) => ch,
            Err(e) => {
                let _ = self.store.delete_channel(ch.id);
                if ch.owns_worktree {
                    let _ = worktree::remove(&repo, Path::new(&ch.cwd)).await;
                }
                return Err(e);
            }
        };
        for ev in events {
            self.emit(ev);
        }
        if let Some(kind) = req.agent {
            let spawn = SpawnSession {
                account_profile: (kind == SessionKind::Claude).then(|| crate::account_profiles::ProfileChoice::pinned(account_profile)),
                channel_id: ch.id,
                kind,
                name: None,
                cwd: None,
                prompt,
                extra_args: req.extra_args,
                cols: req.cols,
                rows: req.rows,
                parent_id: None,
                // The channel's window (just placed above).
                window_id: None,
                from_session: req.from_session,
                prov: req.prov,
                agent_id: None,
            };
            if let Err(e) = self.spawn_session(spawn).await {
                let text = format!("Could not start {}: {e:#}", kind.as_str());
                self.notice("error", text.clone());
                self.log_event(
                    Some(ch.id),
                    None,
                    EventKind::System,
                    serde_json::json!({"text": text, "level": "error"}),
                );
            }
        }
        Ok(ch)
    }

    /// Explicit rename: ends auto naming, and renames a muster-owned worktree branch without
    /// upstream to `muster/<slug>` (never fatal).
    pub async fn rename_channel(self: &Arc<Self>, id: ChannelId, name: String) -> anyhow::Result<Channel> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("channel name is empty");
        }
        let ch = self.update_channel(id, true, |c| {
            c.name = name;
            c.auto_named = false;
        })?;
        if ch.owns_worktree && !ch.archived {
            self.rename_branch_now(id).await;
        }
        Ok(self.channel(id).unwrap_or(ch))
    }

    // ----- auto naming -----

    /// The first prompt sent to a session of an auto-named channel names it (priority 2).
    pub fn note_prompt(&self, session_id: SessionId, text: &str) {
        let Some(cid) = self.session(session_id).map(|s| s.channel_id) else {
            return;
        };
        if !self
            .channel(cid)
            .is_some_and(|c| c.auto_named && !c.archived && c.first_prompt.is_none())
        {
            return;
        }
        let Some(p) = prompt_name(text) else {
            return;
        };
        let _ = self.update_channel(cid, false, |c| {
            if c.first_prompt.is_none() {
                c.first_prompt = Some(p);
            }
        });
        self.rederive_name(cid);
    }

    /// Recompute an auto-named channel's name from its inputs; emits only on change.
    fn rederive_name(&self, id: ChannelId) {
        // Derived and applied under one lock, so a slower update can't overwrite a newer one.
        let (ch, changed, derived) = {
            let mut g = self.inner.lock();
            let Some(c) = g.channels.get(&id).filter(|c| c.auto_named && !c.archived) else {
                return;
            };
            let t = g.titles.get(&id);
            let folder = g
                .workspaces
                .get(&c.workspace_id)
                .map(folder_name)
                .unwrap_or_else(|| c.name.clone());
            let (name, derived) = derive_name(
                t.and_then(|t| t.agent.as_deref()),
                c.first_prompt.as_deref(),
                t.and_then(|t| t.shell.as_deref()),
                &folder,
            );
            let changed = name != c.name;
            let mut next = c.clone();
            if changed {
                next.name = name;
                if let Err(e) = self.store.update_channel(&next) {
                    tracing::warn!(channel = id, "persisting derived name: {e}");
                    return;
                }
                g.channels.insert(id, next.clone());
            }
            (next, changed, derived)
        };
        if changed {
            self.emit(ServerEvent::ChannelUpserted(ch.clone()));
        }
        // Also when the derived name equals the folder name: the branch still gets named.
        if derived != NameSource::Folder {
            self.after_derived_name(&ch, derived);
        }
    }

    /// A derived name renames the worktree's branch: the initial `muster/<id>` one, and again
    /// when a better source takes over (a shell title first, then the agent's own title), so
    /// the branch ends up named after what the tab is about. Same-source updates (an agent
    /// retitling itself) don't rename it. The upstream check is `worktree::rename_branch`'s.
    fn after_derived_name(&self, ch: &Channel, source: NameSource) {
        if !ch.owns_worktree || ch.branch.is_none() {
            return;
        }
        {
            let mut g = self.inner.lock();
            let initial = ch.branch.as_deref().is_some_and(|b| is_initial_branch(b, ch.id));
            let better = match g.branch_named_from.get(&ch.id) {
                Some(prev) => source > *prev,
                None => initial,
            };
            if !better {
                return;
            }
            g.branch_named_from.insert(ch.id, source);
        }
        let (Some(hub), Some(rt)) = (
            self.weak.upgrade(),
            tokio::runtime::Handle::try_current().ok().or_else(|| self.rt.clone()),
        ) else {
            return;
        };
        let id = ch.id;
        rt.spawn(async move { hub.rename_branch_now(id).await });
    }

    /// Rename the channel's branch after its current name, if muster owns the worktree and the
    /// branch has no upstream. Failures are logged.
    async fn rename_branch_now(&self, id: ChannelId) {
        let _serial = self.branch_lock.lock().await;
        let Some(ch) = self.channel(id).filter(|c| c.owns_worktree && !c.archived) else {
            return;
        };
        let Some(old) = ch.branch.clone() else {
            return;
        };
        let slug = worktree::slugify(&ch.name);
        match worktree::rename_branch(Path::new(&ch.cwd), &old, &slug).await {
            Ok(Some(new)) => {
                let _ = self.update_channel(id, true, |c| {
                    if c.branch.as_deref() == Some(old.as_str()) {
                        c.branch = Some(new);
                    }
                });
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(channel = id, "renaming branch {old}: {e:#}"),
        }
    }

    // ----- windows -----

    pub fn windows(&self) -> Vec<Window> {
        let g = self.inner.lock();
        g.windows.keys().filter_map(|id| window_view(&g, *id)).collect()
    }

    pub fn window(&self, id: WindowId) -> Option<Window> {
        window_view(&self.inner.lock(), id)
    }

    /// Add a window row (not held). `id` must be free.
    fn add_window_locked(&self, g: &mut Inner, id: WindowId, name: Option<String>) -> anyhow::Result<()> {
        let now = now_ms();
        let name = clean_window_name(name).unwrap_or_else(|| id.to_string());
        self.store.upsert_window(id, &name, 0, now)?;
        g.windows.insert(
            id,
            WinEntry {
                name,
                last_focused_at: 0,
                created_at: now,
                holder: None,
                layout: None,
            },
        );
        Ok(())
    }

    /// Where a new channel goes: `requested` (must exist); else `prefer` if it exists; else the
    /// open window focused last; else the lowest-id window; else a new window.
    fn place_locked(
        &self,
        g: &mut Inner,
        requested: Option<WindowId>,
        prefer: Option<WindowId>,
        events: &mut Vec<ServerEvent>,
    ) -> anyhow::Result<WindowId> {
        if let Some(id) = requested {
            if g.windows.contains_key(&id) {
                return Ok(id);
            }
            anyhow::bail!("no such window: {id}");
        }
        if let Some(id) = prefer.filter(|id| g.windows.contains_key(id)) {
            return Ok(id);
        }
        let focused = g
            .windows
            .iter()
            .filter(|(_, w)| w.holder.is_some())
            .max_by_key(|(id, w)| (w.last_focused_at, std::cmp::Reverse(**id)))
            .map(|(id, _)| *id);
        if let Some(id) = focused.or_else(|| g.windows.keys().next().copied()) {
            return Ok(id);
        }
        let id = free_window_id(g);
        self.add_window_locked(g, id, None)?;
        if let Some(w) = window_view(g, id) {
            events.push(ServerEvent::WindowUpserted(w));
        }
        Ok(id)
    }

    /// `window.create`: a new window nobody holds yet.
    pub fn create_window(&self, name: Option<String>) -> anyhow::Result<Window> {
        let w = {
            let mut g = self.inner.lock();
            let id = free_window_id(&g);
            self.add_window_locked(&mut g, id, name)?;
            window_view(&g, id).ok_or_else(|| anyhow::anyhow!("window {id} vanished"))?
        };
        self.emit(ServerEvent::WindowUpserted(w.clone()));
        Ok(w)
    }

    /// `window.claim` for connection `conn`: with `id`, that window (created if missing);
    /// else the lowest-id window with tabs (sessions) that nobody holds, else a new one.
    /// Releases the connection's previous window and adopts channels and sessions without a
    /// (live) window.
    pub fn claim_window(&self, conn: u64, id: Option<WindowId>) -> anyhow::Result<Window> {
        let mut events = Vec::new();
        let w = {
            let mut g = self.inner.lock();
            let target = match id {
                Some(id) if id < 1 => anyhow::bail!("invalid window id: {id}"),
                Some(id) => {
                    if !g.windows.contains_key(&id) {
                        self.add_window_locked(&mut g, id, None)?;
                    }
                    id
                }
                None => {
                    let free = g
                        .windows
                        .iter()
                        .find(|(wid, w)| w.holder.is_none() && (has_sessions(&g, **wid)
                            || self.store.load_panels().is_ok_and(|ps| ps.iter().any(|p| !p.closed && p.window_id == Some(**wid)))))
                        .map(|(wid, _)| *wid);
                    match free {
                        Some(wid) => wid,
                        None => {
                            let wid = free_window_id(&g);
                            self.add_window_locked(&mut g, wid, None)?;
                            wid
                        }
                    }
                }
            };
            let prev: Vec<WindowId> = g
                .windows
                .iter()
                .filter(|(wid, w)| w.holder == Some(conn) && **wid != target)
                .map(|(wid, _)| *wid)
                .collect();
            if let Some(w) = g.windows.get_mut(&target) {
                w.holder = Some(conn);
            }
            g.launching.remove(&target);
            // Adopt orphans: live channels whose window is unset or gone, except a hidden agent's
            // task (every one of its tabs hidden: `hide_session`).
            let all_hidden = |cid: ChannelId| {
                let mut tabs = g.sessions.values().filter(|e| e.info.channel_id == cid).peekable();
                tabs.peek().is_some() && tabs.all(|e| e.info.hidden)
            };
            let mut orphans: Vec<(i64, ChannelId)> = g
                .channels
                .values()
                .filter(|c| !c.archived && c.window_id.is_none_or(|w| !g.windows.contains_key(&w)))
                .filter(|c| !all_hidden(c.id))
                .map(|c| (c.sort_order, c.id))
                .collect();
            orphans.sort();
            let mut order = next_tab_order(&g, target);
            for (_, cid) in orphans {
                let Some(c) = g.channels.get(&cid) else { continue };
                let mut next = c.clone();
                next.window_id = Some(target);
                next.sort_order = order;
                order += 1;
                if let Err(e) = self.store.update_channel(&next) {
                    tracing::warn!(channel = cid, "adopting into window {target}: {e}");
                    continue;
                }
                g.channels.insert(cid, next.clone());
                events.push(ServerEvent::ChannelUpserted(next));
            }
            let orphans: Vec<SessionId> = g
                .sessions
                .values()
                .filter(|e| !e.info.hidden && e.info.window_id.is_none_or(|w| !g.windows.contains_key(&w)))
                .map(|e| e.info.id)
                .collect();
            for sid in orphans {
                let Some(e) = g.sessions.get_mut(&sid) else { continue };
                let mut next = e.info.clone();
                next.window_id = Some(target);
                if let Err(err) = self.store.update_session(&next) {
                    tracing::warn!(session = sid, "adopting into window {target}: {err}");
                    continue;
                }
                e.info = next.clone();
                events.push(ServerEvent::SessionUpserted(next));
            }
            for p in prev {
                self.release_locked(&mut g, p, &mut events);
            }
            // Under the lock: the adopted sessions' snapshots go out in the order they changed.
            for ev in events.drain(..) {
                self.emit(ev);
            }
            window_view(&g, target).ok_or_else(|| anyhow::anyhow!("window {target} vanished"))?
        };
        crate::panels::adopt_orphans(self, w.id)?;
        self.emit(ServerEvent::WindowUpserted(w.clone()));
        Ok(w)
    }

    /// A connection closed: release its window (deleted when it has no tabs).
    pub fn release_windows(&self, conn: u64) {
        let mut events = Vec::new();
        {
            let mut g = self.inner.lock();
            let held: Vec<WindowId> = g
                .windows
                .iter()
                .filter(|(_, w)| w.holder == Some(conn))
                .map(|(id, _)| *id)
                .collect();
            for id in held {
                self.release_locked(&mut g, id, &mut events);
            }
        }
        for ev in events {
            self.emit(ev);
        }
    }

    fn release_locked(&self, g: &mut Inner, id: WindowId, events: &mut Vec<ServerEvent>) {
        if let Some(w) = g.windows.get_mut(&id) {
            w.holder = None;
        }
        self.gc_window_locked(g, id, events);
    }

    /// After a window lost a task or its holder: delete it if nobody holds it and it has no
    /// tabs (sessions) or tasks, else report its new state.
    fn gc_window_locked(&self, g: &mut Inner, id: WindowId, events: &mut Vec<ServerEvent>) {
        if !self.drop_if_unused_locked(g, id, events)
            && let Some(v) = window_view(g, id)
        {
            events.push(ServerEvent::WindowUpserted(v));
        }
    }

    pub(crate) fn panel_left_window(&self, id: WindowId) {
        let mut events = Vec::new();
        self.drop_if_unused_locked(&mut self.inner.lock(), id, &mut events);
        for event in events { self.emit(event); }
    }

    /// After a window lost a tab (session): delete it if nobody holds it and it has no tabs or
    /// tasks left. Returns whether it was deleted. (Its `Window` view is unchanged otherwise.)
    fn drop_if_unused_locked(&self, g: &mut Inner, id: WindowId, events: &mut Vec<ServerEvent>) -> bool {
        if !g.windows.get(&id).is_some_and(|w| w.holder.is_none()) || !window_unused(g, id)
            || self.store.load_panels().map_or(true, |ps| ps.iter().any(|p| !p.closed && p.window_id == Some(id))) {
            return false;
        }
        match self.store.delete_window(id) {
            Err(e) => tracing::warn!(window = id, "deleting window: {e}"),
            // Its notes closed with it; tell clients (the store is read directly: `notes::get`
            // would take `inner`, held here).
            Ok(notes) => events.extend(notes.into_iter()
                .filter_map(|n| self.store.load_note(n).ok().flatten()).map(ServerEvent::NoteUpserted)),
        }
        g.windows.remove(&id);
        events.push(ServerEvent::WindowRemoved { id });
        true
    }

    /// `window.set_layout`: store the frontend's split layout (null clears it). No event: the
    /// window that saved it already has it, and echoing it would fight concurrent edits.
    pub fn set_window_layout(&self, id: WindowId, layout: serde_json::Value) -> anyhow::Result<()> {
        let layout = if layout.is_null() { None } else { Some(layout) };
        let mut g = self.inner.lock();
        let w = g
            .windows
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("no such window: {id}"))?;
        self.store.set_window_layout(id, layout.as_ref())?;
        w.layout = layout;
        Ok(())
    }

    /// Mutate a window under the lock, persist, broadcast.
    fn update_window(&self, id: WindowId, f: impl FnOnce(&mut WinEntry)) -> anyhow::Result<Window> {
        let w = {
            let mut g = self.inner.lock();
            let e = g
                .windows
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such window: {id}"))?;
            f(e);
            self.store.upsert_window(id, &e.name, e.last_focused_at, e.created_at)?;
            window_view(&g, id).ok_or_else(|| anyhow::anyhow!("no such window: {id}"))?
        };
        self.emit(ServerEvent::WindowUpserted(w.clone()));
        Ok(w)
    }

    pub fn window_focused(&self, id: WindowId) -> anyhow::Result<Window> {
        let now = now_ms();
        self.update_window(id, |w| w.last_focused_at = w.last_focused_at.max(now))
    }

    pub fn rename_window(&self, id: WindowId, name: String) -> anyhow::Result<Window> {
        let name = clean_window_name(Some(name)).ok_or_else(|| anyhow::anyhow!("window name is empty"))?;
        self.update_window(id, |w| w.name = name)
    }

    /// Set a window's tab order. Ids not in the window are ignored; tabs left out keep their
    /// relative order after the listed ones.
    pub fn reorder_tabs(&self, id: WindowId, ids: &[ChannelId]) -> anyhow::Result<Window> {
        let w = {
            let mut g = self.inner.lock();
            if !g.windows.contains_key(&id) {
                anyhow::bail!("no such window: {id}");
            }
            let current = tab_ids(&g, id);
            let mut order: Vec<ChannelId> = Vec::with_capacity(current.len());
            for c in ids.iter().chain(current.iter()) {
                if current.contains(c) && !order.contains(c) {
                    order.push(*c);
                }
            }
            self.renumber_locked(&mut g, &order)?;
            window_view(&g, id).ok_or_else(|| anyhow::anyhow!("no such window: {id}"))?
        };
        self.emit(ServerEvent::WindowUpserted(w.clone()));
        Ok(w)
    }

    /// Give `order`'s channels sort_order 0, 1, ... (persisting only changes).
    fn renumber_locked(&self, g: &mut Inner, order: &[ChannelId]) -> anyhow::Result<()> {
        for (i, cid) in order.iter().enumerate() {
            let Some(c) = g.channels.get_mut(cid) else { continue };
            if c.sort_order != i as i64 {
                let mut next = c.clone();
                next.sort_order = i as i64;
                self.store.update_channel(&next)?;
                *c = next;
            }
        }
        Ok(())
    }

    /// Move a task and all its sessions (tabs) to another window, at `index` among the window's
    /// tasks (default: the end).
    pub fn move_channel(&self, id: ChannelId, window_id: WindowId, index: Option<usize>) -> anyhow::Result<Channel> {
        let mut events = Vec::new();
        let ch = {
            let mut g = self.inner.lock();
            let c = g
                .channels
                .get(&id)
                .filter(|c| !c.archived)
                .ok_or_else(|| anyhow::anyhow!("no such channel: {id}"))?;
            if !g.windows.contains_key(&window_id) {
                anyhow::bail!("no such window: {window_id}");
            }
            let old = c.window_id;
            let mut next = c.clone();
            next.window_id = Some(window_id);
            self.store.update_channel(&next)?;
            g.channels.insert(id, next);
            let mut order: Vec<ChannelId> = tab_ids(&g, window_id).into_iter().filter(|c| *c != id).collect();
            order.insert(index.unwrap_or(order.len()).min(order.len()), id);
            self.renumber_locked(&mut g, &order)?;
            let ch = g.channels.get(&id).cloned().ok_or_else(|| anyhow::anyhow!("no such channel: {id}"))?;
            events.push(ServerEvent::ChannelUpserted(ch.clone()));
            let mut left: BTreeSet<WindowId> = BTreeSet::new();
            let sids: Vec<SessionId> = g
                .sessions
                .values()
                .filter(|e| e.info.channel_id == id && (e.info.window_id != Some(window_id) || e.info.hidden))
                .map(|e| e.info.id)
                .collect();
            for sid in sids {
                let Some(e) = g.sessions.get_mut(&sid) else { continue };
                let mut next = e.info.clone();
                next.window_id = Some(window_id);
                next.hidden = false;
                self.store.update_session(&next)?;
                left.extend(e.info.window_id);
                e.info = next.clone();
                events.push(ServerEvent::SessionUpserted(next));
            }
            if let Some(o) = old.filter(|o| *o != window_id) {
                left.remove(&o);
                self.gc_window_locked(&mut g, o, &mut events);
            }
            for o in left {
                self.drop_if_unused_locked(&mut g, o, &mut events);
            }
            if let Some(w) = window_view(&g, window_id) {
                events.push(ServerEvent::WindowUpserted(w));
            }
            // Under the lock, as in `set_session_window`.
            for ev in events.drain(..) {
                self.emit(ev);
            }
            ch
        };
        Ok(ch)
    }

    /// Kills and removes the channel's sessions, marks it archived, optionally removes the
    /// worktree (the branch is kept).
    pub async fn archive_channel(self: &Arc<Self>, id: ChannelId, remove_worktree: bool) -> anyhow::Result<()> {
        if remove_worktree
            && let Some(ch) = self.channel(id).filter(|c| c.owns_worktree)
            && let Some(user) = self.worktree_user(&ch)
        {
            anyhow::bail!(
                "the worktree {} is still used by session {} in another tab; close that first",
                ch.cwd,
                user
            );
        }
        let ch = self.update_channel(id, false, |c| c.archived = true)?;
        let sids: Vec<SessionId> = self
            .inner
            .lock()
            .sessions
            .values()
            .filter(|e| e.info.channel_id == id)
            .map(|e| e.info.id)
            .collect();
        let mut failed = None;
        for sid in &sids {
            if let Err(error) = self.remove_session(*sid) {
                failed = Some(error.context(format!("removing session {sid}")));
            }
        }
        if let Some(error) = failed {
            // A session that could not be retired may still run: keep its tab and worktree.
            let _ = self.update_channel(id, true, |c| c.archived = false);
            return Err(error.context(format!("channel {id} was not archived")));
        }
        crate::panels::archive(self, id)?;
        self.emit(ServerEvent::ChannelRemoved { id });
        if let Some(wid) = ch.window_id {
            let mut events = Vec::new();
            {
                let mut g = self.inner.lock();
                g.titles.remove(&id);
                self.gc_window_locked(&mut g, wid, &mut events);
            }
            for ev in events {
                self.emit(ev);
            }
        }
        if remove_worktree && ch.owns_worktree {
            let repo = self.workspace(ch.workspace_id).map(|w| PathBuf::from(w.path));
            if let Some(repo) = repo {
                // Wait until the host confirms that the removed runs (and anything they left
                // behind) have exited; their retirement records go away only then.
                let deadline = tokio::time::Instant::now() + pty::KILL_GRACE + Duration::from_secs(5);
                loop {
                    let exiting = { let records = self.terminal_records.lock(); sids.iter().any(|sid| records.get(sid).is_some_and(|r| r.retired)) };
                    if !exiting || tokio::time::Instant::now() > deadline { break }
                    let hub = self.clone();
                    let _ = tokio::task::spawn_blocking(move || hub.reconcile_retired()).await;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                // Re-checked, and reserved until the delete is done: a session may have moved out
                // (or be starting) since the check above.
                // Reserved (checked and marked under one lock) until the delete ends; the guard
                // also releases it when this future is dropped mid-way (the client went away).
                match self.reserve_worktree_removal(&ch) {
                    Err(user) => self.notice(
                        "warn",
                        format!("Kept worktree {}: session {user} in another tab still runs in it", ch.cwd),
                    ),
                    Ok(reservation) => {
                        // The delete runs as its own task that owns the reservation: if this
                        // request goes away mid-way, the delete still finishes before anything
                        // may start in that directory again.
                        let path = PathBuf::from(&ch.cwd);
                        let del = tokio::spawn(async move {
                            let _reservation = reservation;
                            worktree::remove(&repo, &path).await
                        });
                        if let Ok(Err(e)) = del.await {
                            self.notice("warn", format!("Could not remove worktree {}: {e:#}", ch.cwd));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A running session outside `ch` whose cwd is inside `ch`'s worktree (it moved out of `ch`,
    /// or was started there by hand), if any.
    fn worktree_user(&self, ch: &Channel) -> Option<SessionId> {
        let g = self.inner.lock();
        let records = self.terminal_records.lock();
        dir_user(ch, g.sessions.values().map(|e| (&e.info, occupies(e, &records))))
    }

    /// Mark `ch`'s worktree as being deleted, unless a session outside `ch` runs (or is
    /// starting) in it; checked and marked under one lock, and `start_process` refuses cwds
    /// inside a marked directory, so nothing can start there until `release_worktree`.
    fn reserve_worktree_removal(self: &Arc<Self>, ch: &Channel) -> Result<WorktreeReservation, SessionId> {
        let mut g = self.inner.lock();
        {
            let records = self.terminal_records.lock();
            if let Some(user) = dir_user(ch, g.sessions.values().map(|e| (&e.info, occupies(e, &records)))) {
                return Err(user);
            }
            // A removed session's run that is still exiting (its retirement is pending).
            let root = Path::new(&ch.cwd);
            if let Some((user, _)) = records.iter().find(|(_, r)| r.retired && r.spec.cwd.starts_with(root)) {
                return Err(*user);
            }
        }
        let path = PathBuf::from(&ch.cwd);
        *g.removing_worktrees.entry(path.clone()).or_insert(0) += 1;
        Ok(WorktreeReservation { hub: self.clone(), path })
    }

    /// Archive `id` (keeping any worktree) only if it has no sessions left, checked and marked
    /// under one lock so a session spawned or moved into it meanwhile is never removed with it.
    /// Returns whether it was archived.
    pub fn archive_if_empty(&self, id: ChannelId) -> anyhow::Result<bool> {
        let events = {
            let mut g = self.inner.lock();
            if g.sessions.values().any(|e| e.info.channel_id == id) {
                return Ok(false);
            }
            let Some(c) = g.channels.get(&id).filter(|c| !c.archived) else {
                return Ok(false);
            };
            let mut next = c.clone();
            next.archived = true;
            self.store.update_channel(&next)?;
            g.channels.insert(id, next.clone());
            g.titles.remove(&id);
            let mut events = vec![ServerEvent::ChannelRemoved { id }];
            if let Some(wid) = next.window_id {
                self.gc_window_locked(&mut g, wid, &mut events);
            }
            events
        };
        for ev in events {
            self.emit(ev);
        }
        // Outside `inner`: the task's panels, events and files go with it, as in `archive_channel`.
        if let Err(e) = crate::panels::archive(self, id) {
            tracing::warn!(channel = id, "removing an emptied task's panels (retried at startup): {e:#}");
        }
        Ok(true)
    }

    /// Set a channel's agent permission profile (human only: the UI / RPC; never MCP). Applies
    /// to sessions started or restarted afterwards.
    pub fn set_permissions(&self, id: ChannelId, permissions: Permissions) -> anyhow::Result<Channel> {
        if self.channel(id).is_some_and(|c| c.archived) {
            anyhow::bail!("channel {id} is archived");
        }
        self.update_channel(id, true, |c| c.permissions = permissions)
    }

    pub fn set_layout(&self, id: ChannelId, layout: serde_json::Value) -> anyhow::Result<()> {
        let layout = if layout.is_null() { None } else { Some(layout) };
        self.update_channel(id, false, |c| c.layout = layout).map(|_| ())
    }

    /// Everything up to `at` (unix ms, default now; never later than now) has been seen.
    pub fn mark_read(&self, id: ChannelId, at: Option<i64>) -> anyhow::Result<Channel> {
        let now = now_ms();
        let at = at.map_or(now, |a| a.min(now));
        self.update_channel(id, true, |c| c.last_read_at = c.last_read_at.max(at))
    }

    pub fn reorder_channels(&self, ids: &[ChannelId]) -> anyhow::Result<()> {
        for (i, id) in ids.iter().enumerate() {
            let order = i as i64;
            if self.channel(*id).is_some_and(|c| c.sort_order != order) {
                self.update_channel(*id, true, |c| c.sort_order = order)?;
            }
        }
        Ok(())
    }

    pub async fn channel_diff(&self, id: ChannelId) -> anyhow::Result<String> {
        let ch = self
            .channel(id)
            .ok_or_else(|| anyhow::anyhow!("no such channel: {id}"))?;
        worktree::diff(Path::new(&ch.cwd), ch.base_branch.as_deref()).await
    }

    pub async fn spawn_session(self: &Arc<Self>, req: SpawnSession) -> anyhow::Result<SessionInfo> {
        self.ensure_not_shutting_down()?;
        let ch = self
            .channel(req.channel_id)
            .ok_or_else(|| anyhow::anyhow!("no such channel: {}", req.channel_id))?;
        let account_profile_id = self.resolve_account_profile(req.kind, req.account_profile,
            req.parent_id.or(req.from_session), ch.workspace_id)?;
        crate::adapters::check_extra_args(req.kind, &req.extra_args)?;
        if ch.archived {
            anyhow::bail!("channel {} is archived", ch.id);
        }
        let cwd = match req.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => {
                let p = paths::expand_tilde(c);
                let p = if p.is_relative() { Path::new(&ch.cwd).join(p) } else { p };
                tokio::fs::canonicalize(&p)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?
            }
            None => PathBuf::from(&ch.cwd),
        };
        if !cwd.is_dir() {
            anyhow::bail!("{} is not a directory", cwd.display());
        }
        if let Some(pid) = req.parent_id
            && self.session(pid).is_none()
        {
            anyhow::bail!("no such parent session: {pid}");
        }
        let (cols, rows) = pty::clamp_size(req.cols, req.rows);
        let now = now_ms();
        let mut events = Vec::new();
        let id = {
            let mut g = self.inner.lock();
            // Re-check: the channel may have been archived while we awaited above. (Checked and
            // inserted under one lock: closing a task that lost its last session can't miss it.)
            if g.channels.get(&ch.id).is_none_or(|c| c.archived) {
                anyhow::bail!("channel {} is archived", ch.id);
            }
            let window_id = self.session_window_locked(&mut g, req.window_id, req.parent_id, ch.id, &mut events)?;
            let base = req
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(req.kind.as_str())
                .to_string();
            let taken = |n: &str| {
                g.sessions
                    .values()
                    .any(|e| e.info.channel_id == ch.id && e.info.name == n)
            };
            let mut name = base.clone();
            let mut k = 2;
            while taken(&name) {
                name = format!("{base} {k}");
                k += 1;
            }
            let info = SessionInfo {
                id: 0,
                channel_id: ch.id,
                kind: req.kind,
                account_profile_id,
                name,
                cwd: cwd.to_string_lossy().to_string(),
                state: AgentState::Starting,
                state_detail: None,
                state_since: now,
                title: None,
                parent_id: req.parent_id,
                pid: None,
                exit_code: None,
                cols,
                rows,
                queued: 0,
                subagents: 0,
                size_owner: None,
                input_interrupted: false,
                live_cwd: None,
                last_turn_at: None,
                cache_cold_at: None,
                cache_ttl_min: None,
                cache_basis: Default::default(),
                state_seq: self.state_seq.next()?,
                created_at: now,
                window_id: Some(window_id),
                hidden: false,
                provider: Default::default(),
            };
            let stored = StoredSession {
                info,
                extra_args: req.extra_args.clone(),
            };
            let id = self.store.insert_session(&stored)?;
            // Linked before anyone can see the session (a snapshot or the window events): a
            // client that closes it at once must find the agent, since session.remove hides an
            // agent's session and never kills it. The registry's lock is a leaf, so taking it
            // under this one is safe.
            if let Some(agent) = req.agent_id
                && let Err(e) = self.agents.link_session(&self.store, agent, Some(id))
            {
                let _ = self.store.delete_session(id);
                return Err(e);
            }
            let mut info = stored.info;
            info.id = id;
            g.sessions.insert(
                id,
                SessEntry {
                    info,
                    extra_args: stored.extra_args,
                    token: None,
                    pty: None,
                    generation: 0,
                    early_exit: None,
                    last_hook_at: 0,
                    unverified_working: false,
                    last_dequeue_at: 0,
                    busy: true,
                    kill_requested: false,
                    killed_generation: None,
                    usable_title: None,
                    port_base: 0,
                    conversation: None,
                    conversation_sure: false,
                    opened: false,
                    codex_thread: None,
                    fenced_generation: None,
                },
            );
            id
        };
        for ev in events {
            self.emit(ev);
        }
        self.agents.set_prov(id, req.prov)?;
        let prompt = req.prompt.clone().filter(|p| !p.trim().is_empty());
        let source = match req.prov {
            Some(p) => crate::agents::Source::Chain(p),
            None => crate::agents::Source::Human,
        };
        let started = self.start_process(id, prompt.clone(), Some(source));
        if let Some(e) = self.inner.lock().sessions.get_mut(&id) {
            e.busy = false;
            e.kill_requested = false;
        }
        match started {
            Ok(Started {
                info,
                consumed,
                generation,
                early_exit,
                ..
            }) => {
                self.log_event(
                    Some(ch.id),
                    Some(id),
                    EventKind::SessionStarted,
                    serde_json::json!({
                        "session_id": id,
                        "kind": req.kind,
                        "account_profile_id": info.account_profile_id,
                        "name": info.name,
                        "parent_id": req.parent_id,
                    }),
                );
                if let Some(code) = early_exit {
                    self.pty_exited(id, generation, code);
                } else if let Some(text) = prompt {
                    self.note_prompt(id, &text);
                    self.deliver_initial_prompt(id, ch.id, text, consumed, req.parent_id);
                }
                Ok(self.session(id).unwrap_or(info))
            }
            Err(e) if e.downcast_ref::<crate::terminal::SpawnUncertain>().is_some() => {
                // The process may be running. Keep the session, its launch record and any
                // queued initial prompt; reconcile_pending adopts or retires the launch.
                let _ = self.update_session_info(id, |info| info.state_detail = Some(e.to_string()));
                Err(e.context(SpawnRetained(id)))
            }
            Err(e) => {
                if let Some(agent) = req.agent_id.filter(|_| self.agents.by_session(id).is_some()) {
                    let _ = self.agents.link_session(&self.store, agent, None);
                }
                if self.inner.lock().sessions.remove(&id).is_some() { self.input_handles.lock().remove(&id); }
                let _ = self.store.delete_session(id);
                let _ = std::fs::remove_dir_all(paths::session_dir(id));
                self.emit(ServerEvent::SessionRemoved { id });
                // Its task's last other session may have been removed while this start was
                // pending (that removal kept the task open for it): close it now if empty.
                if let Err(err) = self.archive_if_empty(ch.id) {
                    tracing::warn!(channel = ch.id, "closing an emptied task: {err}");
                }
                Err(e)
            }
        }
    }

    /// The window a new session of channel `cid` goes to: `requested` (must exist); else the
    /// parent (spawning) session's window; else the channel's; else where a new channel would
    /// go (`place_locked`, which may create window 1).
    fn session_window_locked(
        &self,
        g: &mut Inner,
        requested: Option<WindowId>,
        parent: Option<SessionId>,
        cid: ChannelId,
        events: &mut Vec<ServerEvent>,
    ) -> anyhow::Result<WindowId> {
        if requested.is_some() {
            return self.place_locked(g, requested, None, events);
        }
        let live = |w: &Option<WindowId>| w.filter(|w| g.windows.contains_key(w));
        let found = live(&parent.and_then(|p| g.sessions.get(&p)).and_then(|e| e.info.window_id))
            .or_else(|| live(&g.channels.get(&cid).and_then(|c| c.window_id)));
        match found {
            Some(w) => Ok(w),
            None => self.place_locked(g, None, None, events),
        }
    }

    fn deliver_initial_prompt(
        self: &Arc<Self>,
        id: SessionId,
        channel_id: ChannelId,
        text: String,
        consumed: bool,
        parent: Option<SessionId>,
    ) {
        let from = match parent {
            Some(p) => format!("session:{p}"),
            None => "human".to_string(),
        };
        // An unconsumed prompt was saved in the durable queue before the launch
        // (start_process), so a backend crash after this point can't lose it.
        self.log_event(
            Some(channel_id),
            Some(id),
            EventKind::Prompt,
            serde_json::json!({"text": text, "session_id": id, "from": from, "queued": !consumed}),
        );
        if !consumed {
            crate::inject::kick_initial(self, id);
        }
    }

    /// Start a process for an existing session entry. Returns the updated info and whether
    /// the adapter consumed the initial prompt.
    /// The caller logs SessionStarted, then reports `early_exit` via `pty_exited`.
    /// `source`: when set and the adapter doesn't take `prompt` on its command line, the
    /// prompt is saved in the durable queue for the new run before the launch is sent.
    fn start_process(self: &Arc<Self>, id: SessionId, prompt: Option<String>,
        source: Option<crate::agents::Source>) -> anyhow::Result<Started> {
        let retiring = self.terminal_records.lock().get(&id).is_some_and(|r| r.retired);
        if retiring { self.reconcile_retired()?; }
        {
            let records = self.terminal_records.lock();
            if let Some(record) = records.get(&id) {
                anyhow::ensure!(!record.retired, "session {id}'s previous terminal is still being retired; retry after cleanup");
                if record.pending && !record.key_refs.is_empty() && self.openrouter_key().is_none() {
                    return Err(crate::openrouter::missing_key_error()
                        .context(format!("session {id}'s previous launch, prepared for OpenRouter, waits for the key")));
                }
                anyhow::ensure!(!record.pending, "session {id}'s previous launch is unconfirmed; retry after it is reconciled");
            }
        }
        let generation = self.store.next_terminal_generation(self.generation_floor)?;
        let (info, extra_args, port_base, permissions, resume) = {
            let mut g = self.inner.lock();
            let cwd = PathBuf::from(
                &g.sessions
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?
                    .info
                    .cwd,
            );
            if g.removing_worktrees.keys().any(|w| cwd.starts_with(w)) {
                anyhow::bail!("session {id}'s folder is a worktree being removed");
            }
            let e = g.sessions.get_mut(&id).expect("looked up above");
            let previous = e.generation;
            e.generation = generation;
            e.early_exit = None;
            e.fenced_generation = None;
            e.opened = false;
            let resume = self.codex_resume(id, previous, e)?;
            // A resumed thread is the run's top-level conversation from the start.
            e.conversation = resume.clone();
            e.conversation_sure = resume.is_some();
            let (info, extra) = (e.info.clone(), e.extra_args.clone());
            // Also on restarts: args stored before the allowlist existed are replayed here.
            crate::adapters::check_extra_args(info.kind, &extra)?;
            let ch = g.channels.get(&info.channel_id);
            let port_base = ch.map(|c| c.port_base).unwrap_or(20000);
            let permissions = ch.map(|c| c.permissions).unwrap_or_default();
            if let Some(e) = g.sessions.get_mut(&id) {
                e.port_base = port_base;
            }
            (info, extra, port_base, permissions, resume)
        };
        let token = paths::random_token();
        let session_dir = paths::session_dir(id);
        paths::ensure_private_dir(&session_dir)?;
        let ctx = SpawnCtx {
            session_id: id,
            cwd: PathBuf::from(&info.cwd),
            muster_bin: self.cfg.muster_bin.clone(),
            daemon_url: self.daemon_url(),
            session_token: token.clone(),
            session_dir: session_dir.clone(),
            extra_args,
            initial_prompt: prompt.clone(),
            permissions,
            resume,
        };
        let mut spec = adapter_for(info.kind).command(&ctx)?;
        let continues = crate::cache::continues_conversation(info.kind, &ctx.extra_args) || ctx.resume.is_some();
        // A named agent resuming a conversation whose session row is gone (its task was
        // closed) takes the clock that conversation had.
        let resumed_turn = continues.then(|| self.saved_conversation_turn(id)).flatten();
        let provider = self.session_provider(id);
        let openrouter = match provider {
            crate::openrouter::Provider::OpenRouter => {
                let key = self.openrouter_key().cloned().ok_or_else(crate::openrouter::missing_key_error)?;
                if info.kind == SessionKind::Codex {
                    // Muster's own provider settings, ahead of the stored (allowlisted) args
                    // (after `resume`: a subcommand comes first).
                    let at = usize::from(spec.args.first().is_some_and(|a| a == crate::adapters::codex::RESUME));
                    spec.args.splice(at..at, crate::openrouter::codex_args());
                }
                Some(key)
            }
            crate::openrouter::Provider::Subscription => None,
        };
        let queued_prompt = prompt.filter(|_| !spec.consumed_initial_prompt).zip(source);
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("MUSTER".into(), "1".into());
        env.insert("MUSTER_SESSION_ID".into(), id.to_string());
        env.insert("MUSTER_CHANNEL_ID".into(), info.channel_id.to_string());
        env.insert("MUSTER_URL".into(), self.daemon_url());
        env.insert("MUSTER_TOKEN".into(), token.clone());
        env.insert("MUSTER_HOOK_DB".into(), crate::hook_inbox::path().to_string_lossy().to_string());
        // Non-secret per-run marker: recognizes this run's processes when reaping leftovers.
        // (Agent CLIs may strip MUSTER_TOKEN from the commands they run; Codex does by default.)
        env.insert("MUSTER_RUN".into(), paths::random_token()[..16].to_string());
        env.insert("MUSTER_BIN".into(), self.cfg.muster_bin.to_string_lossy().to_string());
        env.insert("PORT".into(), port_base.to_string());
        env.insert("MUSTER_PORT_BASE".into(), port_base.to_string());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.extend(spec.env.clone());
        let host = self.cfg.terminal_host.as_ref().context("terminal host is not configured")?;
        let mut complete_env: BTreeMap<String, String> = std::env::vars().collect();
        for key in INHERITED_SESSION_MARKERS { complete_env.remove(*key); }
        // Never the OpenRouter key (nor a variable holding it, nor the gateway paired with it),
        // nor OpenRouter settings inherited from an OpenRouter session.
        crate::openrouter::scrub_inherited(&mut complete_env, self.openrouter_key());
        complete_env.extend(env);
        self.apply_account_profile(info.account_profile_id, &mut complete_env)?;
        if info.kind == SessionKind::Claude {
            // Its status line becomes Muster's usage reporter around the user's own one, found
            // with exactly the environment this run gets (src/usage.rs).
            crate::usage::install_reporter(&session_dir.join("claude-settings.json"), &self.cfg.muster_bin,
                &complete_env, Path::new(&info.cwd));
            // Its reports are filed under the folder this run really uses, whatever the daemon's
            // environment is later.
            let folder = crate::usage::effective_config_dir(&complete_env).map(|d| d.to_string_lossy().into_owned());
            if let Err(e) = self.store.start_usage_run(id, generation, folder) {
                tracing::warn!(session = id, "usage reports of this run will be ignored: {e:#}");
            }
        }
        let mut injected = Vec::new();
        if let Some(key) = &openrouter {
            injected = crate::openrouter::apply_session_env(info.kind, key, &mut complete_env)?;
            if info.kind == SessionKind::Claude {
                // Its hooks lose MUSTER_TOKEN to Claude Code's credential scrub: they read it here.
                paths::write_private(&session_dir.join(crate::hookcmd::TOKEN_FILE), token.as_bytes())?;
            }
        }
        let cache_basis = crate::cache::basis_from_env(info.kind, |k| complete_env.get(k).map(String::as_str));
        let launch = SpawnSpec {
            run: RunId { id, generation }, program: spec.program.clone(), args: spec.args.clone(),
            env: complete_env, cwd: PathBuf::from(&info.cwd), cols: info.cols, rows: info.rows,
        };
        // No stored launch record holds the key, whatever the session's provider; only the
        // variables Muster set to it are filled in again when it is re-sent (restore_spec).
        let (stored_spec, key_refs) = crate::openrouter::redact_spec(launch.clone(), self.openrouter_key(), &injected);
        let mut record = StoredTerminal {
            host_instance: host.client.hello.instance.clone(), token: token.clone(), port_base,
            spec: stored_spec, key_refs, pending: true, retired: false,
            // Which key it bills (the spend caps only trust readings of that key).
            key_fp: openrouter.as_ref().map(|k| k.fingerprint()),
        };
        let previous = {
            let mut g = self.inner.lock();
            let e = g.sessions.get_mut(&id).filter(|e| e.generation == generation)
                .with_context(|| format!("session {id} was removed or replaced before its launch was prepared"))?;
            // Install this generation's initial state before the host can start it. A crash
            // after the host starts it, or a callback arriving before the acknowledgement,
            // then never sees the previous run's state (e.g. Exited) for the new run.
            let previous = e.info.clone();
            let mut info = previous.clone();
            info.state = AgentState::Starting;
            info.state_detail = None;
            info.state_since = now_ms();
            // What this run bills to (the badge follows the run, not the agent's saved setting).
            info.provider = provider;
            // A new conversation has nothing cached to lose; a resumed one keeps its clock.
            if !continues {
                info.last_turn_at = None;
            } else if info.last_turn_at.is_none() {
                info.last_turn_at = resumed_turn;
            }
            info.cache_basis = cache_basis;
            self.cache_rules.lock().refresh(&mut info);
            info.pid = None;
            info.exit_code = None;
            info.subagents = 0;
            info.title = None;
            info.queued = 0;
            self.store.update_session(&info)?;
            // Queue first: a crash before the launch record leaves only a queue for a run
            // that never started, which recovery discards.
            if let Some((text, source)) = queued_prompt.clone() {
                if let Err(error) = crate::inject::prepare_initial(self, id, generation, text, source) {
                    let _ = self.store.update_session(&previous);
                    return Err(error);
                }
            }
            let mut records = self.terminal_records.lock();
            if let Err(error) = self.store.save_terminal(id, &record) {
                let _ = self.store.update_session(&previous);
                return Err(error);
            }
            records.insert(id, record.clone());
            e.info = info;
            e.last_hook_at = 0;
            e.unverified_working = false;
            previous
        };
        // Clients first hear of the new run with its acknowledgement (pid included).
        let pty = match host.spawn(launch) {
            Ok(pty) => pty,
            Err(error) => {
                if error.downcast_ref::<crate::terminal::SpawnUncertain>().is_none() {
                    // Nothing started: restore the previous run's state (clients never saw
                    // the prepared one).
                    if let Some(e) = self.inner.lock().sessions.get_mut(&id).filter(|e| e.generation == generation) {
                        let info = &mut e.info;
                        info.state = previous.state;
                        info.state_detail = previous.state_detail.clone();
                        info.state_since = previous.state_since;
                        info.pid = previous.pid;
                        info.exit_code = previous.exit_code;
                        info.subagents = previous.subagents;
                        info.title = previous.title.clone();
                        info.queued = previous.queued;
                        info.provider = previous.provider;
                        if let Err(err) = self.store.update_session(info) {
                            tracing::warn!(session = id, "persisting session: {err}");
                        }
                    }
                    record.retired = true;
                    let mut records = self.terminal_records.lock();
                    if records.get(&id).is_none_or(|r| r.spec.run == record.spec.run) {
                        self.store.save_terminal(id, &record)?;
                        records.insert(id, record);
                    }
                }
                return Err(error);
            }
        };
        let (info, early, kill) = {
            let mut g = self.inner.lock();
            let superseded = match g.sessions.get(&id) {
                None => Some("was removed while starting"),
                Some(e) if e.generation != generation => Some("was restarted concurrently"),
                Some(_) => None,
            };
            {
                let mut records = self.terminal_records.lock();
                if records.get(&id).is_none_or(|r| r.spec.run == record.spec.run) {
                    record.pending = false;
                    record.retired = superseded.is_some() || records.get(&id).is_some_and(|r| r.retired);
                    // A lost acknowledgement still leaves the prepared identity.
                    // Removal's durable retirement must never be overwritten here.
                    if let Err(error) = self.store.save_terminal(id, &record) {
                        tracing::warn!(session = id, "persisting terminal acknowledgement: {error:#}");
                    }
                    records.insert(id, record);
                }
            }
            if let Some(why) = superseded {
                drop(g);
                pty.kill();
                anyhow::bail!("session {id} {why}");
            }
            let Some(e) = g.sessions.get_mut(&id) else {
                anyhow::bail!("session {id} was removed while starting");
            };
            let old = e.token.replace(token.clone());
            self.set_pty(id, e, Some(pty.clone()));
            // The initial state was installed before Spawn. Callbacks that arrived since
            // may already have advanced it; keep their state, count and grace time.
            e.info.pid = pty.pid;
            e.info.title = pty.title();
            let early = e.early_exit.take().or_else(|| pty.exit_code());
            let kill = std::mem::take(&mut e.kill_requested);
            let info = e.info.clone();
            if let Err(err) = self.store.update_session(&info) {
                tracing::warn!(session = id, "persisting session: {err}");
            }
            self.publish_session(&info);
            if let Some(old) = old {
                g.tokens.remove(&old);
            }
            g.tokens.insert(token, id);
            (info, early, kill)
        };
        if kill {
            pty.kill();
        }
        Ok(Started {
            info,
            consumed: spec.consumed_initial_prompt,
            generation,
            early_exit: early,
        })
    }

    /// The Codex thread run `previous`'s successor of session `id` reopens: a named agent's
    /// saved conversation (validated against older backends' records at load,
    /// `agents::Registry::load`), else the tab's saved thread if the run that carried it is the
    /// one being replaced (a run an older backend started in between was a new conversation).
    /// A tab's thread is then carried by the new run. None for Claude and shells.
    fn codex_resume(&self, id: SessionId, previous: u64, e: &mut SessEntry) -> anyhow::Result<Option<String>> {
        if e.info.kind != SessionKind::Codex {
            return Ok(None);
        }
        if let Some(agent) = self.agents.by_session(id) {
            return Ok(agent.resume_id.filter(|t| crate::agents::valid_resume_id(t)));
        }
        let Some(saved) = e.codex_thread.clone() else { return Ok(None) };
        if saved.generation != previous || !crate::agents::valid_resume_id(&saved.thread) {
            self.store.set_codex_thread(id, None)?;
            e.codex_thread = None;
            return Ok(None);
        }
        let carried = crate::store::CodexThread { thread: saved.thread.clone(), generation: e.generation };
        self.store.set_codex_thread(id, Some(&carried))?;
        e.codex_thread = Some(carried);
        Ok(Some(saved.thread))
    }

    /// Whether session `id`'s current launch reopens a conversation, and which (read from the
    /// argv it was launched with, `adapters::resumed_in_launch`).
    fn launched_resume(&self, id: SessionId, kind: SessionKind) -> Option<Option<String>> {
        let records = self.terminal_records.lock();
        crate::adapters::resumed_in_launch(kind, &records.get(&id)?.spec.args)
    }

    /// Run `generation` of session `id` couldn't reopen conversation `resumed` (the CLI said it
    /// has none): forget it where it is saved, so the next start begins a new one instead of
    /// failing the same way. A named agent's record, a Claude tab's `--resume`, a Codex tab's
    /// saved thread; nothing when something newer was saved meanwhile. True when it forgot one.
    fn forget_failed_resume(&self, id: SessionId, generation: u64, resumed: Option<&str>) -> bool {
        let forgot = (|| -> anyhow::Result<bool> {
            if self.agents.by_session(id).is_some() {
                return crate::agents::forget_conversation(self, id, resumed);
            }
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id).filter(|e| e.generation == generation) else { return Ok(false) };
            match e.info.kind {
                SessionKind::Claude if crate::cache::continues_conversation(e.info.kind, &e.extra_args) => {
                    let next = crate::agents::with_resume(e.info.kind, &e.extra_args, None)?;
                    self.store.set_session_extra_args(id, &next)?;
                    e.extra_args = next;
                    Ok(true)
                }
                SessionKind::Codex if e.codex_thread.as_ref().is_some_and(|t| resumed.is_none_or(|r| t.thread == r)) => {
                    self.store.set_codex_thread(id, None)?;
                    e.codex_thread = None;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })();
        match forgot {
            Ok(forgot) => {
                if forgot { tracing::info!(session = id, "its conversation couldn't be reopened; the next start begins a new one") }
                forgot
            }
            Err(error) => {
                tracing::warn!(session = id, "dropping a conversation the agent couldn't resume: {error:#}");
                false
            }
        }
    }

    /// At startup: a run that ended while no backend was watching (during a reload) and couldn't
    /// reopen its conversation gets the same cleanup `pty_exited` gives one (otherwise every
    /// later start would repeat the failed resume). Idempotent: once forgotten, nothing matches.
    pub fn reconcile_failed_resumes(&self) {
        let exited: Vec<(SessionId, u64, SessionKind, i32)> = self.inner.lock().sessions.values()
            .filter(|e| e.info.state == AgentState::Exited && !e.opened)
            .filter_map(|e| e.info.exit_code.filter(|c| *c != 0).map(|c| (e.info.id, e.generation, e.info.kind, c)))
            .collect();
        for (id, generation, kind, code) in exited {
            let Some(resumed) = self.launched_resume(id, kind) else { continue };
            if !conversation_missing(kind, code, false, resumed.as_deref(), self.recent_output_text(id, 40).as_deref())
                || !self.forget_failed_resume(id, generation, resumed.as_deref()) {
                continue;
            }
            let _ = self.update_session_info_checked(id, Some(generation), |i| i.state_detail = Some(failed_resume_detail(code)));
        }
    }

    /// Start fresh is replacing session `id`'s current run: from now on its hooks change no
    /// conversation the agent resumes (`HookSession::fenced`).
    pub fn fence_conversation(&self, id: SessionId) {
        if let Some(e) = self.inner.lock().sessions.get_mut(&id) {
            e.fenced_generation = Some(e.generation);
        }
    }

    /// Replace a session's agent arguments for its next (re)start (an agent's `--resume`). Checked
    /// against the allowlist like any extra args.
    pub fn set_session_extra_args(&self, id: SessionId, args: Vec<String>) -> anyhow::Result<()> {
        let mut g = self.inner.lock();
        let e = g.sessions.get_mut(&id).ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        crate::adapters::check_extra_args(e.info.kind, &args)?;
        self.store.set_session_extra_args(id, &args)?;
        e.extra_args = args;
        Ok(())
    }

    /// Re-spawn a session's process (same id, kind, cwd, name). Kills it first if running.
    /// The replay buffer is cleared.
    pub async fn restart_session(self: &Arc<Self>, id: SessionId) -> anyhow::Result<SessionInfo> {
        self.ensure_not_shutting_down()?;
        // Fail before interrupting a live process if its selected profile cannot start.
        let info = self.session(id).ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        self.check_account_profile(info.account_profile_id)?;
        self.check_session_provider(id)?;
        {
            let mut g = self.inner.lock();
            let e = g
                .sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            if e.busy {
                anyhow::bail!("session {id} is already restarting");
            }
            e.busy = true;
        }
        // Cleared on every exit, also when the request is dropped midway (an HTTP client that
        // went away), so the session can't stay "already restarting".
        struct Done<'a>(&'a Hub, SessionId);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                if let Some(e) = self.0.inner.lock().sessions.get_mut(&self.1) {
                    e.busy = false;
                    // A kill that arrived while starting was applied to the new process, or the
                    // start failed; either way it must not hit the next process.
                    e.kill_requested = false;
                }
            }
        }
        let _done = Done(self, id);
        // A named agent restarts with arguments rebuilt from its record (never a stale `--resume`).
        if let Some(args) = crate::agents::restart_args(&self.agents, id) {
            self.set_session_extra_args(id, args?)?;
        }
        self.restart_inner(id).await
    }

    async fn restart_inner(self: &Arc<Self>, id: SessionId) -> anyhow::Result<SessionInfo> {
        if let Some(p) = self.pty(id) {
            if let Some(e) = self.inner.lock().sessions.get_mut(&id) {
                e.killed_generation = Some(p.generation);
            }
            // Also reaps leftovers (background jobs) of an already exited process.
            p.kill();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
            while p.is_alive() || self.session(id).is_some_and(|s| s.state != AgentState::Exited) {
                if tokio::time::Instant::now() > deadline {
                    anyhow::bail!("session {id} did not exit");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // Leftovers (e.g. a dev server still holding the channel's port) must be gone before
            // the new process starts: wait out the kill's grace period, then SIGKILL.
            let deadline = tokio::time::Instant::now() + pty::KILL_GRACE;
            loop {
                let q = p.clone();
                let busy = tokio::task::spawn_blocking(move || q.has_processes())
                    .await
                    .unwrap_or(true);
                if !busy {
                    break;
                }
                if tokio::time::Instant::now() > deadline {
                    let q = p.clone();
                    let _ = tokio::task::spawn_blocking(move || q.kill_now()).await;
                    let end = tokio::time::Instant::now() + Duration::from_secs(1);
                    let mut gone = false;
                    while tokio::time::Instant::now() < end {
                        let q = p.clone();
                        if !tokio::task::spawn_blocking(move || q.has_processes())
                            .await
                            .unwrap_or(true)
                        {
                            gone = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    anyhow::ensure!(gone, "session {id}'s previous run still owns processes; retaining its launch record");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let previous = self.pty(id);
        if let Some(p) = previous {
            p.forget()?;
            let mut g = self.inner.lock();
            if let Some(e) = g.sessions.get_mut(&id) {
                if e.generation == p.generation { self.set_pty(id, e, None); }
            }
        }
        match self.start_process(id, None, None) {
            Ok(Started {
                info,
                generation,
                early_exit,
                ..
            }) => {
                // Clear any legacy application-owned log. The host owns new run logs.
                let _ = std::fs::remove_file(paths::session_dir(id).join("output.log"));
                self.log_event(
                    Some(info.channel_id),
                    Some(id),
                    EventKind::SessionStarted,
                    serde_json::json!({
                        "session_id": id,
                        "kind": info.kind,
                        "account_profile_id": info.account_profile_id,
                        "name": info.name,
                        "parent_id": info.parent_id,
                        "restart": true,
                    }),
                );
                if let Some(code) = early_exit {
                    self.pty_exited(id, generation, code);
                }
                Ok(self.session(id).unwrap_or(info))
            }
            Err(e) => {
                let detail = format!("restart failed: {e:#}");
                let _ = self.update_session_info(id, |i| i.state_detail = Some(detail));
                Err(e)
            }
        }
    }

    pub fn is_shutting_down(&self) -> bool { self.shutting_down.load(Ordering::SeqCst) }

    fn ensure_not_shutting_down(&self) -> anyhow::Result<()> {
        if self.shutting_down.load(Ordering::SeqCst) {
            anyhow::bail!("the daemon is shutting down");
        }
        Ok(())
    }

    /// SIGHUP the process group, then SIGKILL after a grace period.
    pub fn kill_session(&self, id: SessionId) -> anyhow::Result<()> {
        {
            let mut g = self.inner.lock();
            if let Some(e) = g.sessions.get_mut(&id)
                && e.busy
                && e.pty.as_ref().is_none_or(|p| !p.is_alive())
            {
                // A process is being started right now: kill it as soon as it's installed.
                e.kill_requested = true;
                return Ok(());
            }
        }
        match self.pty(id) {
            Some(p) => p.terminate(false),
            None if self.session(id).is_some() => Ok(()),
            None => anyhow::bail!("no such session: {id}"),
        }
    }

    /// Kill (if alive) and forget a session, including its scrollback.
    pub fn remove_session(&self, id: SessionId) -> anyhow::Result<()> {
        let entry = {
            let mut g = self.inner.lock();
            anyhow::ensure!(g.sessions.contains_key(&id), "no such session: {id}");
            let mut records = self.terminal_records.lock();
            let retired = records.get(&id).cloned().map(|mut record| { record.retired = true; record });
            self.store.retire_session(id, retired.as_ref())?;
            if let Some(record) = retired {
                records.insert(id, record);
            }
            let e = g
                .sessions
                .remove(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            self.input_handles.lock().remove(&id);
            if let Some(t) = &e.token {
                g.tokens.remove(t);
            }
            e
        };
        if let Some(p) = &entry.pty {
            // Also reaps leftovers (background jobs) of an already exited process.
            p.kill();
            // The durable retired launch is retried by reconciliation, including
            // when this backend stops before process exit or terminal drain.
        }
        let _ = std::fs::remove_dir_all(paths::session_dir(id));
        self.replay_cache.lock().remove(&id);
        self.save_conversation_turn(&entry.info);
        self.agents.forget_session(id);
        self.emit(ServerEvent::SessionRemoved { id });
        let cid = entry.info.channel_id;
        // A task closes with its last session (worktree kept). Checked and closed under one
        // lock, so a session spawned or moved into it meanwhile keeps it open. A task being
        // archived is already marked, so this is a no-op there.
        let closed = self.archive_if_empty(cid).unwrap_or_else(|e| {
            tracing::warn!(channel = cid, "closing emptied task: {e}");
            false
        });
        let mut events = Vec::new();
        {
            let mut g = self.inner.lock();
            if !closed {
                self.task_follows_locked(&mut g, cid, None, &mut events);
            }
            if let Some(w) = entry.info.window_id {
                self.drop_if_unused_locked(&mut g, w, &mut events);
            }
            for ev in events.drain(..) {
                self.emit(ev);
            }
        }
        if !closed {
            // A removed session's title no longer names the channel: rebuild from the rest.
            {
                let mut g = self.inner.lock();
                let t = titles_of(&g, cid);
                g.titles.insert(cid, t);
            }
            self.rederive_name(cid);
        }
        Ok(())
    }

    /// A task's window follows its tabs (sessions): to the one window they are all in, else,
    /// when none is left in the task's window, to `prefer` (if one of them is there) or the
    /// window of its oldest session, so a task never holds on to a window without its tabs.
    /// Emits `channel_upserted` and the windows' updates (deleting the old one if unused).
    fn task_follows_locked(
        &self,
        g: &mut Inner,
        cid: ChannelId,
        prefer: Option<WindowId>,
        events: &mut Vec<ServerEvent>,
    ) {
        let Some(c) = g.channels.get(&cid).filter(|c| !c.archived) else {
            return;
        };
        let old = c.window_id;
        // By session id: the oldest first.
        let wins: Vec<WindowId> = g
            .sessions
            .values()
            .filter(|e| e.info.channel_id == cid)
            .filter_map(|e| e.info.window_id)
            .filter(|w| g.windows.contains_key(w))
            .collect();
        let Some(first) = wins.first().copied() else {
            // Every tab it has left is a hidden agent's (`hide_session`): the task leaves its
            // window too, so it neither keeps the window alive nor shows up in it.
            let mut tabs = g.sessions.values().filter(|e| e.info.channel_id == cid).peekable();
            if let Some(o) = old
                && tabs.peek().is_some()
                && tabs.all(|e| e.info.hidden)
            {
                let mut nc = c.clone();
                nc.window_id = None;
                if let Err(err) = self.store.update_channel(&nc) {
                    tracing::warn!(channel = cid, "taking a hidden task out of window {o}: {err}");
                    return;
                }
                g.channels.insert(cid, nc.clone());
                events.push(ServerEvent::ChannelUpserted(nc));
                self.gc_window_locked(g, o, events);
            }
            return;
        };
        let target = if wins.iter().all(|w| *w == first) {
            first
        } else if old.is_none_or(|o| !wins.contains(&o)) {
            prefer.filter(|p| wins.contains(p)).unwrap_or(first)
        } else {
            return;
        };
        if old == Some(target) {
            return;
        }
        let mut nc = c.clone();
        nc.window_id = Some(target);
        nc.sort_order = next_tab_order(g, target);
        if let Err(err) = self.store.update_channel(&nc) {
            tracing::warn!(channel = cid, "moving task to window {target}: {err}");
            return;
        }
        g.channels.insert(cid, nc.clone());
        events.push(ServerEvent::ChannelUpserted(nc));
        if let Some(o) = old {
            self.gc_window_locked(g, o, events);
        }
        if let Some(v) = window_view(g, target) {
            events.push(ServerEvent::WindowUpserted(v));
        }
    }

    /// `session.set_window`: show a session (tab) in another window. With `expect_from`, refused
    /// unless it is still in that window (a client acting on a stale view). The session stays in
    /// its task; the task's own window follows when all its sessions are now in `window_id`, or
    /// when none is left in the task's window (`task_follows_locked`). A window left without
    /// tabs or tasks is deleted when nobody holds it.
    pub fn set_session_window(
        &self,
        id: SessionId,
        window_id: WindowId,
        expect_from: Option<WindowId>,
    ) -> anyhow::Result<SessionInfo> {
        let mut events = Vec::new();
        let info = {
            let mut g = self.inner.lock();
            if !g.windows.contains_key(&window_id) {
                anyhow::bail!("no such window: {window_id}");
            }
            let e = g
                .sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            let from = e.info.window_id;
            if let Some(expected) = expect_from
                && from != Some(expected)
            {
                let now = from.map_or("none".to_string(), |w| w.to_string());
                anyhow::bail!("session {id} is no longer in window {expected} (now in {now})");
            }
            if from == Some(window_id) && !e.info.hidden {
                return Ok(e.info.clone());
            }
            let mut next = e.info.clone();
            next.window_id = Some(window_id);
            // Showing a hidden agent tab again (the agents panel, Jump, Alt+J pull it here).
            next.hidden = false;
            self.store.update_session(&next)?;
            e.info = next.clone();
            events.push(ServerEvent::SessionUpserted(next.clone()));
            self.task_follows_locked(&mut g, next.channel_id, Some(window_id), &mut events);
            if let Some(w) = from {
                self.drop_if_unused_locked(&mut g, w, &mut events);
            }
            // Published before the lock is released, so two concurrent moves can't publish
            // their snapshots in the opposite order of the changes (emit never takes the lock).
            for ev in events.drain(..) {
                self.emit(ev);
            }
            next
        };
        Ok(info)
    }

    /// `session.hide`: close a named agent's tab without stopping it (docs/UI.md "Agents panel").
    /// The session keeps its process, scrollback and channel; it leaves its window
    /// (`window_id` None, `hidden`), and so does its task when none of its tabs is left in a
    /// window, so neither keeps a window alive nor is adopted by `window.claim`. Messages still
    /// reach it. `session.set_window` shows it again. Only an agent's session can be hidden: any
    /// other tab would be lost (nothing lists it), so its close stays `session.remove`.
    pub fn hide_session(&self, id: SessionId) -> anyhow::Result<SessionInfo> {
        anyhow::ensure!(
            self.agents.by_session(id).is_some(),
            "session {id} is not a named agent's; only an agent's tab can be hidden"
        );
        let mut events = Vec::new();
        let mut g = self.inner.lock();
        let e = g
            .sessions
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        if e.info.hidden {
            return Ok(e.info.clone());
        }
        let from = e.info.window_id;
        let mut next = e.info.clone();
        next.window_id = None;
        next.hidden = true;
        self.store.update_session(&next)?;
        e.info = next.clone();
        events.push(ServerEvent::SessionUpserted(next.clone()));
        self.task_follows_locked(&mut g, next.channel_id, None, &mut events);
        if let Some(w) = from {
            self.drop_if_unused_locked(&mut g, w, &mut events);
        }
        // Published under the lock, as in `set_session_window`.
        for ev in events.drain(..) {
            self.emit(ev);
        }
        Ok(next)
    }

    /// Move a session to another channel (the UI merges a tab into another one as a split).
    /// The process keeps running as it is: its cwd, its permission flags, and its environment,
    /// so `MUSTER_CHANNEL_ID` and `PORT` still name the old channel until it restarts (a restart
    /// applies the new channel's permission profile). Both channels' auto names are re-derived.
    /// Moving into the session's own channel is a no-op.
    #[cfg(test)]
    pub fn move_session(&self, id: SessionId, channel_id: ChannelId) -> anyhow::Result<SessionInfo> {
        self.move_session_from(id, channel_id, None).map(|(info, _)| info)
    }

    /// `move_session`, also returning the channel the session was in (read under the same lock
    /// as the move). With `expect_from`, refused unless the session is still in that channel, so
    /// a client acting on a stale view (another window moved it meanwhile) can't move it again.
    pub fn move_session_from(
        &self,
        id: SessionId,
        channel_id: ChannelId,
        expect_from: Option<ChannelId>,
    ) -> anyhow::Result<(SessionInfo, ChannelId)> {
        // Panel waits and panel prompts check their task under this lock: none can act for the
        // old task once the move is visible.
        let _panels = self.panels.operations.lock();
        let (info, from) = {
            let mut g = self.inner.lock();
            match g.channels.get(&channel_id) {
                None => anyhow::bail!("no such channel: {channel_id}"),
                Some(c) if c.archived => anyhow::bail!("channel {channel_id} is archived"),
                Some(_) => {}
            }
            let from = g
                .sessions
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?
                .info
                .channel_id;
            if let Some(expected) = expect_from
                && expected != from
            {
                anyhow::bail!("session {id} is no longer in channel {expected} (now in {from})");
            }
            // Archiving marks the channel first, then removes the sessions it finds: a session
            // must not slip out in between (nor out of a closed channel at all).
            if from != channel_id && g.channels.get(&from).is_some_and(|c| c.archived) {
                anyhow::bail!("session {id}'s channel {from} is being closed");
            }
            let e = g.sessions.get_mut(&id).expect("checked above");
            if from == channel_id {
                return Ok((e.info.clone(), from));
            }
            // A start in progress reads the channel it is starting in.
            if e.busy {
                anyhow::bail!("session {id} is starting");
            }
            let mut next = e.info.clone();
            next.channel_id = channel_id;
            self.store.update_session(&next)?;
            e.info = next.clone();
            for cid in [from, channel_id] {
                let t = titles_of(&g, cid);
                g.titles.insert(cid, t);
            }
            // Both tasks' windows follow their sessions, as after `session.set_window`.
            let mut events = vec![ServerEvent::SessionUpserted(next.clone())];
            for cid in [from, channel_id] {
                self.task_follows_locked(&mut g, cid, next.window_id, &mut events);
            }
            for ev in events {
                self.emit(ev);
            }
            (next, from)
        };
        if from != channel_id { crate::inject::release_unbound_panel_batches(self, id); }
        drop(_panels);
        self.rederive_name(from);
        self.rederive_name(channel_id);
        Ok((info, from))
    }

    pub fn rename_session(&self, id: SessionId, name: String) -> anyhow::Result<SessionInfo> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("session name is empty");
        }
        self.update_session_info(id, |i| i.name = name)
    }

    /// Last resizer wins.
    /// Resize a session's terminal. `owner` ("local" or "device:<id>") becomes its
    /// `size_owner`; a change of owner alone is broadcast too (docs/ANDROID.md "Terminal width").
    pub fn resize_session(&self, id: SessionId, cols: u16, rows: u16, owner: &str) -> anyhow::Result<()> {
        let (cols, rows) = pty::clamp_size(cols, rows);
        let cur = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        if let Some(p) = self.pty(id) {
            // Bumped once the host has the new size (never before, so a replay labelled with
            // the new revision is drawn at the new size) and on every call: comparing with
            // `cur` could miss a concurrent resize.
            // Marked in progress (and bumped) before the host is asked, since it applies the
            // size before it answers: an attach overlapping it sees None or a newer revision.
            {
                let mut revs = self.size_revs.lock();
                let e = revs.entry(id).or_default();
                e.0 += 1;
                e.1 += 1;
            }
            let resized = p.resize(cols, rows);
            {
                let mut revs = self.size_revs.lock();
                let e = revs.entry(id).or_default();
                e.0 += 1;
                e.1 -= 1;
            }
            resized?;
        }
        if cur.cols != cols || cur.rows != rows || cur.size_owner.as_deref() != Some(owner) {
            self.update_session_info(id, |i| {
                i.cols = cols;
                i.rows = rows;
                i.size_owner = Some(owner.to_string());
            })?;
        }
        Ok(())
    }

    /// Handle a hook callback authenticated by session token. Routes to the adapter's
    /// `on_hook`, logs a Hook event, applies the resulting state.
    #[cfg(test)]
    pub fn handle_hook(&self, token: &str, event: &str, payload: serde_json::Value) -> anyhow::Result<()> {
        self.handle_hook_at(token, event, payload, self.human_input_seq())
    }

    /// The newest human-input sequence number. A legacy HTTP callback takes this when its
    /// request arrives: keystrokes numbered later came after it.
    pub fn human_input_seq(&self) -> u64 {
        self.human_input.now()
    }

    /// The sequence number and time of the human's newest keystroke into session `id` (None:
    /// none since this backend started).
    pub fn human_input_of(&self, id: SessionId) -> Option<(u64, std::time::Instant)> {
        self.human_input.latest.lock().get(&id).copied()
    }

    /// A legacy HTTP callback that arrived when the human-input sequence was at `arrival`.
    /// It may wait for the callback boundary while later keystrokes are recorded and even
    /// committed (by the attribution worker); a prompt callback consumes the human-input
    /// marker only if some of that input came before it arrived, so a later keystroke can
    /// never lower the provenance of an earlier, agent-delivered turn.
    pub fn handle_hook_at(&self, token: &str, event: &str, payload: serde_json::Value, arrival: u64) -> anyhow::Result<()> {
        self.input_boundary(|| self.handle_hook_checkpoint(token, event, payload, None, arrival, None))
    }

    /// `captured_at`: when the helper saved it (unix ms; None from older helpers).
    pub(crate) fn handle_spooled_hook(&self, token: &str, event: &str, payload: serde_json::Value,
        cursor: &crate::store::HookCursor, captured_at: Option<i64>) -> anyhow::Result<()> {
        // Spooled callbacks are ordered against markers by the inbox itself: a marker is
        // committed only after every earlier insert was drained, with inserts blocked.
        self.handle_hook_checkpoint(token, event, payload, Some(cursor), u64::MAX, captured_at)
    }

    fn handle_hook_checkpoint(&self, token: &str, event: &str, payload: serde_json::Value,
        cursor: Option<&crate::store::HookCursor>, arrival: u64, captured_at: Option<i64>) -> anyhow::Result<()> {
        // Spool callbacks from a process that exited during an outage may still contain
        // its final reply. Accept only the recorded current run, never a previous generation.
        let recorded = cursor.and_then(|_| self.terminal_records.lock().iter()
            .find(|(_, record)| !record.retired && record.token == token)
            .map(|(id, record)| (*id, record.spec.run.generation)));
        let (info, from, fx) = {
            let mut g = self.inner.lock();
            let id = if cursor.is_some() {
                recorded.filter(|(id, generation)| g.sessions.get(id).is_some_and(|entry| entry.generation == *generation))
                    .map(|(id, _)| id)
            } else { g.tokens.get(token).copied() };
            let entry = id.and_then(|id| g.sessions.get_mut(&id));
            let Some(entry) = entry else {
                if let Some(cursor) = cursor {
                    // Removed/restarted sessions cannot receive old callbacks. Advance
                    // the checkpoint so one obsolete run cannot block every other session.
                    self.store.save_runtime("hook-inbox-cursor", cursor)?;
                    return Ok(());
                }
                anyhow::bail!("unknown session token");
            };
            // A target can enqueue hooks before its spawn acknowledgement returns. That is
            // safe to apply: start_process installs the run's initial state together with
            // its launch record and never resets it on acknowledgement. Waiting here would
            // stall every other session's callbacks behind an unconfirmed launch.
            let mut info = entry.info.clone();
            let from = info.state;
            let mut last_hook_at = entry.last_hook_at;
            let now = now_ms();
            let is_agent = self.agents.by_session(entry.info.id).is_some();
            let change = conversation_after_hook(info.kind, entry.conversation.as_deref(), entry.conversation_sure,
                &entry.extra_args, event, &payload);
            // A sub-agent's turn ending says nothing about the session's own state.
            if info.state != AgentState::Exited && !change.child {
                if let Some(update) = adapter_for(info.kind).on_hook(event, &payload) {
                    last_hook_at = now;
                    if info.state != update.state || info.state_detail != update.detail {
                        info.state_seq = self.state_seq.next()?;
                        if info.state != update.state { info.state_since = now; }
                        info.state = update.state;
                        info.state_detail = update.detail;
                    }
                }
                if let Some(delta) = subagent_delta(event, &payload) {
                    info.subagents = delta.map_or(0, |d| info.subagents.saturating_add_signed(d));
                }
                // When the callback was captured: one replayed after an outage happened then.
                let at = captured_at.map_or(now, |t| t.min(now));
                if crate::cache::note_hook(&mut info, event, &payload, at) {
                    self.cache_rules.lock().refresh(&mut info);
                }
                if from != info.state {
                    entry.unverified_working = false;
                }
            } else if info.state == AgentState::Exited {
                info.subagents = 0;
            }
            // Start fresh is replacing this run: it changes no conversation anything resumes.
            let fenced = entry.fenced_generation == Some(entry.generation);
            let mut runtime = crate::store::HookSession { generation: entry.generation,
                subagents: info.subagents, last_hook_at, conversation: change.conversation.clone(),
                conversation_sure: change.sure, fenced, child: change.child, ..Default::default() };
            if !fenced {
                if is_agent {
                    // A named agent's conversation is on its record, its arguments rebuilt from it.
                    runtime.thread = change.thread.clone();
                } else {
                    runtime.extra_args = change.args.clone();
                    runtime.tab_thread = change.thread.clone().map(|thread| thread.map(|thread|
                        crate::store::CodexThread { thread, generation: entry.generation }))
                        .filter(|next| *next != entry.codex_thread);
                }
            }
            // State/count/grace and reply attribution all commit with the cursor.
            // A crash before publishing memory can recover state and attribution.
            let fx = self.agents.on_hook_session_checkpoint(&self.store, &entry.info, event, &payload,
                cursor, Some((&info, &runtime)), arrival)?;
            if fx.duplicate { return Ok(()) }
            let changed = info.state_seq != entry.info.state_seq || info.subagents != entry.info.subagents
                || info.cache_cold_at != entry.info.cache_cold_at;
            entry.info = info.clone();
            entry.last_hook_at = last_hook_at;
            entry.conversation = runtime.conversation;
            entry.conversation_sure = runtime.conversation_sure;
            // Its CLI is running a conversation (the hook state just saved says so too).
            entry.opened = true;
            if let Some(args) = runtime.extra_args { entry.extra_args = args; }
            if let Some(thread) = runtime.tab_thread { entry.codex_thread = thread; }
            if changed {
                self.publish_session(&info);
            }
            (info, from, fx)
        };
        let id = info.id;
        if let Some(m) = fx.replied {
            crate::agents::publish_message(self, m);
            crate::schedules::on_reply(self, m);
        }
        if let Some(a) = fx.agent.and_then(|a| self.agents.by_id(a)) { crate::agents::publish(self, &a); }
        self.log_event(Some(info.channel_id), Some(id), EventKind::Hook,
            serde_json::json!({"session_id": id, "event": event, "summary": hook_summary(&payload)}));
        if from != info.state {
            self.log_event(Some(info.channel_id), Some(id), EventKind::StateChange,
                serde_json::json!({"session_id": id, "from": from, "to": info.state,
                    "detail": info.state_detail, "source": StateSource::Hook}));
        }
        Ok(())
    }

    // ----- daemon lifecycle -----

    /// Finish durable retirement intents after exit/drain, including removals
    /// interrupted by a backend crash. The host owns signaling and grace periods.
    pub fn reconcile_retired(&self) -> anyhow::Result<()> {
        let Some(host) = &self.cfg.terminal_host else { return Ok(()) };
        let retired: Vec<_> = self.terminal_records.lock().iter().filter(|(_, r)| r.retired)
            .map(|(id, r)| (*id, r.host_instance.clone(), r.spec.run)).collect();
        if retired.is_empty() { return Ok(()) }
        let inventory = host.inventory()?;
        let mut failure = None;
        for (id, instance, run) in retired {
            let result = (|| -> anyhow::Result<()> {
                if instance == host.client.hello.instance {
                    if let Some(status) = inventory.iter().find(|s| s.run == run) {
                        if status.has_processes {
                            if !self.retire_signalled.lock().contains(&run) {
                                host.client.call(HostCommand::Terminate { run, force: false })?;
                                self.retire_signalled.lock().insert(run);
                            }
                            return Ok(());
                        }
                        if status.alive || !status.drained { return Ok(()); }
                    }
                    // Also fences a prepared-but-not-yet-spawned run. A concurrent
                    // spawn either loses to this tombstone or makes Forget refuse.
                    host.client.call(HostCommand::Forget { run })?;
                }
                let mut records = self.terminal_records.lock();
                if records.get(&id).is_some_and(|r| r.retired && r.spec.run == run && r.host_instance == instance) {
                    self.store.delete_terminal(id)?;
                    records.remove(&id);
                }
                self.retire_signalled.lock().remove(&run);
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(session = id, ?run, "retaining terminal retirement for retry: {error:#}");
                failure = Some(error);
            }
        }
        match failure { Some(error) => Err(error), None => Ok(()) }
    }

    /// Resolve launches whose spawn acknowledgement was lost, without waiting for a backend
    /// restart. Spawn is retry-safe: the host returns the original process for the same run.
    pub fn reconcile_pending(&self) -> anyhow::Result<()> {
        let Some(host) = &self.cfg.terminal_host else { return Ok(()) };
        let key = self.openrouter_key();
        let pending: Vec<_> = self.terminal_records.lock().iter()
            .filter(|(_, r)| r.pending && !r.retired && r.host_instance == host.client.hello.instance)
            // Deferred until a backend with the OpenRouter key (see `Hub::new`).
            .filter(|(_, r)| key.is_some() || r.key_refs.is_empty())
            .map(|(id, r)| (*id, r.clone())).collect();
        for (id, mut record) in pending {
            let run = record.spec.run;
            // A start still in progress handles its own acknowledgement.
            let idle = self.inner.lock().sessions.get(&id).is_some_and(|e| !e.busy && e.generation == run.generation);
            if !idle { continue }
            let result = host.spawn(crate::openrouter::restore_spec(record.spec.clone(), &record.key_refs, key));
            let mut g = self.inner.lock();
            let mut records = self.terminal_records.lock();
            if !records.get(&id).is_some_and(|r| r.spec.run == run && r.pending && !r.retired) { continue }
            let Some(e) = g.sessions.get_mut(&id).filter(|e| !e.busy && e.generation == run.generation) else { continue };
            match result {
                Ok(pty) => {
                    record.pending = false;
                    if !record.key_refs.is_empty() {
                        record.key_fp = key.map(|k| k.fingerprint());
                    }
                    self.store.save_terminal(id, &record)?;
                    records.insert(id, record.clone());
                    drop(records);
                    let old = e.token.replace(record.token.clone());
                    self.set_pty(id, e, Some(pty.clone()));
                    e.info.pid = pty.pid;
                    e.info.title = pty.title();
                    e.info.state_detail = None;
                    let early = e.early_exit.take().or_else(|| pty.exit_code());
                    let info = e.info.clone();
                    if let Err(err) = self.store.update_session(&info) {
                        tracing::warn!(session = id, "persisting session: {err}");
                    }
                    if let Some(old) = old { g.tokens.remove(&old); }
                    g.tokens.insert(record.token.clone(), id);
                    self.publish_session(&info);
                    drop(g);
                    tracing::info!(session = id, ?run, "adopted a launch whose acknowledgement was lost");
                    if let Some(code) = early { self.pty_exited(id, run.generation, code); }
                }
                Err(error) if is_definite_spawn_failure(&error) => {
                    record.retired = true;
                    self.store.save_terminal(id, &record)?;
                    records.insert(id, record);
                    drop(records);
                    e.info.state = AgentState::Exited;
                    e.info.state_detail = Some(format!("launch failed: {error:#}"));
                    e.info.state_since = now_ms();
                    e.info.pid = None;
                    let info = e.info.clone();
                    if let Err(err) = self.store.update_session(&info) {
                        tracing::warn!(session = id, "persisting session: {err}");
                    }
                    self.publish_session(&info);
                    drop(g);
                    crate::inject::drop_queue(self, id);
                }
                Err(error) => tracing::debug!(session = id, ?run, "launch still unconfirmed: {error:#}"),
            }
        }
        Ok(())
    }

    /// Stop accepting application work. The independently owned terminals continue.
    /// Only the explicit full-stop CLI operation shuts down the terminal host.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.human_input.wake.notify_all();
        // Let already-started submissions reach a recorded outcome. No new queued
        // delivery starts while stopping. A timeout leaves its durable in-flight
        // record for explicit review on recovery, never an automatic duplicate.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while self.injection.in_progress() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Keystrokes: refuse new ones (they interrupt their run, so the user hears of them)
        // and let every admitted one reach the host. Input still pending at the deadline
        // interrupts its run too; nothing is ever resent.
        if let Some(host) = &self.cfg.terminal_host {
            let input = host.input().clone();
            input.close();
            let until = std::time::Instant::now() + Duration::from_secs(3);
            if !tokio::task::spawn_blocking(move || input.drain(until)).await.unwrap_or(false) {
                tracing::warn!("terminal input did not settle before shutdown");
            }
        }
        // Commit the markers of human input that reached a terminal, under the callback
        // boundary, so a callback for that input is attributed to the human after restart.
        // (A crash before this keeps the session's current provenance, the stricter one.)
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match self.flush_human_input() {
                Ok(()) => break,
                Err(error) if std::time::Instant::now() >= until => {
                    tracing::warn!("human-input attribution not saved before shutdown: {error:#}");
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let workers = std::mem::take(&mut *self.workers.lock());
        for worker in workers {
            while !worker.is_finished() && std::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if worker.is_finished() { let _ = worker.join(); }
        }
        // Interruptions announced while stopping are saved for the next backend.
        while self.interruptions_pending.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.save_input_interrupted();
    }

}

/// What a hook changes about the conversation a session restarts into
/// (`conversation_after_hook`).
#[derive(Debug, Default, PartialEq)]
struct ConversationChange {
    /// The run's conversation after the hook (`SessEntry::conversation`).
    conversation: Option<String>,
    /// Codex: it is known to be the run's top-level thread.
    sure: bool,
    /// A Claude tab's new stored arguments (`--resume`).
    args: Option<Vec<String>>,
    /// The conversation the next start resumes, when it changes (Some(None): none): a Codex
    /// tab's saved thread, or a named agent's (Claude or Codex) record.
    thread: Option<Option<String>>,
    /// Codex: a notify from a thread that isn't the run's top-level one (a sub-agent's).
    child: bool,
}

/// What a hook means for the conversation a session restarts into. Pure: the caller applies it
/// (a tab's arguments or saved thread, an agent's record), unless Start fresh is replacing the
/// run (`HookSession::fenced`).
///
/// Claude: every hook carries the id. It is resumed only once a prompt was sent in it or a turn
/// ended (Claude can't resume one without messages), so a tab already running when this shipped
/// picks its conversation up at its next turn; a `/clear` drops the old one until then.
///
/// Codex sends its turn-complete notify for sub-agent threads too, each with its own
/// `thread-id`, so only the run's top-level thread is ever saved: the one it resumed (set at
/// start), else the one its SessionStart hook names, else the first to finish a turn. A
/// SessionStart naming another thread is `/new` (forget the old one until the new one has a
/// finished turn) or Codex's own `/resume` of a saved thread (resume that one). A notify from
/// another thread is a sub-agent's once the top-level thread is known for sure; before that
/// (no hooks, the first turn) it is ambiguous and the first thread is kept. Known gaps: with
/// Codex's hooks off or not yet trusted, a sub-agent finishing before the first top-level turn
/// is taken for the conversation, and `/new` isn't seen (the old thread stays saved). Also then,
/// a later sub-agent's notify still counts as the tab's own turn ending (its state turns idle
/// and an `ask` waiting on it gets the sub-agent's message): without hooks it can't be told
/// from the turns after an unseen `/new`, which must keep ending the tab's turns.
///
/// Known gap: a restart that overtakes a hook still waiting in the inbox (an outage) misses it.
fn conversation_after_hook(kind: SessionKind, conversation: Option<&str>, sure: bool, args: &[String], event: &str,
    payload: &serde_json::Value) -> ConversationChange {
    let reported = payload.get("session_id").and_then(|v| v.as_str()).map(str::trim)
        .filter(|id| crate::agents::valid_resume_id(id));
    let source = payload.get("source").and_then(|v| v.as_str());
    let mut out = ConversationChange { conversation: conversation.map(str::to_string), sure, ..Default::default() };
    match kind {
        SessionKind::Claude => {
            out.conversation = reported.map(str::to_string).or(out.conversation);
            let next = match event {
                "SessionStart" if reported.is_some() && source == Some("clear") => Some(None),
                "UserPromptSubmit" | "Stop" => out.conversation.clone().map(Some),
                _ => None,
            };
            out.args = match next.as_ref().map(|id| crate::agents::with_resume(kind, args, id.as_deref())) {
                Some(Ok(next)) => (next != args).then_some(next),
                Some(Err(error)) => {
                    tracing::warn!("keeping the conversation to resume: {error:#}");
                    None
                }
                None => None,
            };
            out.thread = next;
        }
        SessionKind::Codex => {
            let finished = crate::agents::resume_id_from_hook(kind, event, payload);
            if event == "SessionStart" && let Some(started) = reported {
                if conversation != Some(started) {
                    out.conversation = Some(started.to_string());
                    // A fresh run's own SessionStart has nothing to forget (None before).
                    if conversation.is_some() {
                        out.thread = Some((source == Some("resume")).then(|| started.to_string()));
                    }
                }
                out.sure = true;
            } else if let Some(thread) = finished {
                match conversation {
                    None => {
                        out.conversation = Some(thread.clone());
                        out.thread = Some(Some(thread));
                    }
                    Some(top) if top == thread => out.thread = Some(Some(thread)),
                    Some(_) => out.child = sure,
                }
            }
        }
        SessionKind::Shell => {}
    }
    out
}

/// Whether run `generation` of session `id`, which exited with `code` after being launched to
/// reopen a conversation (`adapters::resumed_in_launch`), failed because the CLI no longer has
/// it: only the CLI's own message says so (`adapters::says_conversation_missing`). Any other
/// failure (Codex also exits 1 when its local database can't open) keeps the conversation.
/// `opened`: the run sent a hook (`SessEntry::opened`). Both CLIs print that message and exit
/// before any hook, so a run that sent one did open its conversation, and its later exit keeps
/// it even when its output quotes the message (a conversation about this very code).
fn conversation_missing(kind: SessionKind, code: i32, opened: bool, resumed: Option<&str>, output: Option<&str>) -> bool {
    code != 0 && !opened && output.is_some_and(|text| crate::adapters::says_conversation_missing(kind, text,
        resumed.filter(|_| kind == SessionKind::Codex)))
}

/// The conversation of a run recovered without any hook state (no hook from it yet), and
/// whether it is known to be the top-level one: a Codex run launched to resume a thread
/// (`codex resume … -- <id>`, `adapters::resumed_in_launch`) has that thread, as
/// `Hub::start_process` set it. Claude's hooks all name theirs; nothing before them.
fn recovered_conversation(kind: SessionKind, launched: Option<&[String]>) -> (Option<String>, bool) {
    let resumed = launched.filter(|_| kind == SessionKind::Codex)
        .and_then(|args| crate::adapters::resumed_in_launch(kind, args)).flatten()
        .filter(|thread| crate::agents::valid_resume_id(thread));
    let sure = resumed.is_some();
    (resumed, sure)
}

/// A Codex tab's saved thread as a backend loads it, for a session whose current run is
/// `generation` (0: none recovered). The thread was carried by launch `thread.generation`. It is
/// still the tab's when no later launch was recorded (`latest_launch`: the session's terminal
/// record, retired ones included): a launch that never started (its spawn failed, so its record
/// was retired, maybe deleted since) or was never sent changes no conversation. It is then
/// taken as carried by `generation`, which `Hub::codex_resume` compares with. A later launch
/// (one an older backend started, a new conversation) leaves it as saved, so it is not resumed.
fn tab_thread_at_load(saved: Option<crate::store::CodexThread>, latest_launch: Option<u64>, generation: u64)
    -> Option<crate::store::CodexThread> {
    saved.map(|thread| match latest_launch.is_none_or(|latest| latest <= thread.generation) {
        true => crate::store::CodexThread { generation, ..thread },
        false => thread,
    })
}

/// The tab's status after a run couldn't reopen its conversation.
fn failed_resume_detail(code: i32) -> String {
    format!("exit code {code}: couldn't reopen its conversation, so the next start begins a new one")
}

/// What the event log keeps of a hook payload: the event's identifying fields only (the full
/// payload can hold prompts, tool inputs and outputs).
fn hook_summary(payload: &serde_json::Value) -> serde_json::Value {
    const KEYS: &[&str] = &[
        "hook_event_name",
        "tool_name",
        "notification_type",
        "matcher",
        "source",
        "reason",
        "type",
    ];
    let mut out = serde_json::Map::new();
    if let Some(obj) = payload.as_object() {
        for k in KEYS {
            if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
                let v: String = v.chars().take(120).collect();
                out.insert((*k).to_string(), serde_json::Value::String(v));
            }
        }
    }
    serde_json::Value::Object(out)
}

/// Each channel gets a block of 10 ports.
pub fn port_base_for(id: ChannelId) -> u16 {
    (20000 + (id.rem_euclid(1000)) * 10) as u16
}

// ----- windows (pure helpers, called under the hub lock) -----

/// A window's tabs: its live channels in sort order.
fn tab_ids(g: &Inner, id: WindowId) -> Vec<ChannelId> {
    let mut v: Vec<(i64, ChannelId)> = g
        .channels
        .values()
        .filter(|c| !c.archived && c.window_id == Some(id))
        .map(|c| (c.sort_order, c.id))
        .collect();
    v.sort();
    v.into_iter().map(|(_, id)| id).collect()
}

fn window_view(g: &Inner, id: WindowId) -> Option<Window> {
    let w = g.windows.get(&id)?;
    Some(Window {
        id,
        name: w.name.clone(),
        channel_ids: tab_ids(g, id),
        open: w.holder.is_some(),
        last_focused_at: w.last_focused_at,
        created_at: w.created_at,
        layout: w.layout.clone(),
    })
}

/// Does window `id` show any tab (session)?
fn has_sessions(g: &Inner, id: WindowId) -> bool {
    g.sessions.values().any(|e| e.info.window_id == Some(id))
}

/// No tab (session) and no task (live channel) refers to window `id`: once nobody holds it,
/// it can go.
fn window_unused(g: &Inner, id: WindowId) -> bool {
    !has_sessions(g, id) && tab_ids(g, id).is_empty()
}

/// sort_order for a tab appended to window `id`.
fn next_tab_order(g: &Inner, id: WindowId) -> i64 {
    g.channels
        .values()
        .filter(|c| !c.archived && c.window_id == Some(id))
        .map(|c| c.sort_order + 1)
        .max()
        .unwrap_or(0)
}

/// Windows are numbered 1, 2, 3...: the lowest free number.
fn free_window_id(g: &Inner) -> WindowId {
    (1..).find(|i| !g.windows.contains_key(i)).unwrap_or(1)
}

fn clean_window_name(name: Option<String>) -> Option<String> {
    let n: String = name?.chars().filter(|c| !c.is_control()).collect();
    let n = cap_chars(n.trim(), NAME_MAX);
    (!n.is_empty()).then_some(n)
}

// ----- auto naming (pure) -----

/// Channel names derived by the daemon are capped at this many chars.
pub const NAME_MAX: usize = 60;

fn cap_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().trim_end().to_string()
}

/// Auto-naming inputs of a channel rebuilt from its sessions' last usable titles.
fn titles_of(g: &Inner, cid: ChannelId) -> Titles {
    let mut t = Titles::default();
    for e in g.sessions.values().filter(|e| e.info.channel_id == cid) {
        if let Some(clean) = e.usable_title.clone() {
            match e.info.kind {
                SessionKind::Shell => t.shell = Some(clean),
                SessionKind::Claude | SessionKind::Codex => t.agent = Some(clean),
            }
        }
    }
    t
}

/// The folder a channel is named after until something better shows up.
fn folder_name(ws: &Workspace) -> String {
    Path::new(&ws.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| ws.name.clone())
}

/// `muster/<id>` or a deduplicated `muster/<id>-<n>`: the branch a worktree starts on.
fn is_initial_branch(branch: &str, id: ChannelId) -> bool {
    let Some(rest) = branch.strip_prefix(&format!("muster/{id}")) else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A terminal title usable as a channel name, or None. Leading spinner/status glyphs and
/// surrounding whitespace are stripped; program names (`claude`, `codex cli`, the kind, a
/// shell's name), paths and `user@host` prompts are rejected.
pub fn clean_title(title: &str, kind: SessionKind) -> Option<String> {
    let t = title.trim_start_matches(|c: char| !(c.is_alphanumeric() || c == '/' || c == '~'));
    let t: String = t
        .split_whitespace()
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if t.is_empty() {
        return None;
    }
    let lower = t.to_lowercase();
    const PROGRAMS: &[&str] = &["claude", "claude code", "codex", "codex cli"];
    const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "nu", "tcsh"];
    if PROGRAMS.contains(&lower.as_str())
        || lower == kind.as_str()
        || (kind == SessionKind::Shell && SHELLS.contains(&lower.as_str()))
    {
        return None;
    }
    if t.starts_with('/') || t.starts_with('~') || looks_like_user_host(&t) {
        return None;
    }
    Some(cap_chars(&t, NAME_MAX))
}

/// `user@host`, `user@host:~/dir ...`: a shell prompt title.
fn looks_like_user_host(t: &str) -> bool {
    let first = t.split_whitespace().next().unwrap_or("");
    let Some((user, rest)) = first.split_once('@') else {
        return false;
    };
    let host = rest.split(':').next().unwrap_or("");
    let ok = |s: &str| {
        !s.is_empty()
            && s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    ok(user) && ok(host)
}

/// Name from a prompt: its first non-empty line, first 6 words, at most 40 chars.
pub fn prompt_name(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let words: Vec<String> = line
        .split_whitespace()
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .take(6)
        .collect();
    let s = cap_chars(&words.join(" "), 40);
    (!s.is_empty()).then_some(s)
}

/// What `window.show` does for a window (`Hub::window_launch`).
pub enum WindowLaunch {
    /// A client shows it (its name, for raising it).
    Held(String),
    /// A launch for it is already on its way.
    Launching,
    /// Launch one now (recorded).
    Go,
}

/// A worktree directory marked as being deleted; unmarked when dropped, however the delete
/// ends (done, failed, or its future dropped).
struct WorktreeReservation {
    hub: Arc<Hub>,
    path: PathBuf,
}

impl Drop for WorktreeReservation {
    fn drop(&mut self) {
        let mut g = self.hub.inner.lock();
        if let Some(n) = g.removing_worktrees.get_mut(&self.path) {
            *n -= 1;
            if *n == 0 {
                g.removing_worktrees.remove(&self.path);
            }
        }
    }
}

/// The first running session (`(info, running)`) outside `ch` whose cwd is inside `ch.cwd`.
/// Whether a session's run may still own processes in its folder and port range: starting,
/// alive, leaving background processes behind, or launched without confirmation. An exited,
/// archived run kept only as a replay handle does not count.
fn occupies(e: &SessEntry, records: &BTreeMap<SessionId, StoredTerminal>) -> bool {
    e.busy || e.pty.as_ref().is_some_and(|p| p.owns_processes())
        || records.get(&e.info.id).is_some_and(|r| r.pending && !r.retired)
}

/// Reconcile a saved session with the host's facts for its run after a backend restart.
/// Returns true when it put the session in a `Working` it didn't see (to re-read a dialog).
fn recover_session_state(info: &mut SessionInfo, status: Option<&muster_terminal_protocol::TerminalStatus>,
    has_record: bool, now: i64) -> bool {
    if let Some(status) = status {
        info.pid = status.alive.then_some(status.pid).flatten();
        info.exit_code = status.exit_code;
        info.cols = status.cols;
        info.rows = status.rows;
        info.title = status.title.clone();
        if !status.alive {
            info.state = AgentState::Exited;
            info.state_detail = Some(format!("exit code {}", status.exit_code.unwrap_or(-1)));
            info.queued = 0;
        } else if info.state == AgentState::Exited {
            // The saved Exited belongs to an earlier run (a restart that crashed before
            // its new state was saved, by an older build). The live run is authoritative.
            info.state = AgentState::Starting;
            info.state_detail = None;
            info.state_since = now;
        } else if matches!(info.state, AgentState::NeedsApproval | AgentState::AskingQuestion) {
            // The saved dialog may have been answered and replaced by another one during the
            // outage. Never pair its description with a fresh, answerable state_seq: let the
            // detector read the dialog that is on screen now (it overrides hook precedence).
            info.state = AgentState::Working;
            info.state_detail = Some("Checking the terminal after a backend restart".into());
            info.state_since = now;
            return true;
        }
    } else if info.state != AgentState::Exited {
        info.state = AgentState::Exited;
        info.state_detail = Some(if has_record { "terminal host restarted or run was lost" } else { "legacy daemon restarted" }.into());
        info.pid = None;
        info.queued = 0;
    }
    false
}

/// Only a host's explicit per-run rejection proves that a launch did not start.
fn is_definite_spawn_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<crate::terminal::SpawnUncertain>().is_none()
}

fn dir_user<'a>(ch: &Channel, sessions: impl Iterator<Item = (&'a SessionInfo, bool)>) -> Option<SessionId> {
    let root = Path::new(&ch.cwd);
    sessions
        .filter(|(i, running)| *running && i.channel_id != ch.id && Path::new(&i.cwd).starts_with(root))
        .map(|(i, _)| i.id)
        .next()
}

/// How a hook changes the count of built-in subagents: +1 when the Agent/Task tool starts, -1
/// when it returns (or fails), reset (`Some(None)`) when a session starts or ends or a turn
/// stops. `None`: no
/// change. Background subagents return at launch, so only foreground ones are counted.
fn subagent_delta(event: &str, payload: &serde_json::Value) -> Option<Option<i32>> {
    let tool = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let agent_tool = matches!(tool, "Agent" | "Task");
    match event {
        "PreToolUse" if agent_tool => Some(Some(1)),
        "PostToolUse" | "PostToolUseFailure" if agent_tool => Some(Some(-1)),
        // A turn's end: foreground subagents are done by then (and a missed PostToolUse can't
        // leave the count stuck past it).
        "SessionStart" | "SessionEnd" | "Stop" | "StopFailure" => Some(None),
        _ => None,
    }
}

/// Where an auto-named channel's name came from, worst to best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameSource {
    Folder,
    ShellTitle,
    FirstPrompt,
    AgentTitle,
}

/// An auto-named channel's name: agent title > first prompt > shell title > folder (inputs
/// already cleaned), and where it came from.
pub fn derive_name(
    agent_title: Option<&str>,
    first_prompt: Option<&str>,
    shell_title: Option<&str>,
    folder: &str,
) -> (String, NameSource) {
    let (n, src) = match (agent_title, first_prompt, shell_title) {
        (Some(n), _, _) => (n, NameSource::AgentTitle),
        (None, Some(n), _) => (n, NameSource::FirstPrompt),
        (None, None, Some(n)) => (n, NameSource::ShellTitle),
        (None, None, None) => (folder, NameSource::Folder),
    };
    (cap_chars(n, NAME_MAX), src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned() {
        let c = SessionKind::Claude;
        assert_eq!(clean_title("✳ Fix the auth bug", c).as_deref(), Some("Fix the auth bug"));
        assert_eq!(clean_title("⠂ Fix the auth bug ", c).as_deref(), Some("Fix the auth bug"));
        assert_eq!(clean_title("⠐  Refactor   parser", c).as_deref(), Some("Refactor parser"));
        assert_eq!(clean_title("· Running tests", c).as_deref(), Some("Running tests"));
        // Program names, blank, glyph-only.
        for t in ["✳ Claude Code", "claude", "CODEX", "Codex CLI", "  ", "✳", "⠂ ·"] {
            assert_eq!(clean_title(t, c), None, "{t}");
        }
        // The kind name and shell names, for shells.
        assert_eq!(clean_title("shell", SessionKind::Shell), None);
        assert_eq!(clean_title("fish", SessionKind::Shell), None);
        assert_eq!(clean_title("bash", SessionKind::Shell), None);
        assert_eq!(clean_title("cargo test", SessionKind::Shell).as_deref(), Some("cargo test"));
        // Paths and user@host prompts.
        for t in ["/home/josh/p/muster", "~/p/muster", "~", "josh@arch: ~/p", "josh@arch:~/p/muster", "root@box"] {
            assert_eq!(clean_title(t, SessionKind::Shell), None, "{t}");
        }
        // Mail-like words elsewhere are fine; long titles are capped.
        assert_eq!(clean_title("ask bob@example.com", c).as_deref(), Some("ask bob@example.com"));
        let long = "word ".repeat(30);
        assert_eq!(clean_title(&long, c).unwrap().chars().count(), NAME_MAX - 1);
        assert!(clean_title("x\u{7}y", c).is_some_and(|t| t == "xy"));
    }

    #[test]
    fn prompt_names() {
        assert_eq!(prompt_name("fix the flaky login test please now and more").as_deref(), Some("fix the flaky login test please"));
        assert_eq!(prompt_name("\n\n  hello world\nsecond line").as_deref(), Some("hello world"));
        assert_eq!(prompt_name("   \n "), None);
        let n = prompt_name("supercalifragilisticexpialidocious-and-more-words-here x").unwrap();
        assert!(n.chars().count() <= 40, "{n}");
    }

    #[test]
    fn name_priority() {
        use NameSource::*;
        assert_eq!(derive_name(Some("Agent"), Some("prompt"), Some("sh"), "repo"), ("Agent".into(), AgentTitle));
        assert_eq!(derive_name(None, Some("prompt"), Some("sh"), "repo"), ("prompt".into(), FirstPrompt));
        assert_eq!(derive_name(None, None, Some("cargo test"), "repo"), ("cargo test".into(), ShellTitle));
        assert_eq!(derive_name(None, None, None, "repo"), ("repo".into(), Folder));
        assert!(AgentTitle > FirstPrompt && FirstPrompt > ShellTitle && ShellTitle > Folder);
    }

    #[test]
    fn initial_branches() {
        assert!(is_initial_branch("muster/7", 7));
        assert!(is_initial_branch("muster/7-2", 7));
        assert!(!is_initial_branch("muster/70", 7));
        assert!(!is_initial_branch("muster/7-x", 7));
        assert!(!is_initial_branch("muster/7-", 7));
        assert!(!is_initial_branch("muster/fix-auth", 7));
    }

    #[test]
    fn detector_verdict_cannot_update_a_replacement_terminal() {
        let hub = move_fixture();
        {
            let mut g = hub.inner.lock();
            let entry = g.sessions.get_mut(&2).unwrap();
            entry.info.state = AgentState::Working;
            entry.info.state_detail = None;
            entry.generation = 8;
        }
        assert!(!hub.set_state_checked(2, StateUpdate::new(AgentState::Stuck), StateSource::Timer, Some(7)));
        assert_eq!(hub.session(2).unwrap().state, AgentState::Working);
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Stuck), StateSource::Timer, Some(8)));
        assert_eq!(hub.session(2).unwrap().state, AgentState::Stuck);
    }

    /// A hub over an in-memory store: workspace `/w/repo`, auto-named channels 1 and 2 (window
    /// 1), archived channel 3, sessions of channel 1 (window 1): a shell titled "build the
    /// widget" and a claude without a title.
    fn move_fixture() -> Arc<Hub> {
        fixture(None)
    }

    fn fixture(terminal_host: Option<Arc<TerminalService>>) -> Arc<Hub> {
        fixture_with(terminal_host, |_| {})
    }

    /// `move_fixture`, with session 2 (Claude, in channel 1) a named agent's.
    fn agent_fixture() -> Arc<Hub> {
        provider_fixture(None, None, Default::default())
    }

    /// `agent_fixture` with the agent on `provider`, a terminal host and an OpenRouter key.
    fn provider_fixture(terminal_host: Option<Arc<TerminalService>>, key: Option<crate::openrouter::ApiKey>,
        provider: crate::openrouter::Provider) -> Arc<Hub> {
        fixture_keyed(terminal_host, key, |store| {
            store
                .insert_agent(&crate::agents::Agent {
                    account_profile_id: None,
                    provider,
                    id: 0,
                    name: "ada".into(),
                    kind: SessionKind::Claude,
                    model: None,
                    effort: None,
                    persona: String::new(),
                    home_dir: "/x".into(),
                    permissions: Permissions::default(),
                    accepts_origins: crate::agents::default_accepts(SessionKind::Claude),
                    channel_id: Some(1),
                    session_id: Some(2),
                    resume_id: None,
                    created_at: 0,
                    autostart: false,
                })
                .unwrap();
        })
    }

    fn fixture_with(terminal_host: Option<Arc<TerminalService>>, setup: impl FnOnce(&Store)) -> Arc<Hub> {
        fixture_keyed(terminal_host, None, setup)
    }

    fn fixture_keyed(terminal_host: Option<Arc<TerminalService>>, openrouter_key: Option<crate::openrouter::ApiKey>,
        setup: impl FnOnce(&Store)) -> Arc<Hub> {
        let store = Store::open_memory().unwrap();
        let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
        let ch = |name: &str, archived| Channel {
            id: 0,
            workspace_id: ws,
            name: name.into(),
            cwd: "/w/repo".into(),
            branch: None,
            base_branch: None,
            owns_worktree: false,
            port_base: 20000,
            archived,
            layout: None,
            created_at: 0,
            last_read_at: 0,
            sort_order: 0,
            permissions: Permissions::default(),
            window_id: Some(1),
            auto_named: true,
            first_prompt: None,
        };
        assert_eq!(store.insert_channel(&ch("build the widget", false)).unwrap(), 1);
        assert_eq!(store.insert_channel(&ch("repo", false)).unwrap(), 2);
        assert_eq!(store.insert_channel(&ch("old", true)).unwrap(), 3);
        store.upsert_window(1, "1", 0, 0).unwrap();
        for (kind, title) in [(SessionKind::Shell, Some("✳ build the widget")), (SessionKind::Claude, None)] {
            let mut info = crate::mcp::guard::test_session(0, None);
            info.kind = kind;
            info.title = title.map(Into::into);
            info.window_id = Some(1);
            store
                .insert_session(&StoredSession {
                    info,
                    extra_args: vec![],
                })
                .unwrap();
        }
        setup(&store);
        let cfg = HubConfig {
            port: 0,
            client_token: "t".into(),
            muster_bin: PathBuf::from("/bin/true"),
            terminal_host,
            openrouter_key,
        };
        Hub::new(cfg, store).unwrap()
    }

    /// Usage reports are attributed by the run's token alone: to that session's pinned profile,
    /// whatever the payload says, and never from a shell, an unknown or a replaced token.
    #[test]
    fn usage_reports_follow_the_session_to_profile_mapping() {
        let hub = fixture(None);
        let base = std::env::temp_dir().join(format!("muster-hub-usage-{}", paths::random_token()));
        std::fs::create_dir_all(base.join("personal")).unwrap();
        std::fs::create_dir_all(base.join("work")).unwrap();
        let personal = hub.add_account_profile("Personal", base.join("personal").to_str().unwrap()).unwrap();
        let work = hub.add_account_profile("Work", base.join("work").to_str().unwrap()).unwrap();
        let generation = {
            let mut g = hub.inner.lock();
            g.sessions.get_mut(&2).unwrap().info.account_profile_id = Some(work.id);
            g.tokens.insert("claude-run".into(), 2);
            g.tokens.insert("shell-run".into(), 1);
            g.sessions[&2].generation
        };
        let resets = now_ms() / 1000 + 3 * 3600;
        let body = |five: f64, api: u64| serde_json::json!({
            "rate_limits": {"five_hour": {"used_percentage": five, "resets_at": resets},
                "seven_day": {"used_percentage": 40, "resets_at": resets + 86_400}},
            "api_duration_ms": api,
            // Labels in the payload are ignored: the run's launch decides.
            "account": "Personal", "account_profile_id": personal.id, "config_dir": personal.config_dir});
        let row = |hub: &Hub, name: &str| hub.account_usage().unwrap().rows.into_iter().find(|r| r.name == name).unwrap();
        // A run Muster has no launch record for reports nothing.
        hub.record_usage("claude-run", &body(25.0, 100)).unwrap();
        assert_eq!(row(&hub, "Work").five_hour.status, crate::usage::Status::Unknown);
        hub.store.start_usage_run(2, generation, Some(work.config_dir.clone())).unwrap();
        // The run's first report has no baseline: shown, not confirmed fresh.
        hub.record_usage("claude-run", &body(24.0, 50)).unwrap();
        assert_eq!(row(&hub, "Work").five_hour.used_percentage, Some(24.0));
        assert_eq!(row(&hub, "Work").five_hour.status, crate::usage::Status::Unconfirmed);
        // More API time than that report: fresh.
        hub.record_usage("claude-run", &body(25.0, 100)).unwrap();
        assert_eq!(row(&hub, "Work").five_hour.used_percentage, Some(25.0));
        assert_eq!(row(&hub, "Work").five_hour.status, crate::usage::Status::Current);
        assert_eq!(row(&hub, "Personal").five_hour.status, crate::usage::Status::Unknown);
        assert_eq!(row(&hub, "Default").five_hour.status, crate::usage::Status::Unknown);
        assert!(hub.record_usage("shell-run", &body(1.0, 1)).is_err());
        assert!(hub.record_usage("forged", &body(1.0, 1)).is_err());
        // Another generation's launch record: the report is ignored.
        hub.store.start_usage_run(2, generation + 1, Some(personal.config_dir.clone())).unwrap();
        hub.record_usage("claude-run", &body(2.0, 900)).unwrap();
        assert_eq!(row(&hub, "Personal").five_hour.status, crate::usage::Status::Unknown);
        hub.store.start_usage_run(2, generation, Some(work.config_dir.clone())).unwrap();
        // A replaced run's token no longer reports anything.
        hub.inner.lock().tokens.remove("claude-run");
        assert!(hub.record_usage("claude-run", &body(1.0, 900)).is_err());
        assert_eq!(row(&hub, "Work").five_hour.used_percentage, Some(25.0));
        // Declared one account: Personal shows Work's numbers, credited to Work.
        hub.set_same_account(personal.id, Some(work.id)).unwrap();
        let p = row(&hub, "Personal");
        assert_eq!((p.five_hour.used_percentage, p.five_hour.reported_by.as_deref()), (Some(25.0), Some("Work")));
        assert_eq!(p.same_account_as, vec!["Work".to_string()]);
        // A login change forgets Work's usage and its same-account declaration.
        hub.account_login_changed(crate::account_profiles::ProfileChoice::Id(work.id)).unwrap();
        assert_eq!(row(&hub, "Work").five_hour.status, crate::usage::Status::Unknown);
        assert!(hub.account_profile(work.id).unwrap().account_group.is_none());
        assert!(hub.set_same_account(work.id, Some(work.id)).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Default and a profile on the same folder are one login: "login changed" on either forgets
    /// the folder's figures (whichever row's session reported them) and that profile's declaration.
    #[test]
    fn a_login_change_covers_every_row_on_the_folder() {
        let hub = fixture(None);
        // Default's folder is the daemon's own (CLAUDE_CONFIG_DIR or ~/.claude); skip where none exists.
        let Some(default_dir) = crate::usage::effective_config_dir(&std::env::vars().collect()) else { return };
        let default_dir = default_dir.to_string_lossy().into_owned();
        let other = std::env::temp_dir().join(format!("muster-hub-usage-{}", paths::random_token()));
        std::fs::create_dir_all(&other).unwrap();
        let main = hub.add_account_profile("Main", &default_dir).unwrap();
        let spare = hub.add_account_profile("Spare", other.to_str().unwrap()).unwrap();
        hub.set_same_account(main.id, Some(spare.id)).unwrap();
        let generation = {
            let mut g = hub.inner.lock();
            g.tokens.insert("claude-run".into(), 2);
            g.sessions[&2].generation
        };
        hub.store.start_usage_run(2, generation, Some(default_dir.clone())).unwrap();
        let resets = now_ms() / 1000 + 3 * 3600;
        hub.record_usage("claude-run", &serde_json::json!({"api_duration_ms": 0})).unwrap();
        hub.record_usage("claude-run", &serde_json::json!({"api_duration_ms": 10, "rate_limits": {
            "five_hour": {"used_percentage": 30, "resets_at": resets}}})).unwrap();
        assert_eq!(hub.account_usage().unwrap().rows[0].five_hour.status, crate::usage::Status::Current);
        let row = |hub: &Hub, name: &str| hub.account_usage().unwrap().rows.into_iter().find(|r| r.name == name).unwrap();
        assert_eq!(row(&hub, "Main").five_hour.used_percentage, Some(30.0));
        hub.account_login_changed(crate::account_profiles::ProfileChoice::pinned(None)).unwrap();
        for name in ["Default", "Main", "Spare"] {
            assert_eq!(row(&hub, name).five_hour.status, crate::usage::Status::Unknown, "{name}");
        }
        assert!(hub.account_profile(main.id).unwrap().account_group.is_none());
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn default_profile_keeps_the_inherited_claude_config_dir() {
        let hub = fixture(None);
        let home = std::env::temp_dir().join(format!("muster-hub-profile-{}", paths::random_token()));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join("work")).unwrap();
        let env = |id| {
            let mut env = BTreeMap::from([
                ("HOME".to_string(), home.display().to_string()),
                ("CLAUDE_CONFIG_DIR".to_string(), "/previous".to_string()),
            ]);
            hub.apply_account_profile(id, &mut env).unwrap();
            env.get("CLAUDE_CONFIG_DIR").cloned()
        };
        assert_eq!(env(None).as_deref(), Some("/previous"));
        let at_default = hub.add_account_profile("Main", &format!("{}/.claude/", home.display())).unwrap();
        assert_eq!(env(Some(at_default.id)), None);
        let work = hub.add_account_profile("Work", home.join("work").to_str().unwrap()).unwrap();
        assert_eq!(env(Some(work.id)), Some(work.config_dir));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Give session `id` a live run on `service` (as an acknowledged launch would).
    fn install_terminal(hub: &Hub, service: &TerminalService, id: SessionId, generation: u64, directory: &Path)
        -> Arc<PtySession> {
        let terminal = crate::terminal::testing::live(service, id, generation, directory);
        let mut g = hub.inner.lock();
        let entry = g.sessions.get_mut(&id).unwrap();
        entry.generation = generation;
        hub.set_pty(id, entry, Some(terminal.clone()));
        terminal
    }

    fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn remember_launch(hub: &Hub, pending: bool) {
        let record = StoredTerminal { host_instance: "fixture-host".into(), token: "run-token".into(), key_refs: Default::default(), key_fp: None,
            port_base: 20000, pending, retired: false,
            spec: SpawnSpec { run: RunId { id: 1, generation: 42 }, program: "/bin/true".into(),
                args: vec![], env: BTreeMap::new(), cwd: std::env::temp_dir(), cols: 80, rows: 24 } };
        hub.store.save_terminal(1, &record).unwrap();
        hub.terminal_records.lock().insert(1, record);
    }

    #[test]
    fn failed_removal_keeps_the_session_and_its_live_launch_record() {
        let hub = move_fixture();
        remember_launch(&hub, false);
        hub.store.fail_session_removal_for_test();
        assert!(hub.remove_session(1).is_err());
        assert!(hub.session(1).is_some());
        assert!(hub.store.load_sessions().unwrap().iter().any(|s| s.info.id == 1));
        assert!(!hub.terminal_records.lock()[&1].retired);
        assert!(!hub.store.load_terminals().unwrap()[&1].retired);
    }

    #[test]
    fn starting_again_cannot_overwrite_an_unconfirmed_launch() {
        let hub = move_fixture();
        remember_launch(&hub, true);
        assert!(hub.start_process(1, None, None).err().unwrap().to_string().contains("unconfirmed"));
        let saved = hub.store.load_terminals().unwrap().remove(&1).unwrap();
        assert_eq!(saved.spec.run.generation, 42);
        assert!(saved.pending && !saved.retired);
        assert_eq!(saved.token, "run-token");
        hub.remove_session(1).unwrap();
        assert!(hub.session(1).is_none());
        assert!(hub.store.load_sessions().unwrap().iter().all(|s| s.info.id != 1));
        assert!(hub.store.load_terminals().unwrap()[&1].retired);
    }

    #[test]
    fn an_initial_prompt_queued_before_launch_survives_until_the_launch_resolves() {
        let hub = move_fixture();
        remember_launch(&hub, true);
        {
            let mut g = hub.inner.lock();
            let entry = g.sessions.get_mut(&1).unwrap();
            entry.generation = 42;
            entry.info.state = AgentState::Starting;
        }
        crate::inject::prepare_initial(&hub, 1, 42, "make test".into(), crate::agents::Source::Human).unwrap();
        // Not live yet (no acknowledgement): the prepared launch keeps its queue.
        crate::inject::kick_initial(&hub, 1);
        assert_eq!(crate::inject::inspect(&hub, 1)["queued"][0], "make test");
        assert_eq!(hub.session(1).unwrap().queued, 1);
        let saved: std::collections::HashMap<SessionId, serde_json::Value> =
            hub.store.load_runtime("prompt-queues").unwrap().unwrap();
        assert!(saved[&1].to_string().contains("make test"), "saved before the launch is sent");
        // Once the launch is known not to run, the queue goes.
        hub.terminal_records.lock().get_mut(&1).unwrap().retired = true;
        crate::inject::kick_initial(&hub, 1);
        assert!(crate::inject::inspect(&hub, 1)["queued"].as_array().unwrap().is_empty());
    }

    #[test]
    fn hook_count_state_and_grace_are_checkpointed_without_duplicate_effects() {
        let hub = move_fixture();
        let record = StoredTerminal { host_instance: "fixture".into(), token: "callback-token".into(), key_refs: Default::default(), key_fp: None,
            port_base: 20000, pending: false, retired: false,
            spec: SpawnSpec { run: RunId { id: 2, generation: 42 }, program: "/bin/true".into(), args: vec![],
                env: BTreeMap::new(), cwd: "/".into(), cols: 80, rows: 24 } };
        hub.terminal_records.lock().insert(2, record);
        {
            let mut g = hub.inner.lock();
            let entry = g.sessions.get_mut(&2).unwrap();
            entry.generation = 42;
            entry.info.state = AgentState::Idle;
        }
        let mut cursor = crate::store::HookCursor { instance: "fixture".into(), sequence: 1 };
        let payload = serde_json::json!({"tool_name":"Agent"});
        // A callback that arrives before the launch is acknowledged is applied, not held at
        // the head of the inbox (that stalled every other session's callbacks and input).
        // start_process installed the initial state with the launch record and leaves it.
        hub.terminal_records.lock().get_mut(&2).unwrap().pending = true;
        hub.handle_spooled_hook("callback-token", "PreToolUse", payload.clone(), &cursor, None).unwrap();
        hub.terminal_records.lock().get_mut(&2).unwrap().pending = false;
        let info = hub.session(2).unwrap();
        assert_eq!((info.state, info.subagents), (AgentState::Working, 1));
        let checkpoint = hub.store.load_hook_session(2, 42).unwrap().unwrap();
        assert_eq!(checkpoint.subagents, 1);
        assert!(checkpoint.last_hook_at > 0);
        assert_eq!(hub.inner.lock().sessions[&2].last_hook_at, checkpoint.last_hook_at);
        assert_eq!(hub.store.load_sessions().unwrap()[1].info.state, AgentState::Working);
        hub.handle_spooled_hook("callback-token", "PreToolUse", payload, &cursor, None).unwrap();
        assert_eq!(hub.session(2).unwrap().subagents, 1);
        assert_eq!(hub.session(2).unwrap().state_seq, info.state_seq);
        cursor.sequence += 1;
        hub.handle_spooled_hook("callback-token", "Stop", serde_json::json!({}), &cursor, None).unwrap();
        assert_eq!(hub.session(2).unwrap().subagents, 0);
        assert_eq!(hub.store.load_hook_session(2, 42).unwrap().unwrap().subagents, 0);
    }

    #[test]
    fn callback_boundary_finishes_the_old_reply_before_attributing_the_next_prompt() {
        use crate::agents::{AgentMessage, Origin, Prov, Source};
        let hub = move_fixture();
        let directory = std::env::temp_dir().join(format!("muster-input-boundary-{}", paths::random_token()));
        let mut inbox = crate::hook_inbox::Inbox::open(&directory.join("hooks.sqlite")).unwrap();
        let record = StoredTerminal { host_instance: "fixture".into(), token: "callback-token".into(), key_refs: Default::default(), key_fp: None,
            port_base: 20000, pending: false, retired: false,
            spec: SpawnSpec { run: RunId { id: 2, generation: 42 }, program: "/bin/true".into(), args: vec![],
                env: BTreeMap::new(), cwd: directory.clone(), cols: 80, rows: 24 } };
        hub.terminal_records.lock().insert(2, record);
        hub.inner.lock().sessions.get_mut(&2).unwrap().generation = 42;
        let message = || hub.store.insert_agent_message(&AgentMessage { id: 0, from: "fixture".into(),
            from_session: None, to_agent: 1, to_session: Some(2), origin: Origin::Schedule, hop: 1,
            reply_to: None, created_at: 1, reply: None, replied_at: None, text: None, refused: None, to_name: None, cold_idle_ms: None }).unwrap();
        let first = message();
        let next = message();
        let source = |msg| Source::Chain(Prov { origin: Origin::Schedule, hop: 1, msg_id: Some(msg) });
        hub.agents.on_delivered(2, source(first), 100).unwrap();
        inbox.enqueue("callback-token", "Stop", br#"{"last_assistant_message":"old reply"}"#).unwrap();
        *hub.callback_inbox.lock() = Some(inbox);
        hub.note_prompt_delivery(2, Some(42), source(next), 200).unwrap();
        assert_eq!(hub.store.agent_message(first).unwrap().unwrap().reply.as_deref(), Some("old reply"));
        assert!(hub.store.agent_message(next).unwrap().unwrap().reply.is_none());
        assert!(hub.agents.msg_status(&hub.store, next).active);
        // A stale generation cannot modify the current run's attribution.
        assert!(hub.note_prompt_delivery(2, Some(41), Source::Human, 300).is_err());
        assert!(hub.agents.msg_status(&hub.store, next).active);

        // An older prompt callback must not consume the human-input marker that
        // belongs to a later keystroke and accidentally lower provenance. The marker is
        // only recorded on the input path; its boundary drains the older callback first.
        let enqueue = |event: &str, payload: &[u8]| hub.callback_inbox.lock().as_mut().unwrap()
            .enqueue("callback-token", event, payload).unwrap();
        let drain = || {
            let inbox = hub.callback_inbox.lock();
            crate::hook_inbox::drain(&hub, inbox.as_ref().unwrap()).unwrap();
        };
        enqueue("UserPromptSubmit", br#"{"prompt":"earlier input"}"#);
        hub.note_human_input(2);
        hub.flush_human_input().unwrap();
        assert_eq!(hub.agents.prov(2).unwrap().origin, Origin::Schedule);
        enqueue("UserPromptSubmit", br#"{"prompt":"new human input"}"#);
        drain();
        assert_eq!(hub.agents.prov(2), None);

        // While a marker is still uncommitted, the background consumer drains callbacks
        // without it: one that may predate the keystroke cannot consume it.
        hub.note_prompt_delivery(2, Some(42), source(message()), 400).unwrap();
        enqueue("UserPromptSubmit", br#"{"prompt":"typed before the keystroke reached us"}"#);
        hub.note_human_input(2);
        drain();
        assert_eq!(hub.agents.prov(2).unwrap().origin, Origin::Schedule, "an uncommitted marker is never consumed");
        hub.flush_human_input().unwrap();
        enqueue("UserPromptSubmit", br#"{"prompt":"after the keystroke"}"#);
        drain();
        assert_eq!(hub.agents.prov(2), None, "the committed marker belongs to the next prompt");

        // A keystroke followed by a prompt delivery: the boundary of the delivery commits
        // the earlier marker first and then clears it, so the marker can never be applied
        // after the delivery and hand the delivered (envelope-less) prompt to the human.
        let chain = Prov { origin: Origin::Schedule, hop: 1, msg_id: None };
        hub.note_human_input(2);
        hub.note_prompt_delivery(2, Some(42), Source::Chain(chain), 500).unwrap();
        hub.flush_human_input().unwrap();
        enqueue("UserPromptSubmit", br#"{"prompt":"scheduled task text"}"#);
        drain();
        assert_eq!(hub.agents.prov(2), Some(chain), "a delivered prompt keeps its chain");
        drop(hub);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keystrokes_never_wait_on_the_callback_lock_or_sqlite() {
        let hub = move_fixture();
        let directory = std::env::temp_dir().join(format!("muster-input-hot-path-{}", paths::random_token()));
        let path = directory.join("hooks.sqlite");
        *hub.callback_inbox.lock() = Some(crate::hook_inbox::Inbox::open(&path).unwrap());
        let (held, release) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // A slow callback batch: the callback lock and the inbox write lock are held.
                let _callbacks = hub.callback_inbox.lock();
                let writer = rusqlite::Connection::open(&path).unwrap();
                writer.execute_batch("BEGIN IMMEDIATE").unwrap();
                held.wait();
                release.wait();
                writer.execute_batch("COMMIT").unwrap();
            });
            held.wait();
            let started = std::time::Instant::now();
            for _ in 0..1000 {
                hub.note_human_input(2);
            }
            assert!(started.elapsed() < Duration::from_millis(50), "keystrokes waited: {:?}", started.elapsed());
            release.wait();
        });
        hub.flush_human_input().unwrap();
        drop(hub);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_callback_never_consumes_a_marker_recorded_after_it_arrived() {
        use crate::agents::{Origin, Prov};
        let hub = move_fixture();
        hub.inner.lock().tokens.insert("legacy-token".into(), 2);
        let scheduled = Prov { origin: Origin::Schedule, hop: 1, msg_id: None };
        let prompt = |text: &str| serde_json::json!({ "prompt": text });

        // The callback for a scheduled turn arrives; the human types afterwards and the
        // attribution worker commits that marker before the callback gets its boundary.
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        let arrival = hub.human_input_seq();
        hub.note_human_input(2);
        hub.flush_human_input().unwrap();
        hub.handle_hook_at("legacy-token", "UserPromptSubmit", prompt("scheduled task"), arrival).unwrap();
        assert_eq!(hub.agents.prov(2), Some(scheduled), "a later keystroke never lowers an earlier turn");
        // The marker stays for the human's own prompt.
        hub.handle_hook("legacy-token", "UserPromptSubmit", prompt("typed by the human")).unwrap();
        assert_eq!(hub.agents.prov(2), None);

        // Same, with the callback's own boundary committing the later marker.
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        let arrival = hub.human_input_seq();
        hub.note_human_input(2);
        hub.handle_hook_at("legacy-token", "UserPromptSubmit", prompt("scheduled task"), arrival).unwrap();
        assert_eq!(hub.agents.prov(2), Some(scheduled));
        hub.handle_hook("legacy-token", "UserPromptSubmit", prompt("typed by the human")).unwrap();
        assert_eq!(hub.agents.prov(2), None);

        // Typing before and after the callback arrived: this prompt is the human's, and so
        // is the next one.
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        hub.note_human_input(2);
        let arrival = hub.human_input_seq();
        hub.note_human_input(2);
        hub.handle_hook_at("legacy-token", "UserPromptSubmit", prompt("typed"), arrival).unwrap();
        assert_eq!(hub.agents.prov(2), None);
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        hub.handle_hook("legacy-token", "UserPromptSubmit", prompt("typed again")).unwrap();
        assert_eq!(hub.agents.prov(2), None, "the later input still marks the next prompt");
        // Consumed: a further plain prompt keeps its provenance.
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        hub.handle_hook("legacy-token", "UserPromptSubmit", prompt("no human input")).unwrap();
        assert_eq!(hub.agents.prov(2), Some(scheduled));
    }

    #[test]
    fn an_openrouter_agent_gets_the_gateway_and_no_record_holds_the_key() {
        use crate::openrouter::{ApiKey, Provider};
        const KEY: &str = "sk-or-fixture-secret-4242";
        let (service, directory, received) = crate::terminal::testing::serving_host("hub-openrouter");
        let hub = provider_fixture(Some(service.clone()), ApiKey::new(KEY), Provider::OpenRouter);
        assert_eq!(hub.session_provider(2), Provider::OpenRouter);
        assert_eq!(hub.session_provider(1), Provider::Subscription);
        hub.check_session_provider(2).unwrap();
        for id in [1, 2] {
            let _ = hub.start_process(id, None, None); // the fixture host refuses every launch
        }
        let spawns = received.spawns.lock().clone();
        assert_eq!(spawns.len(), 2, "{spawns:?}");
        let (shell, claude) = (&spawns[0], &spawns[1]);
        assert_eq!(claude.env["ANTHROPIC_BASE_URL"], "https://openrouter.ai/api");
        assert_eq!(claude.env["ANTHROPIC_AUTH_TOKEN"], KEY);
        assert_eq!(claude.env["ANTHROPIC_API_KEY"], "");
        assert_eq!(claude.env["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"], "1");
        assert_eq!(claude.env["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"], "1");
        // Its hooks lose MUSTER_TOKEN to that scrub; they read the run's token from its folder.
        let token_file = paths::session_dir(2).join(crate::hookcmd::TOKEN_FILE);
        assert_eq!(std::fs::read_to_string(&token_file).unwrap(), claude.env["MUSTER_TOKEN"]);
        assert!(!paths::session_dir(1).join(crate::hookcmd::TOKEN_FILE).exists(), "only where the scrub runs");
        assert!(!claude.env.contains_key("OPENROUTER_API_KEY"));
        for var in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "OPENROUTER_API_KEY", "MUSTER_PROVIDER"] {
            assert!(!shell.env.contains_key(var), "a subscription session never gets {var}");
        }
        assert!(!shell.env.values().any(|v| v.contains(KEY)));
        // The launch records Muster keeps (memory and database) have a placeholder instead.
        for record in hub.store.load_terminals().unwrap().values().chain(hub.terminal_records.lock().values()) {
            assert!(!serde_json::to_string(record).unwrap().contains(KEY));
        }
        assert!(!serde_json::to_string(&hub.sessions()).unwrap().contains(KEY));
        drop((hub, service));
        let _ = std::fs::remove_dir_all(directory);

        // Without a key, an OpenRouter agent's session doesn't start (nothing reaches the host).
        let (service, directory, received) = crate::terminal::testing::serving_host("hub-openrouter-nokey");
        let hub = provider_fixture(Some(service.clone()), None, Provider::OpenRouter);
        assert!(hub.check_session_provider(2).unwrap_err().to_string().contains("OPENROUTER_API_KEY"));
        assert!(hub.start_process(2, None, None).err().unwrap().to_string().contains("OPENROUTER_API_KEY"));
        assert!(received.spawns.lock().is_empty());
        drop((hub, service));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_pending_openrouter_launch_is_adopted_or_deferred_never_rebuilt() {
        use crate::openrouter::{ApiKey, Provider};
        const KEY: &str = "sk-or-fixture-secret-4242";
        let key = ApiKey::new(KEY).unwrap();
        let run = RunId { id: 2, generation: 42 };
        // A launch prepared for OpenRouter, saved (redacted) before a crash.
        let prepared = |key: &ApiKey| {
            let (spec, key_refs) = crate::openrouter::redact_spec(SpawnSpec { run, program: "/bin/true".into(), args: vec![],
                env: BTreeMap::from([
                    ("ANTHROPIC_AUTH_TOKEN".to_string(), key.expose().to_string()),
                    ("MUSTER_PROVIDER".to_string(), "openrouter".to_string()),
                ]),
                cwd: std::env::temp_dir(), cols: 80, rows: 24 }, Some(key), &["ANTHROPIC_AUTH_TOKEN".to_string()]);
            StoredTerminal { host_instance: "input-host".into(), token: "run-token".into(),
                port_base: 20000, pending: true, retired: false, spec, key_refs, key_fp: None }
        };
        let status = |directory: &Path| muster_terminal_protocol::TerminalStatus { run, pid: Some(1), alive: true,
            drained: false, has_processes: true, exit_code: None, title: None, cols: 80, rows: 24, output_seq: 0,
            last_output_at: 0, application_cursor: false, log_persisted: true, cwd: directory.to_owned() };
        let with_agent = |store: &Store, record: &StoredTerminal| {
            store.insert_agent(&crate::agents::Agent {
                account_profile_id: None, provider: Provider::OpenRouter, id: 0, name: "ada".into(),
                kind: SessionKind::Claude, model: None, effort: None, persona: String::new(), home_dir: "/x".into(),
                permissions: Permissions::default(), accepts_origins: crate::agents::default_accepts(SessionKind::Claude),
                channel_id: Some(1), session_id: Some(2), resume_id: None, created_at: 0, autostart: false,
            }).unwrap();
            store.save_terminal(2, record).unwrap();
        };

        // The host already started it, and the backend now has another key (or none): it is
        // adopted as it runs, never re-sent (the host would refuse the different launch and
        // the recovery would retire, i.e. kill, the live process).
        for now_key in [None, ApiKey::new("sk-or-a-different-key-77")] {
            let directory = std::env::temp_dir();
            let (service, dir, received) = crate::terminal::testing::serving_host_with("hub-or-adopt", vec![status(&directory)]);
            let hub = fixture_keyed(Some(service.clone()), now_key, |store| with_agent(store, &prepared(&key)));
            assert!(received.spawns.lock().is_empty(), "nothing re-sent");
            let saved = hub.store.load_terminals().unwrap().remove(&2).unwrap();
            assert!(!saved.pending && !saved.retired, "adopted: {saved:?}");
            assert!(hub.session(2).unwrap().state != AgentState::Exited);
            drop((hub, service));
            let _ = std::fs::remove_dir_all(dir);
        }

        // Never started, and this backend has no key: it waits (nothing runs, nothing retired),
        // and a new start says why.
        let (service, dir, received) = crate::terminal::testing::serving_host("hub-or-defer");
        let hub = fixture_keyed(Some(service.clone()), None, |store| with_agent(store, &prepared(&key)));
        assert!(received.spawns.lock().is_empty());
        let saved = hub.store.load_terminals().unwrap().remove(&2).unwrap();
        assert!(saved.pending && !saved.retired);
        hub.reconcile_pending().unwrap();
        assert!(received.spawns.lock().is_empty(), "reconcile_pending defers it too");
        assert!(format!("{:#}", hub.start_process(2, None, None).err().unwrap()).contains("OPENROUTER_API_KEY"));
        drop((hub, service));
        let _ = std::fs::remove_dir_all(dir);

        // Never started, and the key is back: re-sent with it.
        let (service, dir, received) = crate::terminal::testing::serving_host("hub-or-resend");
        let _hub = fixture_keyed(Some(service.clone()), Some(key.clone()), |store| with_agent(store, &prepared(&key)));
        let spawns = received.spawns.lock().clone();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].env["ANTHROPIC_AUTH_TOKEN"], KEY);
        drop((_hub, service));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keystrokes_reach_the_terminal_while_a_callback_commit_holds_the_hub_lock() {
        let (service, directory, received) = crate::terminal::testing::serving_host("hub-hot-path");
        let hub = move_fixture();
        let terminal = install_terminal(&hub, &service, 2, 42, &directory);
        let record = StoredTerminal { host_instance: "fixture".into(), token: "callback-token".into(), key_refs: Default::default(), key_fp: None,
            port_base: 20000, pending: false, retired: false,
            spec: SpawnSpec { run: terminal.run(), program: "/bin/true".into(), args: vec![],
                env: BTreeMap::new(), cwd: directory.clone(), cols: 80, rows: 24 } };
        hub.terminal_records.lock().insert(2, record);
        let cursor = crate::store::HookCursor { instance: "fixture".into(), sequence: 1 };
        std::thread::scope(|scope| {
            // A slow SQLite statement: the application database connection is busy.
            let database = hub.store.hold_connection_for_test();
            // A real callback reaches its checkpoint and waits for the database while it
            // holds the hub lock (as hook processing does across its commit).
            let callback = scope.spawn(|| hub.handle_spooled_hook("callback-token", "PreToolUse",
                serde_json::json!({"tool_name": "Agent"}), &cursor, None));
            wait_until("the callback to hold the hub lock", || hub.inner.is_locked());
            std::thread::sleep(Duration::from_millis(20));
            assert!(hub.inner.is_locked() && !callback.is_finished(), "the callback is waiting on SQLite");
            let started = std::time::Instant::now();
            for key in 0..500u32 {
                hub.note_human_input(2);
                hub.send_keys(2, &[b'a' + (key % 26) as u8]).unwrap();
            }
            assert!(started.elapsed() < Duration::from_millis(100), "keystrokes waited: {:?}", started.elapsed());
            wait_until("every keystroke at the host", || received.inputs.lock().len() == 500);
            assert!(!callback.is_finished(), "they arrived while the commit was still blocked");
            drop(database);
            callback.join().unwrap().unwrap();
        });
        let typed: Vec<u8> = received.inputs.lock().iter().flat_map(|(_, data)| data.clone()).collect();
        assert_eq!(typed, (0..500u32).map(|key| b'a' + (key % 26) as u8).collect::<Vec<_>>(), "in order, once each");
        hub.flush_human_input().unwrap();
        drop((terminal, hub, service));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interrupted_input_pauses_the_session_and_its_queue_until_the_user_continues() {
        let (service, directory, received) = crate::terminal::testing::serving_host("hub-interrupted");
        let hub = fixture(Some(service.clone()));
        let mut events = hub.subscribe();
        let terminal = install_terminal(&hub, &service, 2, 42, &directory);
        // As the input link does when a keystroke was lost or refused.
        service.input().restore_interrupted(terminal.run(), "the terminal host connection was lost");
        hub.on_input_interrupted(terminal.run());
        assert!(hub.session(2).unwrap().input_interrupted);
        assert!(hub.input_interrupted(2));
        let mut seen = (false, false);
        while let Ok(event) = events.try_recv() {
            match event {
                ServerEvent::SessionUpserted(info) if info.id == 2 && info.input_interrupted => seen.0 = true,
                ServerEvent::Notice { text, .. } if text.contains("Continue typing") => seen.1 = true,
                _ => {}
            }
        }
        assert_eq!(seen, (true, true), "the session shows it and the user is told");
        let saved: Vec<(RunId, String)> = hub.store.load_runtime(INPUT_INTERRUPTED_KEY).unwrap().unwrap();
        assert_eq!(saved[0].0, terminal.run(), "kept across a backend restart");
        // Every input path refuses: keystrokes, RPC/prompt input, and queued delivery waits.
        let error = hub.send_keys(2, b"x").unwrap_err();
        assert!(error.downcast_ref::<crate::terminal::InputInterrupted>().is_some(), "{error:#}");
        assert!(hub.write_input(2, b"x").is_err());
        assert!(hub.write_input_generation(2, Some(42), b"x").is_err());
        hub.continue_input(2).unwrap();
        assert!(!hub.session(2).unwrap().input_interrupted);
        hub.send_keys(2, b"y").unwrap();
        wait_until("input after continuing", || received.inputs.lock().len() == 1);
        assert_eq!(received.inputs.lock()[0].1, b"y", "nothing refused earlier was resent");
        let saved: Vec<(RunId, String)> = hub.store.load_runtime(INPUT_INTERRUPTED_KEY).unwrap().unwrap();
        assert!(saved.is_empty());
        drop((terminal, hub, service));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn shutdown_delivers_admitted_keystrokes_commits_their_markers_and_reports_refused_ones() {
        use crate::agents::{Origin, Prov};
        let (service, directory, received) = crate::terminal::testing::serving_host("hub-shutdown");
        let hub = fixture(Some(service.clone()));
        spawn_input_attribution(&hub).unwrap();
        let terminal = install_terminal(&hub, &service, 2, 42, &directory);
        let scheduled = Prov { origin: Origin::Schedule, hop: 1, msg_id: None };
        hub.agents.set_prov(2, Some(scheduled)).unwrap();
        // A stalled host: the keystrokes are admitted but not yet delivered.
        received.stall.store(true, Ordering::SeqCst);
        for key in [b"a", b"b", b"c"] {
            hub.note_human_input(2);
            hub.send_keys(2, key).unwrap();
        }
        let stopping = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.shutdown().await })
        };
        while !hub.input_closed() { tokio::time::sleep(Duration::from_millis(5)).await }
        // Typed while the backend stops: refused, and the run is marked interrupted.
        hub.note_human_input(2);
        assert!(hub.send_keys(2, b"late").is_err());
        received.stall.store(false, Ordering::SeqCst);
        stopping.await.unwrap();
        let typed: Vec<Vec<u8>> = received.inputs.lock().iter().map(|(_, data)| data.clone()).collect();
        assert_eq!(typed, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], "admitted keystrokes all arrived");
        assert!(hub.workers.lock().is_empty(), "the attribution worker was joined");
        // The markers were committed: the human's next prompt callback lowers provenance.
        let info = hub.session(2).unwrap();
        hub.agents.on_hook(&hub.store, &info, "UserPromptSubmit", &serde_json::json!({"prompt": "typed"})).unwrap();
        assert_eq!(hub.agents.prov(2), None);
        // The refused keystroke paused the run, and the next backend will know.
        let saved: Vec<(RunId, String)> = hub.store.load_runtime(INPUT_INTERRUPTED_KEY).unwrap().unwrap();
        assert_eq!(saved.iter().map(|(run, _)| *run).collect::<Vec<_>>(), vec![terminal.run()]);
        assert!(hub.session(2).unwrap().input_interrupted);
        drop((terminal, hub, service));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn callback_boundary_defers_input_while_backlogged_and_blocks_concurrent_inserts() {
        use crate::agents::{Origin, Prov, Source};
        let hub = move_fixture();
        let directory = std::env::temp_dir().join(format!("muster-input-boundary-{}", paths::random_token()));
        let path = directory.join("hooks.sqlite");
        let mut inbox = crate::hook_inbox::Inbox::open(&path).unwrap();
        let prior = Prov { origin: Origin::Schedule, hop: 1, msg_id: None };
        hub.agents.set_prov(2, Some(prior)).unwrap();
        // The drain batch is bounded; never publish new attribution ahead of
        // callbacks left in the queue, even if they belong to another session.
        for _ in 0..513 { inbox.enqueue("obsolete-token", "Stop", b"{}").unwrap(); }
        *hub.callback_inbox.lock() = Some(inbox);
        assert!(hub.note_prompt_delivery(2, Some(0), Source::Human, 200).is_err());
        assert_eq!(hub.agents.prov(2), Some(prior));
        hub.note_prompt_delivery(2, Some(0), Source::Human, 200).unwrap();
        assert_eq!(hub.agents.prov(2), None);

        let mut writer = crate::hook_inbox::Inbox::open(&path).unwrap();
        hub.input_boundary(|| {
            assert!(writer.enqueue("obsolete-token", "Stop", b"{}").is_err(), "hook inserts cannot cross the attribution commit");
            Ok(())
        }).unwrap();
        writer.enqueue("obsolete-token", "Stop", b"{}").unwrap();
        drop(writer);
        drop(hub);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_unconfirmed_launch_never_blocks_other_sessions_input() {
        let hub = move_fixture();
        let directory = std::env::temp_dir().join(format!("muster-pending-callback-{}", paths::random_token()));
        let mut inbox = crate::hook_inbox::Inbox::open(&directory.join("hooks.sqlite")).unwrap();
        // Session 1's launch is still unconfirmed and its agent already called back.
        remember_launch(&hub, true);
        {
            let mut g = hub.inner.lock();
            let entry = g.sessions.get_mut(&1).unwrap();
            entry.generation = 42;
            entry.info.state = AgentState::Starting;
        }
        inbox.enqueue("run-token", "PreToolUse", br#"{"tool_name":"Agent"}"#).unwrap();
        *hub.callback_inbox.lock() = Some(inbox);
        // Human input to another session establishes its boundary without waiting.
        hub.note_human_input(2);
        hub.flush_human_input().unwrap();
        assert_eq!(hub.session(1).unwrap().subagents, 1, "the early callback was applied");
        drop(hub);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_prompt_is_answered_once_and_only_as_seen() {
        let hub = move_fixture();
        let s = hub.session(1).unwrap();
        assert!(hub.claim_prompt(1, None, Some(s.state_since + 1)).is_err(), "stale state_since");
        assert!(hub.claim_prompt(1, Some(s.state_seq + 1), None).is_err(), "stale state_seq");
        hub.claim_prompt(1, Some(s.state_seq), Some(s.state_since)).unwrap();
        let after = hub.session(1).unwrap();
        assert_ne!(after.state_seq, s.state_seq);
        // The same view can't answer twice (a delayed duplicate would hit the next prompt).
        assert!(hub.claim_prompt(1, Some(s.state_seq), None).unwrap_err().to_string().starts_with("stale"));
        hub.claim_prompt(1, None, None).unwrap();
    }

    #[test]
    fn approval_views_from_before_backend_replacement_are_stale() {
        let original = move_fixture();
        let old = original.session(1).unwrap();
        let cfg = HubConfig { port: 0, client_token: "replacement".into(),
            muster_bin: PathBuf::from("/bin/true"), terminal_host: None, openrouter_key: None };
        let replacement = Hub::new(cfg, original.store.clone()).unwrap();
        let current = replacement.session(1).unwrap();
        assert_ne!(old.state_seq, current.state_seq);
        assert!(replacement.claim_prompt(1, Some(old.state_seq), None).is_err());
        replacement.claim_prompt(1, Some(current.state_seq), None).unwrap();
    }

    #[test]
    fn recovery_trusts_a_live_run_over_stale_saved_state() {
        let status = |alive: bool| muster_terminal_protocol::TerminalStatus {
            run: RunId { id: 1, generation: 2 }, pid: Some(42), alive, drained: !alive,
            has_processes: alive, exit_code: (!alive).then_some(0), title: None, cols: 80, rows: 24,
            output_seq: 0, last_output_at: 0, application_cursor: false, log_persisted: true,
            cwd: PathBuf::from("/"),
        };
        // A restart crashed after the host started the new run, before its state was saved.
        let mut info = crate::mcp::guard::test_session(1, None);
        info.state = AgentState::Exited;
        info.state_detail = Some("exit code 0".into());
        recover_session_state(&mut info, Some(&status(true)), true, 7);
        assert_eq!(info.state, AgentState::Starting);
        assert_eq!((info.pid, info.state_detail.as_deref(), info.state_since), (Some(42), None, 7));
        // A saved dialog is never offered again with a fresh identity: the screen decides.
        for dialog in [AgentState::NeedsApproval, AgentState::AskingQuestion] {
            let mut info = crate::mcp::guard::test_session(1, None);
            info.state = dialog;
            info.state_detail = Some("Bash: command A".into());
            recover_session_state(&mut info, Some(&status(true)), true, 7);
            assert_eq!(info.state, AgentState::Working);
            assert!(!info.state_detail.unwrap().contains("command A"));
        }
        // An exited run and a lost run still end Exited.
        let mut info = crate::mcp::guard::test_session(1, None);
        info.state = AgentState::Working;
        recover_session_state(&mut info, Some(&status(false)), true, 7);
        assert_eq!(info.state, AgentState::Exited);
        let mut info = crate::mcp::guard::test_session(1, None);
        info.state = AgentState::NeedsApproval;
        recover_session_state(&mut info, None, true, 7);
        assert_eq!(info.state, AgentState::Exited);
    }

    #[test]
    fn move_session_between_channels() {
        let hub = move_fixture();
        let mut rx = hub.subscribe();
        let s = hub.move_session(1, 2).unwrap();
        assert_eq!(s.channel_id, 2);
        assert_eq!(hub.session(1).unwrap().channel_id, 2);
        // Persisted.
        let stored = hub.store.load_sessions().unwrap();
        assert_eq!(stored.iter().find(|x| x.info.id == 1).unwrap().info.channel_id, 2);
        // The shell title now names channel 2; channel 1 falls back to its folder.
        assert_eq!(hub.channel(2).unwrap().name, "build the widget");
        assert_eq!(hub.channel(1).unwrap().name, "repo");
        let mut seen = vec![];
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ServerEvent::SessionUpserted(i) => seen.push(format!("s{}@{}", i.id, i.channel_id)),
                ServerEvent::ChannelUpserted(c) => seen.push(format!("c{}={}", c.id, c.name)),
                _ => {}
            }
        }
        assert_eq!(seen, ["s1@2", "c1=repo", "c2=build the widget"]);
        // Same channel: no-op, no event.
        assert_eq!(hub.move_session(1, 2).unwrap().channel_id, 2);
        assert!(rx.try_recv().is_err());
        // Archived, unknown channel, unknown session.
        assert!(hub.move_session(2, 3).unwrap_err().to_string().contains("archived"));
        assert!(hub.move_session(2, 99).unwrap_err().to_string().contains("no such channel"));
        assert!(hub.move_session(42, 2).unwrap_err().to_string().contains("no such session"));
        assert_eq!(hub.session(2).unwrap().channel_id, 1);
        // Channel 1 still has session 2: not closed.
        assert!(!hub.archive_if_empty(1).unwrap());
        assert!(!hub.channel(1).unwrap().archived);
        // Moving the last session out leaves an empty channel; closing it is then allowed.
        hub.move_session(2, 2).unwrap();
        assert!(hub.sessions().iter().all(|s| s.channel_id == 2));
        let mut rx = hub.subscribe();
        assert!(hub.archive_if_empty(1).unwrap());
        assert!(hub.channel(1).unwrap().archived);
        assert!(matches!(rx.try_recv(), Ok(ServerEvent::ChannelRemoved { id: 1 })));
        assert!(!hub.archive_if_empty(1).unwrap(), "already closed");
        assert!(!hub.archive_if_empty(2).unwrap(), "has sessions");
    }

    #[test]
    fn no_move_out_of_a_closing_channel() {
        let hub = move_fixture();
        // archive_channel marks the channel archived before it collects its sessions.
        hub.update_channel(1, false, |c| c.archived = true).unwrap();
        assert!(hub.move_session(1, 2).unwrap_err().to_string().contains("being closed"));
        assert_eq!(hub.session(1).unwrap().channel_id, 1);
    }

    #[test]
    fn stale_moves_are_refused() {
        let hub = move_fixture();
        assert_eq!(hub.move_session_from(1, 2, Some(1)).unwrap().1, 1);
        // A second client still thinks session 1 is in channel 1.
        let e = hub.move_session_from(1, 1, Some(1)).unwrap_err().to_string();
        assert!(e.contains("no longer in channel 1"), "{e}");
        assert_eq!(hub.session(1).unwrap().channel_id, 2);
    }

    /// Events waiting on `rx`, compactly: `s<id>@w<window>`, `c<id>@w<window>`, `w<id>`
    /// (upserted), `-s<id>`, `-c<id>`, `-w<id>` (removed).
    fn drain(rx: &mut broadcast::Receiver<ServerEvent>) -> Vec<String> {
        let w = |w: Option<WindowId>| w.map_or("-".into(), |w| w.to_string());
        let mut out = vec![];
        while let Ok(ev) = rx.try_recv() {
            out.push(match ev {
                ServerEvent::SessionUpserted(i) => format!("s{}@w{}", i.id, w(i.window_id)),
                ServerEvent::SessionRemoved { id } => format!("-s{id}"),
                ServerEvent::ChannelUpserted(c) => format!("c{}@w{}", c.id, w(c.window_id)),
                ServerEvent::ChannelRemoved { id } => format!("-c{id}"),
                ServerEvent::WindowUpserted(v) => format!("w{}", v.id),
                ServerEvent::WindowRemoved { id } => format!("-w{id}"),
                _ => continue,
            });
        }
        out
    }

    #[test]
    fn set_window_moves_a_tab_and_the_task_follows() {
        let hub = move_fixture();
        let w2 = hub.create_window(None).unwrap().id;
        assert_eq!(w2, 2);
        let mut rx = hub.subscribe();
        // One of channel 1's two sessions moves: the task stays in window 1.
        let s = hub.set_session_window(1, 2, Some(1)).unwrap();
        assert_eq!((s.window_id, s.channel_id), (Some(2), 1));
        assert_eq!(hub.channel(1).unwrap().window_id, Some(1));
        assert_eq!(drain(&mut rx), ["s1@w2"]);
        let stored = hub.store.load_sessions().unwrap();
        assert_eq!(stored.iter().find(|x| x.info.id == 1).unwrap().info.window_id, Some(2));
        // A client with a stale view (session 1 still in window 1) is refused.
        let e = hub.set_session_window(1, 1, Some(1)).unwrap_err().to_string();
        assert!(e.contains("no longer in window 1"), "{e}");
        assert_eq!(hub.session(1).unwrap().window_id, Some(2));
        // Same window: no-op, no event. Unknown window or session: errors.
        assert_eq!(hub.set_session_window(1, 2, None).unwrap().window_id, Some(2));
        assert!(hub.set_session_window(1, 9, None).unwrap_err().to_string().contains("no such window"));
        assert!(hub.set_session_window(42, 2, None).unwrap_err().to_string().contains("no such session"));
        assert!(drain(&mut rx).is_empty());
        // The other one follows: now all of the task's sessions are in window 2, so is the task.
        hub.set_session_window(2, 2, Some(1)).unwrap();
        let ch = hub.channel(1).unwrap();
        assert_eq!(ch.window_id, Some(2));
        assert_eq!(hub.window(2).unwrap().channel_ids, vec![1]);
        assert_eq!(hub.window(1).unwrap().channel_ids, vec![2]);
        assert_eq!(drain(&mut rx), ["s2@w2", "c1@w2", "w1", "w2"]);
        assert_eq!(hub.store.load_channels().unwrap()[0].window_id, Some(2));
    }

    #[test]
    fn set_window_task_leaves_a_window_without_its_tabs() {
        let hub = move_fixture();
        for _ in 0..2 {
            hub.create_window(None).unwrap();
        }
        // Task 1's sessions spread over windows 2 and 3: none is left in window 1, so the task
        // follows the last move instead of keeping window 1.
        hub.set_session_window(1, 2, None).unwrap();
        assert_eq!(hub.channel(1).unwrap().window_id, Some(1));
        let mut rx = hub.subscribe();
        hub.set_session_window(2, 3, None).unwrap();
        assert_eq!(hub.channel(1).unwrap().window_id, Some(3));
        assert_eq!(drain(&mut rx), ["s2@w3", "c1@w3", "w1", "w3"]);
        // Window 2 (nobody holds it) loses its only tab and has no task: deleted.
        hub.set_session_window(1, 3, None).unwrap();
        assert_eq!(drain(&mut rx), ["s1@w3", "-w2"]);
        assert!(hub.window(2).is_none());
        assert!(hub.store.load_windows().unwrap().iter().all(|w| w.id != 2));
        // A window a client holds is kept, also without tabs or tasks.
        hub.claim_window(7, Some(1)).unwrap();
        hub.move_channel(2, 3, None).unwrap();
        assert!(hub.window(1).is_some_and(|w| w.open && w.channel_ids.is_empty()));
    }

    #[test]
    fn channel_move_takes_its_sessions() {
        let hub = move_fixture();
        hub.create_window(None).unwrap();
        hub.set_session_window(1, 2, None).unwrap();
        let mut rx = hub.subscribe();
        hub.move_channel(1, 2, None).unwrap();
        assert!(hub.sessions().iter().all(|s| s.window_id == Some(2)));
        assert_eq!(drain(&mut rx), ["c1@w2", "s2@w2", "w1", "w2"]);
    }

    #[test]
    fn last_session_closes_the_task() {
        let hub = move_fixture();
        let mut rx = hub.subscribe();
        hub.remove_session(1).unwrap();
        assert!(!hub.channel(1).unwrap().archived, "one session left");
        // The removed shell's title no longer names the task.
        assert_eq!(drain(&mut rx), ["-s1", "c1@w1"]);
        hub.remove_session(2).unwrap();
        assert!(hub.channel(1).unwrap().archived);
        assert!(hub.store.load_channels().unwrap()[0].archived);
        // Window 1 keeps task 2, which has no sessions and isn't closed by any of this.
        assert_eq!(drain(&mut rx), ["-s2", "-c1", "w1"]);
        assert!(!hub.channel(2).unwrap().archived);
        let w = hub.window(1).unwrap();
        assert_eq!(w.channel_ids, vec![2]);
    }

    #[test]
    fn removing_the_tab_left_in_a_tasks_window_moves_the_task() {
        let hub = move_fixture();
        hub.create_window(None).unwrap();
        hub.set_session_window(1, 2, None).unwrap();
        let mut rx = hub.subscribe();
        hub.remove_session(2).unwrap();
        assert_eq!(hub.channel(1).unwrap().window_id, Some(2));
        assert_eq!(drain(&mut rx), ["-s2", "c1@w2", "w1", "w2"]);
    }

    #[test]
    fn a_task_emptied_by_a_move_waits_for_a_removal() {
        let hub = move_fixture();
        // Moving both sessions into task 2 empties task 1 but doesn't close it (the client
        // does, with `close_empty_source`); removing one of them later leaves task 2 open.
        hub.move_session(1, 2).unwrap();
        hub.move_session(2, 2).unwrap();
        assert!(!hub.channel(1).unwrap().archived);
        hub.remove_session(1).unwrap();
        assert!(!hub.channel(2).unwrap().archived);
        // Its last one: closed.
        hub.remove_session(2).unwrap();
        assert!(hub.channel(2).unwrap().archived);
        assert!(!hub.channel(1).unwrap().archived);
    }

    #[test]
    fn claim_and_release_count_sessions_as_tabs() {
        let hub = move_fixture();
        hub.create_window(None).unwrap();
        hub.create_window(None).unwrap();
        hub.set_session_window(1, 2, None).unwrap();
        // Window 1 has tabs and comes first.
        assert_eq!(hub.claim_window(1, None).unwrap().id, 1);
        // Window 2 has a tab (session 1), though no task: it is the next one with tabs.
        assert_eq!(hub.claim_window(2, None).unwrap().id, 2);
        // Window 3 has no tabs: a third claim gets a new window.
        assert_eq!(hub.claim_window(3, None).unwrap().id, 4);
        // Released: window 2 keeps its tab; window 4 has none and goes.
        let mut rx = hub.subscribe();
        hub.release_windows(2);
        hub.release_windows(3);
        assert!(hub.window(2).is_some_and(|w| !w.open));
        assert!(hub.window(4).is_none());
        assert_eq!(drain(&mut rx), ["w2", "-w4"]);
        // Its tab removed, window 2 (not held) goes too.
        hub.remove_session(1).unwrap();
        assert_eq!(drain(&mut rx), ["-s1", "-w2", "c1@w1"]);
    }

    #[test]
    fn a_closed_agent_tab_is_hidden_not_stopped() {
        let hub = agent_fixture();
        // Only a named agent's tab can be hidden: any other closes for real.
        let e = hub.hide_session(1).unwrap_err().to_string();
        assert!(e.contains("not a named agent"), "{e}");
        // The agent's tab is alone in window 2 (nobody holds it): hidden, it leaves the window,
        // which goes. Its task keeps window 1, where its other tab is.
        hub.create_window(None).unwrap();
        hub.set_session_window(2, 2, None).unwrap();
        let mut rx = hub.subscribe();
        let s = hub.hide_session(2).unwrap();
        assert!(s.hidden && s.window_id.is_none());
        assert_eq!(drain(&mut rx), ["s2@w-", "-w2"]);
        assert_eq!(hub.channel(1).unwrap().window_id, Some(1));
        let stored = |id| hub.store.load_sessions().unwrap().into_iter().find(|x| x.info.id == id).unwrap().info;
        assert!(stored(2).hidden && stored(2).window_id.is_none(), "kept across restarts");
        // Again: a no-op.
        hub.hide_session(2).unwrap();
        assert!(drain(&mut rx).is_empty());
        // Its task's other tab closes: the task stays open (the agent runs in it) and leaves
        // window 1, which keeps only task 2.
        hub.remove_session(1).unwrap();
        assert!(!hub.channel(1).unwrap().archived);
        assert_eq!(hub.channel(1).unwrap().window_id, None);
        assert_eq!(hub.window(1).unwrap().channel_ids, vec![2]);
        // (The last `c1` is its name, re-derived without the removed shell's title.)
        assert_eq!(drain(&mut rx), ["-s1", "c1@w-", "w1", "c1@w-"]);
        // No window claim adopts the hidden tab or its task.
        let w = hub.claim_window(5, Some(3)).unwrap();
        assert!(!w.channel_ids.contains(&1));
        assert!(hub.session(2).unwrap().hidden && hub.channel(1).unwrap().window_id.is_none());
        // Shown again (the agents panel pulls it into its window): the task follows.
        let s = hub.set_session_window(2, 3, None).unwrap();
        assert!(!s.hidden && s.window_id == Some(3));
        assert_eq!(hub.channel(1).unwrap().window_id, Some(3));
        assert!(!stored(2).hidden && stored(2).window_id == Some(3));
    }

    #[test]
    fn claim_adopts_orphan_sessions() {
        let hub = move_fixture();
        {
            let mut g = hub.inner.lock();
            g.sessions.get_mut(&1).unwrap().info.window_id = None;
            g.sessions.get_mut(&2).unwrap().info.window_id = Some(99);
        }
        let mut rx = hub.subscribe();
        let w = hub.claim_window(5, Some(3)).unwrap();
        assert_eq!(w.id, 3);
        assert!(hub.sessions().iter().all(|s| s.window_id == Some(3)));
        assert_eq!(drain(&mut rx), ["s1@w3", "s2@w3", "w3"]);
    }

    #[test]
    fn window_layout_is_stored_without_an_event() {
        let hub = move_fixture();
        let mut rx = hub.subscribe();
        let layout = serde_json::json!({"type": "pane", "id": "g1", "tabs": [1, 2], "active": 1});
        hub.set_window_layout(1, layout.clone()).unwrap();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(hub.window(1).unwrap().layout, Some(layout.clone()));
        assert_eq!(hub.store.load_windows().unwrap()[0].layout, Some(layout));
        assert!(hub.set_window_layout(9, serde_json::json!({})).is_err());
        hub.set_window_layout(1, serde_json::Value::Null).unwrap();
        assert_eq!(hub.window(1).unwrap().layout, None);
    }

    #[test]
    fn startup_keeps_windows_with_sessions() {
        let store = Store::open_memory().unwrap();
        for id in [1, 2, 3] {
            store.upsert_window(id, &id.to_string(), 0, 0).unwrap();
        }
        store.set_window_layout(2, Some(&serde_json::json!({"x": 1}))).unwrap();
        // Window 1: a task. Window 2: a session of that task. Window 3: nothing.
        let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
        let mut ch = move_fixture().channel(1).unwrap();
        ch.workspace_id = ws;
        let cid = store.insert_channel(&ch).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.channel_id = cid;
        info.window_id = Some(2);
        store
            .insert_session(&StoredSession {
                info,
                extra_args: vec![],
            })
            .unwrap();
        let hub = Hub::new(
            HubConfig {
                port: 0,
                client_token: "t".into(),
                muster_bin: PathBuf::from("/bin/true"),
            terminal_host: None,
            openrouter_key: None,
            },
            store,
        )
        .unwrap();
        let ids: Vec<WindowId> = hub.windows().iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(hub.window(2).unwrap().layout, Some(serde_json::json!({"x": 1})));
    }

    #[test]
    fn new_sessions_window() {
        let hub = move_fixture();
        hub.create_window(None).unwrap();
        hub.set_session_window(2, 2, None).unwrap();
        let mut ev = vec![];
        let mut g = hub.inner.lock();
        let mut pick = |req, parent, cid| hub.session_window_locked(&mut g, req, parent, cid, &mut ev);
        // Explicit (must exist), else the parent's window, else the task's.
        assert_eq!(pick(Some(1), Some(2), 1).unwrap(), 1);
        assert!(pick(Some(9), None, 1).unwrap_err().to_string().contains("no such window"));
        assert_eq!(pick(None, Some(2), 1).unwrap(), 2);
        assert_eq!(pick(None, Some(1), 2).unwrap(), 1);
        assert_eq!(pick(None, None, 1).unwrap(), 1);
        assert_eq!(pick(None, Some(42), 1).unwrap(), 1, "unknown parent");
        // A task without a (live) window: where a new task would go (the lowest window here).
        g.channels.get_mut(&1).unwrap().window_id = Some(77);
        let mut pick = |req, parent, cid| hub.session_window_locked(&mut g, req, parent, cid, &mut ev);
        assert_eq!(pick(None, None, 1).unwrap(), 1);
        assert!(ev.is_empty());
    }

    #[test]
    fn worktree_reservations_are_counted_and_released_on_drop() {
        let hub = move_fixture();
        let ch = hub.channel(1).unwrap();
        let reserved = |h: &Hub| h.inner.lock().removing_worktrees.contains_key(Path::new(&ch.cwd));
        let a = hub.reserve_worktree_removal(&ch).ok().unwrap();
        let b = hub.reserve_worktree_removal(&ch).ok().unwrap();
        drop(a);
        assert!(reserved(&hub), "the overlapping removal still holds it");
        drop(b);
        assert!(!reserved(&hub));
        // A dropped (cancelled) delete releases it too.
        {
            let _r = hub.reserve_worktree_removal(&ch).ok().unwrap();
            assert!(reserved(&hub));
        }
        assert!(!reserved(&hub));
    }

    #[test]
    fn window_launches_are_deduplicated_until_failed_or_claimed() {
        let hub = move_fixture();
        let w2 = hub.create_window(None).unwrap().id;
        assert!(hub.begin_window_launch(1));
        assert!(!hub.begin_window_launch(1), "a launch is already on its way");
        assert!(hub.begin_window_launch(w2), "other windows are independent");
        assert!(!hub.begin_window_launch(99), "unknown window");
        hub.end_window_launch(1);
        assert!(hub.begin_window_launch(1), "a failed launch can be retried");
        hub.claim_window(7, Some(1)).unwrap();
        assert!(!hub.inner.lock().launching.contains_key(&1), "claimed: launch done");
        assert!(
            matches!(hub.window_launch(1), Ok(WindowLaunch::Held(_))),
            "a held window is raised, never launched again"
        );
    }

    #[test]
    fn subagent_counting() {
        let tool = |t: &str| serde_json::json!({"tool_name": t});
        assert_eq!(subagent_delta("PreToolUse", &tool("Agent")), Some(Some(1)));
        assert_eq!(subagent_delta("PreToolUse", &tool("Task")), Some(Some(1)));
        assert_eq!(subagent_delta("PostToolUse", &tool("Agent")), Some(Some(-1)));
        assert_eq!(subagent_delta("PostToolUseFailure", &tool("Task")), Some(Some(-1)));
        assert_eq!(subagent_delta("PreToolUse", &tool("Bash")), None);
        // A turn's end or a new/ended session resets it (a missed PostToolUse can't stick).
        for ev in ["Stop", "StopFailure", "SessionStart", "SessionEnd"] {
            assert_eq!(subagent_delta(ev, &serde_json::json!({})), Some(None), "{ev}");
        }
    }

    #[test]
    fn worktree_users() {
        let mut ch = move_fixture().channel(1).unwrap();
        ch.cwd = "/data/wt/7".into();
        let s = |id, channel_id, cwd: &str| {
            let mut i = crate::mcp::guard::test_session(id, None);
            i.channel_id = channel_id;
            i.cwd = cwd.into();
            i
        };
        let (own, moved, nested, other, prefix) = (
            s(1, 1, "/data/wt/7"),
            s(2, 2, "/data/wt/7"),
            s(3, 2, "/data/wt/7/sub"),
            s(4, 2, "/w/repo"),
            s(5, 2, "/data/wt/70"),
        );
        assert_eq!(dir_user(&ch, [(&own, true), (&other, true), (&prefix, true)].into_iter()), None);
        assert_eq!(dir_user(&ch, [(&moved, false)].into_iter()), None, "exited sessions don't count");
        assert_eq!(dir_user(&ch, [(&moved, true)].into_iter()), Some(2));
        assert_eq!(dir_user(&ch, [(&nested, true)].into_iter()), Some(3));
    }

    #[test]
    fn window_placement_helpers() {
        let mut g = Inner::default();
        assert_eq!(free_window_id(&g), 1);
        for id in [1, 2, 4] {
            g.windows.insert(
                id,
                WinEntry {
                    name: id.to_string(),
                    last_focused_at: 0,
                    created_at: 0,
                    holder: None,
                    layout: None,
                },
            );
        }
        assert_eq!(free_window_id(&g), 3);
        let mk = |id, win, order, archived| Channel {
            id,
            workspace_id: 1,
            name: "c".into(),
            cwd: "/".into(),
            branch: None,
            base_branch: None,
            owns_worktree: false,
            port_base: 20000,
            archived,
            layout: None,
            created_at: 0,
            last_read_at: 0,
            sort_order: order,
            permissions: Permissions::default(),
            window_id: Some(win),
            auto_named: false,
            first_prompt: None,
        };
        g.channels.insert(10, mk(10, 1, 3, false));
        g.channels.insert(11, mk(11, 1, 0, false));
        g.channels.insert(12, mk(12, 1, 1, true));
        g.channels.insert(13, mk(13, 2, 0, false));
        assert_eq!(tab_ids(&g, 1), vec![11, 10]);
        assert_eq!(next_tab_order(&g, 1), 4);
        assert_eq!(next_tab_order(&g, 4), 0);
        let v = window_view(&g, 2).unwrap();
        assert_eq!((v.channel_ids, v.open, v.name.as_str()), (vec![13], false, "2"));
        assert_eq!(clean_window_name(Some("  a\u{1b}b ".into())).as_deref(), Some("ab"));
        assert_eq!(clean_window_name(Some(" ".into())), None);
    }

    #[test]
    fn a_plain_claude_tab_restarts_into_its_last_conversation() {
        let hub = move_fixture();
        hub.inner.lock().tokens.insert("plain-token".into(), 2);
        hub.set_state_for_test(2, AgentState::Idle);
        let args = |hub: &Hub| hub.inner.lock().sessions.get(&2).unwrap().extra_args.clone();
        let hook = |hub: &Hub, token: &str, event: &str, payload: serde_json::Value| {
            hub.handle_hook(token, event, payload).unwrap();
        };
        let start = |hub: &Hub, id: &str, source: &str| {
            hook(hub, "plain-token", "SessionStart", serde_json::json!({"session_id": id, "source": source}));
        };
        let prompt = |hub: &Hub| hook(hub, "plain-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"}));
        start(&hub, "conv-1", "startup");
        assert!(args(&hub).is_empty(), "nothing to resume before a prompt");
        prompt(&hub);
        assert_eq!(args(&hub), ["--resume", "conv-1"]);
        // A `/clear` starts an empty conversation: a restart starts empty until it has a prompt.
        start(&hub, "conv-2", "clear");
        assert!(args(&hub).is_empty());
        prompt(&hub);
        assert_eq!(args(&hub), ["--resume", "conv-2"]);
        assert_eq!(hub.store.load_sessions().unwrap().into_iter().find(|s| s.info.id == 2).unwrap().extra_args,
            ["--resume", "conv-2"], "kept across a backend restart");
        assert_eq!(hub.store.load_hook_session(2, 0).unwrap().unwrap().conversation.as_deref(), Some("conv-2"));
        start(&hub, "--evil", "startup");
        prompt(&hub);
        assert_eq!(args(&hub), ["--resume", "conv-2"], "never a flag");
        // A run whose SessionStart came before this backend learns it from its next turn.
        hub.inner.lock().sessions.get_mut(&2).unwrap().conversation = None;
        hook(&hub, "plain-token", "Stop", serde_json::json!({"session_id": "conv-3"}));
        assert_eq!(args(&hub), ["--resume", "conv-3"]);

        // A run that couldn't reopen its conversation is forgotten only on Claude's own word.
        let c = SessionKind::Claude;
        assert!(conversation_missing(c, 1, false, Some("conv-3"), Some("No conversation found with session ID: conv-3")));
        assert!(!conversation_missing(c, 1, false, Some("conv-3"), Some("Error: something else")), "another failure keeps it");
        assert!(!conversation_missing(c, 1, false, Some("conv-3"), None));
        assert!(!conversation_missing(c, 0, false, Some("conv-3"), Some("No conversation found")));
        assert!(hub.forget_failed_resume(2, 0, Some("conv-3")));
        assert!(args(&hub).is_empty(), "the next start is a new conversation");
        assert!(!hub.forget_failed_resume(2, 0, Some("conv-3")), "nothing left to forget");
        assert!(!hub.forget_failed_resume(2, 7, None), "another run's failure");

        // A named agent's session keeps its conversation on the agent's record instead.
        let hub = agent_fixture();
        hub.inner.lock().tokens.insert("agent-token".into(), 2);
        hub.set_state_for_test(2, AgentState::Idle);
        hook(&hub, "agent-token", "SessionStart", serde_json::json!({"session_id": "conv-1"}));
        hook(&hub, "agent-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"}));
        assert!(args(&hub).is_empty());
        assert_eq!(hub.agents.by_session(2).unwrap().resume_id.as_deref(), Some("conv-1"));
        // A run that couldn't reopen it: the agent forgets it, so its next start is a new one
        // (not when it saved another conversation since).
        assert!(!crate::agents::forget_conversation(&hub, 2, Some("conv-0")).unwrap());
        assert!(hub.forget_failed_resume(2, 0, Some("conv-1")));
        assert_eq!(hub.agents.by_session(2).unwrap().resume_id, None);
        assert_eq!(hub.store.load_agents().unwrap()[0].resume_id, None);
        assert!(!crate::agents::forget_conversation(&hub, 1, None).unwrap(), "not an agent's session");
        // A `/clear` in an agent's tab: its next start is empty until the new one has a prompt.
        hook(&hub, "agent-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"}));
        assert_eq!(hub.agents.by_session(2).unwrap().resume_id.as_deref(), Some("conv-1"));
        hook(&hub, "agent-token", "SessionStart", serde_json::json!({"session_id": "conv-2", "source": "clear"}));
        assert_eq!(hub.agents.by_session(2).unwrap().resume_id, None);
        // Start fresh: a hook the old run still sends can't bring a conversation back.
        hook(&hub, "agent-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"}));
        hub.fence_conversation(2);
        hub.agents.update(&hub.store, hub.agents.by_session(2).unwrap().id, |a| a.resume_id = None).unwrap();
        hook(&hub, "agent-token", "Stop", serde_json::json!({"session_id": "conv-2"}));
        assert_eq!(hub.agents.by_session(2).unwrap().resume_id, None, "fenced");
    }

    /// `fixture_with` plus session 3, a Codex tab in channel 1, with `extra` stored arguments
    /// (and a named agent "cody" on it when `agent`).
    fn codex_fixture(extra: &[&str], agent: bool) -> Arc<Hub> {
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let hub = fixture_with(None, |store| {
            let mut info = crate::mcp::guard::test_session(0, None);
            info.kind = SessionKind::Codex;
            info.window_id = Some(1);
            assert_eq!(store.insert_session(&StoredSession { info, extra_args: extra }).unwrap(), 3);
            if agent {
                store.insert_agent(&crate::agents::Agent {
                    account_profile_id: None, provider: Default::default(), id: 0, name: "cody".into(),
                    kind: SessionKind::Codex, model: None, effort: None, persona: String::new(),
                    home_dir: "/x".into(), permissions: Permissions::default(),
                    accepts_origins: crate::agents::default_accepts(SessionKind::Codex),
                    channel_id: Some(1), session_id: Some(3), resume_id: None, created_at: 0, autostart: false,
                }).unwrap();
            }
        });
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        hub.set_state_for_test(3, AgentState::Idle);
        hub
    }

    /// What `start_process` would hand session `id`'s next run as its Codex thread (and the run
    /// takes it, under a new generation), without a terminal host.
    fn next_codex_start(hub: &Hub, id: SessionId) -> Option<String> {
        let mut g = hub.inner.lock();
        let e = g.sessions.get_mut(&id).unwrap();
        let previous = e.generation;
        e.generation = previous + 1;
        e.fenced_generation = None;
        e.opened = false;
        let resume = hub.codex_resume(id, previous, e).unwrap();
        e.conversation = resume.clone();
        e.conversation_sure = resume.is_some();
        resume
    }

    fn turn_done(thread: &str) -> serde_json::Value {
        serde_json::json!({"type": "agent-turn-complete", "thread-id": thread, "input-messages": ["hi"], "last-assistant-message": "ok"})
    }

    #[test]
    fn a_plain_codex_tab_restarts_into_its_top_level_thread() {
        let hub = codex_fixture(&["-m", "gpt-6-sol"], false);
        let hook = |event: &str, payload: serde_json::Value| hub.handle_hook("codex-token", event, payload).unwrap();
        let saved = || hub.store.codex_thread(3).unwrap().map(|t| t.thread);
        let state = || { let s = hub.session(3).unwrap(); (s.state, s.state_detail) };
        // A thread without a finished turn isn't resumed.
        hook("SessionStart", serde_json::json!({"session_id": "th-1", "source": "startup"}));
        hook("UserPromptSubmit", serde_json::json!({"session_id": "th-1", "prompt": "hi"}));
        assert_eq!(saved(), None);
        // Its first finished turn: the next start is `codex resume … -- th-1`.
        hook("notify", turn_done("th-1"));
        assert_eq!(saved().as_deref(), Some("th-1"));
        // The stored arguments never change: older backends must keep accepting them.
        let stored = hub.store.load_sessions().unwrap().into_iter().find(|s| s.info.id == 3).unwrap().extra_args;
        assert_eq!(stored, ["-m", "gpt-6-sol"]);
        // Codex's title request and a sub-agent's turn are not the conversation, and a
        // sub-agent finishing says nothing about the tab's state.
        hook("notify", serde_json::json!({"type": "agent-turn-complete", "thread-id": "th-title",
            "input-messages": ["Generate a concise, single-line task title of at most 36 characters"]}));
        hub.set_state_for_test(3, AgentState::Working);
        hook("notify", turn_done("th-child"));
        assert_eq!(saved().as_deref(), Some("th-1"), "a child's turn never replaces the parent");
        assert_eq!(state().0, AgentState::Working);
        hook("notify", turn_done("--evil"));
        assert_eq!(saved().as_deref(), Some("th-1"));
        // The next run resumes it; a child's turn there doesn't replace it either.
        assert_eq!(next_codex_start(&hub, 3).as_deref(), Some("th-1"));
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        hook("notify", turn_done("th-child-2"));
        assert_eq!(saved().as_deref(), Some("th-1"));
        hook("notify", turn_done("th-1"));
        assert_eq!(state().0, AgentState::Idle);
        // `/new`: the old thread is forgotten at once; the new one is saved at its first turn.
        hook("SessionStart", serde_json::json!({"session_id": "th-2", "source": "startup"}));
        assert_eq!(saved(), None);
        assert_eq!(next_codex_start(&hub, 3), None, "a restart now starts a new conversation");
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        hook("SessionStart", serde_json::json!({"session_id": "th-3", "source": "startup"}));
        hook("notify", turn_done("th-3"));
        assert_eq!(saved().as_deref(), Some("th-3"));
        // Codex's own `/resume` of a saved thread: that one is reopened next.
        hook("SessionStart", serde_json::json!({"session_id": "th-old", "source": "resume"}));
        assert_eq!(saved().as_deref(), Some("th-old"));

        // A run an older backend started in between (it never resumes Codex) was a new
        // conversation: the saved thread belongs to the run before it.
        hub.inner.lock().sessions.get_mut(&3).unwrap().generation += 1;
        assert_eq!(next_codex_start(&hub, 3), None);
        assert_eq!(hub.store.codex_thread(3).unwrap(), None);
    }

    #[test]
    fn a_codex_thread_is_forgotten_only_when_codex_says_it_is_gone() {
        let hub = codex_fixture(&[], false);
        hub.handle_hook("codex-token", "notify", turn_done("th-1")).unwrap();
        let generation = hub.inner.lock().sessions.get(&3).unwrap().generation;
        let x = SessionKind::Codex;
        let gone = "Error: No saved session found with ID th-1. Run `codex resume` without an ID to choose from existing sessions.";
        assert!(conversation_missing(x, 1, false, Some("th-1"), Some(gone)));
        assert!(!conversation_missing(x, 1, false, Some("th-1"), Some("Error: failed to initialize state runtime: database is locked")),
            "Codex exits 1 for other reasons too");
        assert!(!conversation_missing(x, 1, false, Some("th-2"), Some(gone)), "another thread's message");
        assert!(!hub.forget_failed_resume(3, generation, Some("th-2")), "a newer thread was saved");
        assert!(hub.forget_failed_resume(3, generation, Some("th-1")));
        assert_eq!(hub.store.codex_thread(3).unwrap(), None);
        // The tab's launch says which thread it reopened (recovery reads it the same way).
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(crate::adapters::resumed_in_launch(x, &a(&["resume", "-c", "k=v", "--", "th-1"])), Some(Some("th-1".into())));
    }

    #[test]
    fn a_failed_resume_while_no_backend_watched_is_cleaned_up_at_startup() {
        for (output, gone) in [("Error: No saved session found with ID th-1. Run `codex resume` without an ID.\r\n", true),
            ("Error: failed to initialize state runtime: database is locked\r\n", false)] {
            let hub = codex_fixture(&[], false);
            // Unique per case: the log lives under the (isolated) data dir.
            let generation = 9_100_000 + u64::from(gone);
            let thread = crate::store::CodexThread { thread: "th-1".into(), generation };
            hub.store.set_codex_thread(3, Some(&thread)).unwrap();
            {
                let mut g = hub.inner.lock();
                let e = g.sessions.get_mut(&3).unwrap();
                e.generation = generation;
                e.codex_thread = Some(thread);
                e.info.state = AgentState::Exited;
                e.info.exit_code = Some(1);
            }
            hub.terminal_records.lock().insert(3, StoredTerminal { host_instance: "gone".into(), token: "t".into(),
                key_refs: Default::default(), key_fp: None, port_base: 20000, pending: false, retired: false,
                spec: SpawnSpec { run: RunId { id: 3, generation }, program: "codex".into(),
                    args: ["resume", "-c", "k=v", "--", "th-1"].map(String::from).to_vec(), env: BTreeMap::new(),
                    cwd: std::env::temp_dir(), cols: 120, rows: 32 } });
            let log = crate::terminal::host_log_path(3, generation);
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            std::fs::write(&log, output).unwrap();
            hub.reconcile_failed_resumes();
            let detail = hub.session(3).unwrap().state_detail.unwrap_or_default();
            assert_eq!(hub.store.codex_thread(3).unwrap().is_none(), gone, "{output}");
            assert_eq!(detail.contains("couldn't reopen its conversation"), gone, "{detail}");
            let _ = std::fs::remove_dir_all(log.parent().unwrap());
        }
    }

    /// A launch record for session `id`'s run `generation`, as `start_process` saves it.
    fn launch_record(id: SessionId, generation: u64, args: &[&str], retired: bool) -> StoredTerminal {
        StoredTerminal { host_instance: "gone".into(), token: format!("t-{id}-{generation}"),
            key_refs: Default::default(), key_fp: None, port_base: 20000, pending: false, retired,
            spec: SpawnSpec { run: RunId { id, generation }, program: "agent".into(),
                args: args.iter().map(|s| s.to_string()).collect(), env: BTreeMap::new(),
                cwd: std::env::temp_dir(), cols: 120, rows: 32 } }
    }

    /// A backend that starts on `hub`'s store (a reload).
    fn reloaded(hub: &Hub) -> Arc<Hub> {
        let cfg = HubConfig { port: 0, client_token: "reloaded".into(),
            muster_bin: PathBuf::from("/bin/true"), terminal_host: None, openrouter_key: None };
        Hub::new(cfg, hub.store.clone()).unwrap()
    }

    #[test]
    fn a_reload_before_the_first_notify_keeps_the_resumed_thread_top_level() {
        // Codex's hooks off: the run resumed th-a, and no hook came before the reload.
        let hub = codex_fixture(&[], false);
        let generation = 9_300_000;
        hub.store.save_terminal(3, &launch_record(3, generation, &["resume", "-c", "k=v", "--", "th-a"], false)).unwrap();
        hub.store.set_codex_thread(3, Some(&crate::store::CodexThread { thread: "th-a".into(), generation })).unwrap();
        let hub = reloaded(&hub);
        {
            let g = hub.inner.lock();
            let e = g.sessions.get(&3).unwrap();
            assert_eq!((e.conversation.as_deref(), e.conversation_sure, e.opened), (Some("th-a"), true, false));
        }
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        hub.set_state_for_test(3, AgentState::Working);
        let hook = |event: &str, payload: serde_json::Value| hub.handle_hook("codex-token", event, payload).unwrap();
        let saved = || hub.store.codex_thread(3).unwrap().map(|t| t.thread);
        hook("notify", turn_done("th-b"));
        assert_eq!(saved().as_deref(), Some("th-a"), "a sub-agent's turn isn't the conversation");
        assert_eq!(hub.session(3).unwrap().state, AgentState::Working, "nor the tab's turn ending");
        hook("notify", turn_done("th-a"));
        assert_eq!(saved().as_deref(), Some("th-a"));
        assert_eq!(hub.session(3).unwrap().state, AgentState::Idle);
        // A fresh launch (no `resume`) has no conversation until it reports one.
        assert_eq!(recovered_conversation(SessionKind::Codex, Some(&["-c".to_string(), "k=v".to_string()])), (None, false));
        assert_eq!(recovered_conversation(SessionKind::Claude, Some(&["--resume".to_string(), "c-1".to_string()])), (None, false));
        assert_eq!(recovered_conversation(SessionKind::Codex, None), (None, false));
    }

    #[test]
    fn a_run_that_opened_its_conversation_keeps_it_whatever_its_output_quotes() {
        let uuid = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
        let codex_gone = format!("Error: No saved session found with ID {uuid}. Run `codex resume` without an ID to choose from existing sessions.\r\n");
        let claude_gone = "> grep for \"No conversation found with session ID\" in src/\r\n";
        // (kind, a hook came from the run, forgotten). The Codex tab is 20 columns wide, so its
        // message wraps through the id.
        for (case, (kind, hooked, gone)) in [(SessionKind::Claude, true, false), (SessionKind::Claude, false, true),
            (SessionKind::Codex, true, false), (SessionKind::Codex, false, true)].into_iter().enumerate() {
            let hub = codex_fixture(&[], false);
            let (id, output, args) = match kind {
                SessionKind::Claude => (2, claude_gone.to_string(), vec!["--resume", "conv-1"]),
                _ => (3, codex_gone.clone(), vec!["resume", "-c", "k=v", "--", uuid]),
            };
            let generation = 9_400_000 + case as u64;
            hub.store.save_terminal(id, &launch_record(id, generation, &args, false)).unwrap();
            match kind {
                SessionKind::Claude => hub.store.set_session_extra_args(id, &["--resume".into(), "conv-1".into()]).unwrap(),
                _ => hub.store.set_codex_thread(id, Some(&crate::store::CodexThread { thread: uuid.into(), generation })).unwrap(),
            }
            if hooked {
                hub.store.save_runtime(&format!("hook-session:{id}"),
                    &crate::store::HookSession { generation, ..Default::default() }).unwrap();
            }
            let mut info = hub.session(id).unwrap();
            (info.state, info.exit_code, info.cols) = (AgentState::Exited, Some(1), 20);
            hub.store.update_session(&info).unwrap();
            let log = crate::terminal::host_log_path(id, generation);
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            std::fs::write(&log, &output).unwrap();
            // The run ended while no backend watched; the next one checks it at startup.
            let hub = reloaded(&hub);
            assert_eq!(hub.inner.lock().sessions.get(&id).unwrap().opened, hooked);
            hub.reconcile_failed_resumes();
            let kept = match kind {
                SessionKind::Claude => hub.inner.lock().sessions.get(&id).unwrap().extra_args == ["--resume", "conv-1"],
                _ => hub.store.codex_thread(id).unwrap().is_some(),
            };
            assert_eq!(kept, !gone, "{kind:?} hooked={hooked}");
            let _ = std::fs::remove_dir_all(log.parent().unwrap());
        }
        // A hook marks the live run as opened; a new run starts unopened.
        let hub = codex_fixture(&[], false);
        assert!(!hub.inner.lock().sessions.get(&3).unwrap().opened);
        hub.handle_hook("codex-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"})).unwrap();
        assert!(hub.inner.lock().sessions.get(&3).unwrap().opened);
        next_codex_start(&hub, 3);
        assert!(!hub.inner.lock().sessions.get(&3).unwrap().opened);
        assert!(!conversation_missing(SessionKind::Claude, 1, true, None, Some("No conversation found with session ID: c-1")));
    }

    #[test]
    fn a_tab_keeps_its_codex_thread_across_a_launch_that_never_started() {
        // (the session's launch record at reload, resumed). The thread was carried by launch 8.
        let retired = |generation| Some(launch_record(3, generation, &["resume", "--", "th-1"], true));
        for (record, resumed) in [
            (retired(8), true),                                      // its spawn failed
            (None, true),                                            // that record cleaned up since
            (Some(launch_record(3, 7, &["-c", "k=v"], false)), true), // it failed before its record
            (Some(launch_record(3, 8, &["resume", "--", "th-1"], false)), true), // it ran (and ended)
            (Some(launch_record(3, 9, &["-c", "k=v"], false)), false), // an older backend's run since
            (retired(9), false),
        ] {
            let hub = codex_fixture(&[], false);
            if let Some(record) = &record { hub.store.save_terminal(3, record).unwrap() }
            hub.store.set_codex_thread(3, Some(&crate::store::CodexThread { thread: "th-1".into(), generation: 8 })).unwrap();
            let hub = reloaded(&hub);
            assert_eq!(next_codex_start(&hub, 3).is_some(), resumed, "{record:?}");
            assert_eq!(hub.store.codex_thread(3).unwrap().is_some(), resumed, "{record:?}");
        }
    }

    #[test]
    fn a_named_codex_agent_keeps_its_top_level_thread_and_start_fresh_wins() {
        let hub = codex_fixture(&["-m", "x"], true);
        let hook = |event: &str, payload: serde_json::Value| hub.handle_hook("codex-token", event, payload).unwrap();
        let resume_id = || hub.agents.by_session(3).unwrap().resume_id;
        hook("SessionStart", serde_json::json!({"session_id": "th-9", "source": "startup"}));
        hook("notify", turn_done("th-child"));
        assert_eq!(resume_id(), None, "the top-level thread is known: a child's turn isn't it");
        hook("notify", turn_done("th-9"));
        assert_eq!(resume_id().as_deref(), Some("th-9"));
        assert_eq!(hub.store.codex_thread(3).unwrap(), None, "on the agent's record, not the tab's");
        assert_eq!(hub.store.confirmed_codex_agent_thread(hub.agents.by_session(3).unwrap().id).unwrap().as_deref(), Some("th-9"));
        // Its arguments carry no thread (older backends rebuild and check them); the start does.
        assert_eq!(crate::agents::restart_args(&hub.agents, 3).unwrap().unwrap(), Vec::<String>::new());
        assert_eq!(next_codex_start(&hub, 3).as_deref(), Some("th-9"));
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        // `/new` in the agent's tab forgets the old thread at once.
        hook("SessionStart", serde_json::json!({"session_id": "th-10", "source": "startup"}));
        assert_eq!(resume_id(), None);
        hook("notify", turn_done("th-10"));
        assert_eq!(resume_id().as_deref(), Some("th-10"));
        // Start fresh: a turn-complete the old run still sends can't undo it.
        hub.fence_conversation(3);
        let aid = hub.agents.by_session(3).unwrap().id;
        hub.agents.update(&hub.store, aid, |a| a.resume_id = None).unwrap();
        hook("notify", turn_done("th-10"));
        assert_eq!(resume_id(), None, "fenced");
        assert_eq!(next_codex_start(&hub, 3), None);
        hub.inner.lock().tokens.insert("codex-token".into(), 3);
        hook("notify", turn_done("th-11"));
        assert_eq!(resume_id().as_deref(), Some("th-11"), "the new run saves its own");
    }

    #[test]
    fn codex_resume_data_survives_an_older_backend_in_between() {
        // An older backend's records: a Codex agent whose resume_id it saved from any notify
        // (a title request's here), and a Codex tab with plain arguments.
        let store = Store::open_memory().unwrap();
        let agent =|name: &str, resume: Option<&str>| crate::agents::Agent {
            account_profile_id: None, provider: Default::default(), id: 0, name: name.into(),
            kind: SessionKind::Codex, model: None, effort: None, persona: String::new(),
            home_dir: "/x".into(), permissions: Permissions::default(),
            accepts_origins: crate::agents::default_accepts(SessionKind::Codex),
            channel_id: None, session_id: None, resume_id: resume.map(Into::into), created_at: 0, autostart: false,
        };
        let legacy = store.insert_agent(&agent("old", Some("th-title"))).unwrap();
        let confirmed = store.insert_agent(&agent("new", Some("th-1"))).unwrap();
        store.confirm_codex_agent_thread_for_test(confirmed, "th-1");
        let reg = crate::agents::Registry::load(&store).unwrap();
        assert_eq!(reg.by_id(legacy).unwrap().resume_id, None, "an unconfirmed (older backend's) thread isn't resumed");
        assert_eq!(store.load_agents().unwrap().into_iter().find(|a| a.id == legacy).unwrap().resume_id, None);
        assert_eq!(reg.by_id(confirmed).unwrap().resume_id.as_deref(), Some("th-1"));
        // An older backend then replaced the confirmed one (say with a sub-agent's): not resumed.
        store.update_agent_runtime(confirmed, None, None, Some("th-sub")).unwrap();
        let reg = crate::agents::Registry::load(&store).unwrap();
        assert_eq!(reg.by_id(confirmed).unwrap().resume_id, None);

        // A tab: new -> old -> new. What the new backend stores stays valid for the older
        // backend's argument check (the master allowlist this branch keeps), and the older
        // backend's own run in between means a new conversation.
        let hub = codex_fixture(&["-m", "gpt-6-sol"], false);
        hub.handle_hook("codex-token", "notify", turn_done("th-1")).unwrap();
        let stored = hub.store.load_sessions().unwrap().into_iter().find(|s| s.info.id == 3).unwrap().extra_args;
        crate::adapters::check_extra_args(SessionKind::Codex, &stored).expect("an older backend restarts it");
        assert!(!stored.iter().any(|a| a.contains("resume")));
        let carried = hub.store.codex_thread(3).unwrap().unwrap();
        assert_eq!(carried.thread, "th-1");
        // The older backend restarts it (a new generation, plain args); the new one is back.
        hub.inner.lock().sessions.get_mut(&3).unwrap().generation = carried.generation + 5;
        assert_eq!(next_codex_start(&hub, 3), None, "the older backend's run started a new conversation");
    }


    #[test]
    fn cache_goes_cold_a_ttl_after_the_last_turn_and_follows_the_prefs() {
        let hub = move_fixture();
        hub.inner.lock().tokens.insert("cache-token".into(), 2);
        hub.set_state_for_test(1, AgentState::Idle);
        hub.set_state_for_test(2, AgentState::Idle);
        assert_eq!(hub.session(2).unwrap().cache_cold_at, None, "no turn yet: nothing cached");
        let before = now_ms();
        hub.handle_hook("cache-token", "UserPromptSubmit", serde_json::json!({"prompt": "hi"})).unwrap();
        hub.handle_hook("cache-token", "Stop", serde_json::json!({})).unwrap();
        let s = hub.session(2).unwrap();
        let at = s.last_turn_at.expect("a turn ended");
        assert!(at >= before);
        assert_eq!(s.cache_cold_at, Some(at + 60 * 60_000), "Claude on its subscription: 1 h");
        assert_eq!(hub.cache_cold(2, at + 59 * 60_000), None);
        assert_eq!(hub.cache_cold(2, at + 61 * 60_000), Some(61 * 60_000));
        let stored = hub.store.load_sessions().unwrap().into_iter().find(|x| x.info.id == 2).unwrap().info;
        assert_eq!(stored.last_turn_at, Some(at), "kept across a backend restart");
        // Shells have no model and no cache.
        assert!(hub.set_state_checked(1, StateUpdate::new(AgentState::Working), StateSource::Screen, None));
        assert!(hub.set_state_checked(1, StateUpdate::new(AgentState::Idle), StateSource::Screen, None));
        assert_eq!((hub.session(1).unwrap().last_turn_at, hub.cache_cold(1, i64::MAX)), (None, None));
        // A shorter lifetime, then the warning off: every session follows, and says so.
        let mut events = hub.subscribe();
        hub.set_ui_prefs(UiPrefsChange {
            cache_ttl: Some(crate::cache::CacheTtlChange { claude: Some(5), ..Default::default() }),
            ..Default::default()
        }).unwrap();
        assert_eq!(hub.session(2).unwrap().cache_cold_at, Some(at + 5 * 60_000));
        let mut upserted = false;
        while let Ok(e) = events.try_recv() {
            upserted |= matches!(e, ServerEvent::SessionUpserted(ref i) if i.id == 2 && i.cache_cold_at == Some(at + 5 * 60_000));
        }
        assert!(upserted, "clients hear the new cold time");
        hub.set_ui_prefs(UiPrefsChange { cache_warn: Some(false), ..Default::default() }).unwrap();
        assert_eq!(hub.cache_cold(2, i64::MAX), None);
        assert!(hub.set_ui_prefs(UiPrefsChange {
            cache_ttl: Some(crate::cache::CacheTtlChange { codex: Some(0), ..Default::default() }),
            ..Default::default()
        }).is_err());
    }

    #[test]
    fn a_cold_send_marks_its_message_and_the_timeline() {
        use crate::agents::{AgentMessage, Origin, Prov};
        let hub = agent_fixture();
        hub.set_state_for_test(2, AgentState::Idle);
        let now = now_ms();
        {
            let mut g = hub.inner.lock();
            let info = &mut g.sessions.get_mut(&2).unwrap().info;
            info.last_turn_at = Some(now - 90 * 60_000);
            hub.cache_rules.lock().refresh(info);
        }
        let idle = hub.cache_cold(2, now).expect("idle 1 h 30 m past a 1 h cache");
        assert_eq!(idle, 90 * 60_000);
        let msg = hub.store.insert_agent_message(&AgentMessage { id: 0, from: "schedule:1".into(),
            from_session: None, to_agent: 1, to_session: Some(2), origin: Origin::Schedule, hop: 0,
            reply_to: None, created_at: 1, reply: None, replied_at: None, text: Some("digest".into()),
            refused: None, to_name: None, cold_idle_ms: None }).unwrap();
        let mut events = hub.subscribe();
        let prov = Prov { origin: Origin::Schedule, hop: 0, msg_id: Some(msg) };
        hub.note_cold_send(2, prov, false, &format!("msg:{msg}"), idle);
        assert_eq!(hub.store.agent_message(msg).unwrap().unwrap().cold_idle_ms, Some(idle));
        // A retry of the same prompt is recorded once.
        hub.note_cold_send(2, prov, false, &format!("msg:{msg}"), idle + 5);
        assert_eq!(hub.store.agent_message(msg).unwrap().unwrap().cold_idle_ms, Some(idle));
        let mut published = false;
        while let Ok(e) = events.try_recv() {
            if let ServerEvent::AgentMessage(m) = e { published |= m["id"] == msg && m["cold_idle_ms"] == idle; }
        }
        assert!(published, "clients hear the message is a cold start");
        let channel = hub.session(2).unwrap().channel_id;
        let logged = hub.store.timeline(channel, None, 50).unwrap();
        assert_eq!(logged.iter().filter(|e| e.data.get("cold_start").is_some()).count(), 1, "one notice");
        // Panel events carry no message: their batch is what a retry repeats.
        let panel = Prov { origin: Origin::Human, hop: 0, msg_id: None };
        hub.note_cold_send(2, panel, true, "panel:7", idle);
        hub.note_cold_send(2, panel, true, "panel:7", idle);
        hub.note_cold_send(2, panel, true, "panel:8", idle);
        let logged = hub.store.timeline(channel, None, 50).unwrap();
        assert_eq!(logged.iter().filter(|e| e.data.get("cold_start").is_some()).count(), 3, "one per delivery");
        let note = logged.iter().find(|e| e.data.get("cold_start").is_some()).expect("a timeline notice");
        assert!(note.data["text"].as_str().unwrap().contains("after 1 h 30 m idle"), "{}", note.data["text"]);
        // Warm, working or a shell: never cold.
        hub.set_state_for_test(2, AgentState::Working);
        assert_eq!(hub.cache_cold(2, now), None);
        assert_eq!(hub.cache_cold(1, i64::MAX), None);
    }

    #[test]
    fn only_real_model_activity_moves_the_cache_clock() {
        let hub = move_fixture();
        hub.set_state_for_test(2, AgentState::Idle);
        let last = |hub: &Hub| hub.session(2).unwrap().last_turn_at;
        let set_last = |hub: &Hub, at: Option<i64>| hub.inner.lock().sessions.get_mut(&2).unwrap().info.last_turn_at = at;
        // A CLI that sends no hooks: the screen's Working is all there is.
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Working), StateSource::Screen, None));
        assert!(last(&hub).is_some());
        // A timer's Stuck (and its way out) is no request.
        set_last(&hub, Some(1));
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Stuck), StateSource::Timer, None));
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Working), StateSource::Timer, None));
        assert_eq!(last(&hub), Some(1));
        // Leaving the Working that backend recovery put there (to re-read a dialog) is none either.
        hub.inner.lock().sessions.get_mut(&2).unwrap().unverified_working = true;
        assert!(hub.set_state_checked(2, StateUpdate::with(AgentState::NeedsApproval, "Bash"), StateSource::Screen, None));
        assert_eq!(last(&hub), Some(1));
        assert!(!hub.inner.lock().sessions[&2].unverified_working, "cleared by the next state");
        // Once hooks report, they alone move it: a failed turn doesn't, a Stop does.
        hub.inner.lock().tokens.insert("cache-token".into(), 2);
        hub.handle_hook("cache-token", "UserPromptSubmit", serde_json::json!({"prompt": "go"})).unwrap();
        hub.handle_hook("cache-token", "StopFailure", serde_json::json!({"error_type": "rate_limit"})).unwrap();
        assert_eq!(last(&hub), Some(1));
        hub.inner.lock().sessions.get_mut(&2).unwrap().last_hook_at = 1; // past the hook grace
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Working), StateSource::Screen, None));
        assert!(hub.set_state_checked(2, StateUpdate::new(AgentState::Idle), StateSource::Screen, None));
        assert_eq!(last(&hub), Some(1), "the screen doesn't move a hooked session's clock");
        // A callback replayed after an outage counts from when it was captured.
        let captured = now_ms() - 90 * 60_000;
        hub.handle_hook_checkpoint("cache-token", "Stop", serde_json::json!({}), None, u64::MAX, Some(captured)).unwrap();
        assert_eq!(last(&hub), Some(captured));
        assert!(hub.cache_cold(2, now_ms()).is_some(), "a turn that ended 90 min ago is cold");
    }

    #[test]
    fn a_closed_agents_conversation_keeps_its_cache_clock() {
        let hub = agent_fixture();
        hub.set_state_for_test(2, AgentState::Idle);
        hub.inner.lock().tokens.insert("cache-token".into(), 2);
        hub.handle_hook("cache-token", "SessionStart", serde_json::json!({"session_id": "conv-1", "source": "startup"})).unwrap();
        hub.handle_hook("cache-token", "Stop", serde_json::json!({})).unwrap();
        let at = hub.session(2).unwrap().last_turn_at.expect("a turn");
        let agent = hub.agents.by_session(2).unwrap();
        assert_eq!(agent.resume_id.as_deref(), Some("conv-1"));
        // What remove_session does as the session goes (not called here: it deletes the shared
        // session folder other tests use).
        hub.save_conversation_turn(&hub.session(2).unwrap());
        // Its next session resumes conv-1 and takes the clock back.
        hub.agents.link_session(&hub.store, agent.id, Some(1)).unwrap();
        assert_eq!(hub.saved_conversation_turn(1), Some(at));
    }
}
