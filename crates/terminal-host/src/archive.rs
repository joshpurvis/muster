//! Final terminal state on disk. Only small lifecycle metadata stays resident;
//! replay bytes, screen rows and plain-text scrollback are loaded on demand.
use std::path::PathBuf;

use muster_terminal_core::{ScreenSnapshot, private_fs, pty::PtySession, replay};
use muster_terminal_protocol::TerminalStatus;

pub struct Archive {
    pub status: TerminalStatus,
    directory: PathBuf,
}

impl Archive {
    pub fn save(
        directory: PathBuf,
        mut status: TerminalStatus,
        pty: &PtySession,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !status.alive && status.drained && !status.has_processes,
            "terminal is not ready to archive"
        );
        pty.flush_log_final();
        anyhow::ensure!(pty.log_durable(), "terminal log is not durably saved");
        status.log_persisted = true;
        // Rendered once here; attaches only cut it to their limit.
        let (replay, offset) = pty.ended_replay();
        private_fs::write_private(&directory.join("replay.bin"), &replay)?;
        private_fs::write_private(
            &directory.join("replay.json"),
            &serde_json::to_vec(&serde_json::json!({ "offset": offset }))?,
        )?;
        private_fs::write_private(
            &directory.join("screen.json"),
            &serde_json::to_vec(&pty.screen())?,
        )?;
        private_fs::write_private(
            &directory.join("text.txt"),
            pty.recent_text(10000).as_bytes(),
        )?;
        private_fs::write_private(
            &directory.join("archive.json"),
            &serde_json::to_vec(&status)?,
        )?;
        std::fs::File::open(&directory)?.sync_all()?;
        Ok(Self { status, directory })
    }

    pub fn replay(&self, limit: Option<usize>) -> anyhow::Result<(Vec<u8>, u64)> {
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.directory.join("replay.json"))?)?;
        let offset = meta["offset"].as_u64().ok_or_else(|| anyhow::anyhow!("replay.json has no offset"))?;
        let replay = std::fs::read(self.directory.join("replay.bin"))?;
        Ok((replay::trim_replay(&replay, limit), offset))
    }

    pub fn screen(&self) -> anyhow::Result<ScreenSnapshot> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.directory.join("screen.json"),
        )?)?)
    }

    pub fn recent_text(&self, lines: usize) -> anyhow::Result<String> {
        if lines == 0 {
            return Ok(String::new());
        }
        let text = std::fs::read_to_string(self.directory.join("text.txt"))?;
        let mut tail: Vec<_> = text.rsplit('\n').take(lines).collect();
        tail.reverse();
        Ok(tail.join("\n"))
    }
}
