//! Plain shell adapter: the user's shell (see `user_shell`) as a login, interactive shell.
//! State comes from the PTY's foreground process group: Working while a job owns the
//! terminal, Idle otherwise.

use std::path::Path;

use parking_lot::RwLock;

use crate::adapters::*;
use crate::model::*;

pub struct ShellAdapter;

/// The shell chosen in the window preferences (`prefs.set {shell}`), if any. Set by the hub
/// at start and on every change.
static CONFIGURED: RwLock<Option<String>> = RwLock::new(None);

pub fn set_configured_shell(shell: Option<String>) {
    *CONFIGURED.write() = shell;
}

fn usable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    Path::new(path).is_absolute()
        && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The account's login shell from the password database.
fn login_shell() -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pw.pw_shell.is_null() {
        return None;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(pw.pw_shell) }.to_string_lossy().into_owned();
    Some(s)
}

/// The shell new shell sessions run: the configured one, else the account's login shell,
/// else `$SHELL`, else `/bin/bash`. The login shell comes before `$SHELL` because the daemon
/// inherits `$SHELL` from whoever started it last, often an agent's own tool shell after a
/// `muster reload`, not the user's terminal.
pub fn user_shell() -> String {
    let configured = CONFIGURED.read().clone();
    configured.filter(|s| usable(s)).unwrap_or_else(fallback_shell)
}

/// The shell used when none is configured (or the configured one is gone).
pub fn fallback_shell() -> String {
    login_shell()
        .into_iter()
        .chain(std::env::var("SHELL").ok())
        .find(|s| usable(s))
        .unwrap_or_else(|| "/bin/bash".to_string())
}

/// A shell the user typed (`/usr/bin/fish`, `~/bin/nu` or a bare `fish` on `PATH`) as an
/// absolute path to an executable, or an error saying why not.
pub fn resolve_shell(input: &str) -> anyhow::Result<String> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "no shell given");
    let path = if input.contains('/') {
        crate::paths::expand_tilde(input).to_string_lossy().into_owned()
    } else {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(input).to_string_lossy().into_owned())
            .find(|p| usable(p))
            .ok_or_else(|| anyhow::anyhow!("no {input} found on PATH"))?
    };
    anyhow::ensure!(usable(&path), "{path} isn't an executable file (give an absolute path)");
    Ok(path)
}

fn shell_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().trim_start_matches('-').to_string())
        .unwrap_or_default()
}

/// Shells whose line editor understands bracketed paste (bash >= 5.1, zsh >= 5.1, fish >= 3).
fn supports_bracketed_paste(name: &str) -> bool {
    matches!(name, "bash" | "zsh" | "fish")
}

/// Sent before every delivered command: the shell's input line can't be read from the screen,
/// so a half-typed human draft would otherwise be prepended to it. Ctrl-E (end of line) then
/// Ctrl-U (kill to start of line) empties the line in bash, zsh and fish with emacs key
/// bindings; for shells without a line editor, Ctrl-U is the terminal's kill-line character
/// (and the Ctrl-E it follows is erased with it). In vi mode it may not clear the line.
pub(crate) const LINE_CLEAR: &[u8] = b"\x05\x15";

pub(crate) fn prompt_bytes_for(shell: &str, text: &str) -> Vec<u8> {
    let text = text.replace("\r\n", "\n");
    let text = text.trim_end_matches('\n');
    let mut out = LINE_CLEAR.to_vec();
    // Bracketed paste keeps tabs/newlines literal (no completion, no early submit).
    if supports_bracketed_paste(&shell_name(shell)) {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        // No bracketed paste: run the lines one after another; the last one is submitted by
        // submit_bytes. Tabs would trigger completion, so send them as spaces.
        out.extend_from_slice(text.replace('\n', "\r").replace('\t', " ").as_bytes());
    }
    out
}

impl Adapter for ShellAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Shell
    }

    fn command(&self, ctx: &SpawnCtx) -> anyhow::Result<CommandSpec> {
        let shell = user_shell();
        let mut args = vec!["-l".to_string()];
        args.extend(ctx.extra_args.iter().cloned());
        // What a login sets: programs run from this shell see the shell it really is.
        let env = [("SHELL".to_string(), shell.clone())].into();
        Ok(CommandSpec {
            program: shell,
            args,
            env,
            consumed_initial_prompt: false,
        })
    }

    fn on_hook(&self, _event: &str, _payload: &serde_json::Value) -> Option<StateUpdate> {
        None
    }

    fn detect_from_screen(
        &self,
        screen: &ScreenSnapshot,
        current: AgentState,
    ) -> Option<StateUpdate> {
        if screen.foreground_busy {
            return Some(StateUpdate::new(AgentState::Working));
        }
        if current == AgentState::Starting && screen.lines.iter().all(|l| l.trim().is_empty()) {
            // Prompt not drawn yet.
            return None;
        }
        Some(StateUpdate::new(AgentState::Idle))
    }

    fn prompt_bytes(&self, text: &str) -> Vec<u8> {
        prompt_bytes_for(&user_shell(), text)
    }

    fn approve_bytes(&self, _decision: Decision, _screen: &ScreenSnapshot) -> Option<Vec<u8>> {
        None
    }

    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x03]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_bytes_by_shell() {
        assert_eq!(
            prompt_bytes_for("/bin/bash", "ls -la\n"),
            b"\x05\x15\x1b[200~ls -la\x1b[201~".to_vec()
        );
        assert_eq!(prompt_bytes_for("/bin/sh", "a\tb"), b"\x05\x15a b".to_vec());
        assert_eq!(
            prompt_bytes_for("/usr/bin/zsh", "a\nb"),
            b"\x05\x15\x1b[200~a\nb\x1b[201~".to_vec()
        );
        assert_eq!(
            prompt_bytes_for("/bin/dash", "a\r\nb"),
            b"\x05\x15a\rb".to_vec()
        );
    }

    #[test]
    fn state_from_foreground() {
        let a = ShellAdapter;
        let mut s = ScreenSnapshot::default();
        s.lines = vec!["".into(); 3];
        assert_eq!(a.detect_from_screen(&s, AgentState::Starting), None);
        s.lines[0] = "$ ".into();
        assert_eq!(
            a.detect_from_screen(&s, AgentState::Starting)
                .unwrap()
                .state,
            AgentState::Idle
        );
        s.foreground_busy = true;
        assert_eq!(
            a.detect_from_screen(&s, AgentState::Idle).unwrap().state,
            AgentState::Working
        );
    }
}
