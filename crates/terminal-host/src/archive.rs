//! Final terminal state on disk. Only small lifecycle metadata stays resident;
//! replay bytes, screen rows and plain-text scrollback are loaded on demand.
use std::path::PathBuf;

use muster_terminal_core::{
    ScreenSnapshot, private_fs,
    pty::{PtySession, ReplaySnapshot},
};
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
        let replay = pty.replay_snapshot();
        private_fs::write_private(&directory.join("replay.bin"), &replay.body)?;
        private_fs::write_private(
            &directory.join("replay.json"),
            &serde_json::to_vec(&replay)?,
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
        let mut snapshot: ReplaySnapshot =
            serde_json::from_slice(&std::fs::read(self.directory.join("replay.json"))?)?;
        snapshot.body = std::fs::read(self.directory.join("replay.bin"))?;
        let offset = snapshot.offset;
        Ok((snapshot.replay(limit), offset))
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
