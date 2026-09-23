//! SQLite persistence (rusqlite, bundled). The Hub keeps everything in memory and writes
//! through to this store; the store is only read in bulk at daemon start (and for timelines).
//!
//! Lock order: callers may hold a Hub lock while calling into the store, never the reverse.

use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::*;

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
];

fn enum_to_str<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
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
        })
    }

    pub fn load_channels(&self) -> anyhow::Result<Vec<Channel>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, workspace_id, name, cwd, branch, base_branch, owns_worktree, port_base, archived,
                    layout, created_at, last_read_at, sort_order FROM channels ORDER BY id",
        )?;
        let rows = st.query_map([], Self::row_channel)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts the channel (its `id` is ignored) and returns the new id.
    pub fn insert_channel(&self, c: &Channel) -> anyhow::Result<ChannelId> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO channels (workspace_id, name, cwd, branch, base_branch, owns_worktree, port_base,
                                   archived, layout, created_at, last_read_at, sort_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                c.sort_order
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_channel(&self, c: &Channel) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE channels SET workspace_id=?2, name=?3, cwd=?4, branch=?5, base_branch=?6, owns_worktree=?7,
                    port_base=?8, archived=?9, layout=?10, created_at=?11, last_read_at=?12, sort_order=?13
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
                c.sort_order
            ],
        )?;
        Ok(())
    }

    // ----- sessions -----

    pub fn load_sessions(&self) -> anyhow::Result<Vec<StoredSession>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT id, channel_id, kind, name, cwd, state, state_detail, state_since, title, parent_id, pid,
                    exit_code, cols, rows, queued, extra_args, created_at FROM sessions ORDER BY id",
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
                                   parent_id, pid, exit_code, cols, rows, queued, extra_args, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                i.created_at
            ],
        )?;
        let id = conn.last_insert_rowid();
        u32::try_from(id).map_err(|_| anyhow::anyhow!("session id space exhausted"))
    }

    pub fn update_session(&self, i: &SessionInfo) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET name=?2, cwd=?3, state=?4, state_detail=?5, state_since=?6, title=?7, pid=?8,
                    exit_code=?9, cols=?10, rows=?11, queued=?12 WHERE id=?1",
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
                i.queued
            ],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: SessionId) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Sessions that were running when the previous daemon stopped: (id, pid).
    pub fn running_pids(&self) -> anyhow::Result<Vec<(SessionId, u32)>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT id, pid FROM sessions WHERE state != 'exited' AND pid IS NOT NULL")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as SessionId, r.get::<_, i64>(1)? as u32))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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
