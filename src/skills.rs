//! Agent skills Muster ships (skills/<name>/SKILL.md) and `muster skills install|status`, which
//! copies them into Claude Code's and Codex's user skill folders. A marker next to each
//! installed file records the hash of what Muster wrote, so a file the user edited is never
//! overwritten without --force. Nothing else in those folders is touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

pub struct Skill {
    pub name: &'static str,
    pub text: &'static str,
}

pub const SKILLS: &[Skill] = &[
    Skill { name: "muster-pr-review", text: include_str!("../skills/muster-pr-review/SKILL.md") },
    Skill { name: "muster-panels", text: include_str!("../skills/muster-panels/SKILL.md") },
];

/// The hash of the SKILL.md Muster last wrote, beside it.
const MARKER: &str = ".muster-installed";

pub fn get(name: &str) -> &'static Skill {
    SKILLS.iter().find(|s| s.name == name).expect("built-in skill")
}

/// A skill's instructions without its YAML frontmatter (what an agent template reuses).
pub fn body(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|end| &rest[end + 5..]))
        .unwrap_or(text)
        .trim_start_matches('\n')
}

#[derive(clap::Subcommand)]
pub enum SkillsCmd {
    /// Install or update Muster's agent skills for Claude Code and Codex (both unless one is named).
    Install {
        /// Claude Code: $CLAUDE_CONFIG_DIR/skills, else ~/.claude/skills.
        #[arg(long)] claude: bool,
        /// Codex: $CODEX_HOME/skills, else ~/.codex/skills.
        #[arg(long)] codex: bool,
        /// Show what would change; write nothing.
        #[arg(long)] dry_run: bool,
        /// Also replace files you edited since Muster installed them.
        #[arg(long)] force: bool,
    },
    /// Show each skill and target: missing, current, outdated or locally modified.
    Status {
        #[arg(long)] claude: bool,
        #[arg(long)] codex: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Target {
    pub tool: &'static str,
    /// The tool's user skills folder.
    pub root: PathBuf,
}

/// Targets from the environment: `env` reads a variable (HOME, CLAUDE_CONFIG_DIR, CODEX_HOME).
pub fn targets(claude: bool, codex: bool, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Vec<Target>> {
    let (claude, codex) = if claude || codex { (claude, codex) } else { (true, true) };
    let home = || env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).context("HOME is not set");
    let dir = |var: &str, default: &str| -> anyhow::Result<PathBuf> {
        Ok(match env(var).filter(|v| !v.is_empty()) { Some(v) => PathBuf::from(v), None => home()?.join(default) })
    };
    let mut out = Vec::new();
    if claude { out.push(Target { tool: "claude", root: dir("CLAUDE_CONFIG_DIR", ".claude")?.join("skills") }); }
    if codex { out.push(Target { tool: "codex", root: dir("CODEX_HOME", ".codex")?.join("skills") }); }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Missing,
    Current,
    /// Muster's earlier version, untouched since.
    Outdated,
    /// Edited by someone else (or not written by Muster).
    Modified,
    /// Something Muster never writes through or over: a symlink (even a dangling one), a
    /// special or oversized file, or an unreadable entry. Install refuses it even with --force.
    Blocked(String),
}

impl State {
    fn word(&self) -> String {
        match self {
            State::Missing => "missing".into(), State::Current => "current".into(), State::Outdated => "outdated".into(),
            State::Modified => "locally modified".into(), State::Blocked(why) => format!("blocked: {why}"),
        }
    }
}

fn sha(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// Largest SKILL.md or marker Muster reads.
const MAX_READ: u64 = 256 * 1024;

pub fn file(target: &Target, skill: &Skill) -> PathBuf { target.root.join(skill.name).join("SKILL.md") }
fn marker(target: &Target, skill: &Skill) -> PathBuf { target.root.join(skill.name).join(MARKER) }

/// A directory Muster manages: Ok(false) when missing; a symlink or non-directory is refused.
fn check_dir(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(m) if m.file_type().is_symlink() => Err(format!("{} is a symlink", path.display())),
        Ok(m) if !m.is_dir() => Err(format!("{} is not a directory", path.display())),
        Ok(_) => Ok(true),
    }
}

/// A managed file's bytes (None when missing). Regular files only, never through a symlink
/// (dangling ones included), without blocking on a FIFO, and at most [`MAX_READ`] bytes.
fn read_entry(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let shown = path.display();
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{shown}: {e}")),
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{shown} is a symlink")),
        Ok(m) if !m.is_file() => return Err(format!("{shown} is not a regular file")),
        Ok(m) if m.len() > MAX_READ => return Err(format!("{shown} is larger than 256 KiB")),
        Ok(_) => {}
    }
    let file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)
        .map_err(|e| format!("{shown}: {e}"))?;
    if !file.metadata().map_err(|e| format!("{shown}: {e}"))?.is_file() { return Err(format!("{shown} is not a regular file")) }
    let mut bytes = Vec::new();
    file.take(MAX_READ + 1).read_to_end(&mut bytes).map_err(|e| format!("{shown}: {e}"))?;
    if bytes.len() as u64 > MAX_READ { return Err(format!("{shown} is larger than 256 KiB")) }
    Ok(Some(bytes))
}

/// What is on disk for a skill in a target, and its state. Compared again right before writing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed { state: State, skill: Option<Vec<u8>>, marker: Option<Vec<u8>> }

fn observe(target: &Target, skill: &Skill) -> Observed {
    let blocked = |why: String| Observed { state: State::Blocked(why), skill: None, marker: None };
    for dir in [target.root.clone(), target.root.join(skill.name)] {
        match check_dir(&dir) {
            Err(why) => return blocked(why),
            Ok(false) => return Observed { state: State::Missing, skill: None, marker: None },
            Ok(true) => {}
        }
    }
    let path = file(target, skill);
    let current = match read_entry(&path) {
        Ok(bytes) => bytes,
        // A symlink in place of SKILL.md is the user's (or someone's) own arrangement.
        Err(why) if why.ends_with("is a symlink") => return blocked(format!("locally modified ({why})")),
        Err(why) => return blocked(why),
    };
    let mark = match read_entry(&marker(target, skill)) { Ok(m) => m, Err(why) => return blocked(why) };
    let state = match &current {
        None => State::Missing,
        Some(bytes) if bytes == skill.text.as_bytes() => State::Current,
        Some(bytes) if mark.as_deref().is_some_and(|m| String::from_utf8_lossy(m).trim() == sha(bytes)) => State::Outdated,
        Some(_) => State::Modified,
    };
    Observed { state, skill: current, marker: mark }
}

pub fn state(target: &Target, skill: &Skill) -> State { observe(target, skill).state }

/// Creates `dir` (one level, 0700) if missing, then insists it is a real directory.
fn make_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    if !check_dir(dir)? {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("creating {}: {e}", dir.display())),
        }
        if !check_dir(dir)? { return Err(format!("{} vanished", dir.display())) }
    }
    Ok(())
}

/// Writes `bytes` as `dir/name`: an exclusively created, unpredictably named 0600 temporary
/// file (never opened through a symlink), then a rename. With `no_replace`, an entry that
/// appeared meanwhile is left alone and the write fails.
fn write_file(dir: &Path, name: &str, bytes: &[u8], no_replace: bool) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let target = dir.join(name);
    let (tmp, mut f) = (0..8).find_map(|_| {
        let tmp = dir.join(format!(".{name}.muster-{:016x}.tmp", rand::random::<u64>()));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp) {
            Ok(f) => Some(Ok((tmp, f))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(e) => Some(Err(format!("writing in {}: {e}", dir.display()))),
        }
    }).unwrap_or_else(|| Err(format!("no free temporary name in {}", dir.display())))?;
    let result = f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| e.to_string()).and_then(|()| rename(&tmp, &target, no_replace));
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result.map_err(|e| format!("writing {}: {e}", target.display()))
}

fn rename(from: &Path, to: &Path, no_replace: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if no_replace {
        use std::os::unix::ffi::OsStrExt;
        let (a, b) = (std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(|e| e.to_string())?,
            std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|e| e.to_string())?);
        // SAFETY: both are valid NUL-terminated paths for the duration of the call.
        let r = unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_NOREPLACE) };
        return if r == 0 { Ok(()) } else {
            let e = std::io::Error::last_os_error();
            Err(if e.kind() == std::io::ErrorKind::AlreadyExists { "changed while installing (it appeared)".into() } else { e.to_string() })
        };
    }
    let _ = no_replace;
    std::fs::rename(from, to).map_err(|e| e.to_string())
}

/// One planned change: what `install` would do for a skill in a target.
pub struct Plan { pub target: Target, pub skill: &'static Skill, seen: Observed }

impl Plan {
    pub fn state(&self) -> &State { &self.seen.state }
}

pub fn plan(targets: &[Target]) -> Vec<Plan> {
    targets.iter().flat_map(|t| SKILLS.iter().map(move |s| Plan { target: t.clone(), skill: s, seen: observe(t, s) })).collect()
}

fn tilde(path: &Path, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()).and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The lines to print, and the reason the command fails (exit 1) if it does.
pub struct Report { pub lines: Vec<String>, pub failed: Option<String> }

fn line(tool: &str, what: &str, path: &Path, home: Option<&str>) -> String {
    format!("{tool:<10} {what:<26} {}", tilde(path, home))
}

/// Installs every skill into every target. Nothing is written while any entry is blocked, or
/// locally modified without `force`. Each file is checked again right before it is written
/// (a change since planning stops that file); failures are reported per file.
pub fn install(targets: &[Target], dry_run: bool, force: bool, home: Option<&str>) -> Report {
    install_with(targets, dry_run, force, home, &|_| {})
}

fn install_with(targets: &[Target], dry_run: bool, force: bool, home: Option<&str>, before_write: &dyn Fn(&Plan)) -> Report {
    let plans = plan(targets);
    let mut lines: Vec<String> = plans.iter().map(|p| {
        let what = match (p.state(), force) {
            (State::Missing, _) => "new".to_string(),
            (State::Current, _) => "unchanged".into(),
            (State::Outdated, _) => "updated".into(),
            (State::Modified, true) => "replaced (was locally modified)".into(),
            (State::Modified, false) => "skipped: locally modified".into(),
            (State::Blocked(why), _) => format!("refused: {why}"),
        };
        line(p.target.tool, &what, &file(&p.target, p.skill), home)
    }).collect();
    let blocked: Vec<String> = plans.iter().filter_map(|p| match p.state() { State::Blocked(why) => Some(why.clone()), _ => None }).collect();
    if !blocked.is_empty() {
        return Report { lines, failed: Some(format!("Nothing was written: {}. Muster never writes through symlinks or over special files; move them away and run again.", blocked.join("; "))) };
    }
    let edited: Vec<String> = plans.iter().filter(|p| *p.state() == State::Modified && !force).map(|p| tilde(&file(&p.target, p.skill), home)).collect();
    if !edited.is_empty() {
        return Report { lines, failed: Some(format!("Nothing was written: you edited {} since Muster installed it. Keep a copy, then run again with --force to replace it.", edited.join(", "))) };
    }
    if dry_run {
        lines.push("Dry run: nothing was written.".into());
        return Report { lines, failed: None };
    }
    let mut results = Vec::new();
    let mut failures = 0;
    for (p, planned) in plans.iter().zip(&lines) {
        before_write(p);
        match apply(p) {
            Ok(()) => results.push(planned.clone()),
            Err(why) => { failures += 1; results.push(line(p.target.tool, &format!("failed: {why}"), &file(&p.target, p.skill), home)); }
        }
    }
    Report { lines: results, failed: (failures > 0).then(|| format!("{failures} skill file(s) failed; the others above were installed. Fix the cause and run again.")) }
}

fn apply(p: &Plan) -> Result<(), String> {
    let dir = p.target.root.join(p.skill.name);
    if let Some(tool_dir) = p.target.root.parent() {
        // The tool's own folder (~/.claude, $CODEX_HOME) is the user's to arrange, links included.
        std::fs::create_dir_all(tool_dir).map_err(|e| format!("creating {}: {e}", tool_dir.display()))?;
    }
    make_dir(&p.target.root)?;
    make_dir(&dir)?;
    // Revalidate: what is there now must be what was planned and shown.
    let now = observe(&p.target, p.skill);
    if now != p.seen && !(p.seen.state == State::Missing && now.state == State::Missing) {
        return Err("changed while installing; run again".into());
    }
    let hash = format!("{}\n", sha(p.skill.text.as_bytes()));
    if now.state != State::Current {
        // The skill first, then its marker: a failed skill write leaves the old file and its
        // old marker together, so it still reads as Muster's own (outdated), not as edited.
        write_file(&dir, "SKILL.md", p.skill.text.as_bytes(), now.state == State::Missing)?;
    }
    if now.marker.as_deref() != Some(hash.as_bytes()) {
        write_file(&dir, MARKER, hash.as_bytes(), false)?;
    }
    Ok(())
}

pub fn status(targets: &[Target], home: Option<&str>) -> Vec<String> {
    plan(targets).iter().map(|p| format!("{:<18} {:<8} {:<17} {}", p.skill.name, p.target.tool, p.state().word(),
        tilde(&file(&p.target, p.skill), home))).collect()
}

/// Shown after install and in docs/CLI.md.
pub const TRADE_OFF: &str = "A skill runs in your current agent, so the PR's text enters its context; for PRs from people you don't trust, start a separate agent from the PR reviewer template (New agent) to keep it isolated.";

pub fn run(cmd: SkillsCmd) -> anyhow::Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let home = env("HOME");
    match cmd {
        SkillsCmd::Install { claude, codex, dry_run, force } => {
            let report = install(&targets(claude, codex, env)?, dry_run, force, home.as_deref());
            for line in &report.lines { println!("{line}"); }
            if let Some(why) = report.failed { bail!("{why}") }
            if !dry_run { println!("Start a new agent session to pick up the skills.\n{TRADE_OFF}"); }
        }
        SkillsCmd::Status { claude, codex } => {
            for line in status(&targets(claude, codex, env)?, home.as_deref()) { println!("{line}"); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("muster-skills-{}-{name}", std::process::id()));
        if home.exists() { let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)); }
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn targets_in(home: &Path) -> Vec<Target> {
        targets(false, false, |k| (k == "HOME").then(|| home.display().to_string())).unwrap()
    }

    /// The YAML frontmatter as `key: value` pairs, parsed strictly enough for what skills use:
    /// plain scalars may not contain " #" (YAML starts a comment there), quoted ones round-trip.
    fn frontmatter(text: &str) -> Vec<(String, String)> {
        let fm = text.strip_prefix("---\n").and_then(|r| r.split_once("\n---\n")).map(|(f, _)| f).expect("frontmatter");
        fm.lines().map(|l| {
            let (k, v) = l.split_once(": ").unwrap_or_else(|| panic!("not key: value: {l}"));
            let v = v.trim();
            let value = if let Some(inner) = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                assert!(!inner.contains('"') || inner.contains("\\\""), "unescaped quote in {k}");
                inner.replace("\\\"", "\"")
            } else {
                assert!(!v.contains(" #") && !v.starts_with(['#', '&', '*', '!', '|', '>', '\'', '%', '@', '`']) && !v.contains(": "),
                    "{k} must be quoted: YAML would cut or misread it");
                v.to_string()
            };
            (k.to_string(), value)
        }).collect()
    }

    #[test]
    fn skills_have_frontmatter_and_the_session_check() {
        for s in SKILLS {
            let fm = frontmatter(s.text);
            let get = |k: &str| fm.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_else(|| panic!("{} lacks {k}", s.name));
            assert_eq!(get("name"), s.name);
            let description = get("description");
            assert!(description.len() < 1024 && description.ends_with('.'), "{}: {description}", s.name);
            assert!(s.text.contains("MUSTER_SESSION_ID") && s.text.contains("Only inside Muster"), "{}", s.name);
            assert!(!body(s.text).starts_with("---") && body(s.text).starts_with('#'));
        }
        let pr = frontmatter(get("muster-pr-review").text).into_iter().find(|(k, _)| k == "description").unwrap().1;
        for phrase in ["review PR 812", "look at this PR", "can I merge #812", "go through these PRs", "Muster session"] {
            assert!(pr.contains(phrase), "{phrase} survives YAML parsing: {pr}");
        }
        assert!(pr.contains("not when a PR link is only mentioned"), "{pr}");
        let panels_desc = frontmatter(get("muster-panels").text).into_iter().find(|(k, _)| k == "description").unwrap().1;
        assert!(!panels_desc.contains("\"show me\"") && panels_desc.contains("in a panel"), "{panels_desc}");
        let panels = get("muster-panels").text;
        for want in ["ui/initialize", "ui/message", "at most 3", "resourceDomains", "no `fetch`", "panel_open", "wait_for_panel_event"] {
            assert!(panels.contains(want), "{want}");
        }
        let flow = get("muster-pr-review").text;
        for want in ["no --repo, so they use explicit", "contents/PATH?ref=H", "MERGED or CLOSED", "three times"] {
            assert!(flow.contains(want), "{want}");
        }
    }

    #[test]
    fn targets_follow_the_documented_folders() {
        let home = PathBuf::from("/h");
        let t = targets(false, false, |k| (k == "HOME").then(|| "/h".to_string())).unwrap();
        assert_eq!(t.iter().map(|t| (t.tool, t.root.clone())).collect::<Vec<_>>(),
            vec![("claude", home.join(".claude/skills")), ("codex", home.join(".codex/skills"))]);
        let t = targets(false, true, |k| match k { "HOME" => Some("/h".into()), "CODEX_HOME" => Some("/c".into()), _ => None }).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].root, PathBuf::from("/c/skills"));
        let t = targets(true, false, |k| (k == "CLAUDE_CONFIG_DIR").then(|| "/cc".to_string())).unwrap();
        assert_eq!(t[0].root, PathBuf::from("/cc/skills"));
        assert!(targets(true, false, |_| None).is_err(), "no HOME, no guess");
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut n: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        n.sort();
        n
    }

    #[test]
    fn install_is_idempotent_and_never_overwrites_edits() {
        let home = temp_home("install");
        let ts = targets_in(&home);
        std::fs::create_dir_all(home.join(".claude/skills/mine")).unwrap();
        std::fs::write(home.join(".claude/skills/mine/SKILL.md"), "mine").unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "x = 1").unwrap();

        let dry = install(&ts, true, false, None);
        assert!(dry.failed.is_none() && dry.lines.iter().filter(|l| l.contains(" new ")).count() == 4 && dry.lines.last().unwrap().contains("Dry run"));
        assert!(!home.join(".claude/skills/muster-pr-review").exists(), "dry run writes nothing");

        assert!(install(&ts, false, false, None).failed.is_none());
        for t in &ts { for s in SKILLS {
            assert_eq!(state(t, s), State::Current);
            assert_eq!(std::fs::read_to_string(file(t, s)).unwrap(), s.text);
            assert_eq!(std::fs::metadata(file(t, s)).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(names_in(&t.root.join(s.name)), vec![MARKER.to_string(), "SKILL.md".to_string()], "no temporary files left");
        } }
        let again = install(&ts, false, false, None);
        assert!(again.failed.is_none() && again.lines.iter().all(|l| l.contains("unchanged")), "{:?}", again.lines);

        // Muster's own older copy is outdated and gets updated.
        let pr = get("muster-pr-review");
        let claude = &ts[0];
        let old = "---\nname: muster-pr-review\ndescription: old\n---\n# Old\n";
        std::fs::write(file(claude, pr), old).unwrap();
        std::fs::write(marker(claude, pr), format!("{}\n", sha(old.as_bytes()))).unwrap();
        assert_eq!(state(claude, pr), State::Outdated);
        assert!(install(&ts, false, false, None).lines.iter().any(|l| l.contains("updated")));
        assert_eq!(state(claude, pr), State::Current);

        // An edited file is refused, and nothing at all is written then.
        std::fs::write(file(claude, pr), format!("{}\nMy note.\n", pr.text)).unwrap();
        std::fs::remove_dir_all(home.join(".codex/skills/muster-panels")).unwrap();
        assert_eq!(state(claude, pr), State::Modified);
        let refused = install(&ts, false, false, None);
        assert!(refused.failed.as_deref().unwrap().contains("--force") && refused.lines.iter().any(|l| l.contains("skipped: locally modified")));
        assert!(std::fs::read_to_string(file(claude, pr)).unwrap().contains("My note."));
        assert!(!home.join(".codex/skills/muster-panels").exists(), "a refused install writes nothing");
        let st = status(&ts, None).join("\n");
        assert!(st.contains("locally modified") && st.contains("missing"), "{st}");
        assert!(install(&ts, false, true, None).failed.is_none());
        assert_eq!(state(claude, pr), State::Current);
        assert_eq!(std::fs::read_to_string(home.join(".claude/skills/mine/SKILL.md")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), "x = 1");
        // A file Muster never wrote (no marker) is the user's.
        std::fs::remove_file(marker(claude, pr)).unwrap();
        std::fs::write(file(claude, pr), "theirs").unwrap();
        assert_eq!(state(claude, pr), State::Modified);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn symlinks_and_special_files_are_refused_even_with_force() {
        let pr = get("muster-pr-review");
        // A symlinked skill folder, and a symlinked skills root: nothing lands in the link target.
        for linked in ["skills/muster-pr-review", "skills"] {
            let home = temp_home("dirlink");
            let outside = home.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let link = home.join(".claude").join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let ts = targets_in(&home);
            for force in [false, true] {
                let r = install(&ts, false, force, None);
                assert!(r.failed.as_deref().unwrap().contains("is a symlink"), "{linked}: {:?}", r.failed);
            }
            assert!(names_in(&outside).is_empty(), "{linked}: nothing written outside");
            assert!(!home.join(".codex/skills").exists(), "{linked}: nothing written anywhere");
            assert!(status(&ts, None).join("\n").contains("blocked"));
            std::fs::remove_dir_all(&home).unwrap();
        }
        // A dangling SKILL.md symlink is the user's: locally modified, never "new"; a marker
        // symlink is refused too.
        for entry in ["SKILL.md", MARKER] {
            let home = temp_home("filelink");
            let ts = targets_in(&home);
            let dir = ts[0].root.join(pr.name);
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(home.join("nowhere"), dir.join(entry)).unwrap();
            if entry == MARKER { std::fs::write(dir.join("SKILL.md"), "old").unwrap(); }
            let st = state(&ts[0], pr);
            assert!(matches!(&st, State::Blocked(why) if why.contains("symlink")), "{entry}: {st:?}");
            if entry == "SKILL.md" { assert!(st.word().contains("locally modified")); }
            assert!(install(&ts, false, true, None).failed.is_some());
            assert!(std::fs::symlink_metadata(dir.join(entry)).unwrap().file_type().is_symlink() && !home.join("nowhere").exists());
            std::fs::remove_dir_all(&home).unwrap();
        }
        // A FIFO or an oversized file: status and dry runs answer at once instead of hanging.
        let home = temp_home("special");
        let ts = targets_in(&home);
        let dir = ts[0].root.join(pr.name);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = std::ffi::CString::new(dir.join("SKILL.md").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(matches!(state(&ts[0], pr), State::Blocked(why) if why.contains("regular file")));
        assert!(install(&ts, true, false, None).failed.is_some());
        std::fs::remove_file(dir.join("SKILL.md")).unwrap();
        std::fs::write(dir.join("SKILL.md"), vec![b'x'; MAX_READ as usize + 1]).unwrap();
        assert!(matches!(state(&ts[0], pr), State::Blocked(why) if why.contains("256 KiB")));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_change_after_planning_is_never_overwritten() {
        let home = temp_home("race");
        let ts = targets_in(&home);
        let pr = get("muster-pr-review");
        let claude = &ts[0];
        std::fs::create_dir_all(claude.root.join(pr.name)).unwrap();
        let old = "old version";
        std::fs::write(file(claude, pr), old).unwrap();
        std::fs::write(marker(claude, pr), format!("{}\n", sha(old.as_bytes()))).unwrap();
        // The user saves an edit after the plan was made, before Muster reaches the file.
        let r = install_with(&ts, false, false, None, &|p| {
            if p.skill.name == pr.name && p.target.tool == "claude" { std::fs::write(file(&p.target, p.skill), "edited meanwhile").unwrap(); }
        });
        assert!(r.failed.is_some() && r.lines.iter().any(|l| l.contains("changed while installing")), "{:?}", r.lines);
        assert_eq!(std::fs::read_to_string(file(claude, pr)).unwrap(), "edited meanwhile");
        // A file that appears where none was planned is not replaced either.
        let home2 = temp_home("race2");
        let ts2 = targets_in(&home2);
        let r = install_with(&ts2, false, false, None, &|p| {
            if p.skill.name == pr.name && p.target.tool == "codex" {
                std::fs::create_dir_all(p.target.root.join(pr.name)).unwrap();
                std::fs::write(file(&p.target, p.skill), "mine").unwrap();
            }
        });
        assert!(r.failed.is_some());
        assert_eq!(std::fs::read_to_string(file(&ts2[1], pr)).unwrap(), "mine");
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&home2).unwrap();
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_muster_s_and_reports_every_target() {
        let home = temp_home("partial");
        let ts = targets_in(&home);
        let panels = get("muster-panels");
        let codex = &ts[1];
        let dir = codex.root.join(panels.name);
        std::fs::create_dir_all(&dir).unwrap();
        let old = "old panels";
        std::fs::write(dir.join("SKILL.md"), old).unwrap();
        std::fs::write(dir.join(MARKER), format!("{}\n", sha(old.as_bytes()))).unwrap();
        // The codex panels folder cannot take new files: its update fails, the rest installs.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let r = install(&ts, false, false, None);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.failed.as_deref().unwrap().contains("1 skill file(s) failed"), "{:?}", r.failed);
        assert_eq!(r.lines.len(), 4, "{:?}", r.lines);
        assert_eq!(r.lines.iter().filter(|l| l.contains("failed:")).count(), 1);
        assert!(r.lines.iter().filter(|l| l.contains(" new ")).count() == 3, "{:?}", r.lines);
        for s in SKILLS { assert_eq!(state(&ts[0], s), State::Current); }
        // The old file and its marker still agree: outdated, not "locally modified".
        assert_eq!(state(codex, panels), State::Outdated);
        assert_eq!(names_in(&dir), vec![MARKER.to_string(), "SKILL.md".to_string()]);
        assert!(install(&ts, false, false, None).failed.is_none());
        assert_eq!(state(codex, panels), State::Current);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
