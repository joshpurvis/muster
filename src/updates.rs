//! Waiting updates and the window's lifecycle controls (docs/PROTOCOL.md "Updates").
//!
//! An update is waiting when the active installed UI release changed (`ui-install`), when the
//! executable this backend was launched from now holds a different build, or when the terminal
//! host beside that executable differs from the running host. The watcher polls with cheap
//! `stat` calls and hashes a file only once its size and mtime have held still for one tick, so
//! a build still being written is never hashed. Changes go to clients as `updates_changed`.
//!
//! `daemon.reload`, `daemon.restart` and `daemon.shutdown` run the CLI's own `reload` /
//! `restart-all` / `stop`, detached, so the safety checks stay in one place. One of them at a
//! time: while one this backend started may still happen, the others are refused (`BUSY`).

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::hub::{Hub, ServerEvent};

/// How often the watcher looks at the UI pointer and the launch executables.
const POLL: Duration = Duration::from_secs(2);

/// `hello.updates` and the `updates_changed` event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The installed UI release new pages load; null while serving embedded assets or
    /// `MUSTER_WEB_DIR`.
    pub ui_release: Option<String>,
    /// A different backend build waits at the launch path: its content id (short SHA-256).
    pub backend: Option<String>,
    /// A different terminal host waits beside the launch path: its content id.
    pub host: Option<String>,
}

/// Where this backend came from, for the watcher and `daemon.reload`.
pub struct Launch {
    /// The executable the backend was started from (not its snapshot under the data dir).
    pub exe: PathBuf,
    /// SHA-256 of the running backend (its release snapshot's).
    pub app_hash: String,
    /// The running terminal host's pid, if there is one.
    pub host_pid: Option<u32>,
}

#[derive(Default)]
pub struct Updates {
    status: Mutex<Status>,
    /// The launch path and the running build's SHA-256, for `daemon.reload`.
    launch: Mutex<Option<(PathBuf, String)>>,
    /// The running terminal host's pid, for `daemon.restart`'s look at what it would start.
    host_pid: Mutex<Option<u32>>,
    /// A reload, restart or shutdown this backend started may still happen (see [`Claim`]).
    busy: AtomicBool,
}

impl Updates {
    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    /// Store `next`; true when it changed.
    fn set(&self, next: Status) -> bool {
        let mut cur = self.status.lock();
        if *cur == next {
            return false;
        }
        *cur = next;
        true
    }
}

/// What a reload, restart or shutdown is refused with while another one this backend started
/// may still happen (a second window's click, or `muster reload` meanwhile).
pub const BUSY: &str = "Muster is already restarting or shutting down";

/// `Updates::busy`, claimed for one lifecycle command. Dropped before the command has started,
/// it frees the next one; once started ([`Claim::keep`]) only the command's failure frees it
/// ([`watch`]): on success this backend stops.
struct Claim<'a>(Option<&'a Updates>);

impl<'a> Claim<'a> {
    fn new(updates: &'a Updates) -> anyhow::Result<Self> {
        updates.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow::anyhow!("{BUSY}"))?;
        Ok(Self(Some(updates)))
    }

    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if let Some(updates) = self.0 {
            updates.busy.store(false, Ordering::SeqCst);
        }
    }
}

/// A file watched by size, mtime and inode; hashed once those settle.
/// Known gap, accepted: an in-place rewrite of equal length that restores the mtime, or a new
/// file that reuses the inode with the same size and mtime, looks unchanged.
#[derive(Default)]
struct Watched {
    seen: Option<(u64, SystemTime, u64)>,
    /// `seen` changed on the last poll: hash on the next one if it holds still.
    settling: bool,
    hash: Option<String>,
}

impl Watched {
    /// The file's hash as of the last settled look (None: missing or unreadable).
    fn poll(&mut self, path: &Path) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        let now = std::fs::metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.ino()));
        if now.is_none() {
            *self = Self::default();
        } else if now != self.seen {
            self.seen = now;
            self.settling = true;
        } else if self.settling {
            // A failed read (say, a build published before its permissions) is retried on the
            // next poll, never kept as the file's final state.
            if let Ok(hash) = crate::executables::hash_file(path) {
                self.settling = false;
                self.hash = Some(hash);
            }
        }
        self.hash.clone()
    }
}

/// What the launch path holds, against what runs.
fn compare(app: &str, running_host: Option<&str>, launch: Option<&str>, sibling_host: Option<&str>) -> (Option<String>, Option<String>) {
    let short = |h: &str| h.chars().take(12).collect::<String>();
    let backend = launch.filter(|h| *h != app).map(short);
    // Unknown running host (no /proc): never claim it differs.
    let host = match (running_host, sibling_host) {
        (Some(running), Some(new)) if running != new => Some(short(new)),
        _ => None,
    };
    (backend, host)
}

/// `current_exe` names a replaced file "<path> (deleted)"; the launch path is the file name.
fn launch_path(exe: PathBuf) -> PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => exe,
    }
}

/// Start watching for updates. Call before the server accepts clients, so the first `hello`
/// already carries the active UI release.
pub fn spawn(hub: &Arc<Hub>, launch: Launch) {
    let data = crate::paths::data_dir();
    let exe = launch_path(launch.exe);
    *hub.updates().launch.lock() = Some((exe.clone(), launch.app_hash.clone()));
    *hub.updates().host_pid.lock() = launch.host_pid;
    hub.updates().set(Status { ui_release: crate::assets::active_release(&data), ..Status::default() });
    let weak = Arc::downgrade(hub);
    tokio::spawn(async move {
        let host_pid = launch.host_pid;
        // The running host's executable, by content: /proc keeps it even after a rebuild.
        let running_host = tokio::task::spawn_blocking(move || {
            host_pid.and_then(|pid| crate::executables::hash_file(Path::new(&format!("/proc/{pid}/exe"))).ok())
        })
        .await
        .ok()
        .flatten();
        let sibling = exe.with_file_name(crate::executables::HOST);
        let mut app = Watched::default();
        let mut host = Watched::default();
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(hub) = weak.upgrade() else { return };
            let (exe, sibling, data) = (exe.clone(), sibling.clone(), data.clone());
            let running = running_host.clone();
            let app_hash = launch.app_hash.clone();
            let polled = tokio::task::spawn_blocking(move || {
                let launch = app.poll(&exe);
                let new_host = host.poll(&sibling);
                let (backend, host_update) = compare(&app_hash, running.as_deref(), launch.as_deref(), new_host.as_deref());
                let status = Status { ui_release: crate::assets::active_release(&data), backend, host: host_update };
                (app, host, status)
            })
            .await;
            let Ok((a, h, status)) = polled else { return };
            (app, host) = (a, h);
            if hub.updates().set(status.clone()) {
                hub.emit(ServerEvent::UpdatesChanged(status));
            }
        }
    });
}

/// A detached `muster <args>` from `exe` in its own session, its output in the daemon log.
fn run_cli(exe: &Path, args: &[&str], openrouter_key: Option<&crate::openrouter::ApiKey>) -> anyhow::Result<Child> {
    cli_command(exe, args, openrouter_key)?.spawn().with_context(|| format!("starting {}", exe.display()))
}

/// The command [`run_cli`] spawns.
fn cli_command(exe: &Path, args: &[&str], openrouter_key: Option<&crate::openrouter::ApiKey>) -> anyhow::Result<Command> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_path())
        .context("opening the daemon log")?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // The key left this backend's environment at start (src/openrouter.rs); `reload` starts the
    // replacement with this command's environment, so it is handed on explicitly.
    if let Some(key) = openrouter_key {
        cmd.env(crate::openrouter::ENV_KEY, key.expose());
    }
    // SAFETY: setsid is async-signal-safe; the command outlives this backend.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(cmd)
}

/// Wait for a detached lifecycle command. Only a failure reports (on success this backend is
/// stopped before the command ends): an error notice, and the next lifecycle command is free.
fn watch(hub: &Arc<Hub>, mut child: Child, failed: &'static str) {
    let weak = Arc::downgrade(hub);
    std::thread::spawn(move || {
        if let Ok(status) = child.wait()
            && !status.success()
            && let Some(hub) = weak.upgrade()
        {
            hub.updates().busy.store(false, Ordering::SeqCst);
            hub.notice("error", format!("{failed} ({status}). Details: {}", crate::paths::log_path().display()));
        }
    });
}

/// What a refused `daemon.reload_handoff` error starts with: the CLI then reloads the usual way.
pub const HANDOFF_REFUSED: &str = "handoff refused:";

/// `daemon.reload_handoff {exe}`: `muster reload` from a shell without `OPENROUTER_API_KEY`
/// while this backend has a key (the key is only in this backend's memory), or from a sandboxed
/// command that can't see or signal this backend's process. Instead of the command starting the
/// replacement, this backend runs `<launch path> reload` itself, as the update banner does: the
/// replacement gets this backend's environment and its key, if any. Only when the command is the build this backend was
/// started from (`exe`, compared as canonical paths), so the same build is restarted as a
/// reload typed there would have.
pub fn reload_handoff(hub: &Arc<Hub>, exe: &Path) -> anyhow::Result<PathBuf> {
    let (launch, _) = hub.updates().launch.lock().clone()
        .with_context(|| format!("{HANDOFF_REFUSED} this backend does not know where it was launched from"))?;
    let same = match (std::fs::canonicalize(exe), std::fs::canonicalize(&launch)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    anyhow::ensure!(same, "{HANDOFF_REFUSED} this backend was started from {}, not {}", launch.display(), exe.display());
    let claim = Claim::new(hub.updates())?;
    let child = run_cli(&launch, &["reload"], hub.openrouter_key())?;
    claim.keep();
    watch(hub, child, "The backend was not restarted");
    Ok(launch)
}

/// Is the build now at the launch path (`current`, its SHA-256) the one the human approved
/// (`expect`, the `updates.backend` id the banner showed), and not the one already running?
fn check_reload(current: &str, running: &str, expect: &str, exe: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(current != running, "the backend already runs the build at {}", exe.display());
    anyhow::ensure!(
        expect.len() >= 12 && current.starts_with(expect),
        "the build at {} changed since the update was offered (it may still be being written); \
         wait for the update banner to show it again",
        exe.display()
    );
    Ok(())
}

/// `daemon.reload {expect}`: run `<launch path> reload`, which checks that the terminal host
/// can be kept, stops this backend and starts the build at the launch path. Only the build the
/// banner offered (`expect`) is started: a launch file rewritten since, or one this backend
/// already runs (a second window's late click), is refused. A reload the CLI refuses leaves
/// this backend running and says so in a notice. This binds the click to the offered build; it
/// is no defence against a process of this user swapping the file in between.
pub async fn reload(hub: &Arc<Hub>, expect: &str) -> anyhow::Result<PathBuf> {
    let claim = Claim::new(hub.updates())?;
    let (exe, running) = hub.updates().launch.lock().clone()
        .context("this backend does not know where it was launched from")?;
    anyhow::ensure!(exe.is_file(), "no muster build at {}", exe.display());
    let path = exe.clone();
    let current = tokio::task::spawn_blocking(move || crate::executables::hash_file(&path)).await??;
    check_reload(&current, &running, expect, &exe)?;
    let child = run_cli(&exe, &["reload"], hub.openrouter_key())?;
    claim.keep();
    watch(hub, child, "The backend was not restarted");
    Ok(exe)
}

/// `daemon.restart {backend, host}`: the `updates.backend` and `updates.host` ids the window
/// showed (null: none of that kind waiting), so what starts is what the human was shown.
#[derive(Debug, Default, Deserialize)]
pub struct Expect {
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

/// Are the builds a restart would start (`now`: what [`compare`] says of the launch path and
/// the host beside it, read again) the ones the window showed?
fn check_restart(now: &(Option<String>, Option<String>), expect: &Expect, exe: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        now.0 == expect.backend,
        "the Muster build at {} changed since this window last looked (it may still be being written); \
         nothing was stopped. Wait for the window to show it, then try again",
        exe.display()
    );
    anyhow::ensure!(
        now.1 == expect.host,
        "the terminal host beside {} changed since this window last looked (it may still be being written); \
         nothing was stopped. Wait for the window to show it, then try again",
        exe.display()
    );
    Ok(())
}

/// The descriptor the restart command reports on (`restart-all --report-fd 3`).
const REPORT_FD: i32 = 3;
/// How long the restart command may take over its checks (snapshotting the release) before it
/// reports: short enough that the call still answers within a window's 30 s request timeout.
const REPORT_WAIT: Duration = Duration::from_secs(20);

/// `daemon.restart`: what `muster stop` then `muster` do (every session ends; then a new backend
/// and terminal host start from the launch path, as a normal start picks them), as one command:
/// `<launch path> restart-all --launch <it> --app <its SHA-256> [--host <the host's>]`, detached, with this backend's environment and OpenRouter key, as
/// `daemon.reload` runs `reload`. Refused before anything stops: while another lifecycle command
/// may still happen, without a known launch path, when the builds there are not the ones the
/// window showed (`expect`), or when the command's own checks refuse (it reports on a socket
/// before it stops anything, and stops nothing until this backend answers). Windows hear
/// `daemon_stopping {reason: "restart"}` before the command goes ahead.
pub async fn restart(hub: &Arc<Hub>, expect: Expect) -> anyhow::Result<PathBuf> {
    let claim = Claim::new(hub.updates())?;
    let (exe, running) = hub.updates().launch.lock().clone()
        .context("this backend does not know where it was launched from, so it can't start Muster again; nothing was stopped")?;
    anyhow::ensure!(exe.is_file(), "no muster build at {}; nothing was stopped", exe.display());
    let host_pid = *hub.updates().host_pid.lock();
    let path = exe.clone();
    let (launch, sibling, now) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let launch = crate::executables::hash_file(&path)?;
        let sibling = crate::executables::hash_file(&path.with_file_name(crate::executables::HOST)).ok();
        let running_host = host_pid
            .and_then(|pid| crate::executables::hash_file(Path::new(&format!("/proc/{pid}/exe"))).ok());
        let now = compare(&running, running_host.as_deref(), Some(&launch), sibling.as_deref());
        Ok((launch, sibling, now))
    })
    .await??;
    check_restart(&now, &expect, &exe)?;
    // The command refuses unless the release it prepares (and starts) is exactly these files.
    let fd = REPORT_FD.to_string();
    let exe_arg = exe.to_string_lossy();
    let mut args = vec!["restart-all", "--report-fd", &fd, "--launch", &exe_arg, "--app", &launch];
    if let Some(host) = &sibling {
        args.extend(["--host", host.as_str()]);
    }
    let cmd = cli_command(&exe, &args, hub.openrouter_key())?;
    let (child, report) = spawn_reporting(cmd).with_context(|| format!("starting {}", exe.display()))?;
    let announce = Arc::downgrade(hub);
    let shown = exe.clone();
    let child = tokio::task::spawn_blocking(move || {
        handshake(child, report, REPORT_WAIT, &shown, || {
            if let Some(hub) = announce.upgrade() {
                hub.emit(ServerEvent::DaemonStopping { reason: "restart".into() });
            }
        })
    })
    .await??;
    claim.keep();
    watch(hub, child, "Muster was not restarted");
    Ok(exe)
}

/// Spawn `cmd` with one end of a socket pair as its descriptor [`REPORT_FD`]; the other end is
/// returned.
fn spawn_reporting(mut cmd: Command) -> anyhow::Result<(Child, UnixStream)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let (ours, theirs) = UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    // SAFETY: dup2 and fcntl are async-signal-safe. Both ends are close-on-exec; dup2 clears
    // that on the copy, and an end that already is the descriptor gets it cleared by hand.
    unsafe {
        cmd.pre_exec(move || {
            if fd == REPORT_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, REPORT_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(theirs);
    Ok((child, ours))
}

/// The restart command's verdict on `report`: "ready" (its checks passed; it stops nothing until
/// it reads "go"), "refused: <why>", or nothing within `wait` (it exited: an older build without
/// `restart-all`, say). On "ready", `announce` runs before the "go", so windows hear of the
/// restart before it begins; the command is returned to be watched. Otherwise it is ended, if
/// it hasn't ended itself, and the error says why; nothing was stopped.
fn handshake(mut child: Child, report: UnixStream, wait: Duration, exe: &Path, announce: impl FnOnce())
    -> anyhow::Result<Child> {
    use std::io::{BufRead, BufReader, Write};
    report.set_read_timeout(Some(wait))?;
    let mut line = String::new();
    let read = BufReader::new(&report).read_line(&mut line);
    if read.is_ok() && line.trim_end() == "ready" {
        announce();
        // A command that can't hear this stops nothing; the windows' wait for the new backend
        // runs out and says so.
        (&report).write_all(b"go\n").context("telling the restart command to go ahead")?;
        return Ok(child);
    }
    drop(report);
    // Let it finish writing its reason to the log; one that hangs is ended.
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                break child.wait().ok();
            }
        }
    };
    if let Some(why) = line.trim_end().strip_prefix("refused: ") {
        anyhow::bail!("{why}");
    }
    let log = crate::paths::log_path();
    if let Err(error) = read
        && matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
    {
        anyhow::bail!("the restart command did not answer within {} s; nothing was stopped. Details: {}", wait.as_secs(), log.display());
    }
    let status = status.map_or_else(|| "it was ended".to_string(), |s| s.to_string());
    anyhow::bail!(
        "the muster build at {} did not start the restart ({status}); nothing was stopped. It may be older than \
         the running one: run `muster stop`, then `muster`. Details: {}",
        exe.display(),
        log.display()
    )
}

/// `daemon.shutdown`: what `muster stop` does (stop this backend, then the terminal host,
/// ending every session), run from this backend's own release. Windows hear
/// `daemon_stopping` once the stop command has started (it signals this backend only after it
/// has read the discovery file); a command that can't start leaves them connected.
pub fn shutdown(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let claim = Claim::new(hub.updates())?;
    let exe = hub.muster_bin().to_path_buf();
    anyhow::ensure!(exe.exists(), "the muster command {} is missing", exe.display());
    let child = run_cli(&exe, &["stop"], None)?;
    hub.emit(ServerEvent::DaemonStopping { reason: "shutdown".into() });
    claim.keep();
    watch(hub, child, "Muster did not shut down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_reports_only_differing_builds() {
        assert_eq!(compare("aaa", Some("hhh"), Some("aaa"), Some("hhh")), (None, None));
        assert_eq!(compare("aaa", Some("hhh"), None, None), (None, None), "build removed");
        let new = "b".repeat(64);
        assert_eq!(compare("aaa", Some("hhh"), Some(&new), Some("hhh")).0, Some("b".repeat(12)));
        assert_eq!(compare("aaa", Some("hhh"), Some("aaa"), Some("ggg")).1, Some("ggg".into()));
        assert_eq!(compare("aaa", None, Some("aaa"), Some("ggg")).1, None, "unknown running host");
    }

    #[test]
    fn reload_starts_only_the_offered_build() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let exe = Path::new("/p/muster");
        check_reload(&b, &a, &b[..12], exe).unwrap();
        check_reload(&b, &a, &b, exe).unwrap();
        let e = check_reload(&a, &a, &a[..12], exe).unwrap_err().to_string();
        assert!(e.contains("already runs"), "{e}");
        let c = "c".repeat(64);
        let e = check_reload(&c, &a, &b[..12], exe).unwrap_err().to_string();
        assert!(e.contains("changed since the update was offered"), "{e}");
        assert!(check_reload(&b, &a, "", exe).is_err(), "no expected build");
        assert!(check_reload(&b, &a, "bbb", exe).is_err(), "too short to identify a build");
    }

    #[test]
    fn restart_starts_only_the_builds_the_window_showed() {
        let exe = Path::new("/p/muster");
        let none = Expect::default();
        check_restart(&(None, None), &none, exe).unwrap();
        let both = Expect { backend: Some("b".repeat(12)), host: Some("h".repeat(12)) };
        check_restart(&(Some("b".repeat(12)), Some("h".repeat(12))), &both, exe).unwrap();
        // A build that appeared, changed or went away since the window looked: refused.
        let e = check_restart(&(Some("b".repeat(12)), None), &none, exe).unwrap_err().to_string();
        assert!(e.contains("Muster build at /p/muster changed") && e.contains("nothing was stopped"), "{e}");
        let e = check_restart(&(None, Some("g".repeat(12))), &none, exe).unwrap_err().to_string();
        assert!(e.contains("terminal host beside /p/muster changed"), "{e}");
        let e = check_restart(&(Some("b".repeat(12)), Some("g".repeat(12))), &both, exe).unwrap_err().to_string();
        assert!(e.contains("terminal host"), "{e}");
        assert!(check_restart(&(None, None), &both, exe).is_err());
    }

    #[test]
    fn one_lifecycle_command_at_a_time() {
        let u = Updates::default();
        let first = Claim::new(&u).unwrap();
        assert_eq!(Claim::new(&u).err().unwrap().to_string(), BUSY);
        // Refused before it started: the next one may go.
        drop(first);
        let started = Claim::new(&u).unwrap();
        started.keep();
        assert!(Claim::new(&u).is_err(), "a started command keeps it until it fails");
        u.busy.store(false, Ordering::SeqCst);
        assert!(Claim::new(&u).is_ok());
    }

    /// `sh -c <script>` with the report socket on fd 3, as `daemon.restart` runs `restart-all`.
    fn reporting(script: &str) -> (Child, UnixStream) {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        spawn_reporting(cmd).unwrap()
    }

    #[test]
    fn restart_goes_ahead_only_after_the_command_is_ready_and_told_to() {
        let dir = std::env::temp_dir().join(format!("muster-restart-{}", crate::paths::random_token()));
        std::fs::create_dir(&dir).unwrap();
        let heard = dir.join("heard");
        let exe = Path::new("/p/muster");
        // Ready: windows are told first, then the command hears "go".
        let (child, report) = reporting(&format!("echo ready >&3; read x <&3; echo \"$x\" > '{}'", heard.display()));
        let mut announced = false;
        let mut child = handshake(child, report, Duration::from_secs(10), exe, || announced = true).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(announced);
        assert_eq!(std::fs::read_to_string(&heard).unwrap().trim(), "go");
        // Its own checks refused: their reason, nothing announced.
        let (child, report) = reporting("echo 'refused: the terminal host belongs to another data directory' >&3; exit 1");
        let e = handshake(child, report, Duration::from_secs(10), exe, || panic!("announced")).unwrap_err().to_string();
        assert_eq!(e, "the terminal host belongs to another data directory");
        // Exited without a word (an older build without `restart-all`: a usage error).
        let (child, report) = reporting("exit 2");
        let e = handshake(child, report, Duration::from_secs(10), exe, || panic!("announced")).unwrap_err().to_string();
        assert!(e.contains("/p/muster did not start the restart") && e.contains("nothing was stopped"), "{e}");
        assert!(e.contains("muster stop"), "{e}");
        // Says nothing in time: ended, never told to go.
        let (child, report) = reporting(&format!("sleep 30; echo late > '{}'", heard.display()));
        let started = Instant::now();
        let e = handshake(child, report, Duration::from_millis(300), exe, || panic!("announced")).unwrap_err().to_string();
        assert!(e.contains("did not answer within"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10), "the silent command was ended");
        assert_eq!(std::fs::read_to_string(&heard).unwrap().trim(), "go", "it never ran on");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launch_path_drops_the_deleted_marker() {
        assert_eq!(launch_path("/p/muster (deleted)".into()), PathBuf::from("/p/muster"));
        assert_eq!(launch_path("/p/muster".into()), PathBuf::from("/p/muster"));
    }

    #[test]
    fn watched_files_are_hashed_only_after_they_settle() {
        let dir = std::env::temp_dir().join(format!("muster-updates-{}", crate::paths::random_token()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("muster");
        let mut w = Watched::default();
        assert_eq!(w.poll(&file), None, "missing");
        std::fs::write(&file, b"one").unwrap();
        assert_eq!(w.poll(&file), None, "just appeared: still settling");
        let one = w.poll(&file).expect("settled");
        assert_eq!(one, crate::executables::hash_file(&file).unwrap());
        assert_eq!(w.poll(&file), Some(one.clone()), "unchanged: no rehash needed");
        // A rewrite (new size) reports the old hash until it holds still.
        std::fs::write(&file, b"second build").unwrap();
        assert_eq!(w.poll(&file), Some(one.clone()));
        let two = w.poll(&file).unwrap();
        assert_ne!(two, one);
        // A read that fails is retried, not remembered (permissions fixed without other change).
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&file, b"third").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(w.poll(&file), Some(two.clone()));
        let unreadable = w.poll(&file);
        if std::fs::read(&file).is_err() {
            // (Root can read it anyway; then the hash is simply current.)
            assert_eq!(unreadable, Some(two.clone()), "still the last good hash");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            let third = w.poll(&file).expect("retried");
            assert_eq!(third, crate::executables::hash_file(&file).unwrap());
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(w.poll(&file), None, "removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn status_set_reports_changes_only() {
        let u = Updates::default();
        assert!(!u.set(Status::default()));
        let s = Status { ui_release: Some("1-a".into()), ..Status::default() };
        assert!(u.set(s.clone()));
        assert!(!u.set(s.clone()));
        assert_eq!(u.status(), s);
    }
}
