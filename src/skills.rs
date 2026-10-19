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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Missing,
    Current,
    /// Muster's earlier version, untouched since.
    Outdated,
    /// Edited by someone else (or not written by Muster).
    Modified,
}

impl State {
    fn word(self) -> &'static str {
        match self { State::Missing => "missing", State::Current => "current", State::Outdated => "outdated", State::Modified => "locally modified" }
    }
}

fn sha(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

pub fn file(target: &Target, skill: &Skill) -> PathBuf { target.root.join(skill.name).join("SKILL.md") }

pub fn state(target: &Target, skill: &Skill) -> anyhow::Result<State> {
    let path = file(target, skill);
    let current = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if current == skill.text.as_bytes() { return Ok(State::Current) }
    let marker = std::fs::read_to_string(path.with_file_name(MARKER)).unwrap_or_default();
    Ok(if marker.trim() == sha(&current) { State::Outdated } else { State::Modified })
}

/// Write `bytes` to `path` through a temporary file in the same folder and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().context("no parent folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.muster-{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result.with_context(|| format!("writing {}", path.display()))
}

/// One planned change: what `install` would do for a skill in a target.
pub struct Plan { pub target: Target, pub skill: &'static Skill, pub state: State }

pub fn plan(targets: &[Target]) -> anyhow::Result<Vec<Plan>> {
    let mut out = Vec::new();
    for t in targets {
        for s in SKILLS { out.push(Plan { target: t.clone(), skill: s, state: state(t, s)? }); }
    }
    Ok(out)
}

fn tilde(path: &Path, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()).and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Installs every skill into every target. With a locally modified file and no `force`,
/// nothing is written at all. Returns the report lines.
pub fn install(targets: &[Target], dry_run: bool, force: bool, home: Option<&str>) -> anyhow::Result<Vec<String>> {
    let plans = plan(targets)?;
    let mut lines = Vec::new();
    for p in &plans {
        let what = match (p.state, force) {
            (State::Missing, _) => "new",
            (State::Current, _) => "unchanged",
            (State::Outdated, _) => "updated",
            (State::Modified, true) => "replaced (was locally modified)",
            (State::Modified, false) => "skipped: locally modified",
        };
        lines.push(format!("{:<10} {what:<26} {}", p.target.tool, tilde(&file(&p.target, p.skill), home)));
    }
    let refused: Vec<String> = plans.iter().filter(|p| p.state == State::Modified && !force)
        .map(|p| tilde(&file(&p.target, p.skill), home)).collect();
    if !refused.is_empty() {
        bail!("{}\nNothing was written: you edited {} since Muster installed it. Keep a copy, then run again with --force to replace it.",
            lines.join("\n"), refused.join(", "));
    }
    if dry_run {
        lines.push("Dry run: nothing was written.".into());
        return Ok(lines);
    }
    for p in &plans {
        if p.state == State::Current {
            // Repair a lost marker so a later update is recognised as Muster's own file.
            let marker = file(&p.target, p.skill).with_file_name(MARKER);
            if std::fs::read_to_string(&marker).map(|m| m.trim() != sha(p.skill.text.as_bytes())).unwrap_or(true) {
                write_atomic(&marker, format!("{}\n", sha(p.skill.text.as_bytes())).as_bytes())?;
            }
            continue;
        }
        let path = file(&p.target, p.skill);
        // The marker first: if the skill write then fails, the old file still reads as edited
        // (refusing an update) rather than as Muster's.
        write_atomic(&path.with_file_name(MARKER), format!("{}\n", sha(p.skill.text.as_bytes())).as_bytes())?;
        write_atomic(&path, p.skill.text.as_bytes())?;
    }
    Ok(lines)
}

pub fn status(targets: &[Target], home: Option<&str>) -> anyhow::Result<Vec<String>> {
    Ok(plan(targets)?.iter().map(|p| format!("{:<18} {:<8} {:<17} {}", p.skill.name, p.target.tool, p.state.word(),
        tilde(&file(&p.target, p.skill), home))).collect())
}

pub fn run(cmd: SkillsCmd) -> anyhow::Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let home = env("HOME");
    match cmd {
        SkillsCmd::Install { claude, codex, dry_run, force } => {
            for line in install(&targets(claude, codex, env)?, dry_run, force, home.as_deref())? { println!("{line}"); }
            if !dry_run { println!("Start a new agent session to pick up the skills."); }
        }
        SkillsCmd::Status { claude, codex } => {
            for line in status(&targets(claude, codex, env)?, home.as_deref())? { println!("{line}"); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("muster-skills-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn env_for(home: &Path) -> impl Fn(&str) -> Option<String> + '_ {
        move |k| (k == "HOME").then(|| home.display().to_string())
    }

    #[test]
    fn skills_have_frontmatter_and_the_session_check() {
        for s in SKILLS {
            let fm = s.text.strip_prefix("---\n").and_then(|r| r.split_once("\n---\n")).map(|(f, _)| f).expect("frontmatter");
            assert!(fm.contains(&format!("name: {}", s.name)) && fm.contains("description: "), "{}", s.name);
            assert!(fm.lines().all(|l| l.len() < 1024), "{}", s.name);
            assert!(s.text.contains("MUSTER_SESSION_ID") && s.text.contains("Only inside Muster"), "{}", s.name);
            assert!(!body(s.text).starts_with("---") && body(s.text).starts_with('#'));
        }
        let pr = get("muster-pr-review").text;
        for phrase in ["review PR 812", "look at this PR", "can I merge #812", "go through these PRs"] {
            assert!(pr.split("\n---\n").next().unwrap().contains(phrase), "{phrase}");
        }
        let panels = get("muster-panels").text;
        for want in ["ui/initialize", "ui/message", "at most 3", "resourceDomains", "no `fetch`", "panel_open", "wait_for_panel_event"] {
            assert!(panels.contains(want), "{want}");
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

    #[test]
    fn install_is_idempotent_and_never_overwrites_edits() {
        let home = temp_home("install");
        let ts = targets(false, false, env_for(&home)).unwrap();
        // Something else of the user's in the same folders stays untouched.
        std::fs::create_dir_all(home.join(".claude/skills/mine")).unwrap();
        std::fs::write(home.join(".claude/skills/mine/SKILL.md"), "mine").unwrap();
        std::fs::write(home.join(".codex/config.toml").tap_parent(), "x = 1").unwrap();

        let dry = install(&ts, true, false, None).unwrap();
        assert!(dry.iter().filter(|l| l.contains(" new ")).count() == 4 && dry.last().unwrap().contains("Dry run"));
        assert!(!home.join(".claude/skills/muster-pr-review").exists(), "dry run writes nothing");

        install(&ts, false, false, None).unwrap();
        for t in &ts { for s in SKILLS { assert_eq!(state(t, s).unwrap(), State::Current); assert_eq!(std::fs::read_to_string(file(t, s)).unwrap(), s.text); } }
        let again = install(&ts, false, false, None).unwrap();
        assert!(again.iter().all(|l| l.contains("unchanged")), "{again:?}");

        // Muster's own older copy is outdated and gets updated.
        let pr = get("muster-pr-review");
        let claude = &ts[0];
        let old = "---\nname: muster-pr-review\ndescription: old\n---\n# Old\n";
        std::fs::write(file(claude, pr), old).unwrap();
        std::fs::write(file(claude, pr).with_file_name(MARKER), format!("{}\n", sha(old.as_bytes()))).unwrap();
        assert_eq!(state(claude, pr).unwrap(), State::Outdated);
        assert!(install(&ts, false, false, None).unwrap().iter().any(|l| l.contains("updated")));
        assert_eq!(state(claude, pr).unwrap(), State::Current);

        // An edited file is refused, and nothing at all is written then.
        std::fs::write(file(claude, pr), format!("{}\nMy note.\n", pr.text)).unwrap();
        std::fs::remove_dir_all(home.join(".codex/skills/muster-panels")).unwrap();
        assert_eq!(state(claude, pr).unwrap(), State::Modified);
        let e = install(&ts, false, false, None).unwrap_err().to_string();
        assert!(e.contains("--force") && e.contains("skipped: locally modified"), "{e}");
        assert!(std::fs::read_to_string(file(claude, pr)).unwrap().contains("My note."));
        assert!(!home.join(".codex/skills/muster-panels").exists(), "a refused install writes nothing");
        let st = status(&ts, None).unwrap().join("\n");
        assert!(st.contains("locally modified") && st.contains("missing"), "{st}");
        install(&ts, false, true, None).unwrap();
        assert_eq!(state(claude, pr).unwrap(), State::Current);
        assert_eq!(std::fs::read_to_string(home.join(".claude/skills/mine/SKILL.md")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), "x = 1");
        // No temporary files are left behind.
        for t in &ts { for s in SKILLS {
            let names: Vec<_> = std::fs::read_dir(t.root.join(s.name)).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            assert_eq!({ let mut n = names.clone(); n.sort(); n }, vec![MARKER.to_string(), "SKILL.md".to_string()], "{names:?}");
        } }
        // A file Muster never wrote (no marker) is the user's.
        std::fs::remove_file(file(claude, pr).with_file_name(MARKER)).unwrap();
        std::fs::write(file(claude, pr), "theirs").unwrap();
        assert_eq!(state(claude, pr).unwrap(), State::Modified);
        std::fs::remove_dir_all(&home).unwrap();
    }

    trait TapParent { fn tap_parent(self) -> Self; }
    impl TapParent for PathBuf {
        fn tap_parent(self) -> Self { std::fs::create_dir_all(self.parent().unwrap()).unwrap(); self }
    }
}
