use super::{
    ensure_remote, file_version, has_revision, internal, load_baseline, matches_baseline,
    remote_path, validate_relative, verified_same_contents, LocalRoot, LocalVersion, Pair,
    INTERNAL,
};
use crate::{
    backend::{normalize, Backend, Entry, WriteCondition},
    state,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictChoice {
    Local,
    Remote,
}

#[derive(Debug, Serialize)]
pub struct ConflictRepairReport {
    pub path: String,
    pub choice: ConflictChoice,
    pub token: String,
    pub local_size: u64,
    pub local_sha256: String,
    pub remote: Entry,
    pub applied: bool,
    pub baseline_updated: bool,
    /// Relative to the local sync root. This holds the previous file contents.
    pub recovery_path: Option<String>,
}

/// Resolve one regular file that changed on both sides of a common baseline.
/// Without a token this only reports the currently observed revisions. Apply
/// binds the requested choice to that preview and archives the losing content.
pub async fn repair_conflict(
    backend: &dyn Backend,
    local: &Path,
    remote: &str,
    baseline_path: &Path,
    path: &str,
    choice: ConflictChoice,
    expected: Option<&str>,
) -> Result<ConflictRepairReport> {
    repair_conflict_with(
        backend,
        local,
        remote,
        baseline_path,
        path,
        choice,
        expected,
        LocalRoot::install_new,
    )
    .await
}

pub(super) async fn repair_conflict_with(
    backend: &dyn Backend,
    local: &Path,
    remote: &str,
    baseline_path: &Path,
    path: &str,
    choice: ConflictChoice,
    expected: Option<&str>,
    install: impl FnOnce(&LocalRoot, &Path, &str) -> Result<()>,
) -> Result<ConflictRepairReport> {
    let root = LocalRoot::open(local)?;
    let remote_root = normalize(remote)?;
    validate_relative(path)?;
    if internal(path) || internal(&remote_root) {
        bail!("internal RDrive paths cannot be repaired as sync conflicts");
    }
    let mut baseline = load_baseline(&root, &remote_root, baseline_path)?
        .context("conflict repair requires a recorded common sync baseline")?;
    let previous = baseline
        .entries
        .get(path)
        .filter(|pair| !pair.local.is_dir)
        .context("conflict repair requires a common regular-file baseline for this path")?;
    let local = root
        .version(path)?
        .filter(|version| !version.is_dir)
        .context("conflict repair requires an existing local regular file")?;
    let remote_file = remote_path(&remote_root, path);
    let remote = backend
        .stat(&remote_file)
        .await?
        .filter(|entry| entry.path == remote_file && !entry.is_dir)
        .context("conflict repair requires an existing remote regular file")?;
    if !has_revision(&remote) {
        bail!("remote file has no verifiable revision for conflict repair");
    }
    if local == previous.local || matches_baseline(backend, &remote, previous) {
        bail!("both sides must have changed since the common baseline to repair a conflict");
    }
    if choice == ConflictChoice::Local && !backend.capabilities().conditional_write {
        bail!("the backend cannot guard replacement of the remote file");
    }
    let resolved_baseline = state::resolve_location(baseline_path)?;
    let token_inputs = (
        "rdrive-sync-conflict-v1",
        resolved_baseline.as_os_str().as_encoded_bytes(),
        path,
        choice,
        previous,
        &local,
        &remote,
    );
    let token = state::key(&serde_json::to_string(&token_inputs)?);
    let mut report = ConflictRepairReport {
        path: path.into(),
        choice,
        token,
        local_size: local.size,
        local_sha256: local
            .sha256
            .clone()
            .context("local file checksum is missing")?,
        remote: remote.clone(),
        applied: false,
        baseline_updated: false,
        recovery_path: None,
    };
    let Some(expected) = expected else {
        if verified_same_contents(backend, &root, path, &local, &remote).await? {
            bail!("local and remote contents already match; run ordinary sync to refresh the baseline");
        }
        return Ok(report);
    };
    if expected != report.token {
        bail!("the conflict or chosen side changed after preview; preview this repair again");
    }

    let staged_remote = root.stage()?;
    backend
        .download_checked(&remote_file, &staged_remote.path, &remote)
        .await?;
    let remote_content = file_version(&staged_remote.path)?;
    if remote_content.size != remote.size
        || backend
            .content_sha256(&remote)
            .is_some_and(|hash| remote_content.sha256.as_ref() != Some(&hash))
    {
        bail!("downloaded content differs from the reviewed remote revision");
    }
    if remote_content == local {
        bail!("local and remote contents already match; run ordinary sync to refresh the baseline");
    }
    ensure_remote(&backend.stat(&remote_file).await?, Some(&remote))?;
    root.ensure_version(path, Some(&local))?;

    let (resolved, recovery) = match choice {
        ConflictChoice::Local => {
            let staged_local = root.stage()?;
            root.snapshot(path, &local, &staged_local.path)?;
            let recovery = retain_remote(&root, &staged_remote.path, &remote_content)?;
            let resolved = async {
                root.ensure_version(path, Some(&local))?;
                ensure_remote(&backend.stat(&remote_file).await?, Some(&remote))?;
                let written = backend
                    .upload(
                        &staged_local.path,
                        &remote_file,
                        WriteCondition::Match(remote.clone()),
                    )
                    .await?;
                let observed = backend.stat(&remote_file).await?;
                ensure_remote(&observed, Some(&written))?;
                let observed = observed
                    .filter(|entry| {
                        entry.path == remote_file && !entry.is_dir && entry.size == local.size
                    })
                    .context("could not confirm the uploaded file revision")?;
                if backend
                    .content_sha256(&observed)
                    .is_some_and(|hash| local.sha256.as_ref() != Some(&hash))
                {
                    bail!("uploaded content differs from the reviewed local version");
                }
                root.ensure_version(path, Some(&local))?;
                Ok::<_, anyhow::Error>(Pair {
                    local,
                    remote: observed,
                })
            }
            .await
            .with_context(|| format!("previous remote contents are retained at {recovery}"))?;
            (resolved, recovery)
        }
        ConflictChoice::Remote => {
            // A content-only repair keeps the selected local attributes just as
            // ordinary sync does when downloading an updated remote file.
            let mut attributes = root.selected_metadata(path)?;
            attributes.content_sha256 = remote_content
                .sha256
                .clone()
                .context("download checksum is missing")?;
            crate::metadata::apply_with_recovery(
                &staged_remote.path,
                &attributes,
                &Default::default(),
                &staged_remote.directory.join("xattr-recovery.json"),
            )?;
            ensure_remote(&backend.stat(&remote_file).await?, Some(&remote))?;
            let recovery = root.archive(path, &local)?;
            install(&root, &staged_remote.path, path)
                .with_context(|| format!("previous local contents are retained at {recovery}"))?;
            (
                Pair {
                    local: remote_content,
                    remote,
                },
                recovery,
            )
        }
    };
    let complete: Result<()> = async {
        root.ensure_version(path, Some(&resolved.local))?;
        ensure_remote(&backend.stat(&remote_file).await?, Some(&resolved.remote))?;
        baseline.entries.insert(path.into(), resolved);
        state::atomic_json(baseline_path, &baseline)
    }
    .await;
    complete.with_context(|| {
        format!(
            "could not confirm the repaired baseline. Previous contents are retained at {recovery}"
        )
    })?;
    report.applied = true;
    report.baseline_updated = true;
    report.recovery_path = Some(recovery);
    Ok(report)
}

fn retain_remote(root: &LocalRoot, downloaded: &Path, expected: &LocalVersion) -> Result<String> {
    let directory = format!("{INTERNAL}/{}", uuid::Uuid::new_v4());
    #[cfg(unix)]
    root.directory(&directory, true)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(root.checked_path(&directory)?)?;
    let recovery = format!("{directory}/original");
    root.install_new(downloaded, &recovery)?;
    root.ensure_version(&recovery, Some(expected))?;
    Ok(recovery)
}
