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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannel {
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
}

/// What `attach` hands to a client connection.
pub enum Attach {
    /// Running (or exited-this-run) session: replay bytes + live output receiver + the
    /// generation of the process they belong to (taken together, so a restart can't
    /// mismatch them; see `pty_generation`).
    Live(Vec<u8>, broadcast::Receiver<Output>, u64),
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
    pty: Arc<PtySession>,
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
    /// The last title this session set that was usable as a channel name (`clean_title`).
    /// Later unusable titles (a shell prompt, the program name) don't clear it.
    usable_title: Option<String>,
    /// Port block (`PORT`) the current process was started with; it stays reserved while that
    /// process runs, also after the session moved to another channel. 0: none yet.
    port_base: u16,
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

pub struct Hub {
    cfg: HubConfig,
    store: Store,
    tx: broadcast::Sender<ServerEvent>,
    inner: Mutex<Inner>,
    generation: AtomicU64,
    terminal_records: Mutex<BTreeMap<SessionId, StoredTerminal>>,
    shutting_down: AtomicBool,
    /// Client replays of exited sessions rebuilt from output.log (a vt100 pass over up to
    /// 2 MiB), keyed by session and the log's (length, mtime).
    replay_cache: Mutex<HashMap<SessionId, (u64, std::time::SystemTime, Bytes)>>,
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
                if record.retired {
                    if let Some(status) = inventory.iter().find(|s| s.run == record.spec.run) {
                        if status.has_processes {
                            host.client.call(HostCommand::Terminate { run: status.run, force: false })?;
                        } else if status.drained {
                            let _ = host.client.call(HostCommand::Forget { run: status.run });
                        }
                    }
                } else if record.pending {
                    host.spawn(record.spec.clone())?;
                    record.pending = false;
                    store.save_terminal(record.spec.run.id, record)?;
                }
            }
            inventory = host.inventory()?;
        }
        let next_generation = terminal_records.values().map(|r| r.spec.run.generation)
            .chain(inventory.iter().map(|s| s.run.generation)).max().unwrap_or(0) + 1;
        match store.prune_events(now_ms() - EVENT_MAX_AGE_MS, EVENTS_PER_CHANNEL) {
            Ok(n) if n > 0 => tracing::info!("pruned {n} old events"),
            Ok(_) => {}
            Err(e) => tracing::warn!("pruning events: {e}"),
        }
        let mut inner = Inner::default();
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
                }
            } else if info.state != AgentState::Exited {
                info.state = AgentState::Exited;
                info.state_detail = Some(if record.is_some() { "terminal host restarted or run was lost" } else { "legacy daemon restarted" }.into());
                info.pid = None;
                info.queued = 0;
            }
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
                    last_hook_at: 0,
                    last_dequeue_at: 0,
                    busy: false,
                    kill_requested: false,
                    usable_title,
                    port_base,
                },
            );
        }
        for w in store.load_windows()? {
            // Nothing holds a window at startup: ones without tabs or tasks are gone for good.
            if window_unused(&inner, w.id) {
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
        let devices = crate::devices::Registry::load(&store)?;
        let agents = crate::agents::Registry::load(&store)?;
        let injection = crate::inject::Queues::load(&store)?;
        let (tx, _) = broadcast::channel(EVENT_BUS_CAP);
        let hub = Arc::new_cyclic(|weak| Hub {
            cfg,
            devices,
            agents,
            injection,
            mcp: Mutex::new(None),
            state_seq,
            store,
            tx,
            inner: Mutex::new(inner),
            generation: AtomicU64::new(next_generation),
            terminal_records: Mutex::new(terminal_records),
            shutting_down: AtomicBool::new(false),
            replay_cache: Mutex::new(HashMap::new()),
            branch_lock: tokio::sync::Mutex::new(()),
            rt: tokio::runtime::Handle::try_current().ok(),
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
                    TerminalEvent::Drained { id, generation } => hub.pty_drained(id, generation),
                }
            }), Arc::new(move |message| {
                if let Some(hub) = unavailable.upgrade() { hub.notice("error", message); }
            }))?;
        }
        crate::inject::restore(&hub)?;
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

    fn pty(&self, id: SessionId) -> Option<Arc<PtySession>> {
        self.inner.lock().sessions.get(&id).and_then(|e| e.pty.clone())
    }

    // ----- session I/O -----

    /// Raw bytes to the PTY.
    pub fn write_input(&self, id: SessionId, bytes: &[u8]) -> anyhow::Result<()> {
        match self.pty(id) {
            Some(p) => p.write(bytes),
            None if self.session(id).is_some() => anyhow::bail!("session {id} is not running"),
            None => anyhow::bail!("no such session: {id}"),
        }
    }

    /// Bind delayed input to the process it was prepared for, even across a concurrent restart.
    pub(crate) fn write_input_generation(&self, id: SessionId, generation: Option<u64>, bytes: &[u8]) -> anyhow::Result<()> {
        let terminal = self.pty(id).filter(|terminal| Some(terminal.generation) == generation)
            .context("the terminal run changed before input delivery")?;
        terminal.write(bytes)
    }

    /// Is the session's terminal in application cursor mode (DECCKM)?
    pub fn application_cursor(&self, id: SessionId) -> bool {
        self.pty(id).is_some_and(|p| p.application_cursor())
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
            let (replay, rx) = p.attach(max_body)?;
            return Ok(Attach::Live(replay, rx, p.generation));
        }
        let info = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        let path = self.session_log_path(id);
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
        if let Some((len, mtime)) = stamp
            && max_body.is_none()
            && let Some((l, m, b)) = self.replay_cache.lock().get(&id)
            && *l == len
            && *m == mtime
        {
            return Ok(Attach::Replay(b.to_vec()));
        }
        let (cols, rows) = (info.cols, info.rows);
        let p2 = path.clone();
        let data = tokio::task::spawn_blocking(move || pty::log_replay(&p2, cols, rows, max_body)).await?;
        if max_body.is_some() {
            // Only whole replays are cached.
            return Ok(Attach::Replay(data));
        }
        if let Some((len, mtime)) = stamp {
            let mut c = self.replay_cache.lock();
            if c.len() >= REPLAY_CACHE_MAX && !c.contains_key(&id) {
                if let Some(k) = c.keys().next().copied() {
                    c.remove(&k);
                }
            }
            c.insert(id, (len, mtime, Bytes::from(data.clone())));
        }
        Ok(Attach::Replay(data))
    }

    /// Apply a state change with precedence rules:
    /// - `Exited` is terminal.
    /// - A `Screen` update doesn't override a `Hook` update less than 1.5s old, unless it
    ///   reports NeedsApproval/AskingQuestion (prompts are visible before hooks may fire).
    /// - No-op if state and detail are unchanged.
    /// On change: updates SessionInfo, broadcasts SessionUpserted, logs a StateChange event.
    /// Returns true if the state changed.
    pub fn set_state(&self, id: SessionId, update: StateUpdate, source: StateSource) -> bool {
        self.set_state_checked(id, update, source, None)
    }

    /// `set_state`, optionally only if the session's process generation still matches
    /// (checked under the same lock as the mutation).
    fn set_state_checked(
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
            if let Err(err) = self.store.update_session(&e.info) {
                tracing::warn!(session = id, "persisting session: {err}");
            }
            (from, e.info.clone())
        };
        let channel_id = info.channel_id;
        self.emit(ServerEvent::SessionUpserted(info));
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
        let info = {
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
            e.info.clone()
        };
        self.emit(ServerEvent::SessionUpserted(info));
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
        let info = {
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
            e.info.clone()
        };
        self.emit(ServerEvent::SessionUpserted(info));
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
            e.info.clone()
        };
        self.emit(ServerEvent::SessionUpserted(info.clone()));
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

    // Exited terminal handles are retained: their host owns replay and descendant
    // cleanup. A handle contains metadata, not a PTY or a multi-megabyte buffer.
    fn try_release(&self, _id: SessionId, _generation: u64) {}

    pub(crate) fn pty_exited(&self, id: SessionId, generation: u64, code: i32) {
        let channel_id = {
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
            let token = e.token.take();
            let ch = e.info.channel_id;
            if let Some(t) = token {
                g.tokens.remove(&t);
            }
            ch
        };
        crate::inject::drop_queue(self, id);
        // Replay/screen for exited sessions are served from the log once the PTY is released.
        self.try_release(id, generation);
        let detail = if self.shutting_down.load(Ordering::SeqCst) {
            "daemon stopped".to_string()
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

    /// The reader of a PTY saw EOF (nothing holds the terminal any more). Releases the PTY
    /// of an exited process.
    pub(crate) fn pty_drained(&self, id: SessionId, generation: u64) {
        self.try_release(id, generation);
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
                .chain(g.sessions.values().filter(|e| e.pty.is_some() && e.port_base != 0).map(|e| e.port_base))
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
                from_session: None,
                prov: req.prov,
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
                        .find(|(wid, w)| w.holder.is_none() && has_sessions(&g, **wid))
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
            // Adopt orphans: live channels whose window is unset or gone.
            let mut orphans: Vec<(i64, ChannelId)> = g
                .channels
                .values()
                .filter(|c| !c.archived && c.window_id.is_none_or(|w| !g.windows.contains_key(&w)))
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
                .filter(|e| e.info.window_id.is_none_or(|w| !g.windows.contains_key(&w)))
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
            window_view(&g, target).ok_or_else(|| anyhow::anyhow!("window {target} vanished"))?
        };
        events.push(ServerEvent::WindowUpserted(w.clone()));
        for ev in events {
            self.emit(ev);
        }
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

    /// After a window lost a tab (session): delete it if nobody holds it and it has no tabs or
    /// tasks left. Returns whether it was deleted. (Its `Window` view is unchanged otherwise.)
    fn drop_if_unused_locked(&self, g: &mut Inner, id: WindowId, events: &mut Vec<ServerEvent>) -> bool {
        if !g.windows.get(&id).is_some_and(|w| w.holder.is_none()) || !window_unused(g, id) {
            return false;
        }
        if let Err(e) = self.store.delete_window(id) {
            tracing::warn!(window = id, "deleting window: {e}");
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
                .filter(|e| e.info.channel_id == id && e.info.window_id != Some(window_id))
                .map(|e| e.info.id)
                .collect();
            for sid in sids {
                let Some(e) = g.sessions.get_mut(&sid) else { continue };
                let mut next = e.info.clone();
                next.window_id = Some(window_id);
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
            ch
        };
        for ev in events {
            self.emit(ev);
        }
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
        for sid in &sids {
            let _ = self.remove_session(*sid);
        }
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
                // Give killed processes a moment to leave the directory.
                if !sids.is_empty() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
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
        dir_user(ch, self.inner.lock().sessions.values().map(|e| (&e.info, e.pty.is_some() || e.busy)))
    }

    /// Mark `ch`'s worktree as being deleted, unless a session outside `ch` runs (or is
    /// starting) in it; checked and marked under one lock, and `start_process` refuses cwds
    /// inside a marked directory, so nothing can start there until `release_worktree`.
    fn reserve_worktree_removal(self: &Arc<Self>, ch: &Channel) -> Result<WorktreeReservation, SessionId> {
        let mut g = self.inner.lock();
        if let Some(user) = dir_user(ch, g.sessions.values().map(|e| (&e.info, e.pty.is_some() || e.busy))) {
            return Err(user);
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
                state_seq: self.state_seq.next()?,
                created_at: now,
                window_id: Some(window_id),
            };
            let stored = StoredSession {
                info,
                extra_args: req.extra_args.clone(),
            };
            let id = self.store.insert_session(&stored)?;
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
                    last_dequeue_at: 0,
                    busy: true,
                    kill_requested: false,
                    usable_title: None,
                    port_base: 0,
                },
            );
            id
        };
        for ev in events {
            self.emit(ev);
        }
        self.agents.set_prov(id, req.prov)?;
        let prompt = req.prompt.clone().filter(|p| !p.trim().is_empty());
        let started = self.start_process(id, prompt.clone());
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
                        "name": info.name,
                        "parent_id": req.parent_id,
                    }),
                );
                if let Some(code) = early_exit {
                    self.pty_exited(id, generation, code);
                } else if let Some(text) = prompt {
                    self.note_prompt(id, &text);
                    let source = match req.prov {
                        Some(p) => crate::agents::Source::Chain(p),
                        None => crate::agents::Source::Human,
                    };
                    self.deliver_initial_prompt(id, ch.id, text, consumed, req.parent_id, source);
                }
                Ok(self.session(id).unwrap_or(info))
            }
            Err(e) if e.downcast_ref::<crate::terminal::SpawnUncertain>().is_some() => {
                let _ = self.update_session_info(id, |info| info.state_detail = Some(e.to_string()));
                Err(e)
            }
            Err(e) => {
                self.inner.lock().sessions.remove(&id);
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
        source: crate::agents::Source,
    ) {
        let from = match parent {
            Some(p) => format!("session:{p}"),
            None => "human".to_string(),
        };
        if consumed {
            self.log_event(
                Some(channel_id),
                Some(id),
                EventKind::Prompt,
                serde_json::json!({"text": text, "session_id": id, "from": from, "queued": false}),
            );
            return;
        }
        let hub = self.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::inject::send(&hub, id, text, crate::inject::SendMode::Queue, from, source).await {
                hub.notice(
                    "error",
                    format!("Could not deliver the initial prompt to session {id}: {e:#}"),
                );
            }
        });
    }

    /// Start a process for an existing session entry. Returns the updated info and whether
    /// the adapter consumed the initial prompt.
    /// The caller logs SessionStarted, then reports `early_exit` via `pty_exited`.
    fn start_process(self: &Arc<Self>, id: SessionId, prompt: Option<String>) -> anyhow::Result<Started> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let (info, extra_args, port_base, permissions) = {
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
            e.generation = generation;
            e.early_exit = None;
            let (info, extra) = (e.info.clone(), e.extra_args.clone());
            // Also on restarts: args stored before the allowlist existed are replayed here.
            crate::adapters::check_extra_args(info.kind, &extra)?;
            let ch = g.channels.get(&info.channel_id);
            let port_base = ch.map(|c| c.port_base).unwrap_or(20000);
            let permissions = ch.map(|c| c.permissions).unwrap_or_default();
            if let Some(e) = g.sessions.get_mut(&id) {
                e.port_base = port_base;
            }
            (info, extra, port_base, permissions)
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
            initial_prompt: prompt,
            permissions,
        };
        let spec = adapter_for(info.kind).command(&ctx)?;
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
        complete_env.extend(env);
        let launch = SpawnSpec {
            run: RunId { id, generation }, program: spec.program.clone(), args: spec.args.clone(),
            env: complete_env, cwd: PathBuf::from(&info.cwd), cols: info.cols, rows: info.rows,
        };
        let mut record = StoredTerminal {
            host_instance: host.client.hello.instance.clone(), token: token.clone(), port_base,
            spec: launch.clone(), pending: true, retired: false,
        };
        self.store.save_terminal(id, &record)?;
        self.terminal_records.lock().insert(id, record.clone());
        let pty = match host.spawn(launch) {
            Ok(pty) => pty,
            Err(error) => {
                if error.downcast_ref::<crate::terminal::SpawnUncertain>().is_none() {
                    record.retired = true;
                    self.store.save_terminal(id, &record)?;
                    self.terminal_records.lock().insert(id, record);
                }
                return Err(error);
            }
        };
        record.pending = false;
        // If this write fails the prepared record still lets the next backend adopt
        // the exact same process. Never roll back a successful spawn by killing it.
        if let Err(error) = self.store.save_terminal(id, &record) {
            tracing::warn!(session = id, "persisting terminal acknowledgement: {error:#}");
        }
        self.terminal_records.lock().insert(id, record);
        let (info, early, kill) = {
            let mut g = self.inner.lock();
            let superseded = match g.sessions.get(&id) {
                None => Some("was removed while starting"),
                Some(e) if e.generation != generation => Some("was restarted concurrently"),
                Some(_) if self.shutting_down.load(Ordering::SeqCst) => Some("not started: daemon is stopping"),
                Some(_) => None,
            };
            if let Some(why) = superseded {
                drop(g);
                pty.discard();
                pty.kill();
                anyhow::bail!("session {id} {why}");
            }
            let Some(e) = g.sessions.get_mut(&id) else {
                anyhow::bail!("session {id} was removed while starting");
            };
            let old = e.token.replace(token.clone());
            e.pty = Some(pty.clone());
            e.last_hook_at = 0;
            e.info.pid = pty.pid;
            e.info.state = AgentState::Starting;
            e.info.state_detail = None;
            e.info.state_since = now_ms();
            e.info.exit_code = None;
            e.info.queued = 0;
            e.info.title = pty.title();
            let early = e.early_exit.take().or_else(|| pty.exit_code());
            let kill = std::mem::take(&mut e.kill_requested);
            let info = e.info.clone();
            if let Err(err) = self.store.update_session(&info) {
                tracing::warn!(session = id, "persisting session: {err}");
            }
            if let Some(old) = old {
                g.tokens.remove(&old);
            }
            g.tokens.insert(token, id);
            (info, early, kill)
        };
        self.emit(ServerEvent::SessionUpserted(info.clone()));
        if kill {
            pty.kill();
        }
        Ok(Started {
            info,
            consumed: spec.consumed_initial_prompt,
            generation,
            early_exit: early,
            pty,
        })
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
                    .unwrap_or(false);
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
                            .unwrap_or(false)
                        {
                            gone = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    // The new process's record replaces this one: keep the survivors' for the
                    // next start's reaper (and keep trying meanwhile).
                    if !gone && let Some(sid) = p.pid {
                        if let Err(e) = self.store.add_reap(id, sid, p.markers()) {
                            tracing::warn!(session = id, "recording pending reap: {e}");
                        }
                        self.confirm_reap(id, sid, p.clone());
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let previous = self.inner.lock().sessions.get_mut(&id).and_then(|e| e.pty.take());
        if let Some(p) = previous {
            p.discard();
            p.forget()?;
        }
        match self.start_process(id, None) {
            Ok(Started {
                info,
                generation,
                early_exit,
                pty,
                ..
            }) => {
                // Only now drop the old scrollback; the new process's first flush rewrites it.
                let _ = std::fs::remove_file(paths::session_dir(id).join("output.log"));
                pty.mark_dirty();
                self.log_event(
                    Some(info.channel_id),
                    Some(id),
                    EventKind::SessionStarted,
                    serde_json::json!({
                        "session_id": id,
                        "kind": info.kind,
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
        if let Some(record) = self.terminal_records.lock().get_mut(&id) {
            record.retired = true;
            self.store.save_terminal(id, record)?;
        }
        let entry = {
            let mut g = self.inner.lock();
            let e = g
                .sessions
                .remove(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            if let Some(t) = &e.token {
                g.tokens.remove(t);
            }
            e
        };
        if let Some(p) = &entry.pty {
            p.discard();
            // Also reaps leftovers (background jobs) of an already exited process.
            p.kill();
            // Until the kill is confirmed, remember the process session: if the daemon stops
            // during the grace period, the next start reaps what ignored SIGHUP.
            // (Whether any are left is checked by `confirm_reap`, off the async workers.)
            if let Some(sid) = p.pid
                && !p.markers().is_empty()
            {
                if let Err(e) = self.store.add_reap(id, sid, p.markers()) {
                    tracing::warn!(session = id, "recording pending reap: {e}");
                }
                self.confirm_reap(id, sid, p.clone());
            }
        } else if let Err(e) = self.store.keep_sid_for_reap(id) {
            tracing::warn!(session = id, "keeping recorded process session: {e}");
        }
        if let Err(e) = self.store.delete_session(id) {
            tracing::warn!(session = id, "deleting session: {e}");
        }
        if let Err(e) = self.store.delete_session_events(id) {
            tracing::warn!(session = id, "deleting session events: {e}");
        }
        let _ = std::fs::remove_dir_all(paths::session_dir(id));
        self.replay_cache.lock().remove(&id);
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
        }
        for ev in events {
            self.emit(ev);
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
            if from == Some(window_id) {
                return Ok(e.info.clone());
            }
            let mut next = e.info.clone();
            next.window_id = Some(window_id);
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
        self.rederive_name(from);
        self.rederive_name(channel_id);
        Ok((info, from))
    }

    /// Wait (off the async workers) for a removed session's processes to die, SIGKILL them after
    /// the grace period, then drop the pending-reap record once none are left.
    fn confirm_reap(&self, id: SessionId, sid: u32, p: Arc<PtySession>) {
        let weak = self.weak.clone();
        let _ = std::thread::Builder::new().name(format!("reap-{id}")).spawn(move || {
            let deadline = std::time::Instant::now() + pty::KILL_GRACE + Duration::from_millis(500);
            while p.has_processes() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            if p.has_processes() {
                p.kill_now();
                let end = std::time::Instant::now() + Duration::from_secs(1);
                while p.has_processes() && std::time::Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if !p.has_processes() {
                if let Err(error) = p.forget() {
                    tracing::warn!(session = id, "forgetting terminated run: {error:#}");
                    return;
                }
                if let Some(hub) = weak.upgrade() {
                    let _ = hub.store.remove_reap(id, sid);
                    let mut records = hub.terminal_records.lock();
                    if records.get(&id).is_some_and(|r| r.retired && r.spec.run.generation == p.generation) {
                        let _ = hub.store.delete_terminal(id);
                        records.remove(&id);
                    }
                }
            }
        });
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
            p.resize(cols, rows)?;
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
    pub fn handle_hook(&self, token: &str, event: &str, payload: serde_json::Value) -> anyhow::Result<()> {
        self.handle_hook_checkpoint(token, event, payload, None)
    }

    pub(crate) fn handle_spooled_hook(&self, token: &str, event: &str, payload: serde_json::Value,
        cursor: &crate::store::HookCursor) -> anyhow::Result<()> {
        self.handle_hook_checkpoint(token, event, payload, Some(cursor))
    }

    fn handle_hook_checkpoint(&self, token: &str, event: &str, payload: serde_json::Value,
        cursor: Option<&crate::store::HookCursor>) -> anyhow::Result<()> {
        // Spool callbacks from a process that exited during an outage may still contain
        // its final reply. Accept only the recorded current run, never a previous generation.
        let recorded = cursor.and_then(|_| self.terminal_records.lock().iter()
            .find(|(_, record)| !record.retired && record.token == token)
            .map(|(id, record)| (*id, record.spec.run.generation)));
        let (info, generation, fx) = {
            let g = self.inner.lock();
            let entry = if cursor.is_some() {
                recorded.and_then(|(id, generation)| g.sessions.get(&id).filter(|entry| entry.generation == generation))
            } else { g.tokens.get(token).and_then(|id| g.sessions.get(id)) };
            let Some(entry) = entry else {
                if let Some(cursor) = cursor {
                    // Removed/restarted sessions cannot receive old callbacks. Advance
                    // the checkpoint so one obsolete run cannot block every other session.
                    self.store.save_runtime("hook-inbox-cursor", cursor)?;
                    return Ok(());
                }
                anyhow::bail!("unknown session token");
            };
            let info = entry.info.clone();
            let fx = self.agents.on_hook_checkpoint(&self.store, &info, event, &payload, cursor)?;
            (info, entry.generation, fx)
        };
        if fx.duplicate { return Ok(()) }
        let id = info.id;
        if let Some(m) = fx.replied { crate::agents::publish_message(self, m); }
        if let Some(a) = fx.agent.and_then(|a| self.agents.by_id(a)) { crate::agents::publish(self, &a); }
        self.log_event(Some(info.channel_id), Some(id), EventKind::Hook,
            serde_json::json!({"session_id": id, "event": event, "summary": hook_summary(&payload)}));
        if let Some(update) = adapter_for(info.kind).on_hook(event, &payload) {
            self.set_state_checked(id, update, StateSource::Hook, Some(generation));
        }
        if let Some(delta) = subagent_delta(event, &payload) {
            self.adjust_subagents_checked(id, delta, Some(generation));
        }
        Ok(())
    }

    /// Change the running-subagent count (runtime only; emits on change). `None`: reset to 0.
    fn adjust_subagents_checked(&self, id: SessionId, delta: Option<i32>, generation: Option<u64>) {
        let info = {
            let mut g = self.inner.lock();
            let Some(e) = g.sessions.get_mut(&id) else { return };
            if generation.is_some_and(|generation| e.generation != generation) { return }
            let next = match delta {
                Some(d) => (e.info.subagents as i64 + d as i64).max(0) as u32,
                None => 0,
            };
            if next == e.info.subagents {
                return;
            }
            e.info.subagents = next;
            e.info.clone()
        };
        self.emit(ServerEvent::SessionUpserted(info));
    }

    // ----- daemon lifecycle -----

    fn all_ptys(&self) -> Vec<Arc<PtySession>> {
        self.inner
            .lock()
            .sessions
            .values()
            .filter_map(|e| e.pty.clone())
            .collect()
    }

    /// Append new output of every session to its output.log (called periodically; no fsync).
    pub fn flush_logs(&self) {
        for p in self.all_ptys() {
            p.flush_log();
            // Exited processes whose leftovers died since: release their PTY now.
            if !p.is_alive() {
                self.try_release(p.id, p.generation);
            }
        }
    }

    /// Stop accepting application work. The independently owned terminals continue.
    /// Only the explicit full-stop CLI operation shuts down the terminal host.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Let already-started submissions reach a recorded outcome. No new queued
        // delivery starts while stopping. A timeout leaves its durable in-flight
        // record for explicit review on recovery, never an automatic duplicate.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while self.injection.in_progress() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

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

    /// A hub over an in-memory store: workspace `/w/repo`, auto-named channels 1 and 2 (window
    /// 1), archived channel 3, sessions of channel 1 (window 1): a shell titled "build the
    /// widget" and a claude without a title.
    fn move_fixture() -> Arc<Hub> {
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
        let cfg = HubConfig {
            port: 0,
            client_token: "t".into(),
            muster_bin: PathBuf::from("/bin/true"),
            terminal_host: None,
        };
        Hub::new(cfg, store).unwrap()
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
            muster_bin: PathBuf::from("/bin/true"), terminal_host: None };
        let replacement = Hub::new(cfg, original.store.clone()).unwrap();
        let current = replacement.session(1).unwrap();
        assert_ne!(old.state_seq, current.state_seq);
        assert!(replacement.claim_prompt(1, Some(old.state_seq), None).is_err());
        replacement.claim_prompt(1, Some(current.state_seq), None).unwrap();
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
}
