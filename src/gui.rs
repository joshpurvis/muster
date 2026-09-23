//! Opening the UI. With the `gui` feature: a native wry/tao window titled "Muster" (blocks
//! until it is closed). Without it: the first available Chromium-family browser in `--app`
//! mode (detached), else `xdg-open`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Context;

/// Open the UI of daemon `d`. With the `gui` feature this runs a native window (and only
/// returns, by exiting the process, when it is closed); the window is handed the client token
/// in-process and loads the plain URL, so no token or launch code ever goes into a URL or
/// onto a command line. Otherwise (or with MUSTER_BROWSER set) a browser opens
/// `browser_url()`, a single-use launch-code URL.
pub fn open_ui(
    d: &crate::paths::Discovery,
    browser_url: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    {
        if std::env::var_os("MUSTER_BROWSER").is_none() {
            native::run(&d.base_url(), &d.token, browser_url);
        }
    }
    #[cfg(not(feature = "gui"))]
    let _ = d;
    open_browser(&browser_url()?)
}

/// Script run before the page's own scripts on every load of the native window: puts the
/// client token where the frontend keeps it (rpc.ts reads `sessionStorage["muster.token"]`),
/// only on the daemon's own origin.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn token_init_script(origin: &str, token: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{try{{if(location.origin==={o}){{sessionStorage.setItem(\"muster.token\",{t});}}}}catch(e){{}}}})();",
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

fn spawn_detached(mut cmd: Command) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; it detaches the browser from our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("launching browser")?;
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
            return spawn_detached(cmd);
        }
    }
    if explicit.is_some() {
        anyhow::bail!("MUSTER_BROWSER={} not found", candidates[0]);
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut cmd = Command::new(opener);
    cmd.arg(url);
    spawn_detached(cmd).with_context(|| format!("no browser found; open {url} manually"))
}

#[cfg(feature = "gui")]
mod native {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;
    use wry::{NewWindowResponse, WebViewBuilder};

    /// Runs the native window; never returns (tao's event loop exits the process).
    pub fn run(base_url: &str, token: &str, browser_url: impl FnOnce() -> anyhow::Result<String>) -> ! {
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
        let event_loop = EventLoop::new();
        let window = match WindowBuilder::new()
            .with_title("Muster")
            .with_inner_size(LogicalSize::new(1400.0, 900.0))
            .build(&event_loop)
        {
            Ok(w) => w,
            Err(e) => fallback(&e.to_string()),
        };
        let builder = WebViewBuilder::new()
            .with_initialization_script(super::token_init_script(&origin, token))
            .with_url(&url)
            .with_devtools(cfg!(debug_assertions))
            // Links that try to open a new window go to the system browser.
            .with_new_window_req_handler(|target, _features| {
                let _ = super::spawn_detached({
                    let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
                    c.arg(target);
                    c
                });
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
            if let Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } = event
            {
                *control_flow = ControlFlow::Exit;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_script_is_scoped_and_quoted() {
        let s = super::token_init_script("http://127.0.0.1:7433", "ab\"c");
        assert!(s.contains(r#"location.origin==="http://127.0.0.1:7433""#), "{s}");
        assert!(s.contains(r#"sessionStorage.setItem("muster.token","ab\"c")"#), "{s}");
    }
}
