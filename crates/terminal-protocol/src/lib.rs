//! Versioned terminal-only wire contract. No application state or PTY ownership.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub mod client;
pub mod wire;
pub type SessionId = u32;
pub const VERSION: u32 = 1;
pub const MAX_INPUT: usize = 1024 * 1024;
/// Host exit status: another terminal host owns the runtime directory (it may be starting).
pub const EXIT_HOST_BUSY: i32 = 75;
/// Host exit status: its fence lock is held by another process.
pub const EXIT_FENCE_BUSY: i32 = 76;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId {
    pub id: SessionId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub run: RunId,
    pub program: String,
    pub args: Vec<String>,
    /// Complete environment; the long-lived host does not supply stale inherited values.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub instance: String,
    pub pid: u32,
    pub data_dir: PathBuf,
    /// The lock file this host holds for its whole life, if it was given one. Taken before
    /// the host answers anyone, so a host that answers holds it. Absent from older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<FenceLock>,
}

/// A held lock file, identified by its device and inode so a caller can tell that the
/// file it sees at `path` is the one locked (not a replacement created after deletion).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceLock {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalStatus {
    pub run: RunId,
    pub pid: Option<u32>,
    pub alive: bool,
    pub drained: bool,
    pub has_processes: bool,
    pub exit_code: Option<i32>,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub output_seq: u64,
    pub last_output_at: i64,
    pub application_cursor: bool,
    pub log_persisted: bool,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TerminalEvent {
    Title {
        id: SessionId,
        generation: u64,
        title: Option<String>,
    },
    Exited {
        id: SessionId,
        generation: u64,
        code: i32,
    },
    Drained {
        id: SessionId,
        generation: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub instance: Option<String>,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Command {
    Hello,
    Inventory,
    Spawn(SpawnSpec),
    Status {
        run: RunId,
    },
    Input {
        run: RunId,
        data: Vec<u8>,
    },
    Resize {
        run: RunId,
        cols: u16,
        rows: u16,
    },
    Screen {
        run: RunId,
    },
    RecentText {
        run: RunId,
        lines: usize,
    },
    Terminate {
        run: RunId,
        force: bool,
    },
    /// Only succeeds after the run and every descendant have exited.
    Forget {
        run: RunId,
    },
    Attach {
        run: RunId,
        replay_bytes: Option<usize>,
    },
    Events,
    /// A persistent input connection: after `Attached`, each further frame is one
    /// `Command::Input`, applied in the order received and answered with `Ok` or `Error`.
    /// A lost connection leaves the outcome of unanswered input unknown; clients never
    /// resend it.
    InputStream,
    Flush,
    /// Explicit full shutdown; backend disconnect never implies this operation.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Hello(Hello),
    Inventory(Vec<TerminalStatus>),
    Status(TerminalStatus),
    Screen(ScreenSnapshot),
    Text(String),
    Attached,
    Event(TerminalEvent),
    Resync,
    Ok,
    Error { code: String, message: String },
}

/// Plain-text view of the terminal for pattern matching.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// Visible rows, trailing whitespace trimmed.
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cols: u16,
    pub rows: u16,
    /// Unix ms of the last PTY output.
    pub last_output_at: i64,
    /// True if a foreground process other than the session's own process owns the TTY
    /// (only meaningful for shells).
    pub foreground_busy: bool,
}

impl ScreenSnapshot {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    pub fn contains(&self, needle: &str) -> bool {
        self.lines.iter().any(|l| l.contains(needle))
    }
}
