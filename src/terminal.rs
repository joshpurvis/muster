//! Application-side terminal handles. The application never owns a PTY descriptor.
//! Cached lifecycle facts support existing synchronous Hub readers; control and output
//! use independent connections to the long-lived host.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Bytes;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use muster_terminal_core::{EventSink, ScreenSnapshot, TerminalEvent};
use muster_terminal_protocol::{client::Client, Command, Response, RunId, SpawnSpec, TerminalStatus, wire::{self, Frame}};

#[derive(Debug)]
pub struct SpawnUncertain(pub String);
impl std::fmt::Display for SpawnUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "terminal creation outcome is unconfirmed; prepared launch retained: {}", self.0)
    }
}
impl std::error::Error for SpawnUncertain {}

#[derive(Clone, Debug)]
pub enum Output {
    Bytes(Bytes),
    Replay(Bytes),
}

#[derive(Debug)]
pub struct TerminalService {
    pub client: Client,
    handles: Mutex<BTreeMap<RunId, Weak<Terminal>>>,
    input: Arc<InputLink>,
}

/// One input to deliver, and where to report its outcome (`None`: fire and forget).
struct InputJob {
    run: RunId,
    /// Admission order across all runs (`LinkState::admitted`).
    seq: u64,
    data: Vec<u8>,
    done: Option<std::sync::mpsc::SyncSender<anyhow::Result<()>>>,
}

/// A run whose input may have a gap: some earlier input was lost, refused, dropped or its
/// outcome is unknown. Every later input for that run is refused (never sent after the gap)
/// until the user has looked at the terminal and continues (`InputLink::resume`).
#[derive(Debug, Clone)]
pub struct InputInterrupted {
    pub reason: String,
}
impl std::fmt::Display for InputInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "typing into this terminal is paused because earlier input may not have reached it ({}); \
            check the terminal, then choose Continue typing", self.reason)
    }
}
impl std::error::Error for InputInterrupted {}

/// Called once when a run's input becomes interrupted. It runs on the input path or the
/// input thread, so it must only hand the event on.
pub type InputInterruptHook = Arc<dyn Fn(RunId) + Send + Sync>;

struct LinkState {
    /// None once the link is closed (backend shutdown): input is refused from then on.
    tx: Option<std::sync::mpsc::Sender<InputJob>>,
    /// Bytes admitted and not finished yet (queued or in flight).
    bytes: usize,
    /// Jobs admitted and not finished yet, in all and per run.
    jobs: usize,
    pending: BTreeMap<RunId, usize>,
    /// The sequence number of the last admitted job.
    admitted: u64,
    /// Interrupted runs: why, and the last admitted job that is still before the gap (jobs
    /// of that run admitted later are never sent).
    interrupted: BTreeMap<RunId, (String, u64)>,
}

struct LinkShared {
    state: Mutex<LinkState>,
    /// Signalled whenever a job finishes (budget freed, drain progress).
    finished: parking_lot::Condvar,
    hook: std::sync::OnceLock<InputInterruptHook>,
}

impl LinkShared {
    /// Mark `run` interrupted; returns whether it was newly marked (the caller then runs the
    /// hook after releasing the state lock).
    /// The gap is after job `before_gap`: earlier jobs of the run still go, later ones not.
    fn interrupt(state: &mut LinkState, run: RunId, reason: &str, before_gap: u64) -> bool {
        if state.interrupted.contains_key(&run) { return false }
        state.interrupted.insert(run, (reason.to_owned(), before_gap));
        true
    }
    fn announce(&self, run: RunId) {
        if let Some(hook) = self.hook.get() { hook(run) }
    }
}

/// Every terminal's input goes through one persistent host connection, in order, on its
/// own thread: a keystroke costs one frame each way instead of a connect, accept and
/// handshake, and the caller (the client's WebSocket reader) never blocks on the host.
///
/// Input is never replayed and never crosses a gap. Any input whose delivery failed or is
/// unknown (connection lost after the frame was written, the host refused it, it could not
/// be sent, it did not fit the byte budget, or the backend is stopping) interrupts its run:
/// input already queued for that run behind it fails unsent, later input is refused, and
/// the application tells the user (`InputInterruptHook`) until they continue. Other runs
/// are unaffected: their earlier input was all answered before the failure.
pub struct InputLink {
    shared: Arc<LinkShared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for InputLink {
    // The input thread ends once every handle is gone and its queue is settled.
    fn drop(&mut self) { self.close() }
}

impl std::fmt::Debug for InputLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputLink").finish_non_exhaustive()
    }
}

/// Bytes admitted but not yet answered by the host, across all runs. Keystrokes past it are
/// refused (and interrupt their run) instead of growing memory behind a stalled host.
const INPUT_BUDGET: usize = 8 * 1024 * 1024;
/// Waiting callers give up after this; the input itself may still be delivered.
const INPUT_WAIT: Duration = Duration::from_secs(15);

/// The worker's host connection: one persistent input stream, or one call per input for a
/// few seconds after the stream could not be opened (an older host, or stream admission
/// full). Both report the same classified outcome.
struct InputConnection {
    client: Client,
    stream: Option<std::os::unix::net::UnixStream>,
    single_until: Option<Instant>,
}

impl InputConnection {
    fn deliver(&mut self, command: &Command) -> Result<(), muster_terminal_protocol::client::StreamError> {
        use muster_terminal_protocol::client::StreamError;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let single = self.single_until.is_some_and(|until| Instant::now() < until);
            if self.stream.is_none() && !single {
                match self.client.input_stream() {
                    Ok(opened) => self.stream = Some(opened),
                    // Nothing was sent on the failed connection.
                    Err(_) => self.single_until = Some(Instant::now() + Duration::from_secs(5)),
                }
            }
            let outcome = match self.stream.as_mut() {
                Some(open) => Client::stream_input(open, command),
                None => self.client.call_input(command),
            };
            match outcome {
                // The whole frame never reached the host (an idle stream closed by the host
                // side, a refused connect): sending it once more cannot duplicate it.
                Err(StreamError::Unsent(_)) if attempt < 2 => { self.stream = None; }
                Err(error @ (StreamError::Unsent(_) | StreamError::Lost(_))) => {
                    self.stream = None;
                    return Err(error);
                }
                other => return other,
            }
        }
    }
}

impl InputLink {
    fn start(client: Client) -> anyhow::Result<Arc<Self>> {
        let (tx, rx) = std::sync::mpsc::channel::<InputJob>();
        let shared = Arc::new(LinkShared {
            state: Mutex::new(LinkState { tx: Some(tx), bytes: 0, jobs: 0, pending: BTreeMap::new(), admitted: 0,
                interrupted: BTreeMap::new() }),
            finished: parking_lot::Condvar::new(),
            hook: std::sync::OnceLock::new(),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new().name("terminal-input".into()).spawn(move || {
            use muster_terminal_protocol::client::StreamError;
            let shared = worker_shared;
            let mut connection = InputConnection { client, stream: None, single_until: None };
            // Ends once the link is closed and every admitted job has been settled.
            while let Ok(job) = rx.recv() {
                let length = job.data.len();
                let run = job.run;
                let gap = shared.state.lock().interrupted.get(&run)
                    .filter(|(_, before_gap)| job.seq > *before_gap).map(|(reason, _)| reason.clone());
                let result = match gap {
                    // Queued behind input that failed: never sent after the gap.
                    Some(reason) => Err(anyhow::Error::new(InputInterrupted { reason })),
                    None => match connection.deliver(&Command::Input { run, data: job.data }) {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            let (reason, error) = match error {
                                StreamError::Unsent(error) => ("the terminal host could not be reached", error),
                                StreamError::Host(error) => ("the terminal host refused it", error),
                                StreamError::Lost(error) => ("the terminal host connection was lost; it may or may not have arrived", error),
                            };
                            // Everything of this run admitted after it is behind the gap.
                            let new = LinkShared::interrupt(&mut shared.state.lock(), run, reason, job.seq - 1);
                            if new { shared.announce(run) }
                            Err(error.context(InputInterrupted { reason: reason.into() }))
                        }
                    },
                };
                match job.done {
                    Some(done) => { let _ = done.try_send(result); }
                    None => if let Err(error) = result { tracing::debug!(?run, "terminal input: {error:#}") },
                }
                let mut state = shared.state.lock();
                state.bytes -= length;
                state.jobs -= 1;
                if let Some(count) = state.pending.get_mut(&run) {
                    *count -= 1;
                    if *count == 0 { state.pending.remove(&run); }
                }
                drop(state);
                shared.finished.notify_all();
            }
        })?;
        Ok(Arc::new(Self { shared, worker: Mutex::new(Some(worker)) }))
    }

    /// Install the application's interruption hook (once).
    pub fn on_interrupt(&self, hook: InputInterruptHook) {
        let _ = self.shared.hook.set(hook);
    }

    /// Why `run`'s input is interrupted, if it is.
    pub fn interrupted(&self, run: RunId) -> Option<String> {
        self.shared.state.lock().interrupted.get(&run).map(|(reason, _)| reason.clone())
    }

    /// Every interrupted run, with its reason.
    pub fn interrupted_runs(&self) -> Vec<(RunId, String)> {
        self.shared.state.lock().interrupted.iter().map(|(run, (reason, _))| (*run, reason.clone())).collect()
    }

    /// Restore an interruption recorded before a backend restart (no announcement).
    pub fn restore_interrupted(&self, run: RunId, reason: &str) {
        LinkShared::interrupt(&mut self.shared.state.lock(), run, reason, 0);
    }

    /// The user has checked the terminal: accept this session's input again (every run).
    /// Returns whether anything was interrupted.
    pub fn resume(&self, id: u32) -> bool {
        let mut state = self.shared.state.lock();
        let before = state.interrupted.len();
        state.interrupted.retain(|run, _| run.id != id);
        before != state.interrupted.len()
    }

    /// Refuse all further input (backend shutdown). Admitted input is still delivered.
    pub fn close(&self) {
        self.shared.state.lock().tx = None;
    }

    pub fn is_closed(&self) -> bool {
        self.shared.state.lock().tx.is_none()
    }

    /// After `close`: wait until every admitted input has been settled and the input thread
    /// has ended, or `deadline` passes. Returns whether it finished. Runs with input still
    /// pending at the deadline are interrupted (and announced): it may never arrive.
    pub fn drain(&self, deadline: Instant) -> bool {
        let mut state = self.shared.state.lock();
        while state.jobs > 0 {
            if self.shared.finished.wait_until(&mut state, deadline).timed_out() { break }
        }
        if state.jobs > 0 {
            let stuck: Vec<RunId> = state.pending.keys().copied().collect();
            let before_gap = state.admitted;
            let new: Vec<RunId> = stuck.into_iter()
                .filter(|run| LinkShared::interrupt(&mut state, *run, "the application backend stopped before it was delivered", before_gap))
                .collect();
            drop(state);
            for run in new { self.shared.announce(run) }
            return false;
        }
        drop(state);
        loop {
            let finished = self.worker.lock().as_ref().is_none_or(|worker| worker.is_finished());
            if finished { break }
            if Instant::now() >= deadline { return false }
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(worker) = self.worker.lock().take() { let _ = worker.join(); }
        true
    }

    fn refuse(&self, mut state: parking_lot::MutexGuard<'_, LinkState>, run: RunId, gap: bool, reason: &str)
        -> anyhow::Error {
        if !gap {
            // Nothing of this call was admitted and the caller hears about it: no gap.
            return anyhow::anyhow!("terminal input was not sent: {reason}");
        }
        // Input admitted before this refusal is before the gap and still goes.
        let before_gap = state.admitted;
        let new = LinkShared::interrupt(&mut state, run, reason, before_gap);
        drop(state);
        if new { self.shared.announce(run) }
        InputInterrupted { reason: reason.into() }.into()
    }

    /// Admit `data` for `run` behind all earlier input. `wait`: block until the host has
    /// answered every chunk (prompt delivery, RPC input); otherwise return at once
    /// (keystrokes), never blocking on the host or the budget. Input that is refused after
    /// part of it was admitted, or keystrokes that are refused at all (nobody waits for their
    /// outcome), interrupt the run.
    fn send(&self, run: RunId, data: &[u8], wait: bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + INPUT_WAIT;
        let mut results = Vec::new();
        let mut state = self.shared.state.lock();
        if let Some((reason, _)) = state.interrupted.get(&run) {
            return Err(InputInterrupted { reason: reason.clone() }.into());
        }
        for (index, chunk) in data.chunks(muster_terminal_protocol::MAX_INPUT).enumerate() {
            let gap = !wait || index > 0;
            // A chunk always fits an empty queue, so input of any size can make progress.
            while state.jobs > 0 && state.bytes + chunk.len() > INPUT_BUDGET {
                if !wait || self.shared.finished.wait_until(&mut state, deadline).timed_out() {
                    return Err(self.refuse(state, run, gap, "too much input was waiting for the terminal host"));
                }
                if let Some((reason, _)) = state.interrupted.get(&run) {
                    return Err(InputInterrupted { reason: reason.clone() }.into());
                }
            }
            let (done, result) = if wait {
                let (done, result) = std::sync::mpsc::sync_channel(1);
                (Some(done), Some(result))
            } else {
                (None, None)
            };
            if state.tx.is_none() {
                return Err(self.refuse(state, run, gap, "the application backend is stopping"));
            }
            let seq = state.admitted + 1;
            let job = InputJob { run, seq, data: chunk.to_vec(), done };
            if !state.tx.as_ref().is_some_and(|tx| tx.send(job).is_ok()) {
                return Err(self.refuse(state, run, gap, "the terminal input thread stopped"));
            }
            state.admitted = seq;
            state.bytes += chunk.len();
            state.jobs += 1;
            *state.pending.entry(run).or_default() += 1;
            results.extend(result);
        }
        drop(state);
        for result in results {
            let left = deadline.saturating_duration_since(Instant::now());
            // A timeout is not a gap: the input stays queued in order and later input follows it.
            result.recv_timeout(left).map_err(|_| anyhow::anyhow!("terminal input unconfirmed"))??;
        }
        Ok(())
    }
}

impl TerminalService {
    pub fn connect(socket: &Path, expected_data: &Path) -> anyhow::Result<Arc<Self>> {
        let client = Client::connect(socket)?;
        anyhow::ensure!(client.hello.data_dir == std::fs::canonicalize(expected_data)?,
            "terminal host belongs to a different data directory");
        let input = InputLink::start(client.clone())?;
        Ok(Arc::new(Self { client, handles: Mutex::new(BTreeMap::new()), input }))
    }

    /// The shared input connection (interruption state, shutdown).
    pub fn input(&self) -> &Arc<InputLink> { &self.input }

    pub fn inventory(&self) -> anyhow::Result<Vec<TerminalStatus>> {
        match self.client.call(Command::Inventory)? {
            Response::Inventory(statuses) => Ok(statuses),
            _ => anyhow::bail!("invalid terminal host inventory response"),
        }
    }

    pub fn spawn(&self, spec: SpawnSpec) -> anyhow::Result<Arc<Terminal>> {
        let mut result = self.client.call(Command::Spawn(spec.clone()));
        if result.as_ref().is_err_and(|e| e.downcast_ref::<muster_terminal_protocol::client::HostError>().is_none()) {
            // Only spawn has a retry-safe identity. A lost acknowledgement must
            // never become an unrelated second process.
            result = self.client.call(Command::Spawn(spec));
        }
        match result {
            Ok(Response::Status(status)) => Ok(self.adopt(status)),
            Ok(_) => Err(SpawnUncertain("invalid terminal host spawn response".into()).into()),
            Err(error) if error.downcast_ref::<muster_terminal_protocol::client::HostError>()
                .is_some_and(|e| e.code == "operation") => Err(error),
            Err(error) => Err(SpawnUncertain(format!("{error:#}")).into()),
        }
    }

    pub fn adopt(&self, status: TerminalStatus) -> Arc<Terminal> {
        let run = status.run;
        let handle = Arc::new(Terminal {
            id: run.id, generation: run.generation, pid: status.pid,
            client: self.client.clone(), input: self.input.clone(), status: Mutex::new(status),
        });
        self.handles.lock().insert(run, Arc::downgrade(&handle));
        handle
    }

    /// Poll generic host facts independently of UI clients. Losing the host connection
    /// preserves the last known lifecycle instead of falsely announcing process exits.
    /// Detection interprets current screen snapshots separately in the application.
    pub fn watch(self: &Arc<Self>, events: EventSink, unavailable: Arc<dyn Fn(String) + Send + Sync>) -> anyhow::Result<()> {
        // Titles can change twice between inventory polls (an application title then
        // the shell prompt). Consume every event so useful transient titles survive.
        let mut stream = self.client.subscribe(Command::Events)?;
        let event_service = Arc::downgrade(self);
        let title_events = events.clone();
        std::thread::Builder::new().name("terminal-events".into()).spawn(move || {
            loop {
                if event_service.strong_count() == 0 { return }
                match wire::read(&mut stream) {
                    Ok(Frame::Ping) => {},
                    Ok(frame) => match frame.decode::<Response>() {
                        Ok(Response::Event(event @ TerminalEvent::Title { .. })) => title_events(event),
                        Ok(Response::Resync) => {
                            if let Some(service) = event_service.upgrade() {
                                if let Ok(statuses) = service.inventory() {
                                    for status in statuses {
                                        title_events(TerminalEvent::Title { id: status.run.id,
                                            generation: status.run.generation, title: status.title });
                                    }
                                }
                            }
                        }
                        _ => {},
                    },
                    Err(_) => loop {
                        let Some(service) = event_service.upgrade() else { return };
                        if let Ok(next) = service.client.subscribe(Command::Events) {
                            stream = next;
                            break;
                        }
                        drop(service);
                        std::thread::sleep(Duration::from_millis(250));
                    },
                }
            }
        })?;
        let weak = Arc::downgrade(self);
        std::thread::Builder::new().name("terminal-inventory".into()).spawn(move || {
            let mut reported = false;
            loop {
                let Some(service) = weak.upgrade() else { return };
                match service.inventory() {
                    Ok(statuses) => {
                        reported = false;
                        let handles = {
                            let mut handles = service.handles.lock();
                            handles.retain(|_, handle| handle.strong_count() > 0);
                            handles.clone()
                        };
                        for status in statuses {
                            if let Some(handle) = handles.get(&status.run).and_then(Weak::upgrade) {
                                handle.observe(status, &events);
                            }
                        }
                    }
                    Err(error) if !reported => {
                        unavailable(format!("Terminal host unavailable; terminal state is unconfirmed: {error:#}"));
                        reported = true;
                    }
                    Err(_) => {}
                }
                drop(service);
                std::thread::sleep(Duration::from_millis(200));
            }
        })?;
        Ok(())
    }
}

pub struct Terminal {
    pub id: u32,
    pub generation: u64,
    pub pid: Option<u32>,
    client: Client,
    input: Arc<InputLink>,
    status: Mutex<TerminalStatus>,
}

impl Terminal {
    pub fn run(&self) -> RunId { RunId { id: self.id, generation: self.generation } }
    pub fn is_alive(&self) -> bool { self.status.lock().alive }
    pub fn seq(&self) -> u64 { self.status.lock().output_seq }
    pub fn last_output_at(&self) -> i64 { self.status.lock().last_output_at }
    pub fn application_cursor(&self) -> bool { self.status.lock().application_cursor }
    pub fn exit_code(&self) -> Option<i32> { self.status.lock().exit_code }
    pub fn title(&self) -> Option<String> { self.status.lock().title.clone() }
    /// The run is alive or left processes behind, per the last host facts. An archived
    /// run kept only as a replay handle owns nothing.
    pub fn owns_processes(&self) -> bool {
        let status = self.status.lock();
        status.alive || status.has_processes
    }

    fn observe(&self, status: TerminalStatus, events: &EventSink) {
        let previous = std::mem::replace(&mut *self.status.lock(), status.clone());
        if status.title != previous.title {
            events(TerminalEvent::Title { id: self.id, generation: self.generation, title: status.title });
        }
        if let Some(code) = status.exit_code.filter(|code| Some(*code) != previous.exit_code) {
            events(TerminalEvent::Exited { id: self.id, generation: self.generation, code });
        }
        if status.drained && !previous.drained {
            events(TerminalEvent::Drained { id: self.id, generation: self.generation });
        }
    }

    /// Deliver input and wait for the host to accept it (prompt delivery, RPC input).
    pub fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.input.send(self.run(), bytes, true)
    }
    /// Queue keystrokes behind any earlier input without waiting for the host. Byte order
    /// across `write` and `send_keys` is the order of the calls.
    pub fn send_keys(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.input.send(self.run(), bytes, false)
    }
    /// Why this run's input is paused, if it is (see `InputInterrupted`).
    pub fn input_interrupted(&self) -> Option<String> {
        self.input.interrupted(self.run())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.client.call(Command::Resize { run: self.run(), cols, rows })?;
        Ok(())
    }
    pub fn screen(&self) -> Option<ScreenSnapshot> {
        match self.client.call(Command::Screen { run: self.run() }).ok()? {
            Response::Screen(screen) => Some(screen),
            _ => None,
        }
    }
    pub fn recent_text(&self, lines: usize) -> Option<String> {
        match self.client.call(Command::RecentText { run: self.run(), lines }).ok()? {
            Response::Text(text) => Some(text),
            _ => None,
        }
    }
    pub fn has_processes(&self) -> bool {
        match self.client.call(Command::Status { run: self.run() }) {
            Ok(Response::Status(status)) => status.has_processes,
            // Unavailability is never evidence that a worktree or process can be removed.
            _ => true,
        }
    }
    pub fn kill(&self) {
        if let Err(error) = self.terminate(false) { tracing::warn!(session = self.id, "terminating terminal: {error:#}"); }
    }
    pub fn kill_now(&self) {
        if let Err(error) = self.terminate(true) { tracing::warn!(session = self.id, "terminating terminal: {error:#}"); }
    }
    pub fn terminate(&self, force: bool) -> anyhow::Result<()> {
        self.client.call(Command::Terminate { run: self.run(), force })?;
        Ok(())
    }
    pub fn forget(&self) -> anyhow::Result<()> {
        self.client.call(Command::Forget { run: self.run() })?;
        Ok(())
    }

    pub fn attach(&self, max_body: Option<usize>) -> anyhow::Result<(Vec<u8>, broadcast::Receiver<Output>)> {
        let mut stream = self.client.subscribe(Command::Attach { run: self.run(), replay_bytes: max_body })?;
        let (replay, mut offset) = match wire::read(&mut stream)? {
            Frame::Replay { next_offset, data } => (data, next_offset),
            _ => anyhow::bail!("terminal host omitted initial replay"),
        };
        let (tx, rx) = broadcast::channel(1024);
        let client = self.client.clone();
        let run = self.run();
        std::thread::Builder::new().name(format!("terminal-output-{}", self.id)).spawn(move || {
            while tx.receiver_count() > 0 {
                let output = match wire::read(&mut stream) {
                    Ok(Frame::Output { offset: start, data }) if start == offset => {
                        offset += data.len() as u64;
                        Output::Bytes(Bytes::from(data))
                    }
                    Ok(Frame::Replay { next_offset, data }) => {
                        offset = next_offset;
                        Output::Replay(Bytes::from(data))
                    }
                    Ok(Frame::Ping) => continue,
                    _ => {
                        // A stream reset, timeout, or offset gap is recoverable. Reattach
                        // to exactly this host instance and run; never replay input or
                        // silently follow a replacement host. Reset the renderer with a
                        // fresh replay before forwarding any further live bytes.
                        let mut delay = Duration::from_millis(100);
                        loop {
                            if tx.receiver_count() == 0 { return }
                            std::thread::sleep(delay);
                            if let Ok(mut next) = client.subscribe(Command::Attach { run, replay_bytes: max_body }) {
                                if let Ok(Frame::Replay { next_offset, data }) = wire::read(&mut next) {
                                    stream = next;
                                    offset = next_offset;
                                    break Output::Replay(Bytes::from(data));
                                }
                            }
                            delay = (delay * 2).min(Duration::from_secs(2));
                        }
                    }
                };
                if tx.send(output).is_err() { break }
            }
        })?;
        Ok((replay, rx))
    }
}

/// Start a host only if its endpoint is absent/refused. Version errors or a live
/// host using another data directory are hard errors, never reasons to replace it.
/// Connect to (or start) this data directory's terminal host, and make sure no older,
/// monolithic daemon can share the application database with this backend: the host must
/// hold the daemon lock those daemons take to start (see `verify_fence`).
pub fn ensure_host(executable: &Path) -> anyhow::Result<Arc<TerminalService>> {
    let service = connect_or_start_host(executable)?;
    verify_fence(&service.client.hello, &crate::paths::lock_path())?;
    Ok(service)
}

const LEGACY_DAEMON: &str = "an older Muster daemon is running for this runtime directory (it holds daemon.lock); \
    stop it with that build's `muster stop` before starting this one";

/// The answering host must hold `lock` (the file at that path now, by device and inode).
/// A host answers only after taking it, so a starting host is never mistaken for a legacy
/// daemon; a legacy daemon holding it never answers as a host, so it is never mistaken for
/// one. A host that does not report a fence is refused: a lock probed and released here
/// would let an older daemon take it right afterwards and share the database. (No such host
/// has run outside test fixtures: fence reporting shipped with the host's first release.)
pub(crate) fn verify_fence(hello: &muster_terminal_protocol::Hello, lock: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(fence) = &hello.fence else {
        anyhow::bail!("{UNFENCED_HOST}");
    };
    let current = std::fs::metadata(lock).with_context(|| format!("checking {}", lock.display()))?;
    anyhow::ensure!(current.dev() == fence.dev && current.ino() == fence.ino,
        "the terminal host holds a different {} than the file there now (it was deleted or replaced), \
        so an older daemon could start beside this backend; run `muster stop` and start Muster again",
        lock.display());
    Ok(())
}

const UNFENCED_HOST: &str = "the running terminal host was started by an earlier build that does not report \
    holding daemon.lock, so this backend cannot rule out an older Muster daemon sharing its database. \
    Nothing was changed. Run `muster stop` (this ends every terminal) and start Muster again";

fn connect_or_start_host(executable: &Path) -> anyhow::Result<Arc<TerminalService>> {
    let runtime = crate::paths::runtime_dir();
    let data = crate::paths::data_dir().join("terminal");
    let socket = runtime.join("terminal.sock");
    match TerminalService::connect(&socket, &data) {
        Ok(service) => return Ok(service),
        Err(error) => match error.downcast_ref::<std::io::Error>().map(|e| e.kind()) {
            Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {},
            _ => return Err(error),
        },
    }
    crate::paths::ensure_private_dir(&runtime)?;
    anyhow::ensure!(executable.is_file(), "terminal host executable is missing: {}; build with cargo build --workspace", executable.display());
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(runtime.join("terminal.log"))?;
    let mut command = std::process::Command::new(&executable);
    command.arg("--runtime-dir").arg(&runtime).arg("--data-dir").arg(&data)
        // Keeps the monolithic daemon of older builds from starting while this host lives.
        .env("MUSTER_TERMINAL_FENCE_LOCK", crate::paths::lock_path())
        .stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe and runs before exec, as in the daemon launcher.
    unsafe { command.pre_exec(|| {
        if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()) }
        Ok(())
    }); }
    let mut child = command.spawn().context("starting terminal host")?;
    let end = Instant::now() + Duration::from_secs(15);
    let mut relaunch_at: Option<Instant> = None;
    loop {
        if let Ok(service) = TerminalService::connect(&socket, &data) {
            // Reap this child if it ever exits while the backend still runs. Dropping a
            // backend does not terminate the host, which is in its own process session.
            std::thread::spawn(move || { let _ = child.wait(); });
            return Ok(service);
        }
        match child.try_wait()? {
            // Another host owns the runtime directory and is still starting (a backend that
            // launched it may have died): wait for it to answer instead of failing.
            // If that host then exits without answering (its own fence was busy, or it
            // failed), nobody owns the runtime directory: launch again rather than wait out
            // the deadline for a host that is gone.
            Some(status) if status.code() == Some(muster_terminal_protocol::EXIT_HOST_BUSY) => {
                let at = *relaunch_at.get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                if Instant::now() >= at {
                    relaunch_at = None;
                    child = command.spawn().context("starting terminal host")?;
                }
            }
            Some(status) if status.code() == Some(muster_terminal_protocol::EXIT_FENCE_BUSY) => anyhow::bail!(LEGACY_DAEMON),
            Some(status) => {
                // Another launcher may have won the race; accept only a valid handshake.
                if let Ok(service) = TerminalService::connect(&socket, &data) { return Ok(service) }
                anyhow::bail!("terminal host exited ({status}); see {}", runtime.join("terminal.log").display());
            }
            None => {}
        }
        anyhow::ensure!(Instant::now() < end, "terminal host did not become ready; see {}", runtime.join("terminal.log").display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn host_log_path(id: u32, generation: u64) -> PathBuf {
    crate::paths::data_dir().join("terminal/runs").join(format!("{id}-{generation}")).join("output.log")
}

/// Stand-ins for the terminal host, for this module's tests and the hub's.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::os::unix::net::UnixListener;
    use muster_terminal_protocol::{Hello, Request, VERSION};

    /// A scripted stand-in for the terminal host on a private socket. It answers the
    /// handshake, then hands the listener to `script`.
    pub(crate) fn fake_host<T: Send + 'static>(name: &str, script: impl FnOnce(UnixListener) -> T + Send + 'static)
        -> (Arc<TerminalService>, PathBuf, std::thread::JoinHandle<T>) {
        let directory = std::env::temp_dir().join(format!("muster-{name}-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("host.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let hello = Hello { protocol: VERSION, instance: "input-host".into(), pid: std::process::id(),
            data_dir: directory.clone(), fence: None };
        let server = std::thread::spawn(move || {
            let mut handshake = listener.accept().unwrap().0;
            let _: Request = wire::read(&mut handshake).unwrap().decode().unwrap();
            wire::write(&mut handshake, &Frame::json(&Response::Hello(hello)).unwrap()).unwrap();
            script(listener)
        });
        let service = TerminalService::connect(&socket, &directory).unwrap();
        (service, directory, server)
    }

    /// Input a `serving_host` received, in order, and whether it holds its answers.
    #[derive(Default)]
    pub(crate) struct Received {
        pub inputs: Mutex<Vec<(RunId, Vec<u8>)>>,
        pub stall: std::sync::atomic::AtomicBool,
    }

    /// A host that serves a backend: an empty inventory, an idle event stream, and input
    /// streams whose input it records (holding each answer while `stall` is set).
    pub(crate) fn serving_host(name: &str) -> (Arc<TerminalService>, PathBuf, Arc<Received>) {
        let received = Arc::new(Received::default());
        let shared = received.clone();
        let (service, directory, _server) = fake_host(name, move |listener| {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let received = shared.clone();
                std::thread::spawn(move || {
                    let Ok(frame) = wire::read(&mut stream) else { return };
                    let Ok(request) = frame.decode::<Request>() else { return };
                    match request.command {
                        Command::Inventory => { let _ = wire::write(&mut stream, &Frame::json(&Response::Inventory(vec![])).unwrap()); }
                        Command::Events => {
                            let _ = wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap());
                            while wire::write(&mut stream, &Frame::Ping).is_ok() { std::thread::sleep(Duration::from_secs(1)) }
                        }
                        Command::InputStream => {
                            let _ = wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap());
                            while let Ok(frame) = wire::read(&mut stream) {
                                let Ok(Request { command: Command::Input { run, data }, .. }) = frame.decode::<Request>() else { return };
                                while received.stall.load(std::sync::atomic::Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(5)) }
                                received.inputs.lock().push((run, data));
                                if wire::write(&mut stream, &Frame::json(&Response::Ok).unwrap()).is_err() { return }
                            }
                        }
                        _ => { let _ = wire::write(&mut stream, &Frame::json(&Response::Error { code: "operation".into(), message: "unsupported by the fixture".into() }).unwrap()); }
                    }
                });
            }
        });
        (service, directory, received)
    }

    /// A live run's handle, as adopted from the host.
    pub(crate) fn live(service: &TerminalService, id: u32, generation: u64, directory: &Path) -> Arc<Terminal> {
        service.adopt(TerminalStatus { run: RunId { id, generation },
            pid: Some(1), alive: true, drained: false, has_processes: true, exit_code: None,
            title: None, cols: 80, rows: 24, output_seq: 0, last_output_at: 0,
            application_cursor: false, log_persisted: true, cwd: directory.to_owned() })
    }

    pub(crate) fn input_of(frame: Frame) -> (RunId, Vec<u8>) {
        let Command::Input { run, data } = frame.decode::<Request>().unwrap().command else { panic!("not input") };
        (run, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testing::*;
    use std::os::unix::net::UnixListener;
    use muster_terminal_protocol::{Hello, Request, VERSION};

    #[test]
    fn backend_refuses_a_legacy_daemon_and_any_host_without_a_held_fence() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let directory = std::env::temp_dir().join(format!("muster-fence-check-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir(&directory).unwrap();
        let lock = directory.join("daemon.lock");
        let hello = |fence| Hello { protocol: VERSION, instance: "fence-host".into(), pid: 1, data_dir: directory.clone(), fence };
        let take = || {
            let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock).unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
            file
        };
        // A legacy, monolithic daemon holds the lock (its own descriptor, as `muster daemon` does).
        let legacy = take();
        let error = verify_fence(&hello(None), &lock).unwrap_err().to_string();
        assert!(error.contains("does not report holding daemon.lock"), "{error}");
        drop(legacy);
        // A host that does not report a fence is refused even while nobody holds the lock: a
        // probe released here would let an older daemon take it right afterwards.
        let error = verify_fence(&hello(None), &lock).unwrap_err().to_string();
        assert!(error.contains("muster stop"), "{error}");
        // The host reports the lock it holds (a locked descriptor, as a real host keeps it);
        // it must be the file at that path now.
        let held = take();
        let metadata = held.metadata().unwrap();
        let fence = muster_terminal_protocol::FenceLock { path: lock.clone(), dev: metadata.dev(), ino: metadata.ino() };
        verify_fence(&hello(Some(fence.clone())), &lock).unwrap();
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, b"").unwrap();
        assert!(verify_fence(&hello(Some(fence)), &lock).is_err(), "a replaced lock file no longer excludes older daemons");
        drop(held);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn interrupted(error: &anyhow::Error) -> bool {
        error.downcast_ref::<InputInterrupted>().is_some()
    }

    #[test]
    fn input_lost_with_its_connection_interrupts_its_run_and_nothing_queued_behind_it_is_sent() {
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed) = std::sync::mpsc::channel::<()>();
        let (service, directory, server) = fake_host("input-lost", move |listener| {
            let mut seen = vec![];
            for connection in 0..3 {
                let mut stream = listener.accept().unwrap().0;
                let request: Request = wire::read(&mut stream).unwrap().decode().unwrap();
                assert!(matches!(request.command, Command::InputStream));
                wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap()).unwrap();
                loop {
                    let Ok(frame) = wire::read(&mut stream) else { break };
                    let (run, data) = input_of(frame);
                    seen.push((connection, run.id, data.clone()));
                    // The first connection dies after receiving "lost", without answering it,
                    // once the test has queued more input behind it.
                    if connection == 0 && data == b"lost" {
                        release.recv().unwrap();
                        break;
                    }
                    wire::write(&mut stream, &Frame::json(&Response::Ok).unwrap()).unwrap();
                    // The second connection is closed while idle after "after".
                    if connection == 1 && data == b"after" { break }
                }
                if connection == 1 {
                    drop(stream);
                    closed_tx.send(()).unwrap();
                }
            }
            seen
        });
        let (announced_tx, announced) = std::sync::mpsc::channel();
        service.input().on_interrupt(Arc::new(move |run| { let _ = announced_tx.send(run); }));
        let one = live(&service, 1, 7, &directory);
        let two = live(&service, 2, 3, &directory);
        one.send_keys(b"a").unwrap();
        one.write(b"b").unwrap();
        std::thread::scope(|scope| {
            let lost = scope.spawn(|| one.write(b"lost"));
            // Wait until "lost" is in flight, then queue more input behind it: a keystroke and
            // a waiting write for the same run, and a keystroke for another run.
            while service.input().shared.state.lock().jobs == 0 { std::thread::sleep(Duration::from_millis(5)) }
            one.send_keys(b"tail").unwrap();
            let queued = scope.spawn(|| one.write(b"\r"));
            while service.input().shared.state.lock().jobs < 3 { std::thread::sleep(Duration::from_millis(5)) }
            two.send_keys(b"other").unwrap();
            release_tx.send(()).unwrap();
            let error = lost.join().unwrap().unwrap_err();
            assert!(interrupted(&error), "an unanswered input is reported, not retried: {error:#}");
            let error = queued.join().unwrap().unwrap_err();
            assert!(interrupted(&error), "input queued behind the gap fails unsent: {error:#}");
        });
        assert_eq!(announced.recv_timeout(Duration::from_secs(5)).unwrap(), RunId { id: 1, generation: 7 });
        // Later input for that run is refused until the user continues; other runs carry on.
        assert!(interrupted(&one.write(b"enter").unwrap_err()));
        assert!(interrupted(&one.send_keys(b"x").unwrap_err()));
        assert!(one.input_interrupted().is_some());
        two.write(b"two").unwrap();
        assert!(service.input().resume(1));
        one.write(b"after").unwrap();
        closed.recv().unwrap();
        // The write into the closed stream never delivered a frame: sent once on a new one.
        one.write(b"again").unwrap();
        assert!(announced.try_recv().is_err(), "announced once");
        drop((one, two, service));
        let seen = server.join().unwrap();
        assert_eq!(seen, vec![(0, 1, b"a".to_vec()), (0, 1, b"b".to_vec()), (0, 1, b"lost".to_vec()),
            (1, 2, b"other".to_vec()), (1, 2, b"two".to_vec()), (1, 1, b"after".to_vec()), (2, 1, b"again".to_vec())],
            "one connection in order; nothing of run 1 after the gap until it continued; no resend");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn single_call_fallback_never_delivers_input_after_a_lost_one() {
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let (service, directory, server) = fake_host("input-fallback", move |listener| {
            let mut seen = vec![];
            // A host that predates input streams: it closes those without answering, and
            // takes one Input per connection. It dies before answering "lost".
            while seen.len() < 3 {
                let mut stream = listener.accept().unwrap().0;
                let request: Request = wire::read(&mut stream).unwrap().decode().unwrap();
                let Command::Input { run, data } = request.command else { continue };
                seen.push((run.id, data.clone()));
                if data == b"lost" {
                    release.recv().unwrap();
                    continue;
                }
                wire::write(&mut stream, &Frame::json(&Response::Ok).unwrap()).unwrap();
            }
            seen
        });
        let one = live(&service, 1, 1, &directory);
        let two = live(&service, 2, 1, &directory);
        one.write(b"x").unwrap();
        std::thread::scope(|scope| {
            let lost = scope.spawn(|| one.write(b"lost"));
            while service.input().shared.state.lock().jobs == 0 { std::thread::sleep(Duration::from_millis(5)) }
            one.send_keys(b"\r").unwrap();
            two.send_keys(b"y").unwrap();
            release_tx.send(()).unwrap();
            assert!(interrupted(&lost.join().unwrap().unwrap_err()));
        });
        assert!(interrupted(&one.write(b"z").unwrap_err()), "nothing follows the gap");
        let seen = server.join().unwrap();
        assert_eq!(seen, vec![(1, b"x".to_vec()), (1, b"lost".to_vec()), (2, b"y".to_vec())],
            "the Enter queued behind the lost input never ran");
        drop((one, two, service));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keystrokes_past_the_byte_budget_interrupt_their_run_and_later_enter_never_arrives() {
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let (service, directory, server) = fake_host("input-budget", move |listener| {
            let mut stream = listener.accept().unwrap().0;
            let _: Request = wire::read(&mut stream).unwrap().decode().unwrap();
            wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap()).unwrap();
            let mut seen: Vec<(u32, usize)> = vec![];
            loop {
                let Ok(frame) = wire::read(&mut stream) else { break };
                let (run, data) = input_of(frame);
                // A stalled host: the first input is answered only once the queue is full.
                if seen.is_empty() { release.recv().unwrap(); }
                seen.push((run.id, data.len()));
                wire::write(&mut stream, &Frame::json(&Response::Ok).unwrap()).unwrap();
            }
            seen
        });
        let one = live(&service, 1, 1, &directory);
        let two = live(&service, 2, 1, &directory);
        let chunk = vec![b'k'; muster_terminal_protocol::MAX_INPUT];
        for _ in 0..INPUT_BUDGET / chunk.len() { one.send_keys(&chunk).unwrap(); }
        let started = Instant::now();
        assert!(interrupted(&one.send_keys(b"\r").unwrap_err()), "a dropped keystroke interrupts its run");
        assert!(interrupted(&two.send_keys(b"y").unwrap_err()));
        assert!(started.elapsed() < Duration::from_millis(100), "keystrokes never wait for the budget");
        release_tx.send(()).unwrap();
        // Admitted input still arrives in order; nothing after the dropped keystroke does.
        let link = service.input().clone();
        while link.shared.state.lock().jobs > 0 { std::thread::sleep(Duration::from_millis(5)) }
        assert!(interrupted(&one.send_keys(b"\r").unwrap_err()));
        assert!(link.shared.state.lock().bytes == 0);
        drop((one, two, service, link));
        let seen = server.join().unwrap();
        assert_eq!(seen, vec![(1, muster_terminal_protocol::MAX_INPUT); INPUT_BUDGET / muster_terminal_protocol::MAX_INPUT]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn closing_the_link_delivers_admitted_input_and_interrupts_runs_typed_into_afterwards() {
        let (service, directory, server) = fake_host("input-close", move |listener| {
            let mut stream = listener.accept().unwrap().0;
            let _: Request = wire::read(&mut stream).unwrap().decode().unwrap();
            wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap()).unwrap();
            let mut seen = vec![];
            while let Ok(frame) = wire::read(&mut stream) {
                std::thread::sleep(Duration::from_millis(20));
                seen.push(input_of(frame).1);
                wire::write(&mut stream, &Frame::json(&Response::Ok).unwrap()).unwrap();
            }
            seen
        });
        let (announced_tx, announced) = std::sync::mpsc::channel();
        service.input().on_interrupt(Arc::new(move |run| { let _ = announced_tx.send(run); }));
        let one = live(&service, 1, 1, &directory);
        for key in [b"a", b"b", b"c"] { one.send_keys(key).unwrap(); }
        let link = service.input().clone();
        link.close();
        assert!(interrupted(&one.send_keys(b"d").unwrap_err()), "a refused keystroke is never silent");
        assert_eq!(announced.recv_timeout(Duration::from_secs(1)).unwrap().id, 1);
        assert!(link.drain(Instant::now() + Duration::from_secs(5)), "admitted input settles and the thread ends");
        assert_eq!(server.join().unwrap(), vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        drop((one, service, link));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn output_stream_reconnects_with_reset_replay_after_disconnect_or_gap() {
        for offset_gap in [false, true] {
            let directory = std::env::temp_dir().join(format!("muster-stream-test-{}-{}", std::process::id(), rand::random::<u64>()));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("host.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let hello = Hello { protocol: VERSION, instance: "original-host".into(), fence: None,
                pid: std::process::id(), data_dir: directory.clone() };
            let server = std::thread::spawn(move || {
                let accept = || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        if let Ok((stream, _)) = listener.accept() {
                            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                            return stream;
                        }
                        assert!(Instant::now() < deadline, "client did not reconnect");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                };
                let mut handshake = accept();
                let request: Request = wire::read(&mut handshake).unwrap().decode().unwrap();
                assert!(matches!(request.command, Command::Hello));
                wire::write(&mut handshake, &Frame::json(&Response::Hello(hello)).unwrap()).unwrap();
                for attempt in 0..2 {
                    let mut stream = accept();
                    let request: Request = wire::read(&mut stream).unwrap().decode().unwrap();
                    assert_eq!(request.instance.as_deref(), Some("original-host"));
                    assert!(matches!(request.command, Command::Attach { run: RunId { id: 1, generation: 7 }, .. }));
                    wire::write(&mut stream, &Frame::json(&Response::Attached).unwrap()).unwrap();
                    wire::write(&mut stream, &Frame::Replay { next_offset: if attempt == 0 { 1 } else { 2 },
                        data: if attempt == 0 { b"A".to_vec() } else { b"AB".to_vec() } }).unwrap();
                    wire::write(&mut stream, &Frame::Output { offset: if attempt == 0 { 1 } else { 2 },
                        data: if attempt == 0 { b"B".to_vec() } else { b"C".to_vec() } }).unwrap();
                    if attempt == 0 && offset_gap {
                        wire::write(&mut stream, &Frame::Output { offset: 99, data: b"invalid-gap".to_vec() }).unwrap();
                    }
                    // Closing the socket exercises recovery even while the backend stays up.
                }
            });
            let service = TerminalService::connect(&socket, &directory).unwrap();
            let terminal = service.adopt(TerminalStatus { run: RunId { id: 1, generation: 7 },
                pid: Some(1), alive: true, drained: false, has_processes: true, exit_code: None,
                title: None, cols: 80, rows: 24, output_seq: 0, last_output_at: 0,
                application_cursor: false, log_persisted: true, cwd: directory.clone() });
            let (replay, mut rx) = terminal.attach(None).unwrap();
            assert_eq!(replay, b"A");
            for (reset, expected) in [(false, "B"), (true, "AB"), (false, "C")] {
                let output = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
                let (actual_reset, bytes) = match output { Output::Bytes(bytes) => (false, bytes), Output::Replay(bytes) => (true, bytes) };
                assert_eq!(actual_reset, reset);
                assert_eq!(&bytes[..], expected.as_bytes());
            }
            drop(rx);
            server.join().unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
