//! Application-owned callback inbox. Hook helpers append while the backend is absent;
//! PTY ownership and terminal-host IPC never depend on this database.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::store::HookCursor;

pub const MAX_PENDING_BYTES: i64 = 64 * 1024 * 1024;
pub const MAX_PENDING_EVENTS: i64 = 100_000;

pub struct Inbox {
    connection: Connection,
    instance: String,
}

pub struct Hook {
    pub cursor: HookCursor,
    pub token: String,
    pub event: String,
    pub payload: serde_json::Value,
    /// When the helper saved it (unix ms); None from helpers older than the column.
    pub captured_at: Option<i64>,
}

pub fn path() -> PathBuf {
    crate::paths::data_dir().join("hook-inbox.sqlite")
}

impl Inbox {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = path.parent() {
            crate::paths::ensure_private_dir(parent)?;
        }
        // Establish restrictive permissions before SQLite opens a newly created file.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(500))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS inbox_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS hooks(id INTEGER PRIMARY KEY AUTOINCREMENT,
                token TEXT NOT NULL, event TEXT NOT NULL, payload TEXT NOT NULL);",
        )?;
        // When each callback was captured: one replayed after an outage happened then (the
        // cold-cache clock, src/cache.rs). Older helpers leave it NULL.
        let has_captured: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('hooks') WHERE name='captured_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_captured {
            // Two openers may race here; the loser's duplicate-column error is harmless.
            if let Err(error) = connection.execute_batch("ALTER TABLE hooks ADD COLUMN captured_at INTEGER") {
                if !error.to_string().contains("duplicate column") {
                    return Err(error.into());
                }
            }
        }
        connection.execute(
            "INSERT OR IGNORE INTO inbox_meta(key,value) VALUES ('instance',?1)",
            [crate::paths::random_token()],
        )?;
        let instance = connection.query_row(
            "SELECT value FROM inbox_meta WHERE key='instance'",
            [],
            |row| row.get(0),
        )?;
        for suffix in ["", "-wal", "-shm"] {
            crate::paths::make_private_file(Path::new(&format!("{}{suffix}", path.display())));
        }
        Ok(Self {
            connection,
            instance,
        })
    }

    pub fn enqueue(&mut self, token: &str, event: &str, payload: &[u8]) -> anyhow::Result<()> {
        self.enqueue_bounded(token, event, payload, MAX_PENDING_BYTES)
    }

    fn enqueue_bounded(
        &mut self,
        token: &str,
        event: &str,
        payload: &[u8],
        max_bytes: i64,
    ) -> anyhow::Result<()> {
        let payload: serde_json::Value = serde_json::from_slice(payload)?;
        let payload = serde_json::to_string(&payload)?;
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (count, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(CAST(payload AS BLOB))),0) FROM hooks",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count >= MAX_PENDING_EVENTS || bytes + payload.len() as i64 > max_bytes {
            tx.execute(
                "INSERT INTO inbox_meta(key,value) VALUES ('dropped','1')
                ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",
                [],
            )?;
            tx.commit()?;
            anyhow::bail!("callback inbox is full; overflow recorded for the backend");
        }
        tx.execute(
            "INSERT INTO hooks(token,event,payload,captured_at) VALUES (?1,?2,?3,?4)",
            params![token, event, payload, crate::model::now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn next(&self, cursor: Option<&HookCursor>) -> anyhow::Result<Option<Hook>> {
        let after = cursor
            .filter(|cursor| cursor.instance == self.instance)
            .map_or(0, |cursor| cursor.sequence);
        let row: Option<(i64, String, String, String, Option<i64>)> = self
            .connection
            .query_row(
                "SELECT id,token,event,payload,captured_at FROM hooks WHERE id>?1 ORDER BY id LIMIT 1",
                [after],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        row.map(|(sequence, token, event, payload, captured_at)| {
            Ok(Hook {
                cursor: HookCursor {
                    instance: self.instance.clone(),
                    sequence,
                },
                token,
                event,
                payload: serde_json::from_str(&payload).context("reading stored hook payload")?,
                captured_at,
            })
        })
        .transpose()
    }

    pub fn acknowledge(&self, cursor: &HookCursor) -> anyhow::Result<()> {
        anyhow::ensure!(
            cursor.instance == self.instance,
            "callback inbox instance changed"
        );
        self.connection
            .execute("DELETE FROM hooks WHERE id<=?1", [cursor.sequence])?;
        Ok(())
    }

    pub fn dropped(&self) -> anyhow::Result<u64> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM inbox_meta WHERE key='dropped'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.map(|value| value.parse()).transpose()?.unwrap_or(0))
    }

    /// Establish an input boundary in the same ordering domain as hook inserts.
    /// The caller holds the backend's callback mutex. Drain without a writer lock
    /// first, then briefly prevent new inserts while committing the new attribution.
    /// If more callbacks arrived, leave attribution unchanged and let the caller retry.
    pub fn input_boundary<T>(
        &mut self,
        hub: &crate::hub::Hub,
        action: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        drain(hub, self)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let pending: bool =
            transaction.query_row("SELECT EXISTS(SELECT 1 FROM hooks)", [], |row| row.get(0))?;
        anyhow::ensure!(
            !pending,
            "agent callbacks are still catching up; input attribution was not changed"
        );
        let result = action()?;
        transaction.commit()?;
        Ok(result)
    }
}

/// Process in insertion order. The application transaction advances the cursor with
/// attribution before the inbox row is removed; a lost deletion is harmless on restart.
pub fn drain(hub: &crate::hub::Hub, inbox: &Inbox) -> anyhow::Result<()> {
    let mut cursor: Option<HookCursor> = hub.store().load_runtime("hook-inbox-cursor")?;
    if let Some(cursor) = &cursor {
        if cursor.instance == inbox.instance {
            inbox.acknowledge(cursor)?;
        }
    }
    for _ in 0..512 {
        let Some(hook) = inbox.next(cursor.as_ref())? else {
            break;
        };
        hub.handle_spooled_hook(&hook.token, &hook.event, hook.payload, &hook.cursor, hook.captured_at)?;
        inbox.acknowledge(&hook.cursor)?;
        cursor = Some(hook.cursor);
    }
    let dropped = inbox.dropped()?;
    let reported: Option<(String, u64)> = hub.store().load_runtime("hook-inbox-overflow")?;
    if dropped > 0 && reported.as_ref() != Some(&(inbox.instance.clone(), dropped)) {
        tracing::error!(
            dropped,
            "callback inbox overflowed; inspect agent replies and session state"
        );
        hub.log_event(None, None, crate::model::EventKind::System, serde_json::json!({
            "level": "error", "text": format!("Callback inbox overflowed: {dropped} callbacks could not be saved"),
        }));
        hub.notice("error", format!("Callback inbox overflowed while the backend was unavailable: {dropped} callbacks could not be saved. Check agent replies and session state."));
        hub.store()
            .save_runtime("hook-inbox-overflow", &(inbox.instance.clone(), dropped))?;
    }
    Ok(())
}

pub fn spawn(hub: &std::sync::Arc<crate::hub::Hub>, inbox: Inbox) -> anyhow::Result<()> {
    // Complete the bounded recovery batch before detectors, queue workers or autostart.
    let mut guard = hub.callback_inbox.lock();
    *guard = Some(inbox);
    let inbox = guard.as_ref().expect("installed callback inbox");
    while inbox
        .next(
            hub.store()
                .load_runtime::<HookCursor>("hook-inbox-cursor")?
                .as_ref(),
        )?
        .is_some()
    {
        drain(hub, inbox)?;
    }
    drop(guard);
    let weak = std::sync::Arc::downgrade(hub);
    std::thread::Builder::new()
        .name("hook-inbox".into())
        .spawn(move || {
            loop {
                let Some(hub) = weak.upgrade() else { break };
                if hub.is_shutting_down() {
                    break;
                }
                {
                    let guard = hub.callback_inbox.lock();
                    if let Some(inbox) = guard.as_ref() {
                        if let Err(error) = drain(&hub, inbox) {
                            tracing::warn!("callback inbox pending recovery: {error:#}");
                        }
                    }
                }
                drop(hub);
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_keep_when_they_were_captured_also_in_an_older_inbox() {
        let dir = std::env::temp_dir().join(format!("muster-hook-inbox-{}", rand::random::<u64>()));
        let path = dir.join("inbox.sqlite");
        std::fs::create_dir_all(&dir).unwrap();
        {
            // An inbox from before the column, with a row an older helper wrote.
            let old = Connection::open(&path).unwrap();
            old.execute_batch("CREATE TABLE hooks(id INTEGER PRIMARY KEY AUTOINCREMENT,
                token TEXT NOT NULL, event TEXT NOT NULL, payload TEXT NOT NULL);
                INSERT INTO hooks(token,event,payload) VALUES ('t','Stop','{}');").unwrap();
        }
        let before = crate::model::now_ms();
        let mut inbox = Inbox::open(&path).unwrap();
        let old = inbox.next(None).unwrap().unwrap();
        assert_eq!(old.captured_at, None, "an older helper's row has no capture time");
        inbox.enqueue("t", "Stop", b"{}").unwrap();
        let new = inbox.next(Some(&old.cursor)).unwrap().unwrap();
        assert!(new.captured_at.is_some_and(|t| t >= before));
        drop(inbox);
        Inbox::open(&path).unwrap(); // opening again: the column is already there
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn callbacks_survive_reopen_and_acknowledged_ids_are_never_reused() {
        let dir = std::env::temp_dir().join(format!("muster-hook-inbox-{}", rand::random::<u64>()));
        let path = dir.join("inbox.sqlite");
        let mut original = Inbox::open(&path).unwrap();
        original
            .enqueue(
                "fixture-token",
                "Stop",
                br#"{"last_assistant_message":"saved"}"#,
            )
            .unwrap();
        let first = original.next(None).unwrap().unwrap();
        drop(original);
        let mut replacement = Inbox::open(&path).unwrap();
        let replay = replacement.next(None).unwrap().unwrap();
        assert_eq!(replay.cursor, first.cursor);
        assert_eq!(replay.payload["last_assistant_message"], "saved");
        assert!(
            replacement.next(Some(&first.cursor)).unwrap().is_none(),
            "saved application cursor suppresses a repeated inbox row"
        );
        replacement.acknowledge(&first.cursor).unwrap();
        replacement
            .enqueue("fixture-token", "Stop", br#"{}"#)
            .unwrap();
        let second = replacement.next(Some(&first.cursor)).unwrap().unwrap();
        assert!(second.cursor.sequence > first.cursor.sequence);
        assert_eq!(second.cursor.instance, first.cursor.instance);
        assert!(
            replacement
                .enqueue_bounded("fixture-token", "Stop", br#"{"extra":"too much"}"#, 3)
                .is_err()
        );
        assert_eq!(
            replacement.dropped().unwrap(),
            1,
            "overflow is recorded instead of silently dropping callbacks"
        );
        drop(replacement);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
