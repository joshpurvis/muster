//! Agent skills Muster ships (skills/<name>/SKILL.md) and `muster skills install|status`, which
//! copies them into Claude Code's and Codex's user skill folders: the default Claude folder and
//! every Claude account profile's folder (sessions under a profile read skills from there). A
//! marker next to each
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
        /// Claude Code: $CLAUDE_CONFIG_DIR/skills (else ~/.claude/skills) and each Claude profile's skills folder.
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
    /// What output calls this target: the tool, plus the Claude profiles that use the folder.
    pub label: String,
    /// The tool's configuration folder (~/.claude, a profile's folder, ~/.codex), resolved once
    /// at planning: canonical when it existed then, so a symlink changed later cannot redirect.
    pub config: PathBuf,
    /// The tool's user skills folder: `config/skills`.
    pub root: PathBuf,
    /// `config`'s identity (device, inode) at planning; None when it did not exist yet.
    /// Checked again before anything is created under it and before every write.
    pub pin: Option<(u64, u64)>,
    /// Whether Muster may create `config` (the default ~/.claude or ~/.codex). A profile's
    /// folder is the user's own setup and must already exist.
    pub create_parent: bool,
}

impl Target {
    /// A target for `config`, resolved and pinned now.
    pub fn new(tool: &'static str, label: String, config: PathBuf, create_parent: bool) -> Self {
        let (config, pin) = match std::fs::canonicalize(&config) {
            Ok(real) => match identity(&real) { Some(id) if real.is_dir() => (real, Some(id)), _ => (config, None) },
            Err(_) => (config, None),
        };
        Target { tool, label, root: config.join("skills"), config, pin, create_parent }
    }
}

/// A directory's (device, inode), following symlinks.
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Text shown on a terminal (profile names, paths, reasons) with control characters escaped,
/// so a stored value cannot forge or hide output lines.
pub fn shown(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { c.escape_default().to_string() } else { c.to_string() }).collect()
}

/// A Claude account profile as Muster stores it (`muster profile ls`).
#[derive(Debug, Clone)]
pub struct ProfileDir { pub name: String, pub config_dir: PathBuf, pub enabled: bool }

/// Reads the configured Claude profiles straight from Muster's database, read-only, without
/// the daemon. No database (Muster never ran) or no profiles table yet means no profiles.
/// A normal locking connection: a write by a running daemon is either seen whole or waited
/// for. SQLite may create the -wal/-shm files beside it; it gives them the database's own
/// mode (0600 for Muster's), which the tests check.
pub fn read_profiles(db: &Path) -> anyhow::Result<Vec<ProfileDir>> {
    use rusqlite::OpenFlags;
    if !db.try_exists().unwrap_or(true) { return Ok(Vec::new()) }
    let conn = rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", shown(&db.display().to_string())))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let has_table: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='account_profiles'", [], |r| r.get(0))?;
    if has_table == 0 { return Ok(Vec::new()) }
    let mut st = conn.prepare("SELECT name, config_dir, enabled FROM account_profiles ORDER BY name COLLATE NOCASE, id")?;
    let rows = st.query_map([], |r| Ok(ProfileDir {
        name: r.get(0)?, config_dir: PathBuf::from(r.get::<_, String>(1)?), enabled: r.get(2)?,
    }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Entries whose presence (never contents) shows a folder is a Claude Code configuration
/// folder: a fresh one that was signed in has `.claude.json` or `plugins/`, a used one
/// `settings.json` or `projects/`. Only `symlink_metadata` is taken; nothing is opened.
const CLAUDE_FOLDER_SIGNS: &[&str] = &["settings.json", "projects", "plugins", ".claude.json"];

/// A stored profile folder Muster may write skills into: an absolute path without control
/// characters that resolves to an existing directory strictly inside the (resolved) home
/// and looks like a Claude configuration folder. Returns the resolved folder, or why not.
fn profile_folder(dir: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let not_claude = || "skipped (not a Claude config folder under your home)".to_string();
    let text = dir.to_str().ok_or_else(not_claude)?;
    if text.chars().any(char::is_control) || !dir.is_absolute() { return Err(not_claude()) }
    let real = match std::fs::canonicalize(dir) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("skipped: its folder is unavailable".into()),
        Err(_) => return Err(not_claude()),
    };
    let home = home.and_then(|h| std::fs::canonicalize(h).ok()).ok_or_else(not_claude)?;
    if !real.is_dir() || real == home || !real.starts_with(&home)
        || !CLAUDE_FOLDER_SIGNS.iter().any(|sign| std::fs::symlink_metadata(real.join(sign)).is_ok()) {
        return Err(not_claude());
    }
    Ok(real)
}

/// The targets, and notes on profiles that were left out.
pub struct Targets { pub list: Vec<Target>, pub notes: Vec<String> }

/// Targets from the environment and the Claude profiles: `env` reads a variable (HOME,
/// CLAUDE_CONFIG_DIR, CODEX_HOME). Claude gets the default folder plus every valid profile
/// folder ([`profile_folder`]), one target per real folder (a profile that points at the
/// default folder shares its target).
pub fn targets(claude: bool, codex: bool, env: impl Fn(&str) -> Option<String>, profiles: &[ProfileDir]) -> anyhow::Result<Targets> {
    let (claude, codex) = if claude || codex { (claude, codex) } else { (true, true) };
    let home = || env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).context("HOME is not set");
    let dir = |var: &str, default: &str| -> anyhow::Result<PathBuf> {
        Ok(match env(var).filter(|v| !v.is_empty()) { Some(v) => PathBuf::from(v), None => home()?.join(default) })
    };
    let mut out = Vec::new();
    let mut notes = Vec::new();
    if claude {
        let mut targets = vec![(Target::new("claude", String::new(), dir("CLAUDE_CONFIG_DIR", ".claude")?, true), vec!["default".to_string()])];
        let home = home().ok();
        for p in profiles {
            let name = if p.enabled { shown(&p.name) } else { format!("{}: disabled", shown(&p.name)) };
            if p.name.chars().any(char::is_control) {
                notes.push(format!("claude ({name}) skipped: its name has control characters"));
                continue;
            }
            match profile_folder(&p.config_dir, home.as_deref()) {
                Ok(real) => match targets.iter_mut().find(|(t, _)| t.pin.is_some() && t.config == real) {
                    Some((_, names)) => names.push(name),
                    None => targets.push((Target::new("claude", String::new(), real, false), vec![name])),
                },
                Err(why) => notes.push(format!("claude ({name}) {why}: {}", shown(&p.config_dir.display().to_string()))),
            }
        }
        for (mut t, names) in targets {
            t.label = if names == ["default"] { "claude".to_string() } else { format!("claude ({})", names.join(", ")) };
            out.push(t);
        }
    }
    if codex { out.push(Target::new("codex", "codex".into(), dir("CODEX_HOME", ".codex")?, true)); }
    Ok(Targets { list: out, notes })
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

fn cpath(path: &Path) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{} contains a NUL byte", path.display()))
}

fn fd_identity(fd: &std::os::fd::OwnedFd) -> Result<(u64, u64), String> {
    use std::os::fd::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid open descriptor and a properly sized stat buffer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 { return Err(std::io::Error::last_os_error().to_string()) }
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Opens directory `name` inside `parent` (or `path` itself when `parent` is None) without
/// following a symlink at the last step; with `create`, makes it (0700) first if missing.
fn open_dir(parent: Option<&std::os::fd::OwnedFd>, name: &Path, create: bool) -> Result<std::os::fd::OwnedFd, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let c = cpath(name)?;
    let at = parent.map_or(libc::AT_FDCWD, |p| p.as_raw_fd());
    if create {
        // SAFETY: a valid directory descriptor (or AT_FDCWD) and a NUL-terminated name.
        if unsafe { libc::mkdirat(at, c.as_ptr(), 0o700) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::AlreadyExists { return Err(format!("creating {}: {e}", name.display())) }
        }
    }
    // SAFETY: as above; the returned descriptor is owned below.
    let fd = unsafe { libc::openat(at, c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ELOOP) => format!("{} is a symlink", name.display()),
            Some(libc::ENOTDIR) => format!("{} is not a directory", name.display()),
            Some(libc::ENOENT) => format!("{} is unavailable", name.display()),
            _ => format!("{}: {e}", name.display()),
        });
    }
    // SAFETY: fd is a fresh descriptor nothing else owns.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Writes `bytes` as `name` inside the open directory `dir`: an exclusively created,
/// unpredictably named 0600 temporary file (never opened through a symlink), then a rename
/// within that same directory. With `no_replace`, an entry that appeared meanwhile is left
/// alone and the write fails. `shown_dir` is only for messages.
fn write_at(dir: &std::os::fd::OwnedFd, shown_dir: &Path, name: &str, bytes: &[u8], no_replace: bool) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    let target = shown_dir.join(name);
    let (tmp, mut f) = (0..8).find_map(|_| {
        let tmp = cpath(Path::new(&format!(".{name}.muster-{:016x}.tmp", rand::random::<u64>()))).ok()?;
        // SAFETY: a valid directory descriptor and a NUL-terminated name.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600 as libc::c_uint) };
        if fd >= 0 {
            // SAFETY: a fresh descriptor nothing else owns.
            return Some(Ok((tmp, std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }))));
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::AlreadyExists { None } else { Some(Err(format!("writing in {}: {e}", shown_dir.display()))) }
    }).unwrap_or_else(|| Err(format!("no free temporary name in {}", shown_dir.display())))?;
    let to = cpath(Path::new(name))?;
    let result = f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| e.to_string()).and_then(|()| {
        #[cfg(target_os = "linux")]
        // SAFETY: valid descriptors and NUL-terminated names for the duration of the call.
        let r = unsafe { libc::renameat2(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), to.as_ptr(), if no_replace { libc::RENAME_NOREPLACE } else { 0 }) };
        #[cfg(not(target_os = "linux"))]
        let r = { let _ = no_replace; unsafe { libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } };
        if r == 0 { return Ok(()) }
        let e = std::io::Error::last_os_error();
        Err(if e.kind() == std::io::ErrorKind::AlreadyExists { "changed while installing (it appeared)".into() } else { e.to_string() })
    });
    // SAFETY: as above; removing our own temporary name.
    if result.is_err() { unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0); } }
    result.map_err(|e| format!("writing {}: {e}", target.display()))
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

/// The label column's width: the longest label, at least `min`.
fn width(targets: &[Target], min: usize) -> usize {
    targets.iter().map(|t| t.label.chars().count()).max().unwrap_or(0).max(min)
}

fn line(label: &str, w: usize, what: &str, path: &Path, home: Option<&str>) -> String {
    shown(&format!("{label:<w$} {what:<26} {}", tilde(path, home)))
}

/// Installs every skill into every target. Nothing is written while any entry is blocked, or
/// locally modified without `force`. Each file is checked again right before it is written
/// (a change since planning stops that file); failures are reported per file.
pub fn install(targets: &[Target], dry_run: bool, force: bool, home: Option<&str>) -> Report {
    install_with(targets, dry_run, force, home, &|_| {})
}

fn install_with(targets: &[Target], dry_run: bool, force: bool, home: Option<&str>, before_write: &dyn Fn(&Plan)) -> Report {
    let plans = plan(targets);
    let w = width(targets, 10);
    let mut lines: Vec<String> = plans.iter().map(|p| {
        let what = match (p.state(), force) {
            (State::Missing, _) => "new".to_string(),
            (State::Current, _) => "unchanged".into(),
            (State::Outdated, _) => "updated".into(),
            (State::Modified, true) => "replaced (was locally modified)".into(),
            (State::Modified, false) => "skipped: locally modified".into(),
            (State::Blocked(why), _) => format!("refused: {why}"),
        };
        line(&p.target.label, w, &what, &file(&p.target, p.skill), home)
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
            Err(why) => { failures += 1; results.push(line(&p.target.label, w, &format!("failed: {why}"), &file(&p.target, p.skill), home)); }
        }
    }
    Report { lines: results, failed: (failures > 0).then(|| format!("{failures} skill file(s) failed; the others above were installed. Fix the cause and run again.")) }
}

fn apply(p: &Plan) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let t = &p.target;
    let changed = || format!("{} changed while installing (moved or replaced); nothing more was written there; run again", t.config.display());
    // The tool's own folder (~/.claude, $CODEX_HOME, a profile's folder) is the user's to
    // arrange, links included; it was resolved at planning. Only a default folder that did
    // not exist then is created now, and pinned as created.
    let pin = match t.pin {
        Some(pin) => pin,
        None if t.create_parent => {
            std::fs::create_dir_all(&t.config).map_err(|e| format!("creating {}: {e}", t.config.display()))?;
            identity(&t.config).ok_or_else(changed)?
        }
        None => return Err(format!("profile folder {} is unavailable", t.config.display())),
    };
    // Everything below happens through descriptors of the pinned folder, never by path.
    let config = open_dir(None, &t.config, false).or_else(|_| {
        // A default folder that is itself a symlink (allowed): open what it resolves to.
        std::fs::canonicalize(&t.config).map_err(|e| format!("{}: {e}", t.config.display())).and_then(|real| open_dir(None, &real, false))
    })?;
    if fd_identity(&config)? != pin { return Err(changed()) }
    let skills = open_dir(Some(&config), Path::new("skills"), true).map_err(|e| e.replace("skills", &t.root.display().to_string()))?;
    let dir_path = t.root.join(p.skill.name);
    let dir = open_dir(Some(&skills), Path::new(p.skill.name), true).map_err(|e| e.replace(p.skill.name, &dir_path.display().to_string()))?;
    // The paths `observe` reads must still be these very folders.
    let (skills_id, dir_id) = (fd_identity(&skills)?, fd_identity(&dir)?);
    let same = |path: &Path, id: (u64, u64), follow: bool| {
        let m = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
        m.is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == id)
    };
    let check = || if same(&t.config, pin, true) && same(&t.root, skills_id, false) && same(&dir_path, dir_id, false) { Ok(()) } else { Err(changed()) };
    check()?;
    // Revalidate: what is there now must be what was planned and shown.
    let now = observe(t, p.skill);
    if now != p.seen && !(p.seen.state == State::Missing && now.state == State::Missing) {
        return Err("changed while installing; run again".into());
    }
    let hash = format!("{}\n", sha(p.skill.text.as_bytes()));
    if now.state != State::Current {
        check()?;
        // The skill first, then its marker: a failed skill write leaves the old file and its
        // old marker together, so it still reads as Muster's own (outdated), not as edited.
        write_at(&dir, &dir_path, "SKILL.md", p.skill.text.as_bytes(), now.state == State::Missing)?;
    }
    if now.marker.as_deref() != Some(hash.as_bytes()) {
        check()?;
        write_at(&dir, &dir_path, MARKER, hash.as_bytes(), false)?;
    }
    Ok(())
}

pub fn status(targets: &[Target], home: Option<&str>) -> Vec<String> {
    let w = width(targets, 8);
    plan(targets).iter().map(|p| shown(&format!("{:<18} {:<w$} {:<17} {}", p.skill.name, p.target.label, p.state().word(),
        tilde(&file(&p.target, p.skill), home)))).collect()
}

/// Shown after install and in docs/CLI.md.
pub const TRADE_OFF: &str = "A skill runs in your current agent, so the PR's text enters its context; for PRs from people you don't trust, start a separate agent from the PR reviewer template (New agent) to keep it isolated.";

pub fn run(cmd: SkillsCmd) -> anyhow::Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let home = env("HOME");
    let (claude, codex) = match &cmd { SkillsCmd::Install { claude, codex, .. } | SkillsCmd::Status { claude, codex } => (*claude, *codex) };
    let profiles = if claude || !codex {
        let db = crate::paths::db_path();
        read_profiles(&db).unwrap_or_else(|e| {
            eprintln!("{}", shown(&format!("warning: could not read Muster's Claude profiles from {}: {e:#}; only the default Claude folder is covered", db.display())));
            Vec::new()
        })
    } else { Vec::new() };
    let ts = targets(claude, codex, env, &profiles)?;
    match cmd {
        SkillsCmd::Install { dry_run, force, .. } => {
            let report = install(&ts.list, dry_run, force, home.as_deref());
            for line in report.lines.iter().chain(&ts.notes) { println!("{line}"); }
            if let Some(why) = report.failed { bail!("{}", shown(&why)) }
            if !dry_run { println!("Start a new agent session to pick up the skills.\n{TRADE_OFF}"); }
        }
        SkillsCmd::Status { .. } => {
            for line in status(&ts.list, home.as_deref()).iter().chain(&ts.notes) { println!("{line}"); }
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
        targets(false, false, |k| (k == "HOME").then(|| home.display().to_string()), &[]).unwrap().list
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
        for phrase in ["review PR 812", "look at this PR", "can I merge #812", "go through these PRs", "help me review the open PRs",
            "go through the open PRs", "pr_overview", "Muster session"] {
            assert!(pr.contains(phrase), "{phrase} survives YAML parsing: {pr}");
        }
        assert!(pr.contains("not when a PR link is only mentioned"), "{pr}");
        let panels_desc = frontmatter(get("muster-panels").text).into_iter().find(|(k, _)| k == "description").unwrap().1;
        assert!(!panels_desc.contains("\"show me\"") && panels_desc.contains("in a panel"), "{panels_desc}");
        let panels = get("muster-panels").text;
        for want in ["ui/initialize", "ui/message", "at most 3", "resourceDomains", "no `fetch`", "panel_open", "wait_for_panel_event", "`web_preview`", "system browser", "`note`", "`navigated`",
            "`board`", "`kit: true`", "data-send", "data-confirm", "Do not call\n`ui/initialize` yourself"] {
            assert!(panels.contains(want), "{want}");
        }
        let flow = get("muster-pr-review").text;
        for want in ["no --repo, so they use explicit", "contents/PATH?ref=H", "MERGED or CLOSED", "three times",
            "gh pr list --repo R --state open", "app `pr_overview`", "`next` `{repo, number, order}`", "`outward` is required",
            "into a command, not even quoted", "mktemp -d ./.muster-pr-XXXXXXXX", "-- \"$u\"",
            "--limit 201", "with no `panel_id`", "it never repeats `number`", "--body-file <that file>",
            "A custom action never merges", "an overview or custom event"] {
            assert!(flow.contains(want), "{want}");
        }
    }

    #[test]
    fn targets_follow_the_documented_folders() {
        let home = PathBuf::from("/h");
        let t = targets(false, false, |k| (k == "HOME").then(|| "/h".to_string()), &[]).unwrap().list;
        assert_eq!(t.iter().map(|t| (t.tool, t.root.clone())).collect::<Vec<_>>(),
            vec![("claude", home.join(".claude/skills")), ("codex", home.join(".codex/skills"))]);
        let t = targets(false, true, |k| match k { "HOME" => Some("/h".into()), "CODEX_HOME" => Some("/c".into()), _ => None }, &[]).unwrap().list;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].root, PathBuf::from("/c/skills"));
        let t = targets(true, false, |k| (k == "CLAUDE_CONFIG_DIR").then(|| "/cc".to_string()), &[]).unwrap().list;
        assert_eq!(t[0].root, PathBuf::from("/cc/skills"));
        assert!(targets(true, false, |_| None, &[]).is_err(), "no HOME, no guess");
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

    /// A home with Muster's database holding Personal (a symlinked folder) and Work (plain).
    fn profile_home(name: &str) -> (PathBuf, PathBuf) {
        let home = temp_home(name);
        let db = home.join("data/muster.db");
        let store = crate::store::Store::open(&db).unwrap();
        std::fs::create_dir_all(home.join("real-personal/plugins")).unwrap();
        std::fs::write(home.join("real-personal/.claude.json"), "{}").unwrap();
        std::os::unix::fs::symlink(home.join("real-personal"), home.join(".claude-personal")).unwrap();
        std::fs::create_dir_all(home.join(".claude-work")).unwrap();
        std::fs::write(home.join(".claude-work/settings.json"), "{}").unwrap();
        store.insert_account_profile("Personal", home.join(".claude-personal").to_str().unwrap()).unwrap();
        store.insert_account_profile("Work", home.join(".claude-work").to_str().unwrap()).unwrap();
        (home, db)
    }

    #[test]
    fn every_claude_profile_folder_is_a_target_once() {
        let (home, db) = profile_home("profiles");
        let profiles = read_profiles(&db).unwrap();
        assert_eq!(profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Personal", "Work"]);
        assert!(read_profiles(&home.join("no-such.db")).unwrap().is_empty(), "Muster never ran: no profiles");

        let env = |k: &str| (k == "HOME").then(|| home.display().to_string());
        let ts = targets(false, false, env, &profiles).unwrap();
        assert!(ts.notes.is_empty(), "{:?}", ts.notes);
        let labels: Vec<_> = ts.list.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["claude", "claude (Personal)", "claude (Work)", "codex"]);
        assert!(install(&ts.list, false, false, None).failed.is_none());
        for dir in [".claude", "real-personal", ".claude-work", ".codex"] {
            for s in SKILLS {
                assert_eq!(std::fs::read_to_string(home.join(dir).join("skills").join(s.name).join("SKILL.md")).unwrap(), s.text, "{dir}");
            }
        }
        let st = status(&ts.list, Some(home.to_str().unwrap()));
        assert_eq!(st.len(), 8, "{st:?}");
        assert!(st.iter().all(|l| l.contains("current")), "{st:?}");
        // The symlinked profile is shown (and written) where it resolved at planning.
        for want in ["claude (Personal) current           ~/real-personal/skills/muster-pr-review/SKILL.md",
            "claude (Work)     current           ~/.claude-work/skills/muster-panels/SKILL.md"] {
            assert!(st.iter().any(|l| l.contains(want)), "{want} in {st:?}");
        }
        // --codex alone leaves the Claude folders out, profiles included.
        assert_eq!(targets(false, true, env, &profiles).unwrap().list.len(), 1);

        // A profile pointing at the default folder (here through CLAUDE_CONFIG_DIR) shares its
        // target; a missing profile folder is noted, never created; a disabled one is marked.
        let mut more = profiles.clone();
        more.push(ProfileDir { name: "Alias".into(), config_dir: home.join(".claude"), enabled: true });
        more.push(ProfileDir { name: "Gone".into(), config_dir: home.join(".claude-gone"), enabled: true });
        more[1].enabled = false;
        std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
        let env2 = |k: &str| match k { "HOME" => Some(home.display().to_string()), "CLAUDE_CONFIG_DIR" => Some(home.join("./.claude").display().to_string()), _ => None };
        let ts = targets(true, false, env2, &more).unwrap();
        let labels: Vec<_> = ts.list.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["claude (default, Alias)", "claude (Personal)", "claude (Work: disabled)"]);
        assert_eq!(ts.notes.len(), 1);
        assert!(ts.notes[0].contains("claude (Gone) skipped") && ts.notes[0].contains("unavailable"), "{:?}", ts.notes);
        assert!(install(&ts.list, false, false, None).lines.iter().all(|l| l.contains("unchanged")));
        assert!(!home.join(".claude-gone").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn only_claude_folders_under_home_are_profile_targets() {
        let home = temp_home("profile-rules");
        let outside = temp_home("profile-rules-outside");
        std::fs::write(outside.join("settings.json"), "{}").unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap(); // home itself looks like one
        std::fs::create_dir_all(home.join("plain")).unwrap(); // no Claude signs
        std::fs::create_dir_all(home.join("ok/projects")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("link-out")).unwrap();
        std::os::unix::fs::symlink(home.join("ok"), home.join("link-in")).unwrap();
        let p = |name: &str, dir: PathBuf| ProfileDir { name: name.into(), config_dir: dir, enabled: true };
        let bad = [p("Dot", ".".into()), p("Up", "..".into()), p("Rel", "ok".into()), p("Root", "/".into()), p("Etc", "/etc".into()),
            p("Outside", outside.clone()), p("Home", home.clone()), p("Plain", home.join("plain")), p("LinkOut", home.join("link-out")),
            p("Ctl", PathBuf::from(format!("{}/ok\n", home.display()))), p("Up2", home.join("ok/../plain"))];
        let env = |k: &str| (k == "HOME").then(|| home.display().to_string());
        let ts = targets(true, false, env, &bad).unwrap();
        assert_eq!(ts.list.len(), 1, "only the default: {:?}", ts.list.iter().map(|t| &t.label).collect::<Vec<_>>());
        assert_eq!(ts.notes.len(), bad.len(), "{:?}", ts.notes);
        assert!(ts.notes.iter().all(|n| n.contains("skipped (not a Claude config folder under your home)")), "{:?}", ts.notes);
        assert!(ts.notes.iter().all(|n| !n.chars().any(char::is_control)), "{:?}", ts.notes);
        assert!(ts.notes.iter().any(|n| n.ends_with("/ok\\n")), "{:?}", ts.notes);
        assert!(install(&ts.list, false, false, None).failed.is_none());
        assert!(!outside.join("skills").exists() && !home.join("plain/skills").exists() && !home.join("skills").exists());
        // A symlinked profile folder inside home works, through its resolved destination.
        let ts = targets(true, false, env, &[p("In", home.join("link-in"))]).unwrap();
        assert_eq!(ts.list[1].config, std::fs::canonicalize(home.join("ok")).unwrap());
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn a_folder_replaced_after_planning_is_never_written() {
        let (home, db) = profile_home("profile-race");
        let profiles = read_profiles(&db).unwrap();
        let env = |k: &str| (k == "HOME").then(|| home.display().to_string());
        let ts = targets(true, false, env, &profiles).unwrap().list;
        // Retargeting the profile's symlink after planning does not redirect the writes.
        std::fs::create_dir_all(home.join("elsewhere")).unwrap();
        std::fs::remove_file(home.join(".claude-personal")).unwrap();
        std::os::unix::fs::symlink(home.join("elsewhere"), home.join(".claude-personal")).unwrap();
        // Work's folder itself is swapped for another directory before Muster writes there.
        let swapped = std::cell::Cell::new(false);
        let r = install_with(&ts, false, false, None, &|p| {
            if p.target.label == "claude (Work)" && !swapped.replace(true) {
                std::fs::rename(home.join(".claude-work"), home.join("work-moved")).unwrap();
                std::fs::create_dir_all(home.join(".claude-work")).unwrap();
            }
        });
        assert!(r.failed.is_some());
        let work: Vec<_> = r.lines.iter().filter(|l| l.contains("claude (Work)")).collect();
        assert!(work.len() == 2 && work.iter().all(|l| l.contains("changed while installing")), "{:?}", r.lines);
        assert!(names_in(&home.join(".claude-work")).is_empty(), "nothing written in the replacement");
        assert!(names_in(&home.join("elsewhere")).is_empty(), "nothing written through the retargeted link");
        for s in SKILLS { assert!(home.join("real-personal/skills").join(s.name).join("SKILL.md").exists()); }
        // A planned profile folder that disappeared is not recreated.
        let work = ts.iter().find(|t| t.label == "claude (Work)").unwrap().clone();
        std::fs::remove_dir_all(home.join(".claude-work")).unwrap();
        std::fs::remove_dir_all(home.join("work-moved")).unwrap();
        let r = install(&[work], false, false, None);
        assert!(r.lines.iter().all(|l| l.contains("failed:")), "{:?}", r.lines);
        assert!(!home.join(".claude-work").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn profiles_are_read_with_locking_and_private_sidecars() {
        use std::os::unix::fs::PermissionsExt;
        let (home, db) = profile_home("profile-db");
        // The daemon still has the database open and commits another profile: it is seen.
        let store = crate::store::Store::open(&db).unwrap();
        std::fs::create_dir_all(home.join(".claude-new/projects")).unwrap();
        store.insert_account_profile("New", home.join(".claude-new").to_str().unwrap()).unwrap();
        assert_eq!(read_profiles(&db).unwrap().len(), 3);
        drop(store);
        // With the daemon gone (log checkpointed away), a read leaves only private sidecars.
        for suffix in ["-wal", "-shm"] { let _ = std::fs::remove_file(format!("{}{suffix}", db.display())); }
        assert_eq!(read_profiles(&db).unwrap().len(), 3);
        for suffix in ["", "-wal", "-shm"] {
            if let Ok(m) = std::fs::metadata(format!("{}{suffix}", db.display())) {
                assert_eq!(m.permissions().mode() & 0o777, 0o600, "{suffix}");
            }
        }
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn stored_names_and_paths_cannot_forge_output() {
        let home = temp_home("profile-escape");
        std::fs::create_dir_all(home.join("ok/projects")).unwrap();
        let profiles = [ProfileDir { name: "Evil\n\x1b[2Kclaude".into(), config_dir: home.join("ok"), enabled: true },
            ProfileDir { name: "Gone".into(), config_dir: home.join("gone\r\x1b]0;x\x07"), enabled: true }];
        let env = |k: &str| (k == "HOME").then(|| home.display().to_string());
        let ts = targets(true, false, env, &profiles).unwrap();
        assert_eq!(ts.list.len(), 1);
        assert_eq!(ts.notes.len(), 2, "{:?}", ts.notes);
        assert!(ts.notes[0].contains("Evil\\n\\u{1b}[2Kclaude") && ts.notes[0].contains("control characters"), "{:?}", ts.notes);
        assert!(ts.notes.iter().all(|n| !n.chars().any(char::is_control)), "{:?}", ts.notes);
        // Paths and reasons in install and status lines are escaped too.
        let odd = home.join("odd\nname");
        let t = Target::new("claude", "claude".into(), odd, true);
        let lines = install(std::slice::from_ref(&t), false, false, None).lines;
        let lines: Vec<_> = lines.into_iter().chain(status(std::slice::from_ref(&t), None)).collect();
        assert!(lines.iter().all(|l| !l.contains('\n') && l.contains("odd\\nname")), "{lines:?}");
        assert_eq!(shown("a\tb\u{9b}c"), "a\\tb\\u{9b}c");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
