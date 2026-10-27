//! SQLite persistence (rusqlite, bundled). The Hub keeps everything in memory and writes
//! through to this store; the store is only read in bulk at daemon start (and for timelines).
//!
//! Lock order: callers may hold a Hub lock while calling into the store, never the reverse.

use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::*;

/// A persisted window row.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWindow {
    pub id: WindowId,
    pub name: String,
    pub last_focused_at: i64,
    pub created_at: i64,
    pub layout: Option<serde_json::Value>,
}

/// Session fields that are persisted but not part of `SessionInfo`.
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub info: SessionInfo,
    pub extra_args: Vec<String>,
}

/// Application-owned identity and credentials for a host-owned terminal run.
/// Written before spawn so a lost response can be reconciled without another process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredTerminal {
    pub host_instance: String,
    pub token: String,
    pub port_base: u16,
    pub spec: muster_terminal_protocol::SpawnSpec,
    /// Where `spec` had the OpenRouter key, stored empty (src/openrouter.rs `redact_spec`).
    #[serde(default, skip_serializing_if = "crate::openrouter::KeyRefs::is_empty")]
    pub key_refs: crate::openrouter::KeyRefs,
    /// The fingerprint of the OpenRouter key this run was launched with (never the key): the
    /// spend caps refuse unattended work for a run whose key isn't the one the backend reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fp: Option<String>,
    pub pending: bool,
    pub retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookCursor {
    pub instance: String,
    pub sequence: i64,
}

/// Hook-derived runtime facts, committed with the inbox cursor and session state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HookSession {
    pub generation: u64,
    pub subagents: u32,
    pub last_hook_at: i64,
    /// Work the run started in the background that hasn't reported back (src/sleep.rs).
    #[serde(default, skip_serializing_if = "crate::sleep::Background::is_empty")]
    pub background: crate::sleep::Background,
    /// The run's conversation (Claude: the latest reported id; Codex: its top-level thread),
    /// `Hub::conversation_after_hook`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// Codex: `conversation` is known to be the top-level thread (the run resumed it, or its
    /// SessionStart hook named it), not only the first thread that finished a turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conversation_sure: bool,
    /// Not part of the record: the session's new stored arguments, saved in the same
    /// transaction as the hook that changed them.
    #[serde(skip)]
    pub extra_args: Option<Vec<String>>,
    /// Not part of the record: a Codex tab's new saved thread (Some(None): forget it), saved
    /// in the same transaction (`CodexThread`).
    #[serde(skip)]
    pub tab_thread: Option<Option<CodexThread>>,
    /// Not part of the record: a Codex agent's new conversation (Some(None): forget it).
    #[serde(skip)]
    pub thread: Option<Option<String>>,
    /// Not part of the record: the hook came from a run Start fresh is replacing, so it changes
    /// no conversation an agent resumes.
    #[serde(skip)]
    pub fenced: bool,
    /// Not part of the record: a Codex notify from a thread that isn't the run's top-level
    /// one (a sub-agent's turn): no reply, no state.
    #[serde(skip)]
    pub child: bool,
}

/// The Codex thread a tab's next start reopens (`runtime_state` key `codex-thread:<session>`),
/// kept out of `sessions.extra_args` so older backends, whose argument check has no such flag,
/// can still restart the tab. `generation`: the run that last carried it; a start resumes it
/// only from that run, so a run an older backend started in between (a new conversation) wins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodexThread {
    pub thread: String,
    pub generation: u64,
}

fn save_codex_thread(conn: &Connection, session: SessionId, thread: Option<&CodexThread>) -> anyhow::Result<()> {
    match thread {
        Some(t) => conn.execute("INSERT INTO runtime_state(key,record) VALUES (?1,?2)
            ON CONFLICT(key) DO UPDATE SET record=excluded.record", params![codex_thread_key(session), serde_json::to_string(t)?])?,
        None => conn.execute("DELETE FROM runtime_state WHERE key=?1", [codex_thread_key(session)])?,
    };
    Ok(())
}

fn confirm_codex_agent_thread(conn: &Connection, agent: i64, thread: Option<&str>) -> anyhow::Result<()> {
    match thread {
        Some(t) => conn.execute("INSERT INTO runtime_state(key,record) VALUES (?1,?2)
            ON CONFLICT(key) DO UPDATE SET record=excluded.record", params![codex_agent_thread_key(agent), serde_json::to_string(t)?])?,
        None => conn.execute("DELETE FROM runtime_state WHERE key=?1", [codex_agent_thread_key(agent)])?,
    };
    Ok(())
}

pub fn codex_thread_key(session: SessionId) -> String {
    format!("codex-thread:{session}")
}

/// A Codex agent's `resume_id` as this backend saved it (`runtime_state` key
/// `codex-agent-thread:<agent>`). Older backends saved every notify's thread (Codex's title
/// requests and sub-agents' included) without it: a record that doesn't match is not resumed.
pub fn codex_agent_thread_key(agent: i64) -> String {
    format!("codex-agent-thread:{agent}")
}

/// Which daily cap refused a message (`insert_agent_message_capped`), with the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapHit {
    /// Messages of any origin.
    All(usize),
    /// Unattended messages (any origin but the human's).
    Unattended(usize),
}

#[derive(Clone)]
pub struct Store {
    conn: std::sync::Arc<Mutex<Connection>>,
    directory: std::path::PathBuf,
}

/// Reserve sequence ranges durably before publishing any values. Unused numbers are
/// skipped after a backend crash, so an old client's approval cannot name a new prompt.
pub struct StateSequences {
    store: Store,
    available: Mutex<std::ops::Range<u64>>,
}

impl StateSequences {
    pub fn new(store: &Store) -> Self {
        Self { store: store.clone(), available: Mutex::new(0..0) }
    }

    pub fn next(&self) -> anyhow::Result<u64> {
        let mut range = self.available.lock();
        if range.is_empty() {
            let mut conn = self.store.conn.lock();
            let transaction = conn.transaction()?;
            let previous: Option<String> = transaction.query_row(
                "SELECT record FROM runtime_state WHERE key='state-sequence-end'", [], |row| row.get(0),
            ).optional()?;
            // Start above all values used by the pre-host backend.
            let start = previous.map(|text| text.parse::<u64>()).transpose()?.unwrap_or(1 << 32);
            let end = start.checked_add(4096).context("state sequence exhausted")?;
            anyhow::ensure!(end <= (1 << 53) - 1, "state sequence exceeds exact JavaScript integers");
            transaction.execute(
                "INSERT INTO runtime_state(key, record) VALUES ('state-sequence-end', ?1)
                 ON CONFLICT(key) DO UPDATE SET record=excluded.record", [end.to_string()],
            )?;
            transaction.commit()?;
            *range = start..end;
        }
        Ok(range.next().expect("reserved nonempty sequence range"))
    }
}

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE workspaces (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        path TEXT NOT NULL UNIQUE,
        is_git INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE channels (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        workspace_id INTEGER NOT NULL,
        name TEXT NOT NULL,
        cwd TEXT NOT NULL,
        branch TEXT,
        base_branch TEXT,
        owns_worktree INTEGER NOT NULL,
        port_base INTEGER NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0,
        layout TEXT,
        created_at INTEGER NOT NULL,
        last_read_at INTEGER NOT NULL,
        sort_order INTEGER NOT NULL
    );
    CREATE TABLE sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        channel_id INTEGER NOT NULL,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        cwd TEXT NOT NULL,
        state TEXT NOT NULL,
        state_detail TEXT,
        state_since INTEGER NOT NULL,
        title TEXT,
        parent_id INTEGER,
        pid INTEGER,
        exit_code INTEGER,
        cols INTEGER NOT NULL,
        rows INTEGER NOT NULL,
        queued INTEGER NOT NULL DEFAULT 0,
        extra_args TEXT NOT NULL DEFAULT '[]',
        created_at INTEGER NOT NULL
    );
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        channel_id INTEGER,
        session_id INTEGER,
        kind TEXT NOT NULL,
        data TEXT NOT NULL,
        ts INTEGER NOT NULL
    );
    CREATE INDEX events_channel ON events(channel_id, id);
    "#,
    // v2: session id of the last process (to reap leftovers after a restart), event retention
    // (index on ts), hook events keep a summary only.
    r#"
    ALTER TABLE sessions ADD COLUMN sid INTEGER;
    ALTER TABLE sessions ADD COLUMN run_marker TEXT;
    UPDATE sessions SET sid = pid WHERE pid IS NOT NULL;
    CREATE INDEX events_ts ON events(ts);
    CREATE INDEX events_session ON events(session_id);
    UPDATE events SET data = json_object(
        'session_id', json_extract(data, '$.session_id'),
        'event', json_extract(data, '$.event'),
        'summary', json_object())
    WHERE kind = 'hook' AND json_valid(data);
    "#,
    // v3: per-channel agent permission profile (JSON `Permissions`; '{}' = the defaults).
    r#"
    ALTER TABLE channels ADD COLUMN permissions TEXT NOT NULL DEFAULT '{}';
    CREATE TABLE reap (
        session_id INTEGER NOT NULL,
        sid INTEGER NOT NULL,
        run_marker TEXT NOT NULL,
        PRIMARY KEY (session_id, sid)
    );
    "#,
    // v4: client windows (a channel is a tab in one window), auto-named channels. Existing
    // live channels go into window 1 in their sort order (renumbered 0..), keeping their names.
    r#"
    CREATE TABLE windows (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        last_focused_at INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL
    );
    ALTER TABLE channels ADD COLUMN window_id INTEGER;
    ALTER TABLE channels ADD COLUMN auto_named INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE channels ADD COLUMN first_prompt TEXT;
    INSERT INTO windows (id, name, last_focused_at, created_at)
        VALUES (1, '1', 0, CAST(strftime('%s', 'now') AS INTEGER) * 1000);
    UPDATE channels SET window_id = 1,
        sort_order = (SELECT r.rn FROM (
            SELECT id, ROW_NUMBER() OVER (ORDER BY sort_order, id) - 1 AS rn
            FROM channels WHERE archived = 0) r WHERE r.id = channels.id)
    WHERE archived = 0;
    "#,
    // v5: groups layout. A tab is a session, so each session belongs to a window (at first its
    // channel's), and a window keeps its split layout (opaque JSON from the frontend).
    r#"
    ALTER TABLE windows ADD COLUMN layout TEXT;
    ALTER TABLE sessions ADD COLUMN window_id INTEGER;
    UPDATE sessions SET window_id = (SELECT c.window_id FROM channels c WHERE c.id = sessions.channel_id);
    "#,
    // v6: paired remote devices (docs/ANDROID.md) and the origins their pages are served from.
    r#"
    CREATE TABLE devices (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        scopes TEXT NOT NULL,
        ts_login TEXT,
        created_at INTEGER NOT NULL,
        last_seen_at INTEGER NOT NULL,
        last_source TEXT,
        expires_at INTEGER NOT NULL,
        revoked_at INTEGER
    );
    CREATE TABLE remote_origins (
        origin TEXT PRIMARY KEY,
        added_at INTEGER NOT NULL
    );
    "#,
    // v7: bots (PLAN.md §10) and the messages muster delivered to them (origin rule, hop
    // limit, daily cap, replies). Messages live in their own table rather than the event log:
    // retention never prunes them, and the daily cap and reply lookups are indexed queries.
    r#"
    CREATE TABLE bots (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE,
        kind TEXT NOT NULL,
        model TEXT,
        effort TEXT,
        persona TEXT NOT NULL DEFAULT '',
        home_dir TEXT NOT NULL,
        permissions TEXT NOT NULL DEFAULT '{}',
        accepts_origins TEXT NOT NULL DEFAULT '["human"]',
        session_id INTEGER,
        resume_id TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE bot_messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        from_label TEXT NOT NULL,
        from_session INTEGER,
        to_bot INTEGER NOT NULL,
        to_session INTEGER,
        origin TEXT NOT NULL,
        hop INTEGER NOT NULL,
        reply_to INTEGER,
        created_at INTEGER NOT NULL,
        reply TEXT,
        replied_at INTEGER
    );
    CREATE INDEX bot_messages_to ON bot_messages(to_bot, created_at);
    "#,
    // v8: each bot's own tab (never one found by name, which could have a wider profile).
    r#"
    ALTER TABLE bots ADD COLUMN channel_id INTEGER;
    "#,
    // v9: bots are now "agents" (tables, columns and index renamed), with an autostart flag;
    // messages keep their text and, for a refused one, the reason (the agents panel's log).
    // Home dirs and the agents workspace move from `<data>/bots` to `<data>/agents` at daemon
    // start, together with the folders (`agents::relocate_homes`), so a record never points at a
    // folder that didn't move.
    r#"
    ALTER TABLE bots RENAME TO agents;
    ALTER TABLE bot_messages RENAME TO agent_messages;
    ALTER TABLE agent_messages RENAME COLUMN to_bot TO to_agent;
    DROP INDEX IF EXISTS bot_messages_to;
    CREATE INDEX agent_messages_to ON agent_messages(to_agent, created_at);
    ALTER TABLE agents ADD COLUMN autostart INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE agent_messages ADD COLUMN text TEXT;
    ALTER TABLE agent_messages ADD COLUMN refused TEXT;
    "#,
    // v10: application metadata for terminal runs owned by the independent host.
    r#"
    CREATE TABLE terminal_runs (
        session_id INTEGER PRIMARY KEY,
        record TEXT NOT NULL
    );
    "#,
    // v11: application runtime state that must survive backend replacement.
    r#"
    CREATE TABLE runtime_state (
        key TEXT PRIMARY KEY,
        record TEXT NOT NULL
    );
    "#,
    // v12: record MCP-created channels atomically with their creation.
    "CREATE TABLE mcp_channels(channel_id INTEGER PRIMARY KEY);",
    // v13: local Claude configuration references, never provider credentials.
    r#"
    CREATE TABLE account_profiles (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL COLLATE NOCASE UNIQUE,
        config_dir TEXT NOT NULL UNIQUE,
        enabled INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL
    );
    ALTER TABLE workspaces ADD COLUMN account_profile_id INTEGER;
    ALTER TABLE sessions ADD COLUMN account_profile_id INTEGER;
    ALTER TABLE agents ADD COLUMN account_profile_id INTEGER;
    "#,
    // v14: agent panels and their durable, bounded return channel.
    r#"
    CREATE TABLE panels (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        channel_id INTEGER NOT NULL,
        session_id INTEGER NOT NULL,
        title TEXT NOT NULL,
        app TEXT NOT NULL,
        csp TEXT NOT NULL,
        revision INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        closed INTEGER NOT NULL DEFAULT 0,
        record TEXT NOT NULL
    );
    CREATE INDEX panels_channel ON panels(channel_id);
    CREATE TABLE panel_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        panel_id INTEGER NOT NULL,
        record TEXT NOT NULL
    );
    CREATE INDEX panel_events_panel ON panel_events(panel_id, id);
    "#,
    // v15: events shown to an agent only as a placeholder stay fetchable by id (`panel_event`).
    r#"
    ALTER TABLE panel_events ADD COLUMN parked INTEGER NOT NULL DEFAULT 0;
    "#,
    // v16: a named agent's tab the human closed keeps running, hidden (`session.hide`).
    r#"
    ALTER TABLE sessions ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
    "#,
    // v17: plain-text notes shared by the human and agents (src/notes.rs).
    r#"
    CREATE TABLE notes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        rev INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        updated_by TEXT NOT NULL,
        window_id INTEGER,
        beside INTEGER,
        version INTEGER NOT NULL DEFAULT 1
    );
    "#,
    // v18: where an agent's model runs (src/openrouter.rs), and what each session's run bills
    // to; existing ones keep the subscription. Never a key: that stays in the daemon's memory.
    r#"
    ALTER TABLE agents ADD COLUMN provider TEXT NOT NULL DEFAULT 'subscription';
    ALTER TABLE sessions ADD COLUMN provider TEXT NOT NULL DEFAULT 'subscription';
    "#,
    // v19: schedules (src/schedules/): each one's settings and timing as a JSON record, and one
    // row per run. A run's text and reply stay in agent_messages (`msg_id`); the row holds when,
    // how it went and why. A slot (the planned time) is claimed at most once per schedule:
    // the unique index is what keeps a schedule from firing twice for one time.
    r#"
    CREATE TABLE schedules (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        agent_id INTEGER NOT NULL,
        record TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX schedules_agent ON schedules(agent_id);
    CREATE TABLE schedule_runs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        schedule_id INTEGER NOT NULL,
        slot INTEGER NOT NULL,
        kind TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        msg_id INTEGER,
        outcome TEXT NOT NULL,
        reason TEXT,
        finished_at INTEGER,
        noted INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule_id, id);
    CREATE INDEX schedule_runs_msg ON schedule_runs(msg_id);
    CREATE UNIQUE INDEX schedule_runs_slot ON schedule_runs(schedule_id, slot) WHERE kind != 'manual';
    "#,
    // v20: cold prompt cache (src/cache.rs): when each agent session last talked to its model,
    // and, for a message Muster typed into an agent whose cache had likely expired, how long
    // that agent had been idle (NULL: it was warm, or from before this was kept).
    r#"
    ALTER TABLE sessions ADD COLUMN last_turn_at INTEGER;
    ALTER TABLE agent_messages ADD COLUMN cold_idle_ms INTEGER;
    "#,
    // v21: scheduling Phase 2 (src/schedules/). Runs started by an event (a note, a branch,
    // GitHub, a trigger URL) and the "Then…" steps of a chain are runs of their schedule too:
    // a step names the run it follows (`parent_run`), its place in the chain (`step`: `s`/`f`
    // per level, success or failure) and its own agent; a triggered run keeps what happened
    // (`event`, JSON). Only timed slots (scheduled, catch-up) are unique per schedule. A note
    // fires triggers only once the human switched `triggers` on for it (never an agent).
    r#"
    ALTER TABLE schedule_runs ADD COLUMN parent_run INTEGER;
    ALTER TABLE schedule_runs ADD COLUMN step TEXT;
    ALTER TABLE schedule_runs ADD COLUMN agent_id INTEGER;
    ALTER TABLE schedule_runs ADD COLUMN event TEXT;
    DROP INDEX schedule_runs_slot;
    CREATE UNIQUE INDEX schedule_runs_slot ON schedule_runs(schedule_id, slot) WHERE kind IN ('scheduled', 'catch_up');
    ALTER TABLE notes ADD COLUMN triggers INTEGER NOT NULL DEFAULT 0;
    "#,
    // v22: Claude usage windows reported to the status line (src/usage.rs). One row per window
    // per profile key the reporting session was pinned to ('default' or 'profile:<id>'); what
    // each session's latest run last reported (to tell fresh data from a redraw); and which
    // profiles the human declared to be one account. Numbers and times only, never credentials.
    r#"
    CREATE TABLE usage_observations (
        account TEXT NOT NULL,
        window TEXT NOT NULL,
        record TEXT NOT NULL,
        PRIMARY KEY (account, window)
    );
    CREATE TABLE usage_runs (
        session_id INTEGER PRIMARY KEY,
        record TEXT NOT NULL
    );
    ALTER TABLE account_profiles ADD COLUMN account_group TEXT;
    "#,
    // v23: phone alerts (src/push.rs): one Web Push subscription per paired device, with how its
    // last delivery went. Foreign keys are off, so revoking a device deletes its row explicitly.
    r#"
    CREATE TABLE push_subscriptions (
        device_id INTEGER PRIMARY KEY,
        endpoint TEXT NOT NULL,
        p256dh TEXT NOT NULL,
        auth TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_ok_at INTEGER,
        last_error TEXT,
        last_error_at INTEGER
    );
    "#,
    // v24: sleep and wake for named agents (src/sleep.rs): how long an agent may sit idle before
    // Muster stops its process (NULL: never), and since when it is asleep (NULL: awake). An
    // asleep agent keeps its tab and record; the next message resumes its conversation. While
    // asleep, `autostart` reads 0 and `asleep_autostart` holds the setting, so an older backend
    // (which knows nothing of sleep) never starts it at its own start; `asleep_generation` is
    // the run the sleep was stopping, so a restart that cut the stop off finishes it.
    r#"
    ALTER TABLE agents ADD COLUMN sleep_after_min INTEGER;
    ALTER TABLE agents ADD COLUMN asleep_at INTEGER;
    ALTER TABLE agents ADD COLUMN asleep_autostart INTEGER;
    ALTER TABLE agents ADD COLUMN asleep_generation INTEGER;
    "#,
];

/// The first non-blank line of `head`, trimmed, at most 120 characters (a note's preview).
pub fn preview(head: &str) -> String {
    head.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect()
}

/// Panel events moving with a prompt-queue change (`Store::save_prompt_queues`).
#[derive(Debug, Default)]
pub struct PanelEventMoves {
    pub claim: Vec<PanelEvent>,
    pub release: Vec<PanelEvent>,
    pub park: Vec<PanelEvent>,
}

/// Remove exactly these queued events; each must still be there, so two consumers can never
/// both take one. The overflow notice (id 0) resets its panel's dropped count.
fn apply_panel_claims(tx: &rusqlite::Transaction, events: &[PanelEvent]) -> anyhow::Result<()> {
    for event in events {
        if event.id == 0 {
            tx.execute("UPDATE panels SET record=json_set(record,'$.dropped_events',0) WHERE id=?1", [event.panel_id])?;
            continue;
        }
        let n = tx.execute("DELETE FROM panel_events WHERE id=?1 AND panel_id=?2", params![event.id, event.panel_id])?;
        anyhow::ensure!(n == 1, "panel event {} is no longer queued", event.id);
    }
    Ok(())
}

/// Event kinds retention may delete: high-volume machine noise. Notes, prompts, agent posts,
/// approvals, session start/exit and system notices are the channel's history and are kept.
pub const NOISE_EVENT_KINDS: &[EventKind] = &[EventKind::Hook, EventKind::StateChange, EventKind::McpCall];

fn noise_kinds_sql() -> String {
    NOISE_EVENT_KINDS
        .iter()
        .map(|k| format!("'{}'", enum_to_str(k)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn enum_to_str<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// `row_agent_message`'s columns (the recipient's name, while the agent exists).
const AGENT_MESSAGE_SELECT: &str = "SELECT m.id, m.from_label, m.from_session, m.to_agent, m.to_session, m.origin,
    m.hop, m.reply_to, m.created_at, m.reply, m.replied_at, m.text, m.refused, a.name, m.cold_idle_ms
    FROM agent_messages m LEFT JOIN agents a ON a.id = m.to_agent";

fn perms_json(p: &Permissions) -> String {
    serde_json::to_string(p).unwrap_or_else(|_| "{}".into())
}

fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        for suffix in ["", "-wal", "-shm"] {
            crate::paths::make_private_file(std::path::Path::new(&format!("{}{suffix}", path.display())));
        }
        let store = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: path.parent().unwrap_or(Path::new(".")).to_path_buf() };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_memory() -> anyhow::Result<Self> {
        let store = Store {
            conn: std::sync::Arc::new(Mutex::new(Connection::open_in_memory()?)),
            directory: std::env::temp_dir().join(format!("muster-panel-unit-{}-{:x}", std::process::id(), rand::random::<u64>())),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let v = i as i64 + 1;
            if v <= version {
                continue;
            }
            let tx = conn.transaction()?;
            tx.execute_batch(sql).with_context(|| format!("migration v{v}"))?;
            tx.pragma_update(None, "user_version", v)?;
            tx.commit()?;
        }
        Ok(())
    }

    // ----- devices -----

    pub fn panel_dir(&self, id: PanelId) -> std::path::PathBuf { self.directory.join("panels").join(id.to_string()) }

    pub fn create_panel(&self, panel: &mut Panel, write_files: impl FnOnce(&Panel) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO panels(channel_id,session_id,title,app,csp,revision,created_at,updated_at,closed,record)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,'{}')",
            params![panel.channel_id, panel.session_id, panel.title, panel.app, serde_json::to_string(&panel.csp)?, panel.revision, panel.created_at, panel.updated_at])?;
        panel.id = tx.last_insert_rowid();
        write_files(panel)?;
        tx.execute("UPDATE panels SET record=?2 WHERE id=?1", params![panel.id, serde_json::to_string(panel)?])?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_panel(&self, panel: &Panel) -> anyhow::Result<()> {
        self.conn.lock().execute("UPDATE panels SET title=?2,csp=?3,revision=?4,updated_at=?5,closed=?6,record=?7 WHERE id=?1",
            params![panel.id, panel.title, serde_json::to_string(&panel.csp)?, panel.revision, panel.updated_at, panel.closed, serde_json::to_string(panel)?])?;
        Ok(())
    }

    pub fn load_panels(&self) -> anyhow::Result<Vec<Panel>> {
        let conn = self.conn.lock();
        let mut q = conn.prepare("SELECT record FROM panels ORDER BY id")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }

    /// Keep at most 200 events. The overflow count is delivered with the next batch.
    /// [`Self::save_panel`] and [`Self::push_panel_event`] in one transaction: a web preview's
    /// new address and the `navigated` event telling its agent commit together or not at all.
    pub fn save_panel_with_event(&self, panel: &mut Panel, event: &PanelEvent) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO panel_events(panel_id,record) VALUES (?1,?2)", params![panel.id, serde_json::to_string(event)?])?;
        let dropped = tx.execute("DELETE FROM panel_events WHERE panel_id=?1 AND id NOT IN
            (SELECT id FROM panel_events WHERE panel_id=?1 ORDER BY id DESC LIMIT 200)", [panel.id])?;
        panel.dropped_events += dropped as u64;
        tx.execute("UPDATE panels SET title=?2,csp=?3,revision=?4,updated_at=?5,closed=?6,record=?7 WHERE id=?1",
            params![panel.id, panel.title, serde_json::to_string(&panel.csp)?, panel.revision, panel.updated_at, panel.closed, serde_json::to_string(panel)?])?;
        tx.commit()?;
        Ok(())
    }

    pub fn push_panel_event(&self, panel: &mut Panel, event: &PanelEvent) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO panel_events(panel_id,record) VALUES (?1,?2)", params![panel.id, serde_json::to_string(event)?])?;
        let dropped = tx.execute("DELETE FROM panel_events WHERE panel_id=?1 AND id NOT IN
            (SELECT id FROM panel_events WHERE panel_id=?1 ORDER BY id DESC LIMIT 200)", [panel.id])?;
        panel.dropped_events += dropped as u64;
        tx.execute("UPDATE panels SET closed=?2,updated_at=?3,record=?4 WHERE id=?1",
            params![panel.id, panel.closed, panel.updated_at, serde_json::to_string(panel)?])?;
        tx.commit()?;
        Ok(())
    }

    /// The panel's queued events in order, each with its stored id (parked events, already
    /// announced by a placeholder, are left out). An `overflow` notice (id 0) leads when events
    /// were dropped; the latest app context rides on the last event.
    pub fn panel_events(&self, panel: &Panel) -> anyhow::Result<Vec<PanelEvent>> {
        let conn = self.conn.lock();
        let mut q = conn.prepare("SELECT id, record FROM panel_events WHERE panel_id=?1 AND parked=0 ORDER BY id")?;
        let rows = q.query_map([panel.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut events: Vec<PanelEvent> = rows.map(|r| {
            let (id, record) = r?;
            let mut event: PanelEvent = serde_json::from_str(&record)?;
            event.id = id;
            Ok(event)
        }).collect::<anyhow::Result<_>>()?;
        if panel.dropped_events > 0 {
            events.insert(0, PanelEvent { id: 0, panel_id: panel.id, kind: "overflow".into(), payload: serde_json::json!({"dropped": panel.dropped_events}), at: now_ms(), context: None });
        }
        if let Some(last) = events.last_mut() { last.context = panel.context.clone(); }
        Ok(events)
    }

    /// One stored event of `panel` by id, queued or parked: `(event, parked)`.
    pub fn panel_event(&self, panel: PanelId, id: i64) -> anyhow::Result<Option<(PanelEvent, bool)>> {
        let row: Option<(String, bool)> = self.conn.lock().query_row(
            "SELECT record, parked FROM panel_events WHERE panel_id=?1 AND id=?2", params![panel, id],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(record, parked)| {
            let mut event: PanelEvent = serde_json::from_str(&record)?;
            event.id = id;
            Ok((event, parked))
        }).transpose()
    }

    /// Acknowledge exactly these events (a wait returned them, or `panel_event` fetched a
    /// parked one). By id, so events that arrived or returned meanwhile stay queued.
    pub fn ack_panel_events(&self, events: &[PanelEvent]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        apply_panel_claims(&tx, events)?;
        tx.commit()?;
        Ok(())
    }

    /// Save the prompt queues together with the panel event moves their change implies, in one
    /// transaction: a batch admitted to a queue leaves its panels' queues (`claim`), one dropped
    /// before typing returns to them (`release`), and placeholder events of a typed batch stay
    /// fetchable (`park`).
    pub fn save_prompt_queues(&self, queues: &impl serde::Serialize, moves: &PanelEventMoves) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO runtime_state(key,record) VALUES ('prompt-queues',?1)
            ON CONFLICT(key) DO UPDATE SET record=excluded.record", [serde_json::to_string(queues)?])?;
        apply_panel_claims(&tx, &moves.claim)?;
        for (events, parked) in [(&moves.release, false), (&moves.park, true)] {
            for event in events.iter() {
                if event.id == 0 {
                    if !parked {
                        // The overflow count goes back on the panel. A concurrent save of an older
                        // copy of the record can lose this count (a notice only, never an event).
                        tx.execute("UPDATE panels SET record=json_set(record,'$.dropped_events',
                            json_extract(record,'$.dropped_events') + ?2) WHERE id=?1",
                            params![event.panel_id, event.payload["dropped"].as_u64().unwrap_or(0) as i64])?;
                    }
                    continue;
                }
                let mut stored = event.clone();
                stored.context = None;
                // A panel archived meanwhile takes its events with it.
                tx.execute("INSERT OR IGNORE INTO panel_events(id,panel_id,record,parked)
                    SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM panels WHERE id=?2)",
                    params![event.id, event.panel_id, serde_json::to_string(&stored)?, parked])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- notes -----

    fn note_row(r: &Row) -> rusqlite::Result<Note> {
        let by: String = r.get(6)?;
        Ok(Note { id: r.get(0)?, title: r.get(1)?, body: r.get(2)?, rev: r.get(3)?, created_at: r.get(4)?,
            updated_at: r.get(5)?, updated_by: serde_json::from_str(&by).unwrap_or_else(|_| NoteAuthor::human()),
            window_id: r.get(7)?, beside: r.get(8)?, version: r.get(9)?, triggers: r.get::<_, i64>(10)? != 0 })
    }

    /// Every note without its text: sizes and a preview are worked out by SQLite, so only the
    /// first 240 characters of each text leave the database.
    pub fn load_note_summaries(&self) -> anyhow::Result<Vec<NoteSummary>> {
        let conn = self.conn.lock();
        let mut q = conn.prepare("SELECT id,title,rev,version,created_at,updated_at,updated_by,window_id,beside,
            length(CAST(body AS BLOB)),
            CASE WHEN body = '' THEN 0 ELSE length(body) - length(replace(body, char(10), '')) + (substr(body, -1) != char(10)) END,
            substr(body, 1, 240), triggers
            FROM notes ORDER BY id")?;
        let rows = q.query_map([], |r| {
            let by: String = r.get(6)?;
            let head: String = r.get(11)?;
            let window_id: Option<WindowId> = r.get(7)?;
            Ok(NoteSummary { id: r.get(0)?, title: r.get(1)?, rev: r.get(2)?, version: r.get(3)?, created_at: r.get(4)?,
                updated_at: r.get(5)?, updated_by: serde_json::from_str(&by).unwrap_or_else(|_| NoteAuthor::human()),
                window_id, beside: r.get(8)?, open: window_id.is_some(), bytes: r.get(9)?, lines: r.get(10)?,
                preview: preview(&head), triggers: r.get::<_, i64>(12)? != 0 })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Notes whose text holds `query` (ASCII case-insensitive), at most `limit`, each with up to
    /// 400 characters of text around the first match (never a whole text).
    pub fn search_notes(&self, query: &str, limit: usize) -> anyhow::Result<Vec<(NoteId, String)>> {
        let conn = self.conn.lock();
        let mut q = conn.prepare("SELECT id, substr(body, max(1, instr(lower(body), lower(?1)) - 200), 400 + length(?1))
            FROM notes WHERE instr(lower(body), lower(?1)) > 0 ORDER BY updated_at DESC LIMIT ?2")?;
        let rows = q.query_map(params![query, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn load_note(&self, id: NoteId) -> anyhow::Result<Option<Note>> {
        Ok(self.conn.lock().query_row(
            "SELECT id,title,body,rev,created_at,updated_at,updated_by,window_id,beside,version,triggers FROM notes WHERE id=?1",
            [id], Self::note_row).optional()?)
    }

    pub fn count_notes(&self) -> anyhow::Result<usize> {
        Ok(self.conn.lock().query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))? as usize)
    }

    /// Insert `note`; its id is assigned here.
    pub fn create_note(&self, note: &mut Note) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute("INSERT INTO notes(title,body,rev,created_at,updated_at,updated_by,window_id,beside,version) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![note.title, note.body, note.rev, note.created_at, note.updated_at,
                serde_json::to_string(&note.updated_by)?, note.window_id, note.beside, note.version])?;
        note.id = conn.last_insert_rowid();
        Ok(())
    }

    pub fn save_note(&self, note: &Note) -> anyhow::Result<()> {
        let n = self.conn.lock().execute(
            "UPDATE notes SET title=?2,body=?3,rev=?4,updated_at=?5,updated_by=?6,window_id=?7,beside=?8,version=?9 WHERE id=?1",
            params![note.id, note.title, note.body, note.rev, note.updated_at,
                serde_json::to_string(&note.updated_by)?, note.window_id, note.beside, note.version])?;
        anyhow::ensure!(n == 1, "note {} not found", note.id);
        Ok(())
    }

    /// "Allow triggers" on a note: the only write of that column (`save_note` never touches it,
    /// so no other change, an agent's included, can switch it).
    pub fn set_note_triggers(&self, id: NoteId, on: bool) -> anyhow::Result<()> {
        let n = self.conn.lock().execute("UPDATE notes SET triggers=?2, version=version+1 WHERE id=?1", params![id, on as i64])?;
        anyhow::ensure!(n == 1, "note {id} not found");
        Ok(())
    }

    pub fn delete_note(&self, id: NoteId) -> anyhow::Result<bool> {
        Ok(self.conn.lock().execute("DELETE FROM notes WHERE id=?1", [id])? == 1)
    }

    pub fn delete_panel(&self, id: PanelId) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM panel_events WHERE panel_id=?1", [id])?;
        tx.execute("DELETE FROM panels WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Does the panel keep events a placeholder announced, for `panel_event`?
    pub fn panel_has_parked_events(&self, panel: PanelId) -> anyhow::Result<bool> {
        Ok(self.conn.lock().query_row("SELECT EXISTS (SELECT 1 FROM panel_events WHERE panel_id=?1 AND parked=1)",
            [panel], |r| r.get(0))?)
    }

    /// Closed panels with nothing left to deliver or fetch.
    pub fn idle_closed_panels(&self) -> anyhow::Result<Vec<PanelId>> {
        let conn = self.conn.lock();
        let mut q = conn.prepare("SELECT id FROM panels WHERE closed=1
            AND NOT EXISTS (SELECT 1 FROM panel_events e WHERE e.panel_id=panels.id)
            AND COALESCE(json_extract(record,'$.dropped_events'),0)=0")?;
        q.query_map([], |r| r.get(0))?.map(|r| Ok(r?)).collect()
    }

    pub fn panels_root(&self) -> std::path::PathBuf { self.directory.join("panels") }

    pub fn delete_channel_panels(&self, channel: ChannelId) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM panel_events WHERE panel_id IN (SELECT id FROM panels WHERE channel_id=?1)", [channel])?;
        tx.execute("DELETE FROM panels WHERE channel_id=?1", [channel])?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_devices(&self) -> anyhow::Result<Vec<crate::devices::Device>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, name, token_hash, scopes, ts_login, created_at, last_seen_at, last_source, expires_at,
                    revoked_at FROM devices ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            let scopes: String = r.get(3)?;
            Ok(crate::devices::Device {
                id: r.get(0)?,
                name: r.get(1)?,
                token_hash: r.get(2)?,
                scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                ts_login: r.get(4)?,
                created_at: r.get(5)?,
                last_seen_at: r.get(6)?,
                last_source: r.get(7)?,
                expires_at: r.get(8)?,
                revoked_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn insert_device(&self, d: &crate::devices::Device) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO devices (name, token_hash, scopes, ts_login, created_at, last_seen_at, last_source,
                                  expires_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                d.name,
                d.token_hash,
                serde_json::to_string(&d.scopes)?,
                d.ts_login,
                d.created_at,
                d.last_seen_at,
                d.last_source,
                d.expires_at,
                d.revoked_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Persist the mutable fields (last seen, expiry, revocation).
    pub fn update_device(&self, d: &crate::devices::Device) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE devices SET last_seen_at = ?2, last_source = ?3, expires_at = ?4, revoked_at = ?5 WHERE id = ?1",
            params![d.id, d.last_seen_at, d.last_source, d.expires_at, d.revoked_at],
        )?;
        Ok(())
    }

    pub fn load_remote_origins(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT origin FROM remote_origins")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn insert_remote_origin(&self, origin: &str, now: i64) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO remote_origins (origin, added_at) VALUES (?1, ?2)",
            params![origin, now],
        )?;
        Ok(())
    }

    pub fn load_push_subscriptions(&self) -> anyhow::Result<Vec<(i64, crate::push::Subscription)>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT device_id, endpoint, p256dh, auth, created_at, last_ok_at, last_error, last_error_at
             FROM push_subscriptions",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get(0)?,
                crate::push::Subscription {
                    endpoint: r.get(1)?,
                    p256dh: r.get(2)?,
                    auth: r.get(3)?,
                    created_at: r.get(4)?,
                    last_ok_at: r.get(5)?,
                    last_error: r.get(6)?,
                    last_error_at: r.get(7)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Insert or replace a device's subscription (with its delivery status).
    pub fn put_push_subscription(&self, device_id: i64, s: &crate::push::Subscription) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO push_subscriptions
                 (device_id, endpoint, p256dh, auth, created_at, last_ok_at, last_error, last_error_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![device_id, s.endpoint, s.p256dh, s.auth, s.created_at, s.last_ok_at, s.last_error, s.last_error_at],
        )?;
        Ok(())
    }

    pub fn delete_push_subscription(&self, device_id: i64) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM push_subscriptions WHERE device_id = ?1", [device_id])?;
        Ok(())
    }

    // ----- agents -----

    pub fn load_agents(&self) -> anyhow::Result<Vec<crate::agents::Agent>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, name, kind, model, effort, persona, home_dir, permissions, accepts_origins, session_id,
                    resume_id, created_at, channel_id, autostart, account_profile_id, provider, sleep_after_min, asleep_at,
                    asleep_autostart
             FROM agents ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            let kind: String = r.get(2)?;
            let perms: String = r.get(7)?;
            let accepts: String = r.get(8)?;
            Ok(crate::agents::Agent {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: SessionKind::parse(&kind).unwrap_or(SessionKind::Codex),
                model: r.get(3)?,
                effort: r.get(4)?,
                persona: r.get(5)?,
                home_dir: r.get(6)?,
                permissions: serde_json::from_str(&perms).unwrap_or_default(),
                // Unreadable: the strictest set (human only).
                accepts_origins: serde_json::from_str(&accepts).unwrap_or_else(|_| vec![crate::agents::Origin::Human]),
                session_id: r.get::<_, Option<i64>>(9)?.map(|v| v as SessionId),
                resume_id: r.get(10)?,
                created_at: r.get(11)?,
                channel_id: r.get(12)?,
                // While asleep the setting is kept aside (migration v24); an older backend may have
                // turned it on meanwhile.
                autostart: r.get::<_, i64>(13)? != 0
                    || r.get::<_, Option<i64>>(17)?.is_some() && r.get::<_, Option<i64>>(18)?.is_some_and(|v| v != 0),
                account_profile_id: r.get(14)?,
                provider: crate::openrouter::Provider::parse(&r.get::<_, String>(15)?),
                // Out of range (written by hand): the nearest allowed value.
                sleep_after_min: r.get::<_, Option<i64>>(16)?.map(crate::sleep::clamp_after),
                asleep_at: r.get(17)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts the agent (its `id` is ignored) and returns the new id. A name taken
    /// (case-insensitively) is an error.
    pub fn insert_agent(&self, b: &crate::agents::Agent) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO agents (name, kind, model, effort, persona, home_dir, permissions, accepts_origins,
                               session_id, resume_id, created_at, channel_id, autostart, account_profile_id, provider,
                               sleep_after_min, asleep_at, asleep_autostart)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                b.name,
                b.kind.as_str(),
                b.model,
                b.effort,
                b.persona,
                b.home_dir,
                perms_json(&b.permissions),
                serde_json::to_string(&b.accepts_origins)?,
                b.session_id,
                b.resume_id,
                b.created_at,
                b.channel_id,
                (b.autostart && b.asleep_at.is_none()) as i64,
                b.account_profile_id,
                b.provider.as_str(),
                b.sleep_after_min,
                b.asleep_at,
                b.asleep_at.map(|_| b.autostart as i64)
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The fields that change after creation: the agent's tab, session and conversation id.
    pub fn update_agent_runtime(
        &self,
        id: i64,
        channel_id: Option<ChannelId>,
        session_id: Option<SessionId>,
        resume_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE agents SET channel_id = ?2, session_id = ?3, resume_id = ?4 WHERE id = ?1",
            params![id, channel_id, session_id, resume_id],
        )?;
        // Only a hook (`commit_hook`) sets a Codex agent's thread; anything else forgets it.
        if resume_id.is_none() {
            confirm_codex_agent_thread(&conn, id, None)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn confirm_codex_agent_thread_for_test(&self, agent: i64, thread: &str) {
        confirm_codex_agent_thread(&self.conn.lock(), agent, Some(thread)).unwrap();
    }

    /// The Codex agent thread this backend confirmed (`codex_agent_thread_key`).
    pub fn confirmed_codex_agent_thread(&self, agent: i64) -> anyhow::Result<Option<String>> {
        self.load_runtime(&codex_agent_thread_key(agent))
    }

    /// The fields `agent.update` changes (`agents::update`).
    pub fn update_agent_config(&self, b: &crate::agents::Agent) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE agents SET model = ?2, effort = ?3, persona = ?4, permissions = ?5, accepts_origins = ?6,
                    autostart = CASE WHEN asleep_at IS NULL THEN ?7 ELSE 0 END,
                    asleep_autostart = CASE WHEN asleep_at IS NULL THEN NULL ELSE ?7 END,
                    provider = ?8, sleep_after_min = ?9 WHERE id = ?1",
            params![
                b.id,
                b.model,
                b.effort,
                b.persona,
                perms_json(&b.permissions),
                serde_json::to_string(&b.accepts_origins)?,
                b.autostart as i64,
                b.provider.as_str(),
                b.sleep_after_min
            ],
        )?;
        Ok(())
    }

    /// Agent `id` fell asleep at `at` (src/sleep.rs), stopping run `generation`, or woke (None).
    /// While asleep its `autostart` column reads 0, the setting kept in `asleep_autostart`
    /// (migration v24); an older backend that turned autostart on meanwhile keeps it on.
    pub fn set_agent_asleep(&self, id: i64, at: Option<i64>, generation: Option<u64>) -> anyhow::Result<()> {
        let kept = "MAX(autostart, COALESCE(asleep_autostart, 0))";
        let conn = self.conn.lock();
        match at {
            Some(at) => conn.execute(
                &format!("UPDATE agents SET asleep_autostart = CASE WHEN asleep_at IS NULL THEN autostart ELSE {kept} END,
                    autostart = 0, asleep_at = ?2, asleep_generation = ?3 WHERE id = ?1"),
                params![id, at, generation.map(|g| g as i64)],
            )?,
            None => conn.execute(
                &format!("UPDATE agents SET autostart = CASE WHEN asleep_at IS NULL THEN autostart ELSE {kept} END,
                    asleep_autostart = NULL, asleep_at = NULL, asleep_generation = NULL WHERE id = ?1"),
                params![id],
            )?,
        };
        Ok(())
    }

    /// The run agent `id`'s sleep was stopping (`set_agent_asleep`), when it is asleep.
    pub fn agent_asleep_generation(&self, id: i64) -> anyhow::Result<Option<u64>> {
        Ok(self.conn.lock()
            .query_row("SELECT asleep_generation FROM agents WHERE id = ?1", [id], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten()
            .map(|g| g as u64))
    }

    /// The bots -> agents move (`agents::relocate_homes`): agent `id`'s home moved from one of
    /// `olds` (spellings of the same folder) to `new`. Its record points at the new folder and
    /// forgets its conversation id; sessions that ran in the old folder, or below it, follow.
    pub fn relocate_agent_home(&self, id: i64, olds: &[String], new: &str) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("UPDATE agents SET home_dir = ?2, resume_id = NULL WHERE id = ?1", params![id, new])?;
        for old in olds {
            tx.execute(
                "UPDATE sessions SET cwd = ?2 || substr(cwd, length(?1) + 1)
                 WHERE cwd = ?1 OR substr(cwd, 1, length(?1) + 1) = ?1 || '/'",
                params![old, new],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn has_workspace_at(&self, paths: &[String]) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        for p in paths {
            if conn
                .query_row("SELECT 1 FROM workspaces WHERE path = ?1", [p], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The agents workspace moved from one of `olds` to `new` (renamed `name`), with its tabs'
    /// cwd. If a workspace is already at `new`, the old one's tabs join it and the old row goes.
    pub fn relocate_agents_workspace(&self, olds: &[String], new: &str, name: &str) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for old in olds {
            let existing: Option<WorkspaceId> =
                tx.query_row("SELECT id FROM workspaces WHERE path = ?1", [new], |r| r.get(0)).optional()?;
            let Some(id) = tx
                .query_row("SELECT id FROM workspaces WHERE path = ?1", [old], |r| r.get::<_, WorkspaceId>(0))
                .optional()?
            else {
                continue;
            };
            tx.execute("UPDATE channels SET cwd = ?2 WHERE workspace_id = ?1 AND cwd = ?3", params![id, new, old])?;
            match existing {
                Some(to) if to != id => {
                    tx.execute("UPDATE channels SET workspace_id = ?2 WHERE workspace_id = ?1", params![id, to])?;
                    tx.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
                }
                _ => {
                    tx.execute("UPDATE workspaces SET path = ?2, name = ?3 WHERE id = ?1", params![id, new, name])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_agent(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM agents WHERE id = ?1", [id])?;
        confirm_codex_agent_thread(&conn, id, None)?;
        Ok(())
    }

    fn row_agent_message(r: &Row) -> rusqlite::Result<crate::agents::AgentMessage> {
        let origin: String = r.get(5)?;
        Ok(crate::agents::AgentMessage {
            id: r.get(0)?,
            from: r.get(1)?,
            from_session: r.get::<_, Option<i64>>(2)?.map(|v| v as SessionId),
            to_agent: r.get(3)?,
            to_session: r.get::<_, Option<i64>>(4)?.map(|v| v as SessionId),
            // Unreadable: treat as the strictest origin an agent may refuse.
            origin: crate::agents::Origin::parse(&origin).unwrap_or(crate::agents::Origin::Webhook),
            hop: r.get::<_, i64>(6)?.clamp(0, u32::MAX as i64) as u32,
            reply_to: r.get(7)?,
            created_at: r.get(8)?,
            reply: r.get(9)?,
            replied_at: r.get(10)?,
            text: r.get(11)?,
            refused: r.get(12)?,
            to_name: r.get(13)?,
            cold_idle_ms: r.get(14)?,
        })
    }

    /// Records a message muster is about to deliver to an agent; returns its id.
    #[cfg(test)]
    pub fn insert_agent_message(&self, m: &crate::agents::AgentMessage) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Self::insert_agent_message_on(&conn, m, None)?;
        Ok(conn.last_insert_rowid())
    }

    fn insert_agent_message_on(
        conn: &Connection,
        m: &crate::agents::AgentMessage,
        refused: Option<&str>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO agent_messages (from_label, from_session, to_agent, to_session, origin, hop, reply_to,
                                         created_at, text, refused)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.from,
                m.from_session,
                m.to_agent,
                m.to_session,
                m.origin.as_str(),
                m.hop,
                m.reply_to,
                m.created_at,
                m.text,
                refused
            ],
        )
    }

    /// A message an agent's policy refused (origin, hop limit, daily cap, permissions): kept for
    /// the agents panel's log, never counted toward the cap.
    pub fn insert_refused_agent_message(&self, m: &crate::agents::AgentMessage, why: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Self::insert_agent_message_on(&conn, m, Some(why))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn agent_message(&self, id: i64) -> anyhow::Result<Option<crate::agents::AgentMessage>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(&format!("{AGENT_MESSAGE_SELECT} WHERE m.id = ?1"), [id], Self::row_agent_message)
            .optional()?)
    }

    /// Newest first: messages to `agent` (else all), with id below `before`, at most `limit`.
    pub fn agent_messages(
        &self,
        agent: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::agents::AgentMessage>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "{AGENT_MESSAGE_SELECT} WHERE (?1 IS NULL OR m.to_agent = ?1) AND (?2 IS NULL OR m.id < ?2)
             ORDER BY m.id DESC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![agent, before, limit as i64], Self::row_agent_message)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `insert_agent_message`, unless the agent already got `cap` messages at or after `since`:
    /// then `Err(count)`. Counted and inserted in one transaction under the store's lock, so
    /// concurrent asks (MCP or RPC) can't overshoot the cap. Refused messages don't count.
    pub fn insert_agent_message_capped(
        &self,
        m: &crate::agents::AgentMessage,
        since: i64,
        cap: usize,
        unattended_cap: Option<usize>,
    ) -> anyhow::Result<Result<i64, CapHit>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let n: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE to_agent = ?1 AND created_at >= ?2 AND refused IS NULL",
            params![m.to_agent, since],
            |r| r.get(0),
        )?;
        let n = n.max(0) as usize;
        if n >= cap {
            return Ok(Err(CapHit::All(n)));
        }
        if let Some(ucap) = unattended_cap {
            let u = Self::count_unattended_on(&tx, m.to_agent, since)?;
            if u >= ucap {
                return Ok(Err(CapHit::Unattended(u)));
            }
        }
        Self::insert_agent_message_on(&tx, m, None)?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Ok(id))
    }

    /// Retention for refused messages: drop those created before `before`, and all but the
    /// newest `keep`. Delivered messages are never pruned. Returns the rows deleted.
    pub fn prune_refused_agent_messages(&self, before: i64, keep: usize) -> anyhow::Result<usize> {
        Ok(self.conn.lock().execute(
            "DELETE FROM agent_messages WHERE refused IS NOT NULL AND (created_at < ?1 OR id NOT IN (
                SELECT id FROM agent_messages WHERE refused IS NOT NULL ORDER BY id DESC LIMIT ?2))",
            params![before, keep as i64],
        )?)
    }

    /// A message that was never delivered (the send failed): it must not count toward the cap.
    pub fn delete_agent_message(&self, id: i64) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM agent_messages WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Commit attribution, reply, resume ID, session state and inbox checkpoint together.
    /// A crash cannot leave a saved reply eligible for attribution to a later message.
    pub fn commit_hook(
        &self,
        attribution: &impl serde::Serialize,
        reply: Option<(i64, &str)>,
        agent: Option<&crate::agents::Agent>,
        cursor: Option<&HookCursor>,
        session: Option<(&SessionInfo, &HookSession)>,
    ) -> anyhow::Result<()> {
        let mut connection = self.conn.lock();
        let tx = connection.transaction()?;
        if let Some((id, reply)) = reply {
            tx.execute("UPDATE agent_messages SET reply=?2, replied_at=?3 WHERE id=?1",
                params![id, reply, now_ms()])?;
        }
        if let Some(agent) = agent {
            tx.execute("UPDATE agents SET channel_id=?2, session_id=?3, resume_id=?4 WHERE id=?1",
                params![agent.id, agent.channel_id, agent.session_id, agent.resume_id])?;
            if agent.kind == SessionKind::Codex {
                confirm_codex_agent_thread(&tx, agent.id, agent.resume_id.as_deref())?;
            }
        }
        tx.execute("INSERT INTO runtime_state(key,record) VALUES ('agent-attribution',?1)
            ON CONFLICT(key) DO UPDATE SET record=excluded.record", [serde_json::to_string(attribution)?])?;
        if let Some(cursor) = cursor {
            tx.execute("INSERT INTO runtime_state(key,record) VALUES ('hook-inbox-cursor',?1)
                ON CONFLICT(key) DO UPDATE SET record=excluded.record", [serde_json::to_string(cursor)?])?;
        }
        if let Some((info, runtime)) = session {
            tx.execute("UPDATE sessions SET state=?2, state_detail=?3, state_since=?4, last_turn_at=?5 WHERE id=?1",
                params![info.id, info.state.as_str(), info.state_detail, info.state_since, info.last_turn_at])?;
            tx.execute("INSERT INTO runtime_state(key,record) VALUES (?1,?2)
                ON CONFLICT(key) DO UPDATE SET record=excluded.record",
                params![format!("hook-session:{}", info.id), serde_json::to_string(runtime)?])?;
            if let Some(args) = &runtime.extra_args {
                tx.execute("UPDATE sessions SET extra_args = ?2 WHERE id = ?1",
                    params![info.id, serde_json::to_string(args)?])?;
            }
            if let Some(thread) = &runtime.tab_thread {
                save_codex_thread(&tx, info.id, thread.as_ref())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A Codex tab's saved thread (`CodexThread`), if any.
    pub fn codex_thread(&self, session: SessionId) -> anyhow::Result<Option<CodexThread>> {
        self.load_runtime(&codex_thread_key(session))
    }

    /// Save (None: forget) a Codex tab's thread.
    pub fn set_codex_thread(&self, session: SessionId, thread: Option<&CodexThread>) -> anyhow::Result<()> {
        save_codex_thread(&self.conn.lock(), session, thread)
    }

    pub fn load_hook_session(&self, id: SessionId, generation: u64) -> anyhow::Result<Option<HookSession>> {
        Ok(self.load_runtime::<HookSession>(&format!("hook-session:{id}"))?
            .filter(|saved| saved.generation == generation))
    }

    /// Mark message `id` as typed into an agent whose prompt cache had likely expired. False
    /// when it was marked already (a prompt submitted again after a retry) or is gone.
    pub fn set_agent_message_cold(&self, id: i64, idle_ms: i64) -> anyhow::Result<bool> {
        Ok(self.conn.lock().execute("UPDATE agent_messages SET cold_idle_ms=?2 WHERE id=?1 AND cold_idle_ms IS NULL",
            params![id, idle_ms])? == 1)
    }

    /// A message taken back before it was submitted (the spend caps): kept in the log with the
    /// reason, counted toward no cap.
    pub fn set_agent_message_refused(&self, id: i64, why: &str) -> anyhow::Result<()> {
        self.conn.lock().execute("UPDATE agent_messages SET refused=?2 WHERE id=?1 AND reply IS NULL", params![id, why])?;
        Ok(())
    }

    pub fn set_agent_message_reply(&self, id: i64, reply: &str, at: i64) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE agent_messages SET reply = ?2, replied_at = ?3 WHERE id = ?1",
            params![id, reply, at],
        )?;
        Ok(())
    }

    /// Messages delivered to agent `agent_id` at or after `since` (unix ms): the daily cap.
    pub fn count_agent_messages_since(&self, agent_id: i64, since: i64) -> anyhow::Result<usize> {
        let n: i64 = self.conn.lock().query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE to_agent = ?1 AND created_at >= ?2 AND refused IS NULL",
            params![agent_id, since],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    fn count_unattended_on(conn: &Connection, agent_id: i64, since: i64) -> rusqlite::Result<usize> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE to_agent = ?1 AND created_at >= ?2 AND refused IS NULL
             AND origin != 'human'",
            params![agent_id, since],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    /// Unattended (scheduled or triggered: origin other than `human`) messages delivered to
    /// `agent_id` since `since`: the unattended daily cap.
    pub fn count_unattended_since(&self, agent_id: i64, since: i64) -> anyhow::Result<usize> {
        Ok(Self::count_unattended_on(&self.conn.lock(), agent_id, since)?)
    }

    /// The newest message to `agent_id` from sender `from` created at or after `since`: a
    /// schedule run whose delivery happened just before a restart finds its message again.
    pub fn find_agent_message_from(&self, agent_id: i64, from: &str, since: i64) -> anyhow::Result<Option<i64>> {
        Ok(self.conn.lock().query_row(
            "SELECT id FROM agent_messages WHERE to_agent = ?1 AND from_label = ?2 AND created_at >= ?3
             AND refused IS NULL ORDER BY id DESC LIMIT 1",
            params![agent_id, from, since],
            |r| r.get(0),
        ).optional()?)
    }

    // ----- schedules (src/schedules/) -----

    pub fn load_schedules(&self) -> anyhow::Result<Vec<crate::schedules::Schedule>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT id, agent_id, record FROM schedules ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, agent_id, record) = row?;
            match serde_json::from_str::<crate::schedules::Schedule>(&record) {
                Ok(mut s) => {
                    s.id = id;
                    s.agent_id = agent_id;
                    out.push(s);
                }
                Err(e) => tracing::warn!(schedule = id, "unreadable schedule record, ignored: {e}"),
            }
        }
        Ok(out)
    }

    pub fn insert_schedule(&self, s: &crate::schedules::Schedule) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO schedules (agent_id, record, created_at) VALUES (?1, ?2, ?3)",
            params![s.agent_id, serde_json::to_string(s)?, s.created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn save_schedule_on(conn: &Connection, s: &crate::schedules::Schedule) -> anyhow::Result<()> {
        let n = conn.execute("UPDATE schedules SET record = ?2 WHERE id = ?1", params![s.id, serde_json::to_string(s)?])?;
        anyhow::ensure!(n == 1, "schedule {} was removed", s.id);
        Ok(())
    }

    pub fn save_schedule(&self, s: &crate::schedules::Schedule) -> anyhow::Result<()> {
        Self::save_schedule_on(&self.conn.lock(), s)
    }

    /// The schedule and its runs.
    pub fn delete_schedule(&self, id: i64) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM schedule_runs WHERE schedule_id = ?1", [id])?;
        tx.execute("DELETE FROM schedules WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Save `s` (its slot moved on) and record `run` for one of its slots, together. `Ok(None)`:
    /// that slot already has a run (claimed before a crash or a clock change): nothing is
    /// recorded, only the schedule is saved.
    pub fn claim_run(&self, s: &crate::schedules::Schedule, run: &crate::schedules::Run) -> anyhow::Result<Option<i64>> {
        Ok(self.claim_run_with_child(s, run, None)?.map(|(id, _)| id))
    }

    /// `claim_run`, and for a run that ends as it is claimed (a refusal), the "Then…" step it
    /// starts (`child`, its `parent` set to the new run), in the same transaction. Returns the
    /// run's id and the step's.
    pub fn claim_run_with_child(&self, s: &crate::schedules::Schedule, run: &crate::schedules::Run,
        child: Option<&crate::schedules::Run>) -> anyhow::Result<Option<(i64, Option<i64>)>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let n = Self::insert_run_on(&tx, run, "INSERT OR IGNORE")?;
        let id = (n == 1).then(|| tx.last_insert_rowid());
        Self::save_schedule_on(&tx, s)?;
        let mut child_id = None;
        if let Some(id) = id {
            if let Some(c) = child {
                Self::insert_run_on(&tx, &crate::schedules::Run { parent: Some(id), ..c.clone() }, "INSERT")?;
                child_id = Some(tx.last_insert_rowid());
            }
            Self::prune_runs_on(&tx, run.schedule_id, crate::schedules::RUNS_KEEP)?;
        }
        tx.commit()?;
        Ok(id.map(|id| (id, child_id)))
    }

    /// Record how run `run` ended and the "Then…" step it starts, together (a restart in between
    /// can't lose the step: a pending step is picked up like any pending run). A run's step is
    /// recorded once, however often its end is. Returns the step's id.
    pub fn finish_run_with_child(&self, run: &crate::schedules::Run, child: Option<&crate::schedules::Run>) -> anyhow::Result<Option<i64>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE schedule_runs SET msg_id = ?2, outcome = ?3, reason = ?4, finished_at = ?5, agent_id = ?6 WHERE id = ?1",
            params![run.id, run.msg_id, run.outcome.as_str(), run.reason, run.finished_at, run.agent_id],
        )?;
        let mut child_id = None;
        if let Some(c) = child {
            let had: i64 = tx.query_row("SELECT COUNT(*) FROM schedule_runs WHERE parent_run = ?1", [run.id], |r| r.get(0))?;
            if had == 0 {
                Self::insert_run_on(&tx, &crate::schedules::Run { parent: Some(run.id), ..c.clone() }, "INSERT")?;
                child_id = Some(tx.last_insert_rowid());
                Self::prune_runs_on(&tx, run.schedule_id, crate::schedules::RUNS_KEEP)?;
            }
        }
        tx.commit()?;
        Ok(child_id)
    }

    /// Runs of the triggers `schedules` taken up since `since` (scheduler clock) that are going
    /// or reached their agent: the per-note cap (Run now doesn't count).
    pub fn count_trigger_runs_taken(&self, schedules: &[i64], since: i64) -> anyhow::Result<u32> {
        let conn = self.conn.lock();
        let mut total = 0i64;
        for s in schedules {
            total += conn.query_row(
                "SELECT COUNT(*) FROM schedule_runs WHERE schedule_id = ?1 AND kind = 'triggered' AND started_at >= ?2
                 AND (outcome = 'pending' OR msg_id IS NOT NULL)",
                params![s, since],
                |r| r.get::<_, i64>(0),
            )?;
        }
        Ok(total.max(0) as u32)
    }

    fn insert_run_on(conn: &Connection, run: &crate::schedules::Run, verb: &str) -> rusqlite::Result<usize> {
        conn.execute(
            &format!("{verb} INTO schedule_runs (schedule_id, slot, kind, started_at, msg_id, outcome, reason, finished_at,
                parent_run, step, agent_id, event)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"),
            params![run.schedule_id, run.slot, run.kind.as_str(), run.started_at, run.msg_id, run.outcome.as_str(),
                run.reason, run.finished_at, run.parent, run.step, run.agent_id, run.event],
        )
    }

    /// A run that is no slot of the schedule's timing (Run now, a chain step).
    pub fn insert_run(&self, run: &crate::schedules::Run) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Self::insert_run_on(&conn, run, "INSERT")?;
        let id = conn.last_insert_rowid();
        Self::prune_runs_on(&conn, run.schedule_id, crate::schedules::RUNS_KEEP)?;
        Ok(id)
    }

    pub fn update_run(&self, run: &crate::schedules::Run) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE schedule_runs SET msg_id = ?2, outcome = ?3, reason = ?4, finished_at = ?5, agent_id = ?6 WHERE id = ?1",
            params![run.id, run.msg_id, run.outcome.as_str(), run.reason, run.finished_at, run.agent_id],
        )?;
        Ok(())
    }

    fn row_run(r: &Row) -> rusqlite::Result<crate::schedules::Run> {
        let kind: String = r.get(3)?;
        let outcome: String = r.get(6)?;
        Ok(crate::schedules::Run {
            id: r.get(0)?,
            schedule_id: r.get(1)?,
            slot: r.get(2)?,
            kind: crate::schedules::RunKind::parse(&kind),
            started_at: r.get(4)?,
            msg_id: r.get(5)?,
            outcome: crate::schedules::Outcome::parse(&outcome),
            reason: r.get(7)?,
            finished_at: r.get(8)?,
            parent: r.get(9)?,
            step: r.get(10)?,
            agent_id: r.get(11)?,
            event: r.get(12)?,
        })
    }

    const RUN_COLUMNS: &str =
        "id, schedule_id, slot, kind, started_at, msg_id, outcome, reason, finished_at, parent_run, step, agent_id, event";

    pub fn run(&self, id: i64) -> anyhow::Result<Option<crate::schedules::Run>> {
        let conn = self.conn.lock();
        Ok(conn.query_row(&format!("SELECT {} FROM schedule_runs WHERE id = ?1", Self::RUN_COLUMNS), [id], Self::row_run)
            .optional()?)
    }

    /// Newest first: runs of `schedule` with id below `before`, at most `limit`.
    pub fn runs(&self, schedule: i64, before: Option<i64>, limit: usize) -> anyhow::Result<Vec<crate::schedules::Run>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {} FROM schedule_runs WHERE schedule_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3",
            Self::RUN_COLUMNS
        ))?;
        let rows = st.query_map(params![schedule, before, limit as i64], Self::row_run)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn pending_runs(&self) -> anyhow::Result<Vec<crate::schedules::Run>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {} FROM schedule_runs WHERE outcome = 'pending' ORDER BY id",
            Self::RUN_COLUMNS
        ))?;
        let rows = st.query_map([], Self::row_run)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Timed runs of `schedule` (not Run now: the schedule's own cap is about its timing; the
    /// agent's caps count every message) whose message was sent at or after `since` (unix ms on
    /// the daemon's clock: the message's own time, not when the run was taken up), leaving out
    /// message `except` (the one about to be submitted).
    pub fn count_delivered_runs_since(&self, schedule: i64, since: i64, except: Option<i64>) -> anyhow::Result<u32> {
        let n: i64 = self.conn.lock().query_row(
            "SELECT COUNT(*) FROM schedule_runs r JOIN agent_messages m ON m.id = r.msg_id
             WHERE r.schedule_id = ?1 AND m.created_at >= ?2 AND r.kind NOT IN ('manual', 'chain') AND (?3 IS NULL OR m.id != ?3)",
            params![schedule, since, except],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    /// The run whose message is `msg`.
    pub fn run_by_msg(&self, msg: i64) -> anyhow::Result<Option<crate::schedules::Run>> {
        let conn = self.conn.lock();
        Ok(conn.query_row(&format!("SELECT {} FROM schedule_runs WHERE msg_id = ?1", Self::RUN_COLUMNS), [msg], Self::row_run)
            .optional()?)
    }

    /// The newest run of `schedule` below run `before` that sent a message.
    pub fn latest_sent_run(&self, schedule: i64, before: i64) -> anyhow::Result<Option<crate::schedules::Run>> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            &format!("SELECT {} FROM schedule_runs WHERE schedule_id = ?1 AND id < ?2 AND msg_id IS NOT NULL AND kind != 'chain' ORDER BY id DESC LIMIT 1",
                Self::RUN_COLUMNS),
            params![schedule, before],
            Self::row_run,
        ).optional()?)
    }

    /// Mark run `id`'s reply as put in its note; true only for the first caller.
    pub fn mark_run_noted(&self, id: i64) -> anyhow::Result<bool> {
        Ok(self.conn.lock().execute("UPDATE schedule_runs SET noted = 1 WHERE id = ?1 AND noted = 0", [id])? == 1)
    }

    /// Run `id` no longer has a message (it was never typed and was taken back).
    pub fn clear_run_msg(&self, id: i64) -> anyhow::Result<()> {
        self.conn.lock().execute("UPDATE schedule_runs SET msg_id = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The newest delivered run of `schedule` (for `{last_run}` / `{last_reply}`), and the newest
    /// one that got a reply, with that reply.
    pub fn last_runs_for_prompt(&self, schedule: i64, before_run: i64) -> anyhow::Result<(Option<i64>, Option<String>)> {
        let conn = self.conn.lock();
        let last: Option<i64> = conn.query_row(
            "SELECT slot FROM schedule_runs WHERE schedule_id = ?1 AND id < ?2 AND msg_id IS NOT NULL AND kind != 'chain' ORDER BY id DESC LIMIT 1",
            params![schedule, before_run],
            |r| r.get(0),
        ).optional()?;
        let reply: Option<String> = conn.query_row(
            "SELECT m.reply FROM schedule_runs r JOIN agent_messages m ON m.id = r.msg_id
             WHERE r.schedule_id = ?1 AND r.id < ?2 AND m.reply IS NOT NULL AND r.kind != 'chain' ORDER BY r.id DESC LIMIT 1",
            params![schedule, before_run],
            |r| r.get(0),
        ).optional()?;
        Ok((last, reply))
    }

    /// Keep the newest `keep` runs of `schedule`, any still pending, and every run that reached
    /// its agent in the last two days: the daily caps count those, so however many skips and
    /// refusals fill the history, pruning never reopens a cap. Every insert does it.
    fn prune_runs_on(conn: &Connection, schedule: i64, keep: usize) -> rusqlite::Result<usize> {
        let recent = now_ms() - 2 * 86_400_000;
        conn.execute(
            "DELETE FROM schedule_runs WHERE schedule_id = ?1 AND outcome != 'pending'
             AND NOT (msg_id IS NOT NULL AND (started_at >= ?3 OR msg_id IN (SELECT id FROM agent_messages WHERE created_at >= ?3)))
             AND id NOT IN (SELECT id FROM schedule_runs WHERE schedule_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![schedule, keep as i64, recent],
        )
    }

    // ----- Claude account profile references -----

    pub fn account_profiles(&self) -> anyhow::Result<Vec<crate::account_profiles::AccountProfile>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare("SELECT id, name, config_dir, enabled, created_at, account_group FROM account_profiles ORDER BY name COLLATE NOCASE, id")?;
        let rows = statement.query_map([], |r| Ok(crate::account_profiles::AccountProfile {
            id: r.get(0)?, name: r.get(1)?, config_dir: r.get(2)?,
            enabled: r.get::<_, i64>(3)? != 0, created_at: r.get(4)?, account_group: r.get(5)?,
        }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn insert_account_profile(&self, name: &str, config_dir: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.execute("INSERT INTO account_profiles(name, config_dir, created_at) VALUES (?1, ?2, ?3)",
            params![name, config_dir, now_ms()])
            .context("profile name or directory is already registered")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_account_profile(&self, id: i64, name: Option<&str>, enabled: Option<bool>) -> anyhow::Result<()> {
        let changed = self.conn.lock().execute(
            "UPDATE account_profiles SET name=COALESCE(?2, name), enabled=COALESCE(?3, enabled) WHERE id=?1",
            params![id, name, enabled.map(i64::from)],
        )?;
        anyhow::ensure!(changed == 1, "no such account profile: {id}");
        Ok(())
    }

    /// Declare `id` to be on the same account as `with` (None: its own account). Joins `with`'s
    /// group, creating one for both when it has none.
    pub fn set_same_account(&self, id: i64, with: Option<i64>) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let group_of = |tx: &rusqlite::Transaction, p: i64| -> anyhow::Result<Option<String>> {
            tx.query_row("SELECT account_group FROM account_profiles WHERE id=?1", [p], |r| r.get(0))
                .optional()?.with_context(|| format!("no such account profile: {p}"))
        };
        group_of(&tx, id)?;
        let group = match with {
            None => None,
            Some(other) => {
                anyhow::ensure!(other != id, "a profile is always on its own account");
                Some(match group_of(&tx, other)? {
                    Some(g) => g,
                    None => {
                        let g = crate::paths::random_token()[..16].to_string();
                        tx.execute("UPDATE account_profiles SET account_group=?2 WHERE id=?1", params![other, g])?;
                        g
                    }
                })
            }
        };
        tx.execute("UPDATE account_profiles SET account_group=?2 WHERE id=?1", params![id, group])?;
        tx.commit()?;
        Ok(())
    }

    // ----- Claude usage observations (src/usage.rs) -----

    pub fn usage_observations(&self) -> anyhow::Result<Vec<(String, crate::usage::Window, crate::usage::Observation)>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT account, window, record FROM usage_observations")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (account, window, record) = row?;
            // An unreadable row is dropped (unknown), never guessed.
            if let (Some(w), Ok(o)) = (crate::usage::Window::from_key(&window), serde_json::from_str(&record)) {
                out.push((account, w, o));
            }
        }
        Ok(out)
    }

    /// Record a Claude run's launch: the configuration folder its reports are filed under.
    /// Replaces the session's previous run.
    pub fn start_usage_run(&self, session: SessionId, generation: u64, config_dir: Option<String>) -> anyhow::Result<()> {
        let run = crate::usage::RunMemory { generation, config_dir, ..Default::default() };
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // Forget the runs of sessions that no longer exist.
        tx.execute("DELETE FROM usage_runs WHERE session_id NOT IN (SELECT id FROM sessions)", [])?;
        tx.execute("INSERT INTO usage_runs(session_id, record) VALUES (?1, ?2)
            ON CONFLICT(session_id) DO UPDATE SET record=excluded.record", params![session, serde_json::to_string(&run)?])?;
        tx.commit()?;
        Ok(())
    }

    /// Apply one report from run (`session`, `generation`) to the observations of the folder
    /// that run was launched with, in one transaction. A run with no launch record (or another
    /// generation's) is ignored. Returns whether a client-visible observation changed.
    pub fn apply_usage(&self, session: SessionId, generation: u64, report: &crate::usage::Report, now: i64)
        -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let run: Option<crate::usage::RunMemory> = tx.query_row("SELECT record FROM usage_runs WHERE session_id=?1", [session],
            |r| r.get::<_, String>(0)).optional()?.and_then(|j| serde_json::from_str(&j).ok());
        let Some(run) = run.filter(|r| r.generation == generation) else { return Ok(false) };
        let Some(account) = run.config_dir.as_deref().map(crate::usage::folder_key) else { return Ok(false) };
        let mut stored = std::collections::BTreeMap::new();
        {
            let mut st = tx.prepare("SELECT window, record FROM usage_observations WHERE account=?1")?;
            let rows = st.query_map([&account], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (w, record) = row?;
                if let (Some(w), Ok(o)) = (crate::usage::Window::from_key(&w), serde_json::from_str(&record)) {
                    stored.insert(w, o);
                }
            }
        }
        let applied = crate::usage::apply(&run, &stored, (session, generation), report, now);
        for (w, o) in &applied.updates {
            tx.execute("INSERT INTO usage_observations(account, window, record) VALUES (?1, ?2, ?3)
                ON CONFLICT(account, window) DO UPDATE SET record=excluded.record",
                params![account, w.key(), serde_json::to_string(o)?])?;
        }
        if let Some(m) = &applied.run {
            tx.execute("INSERT INTO usage_runs(session_id, record) VALUES (?1, ?2)
                ON CONFLICT(session_id) DO UPDATE SET record=excluded.record", params![session, serde_json::to_string(m)?])?;
        }
        tx.commit()?;
        Ok(applied.notify)
    }

    /// Forget `account`'s observations (the human reported a login change there).
    pub fn clear_usage(&self, account: &str) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM usage_observations WHERE account=?1", [account])?;
        Ok(())
    }

    pub fn set_workspace_account_profile(&self, id: WorkspaceId, profile: Option<i64>) -> anyhow::Result<()> {
        let changed = self.conn.lock().execute("UPDATE workspaces SET account_profile_id=?2 WHERE id=?1", params![id, profile])?;
        anyhow::ensure!(changed == 1, "no such workspace: {id}");
        Ok(())
    }

    // ----- workspaces -----

    pub fn load_workspaces(&self) -> anyhow::Result<Vec<Workspace>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT id, name, path, is_git, created_at, account_profile_id FROM workspaces ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                is_git: r.get::<_, i64>(3)? != 0,
                created_at: r.get(4)?,
                account_profile_id: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn workspace_by_path(&self, path: &str) -> anyhow::Result<Option<WorkspaceId>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id FROM workspaces WHERE path = ?1", [path], |r| r.get(0))
            .optional()?)
    }

    pub fn insert_workspace(
        &self,
        name: &str,
        path: &str,
        is_git: bool,
        created_at: i64,
    ) -> anyhow::Result<WorkspaceId> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO workspaces (name, path, is_git, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, path, is_git as i64, created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn delete_workspace(&self, id: WorkspaceId) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
        Ok(())
    }

    // ----- channels -----

    fn row_channel(r: &Row) -> rusqlite::Result<Channel> {
        let layout: Option<String> = r.get(9)?;
        Ok(Channel {
            id: r.get(0)?,
            workspace_id: r.get(1)?,
            name: r.get(2)?,
            cwd: r.get(3)?,
            branch: r.get(4)?,
            base_branch: r.get(5)?,
            owns_worktree: r.get::<_, i64>(6)? != 0,
            port_base: r.get::<_, i64>(7)?.clamp(0, u16::MAX as i64) as u16,
            archived: r.get::<_, i64>(8)? != 0,
            layout: layout.and_then(|s| serde_json::from_str(&s).ok()),
            created_at: r.get(10)?,
            last_read_at: r.get(11)?,
            sort_order: r.get(12)?,
            permissions: r
                .get::<_, Option<String>>(13)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            window_id: r.get(14)?,
            auto_named: r.get::<_, i64>(15)? != 0,
            first_prompt: r.get(16)?,
        })
    }

    pub fn load_channels(&self) -> anyhow::Result<Vec<Channel>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, workspace_id, name, cwd, branch, base_branch, owns_worktree, port_base, archived,
                    layout, created_at, last_read_at, sort_order, permissions, window_id, auto_named, first_prompt
             FROM channels ORDER BY id",
        )?;
        let rows = st.query_map([], Self::row_channel)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts the channel (its `id` is ignored) and returns the new id.
    pub fn insert_channel(&self, c: &Channel) -> anyhow::Result<ChannelId> {
        self.insert_channel_with_origin(c, false)
    }

    pub fn mcp_channels(&self) -> anyhow::Result<std::collections::HashSet<ChannelId>> {
        let conn = self.conn.lock();
        let mut query = conn.prepare("SELECT m.channel_id FROM mcp_channels m JOIN channels c ON c.id=m.channel_id")?;
        let rows = query.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_channel_with_origin(&self, c: &Channel, mcp: bool) -> anyhow::Result<ChannelId> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO channels (workspace_id, name, cwd, branch, base_branch, owns_worktree, port_base,
                                   archived, layout, created_at, last_read_at, sort_order, permissions,
                                   window_id, auto_named, first_prompt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                c.workspace_id,
                c.name,
                c.cwd,
                c.branch,
                c.base_branch,
                c.owns_worktree as i64,
                c.port_base as i64,
                c.archived as i64,
                c.layout.as_ref().map(|v| v.to_string()),
                c.created_at,
                c.last_read_at,
                c.sort_order,
                perms_json(&c.permissions),
                c.window_id,
                c.auto_named as i64,
                c.first_prompt
            ],
        )?;
        let id = tx.last_insert_rowid();
        if mcp { tx.execute("INSERT INTO mcp_channels(channel_id) VALUES (?1)", [id])?; }
        tx.commit()?;
        Ok(id)
    }

    pub fn update_channel(&self, c: &Channel) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE channels SET workspace_id=?2, name=?3, cwd=?4, branch=?5, base_branch=?6, owns_worktree=?7,
                    port_base=?8, archived=?9, layout=?10, created_at=?11, last_read_at=?12, sort_order=?13,
                    permissions=?14, window_id=?15, auto_named=?16, first_prompt=?17
             WHERE id=?1",
            params![
                c.id,
                c.workspace_id,
                c.name,
                c.cwd,
                c.branch,
                c.base_branch,
                c.owns_worktree as i64,
                c.port_base as i64,
                c.archived as i64,
                c.layout.as_ref().map(|v| v.to_string()),
                c.created_at,
                c.last_read_at,
                c.sort_order,
                perms_json(&c.permissions),
                c.window_id,
                c.auto_named as i64,
                c.first_prompt
            ],
        )?;
        Ok(())
    }

    /// Forget a channel row (only for one that never went live: a failed create).
    pub fn delete_channel(&self, id: ChannelId) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM channels WHERE id = ?1", [id])?;
        Ok(())
    }

    // ----- windows -----

    pub fn load_windows(&self) -> anyhow::Result<Vec<StoredWindow>> {
        let conn = self.conn.lock();
        let mut st =
            conn.prepare("SELECT id, name, last_focused_at, created_at, layout FROM windows ORDER BY id")?;
        let rows = st.query_map([], |r| {
            let layout: Option<String> = r.get(4)?;
            Ok(StoredWindow {
                id: r.get(0)?,
                name: r.get(1)?,
                last_focused_at: r.get(2)?,
                created_at: r.get(3)?,
                layout: layout.and_then(|s| serde_json::from_str(&s).ok()),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Store a window's layout (None clears it). The row must exist.
    pub fn set_window_layout(&self, id: WindowId, layout: Option<&serde_json::Value>) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE windows SET layout = ?2 WHERE id = ?1",
            params![id, layout.map(|v| v.to_string())],
        )?;
        Ok(())
    }

    /// Insert or update a window row (not its layout).
    pub fn upsert_window(&self, id: WindowId, name: &str, last_focused_at: i64, created_at: i64) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO windows (id, name, last_focused_at, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, last_focused_at = excluded.last_focused_at",
            params![id, name, last_focused_at, created_at],
        )?;
        Ok(())
    }

    /// Delete a window. Notes shown there close with it, in the same transaction: window ids are
    /// reused, so a later window with this id must not inherit them. Returns those notes' ids.
    pub fn delete_window(&self, id: WindowId) -> anyhow::Result<Vec<NoteId>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let notes = {
            let mut q = tx.prepare("SELECT id FROM notes WHERE window_id = ?1")?;
            q.query_map([id], |r| r.get(0))?.collect::<Result<Vec<NoteId>, _>>()?
        };
        tx.execute("UPDATE notes SET window_id = NULL, beside = NULL, version = version + 1 WHERE window_id = ?1", [id])?;
        tx.execute("DELETE FROM windows WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(notes)
    }

    /// Close notes whose window no longer exists (a crash between the two writes of an older
    /// build, or records from before windows were deleted with their notes).
    pub fn close_orphan_notes(&self) -> anyhow::Result<usize> {
        Ok(self.conn.lock().execute(
            "UPDATE notes SET window_id = NULL, beside = NULL, version = version + 1 WHERE window_id IS NOT NULL AND window_id NOT IN (SELECT id FROM windows)",
            [],
        )?)
    }

    // ----- sessions -----

    pub fn load_sessions(&self) -> anyhow::Result<Vec<StoredSession>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, channel_id, kind, name, cwd, state, state_detail, state_since, title, parent_id, pid,
                    exit_code, cols, rows, queued, extra_args, created_at, window_id, account_profile_id, hidden, provider,
                    last_turn_at FROM sessions ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            let kind: String = r.get(2)?;
            let state: String = r.get(5)?;
            let extra: String = r.get(15)?;
            Ok(StoredSession {
                info: SessionInfo {
                    id: r.get::<_, i64>(0)? as SessionId,
                    channel_id: r.get(1)?,
                    kind: SessionKind::parse(&kind).unwrap_or(SessionKind::Shell),
                    name: r.get(3)?,
                    cwd: r.get(4)?,
                    state: enum_from_str(&state).unwrap_or(AgentState::Exited),
                    state_detail: r.get(6)?,
                    state_since: r.get(7)?,
                    title: r.get(8)?,
                    parent_id: r.get::<_, Option<i64>>(9)?.map(|v| v as SessionId),
                    pid: r.get::<_, Option<i64>>(10)?.map(|v| v as u32),
                    exit_code: r.get(11)?,
                    cols: r.get::<_, i64>(12)?.clamp(1, u16::MAX as i64) as u16,
                    rows: r.get::<_, i64>(13)?.clamp(1, u16::MAX as i64) as u16,
                    queued: r.get::<_, i64>(14)?.max(0) as u32,
                    subagents: 0,
                    background: 0,
                    size_owner: None,
                    input_interrupted: false,
                    live_cwd: None,
                    last_turn_at: r.get(21)?,
                    // Derived from the shared preferences by the hub.
                    cache_cold_at: None,
                    cache_ttl_min: None,
                    cache_basis: Default::default(),
                    state_seq: 0,
                    created_at: r.get(16)?,
                    window_id: r.get(17)?,
                    hidden: r.get::<_, i64>(19)? != 0,
                    provider: crate::openrouter::Provider::parse(&r.get::<_, String>(20)?),
                    account_profile_id: r.get(18)?,
                },
                extra_args: serde_json::from_str(&extra).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts a session row (its `info.id` is ignored) and returns the new id.
    pub fn insert_session(&self, s: &StoredSession) -> anyhow::Result<SessionId> {
        let conn = self.conn.lock();
        let i = &s.info;
        conn.execute(
            "INSERT INTO sessions (channel_id, kind, name, cwd, state, state_detail, state_since, title,
                                   parent_id, pid, exit_code, cols, rows, queued, extra_args, created_at, window_id, account_profile_id, hidden, provider,
                                   last_turn_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                i.channel_id,
                i.kind.as_str(),
                i.name,
                i.cwd,
                i.state.as_str(),
                i.state_detail,
                i.state_since,
                i.title,
                i.parent_id,
                i.pid,
                i.exit_code,
                i.cols,
                i.rows,
                i.queued,
                serde_json::to_string(&s.extra_args).unwrap_or_else(|_| "[]".into()),
                i.created_at,
                i.window_id,
                i.account_profile_id,
                i.hidden,
                i.provider.as_str(),
                i.last_turn_at
            ],
        )?;
        let id = conn.last_insert_rowid();
        u32::try_from(id).map_err(|_| anyhow::anyhow!("session id space exhausted"))
    }

    pub fn update_session(&self, i: &SessionInfo) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET name=?2, cwd=?3, state=?4, state_detail=?5, state_since=?6, title=?7, pid=?8,
                    exit_code=?9, cols=?10, rows=?11, queued=?12, channel_id=?13, window_id=?14, hidden=?15, provider=?16,
                    last_turn_at=?17 WHERE id=?1",
            params![
                i.id,
                i.name,
                i.cwd,
                i.state.as_str(),
                i.state_detail,
                i.state_since,
                i.title,
                i.pid,
                i.exit_code,
                i.cols,
                i.rows,
                i.queued,
                i.channel_id,
                i.window_id,
                i.hidden,
                i.provider.as_str(),
                i.last_turn_at
            ],
        )?;
        Ok(())
    }

    /// Replace a session's stored agent arguments (used on its next start and restarts).
    pub fn set_session_extra_args(&self, id: SessionId, args: &[String]) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET extra_args = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(args)?],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: SessionId) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM runtime_state WHERE key=?1", [format!("hook-session:{id}")])?;
        tx.execute("DELETE FROM runtime_state WHERE key=?1", [format!("cold-send:{id}")])?;
        tx.execute("DELETE FROM runtime_state WHERE key=?1", [codex_thread_key(id)])?;
        tx.commit()?;
        Ok(())
    }

    /// A removal either keeps the session and its live launch, or durably removes
    /// the row and leaves the host retirement intent. Never publish half of it.
    pub fn retire_session(&self, id: SessionId, terminal: Option<&StoredTerminal>) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        if let Some(terminal) = terminal {
            anyhow::ensure!(terminal.retired && terminal.spec.run.id == id, "invalid terminal retirement");
            transaction.execute(
                "INSERT INTO terminal_runs(session_id, record) VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET record=excluded.record",
                params![id, serde_json::to_string(terminal)?],
            )?;
        }
        transaction.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        transaction.execute("DELETE FROM runtime_state WHERE key=?1", [format!("hook-session:{id}")])?;
        transaction.execute("DELETE FROM runtime_state WHERE key=?1", [format!("cold-send:{id}")])?;
        transaction.execute("DELETE FROM events WHERE session_id=?1", [id])?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn fail_session_removal_for_test(&self) {
        self.conn.lock().execute_batch("CREATE TRIGGER refuse_session_removal BEFORE DELETE ON sessions
            BEGIN SELECT RAISE(ABORT, 'fixture removal failure'); END;").unwrap();
    }

    /// Record the session id (= pid of the process muster started) of a session's current
    /// process. Kept after the process exits, so leftovers can be reaped at the next start.
    /// `markers` are `KEY=value`s from that process's environment unique to the run
    /// (`MUSTER_RUN=…`, `MUSTER_SESSION_ID=…`), used to recognize its leftovers.
    pub fn set_session_sid(&self, id: SessionId, sid: Option<u32>, markers: &[String]) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET sid = ?2, run_marker = ?3 WHERE id = ?1",
            params![id, sid, markers.join("\n")],
        )?;
        Ok(())
    }

    /// Every process session to reap at startup: (muster session id, sid, markers), from
    /// sessions' last processes and from removed sessions whose kill hadn't finished.
    pub fn session_sids(&self) -> anyhow::Result<Vec<(SessionId, u32, Vec<String>)>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, sid, run_marker FROM sessions WHERE sid IS NOT NULL
             UNION ALL SELECT session_id, sid, run_marker FROM reap",
        )?;
        let rows = st.query_map([], |r| {
            let m: Option<String> = r.get(2)?;
            Ok((
                r.get::<_, i64>(0)? as SessionId,
                r.get::<_, i64>(1)? as u32,
                m.map(|m| m.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
                    .unwrap_or_default(),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forget recorded process sessions, except those of `keep` (their processes survived).
    pub fn clear_session_sids(&self, keep: &std::collections::HashSet<SessionId>) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let keep: Vec<String> = keep.iter().map(|i| i.to_string()).collect();
        let keep = keep.join(",");
        conn.execute(
            &format!("UPDATE sessions SET sid = NULL, run_marker = NULL WHERE sid IS NOT NULL AND id NOT IN ({keep})"),
            [],
        )?;
        conn.execute(&format!("DELETE FROM reap WHERE session_id NOT IN ({keep})"), [])?;
        Ok(())
    }

    /// A removed session whose processes are still being killed: remember them until the kill
    /// is confirmed, so a daemon stop in between doesn't leave them unreaped.
    pub fn add_reap(&self, id: SessionId, sid: u32, markers: &[String]) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO reap (session_id, sid, run_marker) VALUES (?1, ?2, ?3)",
            params![id, sid, markers.join("\n")],
        )?;
        Ok(())
    }

    pub fn remove_reap(&self, id: SessionId, sid: u32) -> anyhow::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM reap WHERE session_id = ?1 AND sid = ?2", params![id, sid])?;
        Ok(())
    }

    /// Before a session row is deleted: keep its recorded process session (processes that
    /// survived an earlier reaping) for the next start's reaper.
    pub fn keep_sid_for_reap(&self, id: SessionId) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO reap (session_id, sid, run_marker)
             SELECT id, sid, COALESCE(run_marker, '') FROM sessions WHERE id = ?1 AND sid IS NOT NULL",
            [id],
        )?;
        Ok(())
    }

    /// At daemon start: any session not Exited becomes Exited ("daemon restarted").
    pub fn mark_all_exited(&self, detail: &str, now: i64) -> anyhow::Result<usize> {
        Ok(self.conn.lock().execute(
            "UPDATE sessions SET state='exited', state_detail=?1, state_since=?2, pid=NULL, queued=0
             WHERE state != 'exited'",
            params![detail, now],
        )?)
    }

    /// Hold the database connection, as a slow SQLite statement or commit would.
    #[cfg(test)]
    pub fn hold_connection_for_test(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    #[cfg(test)]
    pub fn fail_runtime_writes_for_test(&self) {
        self.conn.lock().execute("DROP TABLE runtime_state", []).unwrap();
    }

    pub fn save_runtime(&self, key: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO runtime_state(key, record) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET record=excluded.record",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub fn load_runtime<T: serde::de::DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let json: Option<String> = self.conn.lock().query_row(
            "SELECT record FROM runtime_state WHERE key=?1", [key], |row| row.get(0),
        ).optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into)).transpose()
    }

    pub fn delete_runtime(&self, key: &str) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM runtime_state WHERE key=?1", [key])?;
        Ok(())
    }

    pub fn save_terminal(&self, id: SessionId, run: &StoredTerminal) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO terminal_runs(session_id, record) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET record=excluded.record",
            params![id, serde_json::to_string(run)?],
        )?;
        Ok(())
    }

    /// Reserve before sending a spawn request. Deleting retired launch records
    /// must not let a replacement backend reuse a host's generation tombstone.
    pub fn next_terminal_generation(&self, observed_floor: u64) -> anyhow::Result<u64> {
        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let previous: Option<String> = transaction.query_row(
            "SELECT record FROM runtime_state WHERE key='terminal-generation'", [], |row| row.get(0),
        ).optional()?;
        let previous = previous.map(|s| s.parse::<u64>()).transpose()?.unwrap_or(0);
        let next = previous.max(observed_floor).checked_add(1).context("terminal generation exhausted")?;
        anyhow::ensure!(next <= (1 << 53) - 1, "terminal generation exceeds exact JavaScript integers");
        transaction.execute(
            "INSERT INTO runtime_state(key, record) VALUES ('terminal-generation', ?1)
             ON CONFLICT(key) DO UPDATE SET record=excluded.record", [next.to_string()],
        )?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn load_terminals(&self) -> anyhow::Result<std::collections::BTreeMap<SessionId, StoredTerminal>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT session_id, record FROM terminal_runs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, SessionId>(0)?, r.get::<_, String>(1)?)))?;
        let mut records = std::collections::BTreeMap::new();
        for row in rows {
            let (id, json) = row?;
            records.insert(id, serde_json::from_str(&json)?);
        }
        Ok(records)
    }

    pub fn delete_terminal(&self, id: SessionId) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        transaction.execute("DELETE FROM terminal_runs WHERE session_id=?1", [id])?;
        transaction.execute("DELETE FROM reap WHERE session_id=?1", [id])?;
        transaction.commit()?;
        Ok(())
    }

    // ----- events -----

    pub fn insert_event(
        &self,
        channel_id: Option<ChannelId>,
        session_id: Option<SessionId>,
        kind: EventKind,
        data: &serde_json::Value,
        ts: i64,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO events (channel_id, session_id, kind, data, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![channel_id, session_id, enum_to_str(&kind), data.to_string(), ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Retention: delete noise events (`NOISE_EVENT_KINDS`) older than `cutoff_ts`, and all but
    /// the newest `per_channel` noise events of each channel. The channel's history (notes,
    /// prompts, agent posts, approvals, session start/exit, system notices) is never pruned.
    /// Returns the number of rows deleted.
    pub fn prune_events(&self, cutoff_ts: i64, per_channel: usize) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        let kinds = noise_kinds_sql();
        let old = conn.execute(
            &format!("DELETE FROM events WHERE ts < ?1 AND kind IN ({kinds})"),
            [cutoff_ts],
        )?;
        let over = conn.execute(
            &format!(
                "DELETE FROM events WHERE id IN (
                    SELECT id FROM (
                        SELECT id, ROW_NUMBER() OVER (PARTITION BY channel_id ORDER BY id DESC) AS rn
                        FROM events WHERE kind IN ({kinds})
                    ) WHERE rn > ?1
                )"
            ),
            [per_channel as i64],
        )?;
        Ok(old + over)
    }

    /// On session removal: drop its hook and state-change rows (the rest of the timeline,
    /// prompts, posts and so on, stays).
    pub fn delete_session_events(&self, session_id: SessionId) -> anyhow::Result<usize> {
        Ok(self.conn.lock().execute(
            "DELETE FROM events WHERE session_id = ?1 AND kind IN ('hook', 'state_change')",
            [session_id],
        )?)
    }

    pub fn timeline(&self, channel_id: ChannelId, before: Option<i64>, limit: usize) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, channel_id, session_id, kind, data, ts FROM events
             WHERE channel_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![channel_id, before, limit as i64], |r| {
            let kind: String = r.get(3)?;
            let data: String = r.get(4)?;
            Ok(Event {
                id: r.get(0)?,
                channel_id: r.get(1)?,
                session_id: r.get::<_, Option<i64>>(2)?.map(|v| v as SessionId),
                kind: enum_from_str(&kind).unwrap_or(EventKind::System),
                data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
                ts: r.get(5)?,
            })
        })?;
        let mut out: Vec<Event> = rows.collect::<Result<_, _>>()?;
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_references_survive_session_updates_and_workspace_default_changes() {
        let store = Store::open_memory().unwrap();
        let a = store.insert_account_profile("Personal", "/fixture/personal").unwrap();
        let b = store.insert_account_profile("Work", "/fixture/work").unwrap();
        assert!(store.insert_account_profile("personal", "/fixture/another").is_err());
        assert!(store.insert_account_profile("Alias", "/fixture/personal").is_err());
        let ws = store.insert_workspace("repo", "/fixture/repo", false, 1).unwrap();
        assert_eq!(store.load_workspaces().unwrap()[0].account_profile_id, None);
        store.set_workspace_account_profile(ws, Some(a)).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.kind = SessionKind::Claude;
        info.account_profile_id = Some(a);
        info.id = store.insert_session(&StoredSession { info: info.clone(), extra_args: vec![] }).unwrap();
        store.set_workspace_account_profile(ws, Some(b)).unwrap();
        info.name = "renamed session".into();
        store.update_session(&info).unwrap();
        store.update_account_profile(a, Some("Renamed personal"), Some(false)).unwrap();
        assert_eq!(store.load_sessions().unwrap()[0].info.account_profile_id, Some(a));
        assert_eq!(store.load_workspaces().unwrap()[0].account_profile_id, Some(b));
        let profile = store.account_profiles().unwrap().into_iter().find(|p| p.id == a).unwrap();
        assert_eq!(profile.config_dir, "/fixture/personal");
        assert!(!profile.enabled);
    }

    #[test]
    fn profile_migration_keeps_legacy_sessions_on_default() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..12] { conn.execute_batch(sql).unwrap(); }
        conn.pragma_update(None, "user_version", 12).unwrap();
        conn.execute_batch("INSERT INTO sessions(id, channel_id, kind, name, cwd, state, state_since, cols, rows, created_at)
            VALUES (1, 1, 'claude', 'old', '/fixture', 'exited', 0, 80, 24, 0);").unwrap();
        let store = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: std::env::temp_dir().join(format!("muster-store-test-{:x}", rand::random::<u64>())) };
        store.migrate().unwrap();
        assert_eq!(store.load_sessions().unwrap()[0].info.account_profile_id, None);
        assert!(store.account_profiles().unwrap().is_empty());
        store.migrate().unwrap();
        assert_eq!(store.load_sessions().unwrap().len(), 1);
    }

    #[test]
    fn hook_session_state_and_cursor_commit_or_roll_back_together() {
        let store = Store::open_memory().unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.state = AgentState::Idle;
        info.id = store.insert_session(&StoredSession { info: info.clone(), extra_args: vec![] }).unwrap();
        let cursor = HookCursor { instance: "fixture".into(), sequence: 4 };
        let runtime = HookSession { generation: 42, subagents: 1, last_hook_at: 1234, ..Default::default() };
        info.state = AgentState::Working;
        info.state_detail = Some("Agent".into());
        info.state_since = 1234;
        store.conn.lock().execute_batch("CREATE TRIGGER refuse_hook_runtime BEFORE INSERT ON runtime_state
            WHEN NEW.key LIKE 'hook-session:%' BEGIN SELECT RAISE(ABORT, 'fixture failure'); END;").unwrap();
        assert!(store.commit_hook(&serde_json::json!({}), None, None, Some(&cursor), Some((&info, &runtime))).is_err());
        assert_eq!(store.load_sessions().unwrap()[0].info.state, AgentState::Idle);
        assert!(store.load_runtime::<HookCursor>("hook-inbox-cursor").unwrap().is_none());
        assert!(store.load_runtime::<serde_json::Value>("agent-attribution").unwrap().is_none());
        assert!(store.load_hook_session(info.id, 42).unwrap().is_none());
        store.conn.lock().execute_batch("DROP TRIGGER refuse_hook_runtime").unwrap();
        store.commit_hook(&serde_json::json!({}), None, None, Some(&cursor), Some((&info, &runtime))).unwrap();
        let saved = &store.load_sessions().unwrap()[0].info;
        assert_eq!((saved.state, saved.state_since, saved.state_detail.as_deref()),
            (AgentState::Working, 1234, Some("Agent")));
        assert_eq!(store.load_runtime::<HookCursor>("hook-inbox-cursor").unwrap(), Some(cursor));
        let saved = store.load_hook_session(info.id, 42).unwrap().unwrap();
        assert_eq!((saved.subagents, saved.last_hook_at), (1, 1234));
        assert!(store.load_hook_session(info.id, 43).unwrap().is_none());
        store.retire_session(info.id, None).unwrap();
        assert!(store.load_hook_session(info.id, 42).unwrap().is_none());
    }

    #[test]
    fn terminal_generations_survive_record_cleanup_and_replacement() {
        let directory = std::env::temp_dir().join(format!("muster-generations-{}", crate::paths::random_token()));
        let path = directory.join("state.sqlite");
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.next_terminal_generation(42).unwrap(), 43);
            store.conn.lock().execute("DELETE FROM terminal_runs", []).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.next_terminal_generation(0).unwrap(), 44);
        assert_eq!(store.next_terminal_generation(100).unwrap(), 101);
        store.fail_runtime_writes_for_test();
        assert!(store.next_terminal_generation(0).is_err());
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn.lock().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn migration_v4_puts_live_channels_in_window_1() {
        let conn = Connection::open_in_memory().unwrap();
        let s = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: std::env::temp_dir().join(format!("muster-store-test-{:x}", rand::random::<u64>())) };
        // Apply v1..v3 only, add channels the old way, then run the rest.
        {
            let c = s.conn.lock();
            for sql in &MIGRATIONS[..3] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 3).unwrap();
            for (id, ws, order, archived) in [(1, 1, 5, 0), (2, 2, 0, 0), (3, 1, 1, 1), (4, 1, 2, 0)] {
                c.execute(
                    "INSERT INTO channels (id, workspace_id, name, cwd, owns_worktree, port_base, archived,
                                           created_at, last_read_at, sort_order)
                     VALUES (?1, ?2, 'n', '/', 0, 20000, ?4, 0, 0, ?3)",
                    params![id, ws, order, archived],
                )
                .unwrap();
            }
        }
        s.migrate().unwrap();
        let wins = s.load_windows().unwrap();
        assert_eq!(wins.len(), 1);
        assert_eq!((wins[0].id, wins[0].name.as_str()), (1, "1"));
        let chans = s.load_channels().unwrap();
        let live: Vec<(ChannelId, Option<WindowId>, i64, bool)> = chans
            .iter()
            .filter(|c| !c.archived)
            .map(|c| (c.id, c.window_id, c.sort_order, c.auto_named))
            .collect();
        assert_eq!(
            live,
            vec![(1, Some(1), 2, false), (2, Some(1), 0, false), (4, Some(1), 1, false)]
        );
        assert_eq!(chans.iter().find(|c| c.id == 3).unwrap().window_id, None);
        // Round trip of the new columns.
        let mut c = chans[0].clone();
        c.auto_named = true;
        c.first_prompt = Some("fix it".into());
        c.window_id = Some(7);
        s.update_channel(&c).unwrap();
        let back = s.load_channels().unwrap().into_iter().find(|x| x.id == c.id).unwrap();
        assert!(back.auto_named);
        assert_eq!(back.first_prompt.as_deref(), Some("fix it"));
        assert_eq!(back.window_id, Some(7));
        s.upsert_window(2, "two", 5, 6).unwrap();
        s.upsert_window(2, "zwei", 9, 0).unwrap();
        let w = &s.load_windows().unwrap()[1];
        assert_eq!((w.id, w.name.as_str(), w.last_focused_at, w.created_at), (2, "zwei", 9, 6));
        s.delete_window(2).unwrap();
        assert_eq!(s.load_windows().unwrap().len(), 1);
    }

    #[test]
    fn migration_v5_puts_sessions_in_their_channels_window() {
        let conn = Connection::open_in_memory().unwrap();
        let s = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: std::env::temp_dir().join(format!("muster-store-test-{:x}", rand::random::<u64>())) };
        {
            let c = s.conn.lock();
            for sql in &MIGRATIONS[..4] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 4).unwrap();
            c.execute("INSERT INTO windows (id, name, created_at) VALUES (2, '2', 0)", []).unwrap();
            // Channel 1 in window 1, channel 2 in window 2, channel 3 archived (no window).
            for (id, win, archived) in [(1, Some(1), 0), (2, Some(2), 0), (3, None, 1)] {
                c.execute(
                    "INSERT INTO channels (id, workspace_id, name, cwd, owns_worktree, port_base, archived,
                                           created_at, last_read_at, sort_order, window_id)
                     VALUES (?1, 1, 'n', '/', 0, 20000, ?3, 0, 0, 0, ?2)",
                    params![id, win, archived],
                )
                .unwrap();
            }
            for (id, ch) in [(10, 1), (11, 2), (12, 2), (13, 3), (14, 99)] {
                c.execute(
                    "INSERT INTO sessions (id, channel_id, kind, name, cwd, state, state_since, cols, rows, created_at)
                     VALUES (?1, ?2, 'shell', 's', '/', 'exited', 0, 80, 24, 0)",
                    params![id, ch],
                )
                .unwrap();
            }
        }
        s.migrate().unwrap();
        let got: Vec<(SessionId, Option<WindowId>)> =
            s.load_sessions().unwrap().iter().map(|x| (x.info.id, x.info.window_id)).collect();
        assert_eq!(got, vec![(10, Some(1)), (11, Some(2)), (12, Some(2)), (13, None), (14, None)]);
        assert!(s.load_windows().unwrap().iter().all(|w| w.layout.is_none()));
        // Round trips: a session's window, a window's layout (upserts keep it).
        let mut info = s.load_sessions().unwrap()[0].info.clone();
        info.window_id = Some(2);
        s.update_session(&info).unwrap();
        assert_eq!(s.load_sessions().unwrap()[0].info.window_id, Some(2));
        let layout = serde_json::json!({"type": "split", "children": []});
        s.set_window_layout(2, Some(&layout)).unwrap();
        s.upsert_window(2, "two", 3, 0).unwrap();
        let w = s.load_windows().unwrap().into_iter().find(|w| w.id == 2).unwrap();
        assert_eq!((w.name.as_str(), w.layout), ("two", Some(layout)));
        s.set_window_layout(2, None).unwrap();
        assert!(s.load_windows().unwrap().iter().all(|w| w.layout.is_none()));
        let mut new = s.load_sessions().unwrap()[1].clone();
        new.info.window_id = Some(7);
        let id = s.insert_session(&new).unwrap();
        assert_eq!(s.load_sessions().unwrap().iter().find(|x| x.info.id == id).unwrap().info.window_id, Some(7));
    }

    #[test]
    fn migration_v7_creates_agent_tables() {
        use crate::agents::{Agent, AgentMessage, Origin};
        let conn = Connection::open_in_memory().unwrap();
        let s = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: std::env::temp_dir().join(format!("muster-store-test-{:x}", rand::random::<u64>())) };
        {
            let c = s.conn.lock();
            for sql in &MIGRATIONS[..6] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 6).unwrap();
        }
        s.migrate().unwrap();
        assert_eq!(count(&s, "PRAGMA user_version"), MIGRATIONS.len() as i64);
        assert!(s.load_agents().unwrap().is_empty());
        let mut b = Agent {
            account_profile_id: None,
            provider: crate::openrouter::Provider::OpenRouter,
            id: 0,
            name: "Scout".into(),
            kind: SessionKind::Codex,
            model: Some("gpt-6-sol".into()),
            effort: None,
            persona: "Finds things.".into(),
            home_dir: "/data/agents/scout".into(),
            permissions: Permissions::default(),
            accepts_origins: vec![Origin::Human, Origin::Schedule],
            channel_id: None,
            session_id: None,
            resume_id: None,
            created_at: 5,
            autostart: false, sleep_after_min: None, asleep_at: None,
        };
        b.id = s.insert_agent(&b).unwrap();
        // Names are unique case-insensitively.
        let mut dup = b.clone();
        dup.name = "scout".into();
        assert!(s.insert_agent(&dup).is_err());
        s.update_agent_runtime(b.id, Some(8), Some(42), Some("abc-123")).unwrap();
        let back = &s.load_agents().unwrap()[0];
        assert_eq!((back.channel_id, back.session_id, back.resume_id.as_deref()), (Some(8), Some(42), Some("abc-123")));
        assert_eq!(back.accepts_origins, vec![Origin::Human, Origin::Schedule]);
        assert_eq!(back.model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(back.provider, crate::openrouter::Provider::OpenRouter);
        let mut sub = back.clone();
        sub.provider = crate::openrouter::Provider::Subscription;
        s.update_agent_config(&sub).unwrap();
        assert_eq!(s.load_agents().unwrap()[0].provider, crate::openrouter::Provider::Subscription);
        // Rows from before v17 (and anything unreadable) are the subscription.
        s.conn.lock().execute("UPDATE agents SET provider = 'bogus'", []).unwrap();
        assert_eq!(s.load_agents().unwrap()[0].provider, crate::openrouter::Provider::Subscription);
        // Sleep (v24): off by default; the setting is config, asleep is its own write.
        assert_eq!((back.sleep_after_min, back.asleep_at), (None, None));
        s.update_agent_config(&crate::agents::Agent { sleep_after_min: Some(45), ..sub.clone() }).unwrap();
        s.set_agent_asleep(b.id, Some(7_000), Some(3)).unwrap();
        let slept = &s.load_agents().unwrap()[0];
        assert_eq!((slept.sleep_after_min, slept.asleep_at), (Some(45), Some(7_000)));
        assert_eq!(s.agent_asleep_generation(b.id).unwrap(), Some(3));
        s.set_agent_asleep(b.id, None, None).unwrap();
        assert_eq!(s.agent_asleep_generation(b.id).unwrap(), None);
        // An older backend reads only `autostart`: while asleep it reads 0, so a rollback never
        // starts an asleep agent at its own start; this backend still sees the setting.
        let col = |s: &Store| count(s, "SELECT autostart FROM agents");
        s.update_agent_config(&crate::agents::Agent { autostart: true, ..sub.clone() }).unwrap();
        assert_eq!(col(&s), 1);
        s.set_agent_asleep(b.id, Some(8_000), None).unwrap();
        assert_eq!(col(&s), 0);
        let asleep = s.load_agents().unwrap()[0].clone();
        assert!(asleep.autostart);
        // A config change while asleep keeps the column at 0 (and the setting).
        s.update_agent_config(&crate::agents::Agent { persona: "Asleep.".into(), ..asleep.clone() }).unwrap();
        assert_eq!(col(&s), 0);
        assert!(s.load_agents().unwrap()[0].autostart);
        s.update_agent_config(&crate::agents::Agent { autostart: false, ..asleep.clone() }).unwrap();
        assert!(!s.load_agents().unwrap()[0].autostart);
        s.update_agent_config(&crate::agents::Agent { autostart: true, ..asleep.clone() }).unwrap();
        // Awake: the column holds the setting again.
        s.set_agent_asleep(b.id, None, None).unwrap();
        assert_eq!(col(&s), 1);
        assert!(s.load_agents().unwrap()[0].autostart);
        // An older backend that turned autostart on while it was asleep keeps it on.
        s.update_agent_config(&crate::agents::Agent { autostart: false, ..asleep.clone() }).unwrap();
        s.set_agent_asleep(b.id, Some(9_000), None).unwrap();
        s.conn.lock().execute("UPDATE agents SET autostart = 1", []).unwrap();
        assert!(s.load_agents().unwrap()[0].autostart);
        s.set_agent_asleep(b.id, None, None).unwrap();
        assert_eq!(col(&s), 1);
        s.update_agent_config(&crate::agents::Agent { autostart: false, asleep_at: None, ..asleep.clone() }).unwrap();
        s.conn.lock().execute("UPDATE agents SET sleep_after_min = 1", []).unwrap();
        let back2 = &s.load_agents().unwrap()[0];
        assert_eq!((back2.sleep_after_min, back2.asleep_at), (Some(crate::sleep::AFTER_MIN), None));

        let m = AgentMessage {
            id: 0,
            from: "human".into(),
            from_session: None,
            to_agent: b.id,
            to_session: Some(42),
            origin: Origin::Schedule,
            hop: 2,
            reply_to: Some(9),
            created_at: 1_000,
            reply: None,
            replied_at: None,
            text: None,
            refused: None,
            to_name: None,
            cold_idle_ms: None,
        };
        let id = s.insert_agent_message(&m).unwrap();
        s.insert_agent_message(&AgentMessage { created_at: 10, ..m.clone() }).unwrap();
        assert_eq!(s.count_agent_messages_since(b.id, 500).unwrap(), 1);
        assert_eq!(s.count_agent_messages_since(b.id, 0).unwrap(), 2);
        s.set_agent_message_reply(id, "done", 2_000).unwrap();
        let got = s.agent_message(id).unwrap().unwrap();
        assert_eq!((got.origin, got.hop, got.reply_to), (Origin::Schedule, 2, Some(9)));
        assert_eq!(got.reply.as_deref(), Some("done"));
        s.delete_agent_message(id).unwrap();
        assert!(s.agent_message(id).unwrap().is_none());
        // The cap is checked with the insert.
        let fresh = AgentMessage { created_at: 5_000, ..m.clone() };
        assert_eq!(s.insert_agent_message_capped(&fresh, 4_000, 1, None).unwrap().map(|_| ()), Ok(()));
        assert_eq!(s.insert_agent_message_capped(&fresh, 4_000, 1, None).unwrap(), Err(CapHit::All(1)));
        assert_eq!(s.count_agent_messages_since(b.id, 4_000).unwrap(), 1);
        s.delete_agent(b.id).unwrap();
        assert!(s.load_agents().unwrap().is_empty());
    }

    #[test]
    fn migration_v9_renames_bots_to_agents_on_a_populated_db() {
        use crate::agents::Origin;
        let conn = Connection::open_in_memory().unwrap();
        let s = Store { conn: std::sync::Arc::new(Mutex::new(conn)), directory: std::env::temp_dir().join(format!("muster-store-test-{:x}", rand::random::<u64>())) };
        {
            let c = s.conn.lock();
            for sql in &MIGRATIONS[..8] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 8).unwrap();
            c.execute(
                "INSERT INTO bots (id, name, kind, model, persona, home_dir, permissions, accepts_origins, session_id,
                                   resume_id, created_at, channel_id)
                 VALUES (3, 'Ada', 'claude', 'opus', 'Reviews.', '/d/bots/ada', '{\"claude_mode\":\"plan\"}',
                         '[\"human\"]', 7, 'conv-1', 5, 4)",
                [],
            )
            .unwrap();
            for (id, at, reply) in [(1, 100, Some("ok")), (2, 200, None)] {
                c.execute(
                    "INSERT INTO bot_messages (id, from_label, to_bot, to_session, origin, hop, created_at, reply)
                     VALUES (?1, 'human', 3, 7, 'human', 0, ?2, ?3)",
                    params![id, at, reply],
                )
                .unwrap();
            }
        }
        s.migrate().unwrap();
        assert_eq!(count(&s, "PRAGMA user_version"), MIGRATIONS.len() as i64);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('bots', 'bot_messages', 'bot_messages_to')"), 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'agent_messages_to'"), 1);
        // Every field survives; new ones get their defaults.
        let a = &s.load_agents().unwrap()[0];
        assert_eq!((a.id, a.name.as_str(), a.model.as_deref(), a.home_dir.as_str()), (3, "Ada", Some("opus"), "/d/bots/ada"));
        assert_eq!((a.session_id, a.channel_id, a.resume_id.as_deref()), (Some(7), Some(4), Some("conv-1")));
        assert_eq!(a.permissions.claude_mode, ClaudeMode::Plan);
        assert!(!a.autostart);
        let m = s.agent_message(1).unwrap().unwrap();
        assert_eq!((m.to_agent, m.reply.as_deref(), m.text.as_deref(), m.refused.as_deref()), (3, Some("ok"), None, None));
        assert_eq!(m.to_name.as_deref(), Some("Ada"));
        assert_eq!(s.count_agent_messages_since(3, 0).unwrap(), 2);
        // Refused messages are kept for the log but never count toward the cap.
        let mut r = m.clone();
        r.text = Some("from a cron".into());
        r.origin = Origin::Schedule;
        r.created_at = 300;
        let rid = s.insert_refused_agent_message(&r, "Ada accepts only human").unwrap();
        assert_eq!(s.count_agent_messages_since(3, 0).unwrap(), 2);
        assert_eq!(s.insert_agent_message_capped(&r, 0, 2, None).unwrap(), Err(CapHit::All(2)));
        // Newest first, filtered, paged.
        let ids = |v: Vec<crate::agents::AgentMessage>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(s.agent_messages(None, None, 10).unwrap()), vec![rid, 2, 1]);
        assert_eq!(ids(s.agent_messages(Some(3), Some(rid), 1).unwrap()), vec![2]);
        assert!(s.agent_messages(Some(99), None, 10).unwrap().is_empty());
        assert_eq!(s.agent_message(rid).unwrap().unwrap().refused.as_deref(), Some("Ada accepts only human"));
        // Config round trip.
        let mut a = a.clone();
        a.autostart = true;
        a.persona = "New.".into();
        s.update_agent_config(&a).unwrap();
        let back = &s.load_agents().unwrap()[0];
        assert!(back.autostart && back.persona == "New." && back.session_id == Some(7));
        // Nothing left to do on the next open.
        s.migrate().unwrap();
        assert_eq!(s.load_agents().unwrap().len(), 1);
    }

    #[test]
    fn refused_messages_are_pruned_by_age_and_count() {
        let s = Store::open_memory().unwrap();
        let m = crate::agents::AgentMessage {
            id: 0,
            from: "human".into(),
            from_session: None,
            to_agent: 1,
            to_session: None,
            origin: crate::agents::Origin::Human,
            hop: 0,
            reply_to: None,
            created_at: 100,
            reply: None,
            replied_at: None,
            text: Some("x".into()),
            refused: None,
            to_name: None,
            cold_idle_ms: None,
        };
        let delivered = s.insert_agent_message(&m).unwrap();
        for at in [100, 200, 300, 400, 500] {
            s.insert_refused_agent_message(&crate::agents::AgentMessage { created_at: at, ..m.clone() }, "no").unwrap();
        }
        // Older than 250 goes; of the rest, only the newest 2 stay. Delivered rows never go.
        assert_eq!(s.prune_refused_agent_messages(250, 2).unwrap(), 3);
        let left: Vec<i64> = s.agent_messages(None, None, 10).unwrap().iter().map(|m| m.created_at).collect();
        assert_eq!(left, vec![500, 400, 100]);
        assert!(s.agent_message(delivered).unwrap().is_some());
    }

    #[test]
    fn concurrent_capped_inserts_never_overshoot() {
        let s = Store::open_memory().unwrap();
        let m = crate::agents::AgentMessage {
            id: 0,
            from: "human".into(),
            from_session: None,
            to_agent: 1,
            to_session: Some(1),
            origin: crate::agents::Origin::Human,
            hop: 0,
            reply_to: None,
            created_at: 100,
            reply: None,
            replied_at: None,
            text: None,
            refused: None,
            to_name: None,
            cold_idle_ms: None,
        };
        let ok = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..16 {
                sc.spawn(|| {
                    for _ in 0..5 {
                        if s.insert_agent_message_capped(&m, 0, 7, None).unwrap().is_ok() {
                            ok.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(ok.into_inner(), 7);
        assert_eq!(s.count_agent_messages_since(1, 0).unwrap(), 7);
    }

    #[test]
    fn event_retention() {
        let s = Store::open_memory().unwrap();
        let v = serde_json::json!({"x": 1});
        // Channel 1: 10 old + 20 new hook events; channel 2: 3 new; no channel: 2 old.
        for i in 0..10 {
            s.insert_event(Some(1), None, EventKind::Hook, &v, 100 + i).unwrap();
        }
        for i in 0..20 {
            s.insert_event(Some(1), None, EventKind::StateChange, &v, 10_000 + i)
                .unwrap();
        }
        for i in 0..3 {
            s.insert_event(Some(2), None, EventKind::McpCall, &v, 10_000 + i)
                .unwrap();
        }
        for i in 0..2 {
            s.insert_event(None, None, EventKind::Hook, &v, 50 + i).unwrap();
        }
        let n = s.prune_events(5_000, 15).unwrap();
        assert_eq!(n, 12 + 5);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM events WHERE channel_id = 1"), 15);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM events WHERE channel_id = 2"), 3);
        // The newest ones of channel 1 are kept.
        assert_eq!(count(&s, "SELECT MIN(ts) FROM events WHERE channel_id = 1"), 10_005);
        assert_eq!(s.timeline(1, None, 100).unwrap().len(), 15);
    }

    #[test]
    fn retention_never_prunes_history() {
        let s = Store::open_memory().unwrap();
        let v = serde_json::json!({});
        let history = [
            EventKind::Note,
            EventKind::Prompt,
            EventKind::AgentPost,
            EventKind::Approval,
            EventKind::SessionStarted,
            EventKind::SessionExited,
            EventKind::System,
        ];
        // Old history, then far more (old and new) noise than the per-channel cap.
        for k in history {
            s.insert_event(Some(1), Some(3), k, &v, 1).unwrap();
        }
        for i in 0..50 {
            s.insert_event(
                Some(1),
                Some(3),
                NOISE_EVENT_KINDS[i % 3],
                &v,
                if i < 25 { 2 } else { 10_000 },
            )
            .unwrap();
        }
        s.prune_events(5_000, 5).unwrap();
        let left = s.timeline(1, None, 1000).unwrap();
        for k in history {
            assert!(left.iter().any(|e| e.kind == k), "{k:?} was pruned");
        }
        let noise = left.iter().filter(|e| NOISE_EVENT_KINDS.contains(&e.kind)).count();
        assert_eq!(noise, 5);
        assert_eq!(left.len(), history.len() + 5);
    }

    #[test]
    fn session_events_removed_with_session() {
        let s = Store::open_memory().unwrap();
        let v = serde_json::json!({});
        s.insert_event(Some(1), Some(7), EventKind::Hook, &v, 1).unwrap();
        s.insert_event(Some(1), Some(7), EventKind::StateChange, &v, 2).unwrap();
        s.insert_event(Some(1), Some(7), EventKind::Prompt, &v, 3).unwrap();
        s.insert_event(Some(1), Some(8), EventKind::Hook, &v, 4).unwrap();
        assert_eq!(s.delete_session_events(7).unwrap(), 2);
        let left: Vec<_> = s
            .timeline(1, None, 10)
            .unwrap()
            .into_iter()
            .map(|e| (e.session_id, e.kind))
            .collect();
        assert_eq!(left, vec![(Some(7), EventKind::Prompt), (Some(8), EventKind::Hook)]);
    }

    #[test]
    fn sids_roundtrip() {
        let s = Store::open_memory().unwrap();
        assert!(s.session_sids().unwrap().is_empty());
        for id in [3, 4] {
            s.conn
                .lock()
                .execute(
                    "INSERT INTO sessions (id, channel_id, kind, name, cwd, state, state_since, cols, rows, created_at)
                     VALUES (?1, 1, 'shell', 's', '/', 'exited', 0, 80, 24, 0)",
                    [id],
                )
                .unwrap();
        }
        let m = vec!["MUSTER_RUN=ab".to_string(), "MUSTER_SESSION_ID=3".to_string()];
        s.set_session_sid(3, Some(4242), &m).unwrap();
        s.set_session_sid(4, Some(4343), &m[..1]).unwrap();
        s.add_reap(7, 4444, &m).unwrap();
        let mut got = s.session_sids().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![(3, 4242, m.clone()), (4, 4343, m[..1].to_vec()), (7, 4444, m.clone())]
        );
        // Legacy single-marker rows still parse.
        s.conn
            .lock()
            .execute("UPDATE sessions SET run_marker = 'MUSTER_TOKEN=x' WHERE id = 4", [])
            .unwrap();
        assert!(
            s.session_sids()
                .unwrap()
                .contains(&(4, 4343, vec!["MUSTER_TOKEN=x".to_string()]))
        );
        // Survivors are kept for the next attempt.
        s.clear_session_sids(&[4, 7].into_iter().collect()).unwrap();
        let ids: Vec<SessionId> = s.session_sids().unwrap().into_iter().map(|r| r.0).collect();
        assert_eq!(ids, vec![4, 7]);
        s.remove_reap(7, 4444).unwrap();
        // A removed session's surviving record moves to the reap list.
        s.keep_sid_for_reap(4).unwrap();
        s.delete_session(4).unwrap();
        assert_eq!(
            s.session_sids().unwrap(),
            vec![(4, 4343, vec!["MUSTER_TOKEN=x".to_string()])]
        );
        s.keep_sid_for_reap(3).unwrap(); // no sid recorded: nothing to keep
        assert_eq!(s.session_sids().unwrap().len(), 1);
        s.clear_session_sids(&Default::default()).unwrap();
        assert!(s.session_sids().unwrap().is_empty());
    }
}
