//! Which WebKitGTK renderer the Linux native window uses, and the files that remember a machine
//! where the fast one failed. Pure decision logic plus file I/O, so it is tested without
//! starting WebKit; `gui.rs` gathers the inputs and applies the plan.
//!
//! WebKitGTK's DMA-BUF renderer is much faster to present than the fallback path, but on NVIDIA
//! under Wayland it dies at once with "Error 71 (Protocol error)" unless the driver's explicit
//! sync is turned off (`__NV_DISABLE_EXPLICIT_SYNC=1`). So on NVIDIA + Wayland the window keeps
//! DMA-BUF on with explicit sync off; everywhere else DMA-BUF stays off, as before. A user-set
//! `WEBKIT_DISABLE_DMABUF_RENDERER` or `__NV_DISABLE_EXPLICIT_SYNC` always wins.
//!
//! Crash detection needs no extra process: a window trying the fast path writes a "launching"
//! file (`<data>/webkit-dmabuf-launching/<pid>`, holding the marker key) just before WebKit
//! starts, and deletes it once its page has loaded and it stayed up a few seconds, or on any
//! normal exit. A file left behind by a process that is no longer running means that attempt
//! died during startup: the next window uses DMA-BUF off and writes the persistent fallback
//! marker (`<data>/webkit-dmabuf-failed`), keyed by the NVIDIA driver and WebKitGTK versions so
//! an update tries the fast path again.

#![cfg_attr(not(all(feature = "gui", target_os = "linux")), allow(dead_code))]

use std::path::{Path, PathBuf};

pub const DISABLE_DMABUF: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
pub const NV_EXPLICIT_SYNC: &str = "__NV_DISABLE_EXPLICIT_SYNC";

/// Everything the decision depends on, gathered once by the caller.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// The user's `WEBKIT_DISABLE_DMABUF_RENDERER`, if set.
    pub user_disable_dmabuf: Option<String>,
    /// The user's `__NV_DISABLE_EXPLICIT_SYNC`, if set.
    pub user_nv_explicit_sync: Option<String>,
    /// GDK is expected to open a Wayland display (see `expect_wayland`).
    pub wayland: bool,
    /// The NVIDIA proprietary driver's version (`/proc/driver/nvidia/version`), if loaded.
    pub nvidia: Option<String>,
    /// The linked WebKitGTK's version.
    pub webkit: Option<String>,
    /// The contents of the fallback marker file, if any.
    pub marker: Option<String>,
    /// A launching file with this machine's key was left by a process that is gone.
    pub crashed_launch: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Environment variables to set for the window (never one the user set).
    pub set_env: Vec<(&'static str, &'static str)>,
    /// Whether the DMA-BUF renderer will be on.
    pub dmabuf: bool,
    /// The fast path was chosen automatically: write a launching file so a startup crash is
    /// caught by the next launch.
    pub guarded: bool,
    /// The previous attempt crashed: write the persistent fallback marker now.
    pub write_marker: bool,
    /// What the marker is keyed by on this machine (driver + WebKitGTK versions).
    pub marker_key: Option<String>,
    /// One line for the log and `muster status`.
    pub reason: String,
}

/// The marker key: a driver or WebKitGTK update changes it, so the fast path is tried again.
pub fn marker_key(nvidia: &str, webkit: Option<&str>) -> String {
    format!("nvidia {nvidia} webkit {}", webkit.unwrap_or("unknown"))
}

/// WebKit reads the variable as "disabled unless it is `0`".
fn disables(v: &str) -> bool {
    v != "0"
}

pub fn decide(i: &Inputs) -> Plan {
    let nv_wayland = i.wayland && i.nvidia.is_some();
    let key = i.nvidia.as_deref().map(|n| marker_key(n, i.webkit.as_deref()));
    // Explicit sync off is what lets DMA-BUF work on NVIDIA + Wayland; set it whenever the
    // renderer will be on there, unless the user chose a value.
    let nv_env = |dmabuf: bool| -> Vec<(&'static str, &'static str)> {
        if dmabuf && nv_wayland && i.user_nv_explicit_sync.is_none() { vec![(NV_EXPLICIT_SYNC, "1")] } else { vec![] }
    };
    if let Some(v) = &i.user_disable_dmabuf {
        let dmabuf = !disables(v);
        return Plan {
            set_env: nv_env(dmabuf),
            dmabuf,
            guarded: false,
            write_marker: false,
            marker_key: key,
            reason: format!("{DISABLE_DMABUF}={v} set by the user"),
        };
    }
    let off = |reason: String, write_marker: bool| Plan {
        set_env: vec![(DISABLE_DMABUF, "1")],
        dmabuf: false,
        guarded: false,
        write_marker,
        marker_key: key.clone(),
        reason,
    };
    let Some(nvidia) = i.nvidia.as_deref().filter(|_| i.wayland) else {
        return off("DMA-BUF is only enabled automatically for NVIDIA on Wayland".into(), false);
    };
    let k = key.clone().unwrap_or_default();
    if i.marker.as_deref().map(str::trim) == Some(k.as_str()) {
        return off(format!("DMA-BUF failed before with this driver and WebKitGTK ({k}); fallback marker present"), false);
    }
    if i.crashed_launch {
        return off(format!("the last DMA-BUF window died during startup ({k}); falling back from now on"), true);
    }
    let sync = match &i.user_nv_explicit_sync {
        Some(v) => format!("{NV_EXPLICIT_SYNC}={v} set by the user"),
        None => format!("{NV_EXPLICIT_SYNC}=1"),
    };
    Plan {
        set_env: nv_env(true),
        dmabuf: true,
        guarded: true,
        write_marker: false,
        marker_key: key,
        reason: format!("NVIDIA {nvidia} on Wayland, {sync}; a startup crash switches later windows to DMA-BUF off"),
    }
}

/// The version from `/proc/driver/nvidia/version` ("NVRM version: NVIDIA UNIX ... 615.78.08 ...").
pub fn parse_nvidia_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("NVRM version"))?;
    line.split_whitespace()
        .find(|w| {
            w.contains('.') && w.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// The session's display environment, as GDK would see it.
#[derive(Debug, Clone, Default)]
pub struct DisplayEnv {
    pub gdk_backend: Option<String>,
    pub wayland_display: Option<String>,
    pub wayland_socket: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub display: Option<String>,
}

/// Whether GDK will open a Wayland display, following GDK 3's rules closely enough: it tries
/// the `GDK_BACKEND` list in order (unset or `*`: Wayland first, then X11), Wayland works when
/// `WAYLAND_SOCKET` is set or the `WAYLAND_DISPLAY` socket (default `wayland-0`, relative to
/// `XDG_RUNTIME_DIR` unless absolute) exists, and X11 when `DISPLAY` is set. The window checks
/// the display GDK really opened afterwards (`gui.rs`).
pub fn expect_wayland(e: &DisplayEnv, exists: impl Fn(&Path) -> bool) -> bool {
    let nonempty = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let wayland_ok = || {
        if nonempty(&e.wayland_socket).is_some() {
            return true;
        }
        let name = nonempty(&e.wayland_display).unwrap_or_else(|| "wayland-0".into());
        let path = if name.starts_with('/') {
            PathBuf::from(name)
        } else {
            match nonempty(&e.xdg_runtime_dir) {
                Some(dir) => Path::new(&dir).join(name),
                None => return false,
            }
        };
        exists(&path)
    };
    let x11_ok = || nonempty(&e.display).is_some();
    let list = nonempty(&e.gdk_backend).unwrap_or_else(|| "*".into());
    for b in list.split(',').map(str::trim) {
        match b {
            "wayland" if wayland_ok() => return true,
            "x11" if x11_ok() => return false,
            "*" => return wayland_ok(),
            _ => {}
        }
    }
    false
}

pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("webkit-dmabuf-failed")
}

pub fn launching_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("webkit-dmabuf-launching")
}

pub fn read_marker(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn write_marker(path: &Path, key: &str) -> std::io::Result<()> {
    crate::paths::write_private(path, format!("{key}\n").as_bytes())
}

/// What the launching files say: one file per window process, named by its pid.
#[derive(Debug, Default, PartialEq)]
pub struct Leftovers {
    /// A process that is gone left a file with the current key: it died during startup.
    pub crashed: bool,
    /// Files of processes that are gone (crashed or stale keys): safe to delete.
    pub dead: Vec<u32>,
}

/// Judge `(pid, key)` entries: a dead pid with the current key is a startup crash; a dead pid
/// with another key is stale; a live pid is a window starting right now.
pub fn classify(entries: &[(u32, String)], key: &str, alive: impl Fn(u32) -> bool) -> Leftovers {
    let mut out = Leftovers::default();
    for (pid, k) in entries {
        if alive(*pid) {
            continue;
        }
        out.crashed |= k.trim() == key;
        out.dead.push(*pid);
    }
    out
}

/// Read the launching files (pid-named; anything else is ignored).
pub fn read_launching(dir: &Path) -> Vec<(u32, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
            Some((pid, std::fs::read_to_string(e.path()).unwrap_or_default()))
        })
        .collect()
}

/// Check for crashed launches; with `cleanup`, delete the dead processes' files.
pub fn scan_launching(dir: &Path, key: &str, alive: impl Fn(u32) -> bool, cleanup: bool) -> bool {
    let l = classify(&read_launching(dir), key, alive);
    if cleanup {
        for pid in &l.dead {
            let _ = std::fs::remove_file(dir.join(pid.to_string()));
        }
    }
    l.crashed
}

/// This process's launching file (atomic, 0600; one per pid, so concurrent windows never share one).
pub fn write_launching(dir: &Path, pid: u32, key: &str) -> std::io::Result<PathBuf> {
    let path = dir.join(pid.to_string());
    crate::paths::write_private(&path, format!("{key}\n").as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv() -> Inputs {
        Inputs {
            wayland: true,
            nvidia: Some("615.78.08".into()),
            webkit: Some("2.54.1".into()),
            ..Default::default()
        }
    }
    const KEY: &str = "nvidia 615.78.08 webkit 2.54.1";

    fn off_env() -> Vec<(&'static str, &'static str)> {
        vec![(DISABLE_DMABUF, "1")]
    }

    #[test]
    fn nvidia_wayland_enables_dmabuf_with_explicit_sync_off() {
        let p = decide(&nv());
        assert!(p.dmabuf && p.guarded && !p.write_marker, "{p:?}");
        assert_eq!(p.set_env, vec![(NV_EXPLICIT_SYNC, "1")]);
        assert_eq!(p.marker_key.as_deref(), Some(KEY));
    }

    #[test]
    fn other_setups_keep_dmabuf_off() {
        for i in [
            Inputs { wayland: true, ..Default::default() },
            Inputs { wayland: false, ..nv() },
            Inputs { wayland: false, crashed_launch: true, ..nv() },
            Inputs::default(),
        ] {
            let p = decide(&i);
            assert!(!p.dmabuf && !p.guarded && !p.write_marker, "{i:?} {p:?}");
            assert_eq!(p.set_env, off_env());
        }
    }

    #[test]
    fn marker_for_this_driver_and_webkit_disables() {
        let p = decide(&Inputs { marker: Some(format!("{KEY}\n")), ..nv() });
        assert!(!p.dmabuf && !p.guarded && !p.write_marker, "{p:?}");
        assert_eq!(p.set_env, off_env());
        assert!(p.reason.contains("marker"), "{}", p.reason);
        // A driver or WebKit update tries again.
        for stale in ["nvidia 600.1 webkit 2.54.1", "nvidia 615.78.08 webkit 2.52.0", "nvidia 615.78.08 webkit unknown"] {
            let p = decide(&Inputs { marker: Some(stale.into()), ..nv() });
            assert!(p.dmabuf && p.guarded, "{stale}: {p:?}");
        }
    }

    #[test]
    fn crashed_launch_falls_back_and_writes_the_marker() {
        let p = decide(&Inputs { crashed_launch: true, ..nv() });
        assert!(!p.dmabuf && !p.guarded && p.write_marker, "{p:?}");
        assert_eq!(p.set_env, off_env());
        assert_eq!(p.marker_key.as_deref(), Some(KEY));
        // Already marked: nothing more to write.
        let p = decide(&Inputs { crashed_launch: true, marker: Some(KEY.into()), ..nv() });
        assert!(!p.dmabuf && !p.write_marker, "{p:?}");
    }

    #[test]
    fn user_settings_win() {
        let all = |i: Inputs| {
            let p = decide(&i);
            // The user's choice is final: never guarded, never a marker, never their variable overwritten.
            assert!(!p.guarded && !p.write_marker, "{i:?} {p:?}");
            for (k, _) in &p.set_env {
                assert!(*k != DISABLE_DMABUF, "{i:?} {p:?}");
                assert!(!(*k == NV_EXPLICIT_SYNC && i.user_nv_explicit_sync.is_some()), "{i:?} {p:?}");
            }
            p
        };
        for extra in [
            Inputs::default(),
            Inputs { marker: Some(KEY.into()), ..Default::default() },
            Inputs { crashed_launch: true, ..Default::default() },
        ] {
            let base = Inputs { marker: extra.marker.clone(), crashed_launch: extra.crashed_launch, ..nv() };
            let p = all(Inputs { user_disable_dmabuf: Some("1".into()), ..base.clone() });
            assert!(!p.dmabuf && p.set_env.is_empty(), "{p:?}");
            // DMA-BUF forced on: explicit sync still turned off for them, even over a marker.
            let p = all(Inputs { user_disable_dmabuf: Some("0".into()), ..base.clone() });
            assert!(p.dmabuf, "{p:?}");
            assert_eq!(p.set_env, vec![(NV_EXPLICIT_SYNC, "1")]);
            // Both set: nothing of ours.
            for (d, s) in [("0", "0"), ("0", "1"), ("1", "0"), ("1", "1")] {
                let p = all(Inputs {
                    user_disable_dmabuf: Some(d.into()),
                    user_nv_explicit_sync: Some(s.into()),
                    ..base.clone()
                });
                assert_eq!(p.dmabuf, d == "0", "{d} {s}");
                assert!(p.set_env.is_empty(), "{d} {s} {p:?}");
            }
        }
        // Any value but "0" disables, as WebKit reads it.
        assert!(!all(Inputs { user_disable_dmabuf: Some("".into()), ..nv() }).dmabuf);
        // Off the NVIDIA path, a user "0" sets nothing.
        let p = all(Inputs { user_disable_dmabuf: Some("0".into()), ..Default::default() });
        assert!(p.dmabuf && p.set_env.is_empty(), "{p:?}");
        // Only explicit sync set by the user: their value stays, the fast path is still guarded.
        for s in ["0", "1"] {
            let p = decide(&Inputs { user_nv_explicit_sync: Some(s.into()), ..nv() });
            assert!(p.dmabuf && p.guarded && p.set_env.is_empty(), "{s} {p:?}");
            let p = decide(&Inputs { user_nv_explicit_sync: Some(s.into()), marker: Some(KEY.into()), ..nv() });
            assert!(!p.dmabuf && p.set_env == off_env(), "{s} {p:?}");
        }
    }

    #[test]
    fn unknown_webkit_still_keys_the_marker() {
        let p = decide(&Inputs { webkit: None, ..nv() });
        assert_eq!(p.marker_key.as_deref(), Some("nvidia 615.78.08 webkit unknown"));
    }

    #[test]
    fn nvidia_version_parsing() {
        let open = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  615.78.08  Release Build  (notroot@)  Sun Oct 18 15:08:25 UTC 2026\nGCC version:  Selected multilib: .;@m64\n";
        assert_eq!(parse_nvidia_version(open).as_deref(), Some("615.78.08"));
        let closed = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Thu Feb 22 01:44:30 UTC 2024\nGCC version:  gcc version 13.2.1\n";
        assert_eq!(parse_nvidia_version(closed).as_deref(), Some("550.54.14"));
        assert_eq!(parse_nvidia_version("NVRM version: NVIDIA UNIX x86_64 Kernel Module  560.35  x\n").as_deref(), Some("560.35"));
        assert_eq!(parse_nvidia_version(""), None);
        assert_eq!(parse_nvidia_version("GCC version: 13.2.1\n"), None);
    }

    #[test]
    fn wayland_expectation() {
        let sock = |p: &Path| p == Path::new("/run/user/1000/wayland-1") || p == Path::new("/run/user/1000/wayland-0");
        let env = |gdk: Option<&str>, wd: Option<&str>, display: Option<&str>| DisplayEnv {
            gdk_backend: gdk.map(Into::into),
            wayland_display: wd.map(Into::into),
            wayland_socket: None,
            xdg_runtime_dir: Some("/run/user/1000".into()),
            display: display.map(Into::into),
        };
        assert!(expect_wayland(&env(None, Some("wayland-1"), Some(":0")), sock));
        assert!(expect_wayland(&env(Some(""), Some("wayland-1"), None), sock));
        assert!(expect_wayland(&env(Some("*"), Some("wayland-1"), None), sock));
        assert!(expect_wayland(&env(Some("wayland,x11"), Some("wayland-1"), Some(":0")), sock));
        // The default socket name.
        assert!(expect_wayland(&env(None, None, None), sock));
        // GDK_BACKEND lists are tried in order.
        assert!(!expect_wayland(&env(Some("x11"), Some("wayland-1"), Some(":0")), sock));
        assert!(!expect_wayland(&env(Some("x11,wayland"), Some("wayland-1"), Some(":0")), sock));
        assert!(expect_wayland(&env(Some("x11,wayland"), Some("wayland-1"), None), sock));
        // A stale Wayland socket with a working X display: X11.
        assert!(!expect_wayland(&env(Some("wayland,x11"), Some("wayland-9"), Some(":0")), sock));
        assert!(!expect_wayland(&env(None, Some("wayland-9"), Some(":0")), sock));
        // Absolute WAYLAND_DISPLAY, and WAYLAND_SOCKET.
        assert!(expect_wayland(&env(None, Some("/run/user/1000/wayland-1"), None), sock));
        assert!(expect_wayland(&DisplayEnv { wayland_socket: Some("5".into()), ..Default::default() }, |_| false));
        assert!(!expect_wayland(&DisplayEnv { display: Some(":0".into()), ..Default::default() }, |_| false));
    }

    #[test]
    fn leftover_classification() {
        let alive = |pid: u32| pid == 10;
        // A dead pid with this key: crashed during startup.
        let l = classify(&[(7, format!("{KEY}\n"))], KEY, alive);
        assert_eq!(l, Leftovers { crashed: true, dead: vec![7] });
        // A live pid: another window is starting right now.
        assert_eq!(classify(&[(10, KEY.into())], KEY, alive), Leftovers::default());
        // A dead pid with an old key: stale, removed, not a crash.
        assert_eq!(classify(&[(8, "nvidia 1 webkit 2".into())], KEY, alive), Leftovers { crashed: false, dead: vec![8] });
        assert_eq!(classify(&[], KEY, alive), Leftovers::default());
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("muster-renderer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn launching_files() {
        let data = temp("launch");
        let dir = launching_dir(&data);
        let alive = |pid: u32| pid == 10;
        assert!(!scan_launching(&dir, KEY, alive, true));
        // A window starting now (live), then one that died during startup.
        write_launching(&dir, 10, KEY).unwrap();
        assert!(!scan_launching(&dir, KEY, alive, true));
        let dead = write_launching(&dir, 7, KEY).unwrap();
        write_launching(&dir, 8, "nvidia 1 webkit 2").unwrap();
        std::fs::write(dir.join("junk"), "x").unwrap();
        // Status looks without touching anything.
        assert!(scan_launching(&dir, KEY, alive, false));
        assert!(dead.exists());
        // A launch cleans up the dead files and keeps the live one.
        assert!(scan_launching(&dir, KEY, alive, true));
        assert!(!dead.exists() && !dir.join("8").exists() && dir.join("10").exists());
        assert!(!scan_launching(&dir, KEY, alive, true));
        // A normal exit removes its own file: nothing is left to count.
        std::fs::remove_file(dir.join("10")).unwrap();
        assert!(!scan_launching(&dir, KEY, |_| false, true));
        use std::os::unix::fs::PermissionsExt;
        let p = write_launching(&dir, 11, KEY).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn marker_round_trip() {
        let dir = temp("marker");
        let path = marker_path(&dir);
        assert_eq!(read_marker(&path), None);
        write_marker(&path, KEY).unwrap();
        assert_eq!(read_marker(&path).as_deref(), Some(KEY));
        let p = decide(&Inputs { marker: read_marker(&path), ..nv() });
        assert!(!p.dmabuf, "{p:?}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_marker(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
