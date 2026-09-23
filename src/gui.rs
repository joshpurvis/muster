//! Opening the UI. With the `gui` feature: a native wry/tao window titled "Muster" (blocks
//! until it is closed). Without it: the first available Chromium-family browser in `--app`
//! mode (detached), else `xdg-open`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Context;

/// Open the UI at `url`. With the `gui` feature this runs the window's event loop and only
/// returns (exits the process) when the window is closed.
pub fn open(url: &str) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    {
        if std::env::var_os("MUSTER_BROWSER").is_none() {
            native::run(url);
        }
    }
    open_browser(url)
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
    pub fn run(url: &str) -> ! {
        let event_loop = EventLoop::new();
        let window = match WindowBuilder::new()
            .with_title("Muster")
            .with_inner_size(LogicalSize::new(1400.0, 900.0))
            .build(&event_loop)
        {
            Ok(w) => w,
            Err(e) => fallback(url, &e.to_string()),
        };
        let origin = url.split("/#").next().unwrap_or(url).trim_end_matches('/').to_string();
        let builder = WebViewBuilder::new()
            .with_url(url)
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
                None => fallback(url, "no GTK container"),
            }
        };
        let webview = match webview {
            Ok(w) => w,
            Err(e) => fallback(url, &e.to_string()),
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

    fn fallback(url: &str, why: &str) -> ! {
        eprintln!("muster: native window unavailable ({why}); opening a browser instead");
        let code = match super::open_browser(url) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("muster: {e:#}");
                1
            }
        };
        std::process::exit(code)
    }
}
