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

pub struct Store {
    conn: Mutex<Connection>,
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
];

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
        let store = Store { conn: Mutex::new(conn) };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_memory() -> anyhow::Result<Self> {
        let store = Store {
            conn: Mutex::new(Connection::open_in_memory()?),
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

    // ----- workspaces -----

    pub fn load_workspaces(&self) -> anyhow::Result<Vec<Workspace>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT id, name, path, is_git, created_at FROM workspaces ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                is_git: r.get::<_, i64>(3)? != 0,
                created_at: r.get(4)?,
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
        let conn = self.conn.lock();
        conn.execute(
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
        Ok(conn.last_insert_rowid())
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

    pub fn delete_window(&self, id: WindowId) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM windows WHERE id = ?1", [id])?;
        Ok(())
    }

    // ----- sessions -----

    pub fn load_sessions(&self) -> anyhow::Result<Vec<StoredSession>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, channel_id, kind, name, cwd, state, state_detail, state_since, title, parent_id, pid,
                    exit_code, cols, rows, queued, extra_args, created_at, window_id FROM sessions ORDER BY id",
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
                    created_at: r.get(16)?,
                    window_id: r.get(17)?,
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
                                   parent_id, pid, exit_code, cols, rows, queued, extra_args, created_at, window_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                i.window_id
            ],
        )?;
        let id = conn.last_insert_rowid();
        u32::try_from(id).map_err(|_| anyhow::anyhow!("session id space exhausted"))
    }

    pub fn update_session(&self, i: &SessionInfo) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET name=?2, cwd=?3, state=?4, state_detail=?5, state_since=?6, title=?7, pid=?8,
                    exit_code=?9, cols=?10, rows=?11, queued=?12, channel_id=?13, window_id=?14 WHERE id=?1",
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
                i.window_id
            ],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: SessionId) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
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

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn.lock().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn migration_v4_puts_live_channels_in_window_1() {
        let conn = Connection::open_in_memory().unwrap();
        let s = Store { conn: Mutex::new(conn) };
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
        let s = Store { conn: Mutex::new(conn) };
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
