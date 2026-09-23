//! PTY manager: one `PtySession` per running process.
//!
//! Threads per session:
//! - reader: PTY master -> headless vt100 parser -> raw ring buffer -> output broadcast.
//! - writer: input queue -> PTY master (so `write_input` never blocks on a stalled program).
//! - waiter: waits for the child, lets the reader drain, flushes `output.log`, reports exit.
//!
//! The ring buffer and the output broadcast are updated under the same lock, so `attach`
//! (snapshot + subscribe) sees every byte exactly once.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use crate::agents::ScreenSnapshot;
use crate::hub::Hub;
use crate::model::{SessionId, now_ms};

/// Raw output kept for replay (and persisted as output.log).
pub const RING_CAP: usize = 2 * 1024 * 1024;
const SCROLLBACK_LINES: usize = 5000;
const OUTPUT_BROADCAST_CAP: usize = 1024;
const KILL_GRACE: Duration = Duration::from_secs(3);
/// Input queued for a program that isn't reading its terminal is capped at this.
const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;

pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(2, 500))
}

#[derive(Default)]
struct TitleCb {
    title: Option<String>,
    changed: bool,
    /// Answers to terminal queries seen in the output (sent only when no client is attached).
    replies: Vec<Vec<u8>>,
}

impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let t = String::from_utf8_lossy(title).trim().to_string();
        let t = if t.is_empty() { None } else { Some(t) };
        if t != self.title {
            self.title = t;
            self.changed = true;
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c) {
            // DA1. Programs such as fish block until it is answered; reply like xterm.js does.
            (None, 'c') if p0 == 0 => self.replies.push(b"\x1b[?1;2c".to_vec()),
            // DSR cursor position report.
            (None, 'n') if p0 == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            // DSR status report.
            (None, 'n') if p0 == 5 => self.replies.push(b"\x1b[0n".to_vec()),
            _ => {}
        }
    }
}

struct OutBuf {
    ring: VecDeque<u8>,
    /// True once bytes have been dropped from the front.
    wrapped: bool,
}

pub struct SpawnParams {
    pub id: SessionId,
    /// Distinguishes successive processes of one session (restart) in callbacks.
    pub generation: u64,
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub log_path: PathBuf,
}

pub struct PtySession {
    pub id: SessionId,
    pub generation: u64,
    pub pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input_tx: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>,
    /// Bytes queued for the writer thread but not yet written.
    input_pending: Arc<AtomicUsize>,
    out: Mutex<OutBuf>,
    out_tx: broadcast::Sender<Bytes>,
    parser: Mutex<vt100::Parser<TitleCb>>,
    seq: AtomicU64,
    last_output_at: AtomicI64,
    alive: AtomicBool,
    dirty: AtomicBool,
    /// Set when the session is removed/restarted: never write output.log again.
    discarded: AtomicBool,
    /// Serializes output.log writes with each other and with `discard`.
    flush_lock: Mutex<()>,
    log_path: PathBuf,
}

/// Spawn a process in a new PTY. The returned session is already running.
pub fn spawn(p: SpawnParams, hub: Weak<Hub>) -> anyhow::Result<Arc<PtySession>> {
    let (cols, rows) = clamp_size(p.cols, p.rows);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(&p.program);
    cmd.args(&p.args);
    cmd.cwd(&p.cwd);
    for k in &p.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &p.env {
        cmd.env(k, v);
    }
    let master = pair.master;
    let mut reader = master.try_clone_reader().context("pty reader")?;
    let mut writer = master.take_writer().context("pty writer")?;
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {}", p.program))?;
    // The parent must not keep the slave open, or the reader never sees EOF.
    drop(pair.slave);
    let pid = child.process_id();

    // If a thread can't be started below, don't leave the child running unmanaged.
    let kill_child = |pid: Option<u32>| {
        if let Some(pid) = pid.filter(|p| *p > 0 && *p <= i32::MAX as u32) {
            // SAFETY: kill(2) on the process group we just created.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    };
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let input_pending = Arc::new(AtomicUsize::new(0));
    let (out_tx, _) = broadcast::channel(OUTPUT_BROADCAST_CAP);

    let sess = Arc::new(PtySession {
        id: p.id,
        generation: p.generation,
        pid,
        master: Mutex::new(master),
        input_tx: Mutex::new(Some(input_tx)),
        input_pending: input_pending.clone(),
        out: Mutex::new(OutBuf {
            ring: VecDeque::new(),
            wrapped: false,
        }),
        out_tx,
        parser: Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_LINES,
            TitleCb::default(),
        )),
        seq: AtomicU64::new(0),
        last_output_at: AtomicI64::new(0),
        alive: AtomicBool::new(true),
        dirty: AtomicBool::new(false),
        discarded: AtomicBool::new(false),
        flush_lock: Mutex::new(()),
        log_path: p.log_path,
    });

    // writer
    std::thread::Builder::new()
        .name(format!("pty-w-{}", p.id))
        .spawn(move || {
            while let Ok(buf) = input_rx.recv() {
                let r = writer.write_all(&buf).and_then(|_| writer.flush());
                input_pending.fetch_sub(buf.len(), Ordering::SeqCst);
                if r.is_err() {
                    break;
                }
            }
        })
        .inspect_err(|_| kill_child(pid))?;

    // reader
    let (eof_tx, eof_rx) = std::sync::mpsc::channel::<()>();
    {
        let sess = sess.clone();
        let hub = hub.clone();
        std::thread::Builder::new()
            .name(format!("pty-r-{}", p.id))
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    let title = sess.ingest(&buf[..n]);
                    if let Some(title) = title
                        && let Some(hub) = hub.upgrade()
                    {
                        hub.pty_title_changed(sess.id, sess.generation, title);
                    }
                }
                // Persist anything that arrived after the waiter's flush.
                sess.flush_log();
                let _ = eof_tx.send(());
            })
            .inspect_err(|_| kill_child(pid))?;
    }

    // waiter
    {
        let sess = sess.clone();
        std::thread::Builder::new()
            .name(format!("pty-x-{}", p.id))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(st) => exit_code_of(&st),
                    Err(_) => -1,
                };
                // Let the reader drain what the process wrote last (it may never see EOF if a
                // grandchild keeps the tty open).
                let _ = eof_rx.recv_timeout(Duration::from_millis(500));
                sess.alive.store(false, Ordering::SeqCst);
                sess.input_tx.lock().take();
                sess.flush_log();
                if let Some(hub) = hub.upgrade() {
                    hub.pty_exited(sess.id, sess.generation, code);
                }
            })
            .inspect_err(|_| kill_child(pid))?;
    }

    Ok(sess)
}

impl PtySession {
    /// Feed output through the parser and the ring, then broadcast. Returns a new title if
    /// the program changed it.
    fn ingest(&self, data: &[u8]) -> Option<Option<String>> {
        let mut title = None;
        let replies: Vec<Vec<u8>>;
        {
            let mut parser = self.parser.lock();
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(data))).is_ok();
            if !ok {
                // A parser bug must never take the session down: start a fresh screen.
                let (rows, cols) = parser.screen().size();
                *parser = vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, TitleCb::default());
            }
            let cb = parser.callbacks_mut();
            if cb.changed {
                cb.changed = false;
                title = Some(cb.title.clone());
            }
            replies = std::mem::take(&mut cb.replies);
        }
        let chunk = Bytes::copy_from_slice(data);
        {
            let mut out = self.out.lock();
            out.ring.extend(data.iter().copied());
            let over = out.ring.len().saturating_sub(RING_CAP);
            if over > 0 {
                out.ring.drain(..over);
                out.wrapped = true;
            }
            self.seq.fetch_add(1, Ordering::SeqCst);
            self.last_output_at.store(now_ms(), Ordering::SeqCst);
            // Receivers are exactly the attached clients' forwarders. With a client attached,
            // its terminal answers queries; without one, we do (or e.g. fish hangs at startup).
            let headless = self.out_tx.receiver_count() == 0;
            let _ = self.out_tx.send(chunk);
            if headless {
                for r in &replies {
                    let _ = self.write(r);
                }
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
        title
    }

    fn snapshot_locked(out: &OutBuf) -> Vec<u8> {
        let (a, b) = out.ring.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        if out.wrapped {
            // Don't start the replay in the middle of an escape sequence / UTF-8 char.
            if let Some(pos) = v.iter().take(64 * 1024).position(|&c| c == b'\n') {
                v.drain(..=pos);
            }
        }
        v
    }

    /// Replay bytes + a live receiver, taken atomically (no lost or duplicated bytes).
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let out = self.out.lock();
        (Self::snapshot_locked(&out), self.out_tx.subscribe())
    }

    pub fn replay(&self) -> Vec<u8> {
        Self::snapshot_locked(&self.out.lock())
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub fn last_output_at(&self) -> i64 {
        self.last_output_at.load(Ordering::SeqCst)
    }

    pub fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        if !self.is_alive() {
            anyhow::bail!("session {} is not running", self.id);
        }
        if self.input_pending.load(Ordering::SeqCst) + bytes.len() > MAX_PENDING_INPUT {
            anyhow::bail!("session {} is not reading its input (input buffer full)", self.id);
        }
        match self.input_tx.lock().as_ref() {
            Some(tx) => {
                self.input_pending.fetch_add(bytes.len(), Ordering::SeqCst);
                tx.send(bytes.to_vec()).map_err(|_| {
                    self.input_pending.fetch_sub(bytes.len(), Ordering::SeqCst);
                    anyhow::anyhow!("session {} input closed", self.id)
                })
            }
            None => anyhow::bail!("session {} is not running", self.id),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let (cols, rows) = clamp_size(cols, rows);
        self.parser.lock().screen_mut().set_size(rows, cols);
        if self.is_alive() {
            self.master
                .lock()
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize")?;
        }
        Ok(())
    }

    fn foreground_pgrp(&self) -> Option<i32> {
        self.master.lock().process_group_leader()
    }

    pub fn screen(&self) -> ScreenSnapshot {
        let foreground_busy = self.is_alive()
            && match (self.foreground_pgrp(), self.pid) {
                (Some(fg), Some(pid)) => fg > 0 && fg as u32 != pid,
                _ => false,
            };
        let parser = self.parser.lock();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let lines = screen.rows(0, cols).map(|l| l.trim_end().to_string()).collect();
        ScreenSnapshot {
            lines,
            cursor_row,
            cursor_col,
            cols,
            rows,
            last_output_at: self.last_output_at(),
            foreground_busy,
        }
    }

    /// Last `n` lines of scrollback + screen as plain text.
    pub fn recent_text(&self, n: usize) -> String {
        let mut parser = self.parser.lock();
        tail_text(&mut parser, n)
    }

    /// Persist the ring buffer to output.log if it changed since the last flush.
    pub fn flush_log(&self) {
        let _guard = self.flush_lock.lock();
        if self.discarded.load(Ordering::SeqCst) || !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let data = self.replay();
        if let Err(e) = crate::paths::write_private(&self.log_path, &data) {
            tracing::warn!(session = self.id, "flushing output.log: {e}");
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Force the next flush to rewrite output.log.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// True when output.log is up to date with the ring buffer.
    pub fn log_persisted(&self) -> bool {
        !self.dirty.load(Ordering::SeqCst) && !self.discarded.load(Ordering::SeqCst)
    }

    /// Stop persisting output (the session is being removed or restarted).
    pub fn discard(&self) {
        // Wait out an in-flight flush so it can't recreate a log we're about to delete.
        let _guard = self.flush_lock.lock();
        self.discarded.store(true, Ordering::SeqCst);
    }

    /// SIGHUP the session's process group (and the foreground job's group), SIGKILL after
    /// a grace period if the process is still alive. Returns immediately.
    pub fn kill(self: &Arc<Self>) {
        if !self.is_alive() {
            return;
        }
        let Some(pid) = self.pid.map(|p| p as i32).filter(|p| *p > 0) else {
            return;
        };
        let fg = self.foreground_pgrp().filter(|g| *g > 0 && *g != pid);
        // The child is a session leader (setsid). Signal its group, the foreground job's
        // group, and every other process in the session (background jobs).
        let signal_all = move |sig: i32| {
            // SAFETY: plain kill(2) calls on groups/processes of the session we created.
            unsafe {
                libc::kill(-pid, sig);
                if let Some(g) = fg {
                    libc::kill(-g, sig);
                }
                for p in session_members(pid) {
                    libc::kill(p, sig);
                }
            }
        };
        signal_all(libc::SIGHUP);
        signal_all(libc::SIGCONT);
        let me = self.clone();
        let _ = std::thread::Builder::new()
            .name(format!("pty-k-{}", self.id))
            .spawn(move || {
                let deadline = std::time::Instant::now() + KILL_GRACE;
                while std::time::Instant::now() < deadline {
                    if !me.is_alive() && session_members(pid).is_empty() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // Escalate for the child and for any descendant that ignored SIGHUP, even if
                // the direct child already exited. Session membership is re-read, so a
                // recycled pid outside the session is never hit.
                // SAFETY: as above.
                unsafe {
                    for p in session_members(pid) {
                        libc::kill(p, libc::SIGKILL);
                    }
                    if me.is_alive() {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            });
    }
}

/// Pids whose session id is `sid` (Linux /proc walk; empty elsewhere).
pub fn session_members(sid: i32) -> Vec<i32> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
        .filter(|&p| proc_session_id(p) == Some(sid))
        .collect()
}

fn proc_session_id(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised comm: state ppid pgrp session ...
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(3)?.parse().ok()
}

/// After a daemon crash: terminate a session that survived, but only if `pid` is still a
/// session leader whose environment says it is muster session `id` (guards against pid reuse).
pub fn reap_orphan(pid: u32, id: SessionId) {
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 0 || proc_session_id(pid) != Some(pid) {
        return;
    }
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return;
    };
    let needle = format!("MUSTER_SESSION_ID={id}");
    if !env.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()) {
        return;
    }
    let members = session_members(pid);
    // SAFETY: kill(2) on processes verified to belong to that muster session.
    unsafe {
        for &p in &members {
            libc::kill(p, libc::SIGHUP);
            libc::kill(p, libc::SIGCONT);
        }
    }
    let _ = std::thread::Builder::new().name(format!("reap-{id}")).spawn(move || {
        std::thread::sleep(KILL_GRACE);
        // SAFETY: as above; membership re-read.
        unsafe {
            for p in session_members(pid) {
                libc::kill(p, libc::SIGKILL);
            }
        }
    });
}

/// Map portable-pty's exit status to a shell-style code (128 + signal when killed).
fn exit_code_of(st: &portable_pty::ExitStatus) -> i32 {
    let Some(name) = st.signal() else {
        return st.exit_code() as i32;
    };
    for sig in 1..65 {
        // SAFETY: strsignal returns a pointer to a static/thread-local string.
        let s = unsafe { libc::strsignal(sig) };
        if !s.is_null() && unsafe { std::ffi::CStr::from_ptr(s) }.to_string_lossy() == name {
            return 128 + sig;
        }
    }
    128
}

/// Plain-text tail of a parser's scrollback + visible screen.
fn tail_text(parser: &mut vt100::Parser<TitleCb>, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let screen = parser.screen_mut();
    let (rows, cols) = screen.size();
    let rows = rows as usize;
    let prev = screen.scrollback();
    // Blank rows below the last written line of the visible screen don't count toward `n`.
    screen.set_scrollback(0);
    let visible_used = screen
        .rows(0, cols)
        .enumerate()
        .filter(|(_, r)| !r.trim_end().is_empty())
        .map(|(i, _)| i + 1)
        .last()
        .unwrap_or(0);
    screen.set_scrollback(usize::MAX);
    let sb_len = screen.scrollback();
    // Combined line index i (0 = oldest scrollback line). A view at offset `o` shows lines
    // [sb_len - o, sb_len - o + rows).
    let total = sb_len + visible_used.min(rows);
    let want_from = total.saturating_sub(n);
    let mut lines: Vec<String> = Vec::with_capacity(n.min(total));
    let mut next = want_from;
    while next < total {
        let o = sb_len.saturating_sub(next).min(sb_len);
        screen.set_scrollback(o);
        let start = sb_len - o;
        for (k, row) in screen.rows(0, cols).enumerate() {
            let idx = start + k;
            if idx == next {
                lines.push(row.trim_end().to_string());
                next += 1;
            }
        }
        if o == 0 {
            break;
        }
    }
    screen.set_scrollback(prev);
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Replay bytes from a persisted output.log (last `RING_CAP` bytes).
pub fn read_log(path: &Path) -> Vec<u8> {
    let Ok(mut data) = std::fs::read(path) else {
        return Vec::new();
    };
    if data.len() > RING_CAP {
        let cut = data.len() - RING_CAP;
        data.drain(..cut);
        if let Some(pos) = data.iter().take(64 * 1024).position(|&c| c == b'\n') {
            data.drain(..=pos);
        }
    }
    data
}

/// Plain-text tail of a persisted output.log (for exited sessions from a previous daemon).
pub fn log_tail_text(path: &Path, cols: u16, rows: u16, n: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let (cols, rows) = clamp_size(cols, rows);
    let mut parser = vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, TitleCb::default());
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(&data))).ok()?;
    Some(tail_text(&mut parser, n))
}

/// Screen snapshot rebuilt from a persisted output.log.
pub fn log_screen(path: &Path, cols: u16, rows: u16) -> Option<ScreenSnapshot> {
    let data = std::fs::read(path).ok()?;
    let (cols, rows) = clamp_size(cols, rows);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(&data))).ok()?;
    let screen = parser.screen();
    let (cursor_row, cursor_col) = screen.cursor_position();
    Some(ScreenSnapshot {
        lines: screen.rows(0, cols).map(|l| l.trim_end().to_string()).collect(),
        cursor_row,
        cursor_col,
        cols,
        rows,
        last_output_at: 0,
        foreground_busy: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser_with(data: &[u8], rows: u16) -> vt100::Parser<TitleCb> {
        let mut p = vt100::Parser::new_with_callbacks(rows, 40, SCROLLBACK_LINES, TitleCb::default());
        p.process(data);
        p
    }

    #[test]
    fn tail_ignores_blank_rows_below_output() {
        let mut p = parser_with(b"one\r\ntwo\r\nthree\r\n$ ", 24);
        assert_eq!(tail_text(&mut p, 1), "$");
        assert_eq!(tail_text(&mut p, 2), "three\n$");
    }

    #[test]
    fn tail_spans_scrollback() {
        let data: String = (0..30).map(|i| format!("line{i}\r\n")).collect();
        let mut p = parser_with(data.as_bytes(), 5);
        assert_eq!(tail_text(&mut p, 3), "line27\nline28\nline29");
        let all = tail_text(&mut p, 100);
        assert!(all.starts_with("line0\n") && all.ends_with("line29"));
    }

    #[test]
    fn foreground_busy_tracks_running_job() {
        let dir = std::env::temp_dir().join(format!("muster-pty-test-{}", std::process::id()));
        let sess = spawn(
            SpawnParams {
                id: 1,
                generation: 1,
                program: std::env::var("MUSTER_TEST_SHELL").unwrap_or_else(|_| "bash".into()),
                args: match std::env::var("MUSTER_TEST_SHELL") {
                    Ok(_) => vec!["-l".into()],
                    Err(_) => vec!["--noprofile".into(), "--norc".into(), "-i".into()],
                },
                env: BTreeMap::new(),
                env_remove: vec![],
                cwd: std::env::temp_dir(),
                cols: 80,
                rows: 24,
                log_path: dir.join("output.log"),
            },
            Weak::new(),
        )
        .expect("spawn bash");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!sess.screen().foreground_busy, "idle shell reported busy");
        sess.write(b"sleep 3\r").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let snap = sess.screen();
        assert!(
            snap.foreground_busy,
            "running job not reported busy: fg={:?} pid={:?} screen={:?}",
            sess.foreground_pgrp(),
            sess.pid,
            snap.lines
        );
        sess.kill();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn title_callback() {
        let mut p = parser_with(b"\x1b]0;hello\x07", 5);
        assert_eq!(p.callbacks_mut().title.as_deref(), Some("hello"));
    }
}
