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
    native::run(&d.base_url(), &d.token, window, browser_url);
    #[cfg(not(feature = "gui"))]
    {
        let _ = (d, window);
        open_browser(&browser_url()?)
    }
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
/// and the window to claim in `sessionStorage["muster.window"]`, only on the daemon's own
/// origin.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn init_script(origin: &str, token: &str, window: Option<i64>) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let win = window.map_or(String::new(), |w| {
        format!("sessionStorage.setItem(\"muster.window\",{});", q(&w.to_string()))
    });
    format!(
        "(function(){{try{{if(location.origin==={o}){{sessionStorage.setItem(\"muster.token\",{t});{win}}}}}catch(e){{}}}})();",
        o = q(origin),
        t = q(token)
    )
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

#[cfg(feature = "gui")]
mod native {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::{NewWindowResponse, WebViewBuilder};

    /// Runs the native window; never returns (tao's event loop exits the process). The window
    /// title follows the page's `document.title` (`muster · <window name>`), which is how the
    /// window-manager helpers behind `window.raise` find it.
    pub fn run(
        base_url: &str,
        token: &str,
        window_id: Option<i64>,
        browser_url: impl FnOnce() -> anyhow::Result<String>,
    ) -> ! {
        let origin = base_url.trim_end_matches('/').to_string();
        let url = format!("{origin}/");
        let mut browser_url = Some(browser_url);
        let mut fallback = move |why: &str| -> ! {
            eprintln!("muster: native window unavailable ({why}); opening a browser instead");
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
        // WebKitGTK's DMA-BUF renderer dies with "Error 71 (Protocol error)" on many Wayland
        // setups (notably NVIDIA). Opt out unless the user chose otherwise.
        #[cfg(target_os = "linux")]
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            // SAFETY: called before the event loop or any other thread of this process starts.
            unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
        }
        // User events carry document title changes into the event loop.
        let event_loop = EventLoopBuilder::<String>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        let window = match WindowBuilder::new()
            .with_title("muster")
            .with_inner_size(LogicalSize::new(1400.0, 900.0))
            .build(&event_loop)
        {
            Ok(w) => w,
            Err(e) => fallback(&e.to_string()),
        };
        let builder = WebViewBuilder::new()
            .with_initialization_script(super::init_script(&origin, token, window_id))
            .with_url(&url)
            .with_devtools(cfg!(debug_assertions))
            .with_document_title_changed_handler(move |title| {
                let _ = proxy.send_event(title);
            })
            // Links that try to open a new window go to the system browser.
            .with_new_window_req_handler(|target, _features| {
                let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
                c.arg(target);
                let _ = super::spawn_detached(c, None);
                NewWindowResponse::Deny
            })
            // Keep the window on the daemon's own origin.
            .with_navigation_handler(move |target| {
                target == origin || target.starts_with(&format!("{origin}/")) || target == "about:blank"
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

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            let _keep = &webview;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                Event::UserEvent(title) if !title.trim().is_empty() => window.set_title(&title),
                _ => {}
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_script_is_scoped_and_quoted() {
        let s = super::init_script("http://127.0.0.1:7433", "ab\"c", None);
        assert!(s.contains(r#"location.origin==="http://127.0.0.1:7433""#), "{s}");
        assert!(s.contains(r#"sessionStorage.setItem("muster.token","ab\"c")"#), "{s}");
        assert!(!s.contains("muster.window"), "{s}");
        let s = super::init_script("http://127.0.0.1:7433", "t", Some(3));
        assert!(s.contains(r#"sessionStorage.setItem("muster.window","3");"#), "{s}");
    }
}
