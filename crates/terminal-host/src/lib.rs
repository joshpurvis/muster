//! A private local service for generic terminal operations only.
mod archive;
mod ownership;
use ownership::OwnedProcess;
pub use ownership::launch_owned_child;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use muster_terminal_core::{
    private_fs,
    pty::{self, PtySession, SpawnParams},
};
use muster_terminal_protocol::{
    Command, Hello, MAX_INPUT, Request, Response, RunId, SpawnSpec, TerminalEvent, TerminalStatus,
    VERSION,
    wire::{self, Frame},
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct Entry {
    spec_hash: [u8; 32],
    cwd: PathBuf,
    terminal: Terminal,
}

#[derive(Clone)]
enum Terminal {
    Live(Arc<PtySession>),
    Archived(Arc<archive::Archive>),
}

impl Terminal {
    fn has_processes(&self) -> bool {
        match self {
            Self::Live(p) => p.has_processes(),
            Self::Archived(_) => false,
        }
    }
}

struct Attachment {
    replay: Vec<u8>,
    receiver: Option<broadcast::Receiver<bytes::Bytes>>,
    offset: u64,
}

struct Host {
    hello: Hello,
    // Production uses the host's own early exec mode. Core resource unit fixtures
    // can spawn directly; process-boundary tests exercise this guard end to end.
    bootstrap: Option<PathBuf>,
    entries: Mutex<BTreeMap<RunId, Entry>>,
    // One generation watermark per session, rather than one tombstone per restart.
    forgotten: Mutex<BTreeMap<u32, u64>>,
    // Serializes process creation, forgetting and shutdown. Never held by PTY callbacks.
    lifecycle: Mutex<()>,
    events: broadcast::Sender<TerminalEvent>,
    stopping: AtomicBool,
    stop: Notify,
}

impl Host {
    fn get(&self, run: RunId) -> anyhow::Result<Terminal> {
        self.entries
            .lock()
            .get(&run)
            .map(|e| e.terminal.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown terminal run {run:?}"))
    }

    fn status(&self, run: RunId) -> anyhow::Result<TerminalStatus> {
        let (p, cwd) = {
            let entries = self.entries.lock();
            let e = entries.get(&run).context("unknown terminal run")?;
            (e.terminal.clone(), e.cwd.clone())
        };
        let p = match p {
            Terminal::Archived(archive) => return Ok(archive.status.clone()),
            Terminal::Live(p) => p,
        };
        let (cols, rows) = p.size();
        Ok(TerminalStatus {
            run,
            pid: p.pid,
            alive: p.is_alive(),
            drained: p.is_drained(),
            has_processes: p.has_processes(),
            exit_code: p.exit_code(),
            title: p.title(),
            cols,
            rows,
            output_seq: p.seq(),
            last_output_at: p.last_output_at(),
            application_cursor: p.application_cursor(),
            log_persisted: p.log_persisted(),
            cwd,
        })
    }

    fn spawn(&self, spec: SpawnSpec) -> anyhow::Result<Response> {
        let _guard = self.lifecycle.lock();
        anyhow::ensure!(
            !self.stopping.load(Ordering::SeqCst),
            "terminal host is stopping"
        );
        anyhow::ensure!(
            spec.run.id > 0 && spec.run.generation > 0,
            "run identity must be nonzero"
        );
        anyhow::ensure!(spec.cwd.is_absolute(), "launch cwd must be absolute");
        let spec_hash: [u8; 32] = Sha256::digest(serde_json::to_vec(&spec)?).into();
        {
            let entries = self.entries.lock();
            if let Some(e) = entries.get(&spec.run) {
                anyhow::ensure!(
                    e.spec_hash == spec_hash,
                    "run identity was already used with a different launch specification"
                );
                drop(entries);
                return Ok(Response::Status(self.status(spec.run)?));
            }
            anyhow::ensure!(
                self.forgotten
                    .lock()
                    .get(&spec.run.id)
                    .is_none_or(|generation| spec.run.generation > *generation),
                "run identity was already retired or superseded"
            );
            for (run, entry) in entries.iter().filter(|(run, _)| run.id == spec.run.id) {
                anyhow::ensure!(
                    spec.run.generation > run.generation,
                    "stale process generation"
                );
                anyhow::ensure!(
                    !entry.terminal.has_processes(),
                    "previous terminal run still owns processes"
                );
            }
        }
        // Existing runs remain retryable even if their original cwd or executable
        // has since disappeared. Validate filesystem inputs only for a new launch.
        anyhow::ensure!(
            spec.cwd.is_dir(),
            "launch cwd must be an existing directory"
        );
        ownership::check_program(&spec)?;
        let log_dir = self.run_dir(spec.run);
        private_fs::ensure_private_dir(&log_dir)?;
        let mut env = spec.env.clone();
        // Process ownership markers are generated by the host, never accepted from a caller.
        env.insert(
            "MUSTER_RUN".into(),
            format!(
                "{}-{}-{}",
                self.hello.instance, spec.run.id, spec.run.generation
            ),
        );
        env.insert("MUSTER_SESSION_ID".into(), spec.run.id.to_string());
        let mut owned = OwnedProcess {
            run: spec.run,
            pid: None,
            markers: pty::run_markers(&env),
        };
        let journal = log_dir.join("owner.json");
        private_fs::write_private(&journal, &serde_json::to_vec(&owned)?)?;
        let (program, args) = match &self.bootstrap {
            Some(bootstrap) => {
                let mut args = vec![
                    "--owned-child".into(),
                    self.hello.pid.to_string(),
                    journal.to_string_lossy().into_owned(),
                    spec.cwd.to_string_lossy().into_owned(),
                    spec.program.clone(),
                ];
                args.extend(spec.args.clone());
                (bootstrap.to_string_lossy().into_owned(), args)
            }
            None => (spec.program.clone(), spec.args.clone()),
        };
        let events = self.events.clone();
        let pty = pty::spawn(
            SpawnParams {
                id: spec.run.id,
                generation: spec.run.generation,
                program,
                args,
                env,
                clear_env: true,
                env_remove: vec![],
                cwd: spec.cwd.clone(),
                cols: spec.cols,
                rows: spec.rows,
                log_path: log_dir.join("output.log"),
            },
            Arc::new(move |event| {
                let _ = events.send(event);
            }),
        )?;
        // Save process ownership before acknowledging spawn. The journal contains only
        // host-owned process markers, not the launch environment or application tokens.
        owned.pid = pty.pid;
        if let Err(error) = private_fs::write_private(&journal, &serde_json::to_vec(&owned)?) {
            pty.kill_now();
            return Err(error.into());
        }
        let run = spec.run;
        self.entries.lock().insert(
            run,
            Entry {
                spec_hash,
                cwd: spec.cwd,
                terminal: Terminal::Live(pty),
            },
        );
        Ok(Response::Status(self.status(run)?))
    }

    fn run_dir(&self, run: RunId) -> PathBuf {
        self.hello
            .data_dir
            .join("runs")
            .join(format!("{}-{}", run.id, run.generation))
    }

    fn execute(&self, command: Command) -> anyhow::Result<Response> {
        match command {
            Command::Hello => Ok(Response::Hello(self.hello.clone())),
            Command::Inventory => {
                let ids: Vec<_> = self.entries.lock().keys().copied().collect();
                Ok(Response::Inventory(
                    ids.into_iter()
                        .filter_map(|id| self.status(id).ok())
                        .collect(),
                ))
            }
            Command::Spawn(spec) => self.spawn(spec),
            Command::Status { run } => Ok(Response::Status(self.status(run)?)),
            Command::Input { run, data } => {
                anyhow::ensure!(data.len() <= MAX_INPUT, "input exceeds {MAX_INPUT} bytes");
                match self.get(run)? {
                    Terminal::Live(p) => p.write(&data)?,
                    Terminal::Archived(_) => anyhow::bail!("terminal run has exited"),
                }
                Ok(Response::Ok)
            }
            Command::Resize { run, cols, rows } => {
                let _guard = self.lifecycle.lock();
                if let Terminal::Live(p) = self.get(run)? {
                    p.resize(cols, rows)?;
                }
                Ok(Response::Ok)
            }
            Command::Screen { run } => Ok(Response::Screen(match self.get(run)? {
                Terminal::Live(p) => p.screen(),
                Terminal::Archived(archive) => archive.screen()?,
            })),
            Command::RecentText { run, lines } => Ok(Response::Text(match self.get(run)? {
                Terminal::Live(p) => p.recent_text(lines.min(10000)),
                Terminal::Archived(archive) => archive.recent_text(lines.min(10000))?,
            })),
            Command::Terminate { run, force } => {
                if let Terminal::Live(p) = self.get(run)? {
                    if force {
                        p.kill_now();
                    } else {
                        p.kill();
                    }
                }
                Ok(Response::Ok)
            }
            Command::Forget { run } => {
                let _guard = self.lifecycle.lock();
                anyhow::ensure!(
                    run.id > 0 && run.generation > 0,
                    "run identity must be nonzero"
                );
                if !self.entries.lock().contains_key(&run)
                    && self
                        .forgotten
                        .lock()
                        .get(&run.id)
                        .is_some_and(|generation| run.generation <= *generation)
                {
                    return Ok(Response::Ok);
                }
                let terminal = self.entries.lock().get(&run).map(|e| e.terminal.clone());
                if let Some(Terminal::Live(p)) = terminal {
                    anyhow::ensure!(
                        !p.has_processes() && p.is_drained(),
                        "terminal still owns processes or pending output"
                    );
                    p.discard();
                }
                match std::fs::remove_dir_all(self.run_dir(run)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                self.entries.lock().remove(&run);
                self.forgotten
                    .lock()
                    .entry(run.id)
                    .and_modify(|g| *g = (*g).max(run.generation))
                    .or_insert(run.generation);
                Ok(Response::Ok)
            }
            Command::Flush => {
                for p in self.ptys() {
                    p.flush_log_final();
                }
                Ok(Response::Ok)
            }
            Command::Shutdown => anyhow::bail!("shutdown must be acknowledged before stopping"),
            Command::Attach { .. } | Command::Events => {
                anyhow::bail!("streaming command requires streaming connection")
            }
        }
    }

    fn ptys(&self) -> Vec<Arc<PtySession>> {
        self.entries
            .lock()
            .values()
            .filter_map(|e| match &e.terminal {
                Terminal::Live(p) => Some(p.clone()),
                Terminal::Archived(_) => None,
            })
            .collect()
    }

    fn attach(&self, run: RunId, limit: Option<usize>) -> anyhow::Result<Attachment> {
        match self.get(run)? {
            Terminal::Live(p) => {
                let (replay, receiver, offset) = p.attach_sequenced(limit);
                Ok(Attachment {
                    replay,
                    receiver: Some(receiver),
                    offset,
                })
            }
            Terminal::Archived(archive) => {
                let (replay, offset) = archive.replay(limit)?;
                Ok(Attachment {
                    replay,
                    receiver: None,
                    offset,
                })
            }
        }
    }

    fn flush_and_archive(&self) {
        let runs: Vec<_> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(run, e)| matches!(e.terminal, Terminal::Live(_)).then_some(*run))
            .collect();
        for run in runs {
            // Serialize the final checkpoint with resize, Forget and shutdown.
            // Streams hold receivers, never a strong reference to the PTY object.
            let _guard = self.lifecycle.lock();
            let Ok(Terminal::Live(p)) = self.get(run) else {
                continue;
            };
            p.flush_log();
            if p.is_alive() || !p.is_drained() || p.has_processes() {
                continue;
            }
            let result = self
                .status(run)
                .and_then(|status| archive::Archive::save(self.run_dir(run), status, &p));
            match result {
                Ok(archive) => {
                    if let Some(e) = self.entries.lock().get_mut(&run) {
                        e.terminal = Terminal::Archived(Arc::new(archive));
                    }
                }
                Err(error) => tracing::warn!(
                    ?run,
                    "retaining terminal until its final snapshot is saved: {error:#}"
                ),
            }
        }
    }

    fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _guard = self.lifecycle.lock();
        let ptys = self.ptys();
        for p in &ptys {
            p.kill();
        }
        let end = Instant::now() + pty::KILL_GRACE;
        while ptys.iter().any(|p| p.has_processes()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        for p in &ptys {
            if p.has_processes() {
                p.kill_now();
            }
        }
        let end = Instant::now() + Duration::from_secs(1);
        while ptys.iter().any(|p| p.is_alive() || !p.is_drained()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        for p in &ptys {
            p.flush_log_final();
        }
    }
}

// Runs only when the host itself restarts, never when an application reconnects.
fn reap_previous_host(data: &Path) -> anyhow::Result<()> {
    let runs = data.join("runs");
    if !runs.exists() {
        return Ok(());
    }
    let mut owners = vec![];
    for entry in std::fs::read_dir(runs)? {
        let path = entry?.path().join("owner.json");
        if !path.exists() {
            continue;
        }
        let owner: OwnedProcess = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("reading terminal ownership {}", path.display()))?;
        if let Some(pid) = owner.pid {
            owners.push((pid, owner.run.id, owner.markers));
        }
    }
    let survivors = pty::reap_leftovers(owners);
    anyhow::ensure!(
        survivors.is_empty(),
        "processes from the previous host could not be reaped"
    );
    Ok(())
}

struct Lock(File);
impl Lock {
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        // SAFETY: flock acts only on the descriptor we just opened.
        anyhow::ensure!(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another terminal host owns {}",
            path.display()
        );
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the file is still open while this guard drops.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Hold an advisory lock on `path` for the rest of this process's life, retrying while
/// another process holds it. The application names the file; the host attaches no meaning
/// to it. (Muster uses it to keep older, monolithic daemon builds from starting beside
/// terminals this host owns.) The descriptor is close-on-exec, so children never inherit it.
pub fn hold_fence_lock(path: PathBuf) {
    let _ = std::thread::Builder::new().name("fence-lock".into()).spawn(move || {
        let mut warned = false;
        loop {
            match Lock::acquire(&path) {
                Ok(lock) => {
                    std::mem::forget(lock);
                    return;
                }
                Err(error) if !warned => {
                    tracing::warn!("fence lock {} not held yet: {error:#}", path.display());
                    warned = true;
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

pub async fn serve(runtime: PathBuf, data: PathBuf) -> anyhow::Result<()> {
    private_fs::ensure_private_dir(&runtime)?;
    private_fs::ensure_private_dir(&data)?;
    let runtime = std::fs::canonicalize(runtime)?;
    let data = std::fs::canonicalize(data)?;
    let _lock = Lock::acquire(&runtime.join("terminal.lock"))?;
    // A different runtime directory must not allow a second host to reap these processes.
    let _data_lock = Lock::acquire(&data.join("host.lock"))?;
    let reap_data = data.clone();
    tokio::task::spawn_blocking(move || reap_previous_host(&reap_data)).await??;
    let socket = runtime.join("terminal.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let hello = Hello {
        protocol: VERSION,
        instance: format!("{:032x}", rand::random::<u128>()),
        pid: std::process::id(),
        data_dir: data,
    };
    let discovery = runtime.join("terminal.json");
    private_fs::write_private(&discovery, &serde_json::to_vec(&hello)?)?;
    let (events, _) = broadcast::channel(4096);
    let host = Arc::new(Host {
        hello,
        bootstrap: Some(std::env::current_exe()?),
        entries: Mutex::new(BTreeMap::new()),
        forgotten: Mutex::new(BTreeMap::new()),
        lifecycle: Mutex::new(()),
        events,
        stopping: AtomicBool::new(false),
        stop: Notify::new(),
    });
    let flush_host = host.clone();
    let flusher = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            let host = flush_host.clone();
            let _ = tokio::task::spawn_blocking(move || {
                host.flush_and_archive();
            })
            .await;
        }
    });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    // Output and event streams hold their connection for their whole lifetime, so they
    // get their own admission limit. Short control requests (Input, Inventory, Terminate,
    // Forget, Hello) keep the remaining capacity even when every stream slot is taken.
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let streams = Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS));
    let result: anyhow::Result<()> = loop {
        tokio::select! {
            _ = host.stop.notified() => break Ok(()),
            _ = term.recv() => break Ok(()),
            _ = int.recv() => break Ok(()),
            _ = hup.recv() => {},
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                    let host = host.clone();
                    let streams = streams.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(error) = connection(stream, host, streams).await {
                        tracing::debug!("host connection closed: {error:#}");
                    }});
                }
                Err(error) => break Err(error.into()),
            }
        }
    };
    drop(listener);
    host.stopping.store(true, Ordering::SeqCst);
    flusher.abort();
    tokio::task::spawn_blocking(move || host.shutdown()).await?;
    let _ = std::fs::remove_file(discovery);
    let _ = std::fs::remove_file(socket);
    result
}

async fn send(stream: &mut UnixStream, frame: Frame) -> anyhow::Result<()> {
    tokio::time::timeout(IO_TIMEOUT, wire::write_async(stream, &frame)).await??;
    Ok(())
}

const MAX_CONNECTIONS: usize = 512;
const MAX_STREAMS: usize = 256;

async fn connection(mut stream: UnixStream, host: Arc<Host>, streams: Arc<tokio::sync::Semaphore>) -> anyhow::Result<()> {
    // SAFETY: geteuid is a side-effect-free query. Filesystem permissions also restrict access.
    anyhow::ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "foreign user"
    );
    let request: Request = tokio::time::timeout(IO_TIMEOUT, wire::read_async(&mut stream))
        .await??
        .decode()?;
    let error = if request.version != VERSION {
        Some(("protocol", "unsupported terminal protocol"))
    } else if !matches!(request.command, Command::Hello)
        && request.instance.as_deref() != Some(&host.hello.instance)
    {
        Some((
            "instance",
            "terminal host instance changed; reconcile before issuing commands",
        ))
    } else {
        None
    };
    if let Some((code, message)) = error {
        return send(
            &mut stream,
            Frame::json(&Response::Error {
                code: code.into(),
                message: message.into(),
            })?,
        )
        .await;
    }
    // Classify the bounded first request before a stream can hold a slot.
    let _stream_permit = match request.command {
        Command::Attach { .. } | Command::Events => match streams.try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return send(&mut stream, Frame::json(&Response::Error {
                code: "busy".into(),
                message: "too many terminal output subscriptions".into(),
            })?).await,
        },
        _ => None,
    };
    match request.command {
        Command::Shutdown => {
            send(&mut stream, Frame::json(&Response::Ok)?).await?;
            host.stopping.store(true, Ordering::SeqCst);
            host.stop.notify_one();
            Ok(())
        }
        Command::Attach { run, replay_bytes } => {
            let limit = replay_bytes.map(|n| n.clamp(4096, pty::RING_CAP));
            let reader = host.clone();
            let attachment =
                match tokio::task::spawn_blocking(move || reader.attach(run, limit)).await? {
                    Ok(attachment) => attachment,
                    Err(error) => return send_error(&mut stream, error).await,
                };
            let Attachment {
                replay,
                receiver: mut rx,
                mut offset,
            } = attachment;
            send(&mut stream, Frame::json(&Response::Attached)?).await?;
            send(
                &mut stream,
                Frame::Replay {
                    next_offset: offset,
                    data: replay,
                },
            )
            .await?;
            let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    output = async {
                        match &mut rx {
                            Some(receiver) => receiver.recv().await,
                            None => std::future::pending().await,
                        }
                    } => match output {
                        Ok(bytes) => {
                            let next = offset + bytes.len() as u64;
                            send(&mut stream, Frame::Output { offset, data: bytes.to_vec() }).await?;
                            offset = next;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            let reader = host.clone();
                            let attachment = tokio::task::spawn_blocking(move || reader.attach(run, limit)).await??;
                            rx = attachment.receiver;
                            offset = attachment.offset;
                            send(&mut stream, Frame::Replay { next_offset: offset, data: attachment.replay }).await?;
                        }
                        // All final bytes have arrived. The PTY owner can now be
                        // released without repeatedly disconnecting exited tabs.
                        Err(broadcast::error::RecvError::Closed) => rx = None,
                    },
                    _ = heartbeat.tick() => {
                        if !host.entries.lock().contains_key(&run) { return Ok(()); }
                        send(&mut stream, Frame::Ping).await?;
                    },
                    read = stream.readable() => {
                        read?;
                        let mut byte = [0];
                        match stream.try_read(&mut byte) {
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                            _ => return Ok(()), // EOF or unexpected client data ends the subscription.
                        }
                    }
                }
            }
        }
        Command::Events => {
            let mut rx = host.events.subscribe();
            send(&mut stream, Frame::json(&Response::Attached)?).await?;
            let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    event = rx.recv() => {
                        let response = match event {
                            Ok(event) => Response::Event(event),
                            Err(broadcast::error::RecvError::Lagged(_)) => Response::Resync,
                            Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        };
                        send(&mut stream, Frame::json(&response)?).await?;
                    }
                    _ = heartbeat.tick() => send(&mut stream, Frame::Ping).await?,
                    read = stream.readable() => {
                        read?;
                        let mut byte = [0];
                        match stream.try_read(&mut byte) {
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                            _ => return Ok(()),
                        }
                    }
                }
            }
        }
        command => {
            let result = tokio::task::spawn_blocking(move || host.execute(command)).await?;
            match result {
                Ok(response) => send(&mut stream, Frame::json(&response)?).await,
                Err(error) => send_error(&mut stream, error).await,
            }
        }
    }
}

async fn send_error(stream: &mut UnixStream, error: anyhow::Error) -> anyhow::Result<()> {
    send(
        stream,
        Frame::json(&Response::Error {
            code: "operation".into(),
            message: format!("{error:#}"),
        })?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_lock_is_held_for_the_host_lifetime() {
        let directory = std::env::temp_dir().join(format!("muster-fence-{}", rand::random::<u64>()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("daemon.lock");
        // Another holder first: the host waits for it, then takes the lock.
        let earlier = Lock::acquire(&path).unwrap();
        hold_fence_lock(path.clone());
        std::thread::sleep(Duration::from_millis(100));
        drop(earlier);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Lock::acquire(&path).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the host never took the fence lock");
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    struct Fixture(Host);
    impl Fixture {
        fn new() -> Self {
            let data = std::env::temp_dir().join(format!(
                "muster-archive-test-{:032x}",
                rand::random::<u128>()
            ));
            private_fs::ensure_private_dir(&data).unwrap();
            Self(Host {
                hello: Hello {
                    protocol: VERSION,
                    instance: format!("{:032x}", rand::random::<u128>()),
                    pid: std::process::id(),
                    data_dir: data,
                },
                bootstrap: None,
                entries: Mutex::new(BTreeMap::new()),
                forgotten: Mutex::new(BTreeMap::new()),
                lifecycle: Mutex::new(()),
                events: broadcast::channel(4096).0,
                stopping: AtomicBool::new(false),
                stop: Notify::new(),
            })
        }

        fn spec(&self, generation: u64) -> SpawnSpec {
            SpawnSpec {
                run: RunId { id: 1, generation },
                program: "/bin/bash".into(),
                args: vec![
                    "--noprofile".into(),
                    "--norc".into(),
                    "-c".into(),
                    r"printf '\033[?2004h\033[?1hfirst\nsecond\n'; exit 7".into(),
                ],
                env: BTreeMap::new(),
                cwd: self.0.hello.data_dir.clone(),
                cols: 80,
                rows: 24,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.0.shutdown();
            let _ = std::fs::remove_dir_all(&self.0.hello.data_dir);
        }
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "terminal did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn forgetting_an_unspawned_run_fences_a_delayed_launch() {
        let fixture = Fixture::new();
        let spec = fixture.spec(10);
        fixture
            .0
            .execute(Command::Forget { run: spec.run })
            .unwrap();
        assert!(fixture.0.spawn(spec).is_err());
        fixture.0.spawn(fixture.spec(11)).unwrap();
        let mut unrelated = fixture.spec(1);
        unrelated.run.id = 2;
        fixture.0.spawn(unrelated).unwrap();
        assert!(
            fixture
                .0
                .execute(Command::Forget {
                    run: RunId {
                        id: 0,
                        generation: 0
                    }
                })
                .is_err()
        );
    }

    #[test]
    fn invalid_targets_are_rejected_before_preparing_an_owned_launch() {
        let fixture = Fixture::new();
        let mut spec = fixture.spec(1);
        spec.program = fixture
            .0
            .hello
            .data_dir
            .join("missing-program")
            .to_string_lossy()
            .into_owned();
        assert!(fixture.0.spawn(spec.clone()).is_err());
        assert!(!fixture.0.run_dir(spec.run).exists());
        spec.program = "/bin/sh".into();
        spec.cwd = fixture.0.hello.data_dir.join("missing-cwd");
        assert!(fixture.0.spawn(spec).is_err());
        assert!(fixture.0.entries.lock().is_empty());
    }

    #[test]
    fn spawn_retry_survives_deleted_launch_files() {
        let fixture = Fixture::new();
        let mut spec = fixture.spec(1);
        spec.cwd = fixture.0.hello.data_dir.join("disposable-cwd");
        std::fs::create_dir(&spec.cwd).unwrap();
        let program = spec.cwd.join("shell");
        std::fs::copy("/bin/sh", &program).unwrap();
        spec.program = program.to_string_lossy().into_owned();
        spec.args = vec!["-c".into(), "exit 0".into()];
        let Response::Status(first) = fixture.0.spawn(spec.clone()).unwrap() else {
            panic!("spawn did not return status");
        };
        until(|| fixture.0.status(spec.run).unwrap().drained);
        std::fs::remove_dir_all(&spec.cwd).unwrap();
        let Response::Status(retry) = fixture.0.spawn(spec).unwrap() else {
            panic!("retry did not return status");
        };
        assert_eq!(first.pid, retry.pid);
        assert_eq!(first.run, retry.run);
    }

    #[test]
    fn archived_terminal_releases_pty_with_subscribers_and_retries_failed_checkpoint() {
        let fixture = Fixture::new();
        let host = &fixture.0;
        let spec = fixture.spec(1);
        let run = spec.run;
        host.spawn(spec.clone()).unwrap();
        let Terminal::Live(pty) = host.get(run).unwrap() else {
            panic!("expected live owner")
        };
        let weak = Arc::downgrade(&pty);
        let subscriber = pty.attach_sequenced(None).1;
        until(|| !pty.is_alive() && pty.is_drained() && !pty.has_processes());
        let replay = pty.attach_sequenced(None).0;
        let short_replay = pty.attach_sequenced(Some(8)).0;
        let screen = serde_json::to_value(pty.screen()).unwrap();
        let text = pty.recent_text(10000);
        let final_line = pty.recent_text(1);
        drop(pty);

        // Force a checkpoint write failure. The complete PTY state must remain
        // available until saving all of the final state succeeds.
        let blocked_file = host.run_dir(run).join("screen.json");
        std::fs::create_dir(&blocked_file).unwrap();
        host.flush_and_archive();
        assert!(matches!(host.get(run).unwrap(), Terminal::Live(_)));
        assert!(weak.upgrade().is_some());
        std::fs::remove_dir(&blocked_file).unwrap();
        host.flush_and_archive();
        until(|| weak.upgrade().is_none());
        assert!(matches!(host.get(run).unwrap(), Terminal::Archived(_)));
        // Keeping an output receiver cannot pin the old PTY/parser/ring.
        drop(subscriber);
        let attachment = host.attach(run, None).unwrap();
        assert!(attachment.receiver.is_none());
        assert_eq!(attachment.replay, replay);
        assert_eq!(host.attach(run, Some(8)).unwrap().replay, short_replay);
        let Terminal::Archived(archive) = host.get(run).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            serde_json::to_value(archive.screen().unwrap()).unwrap(),
            screen
        );
        assert_eq!(archive.recent_text(10000).unwrap(), text);
        assert_eq!(archive.recent_text(1).unwrap(), final_line);
        assert!(archive.recent_text(0).unwrap().is_empty());
        assert_eq!(host.status(run).unwrap().exit_code, Some(7));
        assert!(host.status(run).unwrap().log_persisted);
        assert!(
            host.execute(Command::Input {
                run,
                data: b"no".to_vec()
            })
            .is_err()
        );
        host.execute(Command::Resize {
            run,
            cols: 120,
            rows: 40,
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(archive.screen().unwrap()).unwrap(),
            screen,
            "final snapshot is immutable"
        );
        host.spawn(spec.clone()).unwrap(); // Retry returns the same exited run.
        let mut different = spec.clone();
        different.args.push("different".into());
        assert!(host.spawn(different).is_err());

        host.execute(Command::Forget { run }).unwrap();
        assert!(!host.run_dir(run).exists());
        assert!(host.spawn(spec).is_err());
        let next = fixture.spec(2);
        host.spawn(next.clone()).unwrap();
        until(|| {
            let s = host.status(next.run).unwrap();
            s.drained && !s.has_processes
        });
        host.execute(Command::Forget { run: next.run }).unwrap();
        host.execute(Command::Forget { run }).unwrap();
        assert_eq!(*host.forgotten.lock(), BTreeMap::from([(1, 2)]));
    }
}
