//! Window-manager helpers for `window.raise`: focus the OS window whose title is
//! `muster · <name>` with whatever helper the session has (hyprctl, swaymsg, wmctrl, xdotool).
//! Every helper is looked up on PATH and gets a short timeout; failures just return false.

use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

const HELPER_TIMEOUT: Duration = Duration::from_millis(1500);

/// Title prefix of every muster client window (`document.title`).
pub fn window_title(name: &str) -> String {
    format!("muster · {name}")
}

/// Characters with a meaning in POSIX ERE, RE2 and PCRE.
const REGEX_META: &[char] = &['\\', '.', '^', '$', '|', '?', '*', '+', '(', ')', '[', ']', '{', '}'];

/// Backslash-escape regex metacharacters (for hyprctl's RE2 and xdotool's POSIX ERE). A comma
/// becomes `.`, because Hyprland splits some dispatcher arguments at commas.
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == ',' || c.is_control() {
            out.push('.');
        } else {
            if REGEX_META.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// Regex for sway criteria, which sit inside `[title="..."]` in a command string: every
/// metacharacter, quote, command separator (`;` `,`) and control character becomes `.` (any
/// char), so the value never needs a backslash and can't end the criteria or chain another
/// command. Slightly looser, never invalid.
pub fn sway_regex(s: &str) -> String {
    s.chars()
        .map(|c| {
            if REGEX_META.contains(&c) || matches!(c, '"' | '\'' | ';' | ',') || c.is_control() {
                '.'
            } else {
                c
            }
        })
        .collect()
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn env_set(k: &str) -> bool {
    std::env::var_os(k).is_some_and(|v| !v.is_empty())
}

/// Run a helper; Some(stdout) when it exited 0 within the timeout.
async fn run(program: &str, args: &[&str]) -> Option<String> {
    let bin = on_path(program)?;
    let child = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(HELPER_TIMEOUT, child).await {
        Ok(Ok(out)) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => None,
    }
}

/// Ask the window manager to focus the window titled exactly `title`. True if a helper
/// reported success.
pub async fn raise(title: &str) -> bool {
    if env_set("HYPRLAND_INSTANCE_SIGNATURE") {
        let arg = format!("title:^({})$", regex_escape(title));
        // hyprctl exits 0 even when nothing matched; it prints "ok" on success.
        if run("hyprctl", &["dispatch", "focuswindow", &arg])
            .await
            .is_some_and(|o| o.trim() == "ok")
        {
            return true;
        }
    }
    if env_set("SWAYSOCK") {
        let cmd = format!("[title=\"^{}$\"] focus", sway_regex(title));
        if run("swaymsg", &[&cmd]).await.is_some() {
            return true;
        }
    }
    if env_set("DISPLAY") {
        if run("wmctrl", &["-F", "-a", title]).await.is_some() {
            return true;
        }
        let re = format!("^{}$", regex_escape(title));
        if run("xdotool", &["search", "--name", &re, "windowactivate"]).await.is_some() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(window_title("3"), "muster · 3");
        assert_eq!(regex_escape("muster · 1"), "muster · 1");
        assert_eq!(regex_escape("a.b*(c)"), r"a\.b\*\(c\)");
        assert_eq!(regex_escape(r"x\y [z] {1} ^$ | ? +"), r"x\\y \[z\] \{1\} \^\$ \| \? \+");
        assert_eq!(regex_escape("a,b"), "a.b");
        assert_eq!(regex_escape("tab\there"), "tab.here");
        assert_eq!(regex_escape("ünï 🙂"), "ünï 🙂");
    }

    #[test]
    fn sway_regex_has_no_quotes_or_backslashes() {
        assert_eq!(sway_regex("muster · 2"), "muster · 2");
        assert_eq!(sway_regex(r#"a "b" \c (d)"#), "a .b. .c .d.");
        assert_eq!(sway_regex("it's"), "it.s");
        for s in [r#"x"]; exec foo"#, r"\\", "[title]", "a, b"] {
            let r = sway_regex(s);
            assert!(!r.contains(['"', '\\', ']', ';', ',']), "{r}");
        }
    }
}
