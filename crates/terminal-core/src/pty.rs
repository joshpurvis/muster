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
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use crate::{ScreenSnapshot, SessionId, TerminalEvent, EventSink, now_ms};

/// Raw output kept for replay (and persisted as output.log).
pub const RING_CAP: usize = 2 * 1024 * 1024;
const SCROLLBACK_LINES: usize = 5000;
const OUTPUT_BROADCAST_CAP: usize = 1024;
pub const KILL_GRACE: Duration = Duration::from_secs(3);

/// An immutable replay checkpoint. The owner stores the body separately from
/// the small mode/offset metadata, avoiding JSON expansion of terminal bytes.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ReplaySnapshot {
    #[serde(skip)]
    pub body: Vec<u8>,
    input_modes: Vec<u8>,
    alternate_screen: bool,
    pub offset: u64,
}

impl ReplaySnapshot {
    pub fn replay(mut self, max_body: Option<usize>) -> Vec<u8> {
        if let Some(max) = max_body { tail_from_line(&mut self.body, max); }
        let mut replay = replay_prefix_parts(self.alternate_screen, &self.input_modes, &self.body);
        replay.extend_from_slice(&self.body);
        replay
    }
}
/// Input queued for a program that isn't reading its terminal is capped at this.
const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;
/// Window titles are kept to this many bytes. A program can emit an OSC title of any
/// length, and the title travels in every host status frame, which has a size limit.
pub const MAX_TITLE_BYTES: usize = 1024;

pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(2, 500))
}

#[derive(Default)]
struct TitleCb {
    title: Option<String>,
    changed: bool,
    /// Answers to terminal queries seen in the output. The daemon is the only responder for
    /// these: the web UI's xterm.js is told not to answer them (see web/src/terms.ts), so a
    /// program gets exactly one reply however many clients watch, and none from replays.
    replies: Vec<Vec<u8>>,
}

impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        // Decode at most a few bytes past the cap, then cut on a character boundary.
        let raw = &title[..title.len().min(MAX_TITLE_BYTES + 4)];
        let decoded = String::from_utf8_lossy(raw);
        let t = decoded.trim();
        let t = t[..t.floor_char_boundary(MAX_TITLE_BYTES)].trim_end().to_string();
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
        // Same answers xterm.js would give (TERM=xterm-256color).
        match (i1, c) {
            // DA1. Programs such as fish block until it is answered.
            (None, 'c') if p0 == 0 => self.replies.push(b"\x1b[?1;2c".to_vec()),
            // DA2 (secondary device attributes).
            (Some(b'>'), 'c') if p0 == 0 => self.replies.push(b"\x1b[>0;276;0c".to_vec()),
            // DSR cursor position report.
            (None, 'n') if p0 == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            // DECXCPR (DEC-private cursor position report).
            (Some(b'?'), 'n') if p0 == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .push(format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes());
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
    /// Bytes ever ingested (the ring holds the last `ring.len()` of them).
    total: u64,
}

/// output.log bookkeeping. The file is append-only between compactions.
struct LogState {
    /// `OutBuf::total` at the last successful flush.
    persisted: u64,
    /// Current length of output.log.
    size: u64,
    /// Next flush must rewrite the file from the ring (new process, lost bytes, compaction).
    rewrite: bool,
    /// Appended since the last fsync.
    unsynced: bool,
}

pub struct SpawnParams {
    pub id: SessionId,
    /// Distinguishes successive processes of one session (restart) in callbacks.
    pub generation: u64,
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The host receives a complete launch environment from the application.
    pub clear_env: bool,
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
    /// Dropped once the reader sees EOF (no process holds the terminal any more).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input_tx: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>,
    /// Bytes queued for the writer thread but not yet written.
    input_pending: Arc<AtomicUsize>,
    out: Mutex<OutBuf>,
    out_tx: broadcast::Sender<Bytes>,
    parser: Mutex<vt100::Parser<TitleCb>>,
    seq: AtomicU64,
    last_output_at: AtomicI64,
    alive: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    /// `wait()` returned: the leader's pid may be reused from now on, so it and its process
    /// group are no longer signalled directly (only verified session members are).
    reaped: AtomicBool,
    /// The reader saw EOF: every process that had the terminal open has closed it.
    reader_done: AtomicBool,
    dirty: AtomicBool,
    /// Set when the session is removed/restarted: never write output.log again.
    discarded: AtomicBool,
    /// Serializes output.log writes with each other and with `discard`.
    log: Mutex<LogState>,
    log_path: PathBuf,
    /// `KEY=value` pairs from the process environment that mark processes of this run
    /// (`MUSTER_RUN=…`, `MUSTER_SESSION_ID=…`), to recognize leftovers after the leader exited.
    markers: Vec<String>,
    /// The slave side of this PTY (`/dev/pts/N`). While our master is open the number can't
    /// be reused, so a process holding it open is one of ours even if it cleared its env.
    tty: Option<PathBuf>,
}

/// Environment variables whose values mark a run's processes (all present ones must match).
/// Neither contains KEY/SECRET/TOKEN, so agent CLIs that filter secrets out of the environment
/// of the commands they run (Codex does by default) keep them.
pub const RUN_MARKER_VARS: &[&str] = &["MUSTER_RUN", "MUSTER_SESSION_ID"];

/// The marker set for a spawn environment. Falls back to `MUSTER_TOKEN` for callers that set
/// neither marker variable.
pub fn run_markers(env: &BTreeMap<String, String>) -> Vec<String> {
    let mut m: Vec<String> = RUN_MARKER_VARS
        .iter()
        .filter_map(|k| env.get(*k).map(|v| format!("{k}={v}")))
        .collect();
    if !env.contains_key("MUSTER_RUN")
        && let Some(v) = env.get("MUSTER_TOKEN")
    {
        m.push(format!("MUSTER_TOKEN={v}"));
    }
    m
}

/// Spawn a process in a new PTY. The returned session is already running.
pub fn spawn(p: SpawnParams, events: EventSink) -> anyhow::Result<Arc<PtySession>> {
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
    if p.clear_env {
        cmd.env_clear();
    }
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
    let markers = run_markers(&p.env);
    let tty = master.tty_name();

    let sess = Arc::new(PtySession {
        id: p.id,
        generation: p.generation,
        pid,
        master: Mutex::new(Some(master)),
        input_tx: Mutex::new(Some(input_tx)),
        input_pending: input_pending.clone(),
        out: Mutex::new(OutBuf {
            ring: VecDeque::new(),
            wrapped: false,
            total: 0,
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
        exit_code: Mutex::new(None),
        reaped: AtomicBool::new(false),
        reader_done: AtomicBool::new(false),
        dirty: AtomicBool::new(false),
        discarded: AtomicBool::new(false),
        log: Mutex::new(LogState {
            persisted: 0,
            size: 0,
            // A new process always starts a new output.log.
            rewrite: true,
            unsynced: false,
        }),
        log_path: p.log_path,
        markers,
        tty,
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
        let events = events.clone();
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
                    if let Some(title) = title {
                        events(TerminalEvent::Title { id: sess.id, generation: sess.generation, title });
                    }
                }
                // Nobody has the terminal open any more: release the master fd.
                sess.reader_done.store(true, Ordering::SeqCst);
                sess.master.lock().take();
                // Persist anything that arrived after the waiter's flush.
                sess.flush_log_final();
                let _ = eof_tx.send(());
                events(TerminalEvent::Drained { id: sess.id, generation: sess.generation });
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
                sess.reaped.store(true, Ordering::SeqCst);
                // Let the reader drain what the process wrote last (it may never see EOF if a
                // grandchild keeps the tty open).
                let _ = eof_rx.recv_timeout(Duration::from_millis(500));
                *sess.exit_code.lock() = Some(code);
                sess.alive.store(false, Ordering::SeqCst);
                sess.input_tx.lock().take();
                sess.flush_log_final();
                events(TerminalEvent::Exited { id: sess.id, generation: sess.generation, code });
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
        // Lock order: parser, then out. Holding both keeps the replay's mode prefix (from the
        // parser) consistent with the ring snapshot taken by `attach`.
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
        let replies = std::mem::take(&mut cb.replies);
        let chunk = Bytes::copy_from_slice(data);
        {
            let mut out = self.out.lock();
            out.ring.extend(data.iter().copied());
            out.total += data.len() as u64;
            let over = out.ring.len().saturating_sub(RING_CAP);
            if over > 0 {
                out.ring.drain(..over);
                out.wrapped = true;
            }
            self.seq.fetch_add(1, Ordering::SeqCst);
            self.last_output_at.store(now_ms(), Ordering::SeqCst);
            let _ = self.out_tx.send(chunk);
            // The daemon answers terminal queries itself (clients don't).
            for r in &replies {
                let _ = self.write(r);
            }
        }
        drop(parser);
        self.dirty.store(true, Ordering::SeqCst);
        title
    }

    /// The ring's bytes exactly (what output.log persists).
    fn raw_locked(out: &OutBuf) -> Vec<u8> {
        let (a, b) = out.ring.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        v
    }

    /// The ring's bytes for a client replay: once it wrapped, starting after the first
    /// newline so the replay doesn't begin inside an escape sequence or UTF-8 character.
    fn snapshot_locked(out: &OutBuf) -> Vec<u8> {
        let mut v = Self::raw_locked(out);
        if out.wrapped {
            // Don't start the replay in the middle of an escape sequence / UTF-8 char.
            if let Some(pos) = v.iter().take(64 * 1024).position(|&c| c == b'\n') {
                v.drain(..=pos);
            }
        }
        v
    }

    /// Replay for a client: the terminal modes that the ring may no longer contain, then the
    /// ring (its last `max_body` bytes when set). Callers hold the parser lock and then the out
    /// lock.
    fn client_replay(parser: &vt100::Parser<TitleCb>, out: &OutBuf, max_body: Option<usize>) -> Vec<u8> {
        let mut body = Self::snapshot_locked(out);
        if let Some(max) = max_body {
            tail_from_line(&mut body, max);
        }
        let mut v = replay_prefix(parser.screen(), &body);
        v.extend_from_slice(&body);
        v
    }

    /// Replay bytes + a live receiver, taken atomically (no lost or duplicated bytes).
    pub fn attach(&self, max_body: Option<usize>) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let (replay, receiver, _) = self.attach_sequenced(max_body);
        (replay, receiver)
    }

    /// The offset is the first live byte after this replay, captured under the
    /// same locks as the snapshot and subscription. Synthetic mode prefixes do
    /// not advance the raw output offset.
    pub fn attach_sequenced(&self, max_body: Option<usize>) -> (Vec<u8>, broadcast::Receiver<Bytes>, u64) {
        let parser = self.parser.lock();
        let out = self.out.lock();
        (Self::client_replay(&parser, &out, max_body), self.out_tx.subscribe(), out.total)
    }

    /// Capture replay state without retaining a PTY, parser, or output channel.
    pub fn replay_snapshot(&self) -> ReplaySnapshot {
        let parser = self.parser.lock();
        let out = self.out.lock();
        ReplaySnapshot {
            body: Self::snapshot_locked(&out),
            input_modes: parser.screen().input_mode_formatted(),
            alternate_screen: parser.screen().alternate_screen(),
            offset: out.total,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock()
    }

    pub fn title(&self) -> Option<String> {
        self.parser.lock().callbacks().title.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// True once no process has the terminal open any more (the reader saw EOF).
    pub fn is_drained(&self) -> bool {
        self.reader_done.load(Ordering::SeqCst)
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
        // Check and reserve under the same lock: simultaneous writers must not
        // each observe spare capacity and collectively exceed the bound.
        let input = self.input_tx.lock();
        if bytes.len() > MAX_PENDING_INPUT.saturating_sub(self.input_pending.load(Ordering::SeqCst)) {
            anyhow::bail!("session {} is not reading its input (input buffer full)", self.id);
        }
        match input.as_ref() {
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
        if self.is_alive()
            && let Some(m) = self.master.lock().as_ref()
        {
            m.resize(PtySize {
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
        self.master.lock().as_ref()?.process_group_leader()
    }

    /// Current (cols, rows), without rendering a screen snapshot.
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.lock().screen().size();
        (cols, rows)
    }

    /// Application cursor mode (DECCKM) is on.
    pub fn application_cursor(&self) -> bool {
        self.parser.lock().screen().application_cursor()
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

    /// Persist new output to output.log (periodic; no fsync).
    pub fn flush_log(&self) {
        self.flush_log_inner(false);
    }

    /// Persist new output and fsync (process exit, daemon shutdown).
    pub fn flush_log_final(&self) {
        self.flush_log_inner(true);
    }

    /// output.log is append-only: new bytes since the last flush are appended. It is rewritten
    /// from the ring (the last `RING_CAP` bytes) for a new process, when bytes were lost (more
    /// output than the ring holds since the last flush), and when it grows past twice the cap.
    fn flush_log_inner(&self, sync: bool) {
        let mut st = self.log.lock();
        if self.discarded.load(Ordering::SeqCst) {
            return;
        }
        if !self.dirty.swap(false, Ordering::SeqCst) && !st.rewrite {
            if sync && st.unsynced && sync_file(&self.log_path).is_ok() {
                st.unsynced = false;
            }
            return;
        }
        let (data, rewrite, total) = {
            let out = self.out.lock();
            let pending = out.total - st.persisted;
            let rewrite = st.rewrite || pending > out.ring.len() as u64 || st.size + pending > 2 * RING_CAP as u64;
            // Persist the ring exactly (`total` counts every byte of it); readers trim the
            // partial first line (`read_log`).
            let data = if rewrite {
                Self::raw_locked(&out)
            } else {
                let skip = out.ring.len() - pending as usize;
                out.ring.range(skip..).copied().collect()
            };
            (data, rewrite, out.total)
        };
        let r = if rewrite {
            write_file(&self.log_path, &data, sync)
        } else {
            append_file(&self.log_path, &data, sync)
        };
        match r {
            Ok(()) => {
                st.persisted = total;
                st.size = if rewrite {
                    data.len() as u64
                } else {
                    st.size + data.len() as u64
                };
                st.rewrite = false;
                st.unsynced = !sync;
            }
            Err(e) => {
                tracing::warn!(session = self.id, "flushing output.log: {e}");
                self.dirty.store(true, Ordering::SeqCst);
                // A failed append may have written part of the data.
                st.rewrite = true;
            }
        }
    }

    /// Force the next flush to rewrite output.log from the ring.
    pub fn mark_dirty(&self) {
        self.log.lock().rewrite = true;
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// True when output.log is up to date with the ring buffer.
    pub fn log_persisted(&self) -> bool {
        !self.dirty.load(Ordering::SeqCst) && !self.discarded.load(Ordering::SeqCst) && !self.log.lock().rewrite
    }

    /// Safe to release the in-memory replay only after the final fsync succeeds.
    pub fn log_durable(&self) -> bool {
        let log = self.log.lock();
        !self.dirty.load(Ordering::SeqCst) && !self.discarded.load(Ordering::SeqCst)
            && !log.rewrite && !log.unsynced
    }

    /// Stop persisting output (the session is being removed or restarted).
    pub fn discard(&self) {
        // Wait out an in-flight flush so it can't recreate a log we're about to delete.
        let _guard = self.log.lock();
        self.discarded.store(true, Ordering::SeqCst);
    }

    /// Processes of this session to signal once the leader has been reaped. Its pid (the
    /// session id) may then be reused, so session membership alone is never trusted: a process
    /// must also carry this run's environment markers, or hold this PTY's slave open while our
    /// master still reserves its number (not drained).
    fn leftovers(&self, sid: i32) -> Vec<i32> {
        let markers: Vec<&str> = self.markers.iter().map(String::as_str).collect();
        let tty = if self.is_drained() { None } else { self.tty.as_deref() };
        session_members(sid)
            .into_iter()
            .filter(|&p| (!markers.is_empty() && has_env(p, &markers)) || tty.is_some_and(|t| holds_tty(p, t)))
            .collect()
    }

    /// Members to signal: every session member while the leader hasn't been reaped (its pid
    /// can't be reused yet), verified leftovers after that.
    fn targets(&self, sid: i32) -> Vec<i32> {
        if self.reaped.load(Ordering::SeqCst) {
            self.leftovers(sid)
        } else {
            session_members(sid)
        }
    }

    /// SIGHUP the session's process group (and the foreground job's group) and every other
    /// process left in its session (background jobs), then SIGKILL after a grace period.
    /// Also works after the leader exited: leftovers such as `sleep 1000 &` are still in the
    /// session and would otherwise keep the terminal (and the reader thread) alive.
    /// Returns immediately.
    pub fn kill(self: &Arc<Self>) {
        let Some(pid) = self.pid.map(|p| p as i32).filter(|p| *p > 0) else {
            return;
        };
        let alive = self.is_alive();
        if !alive && self.is_drained() && self.markers.is_empty() {
            return;
        }
        // Groups are signalled only while the leader hasn't been reaped (its pid, which is
        // also the group id, can't be reused before that).
        let direct = alive && !self.reaped.load(Ordering::SeqCst);
        let fg = if direct {
            self.foreground_pgrp().filter(|g| *g > 0 && *g != pid)
        } else {
            None
        };
        let me = self.clone();
        let targets = move || -> Vec<i32> { me.targets(pid) };
        // The child is a session leader (setsid). Signal its group, the foreground job's
        // group, and every other process in the session (background jobs).
        let signal_all = |sig: i32, members: &[i32]| {
            // SAFETY: plain kill(2) calls on groups/processes of the session we created.
            unsafe {
                if direct {
                    libc::kill(-pid, sig);
                    if let Some(g) = fg {
                        libc::kill(-g, sig);
                    }
                }
                for &p in members {
                    libc::kill(p, sig);
                }
            }
        };
        let members = targets();
        if !alive && members.is_empty() {
            return;
        }
        signal_all(libc::SIGHUP, &members);
        signal_all(libc::SIGCONT, &members);
        let me = self.clone();
        let _ = std::thread::Builder::new()
            .name(format!("pty-k-{}", self.id))
            .spawn(move || {
                let deadline = std::time::Instant::now() + KILL_GRACE;
                while std::time::Instant::now() < deadline {
                    if !me.is_alive() && targets().is_empty() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                me.kill_now();
            });
    }

    /// SIGKILL the leader (if alive) and every leftover right away.
    pub fn kill_now(&self) {
        let Some(pid) = self.pid.map(|p| p as i32).filter(|p| *p > 0) else {
            return;
        };
        // Session membership (and, once reaped, the run markers) are re-read, so a recycled
        // pid is never hit.
        let members = self.targets(pid);
        // SAFETY: as in `kill`.
        unsafe {
            for p in members {
                libc::kill(p, libc::SIGKILL);
            }
            if self.is_alive() && !self.reaped.load(Ordering::SeqCst) {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    /// True while the leader or any leftover of its session is still running.
    pub fn has_processes(&self) -> bool {
        if self.is_alive() {
            return true;
        }
        let Some(pid) = self.pid.map(|p| p as i32).filter(|p| *p > 0) else {
            return false;
        };
        !self.targets(pid).is_empty()
    }

    /// `KEY=value` markers of this run (stored so a later daemon can reap leftovers).
    pub fn markers(&self) -> &[String] {
        &self.markers
    }
}

/// Does `pid` have `tty` open on any file descriptor?
fn holds_tty(pid: i32, tty: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .any(|e| std::fs::read_link(e.path()).is_ok_and(|l| l == tty))
}

/// Does `pid`'s environment contain every `KEY=value` in `needles`?
fn has_env(pid: i32, needles: &[&str]) -> bool {
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    needles
        .iter()
        .all(|n| env.split(|b| *b == 0).any(|kv| kv == n.as_bytes()))
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

/// Terminate processes left from sessions of a previous daemon run (the daemon crashed, or
/// they ignored SIGHUP, or an exited shell left background jobs), or of removed sessions. Each
/// entry is (session id = pid of the process muster started, muster session id, `KEY=value`
/// environment markers). Only processes in that session whose environment carries every
/// marker are touched, which guards against pid reuse. Blocks: SIGHUP, up to `KILL_GRACE` for
/// them to exit, SIGKILL, and a short wait to confirm. Returns the muster session ids that
/// still have processes (their records must be kept for the next attempt).
pub fn reap_leftovers(sessions: Vec<(u32, SessionId, Vec<String>)>) -> std::collections::HashSet<SessionId> {
    let wanted: Vec<(i32, SessionId, Vec<String>)> = sessions
        .into_iter()
        .filter_map(|(sid, id, n)| i32::try_from(sid).ok().filter(|s| *s > 0).map(|s| (s, id, n)))
        // Without markers a recycled session id can't be told apart: never signal those.
        .filter(|(_, _, n)| !n.is_empty())
        .collect();
    if wanted.is_empty() {
        return Default::default();
    }
    let ours = || -> Vec<(i32, SessionId)> {
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let pids: Vec<i32> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
            .collect();
        let mut out = Vec::new();
        for p in pids {
            let Some(sid) = proc_session_id(p) else { continue };
            for (s, id, needles) in &wanted {
                let n: Vec<&str> = needles.iter().map(String::as_str).collect();
                if *s == sid && has_env(p, &n) {
                    out.push((p, *id));
                }
            }
        }
        out
    };
    let found = ours();
    if found.is_empty() {
        return Default::default();
    }
    tracing::info!("terminating {} leftover process(es) from a previous run", found.len());
    // SAFETY: kill(2) on processes verified to belong to those muster sessions.
    unsafe {
        for &(p, _) in &found {
            libc::kill(p, libc::SIGHUP);
            libc::kill(p, libc::SIGCONT);
        }
    }
    let deadline = std::time::Instant::now() + KILL_GRACE;
    while std::time::Instant::now() < deadline && !ours().is_empty() {
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: as above; membership and environment re-read.
    unsafe {
        for (p, _) in ours() {
            libc::kill(p, libc::SIGKILL);
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let left = ours();
        if left.is_empty() {
            return Default::default();
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("{} leftover process(es) survived SIGKILL", left.len());
            return left.into_iter().map(|(_, id)| id).collect();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Rewrite `p` (tmp + rename, mode 0600), fsync only if `sync`.
fn write_file(p: &Path, data: &[u8], sync: bool) -> std::io::Result<()> {
    if sync {
        return crate::private_fs::write_private(p, data);
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent() {
        crate::private_fs::ensure_private_dir(dir)?;
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, p)
}

fn append_file(p: &Path, data: &[u8], sync: bool) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // Never create it here: a missing file means it was deleted (session removed) or never
    // written, and the next rewrite handles both.
    let mut f = std::fs::OpenOptions::new().append(true).mode(0o600).open(p)?;
    f.write_all(data)?;
    if sync {
        f.sync_all()?;
    }
    Ok(())
}

fn sync_file(p: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().append(true).open(p)?.sync_all()
}

/// Escape codes a client needs before the ring's bytes to end up in the daemon's terminal
/// state even when the ring wrapped past the sequences that set it up: the input modes
/// (bracketed paste, application cursor/keypad, mouse reporting) and, if the ring starts
/// inside the alternate screen, a switch to it. Setting a mode the replay sets again later
/// is harmless, so this is also correct for an unwrapped ring.
fn replay_prefix(screen: &vt100::Screen, body: &[u8]) -> Vec<u8> {
    replay_prefix_parts(screen.alternate_screen(), &screen.input_mode_formatted(), body)
}

fn replay_prefix_parts(alternate_screen: bool, input_modes: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let starts_in_alt = match first_alt_switch(body) {
        // The first switch leaves the alternate screen: everything before it was drawn there.
        Some(enter) => !enter,
        None => alternate_screen,
    };
    if starts_in_alt {
        v.extend_from_slice(b"\x1b[?1049h");
    }
    v.extend_from_slice(input_modes);
    v
}

/// Direction of the first alternate-screen switch (`CSI ? 47/1047/1049 h|l`) in `data`:
/// Some(true) = enter, Some(false) = leave.
fn first_alt_switch(data: &[u8]) -> Option<bool> {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] != 0x1b || data[i + 1] != b'[' || data[i + 2] != b'?' {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let mut params: Vec<u32> = Vec::new();
        let mut cur: Option<u32> = None;
        while j < data.len() {
            match data[j] {
                d @ b'0'..=b'9' => cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add((d - b'0') as u32)),
                b';' => params.push(cur.take().unwrap_or(0)),
                _ => break,
            }
            j += 1;
        }
        if let Some(c) = cur {
            params.push(c);
        }
        if let Some(&f) = data.get(j)
            && (f == b'h' || f == b'l')
            && params.iter().any(|p| matches!(p, 47 | 1047 | 1049))
        {
            return Some(f == b'h');
        }
        i = j.max(i + 1);
    }
    None
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

/// Replay bytes from a persisted output.log (last `RING_CAP` bytes; the file itself may be
/// up to twice that between compactions).
pub fn read_log(path: &Path) -> Vec<u8> {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // A log rewritten from a wrapped ring is exactly RING_CAP bytes and starts mid-line; longer
    // ones are read from the middle. Either way, start after the first newline.
    let cut = len >= RING_CAP as u64;
    if len > RING_CAP as u64 && f.seek(SeekFrom::Start(len - RING_CAP as u64)).is_err() {
        return Vec::new();
    }
    let mut data = Vec::with_capacity(len.min(RING_CAP as u64) as usize);
    if f.take(RING_CAP as u64).read_to_end(&mut data).is_err() {
        return Vec::new();
    }
    if cut && let Some(pos) = data.iter().take(64 * 1024).position(|&c| c == b'\n') {
        data.drain(..=pos);
    }
    data
}

/// Client replay of a persisted output.log: its last `RING_CAP` bytes (or `max_body`),
/// prefixed like a live replay with the terminal modes and alternate-screen state they leave
/// behind.
pub fn log_replay(path: &Path, cols: u16, rows: u16, max_body: Option<usize>) -> Vec<u8> {
    let mut data = read_log(path);
    if data.is_empty() {
        return data;
    }
    let (cols, rows) = clamp_size(cols, rows);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(&data))).is_ok();
    if let Some(max) = max_body {
        tail_from_line(&mut data, max);
    }
    if !parsed {
        return data;
    }
    // A compacted log may start inside the alternate screen; `replay_prefix` detects that from
    // the first switch in the bytes.
    let mut v = replay_prefix(parser.screen(), &data);
    v.extend_from_slice(&data);
    v
}

/// Keep at most the last `max` bytes, starting after a newline so the cut doesn't land inside
/// an escape sequence or a UTF-8 character (if there is none in the first 64 KiB of the tail,
/// the cut stays where it is).
fn tail_from_line(v: &mut Vec<u8>, max: usize) {
    if v.len() <= max {
        return;
    }
    v.drain(..v.len() - max);
    if let Some(pos) = v.iter().take(64 * 1024).position(|&c| c == b'\n') {
        v.drain(..=pos);
    }
}

/// Plain-text tail of a persisted output.log (for exited sessions from a previous daemon).
pub fn log_tail_text(path: &Path, cols: u16, rows: u16, n: usize) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let data = read_log(path);
    let (cols, rows) = clamp_size(cols, rows);
    let mut parser = vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, TitleCb::default());
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(&data))).ok()?;
    Some(tail_text(&mut parser, n))
}

/// Screen snapshot rebuilt from a persisted output.log.
pub fn log_screen(path: &Path, cols: u16, rows: u16) -> Option<ScreenSnapshot> {
    if !path.exists() {
        return None;
    }
    let data = read_log(path);
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

/// Unmodified cursor keys (`CSI A`..`CSI D`, `CSI H`, `CSI F`) as a keyboard sends them in
/// application cursor mode (`SS3 A`...). Modified keys (`CSI 1;5A`) are left alone.
pub fn to_application_cursor(data: &str) -> String {
    let mut out = data.to_string();
    for k in ['A', 'B', 'C', 'D', 'H', 'F'] {
        out = out.replace(&format!("\x1b[{k}"), &format!("\x1bO{k}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_keys_in_application_mode() {
        assert_eq!(to_application_cursor("\x1b[A\x1b[D\x1b[H"), "\x1bOA\x1bOD\x1bOH");
        assert_eq!(to_application_cursor("\x1b[1;5A\r"), "\x1b[1;5A\r", "modified keys stay CSI");
        assert_eq!(to_application_cursor("plain"), "plain");
    }

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
                clear_env: false,
                env_remove: vec![],
                cwd: std::env::temp_dir(),
                cols: 80,
                rows: 24,
                log_path: dir.join("output.log"),
            },
            Arc::new(|_| {}),
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

    fn test_spawn(dir: &Path, env: &[(&str, &str)]) -> Arc<PtySession> {
        spawn(
            SpawnParams {
                id: 1,
                generation: 1,
                program: "bash".into(),
                args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
                env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                clear_env: false,
                env_remove: vec![],
                cwd: std::env::temp_dir(),
                cols: 80,
                rows: 24,
                log_path: dir.join("output.log"),
            },
            Arc::new(|_| {}),
        )
        .expect("spawn bash")
    }

    fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn kill_reaps_background_jobs_after_the_shell_exited() {
        let dir = std::env::temp_dir().join(format!("muster-pty-leftover-{}", std::process::id()));
        let marker = format!("leftover-test-{}", std::process::id());
        let sess = test_spawn(&dir, &[("MUSTER_RUN", &marker), ("MUSTER_SESSION_ID", "1")]);
        let sid = sess.pid.unwrap() as i32;
        std::thread::sleep(Duration::from_millis(300));
        sess.write(b"sleep 777 &\r").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        sess.write(b"exit\r").unwrap();
        wait_until("the shell to exit", 5, || !sess.is_alive());
        // The background job keeps the terminal open: reader and master are still there.
        assert!(!session_members(sid).is_empty(), "sleep not in the session");
        assert!(!sess.is_drained());
        assert!(sess.has_processes());
        sess.kill();
        wait_until("leftovers to die", 6, || session_members(sid).is_empty());
        wait_until("the reader to see EOF", 3, || sess.is_drained());
        assert!(sess.master.lock().is_none(), "master fd not released");
        assert!(!sess.has_processes());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leftovers_after_reap_need_the_run_markers() {
        let dir = std::env::temp_dir().join(format!("muster-pty-g2-{}", std::process::id()));
        let run = format!("g2-{}", std::process::id());
        let sess = test_spawn(&dir, &[("MUSTER_RUN", &run), ("MUSTER_SESSION_ID", "1")]);
        let sid = sess.pid.unwrap() as i32;
        std::thread::sleep(Duration::from_millis(300));
        // One marked job, one that cleared its env and detached from the terminal (it can't
        // be proven ours once the leader is gone), then the shell exits.
        sess.write(b"sleep 771 & env -i /bin/sh -c 'exec sleep 772' </dev/null >/dev/null 2>&1 &\r")
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        sess.write(b"exit\r").unwrap();
        wait_until("the shell to exit", 5, || sess.reaped.load(Ordering::SeqCst));
        let members = session_members(sid);
        assert_eq!(members.len(), 2, "{members:?}");
        let marked: Vec<i32> = members
            .iter()
            .copied()
            .filter(|&p| has_env(p, &[&format!("MUSTER_RUN={run}")]))
            .collect();
        assert_eq!(marked.len(), 1);
        // `sleep 771` still holds the tty and carries the marker; the env-less one holds no
        // tty and has no marker: after the reap it is never a target.
        let t = sess.targets(sid);
        assert!(t.contains(&marked[0]));
        let unmarked: Vec<i32> = members.iter().copied().filter(|p| !marked.contains(p)).collect();
        assert!(!t.contains(&unmarked[0]), "unverified process targeted: {t:?}");
        sess.kill_now();
        wait_until("the marked job to die", 5, || {
            !std::path::Path::new(&format!("/proc/{}", marked[0])).exists()
        });
        assert!(std::path::Path::new(&format!("/proc/{}", unmarked[0])).exists());
        // SAFETY: test cleanup of the process this test started.
        unsafe {
            libc::kill(unmarked[0], libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn startup_reaper_kills_only_marked_leftovers_and_waits() {
        let run = format!("reap-{}", std::process::id());
        // A detached session (setsid) holding a SIGHUP-ignoring marked process, like one a
        // crashed daemon left behind.
        let mut child = std::process::Command::new("setsid")
            .args(["-w", "sh", "-c", "trap '' HUP; sleep 600 & wait"])
            .env("MUSTER_RUN", &run)
            .env("MUSTER_SESSION_ID", "9")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let sh = child.id() as i32;
        let sid = proc_session_id(sh).unwrap();
        assert!(session_members(sid).len() >= 2);
        let wrong = reap_leftovers(vec![(sid as u32, 9, vec!["MUSTER_RUN=other".into()])]);
        assert!(wrong.is_empty());
        assert!(session_members(sid).len() >= 2, "unmarked processes were killed");
        let left = reap_leftovers(vec![(
            sid as u32,
            9,
            vec![format!("MUSTER_RUN={run}"), "MUSTER_SESSION_ID=9".into()],
        )]);
        // It returns only after they are gone (SIGHUP is ignored, so via SIGKILL).
        assert!(left.is_empty());
        let _ = child.wait();
        assert!(session_members(sid).is_empty());
    }

    #[test]
    fn output_log_appends_and_compacts() {
        let dir = std::env::temp_dir().join(format!("muster-pty-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sess = test_spawn(&dir, &[]);
        sess.kill_now();
        wait_until("exit", 5, || !sess.is_alive());
        let path = dir.join("output.log");
        let _ = std::fs::remove_file(&path);
        sess.mark_dirty();
        sess.ingest(b"hello\n");
        sess.flush_log();
        let first = std::fs::read(&path).unwrap();
        assert!(first.ends_with(b"hello\n"));
        // Appends only the new bytes.
        sess.ingest(b"world\n");
        sess.flush_log();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(second.len(), first.len() + 6);
        assert!(second.ends_with(b"hello\nworld\n"));
        // Unchanged: no write at all.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        sess.flush_log();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        // Grows past 2x the cap across flushes, then gets compacted to the ring.
        let line = vec![b'x'; 1023].into_iter().chain([b'\n']).collect::<Vec<u8>>();
        for _ in 0..3 {
            for _ in 0..(RING_CAP / 1024 - 16) {
                sess.ingest(&line);
            }
            sess.flush_log();
            assert!(std::fs::metadata(&path).unwrap().len() <= 2 * RING_CAP as u64);
        }
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len <= 2 * RING_CAP as u64 && len >= RING_CAP as u64 / 2, "{len}");
        // Replay reads never return more than the cap, starting at a line.
        let replay = read_log(&path);
        assert!(replay.len() <= RING_CAP);
        assert_eq!(replay[0], b'x');
        // More output than the ring holds between flushes: rewritten, not appended.
        for _ in 0..(RING_CAP / 1024 + 64) {
            sess.ingest(&line);
        }
        // A ring that doesn't end on a line boundary: the log holds it byte for byte (G10),
        // and only readers skip the partial first line.
        sess.ingest(b"tail-without-newline");
        sess.flush_log_final();
        let ring = PtySession::raw_locked(&sess.out.lock());
        assert_eq!(std::fs::read(&path).unwrap(), ring);
        assert_eq!(ring.len(), RING_CAP);
        let replay = read_log(&path);
        assert!(replay.ends_with(b"tail-without-newline"));
        assert_eq!(
            replay.len(),
            ring.len() - (ring.iter().position(|&c| c == b'\n').unwrap() + 1)
        );
        // Appending after the rewrite keeps the file equal to everything since.
        sess.ingest(b"\nmore\n");
        sess.flush_log_final();
        assert!(std::fs::read(&path).unwrap().ends_with(b"tail-without-newline\nmore\n"));
        assert!(std::fs::metadata(&path).unwrap().len() <= RING_CAP as u64 + 6);
        assert!(sess.log_persisted());
        sess.discard();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daemon_answers_terminal_queries() {
        let mut p = parser_with(b"ab\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?6n\x1b[>1c\x1b[?u", 5);
        let r: Vec<String> = std::mem::take(&mut p.callbacks_mut().replies)
            .into_iter()
            .map(|r| String::from_utf8(r).unwrap())
            .collect();
        assert_eq!(
            r,
            ["\x1b[?1;2c", "\x1b[>0;276;0c", "\x1b[0n", "\x1b[1;3R", "\x1b[?1;3R"]
        );
    }

    #[test]
    fn tail_from_line_cuts_at_a_newline() {
        let mut v = b"aaaa\nbbbb\ncccc".to_vec();
        tail_from_line(&mut v, 100);
        assert_eq!(v, b"aaaa\nbbbb\ncccc");
        tail_from_line(&mut v, 7);
        assert_eq!(v, b"cccc");
        let mut v = b"no newline here".to_vec();
        tail_from_line(&mut v, 4);
        assert_eq!(v, b"here");
    }

    #[test]
    fn replay_prefix_restores_modes_and_alt_screen() {
        // Modes set long ago (not in the replayed bytes) come back via the prefix.
        let p = parser_with(b"\x1b[?2004h\x1b[?1h\x1b[?1000h", 5);
        let pre = replay_prefix(p.screen(), b"plain output\r\n");
        let s = String::from_utf8_lossy(&pre);
        assert!(
            s.contains("\x1b[?2004h") && s.contains("\x1b[?1h") && s.contains("\x1b[?1000h"),
            "{s:?}"
        );
        assert!(!s.contains("1049"));
        // Ring starts inside the alternate screen (its switch was cut off).
        let p = parser_with(b"\x1b[?1049hfull screen app", 5);
        assert!(replay_prefix(p.screen(), b"more app output").starts_with(b"\x1b[?1049h"));
        // Ring holds the switch itself (not wrapped): no extra switch.
        assert!(!replay_prefix(p.screen(), b"shell\r\n\x1b[?1049happ").starts_with(b"\x1b[?1049h"));
        // Ring starts in the alt screen and leaves it later.
        let p = parser_with(b"\x1b[?1049happ\x1b[?1049lback", 5);
        assert!(replay_prefix(p.screen(), b"app\x1b[?1049lback").starts_with(b"\x1b[?1049h"));
        assert_eq!(first_alt_switch(b"x\x1b[?25;1049hy"), Some(true));
        assert_eq!(first_alt_switch(b"x\x1b[?25h\x1b[?47l"), Some(false));
        assert_eq!(first_alt_switch(b"\x1b[?2004h"), None);
    }

    #[test]
    fn title_callback() {
        let mut p = parser_with(b"\x1b]0;hello\x07", 5);
        assert_eq!(p.callbacks_mut().title.as_deref(), Some("hello"));
        // An oversized title is capped on a UTF-8 boundary, never stored whole.
        let mut long = b"\x1b]2;".to_vec();
        long.extend("\u{e9}".repeat(MAX_TITLE_BYTES).as_bytes());
        long.push(7);
        let mut p = parser_with(&long, 5);
        let title = p.callbacks_mut().title.clone().unwrap();
        assert!(title.len() <= MAX_TITLE_BYTES && title.len() >= MAX_TITLE_BYTES - 1);
        assert!(title.chars().all(|c| c == '\u{e9}'));
    }
}
