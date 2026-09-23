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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::body::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agents::{ScreenSnapshot, SpawnCtx, adapter_for};
use crate::model::*;
use crate::paths;
use crate::pty::{self, PtySession};
use crate::store::{Store, StoredSession};
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannel {
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Create a git worktree + branch `muster/<slug>` for this channel. Ignored for non-git
    /// workspaces (cwd is then the workspace path).
    #[serde(default = "yes")]
    pub worktree: bool,
    /// Branch/commit to base the worktree on. Default: the workspace's current HEAD.
    pub base: Option<String>,
    /// Start an agent right away (usual case). None = empty channel.
    pub agent: Option<SessionKind>,
    /// Initial prompt for that agent.
    pub prompt: Option<String>,
    /// Agent permission profile. Default: the CLIs' own defaults (see `Permissions`).
    #[serde(default, deserialize_with = "de_permissions")]
    pub permissions: Option<Permissions>,
    /// Terminal size for that first agent (what the client will show it at).
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn de_permissions<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Permissions>, D::Error> {
    Ok(Option::<PermissionsInput>::deserialize(d)?.map(Into::into))
}

fn yes() -> bool {
    true
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
}

/// What `attach` hands to a client connection.
pub enum Attach {
    /// Running (or exited-this-run) session: replay bytes + live output receiver + the
    /// generation of the process they belong to (taken together, so a restart can't
    /// mismatch them; see `pty_generation`).
    Live(Vec<u8>, broadcast::Receiver<Bytes>, u64),
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
    /// A spawn or restart is in progress (lifecycle ops on one session are serialized).
    busy: bool,
    /// `kill_session` arrived while a process was being started.
    kill_requested: bool,
}

#[derive(Default)]
struct Inner {
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    channels: BTreeMap<ChannelId, Channel>,
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
    shutting_down: AtomicBool,
    /// Client replays of exited sessions rebuilt from output.log (a vt100 pass over up to
    /// 2 MiB), keyed by session and the log's (length, mtime).
    replay_cache: Mutex<HashMap<SessionId, (u64, std::time::SystemTime, Bytes)>>,
    weak: Weak<Hub>,
}

const REPLAY_CACHE_MAX: usize = 8;

impl Hub {
    /// Load persisted state. Sessions that were running when the previous daemon died are
    /// marked Exited ("daemon restarted").
    pub fn new(cfg: HubConfig, store: Store) -> anyhow::Result<Arc<Hub>> {
        // Processes that outlived a crashed daemon (or ignored SIGHUP, or were left behind by
        // an exited shell) would otherwise keep running unmanaged next to a restarted copy.
        // Waits until they are gone (a few seconds at most, and only if there are any); the
        // records of sessions whose processes survive are kept for the next start.
        let url = format!("MUSTER_URL=http://127.0.0.1:{}", cfg.port);
        let survivors = pty::reap_leftovers(
            store
                .session_sids()
                .unwrap_or_default()
                .into_iter()
                .map(|(id, sid, mut markers)| {
                    // The per-run marker identifies that run's processes (whatever port it
                    // used); rows from before markers were stored fall back to this daemon's URL.
                    if markers.is_empty() {
                        markers.push(url.clone());
                    }
                    let own = format!("MUSTER_SESSION_ID={id}");
                    if !markers.contains(&own) {
                        markers.push(own);
                    }
                    (sid, id, markers)
                })
                .collect(),
        );
        if let Err(e) = store.clear_session_sids(&survivors) {
            tracing::warn!("clearing reaped process sessions: {e}");
        }
        store.mark_all_exited("daemon restarted", now_ms())?;
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
            let StoredSession { info, extra_args } = s;
            inner.sessions.insert(
                info.id,
                SessEntry {
                    info,
                    extra_args,
                    token: None,
                    pty: None,
                    generation: 0,
                    early_exit: None,
                    last_hook_at: 0,
                    busy: false,
                    kill_requested: false,
                },
            );
        }
        let (tx, _) = broadcast::channel(EVENT_BUS_CAP);
        Ok(Arc::new_cyclic(|weak| Hub {
            cfg,
            store,
            tx,
            inner: Mutex::new(inner),
            generation: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            replay_cache: Mutex::new(HashMap::new()),
            weak: weak.clone(),
        }))
    }

    /// Base URL of this daemon, e.g. "http://127.0.0.1:7433".
    pub fn daemon_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.cfg.port)
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

    /// Generation of the session's current process (changes on every restart), if it has one
    /// this daemon run.
    pub fn pty_generation(&self, id: SessionId) -> Option<u64> {
        self.pty(id).map(|p| p.generation)
    }

    /// Current screen as text, from the headless VT.
    pub fn screen(&self, id: SessionId) -> Option<ScreenSnapshot> {
        if let Some(p) = self.pty(id) {
            return Some(p.screen());
        }
        let info = self.session(id)?;
        pty::log_screen(&paths::session_dir(id).join("output.log"), info.cols, info.rows)
    }

    /// Last `lines` lines of output as plain text (scrollback + screen, ANSI stripped).
    pub fn recent_output_text(&self, id: SessionId, lines: usize) -> Option<String> {
        if let Some(p) = self.pty(id) {
            return Some(p.recent_text(lines));
        }
        let info = self.session(id)?;
        pty::log_tail_text(&paths::session_dir(id).join("output.log"), info.cols, info.rows, lines)
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
    pub async fn attach(&self, id: SessionId) -> anyhow::Result<Attach> {
        if let Some(p) = self.pty(id) {
            let (replay, rx) = p.attach();
            return Ok(Attach::Live(replay, rx, p.generation));
        }
        let info = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        let path = paths::session_dir(id).join("output.log");
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
        if let Some((len, mtime)) = stamp
            && let Some((l, m, b)) = self.replay_cache.lock().get(&id)
            && *l == len
            && *m == mtime
        {
            return Ok(Attach::Replay(b.to_vec()));
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
            let from = e.info.state;
            if from != update.state {
                e.info.state_since = now;
            }
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
        let _ = self.update_session_info_checked(id, Some(generation), |i| i.title = title);
    }

    /// Can an exited process's PTY be dropped from the hub? Only once output.log holds the
    /// scrollback and nothing of its session is left running (the PTY is the handle used to
    /// kill leftovers, including ones that closed the terminal, e.g. `nohup … &`). Walks /proc:
    /// call without the hub lock.
    fn releasable(p: &PtySession) -> bool {
        !p.is_alive() && p.is_drained() && p.log_persisted() && !p.has_processes()
    }

    /// Release an exited process's PTY (and its in-memory ring) if it is releasable and its
    /// exit has been recorded. Called after each of the events that can make it releasable
    /// (exit recorded, reader drained, leftovers gone), each time after that event's state is
    /// published, so whichever comes last sees all of them.
    fn try_release(&self, id: SessionId, generation: u64) {
        let Some(p) = self.pty(id).filter(|p| p.generation == generation) else {
            return;
        };
        if !Self::releasable(&p) {
            return;
        }
        let mut g = self.inner.lock();
        // Only after `pty_exited` recorded the exit: it treats a missing PTY as "not installed
        // yet" and would otherwise drop the exit.
        if let Some(e) = g.sessions.get_mut(&id)
            && e.generation == generation
            && e.info.exit_code.is_some()
            && e.pty.as_ref().is_some_and(|q| Arc::ptr_eq(q, &p))
        {
            e.pty = None;
        }
    }

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
            let token = e.token.take();
            let ch = e.info.channel_id;
            if let Some(t) = token {
                g.tokens.remove(&t);
            }
            ch
        };
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
        self.ensure_not_shutting_down()?;
        let ws = self
            .workspace(req.workspace_id)
            .ok_or_else(|| anyhow::anyhow!("no such workspace: {}", req.workspace_id))?;
        let name = req.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("channel name is empty");
        }
        let repo = PathBuf::from(&ws.path);
        let slug = worktree::slugify(&name);
        let (cwd, branch, base_branch, owns) = if ws.is_git && req.worktree {
            let root = paths::worktrees_dir().join(format!("{}-{}", worktree::slugify(&ws.name), ws.id));
            let wt = worktree::create(&repo, &root, &slug, req.base.as_deref()).await?;
            (
                wt.path.to_string_lossy().to_string(),
                Some(wt.branch),
                Some(wt.base),
                true,
            )
        } else {
            let branch = if ws.is_git {
                worktree::current_head(&repo).await.ok()
            } else {
                None
            };
            (ws.path.clone(), branch, None, false)
        };
        let now = now_ms();
        let insert = || -> anyhow::Result<Channel> {
            let mut g = self.inner.lock();
            if !g.workspaces.contains_key(&ws.id) {
                anyhow::bail!("workspace {} was removed", ws.id);
            }
            let sort_order = g
                .channels
                .values()
                .filter(|c| c.workspace_id == ws.id)
                .map(|c| c.sort_order + 1)
                .max()
                .unwrap_or(0);
            let mut ch = Channel {
                id: 0,
                workspace_id: ws.id,
                name: name.clone(),
                cwd: cwd.clone(),
                branch: branch.clone(),
                base_branch: base_branch.clone(),
                owns_worktree: owns,
                port_base: 0,
                archived: false,
                layout: None,
                created_at: now,
                last_read_at: now,
                sort_order,
                permissions: req.permissions.unwrap_or_default(),
            };
            ch.id = self.store.insert_channel(&ch)?;
            // Preferred block derives from the id; if an active channel already uses it
            // (ids 1000 apart), take the next free block.
            let used: std::collections::HashSet<u16> = g
                .channels
                .values()
                .filter(|c| !c.archived)
                .map(|c| c.port_base)
                .collect();
            let pref = port_base_for(ch.id);
            ch.port_base = (0..1000u16)
                .map(|k| 20000 + ((pref - 20000) / 10 + k) % 1000 * 10)
                .find(|b| !used.contains(b))
                .unwrap_or(pref);
            self.store.update_channel(&ch)?;
            g.channels.insert(ch.id, ch.clone());
            Ok(ch)
        };
        let ch = match insert() {
            Ok(ch) => ch,
            Err(e) => {
                if owns {
                    let _ = worktree::remove(&repo, Path::new(&cwd)).await;
                }
                return Err(e);
            }
        };
        self.emit(ServerEvent::ChannelUpserted(ch.clone()));
        if let Some(kind) = req.agent {
            let spawn = SpawnSession {
                channel_id: ch.id,
                kind,
                name: None,
                cwd: None,
                prompt: req.prompt.clone().filter(|p| !p.trim().is_empty()),
                extra_args: Vec::new(),
                cols: req.cols,
                rows: req.rows,
                parent_id: None,
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

    pub fn rename_channel(&self, id: ChannelId, name: String) -> anyhow::Result<Channel> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("channel name is empty");
        }
        self.update_channel(id, true, |c| c.name = name)
    }

    /// Kills and removes the channel's sessions, marks it archived, optionally removes the
    /// worktree (the branch is kept).
    pub async fn archive_channel(self: &Arc<Self>, id: ChannelId, remove_worktree: bool) -> anyhow::Result<()> {
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
        if remove_worktree && ch.owns_worktree {
            let repo = self.workspace(ch.workspace_id).map(|w| PathBuf::from(w.path));
            if let Some(repo) = repo {
                // Give killed processes a moment to leave the directory.
                if !sids.is_empty() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                if let Err(e) = worktree::remove(&repo, Path::new(&ch.cwd)).await {
                    self.notice("warn", format!("Could not remove worktree {}: {e:#}", ch.cwd));
                }
            }
        }
        Ok(())
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
        let id = {
            let mut g = self.inner.lock();
            // Re-check: the channel may have been archived while we awaited above.
            if g.channels.get(&ch.id).is_none_or(|c| c.archived) {
                anyhow::bail!("channel {} is archived", ch.id);
            }
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
                created_at: now,
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
                    busy: true,
                    kill_requested: false,
                },
            );
            id
        };
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
                    self.deliver_initial_prompt(id, ch.id, text, consumed, req.parent_id);
                }
                Ok(self.session(id).unwrap_or(info))
            }
            Err(e) => {
                self.inner.lock().sessions.remove(&id);
                let _ = self.store.delete_session(id);
                let _ = std::fs::remove_dir_all(paths::session_dir(id));
                Err(e)
            }
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
            if let Err(e) = crate::inject::send(&hub, id, text, crate::inject::SendMode::Queue, from).await {
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
            let e = g
                .sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
            e.generation = generation;
            e.early_exit = None;
            let (info, extra) = (e.info.clone(), e.extra_args.clone());
            let ch = g.channels.get(&info.channel_id);
            let port_base = ch.map(|c| c.port_base).unwrap_or(20000);
            let permissions = ch.map(|c| c.permissions).unwrap_or_default();
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
        // Non-secret per-run marker: recognizes this run's processes when reaping leftovers.
        // (Agent CLIs may strip MUSTER_TOKEN from the commands they run; Codex does by default.)
        env.insert("MUSTER_RUN".into(), paths::random_token()[..16].to_string());
        env.insert("MUSTER_BIN".into(), self.cfg.muster_bin.to_string_lossy().to_string());
        env.insert("PORT".into(), port_base.to_string());
        env.insert("MUSTER_PORT_BASE".into(), port_base.to_string());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.extend(spec.env.clone());
        let pty = pty::spawn(
            pty::SpawnParams {
                id,
                generation,
                program: spec.program.clone(),
                args: spec.args.clone(),
                env,
                // Don't leak "you are inside another agent" markers from the daemon's own env.
                env_remove: INHERITED_SESSION_MARKERS.iter().map(|s| s.to_string()).collect(),
                cwd: PathBuf::from(&info.cwd),
                cols: info.cols,
                rows: info.rows,
                log_path: session_dir.join("output.log"),
            },
            self.weak.clone(),
        )?;
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
            e.info.title = None;
            if let Err(err) = self.store.set_session_sid(id, pty.pid, pty.markers()) {
                tracing::warn!(session = id, "persisting session id: {err}");
            }
            let early = e.early_exit.take();
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
        let r = self.restart_inner(id).await;
        if let Some(e) = self.inner.lock().sessions.get_mut(&id) {
            e.busy = false;
            // A kill that arrived while starting was applied to the new process, or the
            // start failed; either way it must not hit the next process.
            e.kill_requested = false;
        }
        r
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
        {
            let mut g = self.inner.lock();
            if let Some(e) = g.sessions.get_mut(&id)
                && let Some(p) = e.pty.take()
            {
                p.discard();
            }
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
            Some(p) => {
                p.kill();
                Ok(())
            }
            None if self.session(id).is_some() => Ok(()),
            None => anyhow::bail!("no such session: {id}"),
        }
    }

    /// Kill (if alive) and forget a session, including its scrollback.
    pub fn remove_session(&self, id: SessionId) -> anyhow::Result<()> {
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
        self.emit(ServerEvent::SessionRemoved { id });
        Ok(())
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
            if !p.has_processes()
                && let Some(hub) = weak.upgrade()
                && let Err(e) = hub.store.remove_reap(id, sid)
            {
                tracing::warn!(session = id, "clearing pending reap: {e}");
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
    pub fn resize_session(&self, id: SessionId, cols: u16, rows: u16) -> anyhow::Result<()> {
        let (cols, rows) = pty::clamp_size(cols, rows);
        let cur = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        if let Some(p) = self.pty(id) {
            p.resize(cols, rows)?;
        }
        if cur.cols != cols || cur.rows != rows {
            self.update_session_info(id, |i| {
                i.cols = cols;
                i.rows = rows;
            })?;
        }
        Ok(())
    }

    /// Handle a hook callback authenticated by session token. Routes to the adapter's
    /// `on_hook`, logs a Hook event, applies the resulting state.
    pub fn handle_hook(&self, token: &str, event: &str, payload: serde_json::Value) -> anyhow::Result<()> {
        let id = self
            .session_by_token(token)
            .ok_or_else(|| anyhow::anyhow!("unknown session token"))?;
        let info = self
            .session(id)
            .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
        let update = adapter_for(info.kind).on_hook(event, &payload);
        self.log_event(
            Some(info.channel_id),
            Some(id),
            EventKind::Hook,
            serde_json::json!({"session_id": id, "event": event, "summary": hook_summary(&payload)}),
        );
        if let Some(u) = update {
            self.set_state(id, u, StateSource::Hook);
        }
        Ok(())
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

    /// Kill all sessions (and their leftover background processes), wait briefly for them to
    /// exit, flush and fsync logs, mark the rest Exited.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let ptys: Vec<Arc<PtySession>> = self
            .all_ptys()
            .into_iter()
            .filter(|p| p.is_alive() || !p.is_drained())
            .collect();
        for p in &ptys {
            p.kill();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3500);
        while ptys.iter().any(|p| p.has_processes()) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The kill threads' SIGKILL escalation may not get to run before the process exits.
        for p in &ptys {
            if p.has_processes() {
                p.kill_now();
            }
        }
        // Give waiter threads a moment to report exits.
        tokio::time::sleep(Duration::from_millis(100)).await;
        for p in self.all_ptys() {
            p.flush_log_final();
        }
        let ids: Vec<SessionId> = self
            .inner
            .lock()
            .sessions
            .values()
            .filter(|e| e.info.state != AgentState::Exited)
            .map(|e| e.info.id)
            .collect();
        for id in ids {
            self.set_state(
                id,
                StateUpdate::with(AgentState::Exited, "daemon stopped"),
                StateSource::Daemon,
            );
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
