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
}

impl TerminalService {
    pub fn connect(socket: &Path, expected_data: &Path) -> anyhow::Result<Arc<Self>> {
        let client = Client::connect(socket)?;
        anyhow::ensure!(client.hello.data_dir == std::fs::canonicalize(expected_data)?,
            "terminal host belongs to a different data directory");
        Ok(Arc::new(Self { client, handles: Mutex::new(BTreeMap::new()) }))
    }

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
            client: self.client.clone(), status: Mutex::new(status),
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

    pub fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        for chunk in bytes.chunks(muster_terminal_protocol::MAX_INPUT) {
            self.client.call(Command::Input { run: self.run(), data: chunk.to_vec() })?;
        }
        Ok(())
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
pub fn ensure_host(executable: &Path) -> anyhow::Result<Arc<TerminalService>> {
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
        .stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe and runs before exec, as in the daemon launcher.
    unsafe { command.pre_exec(|| {
        if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()) }
        Ok(())
    }); }
    let mut child = command.spawn().context("starting terminal host")?;
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(service) = TerminalService::connect(&socket, &data) {
            // Reap this child if it ever exits while the backend still runs. Dropping a
            // backend does not terminate the host, which is in its own process session.
            std::thread::spawn(move || { let _ = child.wait(); });
            return Ok(service);
        }
        if let Some(status) = child.try_wait()? {
            // Another launcher may have won the race; accept only a valid handshake.
            if let Ok(service) = TerminalService::connect(&socket, &data) { return Ok(service) }
            anyhow::bail!("terminal host exited ({status}); see {}", runtime.join("terminal.log").display());
        }
        anyhow::ensure!(Instant::now() < end, "terminal host did not become ready; see {}", runtime.join("terminal.log").display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn host_log_path(id: u32, generation: u64) -> PathBuf {
    crate::paths::data_dir().join("terminal/runs").join(format!("{id}-{generation}")).join("output.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use muster_terminal_protocol::{Hello, Request, VERSION};

    #[tokio::test]
    async fn output_stream_reconnects_with_reset_replay_after_disconnect_or_gap() {
        for offset_gap in [false, true] {
            let directory = std::env::temp_dir().join(format!("muster-stream-test-{}-{}", std::process::id(), rand::random::<u64>()));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("host.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let hello = Hello { protocol: VERSION, instance: "original-host".into(),
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
