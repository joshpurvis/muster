//! Human-managed references to Claude Code configuration directories.
//!
//! This module may stat a directory, but never reads or writes its contents. Authentication,
//! onboarding, settings and account identity belong to the official Claude Code binary.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub type ProfileId = i64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountProfile {
    pub id: ProfileId,
    pub name: String,
    /// Canonical absolute path. Immutable after registration.
    pub config_dir: String,
    pub enabled: bool,
    pub created_at: i64,
    /// Opaque: profiles with the same value were declared by the human to sign in to one
    /// account, so they share its limits (src/usage.rs). Never derived from login files.
    #[serde(default)]
    pub account_group: Option<String>,
}

/// Omitted/null on creation means inherit; "default" explicitly selects the ordinary
/// daemon-inherited Claude environment. Once resolved, sessions persist an ID or null.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProfileChoice {
    Id(ProfileId),
    Default(DefaultProfile),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultProfile {
    Default,
}

impl ProfileChoice {
    pub fn pinned(id: Option<ProfileId>) -> Self {
        id.map(Self::Id)
            .unwrap_or(Self::Default(DefaultProfile::Default))
    }

    pub fn id(self) -> Option<ProfileId> {
        match self {
            Self::Id(id) => Some(id),
            Self::Default(_) => None,
        }
    }
}

pub fn clean_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
        "profile name must be 1–80 bytes without control characters"
    );
    anyhow::ensure!(
        !name.eq_ignore_ascii_case("default") && name.parse::<i64>().is_err(),
        "profile name cannot be 'default' or a number"
    );
    Ok(name.to_string())
}

/// Require a directory the human already set up. Do not create it, enumerate its files,
/// check for credentials, or infer whether anyone is signed in.
pub fn config_directory(path: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !path.trim().is_empty(),
        "Claude configuration directory is required"
    );
    let path = crate::paths::expand_tilde(path.trim());
    anyhow::ensure!(
        path.is_absolute(),
        "Claude configuration directory must be an absolute path"
    );
    let canonical = std::fs::canonicalize(&path).with_context(|| {
        format!(
            "Claude configuration directory {} is unavailable; set it up in Claude Code first",
            path.display()
        )
    })?;
    anyhow::ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );
    let directory = canonical
        .to_str()
        .context("Claude configuration directory must be valid UTF-8")?;
    Ok(directory.to_string())
}

// Only names are inspected or included in errors. Never print or persist their values here.
// These select authentication or endpoints ahead of the directory's subscription login.
const AUTH_OVERRIDES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
];

/// Called before a named-profile launch (and before killing an old run for restart).
/// Legacy/default launches keep their original environment. Profile settings may themselves
/// choose a provider; Muster does not inspect those files or claim to verify the account.
pub fn apply_environment(
    profile: &AccountProfile,
    env: &mut BTreeMap<String, String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        profile.enabled,
        "Claude profile '{}' is disabled; enable it before starting",
        profile.name
    );
    let conflicts: Vec<_> = AUTH_OVERRIDES
        .iter()
        .filter(|key| env.get(**key).is_some_and(|value| !value.is_empty()))
        .copied()
        .collect();
    anyhow::ensure!(
        conflicts.is_empty(),
        "Claude profile '{}' conflicts with inherited {}. Remove these overrides from Muster's launch environment, or explicitly choose Default to keep ordinary inherited behavior. No authentication files were changed",
        profile.name,
        conflicts.join(", ")
    );
    let canonical = std::fs::canonicalize(&profile.config_dir).with_context(|| {
        format!(
            "Claude profile '{}' directory is unavailable: {}",
            profile.name, profile.config_dir
        )
    })?;
    anyhow::ensure!(
        canonical.is_dir() && canonical == Path::new(&profile.config_dir),
        "Claude profile '{}' directory moved or was redirected; register its new location as a new profile",
        profile.name
    );
    // Claude Code keeps its state file in $HOME/.claude.json only while CLAUDE_CONFIG_DIR is
    // unset; set (even to the default folder) it moves to $CLAUDE_CONFIG_DIR/.claude.json. So a
    // profile at the default folder launches with the variable removed (any inherited one too).
    if launch_home(env).is_some_and(|home| is_default_folder(&canonical, &home)) {
        env.remove("CLAUDE_CONFIG_DIR");
    } else {
        env.insert("CLAUDE_CONFIG_DIR".into(), profile.config_dir.clone());
    }
    Ok(())
}

/// The HOME a child launched with `env` resolves `~/.claude` against: its own HOME, or the
/// daemon user's home when the variable is unset. None when that HOME is empty or relative:
/// the child would resolve it against its own working directory, which differs from the
/// daemon's, so such a HOME never identifies the default folder.
pub fn launch_home(env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    let home = match env.get("HOME") {
        Some(home) => std::path::PathBuf::from(home),
        None => crate::paths::home_dir(),
    };
    home.is_absolute().then_some(home)
}

/// Whether `dir` is the folder Claude Code uses when CLAUDE_CONFIG_DIR is unset:
/// `home/.claude`, compared as canonical paths so a symlink or trailing slash still matches.
/// `home` must be absolute (see [`launch_home`]); a relative one never matches. A
/// CLAUDE_CONFIG_DIR in the daemon's own environment does not count: Claude Code's state file
/// only stays in $HOME when the variable is unset.
pub fn is_default_folder(dir: &Path, home: &Path) -> bool {
    if !home.is_absolute() {
        return false;
    }
    match (std::fs::canonicalize(dir), std::fs::canonicalize(home.join(".claude"))) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("muster-profile-{}", crate::paths::random_token()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn profile(&self) -> AccountProfile {
            AccountProfile {
                id: 1,
                name: "Work".into(),
                config_dir: config_directory(self.0.to_str().unwrap()).unwrap(),
                enabled: true,
                created_at: 1,
                account_group: None,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registration_only_needs_a_directory_not_authentication_files() {
        let f = Fixture::new();
        // The empty directory is sufficient: no invented credential/onboarding checks.
        let profile = f.profile();
        assert!(std::fs::read_dir(&f.0).unwrap().next().is_none());
        let mut env = BTreeMap::from([
            ("CLAUDE_CONFIG_DIR".into(), "/previous".into()),
            ("EDITOR".into(), "vi".into()),
        ]);
        apply_environment(&profile, &mut env).unwrap();
        assert_eq!(env["CLAUDE_CONFIG_DIR"], profile.config_dir);
        assert_eq!(env["EDITOR"], "vi");
        assert!(std::fs::read_dir(&f.0).unwrap().next().is_none());
        assert!(config_directory("relative-directory").is_err());
        assert!(config_directory(f.0.join("missing").to_str().unwrap()).is_err());
    }

    #[test]
    fn auth_override_errors_name_keys_without_exposing_values_or_mutating_environment() {
        let f = Fixture::new();
        for key in AUTH_OVERRIDES {
            let mut env = BTreeMap::from([(key.to_string(), "fixture-sensitive-value".into())]);
            let before = env.clone();
            let error = apply_environment(&f.profile(), &mut env)
                .unwrap_err()
                .to_string();
            assert!(error.contains(key));
            assert!(!error.contains("fixture-sensitive-value"));
            assert_eq!(env, before);
        }
    }

    #[test]
    fn missing_disabled_and_redirected_directories_fail_without_fallback() {
        let f = Fixture::new();
        let mut profile = f.profile();
        profile.enabled = false;
        let mut env = BTreeMap::new();
        assert!(
            apply_environment(&profile, &mut env)
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        profile.enabled = true;
        std::fs::remove_dir(&f.0).unwrap();
        assert!(apply_environment(&profile, &mut env).is_err());
        let replacement = Fixture::new();
        std::os::unix::fs::symlink(&replacement.0, &f.0).unwrap();
        assert!(
            apply_environment(&profile, &mut env)
                .unwrap_err()
                .to_string()
                .contains("redirected")
        );
        assert!(env.is_empty());
        std::fs::remove_file(&f.0).unwrap();
    }

    #[test]
    fn a_profile_at_the_default_folder_launches_without_claude_config_dir() {
        // A fake HOME: <home>/.claude is the default folder, <home>/dotfiles/claude a real
        // folder that <home>/link points at, <home>/other a separate profile folder.
        let home = Fixture::new();
        let default = home.0.join(".claude");
        std::fs::create_dir(&default).unwrap();
        std::os::unix::fs::symlink(&default, home.0.join("link")).unwrap();
        std::fs::create_dir(home.0.join("other")).unwrap();
        let profile = |dir: &std::path::Path| AccountProfile {
            id: 2, name: "Work".into(), enabled: true, created_at: 1, account_group: None,
            config_dir: config_directory(dir.to_str().unwrap()).unwrap(),
        };
        let launch = |p: &AccountProfile, home: String| {
            let mut env = BTreeMap::from([
                ("HOME".into(), home),
                ("CLAUDE_CONFIG_DIR".into(), "/previous".into()),
                ("EDITOR".into(), "vi".into()),
            ]);
            apply_environment(p, &mut env).unwrap();
            env
        };
        let h = home.0.display().to_string();
        for dir in [default.clone(), home.0.join("link"), home.0.join(".claude/"), home.0.join("other/../.claude")] {
            for home in [h.clone(), format!("{h}/"), home.0.join("link/..").display().to_string()] {
                let env = launch(&profile(&dir), home);
                assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "{dir:?} must not set CLAUDE_CONFIG_DIR");
                assert_eq!(env["EDITOR"], "vi");
            }
        }
        // The default folder itself reached through a symlinked HOME/.claude also matches.
        let real = home.0.join("dotfiles");
        std::fs::create_dir(&real).unwrap();
        let other_home = Fixture::new();
        std::os::unix::fs::symlink(&real, other_home.0.join(".claude")).unwrap();
        let env = launch(&profile(&real), other_home.0.display().to_string());
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"));
        // Any other folder still selects itself.
        let other = profile(&home.0.join("other"));
        assert_eq!(launch(&other, h.clone())["CLAUDE_CONFIG_DIR"], other.config_dir);
        // The default folder of a different HOME is just another folder.
        let env = launch(&profile(&default), other_home.0.display().to_string());
        assert_eq!(env["CLAUDE_CONFIG_DIR"], default.canonicalize().unwrap().to_str().unwrap());
        assert!(is_default_folder(&default, &home.0));
        assert!(!is_default_folder(&home.0.join("missing"), &home.0));

        // A relative or empty HOME never identifies the default folder, whatever the cwd it
        // would resolve against: the profile keeps its explicit CLAUDE_CONFIG_DIR.
        let p = profile(&default);
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count());
        let relative = format!("{up}{}", h.trim_start_matches('/'));
        assert_eq!(std::fs::canonicalize(std::path::Path::new(&relative).join(".claude")).unwrap(),
            std::fs::canonicalize(&default).unwrap(), "the relative HOME resolves to the fixture from this cwd");
        for home in [relative, "home".into(), ".".into(), String::new()] {
            assert_eq!(launch(&p, home.clone())["CLAUDE_CONFIG_DIR"], p.config_dir, "HOME {home:?}");
        }
        assert!(!is_default_folder(&default, Path::new(".")));
        // Unset HOME falls back to the daemon user's (absolute) home, which is not the fixture.
        let mut env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), "/previous".to_string())]);
        apply_environment(&p, &mut env).unwrap();
        assert_eq!(env["CLAUDE_CONFIG_DIR"], p.config_dir);
        assert!(launch_home(&BTreeMap::new()).is_none_or(|h| h.is_absolute()));
        assert_eq!(launch_home(&BTreeMap::from([("HOME".to_string(), "rel".to_string())])), None);
        assert_eq!(launch_home(&BTreeMap::from([("HOME".to_string(), String::new())])), None);
    }

    #[test]
    fn default_and_omitted_selection_are_distinct() {
        #[derive(Deserialize)]
        struct Request {
            account_profile: Option<ProfileChoice>,
        }
        let absent: Request = serde_json::from_str("{}").unwrap();
        let default: Request = serde_json::from_str(r#"{"account_profile":"default"}"#).unwrap();
        let selected: Request = serde_json::from_str(r#"{"account_profile":8}"#).unwrap();
        assert_eq!(absent.account_profile, None);
        assert_eq!(default.account_profile, Some(ProfileChoice::pinned(None)));
        assert_eq!(selected.account_profile, Some(ProfileChoice::Id(8)));
        assert!(clean_name("default").is_err());
        assert!(clean_name("42").is_err());
        assert!(clean_name("bad\nname").is_err());
    }
}
