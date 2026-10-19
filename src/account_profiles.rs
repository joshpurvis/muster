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
    env.insert("CLAUDE_CONFIG_DIR".into(), profile.config_dir.clone());
    Ok(())
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
