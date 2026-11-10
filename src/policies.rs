//! Explicit version retention and conservative conflict resolution policy.
use crate::{
    backend::{self, Backend, Entry},
    operations::{self, Version},
    state::{self, Store},
};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictStrategy {
    #[default]
    Stop,
    KeepBoth,
    Replace,
}
impl std::str::FromStr for ConflictStrategy {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "stop" => Ok(Self::Stop),
            "keep-both" => Ok(Self::KeepBoth),
            "replace" => Ok(Self::Replace),
            _ => bail!("conflict strategy must be stop, keep-both or replace"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VersionPolicy {
    pub enabled: bool,
    pub keep: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}
impl Default for VersionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            keep: 5,
            include: vec!["*".into()],
            exclude: vec![],
        }
    }
}
impl VersionPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.keep == 0 || self.keep > 100_000 {
            bail!("version retention must keep between 1 and 100000 versions");
        }
        if self.include.len() + self.exclude.len() > 512 {
            bail!("too many version filename patterns");
        }
        for pattern in self.include.iter().chain(&self.exclude) {
            if pattern.is_empty() || pattern.len() > 4096 || pattern.chars().any(char::is_control) {
                bail!("invalid version filename pattern");
            }
            crate::filtering::matches_pattern(pattern, "")?;
        }
        Ok(())
    }
    pub fn matches(&self, path: &str) -> Result<bool> {
        self.validate()?;
        if !self.enabled {
            return Ok(false);
        }
        let mut included = false;
        for pattern in &self.include {
            included |= crate::filtering::matches_pattern(pattern, path)?;
        }
        for pattern in &self.exclude {
            if crate::filtering::matches_pattern(pattern, path)? {
                return Ok(false);
            }
        }
        Ok(included)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePolicy {
    pub versions: Option<VersionPolicy>,
    pub conflicts: Option<ConflictStrategy>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub versions: VersionPolicy,
    pub conflicts: ConflictStrategy,
    pub profiles: BTreeMap<String, ProfilePolicy>,
}
#[derive(Debug, Clone, Serialize)]
pub struct EffectivePolicy {
    pub versions: VersionPolicy,
    pub conflicts: ConflictStrategy,
}
impl PolicyConfig {
    pub fn validate(&self) -> Result<()> {
        self.versions.validate()?;
        for (profile, policy) in &self.profiles {
            if profile.is_empty() || profile.len() > 120 || profile.chars().any(char::is_control) {
                bail!("invalid policy profile name");
            }
            if let Some(versions) = &policy.versions {
                versions.validate()?;
            }
        }
        Ok(())
    }
    pub fn effective(&self, profile: &str) -> EffectivePolicy {
        let selected = self.profiles.get(profile);
        EffectivePolicy {
            versions: selected
                .and_then(|p| p.versions.clone())
                .unwrap_or_else(|| self.versions.clone()),
            conflicts: selected.and_then(|p| p.conflicts).unwrap_or(self.conflicts),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionPlan {
    pub strategy: ConflictStrategy,
    pub request: serde_json::Value,
    pub target_path: String,
    #[serde(default)]
    pub version: Option<Version>,
    #[serde(default)]
    pub repository_snapshot: Option<String>,
    pub fingerprint: String,
    pub created: String,
}

/// A deterministic name makes an interrupted keep-both operation retryable.
/// CreateNew still rejects an independently occupied destination.
pub fn conflict_name(name: &str, identity: &str) -> Result<String> {
    if name.is_empty()
        || identity.is_empty()
        || backend::normalize(name)? != name
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        bail!("invalid conflict item name or identity");
    }
    let suffix = format!(" (conflict-{})", &state::key(identity)[..16]);
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 24 => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let maximum = 255usize.saturating_sub(suffix.len() + extension.len());
    let mut end = stem.len().min(maximum);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    Ok(format!("{}{}{}", &stem[..end], suffix, extension))
}

/// Archive the exact guarded revision before destructive replacement. The
/// returned original entry must remain the write's Match condition.
pub async fn prepare_version(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    path: &str,
    expected: &Entry,
    request_id: &str,
    force: bool,
) -> Result<Option<Version>> {
    let policy = store.config.policies.effective(profile).versions;
    if !force && !policy.matches(path)? {
        return Ok(None);
    }
    policy.validate()?;
    let identity = state::key(&serde_json::to_string(&(
        profile, path, expected, request_id,
    ))?);
    Ok(Some(
        operations::archive_expected_at(
            &store.home,
            backend,
            path,
            expected,
            &format!("policy-{identity}"),
        )
        .await?,
    ))
}

/// Call only after remotely confirmed publication. A failed archive or uncertain
/// write never reaches retention; the just-captured prior revision is protected.
pub async fn finish_version(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    path: &str,
    version: &Version,
) -> Result<Vec<String>> {
    let policy = store.config.policies.effective(profile).versions;
    policy.validate()?;
    let all = operations::versions(store, backend, path).await?;
    if !all.iter().any(|stored| {
        stored.id == version.id
            && stored.sha256 == version.sha256
            && stored.original.same_revision(&version.original)
    }) {
        bail!("saved prior revision was not confirmed; no versions were pruned");
    }
    confirm_version(store, backend, path, version).await?;
    let mut keep = BTreeSet::from([version.id.clone()]);
    // Another client may have prepared a backup but not yet published its edit.
    // Never consume that client's recovery copy to satisfy our retention count.
    let mut eligible = Vec::new();
    for prior in &all {
        if prior.id.starts_with("policy-")
            && !confirmed_version(store, backend, path, prior).await?
        {
            keep.insert(prior.id.clone());
        } else {
            eligible.push(prior);
        }
    }
    let mut retained = 1;
    for prior in eligible {
        if retained >= policy.keep {
            break;
        }
        if keep.insert(prior.id.clone()) {
            retained += 1;
        }
    }
    let mut removed = Vec::new();
    for prior in all {
        if !keep.contains(&prior.id) {
            backend
                .remove(
                    &format!("{}/{}", operations::version_root(path), prior.id),
                    true,
                )
                .await?;
            removed.push(prior.id);
        }
    }
    Ok(removed)
}

fn published_path(path: &str, version: &Version) -> String {
    format!(
        "{}/{}/published.json",
        operations::version_root(path),
        version.id
    )
}
fn publication(version: &Version) -> serde_json::Value {
    serde_json::json!({"format":1,"version":version.id,"path":version.path,"sha256":version.sha256})
}
async fn confirmed_version(
    store: &Store,
    backend: &dyn Backend,
    path: &str,
    version: &Version,
) -> Result<bool> {
    let remote = published_path(path, version);
    if backend.stat(&remote).await?.is_none() {
        return Ok(false);
    }
    let temporary = store
        .home
        .join("tmp")
        .join(uuid::Uuid::new_v4().to_string());
    let checked = async {
        operations::verified_download_limited(backend, &remote, &temporary, Some(1024 * 1024))
            .await?;
        let actual: serde_json::Value = serde_json::from_slice(&std::fs::read(&temporary)?)?;
        if actual != publication(version) {
            bail!("version publication marker is invalid; no history was pruned");
        }
        Ok(true)
    }
    .await;
    let _ = std::fs::remove_file(temporary);
    checked
}
async fn confirm_version(
    store: &Store,
    backend: &dyn Backend,
    path: &str,
    version: &Version,
) -> Result<()> {
    if confirmed_version(store, backend, path, version).await? {
        return Ok(());
    }
    let temporary = store
        .home
        .join("tmp")
        .join(uuid::Uuid::new_v4().to_string());
    state::atomic_json(&temporary, &publication(version))?;
    let saved = backend
        .upload(
            &temporary,
            &published_path(path, version),
            crate::backend::WriteCondition::CreateNew,
        )
        .await;
    let _ = std::fs::remove_file(temporary);
    if saved.is_err() && !confirmed_version(store, backend, path, version).await? {
        saved?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_defaults_and_profile_overrides_are_explicit() {
        let mut config = PolicyConfig::default();
        assert!(!config.effective("p").versions.enabled);
        assert_eq!(config.effective("p").versions.keep, 5);
        assert_eq!(config.effective("p").conflicts, ConflictStrategy::Stop);
        config.versions.enabled = true;
        config.profiles.insert(
            "p".into(),
            ProfilePolicy {
                versions: None,
                conflicts: Some(ConflictStrategy::KeepBoth),
            },
        );
        assert!(config.effective("p").versions.enabled);
        assert_eq!(config.effective("p").conflicts, ConflictStrategy::KeepBoth);
        assert_eq!(config.effective("other").conflicts, ConflictStrategy::Stop);
    }
    #[test]
    fn retention_never_accepts_zero_and_filters_are_validated() {
        let mut policy = VersionPolicy {
            enabled: true,
            ..Default::default()
        };
        policy.include = vec!["*.txt".into()];
        policy.exclude = vec!["private*".into()];
        assert!(policy.matches("notes.txt").unwrap());
        assert!(!policy.matches("photo.png").unwrap());
        assert!(!policy.matches("private-notes.txt").unwrap());
        policy.keep = 0;
        assert!(policy.validate().is_err());
    }
    #[test]
    fn conflict_names_are_stable_distinct_bounded_and_preserve_extensions() {
        assert_eq!(
            conflict_name("report.txt", "a").unwrap(),
            conflict_name("report.txt", "a").unwrap()
        );
        assert_ne!(
            conflict_name("report.txt", "a").unwrap(),
            conflict_name("report.txt", "b").unwrap()
        );
        assert!(conflict_name("report.txt", "a").unwrap().ends_with(".txt"));
        let name = conflict_name(&format!("{}.txt", "测".repeat(100)), "a").unwrap();
        assert!(name.len() <= 255);
        assert!(name.ends_with(".txt"));
        assert!(conflict_name("../escape", "x").is_err());
    }

    #[test]
    fn invalid_strategies_profiles_patterns_and_names_are_rejected() {
        assert!("merge".parse::<ConflictStrategy>().unwrap_err().to_string().contains("conflict strategy"));
        let mut policy = VersionPolicy::default(); policy.include = vec!["*".into(); 513];
        assert!(policy.validate().unwrap_err().to_string().contains("too many"));
        for pattern in ["", "control\n", &"a".repeat(4097)] {
            policy.include = vec![pattern.into()];
            assert!(policy.validate().unwrap_err().to_string().contains("invalid version filename"));
        }
        let mut config = PolicyConfig::default(); config.profiles.insert("".into(), ProfilePolicy::default());
        assert!(config.validate().unwrap_err().to_string().contains("invalid policy profile"));
        assert!(conflict_name("", "identity").unwrap_err().to_string().contains("invalid conflict item"));
        assert!(conflict_name("file", "").is_err());
        assert!(conflict_name("dotless", "identity").unwrap().starts_with("dotless (conflict-"));
        assert!(conflict_name(".hidden", "identity").unwrap().starts_with(".hidden (conflict-"));
    }
}
