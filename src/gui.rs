//! Opening the UI. With the `gui` feature: a native wry/tao window, run by a detached
//! `muster native-window` process, so every `muster` invocation gets its own window and the
//! command returns at once. Without it: the first available Chromium-family browser in `--app`
//! mode (detached), else `xdg-open`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Context;

/// Open a UI window on daemon `d` showing `window` (else the page claims one itself). With the
/// `gui` feature this starts a detached native window process (see `run_window`); the window
/// is handed the client token in-process and loads the plain URL, so no token or launch code
/// ever goes into a URL or onto a command line. Otherwise (or with MUSTER_BROWSER set) a
/// browser opens `browser_url()`, a single-use launch-code URL.
pub fn open_ui(
    d: &crate::paths::Discovery,
    window: Option<i64>,
    browser_url: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    {
        if std::env::var_os("MUSTER_BROWSER").is_none() {
            return spawn_window_process(window);
        }
    }
    let _ = (d, window);
    open_browser(&browser_url()?)
}

/// `muster native-window`: run the native window in this process (never returns), falling back
/// to a browser when it can't be created. Without the `gui` feature: open a browser.
pub fn run_window(
    d: &crate::paths::Discovery,
    window: Option<i64>,
    browser_url: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    {
        // Pick the WebKitGTK renderer before GTK or any thread starts.
        #[cfg(target_os = "linux")]
        linux::prepare();
        native::run(&d.base_url(), &d.token, d.panel_port, window, decorations(d), browser_url);
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (d, window);
        open_browser(&browser_url()?)
    }
}

/// The shared "system title bar" preference (`prefs.get`), read before the window is built so
/// it never appears with the wrong frame. An older daemon without it: shown.
#[cfg(feature = "gui")]
fn decorations(d: &crate::paths::Discovery) -> bool {
    crate::client::rpc_with_timeout(d, "prefs.get", serde_json::json!({}), std::time::Duration::from_secs(3))
        .ok()
        .and_then(|p| p["decorations"].as_bool())
        .unwrap_or(true)
}

/// Whether the native window may load `target` (wry's navigation handler: main frame and
/// subframes alike, which it cannot tell apart). The daemon's own UI, `about:blank`, and for
/// agent panels (docs/AGENT_PANELS.md) exactly the panel server's sandbox proxy and web
/// preview pages (`http://127.0.0.1:<panel_port>/proxy?...`, `/preview?...`), the proxy's
/// inline app frame (`about:srcdoc`), and the loopback dev servers a web preview frames
/// (http(s) on localhost or 127.0.0.1, never the UI's or the panel server's port).
/// `panel_port` is asked for only when a panel page or a loopback URL is seen, so a backend
/// reload that moved the panel server is followed. Everything else stays blocked.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn allow_navigation(origin: &str, target: &str, panel_port: impl FnOnce() -> Option<u16>) -> bool {
    if target == origin || target.starts_with(&format!("{origin}/")) || target == "about:blank" || target == "about:srcdoc" {
        return true;
    }
    let panel_page = target.strip_prefix("http://127.0.0.1:").and_then(|rest| {
        let (port, path) = rest.split_once('/')?;
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let path = path.split_once('#').map_or(path, |(p, _)| p);
        let page = ["proxy", "preview"].iter().any(|p| path == *p || path.starts_with(&format!("{p}?")));
        if page { port.parse::<u16>().ok() } else { None }
    });
    if let Some(port) = panel_page {
        return port != 0 && panel_port() == Some(port);
    }
    // A web preview's frame. Its page's CSP already holds it to one dev server's origin.
    let Ok(dev) = crate::web_preview::parse(target, &[]) else { return false };
    let ui_port = origin.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
    Some(dev.port) != ui_port && panel_port() != Some(dev.port)
}

/// A message the page sends the native window (`window.ipc.postMessage`, JSON).
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Debug, PartialEq)]
pub enum IpcMessage {
    /// Show or hide the system title bar (`{"decorations": bool}`).
    Decorations(bool),
    /// Close this window (`{"close": true}`).
    Close,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn parse_ipc(body: &str, secret: &str) -> Option<IpcMessage> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let sent = v.get("secret").and_then(|s| s.as_str()).unwrap_or("");
    if secret.is_empty() || !crate::paths::token_eq(sent, secret) {
        return None;
    }
    if let Some(d) = v.get("decorations").and_then(|d| d.as_bool()) {
        return Some(IpcMessage::Decorations(d));
    }
    (v.get("close") == Some(&serde_json::Value::Bool(true))).then_some(IpcMessage::Close)
}

/// Re-exec `muster native-window [--window N]` in its own session. Its stderr goes to the
/// daemon log, where a failed window or browser fallback explains itself.
#[cfg(feature = "gui")]
fn spawn_window_process(window: Option<i64>) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating the muster binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("native-window");
    if let Some(w) = window {
        cmd.args(["--window", &w.to_string()]);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_path())
        .ok();
    spawn_detached(cmd, log.map(Stdio::from))
}

/// Script run before the page's own scripts on every load of the native window: puts the
/// client token where the frontend keeps it (rpc.ts reads `sessionStorage["muster.token"]`)
/// and the window to claim in `sessionStorage["muster.window"]`, and tells the page which
/// `window.ipc` messages this window understands (`__musterNative.ipc`, see `parse_ipc`), only
/// on the daemon's own origin.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn init_script(origin: &str, token: &str, window: Option<i64>, secret: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let win = window.map_or(String::new(), |w| {
        format!("sessionStorage.setItem(\"muster.window\",{});", q(&w.to_string()))
    });
    format!(
        "(function(){{try{{if(window===window.top&&location.origin==={o}){{window.__musterNative={{ipc:2,secret:{s}}};sessionStorage.setItem(\"muster.token\",{t});{win}}}}}catch(e){{}}}})();",
        o = q(origin),
        s = q(secret),
        t = q(token)
    )
}

/// `MUSTER_WEB_INSPECTOR=1`: the native window allows WebKit's inspector (right-click >
/// Inspect Element) in release builds too, so a frozen page can be paused to see its script.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn web_inspector_requested() -> bool {
    std::env::var_os("MUSTER_WEB_INSPECTOR").is_some_and(|v| v == "1")
}

/// How often the native window asks its page for a sign of life (at most one question open).
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// How long the page may leave a question unanswered before the window offers a reload.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const FREEZE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// The script the native window evaluates in its page. The answer comes back only once the
/// page's main thread runs it, so a page stuck in a script never answers, while a hidden or
/// backgrounded page (whose timers and animation frames are throttled) still does at once. It
/// returns the window the page shows (`sessionStorage["muster.window"]`), which a reload of a
/// frozen page asks for again.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn ping_script(origin: &str) -> String {
    let o = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{try{{return location.origin==={o}?String(sessionStorage.getItem(\"muster.window\")||\"\"):\"\"}}catch(e){{return \"\"}}}})()"
    )
}

/// The window id in a `ping_script` answer (JSON, as the webview returns it), if any.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn ping_window(answer: &str) -> Option<i64> {
    let s: String = serde_json::from_str(answer).ok()?;
    s.parse::<i64>().ok().filter(|w| *w > 0)
}

/// What the watchdog wants done on a tick.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Debug, PartialEq)]
pub enum Check {
    Idle,
    /// Ask the page (`ping_script`); its answer goes to `answered` with this number.
    Ping(u64),
    /// The page has not answered for this long: report it and offer a reload, once per freeze.
    Froze(std::time::Duration),
}

/// Whether the native window's page still answers. Pure: the window feeds it ticks (every
/// `PING_EVERY`) and answers, with the time on a monotonic clock, which stands still while the
/// machine is suspended. Only one question is open at a time, and only while armed (a page
/// committed in a live web process), so a page that is loading or a web process that is gone
/// raises nothing.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Debug, Default)]
pub struct Watchdog {
    armed: bool,
    last: u64,
    /// The open question and when it was asked.
    waiting: Option<(u64, std::time::Instant)>,
    last_tick: Option<std::time::Instant>,
    /// This freeze has been reported (until the page answers again).
    frozen: bool,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
impl Watchdog {
    /// A page committed: start (or keep) asking. A question the old page may never answer (a
    /// navigation can drop it) is asked again of the new one.
    pub fn arm(&mut self) {
        if !self.frozen {
            self.waiting = None;
        }
        self.armed = true;
    }

    /// Forget the open question and any freeze: the web process is gone or being replaced.
    /// Answers to earlier questions are ignored from now on.
    pub fn reset(&mut self, armed: bool) {
        *self = Watchdog { armed, last: self.last, last_tick: self.last_tick, ..Default::default() };
    }

    pub fn tick(&mut self, now: std::time::Instant) -> Check {
        // Ticks that come far apart mean this process (or the whole machine) was stopped, not
        // the page: give the open question a fresh start.
        let stalled = self.last_tick.is_some_and(|t| now.saturating_duration_since(t) > PING_EVERY * 3);
        self.last_tick = Some(now);
        if !self.armed {
            return Check::Idle;
        }
        match &mut self.waiting {
            None => {
                self.last += 1;
                self.waiting = Some((self.last, now));
                Check::Ping(self.last)
            }
            Some((_, asked)) => {
                if stalled && !self.frozen {
                    *asked = now;
                }
                let silent = now.saturating_duration_since(*asked);
                if self.frozen || silent < FREEZE_AFTER {
                    return Check::Idle;
                }
                self.frozen = true;
                Check::Froze(silent)
            }
        }
    }

    /// The page answered question `seq` (any reply from the webview, even an error, means its
    /// main thread is not stuck). After a reported freeze: how long the page was silent.
    pub fn answered(&mut self, seq: u64, now: std::time::Instant) -> Option<std::time::Duration> {
        let (_, asked) = self.waiting.filter(|(s, _)| *s == seq)?;
        self.waiting = None;
        std::mem::take(&mut self.frozen).then(|| now.saturating_duration_since(asked))
    }

    /// How long the open question has gone unanswered.
    pub fn silent(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.waiting.map(|(_, asked)| now.saturating_duration_since(asked))
    }
}

/// From a `/proc/<pid>/stat` line: the process's command name (at most 15 bytes) and parent pid.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn parse_proc_stat(stat: &str) -> Option<(&str, u32)> {
    // `pid (comm) state ppid ...`; comm may itself hold spaces and parentheses.
    let (_, rest) = stat.split_once(" (")?;
    let (comm, rest) = rest.rsplit_once(") ")?;
    let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
    Some((comm, ppid))
}

fn find_in_path(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        let p = PathBuf::from(prog);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

fn spawn_detached(mut cmd: Command, stderr: Option<Stdio>) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    #[cfg(all(feature = "gui", target_os = "linux"))]
    linux::scrub_env(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.unwrap_or_else(Stdio::null));
    // SAFETY: setsid is async-signal-safe; it detaches the child from our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("launching the UI")?;
    Ok(())
}

pub fn open_browser(url: &str) -> anyhow::Result<()> {
    let explicit = std::env::var("MUSTER_BROWSER").ok().filter(|s| !s.trim().is_empty());
    let candidates: Vec<String> = match &explicit {
        Some(b) => vec![b.trim().to_string()],
        None => [
            "chromium",
            "google-chrome-stable",
            "brave",
            "brave-browser",
            "chromium-browser",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    };
    for c in &candidates {
        if let Some(bin) = find_in_path(c) {
            let profile = crate::paths::browser_profile_dir();
            let _ = crate::paths::ensure_private_dir(&profile);
            let mut cmd = Command::new(bin);
            cmd.arg(format!("--app={url}"))
                .arg("--class=muster")
                .arg(format!("--user-data-dir={}", profile.display()));
            return spawn_detached(cmd, None);
        }
    }
    if explicit.is_some() {
        anyhow::bail!("MUSTER_BROWSER={} not found", candidates[0]);
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut cmd = Command::new(opener);
    cmd.arg(url);
    spawn_detached(cmd, None).with_context(|| format!("no browser found; open {url} manually"))
}

/// The Linux renderer choice (see `crate::renderer`): gathers the inputs, applies the plan
/// before GTK starts, and keeps this window's "launching" file while the fast path is on
/// probation. No extra process: a crash during startup leaves the file behind, and the next
/// window's `prepare` turns that into the fallback marker.
#[cfg(all(feature = "gui", target_os = "linux"))]
pub mod linux {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::renderer::{self, DISABLE_DMABUF, NV_EXPLICIT_SYNC};

    /// How long the window must stay up after its page loaded before the fast path counts as working.
    const PROBATION: Duration = Duration::from_secs(3);

    /// The plan `prepare` applied, for the checks after GTK starts.
    static PLAN: Mutex<Option<renderer::Plan>> = Mutex::new(None);
    /// This process's launching file while it is on probation.
    static LAUNCHING: Mutex<Option<PathBuf>> = Mutex::new(None);
    /// Set on the re-exec after GDK opened a non-Wayland display (see `after_gtk_init`).
    const REEXEC: &str = "MUSTER_WINDOW_NOT_WAYLAND";
    static NOT_WAYLAND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// First thing in `muster native-window`, before anything can spawn a process: take the
    /// re-exec flag out of the environment so nothing started from here inherits it.
    pub fn take_reexec_flag() {
        if std::env::var_os(REEXEC).is_some() {
            // SAFETY: called at the start of the command, before any thread starts.
            unsafe { std::env::remove_var(REEXEC) };
            NOT_WAYLAND.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Processes this window starts (links, a browser fallback) get the user's environment,
    /// not the renderer variables this window set for itself.
    pub fn scrub_env(cmd: &mut std::process::Command) {
        if let Some(p) = PLAN.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for (k, _) in &p.set_env {
                cmd.env_remove(k);
            }
        }
    }

    unsafe extern "C" {
        fn webkit_get_major_version() -> u32;
        fn webkit_get_minor_version() -> u32;
        fn webkit_get_micro_version() -> u32;
    }

    fn webkit_version() -> String {
        // SAFETY: plain getters on the linked WebKitGTK; no initialisation needed.
        unsafe { format!("{}.{}.{}", webkit_get_major_version(), webkit_get_minor_version(), webkit_get_micro_version()) }
    }

    fn env(name: &str) -> Option<String> {
        std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
    }

    fn data_dir() -> PathBuf {
        crate::paths::data_dir()
    }

    /// `cleanup`: delete launching files of processes that are gone (a launch does; status doesn't).
    fn inputs(cleanup: bool) -> renderer::Inputs {
        let nvidia = std::fs::read_to_string("/proc/driver/nvidia/version")
            .ok()
            .map(|t| renderer::parse_nvidia_version(&t).unwrap_or_else(|| "unknown".into()));
        let webkit = webkit_version();
        let display = renderer::DisplayEnv {
            gdk_backend: env("GDK_BACKEND"),
            wayland_display: env("WAYLAND_DISPLAY"),
            wayland_socket: env("WAYLAND_SOCKET"),
            xdg_runtime_dir: env("XDG_RUNTIME_DIR"),
            display: env("DISPLAY"),
        };
        let crashed_launch = nvidia.as_deref().is_some_and(|n| {
            renderer::scan_launching(
                &renderer::launching_dir(&data_dir()),
                &renderer::marker_key(n, Some(&webkit)),
                crate::paths::pid_alive,
                cleanup,
            )
        });
        let not_wayland = NOT_WAYLAND.load(std::sync::atomic::Ordering::Relaxed);
        renderer::Inputs {
            user_disable_dmabuf: env(DISABLE_DMABUF),
            user_nv_explicit_sync: env(NV_EXPLICIT_SYNC),
            wayland: !not_wayland && renderer::expect_wayland(&display, |p| p.exists()),
            nvidia,
            webkit: Some(webkit),
            marker: renderer::read_marker(&renderer::marker_path(&data_dir())),
            crashed_launch,
        }
    }

    fn describe(p: &renderer::Plan) -> String {
        format!("{} ({})", if p.dmabuf { "DMA-BUF renderer on" } else { "DMA-BUF renderer off" }, p.reason)
    }

    /// For `muster status`: what a window opened from this shell's environment would use.
    pub fn status_line() -> String {
        let mut line = describe(&renderer::decide(&inputs(false)));
        let marker = renderer::marker_path(&data_dir());
        if marker.exists() {
            line.push_str(&format!("; fallback marker: {} (delete it to retry DMA-BUF)", marker.display()));
        }
        line
    }

    /// Choose the renderer and set its environment. Runs in `muster native-window` after the
    /// daemon is found (so a daemon this process starts never inherits these variables) and
    /// before GTK or any thread starts.
    pub fn prepare() {
        let plan = renderer::decide(&inputs(true));
        eprintln!("muster: native window: {}", describe(&plan));
        if plan.write_marker
            && let Some(key) = &plan.marker_key
        {
            let path = renderer::marker_path(&data_dir());
            match renderer::write_marker(&path, key) {
                Ok(()) => eprintln!(
                    "muster: native window: DMA-BUF renderer off from now on for {key}; delete {} to try it again",
                    path.display()
                ),
                Err(e) => eprintln!("muster: native window: could not write {}: {e}", path.display()),
            }
        }
        for (k, v) in &plan.set_env {
            // SAFETY: called before the event loop or any other thread of this process starts.
            unsafe { std::env::set_var(k, v) };
        }
        *PLAN.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan);
    }

    /// After GTK opened its display, before WebKit starts. If the fast path was chosen for a
    /// Wayland display but GDK opened X11 (a black window there), re-exec this process with
    /// DMA-BUF off: WebKit reads its environment when the webview is created, and changing the
    /// environment of a process whose GTK threads are running is not safe. Otherwise, when the
    /// fast path is on probation, write this process's launching file.
    pub fn after_gtk_init(is_wayland: bool) {
        let plan = PLAN.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(plan) = plan else { return };
        let expected = plan.dmabuf && plan.guarded;
        if expected && !is_wayland {
            use std::os::unix::process::CommandExt;
            eprintln!("muster: native window: GDK opened a non-Wayland display; restarting with DMA-BUF off");
            if let Ok(exe) = std::env::current_exe() {
                let mut cmd = std::process::Command::new(exe);
                cmd.args(std::env::args_os().skip(1)).env(REEXEC, "1");
                scrub_env(&mut cmd);
                let err = cmd.exec();
                eprintln!("muster: native window: restart failed ({err}); continuing with DMA-BUF on");
            }
        }
        if !plan.guarded {
            if is_wayland && !plan.dmabuf && plan.reason.starts_with("DMA-BUF is only enabled") {
                eprintln!("muster: native window: note: GDK opened a Wayland display, but the environment did not predict it");
            }
            return;
        }
        let Some(key) = &plan.marker_key else { return };
        match renderer::write_launching(&renderer::launching_dir(&data_dir()), std::process::id(), key) {
            Ok(path) => *LAUNCHING.lock().unwrap_or_else(|e| e.into_inner()) = Some(path),
            Err(e) => eprintln!("muster: native window: could not write the launching file ({e}); a startup crash won't be detected"),
        }
    }

    /// The page loaded: end probation once the window has stayed up a little longer.
    pub fn page_loaded() {
        if LAUNCHING.lock().map(|l| l.is_none()).unwrap_or(true) {
            return;
        }
        std::thread::spawn(|| {
            std::thread::sleep(PROBATION);
            if end_probation() {
                eprintln!("muster: native window: DMA-BUF renderer up");
            }
        });
    }

    /// WebKit's web process crashed while the fast path was on probation: the window survives
    /// (blank), so nothing would be left behind. Record the failure now for later windows.
    pub fn web_process_crashed() {
        if !end_probation() {
            return;
        }
        let plan = PLAN.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(key) = plan.and_then(|p| p.marker_key) {
            let path = renderer::marker_path(&data_dir());
            match renderer::write_marker(&path, &key) {
                Ok(()) => eprintln!(
                    "muster: native window: the web process crashed during startup with DMA-BUF on; later windows use DMA-BUF off (delete {} to try again). Close this window and open a new one.",
                    path.display()
                ),
                Err(e) => eprintln!("muster: native window: could not write {}: {e}", path.display()),
            }
        }
    }

    /// Delete this process's launching file (probation over, or a normal exit). True if there was one.
    pub fn end_probation() -> bool {
        let path = LAUNCHING.lock().unwrap_or_else(|e| e.into_inner()).take();
        path.map(|p| std::fs::remove_file(p).is_ok()).unwrap_or(false)
    }

    /// This window's WebKit web processes (WebKitGTK says no pid; they are our children), for
    /// the log: "1234", "1234, 5678", or "unknown".
    pub fn web_processes() -> String {
        let me = std::process::id();
        let mut pids: Vec<u32> = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
                    super::parse_proc_stat(&s).is_some_and(|(comm, ppid)| ppid == me && comm.starts_with("WebKitWebProces"))
                })
            })
            .collect();
        pids.sort_unstable();
        if pids.is_empty() {
            return "unknown".into();
        }
        pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(feature = "gui")]
mod native {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::{NewWindowResponse, WebViewBuilder};

    use super::IpcMessage;

    /// What reaches the event loop from the webview's handlers.
    enum UserEvent {
        Title(String),
        Ipc(IpcMessage),
        /// The watchdog's clock (every `PING_EVERY`).
        #[cfg(target_os = "linux")]
        Tick,
        /// The page answered watchdog question `.0` (`ping_script`'s result as JSON).
        #[cfg(target_os = "linux")]
        Pong(u64, String),
        /// A page committed in the webview (its web process runs).
        #[cfg(target_os = "linux")]
        PageCommitted,
        /// The web process crashed or was killed by someone else.
        #[cfg(target_os = "linux")]
        WebProcessGone,
        /// The answer to the "stopped responding" offer: true for Reload window.
        #[cfg(target_os = "linux")]
        Offer(bool),
        /// SIGTERM, SIGHUP or SIGINT: close like the user would.
        #[cfg(target_os = "linux")]
        Quit,
    }

    /// When `ago` was, on the local clock, for the log.
    #[cfg(target_os = "linux")]
    fn at(ago: std::time::Duration) -> String {
        jiff::Zoned::now().checked_sub(ago).map(|t| t.strftime("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
    }

    /// The page stopped answering: ask, from the native side (the page can't), whether to
    /// reload it. Not modal, so the window can still be closed; the answer comes back as
    /// `UserEvent::Offer`.
    #[cfg(target_os = "linux")]
    fn offer_reload(window: &tao::window::Window, proxy: tao::event_loop::EventLoopProxy<UserEvent>) -> gtk::MessageDialog {
        use gtk::prelude::*;
        use tao::platform::unix::WindowExtUnix;
        let d = gtk::MessageDialog::new(
            Some(window.gtk_window()),
            gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Warning,
            gtk::ButtonsType::None,
            "This Muster window stopped responding",
        );
        d.set_secondary_text(Some(&format!(
            "Its page has not answered for {} seconds. Reload window starts the page again; your sessions keep running. If the page recovers by itself, this message goes away.",
            super::FREEZE_AFTER.as_secs()
        )));
        d.add_button("Keep waiting", gtk::ResponseType::Cancel);
        d.add_button("Reload window", gtk::ResponseType::Accept);
        d.set_default_response(gtk::ResponseType::Accept);
        d.connect_response(move |_, r| {
            let _ = proxy.send_event(UserEvent::Offer(r == gtk::ResponseType::Accept));
        });
        d.present();
        d
    }

    /// Runs the native window; never returns (tao's event loop exits the process). The window
    /// title follows the page's `document.title` (`muster · <window name>`), which is how the
    /// window-manager helpers behind `window.raise` find it. `decorations`: start with the
    /// system title bar; the page toggles it later (and closes the window) over `window.ipc`.
    pub fn run(
        base_url: &str,
        token: &str,
        panel_port: Option<u16>,
        window_id: Option<i64>,
        decorations: bool,
        browser_url: impl FnOnce() -> anyhow::Result<String>,
    ) -> ! {
        let origin = base_url.trim_end_matches('/').to_string();
        let url = format!("{origin}/");
        let mut browser_url = Some(browser_url);
        let mut fallback = move |why: &str| -> ! {
            eprintln!("muster: native window unavailable ({why}); opening a browser instead");
            // A normal exit: not a renderer crash.
            #[cfg(target_os = "linux")]
            super::linux::end_probation();
            let r = browser_url
                .take()
                .map(|f| f())
                .unwrap_or_else(|| Err(anyhow::anyhow!("no URL")));
            let code = match r.and_then(|u| super::open_browser(&u)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("muster: {e:#}");
                    1
                }
            };
            std::process::exit(code)
        };
        // User events carry document title changes and the page's messages into the event loop.
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        // GTK is up (its display open), WebKit not yet: check the display matches the renderer
        // plan, and start the fast path's probation (see `linux`).
        #[cfg(target_os = "linux")]
        {
            use tao::platform::unix::EventLoopWindowTargetExtUnix;
            super::linux::after_gtk_init(event_loop.is_wayland());
        }
        let proxy = event_loop.create_proxy();
        let ipc_proxy = event_loop.create_proxy();
        #[cfg(target_os = "linux")]
        let watch_proxy = event_loop.create_proxy();
        #[cfg(target_os = "linux")]
        let (ping, reload_origin) = (super::ping_script(&origin), origin.clone());
        let ipc_origin = origin.clone();
        let ipc_secret = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect::<String>();
        let load_origin = origin.clone();
        let nav_panel_port = std::sync::Mutex::new(panel_port);
        let window = match WindowBuilder::new()
            .with_title("muster")
            .with_inner_size(LogicalSize::new(1400.0, 900.0))
            .with_decorations(decorations)
            .build(&event_loop)
        {
            Ok(w) => w,
            Err(e) => fallback(&e.to_string()),
        };
        // Release builds get the inspector only on request (README, "Native window watchdog").
        let inspector = super::web_inspector_requested();
        if inspector && !cfg!(debug_assertions) {
            eprintln!("muster: native window: web inspector on (MUSTER_WEB_INSPECTOR=1); right-click the page > Inspect Element");
        }
        let builder = WebViewBuilder::new()
            .with_initialization_script_for_main_only(super::init_script(&origin, token, window_id, &ipc_secret), true)
            .with_url(&url)
            .with_devtools(cfg!(debug_assertions) || inspector)
            // The fast renderer's probation ends a few seconds after our own page loads.
            .with_on_page_load_handler(move |event, url| {
                let ours = url == load_origin || url.starts_with(&format!("{load_origin}/"));
                if ours && matches!(event, wry::PageLoadEvent::Finished) {
                    #[cfg(target_os = "linux")]
                    super::linux::page_loaded();
                }
            })
            .with_document_title_changed_handler(move |title| {
                let _ = proxy.send_event(UserEvent::Title(title));
            })
            // WebKitGTK reports the top-level URI even for framed IPC. Only the main
            // frame receives the per-window capability, so checking the URI alone is not enough.
            .with_ipc_handler(move |req| {
                let from = req.uri().to_string();
                if from != ipc_origin && !from.starts_with(&format!("{ipc_origin}/")) {
                    return;
                }
                if let Some(m) = super::parse_ipc(req.body(), &ipc_secret) {
                    let _ = ipc_proxy.send_event(UserEvent::Ipc(m));
                }
            })
            // Links that try to open a new window go to the system browser.
            .with_new_window_req_handler(|target, _features| {
                let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
                c.arg(target);
                let _ = super::spawn_detached(c, None);
                NewWindowResponse::Deny
            })
            // Keep the window on the daemon's own origin; panel frames load only the panel
            // server's pages and, inside a web preview, loopback dev servers. wry does not say
            // whether a navigation is the main frame's, so a top-level load of those is allowed
            // too: they get no token or IPC capability (the init script checks the origin), and
            // the proxy does nothing outside a frame (web/panels/proxy.html). The UI's frames
            // are sandboxed without top navigation, so only the UI itself could navigate there.
            .with_navigation_handler(move |target| {
                super::allow_navigation(&origin, &target, || {
                    let mut known = nav_panel_port.lock().unwrap_or_else(|e| e.into_inner());
                    let want = target
                        .strip_prefix("http://127.0.0.1:")
                        .and_then(|r| r.split('/').next())
                        .and_then(|p| p.parse::<u16>().ok());
                    // A backend reload starts a new panel server: re-read the daemon's discovery
                    // file (same daemon: same UI port) when a proxy names a port we don't know.
                    if want.is_some() && *known != want
                        && let Some(d) = crate::paths::read_discovery()
                        && d.base_url() == origin
                    {
                        *known = d.panel_port;
                    }
                    *known
                })
            });

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let webview = builder.build(&window);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            match window.default_vbox() {
                Some(vbox) => builder.build_gtk(vbox),
                None => fallback("no GTK container"),
            }
        };
        let webview = match webview {
            Ok(w) => w,
            Err(e) => fallback(&e.to_string()),
        };
        // A web process that crashes leaves this process (and its launching file) alive.
        // The watchdog asks the page every `PING_EVERY` whether it still answers, from when a
        // page commits until its web process goes; a process killed with a signal closes like
        // the user closed it, so its web process goes too.
        #[cfg(target_os = "linux")]
        {
            use webkit2gtk::WebProcessTerminationReason as Why;
            use webkit2gtk::WebViewExt;
            use wry::WebViewExtUnix;
            let gone = watch_proxy.clone();
            webview.webview().connect_web_process_terminated(move |_, reason| {
                if matches!(reason, Why::Crashed) {
                    super::linux::web_process_crashed();
                }
                // Our own reload re-arms the watchdog when its new page commits.
                if !matches!(reason, Why::TerminatedByApi) {
                    let _ = gone.send_event(UserEvent::WebProcessGone);
                }
            });
            let committed = watch_proxy.clone();
            webview.webview().connect_load_changed(move |_, event| {
                if event == webkit2gtk::LoadEvent::Committed {
                    let _ = committed.send_event(UserEvent::PageCommitted);
                }
            });
            let ticks = watch_proxy.clone();
            std::thread::spawn(move || {
                while ticks.send_event(UserEvent::Tick).is_ok() {
                    std::thread::sleep(super::PING_EVERY);
                }
            });
            for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
                let quit = watch_proxy.clone();
                webkit2gtk::glib::source::unix_signal_add_local(signal, move || {
                    let _ = quit.send_event(UserEvent::Quit);
                    webkit2gtk::glib::ControlFlow::Continue
                });
            }
        }
        #[cfg(target_os = "linux")]
        let (mut watchdog, mut offer, mut page_window) = (super::Watchdog::default(), None::<gtk::MessageDialog>, None::<i64>);

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            let _keep = &webview;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                Event::UserEvent(UserEvent::Title(title)) if !title.trim().is_empty() => window.set_title(&title),
                Event::UserEvent(UserEvent::Ipc(IpcMessage::Decorations(on))) => window.set_decorations(on),
                Event::UserEvent(UserEvent::Ipc(IpcMessage::Close)) => *control_flow = ControlFlow::Exit,
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::Quit) => *control_flow = ControlFlow::Exit,
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::Tick) => match watchdog.tick(std::time::Instant::now()) {
                    super::Check::Ping(seq) => {
                        let answer = watch_proxy.clone();
                        let _ = webview.evaluate_script_with_callback(&ping, move |r| {
                            let _ = answer.send_event(UserEvent::Pong(seq, r));
                        });
                    }
                    super::Check::Froze(silent) => {
                        let hint = if inspector {
                            ""
                        } else {
                            "; to see its script next time, open windows with MUSTER_WEB_INSPECTOR=1 (README)"
                        };
                        eprintln!(
                            "muster: native window: the page stopped responding at {} (no answer for {} s; web process {}); offering Reload window{hint}",
                            at(silent),
                            silent.as_secs(),
                            super::linux::web_processes()
                        );
                        offer = Some(offer_reload(&window, watch_proxy.clone()));
                    }
                    super::Check::Idle => {}
                },
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::Pong(seq, answer)) => {
                    if let Some(w) = super::ping_window(&answer) {
                        page_window = Some(w);
                    }
                    if let Some(after) = watchdog.answered(seq, std::time::Instant::now()) {
                        use gtk::prelude::WidgetExtManual;
                        // SAFETY: our own dialog, not used after this; its response handler only sends an event.
                        let withdrawn = offer.take().map(|d| unsafe { d.destroy() }).is_some();
                        eprintln!(
                            "muster: native window: the page is responding again after {} s (it stopped at {}){}",
                            after.as_secs(),
                            at(after),
                            if withdrawn { "; Reload window offer withdrawn" } else { "" }
                        );
                    }
                }
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::PageCommitted) => watchdog.arm(),
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::WebProcessGone) => watchdog.reset(false),
                #[cfg(target_os = "linux")]
                Event::UserEvent(UserEvent::Offer(reload)) => {
                    use gtk::prelude::WidgetExtManual;
                    use webkit2gtk::WebViewExt;
                    use wry::WebViewExtUnix;
                    // Already withdrawn (the page recovered): nothing to do.
                    let Some(d) = offer.take() else { return };
                    // SAFETY: as above.
                    unsafe { d.destroy() };
                    let silent = watchdog.silent(std::time::Instant::now()).map_or(0, |s| s.as_secs());
                    if !reload {
                        eprintln!("muster: native window: kept waiting for the page (no answer for {silent} s)");
                        return;
                    }
                    // The stuck page can't be asked to navigate: end its web process, then load
                    // the UI again (in a new one) on the window it showed.
                    eprintln!(
                        "muster: native window: reloading the page (no answer for {silent} s): ending web process {}",
                        super::linux::web_processes()
                    );
                    webview.webview().terminate_web_process();
                    watchdog.reset(false);
                    let target = page_window.map_or_else(|| format!("{reload_origin}/"), |w| format!("{reload_origin}/#window={w}"));
                    if let Err(e) = webview.load_url(&target) {
                        eprintln!("muster: native window: reload failed ({e})");
                    }
                }
                // A normal exit (closed by the user or the page, or a signal): not a renderer
                // crash. Its web process goes too, even one stuck in a script, which would never
                // notice this process is gone.
                #[cfg(target_os = "linux")]
                Event::LoopDestroyed => {
                    use webkit2gtk::WebViewExt;
                    use wry::WebViewExtUnix;
                    super::linux::end_probation();
                    webview.webview().terminate_web_process();
                }
                _ => {}
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_script_is_scoped_and_quoted() {
        let s = super::init_script("http://127.0.0.1:7433", "ab\"c", None, "secret");
        assert!(s.contains(r#"location.origin==="http://127.0.0.1:7433""#), "{s}");
        assert!(s.contains(r#"sessionStorage.setItem("muster.token","ab\"c")"#), "{s}");
        assert!(!s.contains("muster.window"), "{s}");
        assert!(s.contains("window===window.top"), "{s}");
        assert!(s.contains(r#"window.__musterNative={ipc:2,secret:"secret"};"#), "{s}");
        let s = super::init_script("http://127.0.0.1:7433", "t", Some(3), "secret");
        assert!(s.contains(r#"sessionStorage.setItem("muster.window","3");"#), "{s}");
    }

    #[test]
    fn navigation_allows_only_the_ui_and_the_panel_proxy() {
        use super::allow_navigation as allow;
        let ui = "http://127.0.0.1:7433";
        let port = || Some(40001);
        for ok in [
            "http://127.0.0.1:7433", "http://127.0.0.1:7433/", "http://127.0.0.1:7433/#token=x", "about:blank",
            "about:srcdoc", "http://127.0.0.1:40001/proxy", "http://127.0.0.1:40001/proxy?csp=%7B%7D",
        ] {
            assert!(allow(ui, ok, port), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:40002/proxy", "http://127.0.0.1:40001/", "http://127.0.0.1:40001/proxyx",
            "http://127.0.0.1:40001/api/rpc", "http://localhost:40001/proxy", "https://127.0.0.1:40001/proxy",
            "http://127.0.0.1:40001@evil.example/proxy", "http://127.0.0.1:7433.evil.example/", "http://127.0.0.1:0/proxy",
            "https://example.com/", "data:text/html,x", "file:///etc/passwd", "about:srcdoc2", "javascript:alert(1)",
        ] {
            assert!(!allow(ui, bad, port), "{bad}");
        }
        assert!(!allow(ui, "http://127.0.0.1:40001/proxy", || None), "an older daemon has no panel server");
        // A web preview: its page on the panel server, and the loopback dev servers it frames.
        for ok in ["http://127.0.0.1:40001/preview?url=http%3A%2F%2Flocalhost%3A5173%2F", "http://localhost:5173/",
            "http://127.0.0.1:8000/app?x=1#y", "https://localhost:8443/"] {
            assert!(allow(ui, ok, port), "{ok}");
        }
        for bad in ["http://localhost:7433/", "http://[::1]:3000/", "http://[::1]:7433/", "http://localhost:40001/preview", "http://127.0.0.1:40002/preview",
            "http://localhost:5173@evil.example/", "http://localhost.evil.example:5173/", "http://192.168.1.2:5173/",
            "http://2130706433:5173/", "ws://localhost:5173/"] {
            assert!(!allow(ui, bad, port), "{bad}");
        }
        let mut asked = false;
        assert!(allow(ui, "http://127.0.0.1:7433/x", || { asked = true; None }) && !asked, "UI loads never read discovery");
    }

    #[test]
    fn watchdog_reports_a_freeze_once_and_its_recovery() {
        use super::{Check, FREEZE_AFTER, PING_EVERY, Watchdog};
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watchdog::default();
        assert_eq!(w.tick(at(0)), Check::Idle, "nothing asked before a page commits");
        w.arm();
        // An answering page: one question per tick, each answered.
        assert_eq!(w.tick(at(2)), Check::Ping(1));
        assert_eq!(w.answered(1, at(2)), None);
        assert_eq!(w.tick(at(4)), Check::Ping(2));
        // It stops answering: no second question while one is open, a report after FREEZE_AFTER.
        let mut froze = None;
        for s in (6..=30).step_by(2) {
            match w.tick(at(s)) {
                Check::Idle => {}
                Check::Froze(d) => {
                    assert!(froze.is_none(), "reported twice");
                    assert!(d >= FREEZE_AFTER && d < FREEZE_AFTER + PING_EVERY, "{d:?}");
                    froze = Some(s);
                }
                c => panic!("{c:?} at {s}"),
            }
        }
        assert_eq!(froze, Some(14));
        assert_eq!(w.silent(at(30)), Some(Duration::from_secs(26)));
        assert_eq!(w.answered(1, at(31)), None, "a stale answer changes nothing");
        assert_eq!(w.answered(2, at(31)), Some(Duration::from_secs(27)), "recovered, with how long it was silent");
        assert_eq!(w.tick(at(32)), Check::Ping(3));
        assert_eq!(w.answered(3, at(32)), None, "an ordinary answer after a recovery");
        // A second freeze is reported again.
        assert_eq!(w.tick(at(34)), Check::Ping(4));
        for s in (36..=42).step_by(2) {
            assert_eq!(w.tick(at(s)), Check::Idle, "{s}");
        }
        assert_eq!(w.tick(at(44)), Check::Froze(Duration::from_secs(10)));
    }

    #[test]
    fn watchdog_ignores_stalls_gone_processes_and_navigations() {
        use super::{Check, Watchdog};
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watchdog::default();
        w.arm();
        assert_eq!(w.tick(at(0)), Check::Ping(1));
        // This process was stopped for a minute (ticks far apart): the question starts over.
        for s in (60..=68).step_by(2) {
            assert_eq!(w.tick(at(s)), Check::Idle, "{s}");
        }
        assert_eq!(w.tick(at(70)), Check::Froze(Duration::from_secs(10)));
        // The web process went (crash, or our reload): nothing is asked or reported, and the
        // late answer of the dead page is ignored.
        w.reset(false);
        assert_eq!(w.answered(1, at(71)), None);
        for s in (72..=100).step_by(2) {
            assert_eq!(w.tick(at(s)), Check::Idle, "{s}");
        }
        // A new page commits: asked again, with fresh numbers.
        w.arm();
        assert_eq!(w.tick(at(102)), Check::Ping(2));
        // A navigation commits before the answer: the question is asked of the new page.
        w.arm();
        assert_eq!(w.tick(at(104)), Check::Ping(3));
        assert_eq!(w.answered(2, at(104)), None);
        for s in (106..=112).step_by(2) {
            assert_eq!(w.tick(at(s)), Check::Idle, "{s}");
        }
        assert_eq!(w.tick(at(114)), Check::Froze(Duration::from_secs(10)), "the open question is still 3");
        // A commit during a reported freeze keeps it (it is reported once).
        w.arm();
        assert_eq!(w.tick(at(116)), Check::Idle);
        assert_eq!(w.answered(3, at(117)), Some(Duration::from_secs(13)));
    }

    #[test]
    fn ping_script_and_answer() {
        let s = super::ping_script("http://127.0.0.1:7433");
        assert!(s.contains(r#"location.origin==="http://127.0.0.1:7433""#), "{s}");
        assert!(s.contains(r#"sessionStorage.getItem("muster.window")"#), "{s}");
        assert_eq!(super::ping_window(r#""3""#), Some(3));
        for none in [r#""""#, r#""0""#, r#""-2""#, r#""x""#, "3", "", "null"] {
            assert_eq!(super::ping_window(none), None, "{none}");
        }
    }

    #[test]
    fn proc_stat() {
        use super::parse_proc_stat as p;
        assert_eq!(p("4242 (WebKitWebProces) S 4200 4200 4200 0 -1"), Some(("WebKitWebProces", 4200)));
        assert_eq!(p("7 (a) b (c) R 1 7 7 0"), Some(("a) b (c", 1)));
        assert_eq!(p("garbage"), None);
    }

    #[test]
    fn ipc_messages() {
        use super::{IpcMessage, parse_ipc};
        assert_eq!(parse_ipc(r#"{"decorations":false,"secret":"s"}"#, "s"), Some(IpcMessage::Decorations(false)));
        assert_eq!(parse_ipc(r#"{"decorations":true,"secret":"s"}"#, "s"), Some(IpcMessage::Decorations(true)));
        assert_eq!(parse_ipc(r#"{"close":true,"secret":"s"}"#, "s"), Some(IpcMessage::Close));
        for junk in [r#"{"close":true}"#, r#"{"close":true,"secret":"wrong"}"#, r#"{"secret":"s","close":1}"#, r#"{"decorations":"no"}"#, "close", "{}", ""] {
            assert_eq!(parse_ipc(junk, "s"), None, "{junk}");
        }
    }
}
