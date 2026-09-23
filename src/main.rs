mod agents;
mod assets;
mod client;
mod detect;
mod gui;
mod hookcmd;
mod hub;
mod inject;
mod mcp;
mod model;
mod notify;
mod paths;
mod pty;
mod rpc;
mod server;
mod store;
mod worktree;


use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};

use crate::paths::Discovery;

#[derive(Parser)]
#[command(
    name = "muster",
    version,
    about = "Run many coding agents side by side, Slack-style."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (detaches into the background unless --foreground).
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    /// Forward an agent hook callback to the daemon (used inside agent CLIs).
    Hook {
        agent: String,
        event: String,
        payload: Option<String>,
    },
    /// Show whether the daemon is running.
    Status,
    /// Stop the daemon (running sessions are terminated).
    Stop,
    /// Print the UI URL (with its access token).
    Url,
    /// Add a folder/repo as a workspace (default: current directory) and open the UI.
    Open { path: Option<PathBuf> },
}

fn main() -> ExitCode {
    // Hook callbacks run inside agent CLIs on every event: dispatch before anything else
    // (no clap, no runtime) so they stay fast.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook") {
        let code = hookcmd::run(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
            args.get(4).cloned(),
        );
        return ExitCode::from(code.clamp(0, 255) as u8);
    }

    let cli = Cli::parse();
    let r = match cli.cmd {
        None => ensure_daemon().and_then(|d| open_ui(&d)),
        Some(Cmd::Daemon { foreground: true }) => run_daemon(),
        Some(Cmd::Daemon { foreground: false }) => start_daemon_detached().map(|d| println!("{}", d.ui_url())),
        Some(Cmd::Hook { .. }) => unreachable!("handled above"),
        Some(Cmd::Status) => status(),
        Some(Cmd::Stop) => stop(),
        Some(Cmd::Url) => running().map(|d| println!("{}", d.ui_url())),
        Some(Cmd::Open { path }) => open(path),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("muster: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ----- CLI side -----

/// The running daemon, if its discovery file is fresh and it answers.
fn find_daemon() -> Option<Discovery> {
    let d = paths::read_discovery()?;
    (paths::pid_alive(d.pid) && is_muster_process(d.pid) && client::healthy(d.port)).then_some(d)
}

fn running() -> anyhow::Result<Discovery> {
    find_daemon().context("daemon is not running (start it with `muster daemon`)")
}

fn ensure_daemon() -> anyhow::Result<Discovery> {
    match find_daemon() {
        Some(d) => Ok(d),
        None => start_daemon_detached(),
    }
}

/// Re-exec `muster daemon --foreground` in its own session, logging to the runtime dir,
/// and wait until it answers.
fn start_daemon_detached() -> anyhow::Result<Discovery> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    if let Some(d) = find_daemon() {
        return Ok(d);
    }
    let rt = paths::runtime_dir();
    paths::ensure_private_dir(&rt)?;
    let log_path = paths::log_path();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    paths::make_private_file(&log_path);
    let exe = std::env::current_exe().context("locating the muster binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the daemon from our terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting daemon")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    loop {
        if let Some(d) = find_daemon() {
            // No need to reap: the daemon outlives us and is reparented to init.
            return Ok(d);
        }
        if exited.is_none()
            && let Ok(Some(st)) = child.try_wait()
        {
            exited = Some((st, Instant::now()));
        }
        // Our child may have lost a startup race to another daemon: give that one a moment.
        if let Some((st, at)) = exited
            && at.elapsed() > Duration::from_secs(3)
        {
            anyhow::bail!("daemon exited during startup ({st}); see {}", log_path.display());
        }
        if Instant::now() > deadline {
            anyhow::bail!("daemon did not come up within 15s; see {}", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn status() -> anyhow::Result<()> {
    let Some(d) = find_daemon() else {
        println!("muster daemon: not running");
        std::process::exit(1);
    };
    println!("muster daemon: running (pid {}, version {})", d.pid, d.version);
    println!("url: {}", d.base_url());
    if let Ok(h) = client::rpc(&d, "hello", serde_json::json!({})) {
        let sessions = h["sessions"].as_array().cloned().unwrap_or_default();
        let live = sessions.iter().filter(|s| s["state"] != "exited").count();
        println!(
            "workspaces: {}  channels: {}  sessions: {} running / {} total",
            h["workspaces"].as_array().map_or(0, |a| a.len()),
            h["channels"].as_array().map_or(0, |a| a.len()),
            live,
            sessions.len()
        );
    }
    println!("data: {}", paths::data_dir().display());
    Ok(())
}

fn stop() -> anyhow::Result<()> {
    let Some(d) = paths::read_discovery().filter(|d| paths::pid_alive(d.pid)) else {
        println!("muster daemon: not running");
        return Ok(());
    };
    // Guard against a stale daemon.json whose pid now belongs to an unrelated process.
    if !is_muster_process(d.pid) {
        let _ = std::fs::remove_file(paths::discovery_path());
        println!("muster daemon: not running (removed stale daemon.json)");
        return Ok(());
    }
    // SAFETY: plain kill(2).
    unsafe {
        libc::kill(d.pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while paths::pid_alive(d.pid) {
        if Instant::now() > deadline {
            anyhow::bail!("daemon (pid {}) did not stop within 10s", d.pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("muster daemon stopped");
    Ok(())
}

/// Best effort: does `pid` look like a `muster daemon` process?
fn is_muster_process(pid: u32) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => {
            let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            let exe_is_muster = args
                .first()
                .is_some_and(|a| String::from_utf8_lossy(a).rsplit('/').next() == Some("muster"));
            exe_is_muster && args.iter().any(|a| *a == b"daemon")
        }
        // No procfs (macOS): fall back to trusting the pid.
        Err(_) => !std::path::Path::new("/proc/self").exists(),
    }
}

fn open(path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let abs = std::fs::canonicalize(&path).with_context(|| format!("{}", path.display()))?;
    let d = ensure_daemon()?;
    let ws = client::rpc(&d, "workspace.add", serde_json::json!({ "path": abs }))?;
    println!("workspace: {} ({})", ws["name"].as_str().unwrap_or("?"), abs.display());
    open_ui(&d)
}

/// Open the UI with a single-use launch code, so the client token never appears on the
/// browser's command line (/proc/<pid>/cmdline is world-readable).
fn open_ui(d: &Discovery) -> anyhow::Result<()> {
    match client::launch_code(d).context("getting a launch code from the daemon")? {
        Some(code) => gui::open(&d.launch_url(&code)),
        None => {
            eprintln!(
                "muster: the running daemon (version {}) has no launch codes; opening with the token URL. \
                 Restart it (`muster stop`) to stop passing the token on the browser command line.",
                d.version
            );
            gui::open(&d.ui_url())
        }
    }
}

// ----- daemon side -----

/// Held for the daemon's lifetime; a second daemon for the same runtime dir fails to lock.
struct DaemonLock(#[allow(dead_code)] std::fs::File);

fn acquire_lock() -> anyhow::Result<DaemonLock> {
    use std::os::fd::AsRawFd;
    let rt = paths::runtime_dir();
    paths::ensure_private_dir(&rt)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::lock_path())?;
    // SAFETY: flock on an fd we own.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        anyhow::bail!("another muster daemon is already running for {}", rt.display());
    }
    Ok(DaemonLock(f))
}

fn load_client_token() -> anyhow::Result<String> {
    let p = paths::client_token_path();
    if let Ok(t) = std::fs::read_to_string(&p) {
        let t = t.trim().to_string();
        if t.len() >= 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(t);
        }
    }
    let t = paths::random_token();
    paths::write_private(&p, t.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    Ok(t)
}

async fn bind() -> anyhow::Result<tokio::net::TcpListener> {
    use tokio::net::TcpListener;
    if let Some(port) = paths::port_override()? {
        return TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("binding 127.0.0.1:{port} (MUSTER_PORT)"));
    }
    match TcpListener::bind(("127.0.0.1", paths::DEFAULT_PORT)).await {
        Ok(l) => Ok(l),
        Err(e) => {
            tracing::info!("port {} unavailable ({e}); picking a free port", paths::DEFAULT_PORT);
            Ok(TcpListener::bind(("127.0.0.1", 0)).await?)
        }
    }
}

fn run_daemon() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUSTER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    if let Some(d) = find_daemon() {
        println!("muster daemon already running (pid {}): {}", d.pid, d.base_url());
        return Ok(());
    }
    let _lock = acquire_lock()?;
    paths::ensure_private_dir(&paths::data_dir())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(daemon_main());
    rt.shutdown_timeout(Duration::from_secs(2));
    paths::remove_discovery_if_ours();
    r
}

async fn daemon_main() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let listener = bind().await?;
    let port = listener.local_addr()?.port();
    let client_token = load_client_token()?;
    let store = store::Store::open(&paths::db_path())?;
    let muster_bin = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| PathBuf::from("muster"));
    let hub = hub::Hub::new(
        hub::HubConfig {
            port,
            client_token: client_token.clone(),
            muster_bin,
        },
        store,
    )?;

    inject::spawn_tasks(hub.clone());
    detect::spawn(hub.clone());
    notify::spawn(hub.clone());

    // Event log retention (also applied at startup).
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(3600));
            t.tick().await;
            loop {
                t.tick().await;
                let h = hub.clone();
                let _ = tokio::task::spawn_blocking(move || h.prune_events()).await;
            }
        });
    }

    // Periodically persist scrollback so a crash loses at most a few seconds.
    // Appends only what's new since the last flush, without fsync.
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(3));
            loop {
                t.tick().await;
                let h = hub.clone();
                let _ = tokio::task::spawn_blocking(move || h.flush_logs()).await;
            }
        });
    }

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;

    let app = server::router(hub.clone());
    let serve = axum::serve(listener, app);

    paths::write_discovery(&Discovery {
        pid: std::process::id(),
        port,
        token: client_token,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .context("writing daemon.json")?;
    tracing::info!(
        "muster daemon listening on http://127.0.0.1:{port} (data: {})",
        paths::data_dir().display()
    );

    let r: anyhow::Result<()> = async {
        tokio::select! {
            r = serve => r.context("server failed"),
            _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); Ok(()) }
            _ = int.recv() => { tracing::info!("SIGINT: shutting down"); Ok(()) }
            _ = async { loop { hup.recv().await; tracing::info!("ignoring SIGHUP"); } } => Ok(()),
        }
    }
    .await;

    shutdown(&hub).await;
    // Removed only after teardown, so a new daemon doesn't start while sessions are still
    // being killed (it would fail to take the lock anyway).
    paths::remove_discovery_if_ours();
    r
}

async fn shutdown(hub: &Arc<hub::Hub>) {
    hub.shutdown().await;
    tracing::info!("muster daemon stopped");
}
