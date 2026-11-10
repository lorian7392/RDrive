use crate::{
    backend::{self, Backend, Entry, WriteCondition},
    state::{self, CacheItem, Job, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub async fn verified_download(backend: &dyn Backend, remote: &str, temp: &Path) -> Result<Entry> {
    verified_download_limited(backend, remote, temp, None).await
}

/// Bound the guarded revision before requesting its contents. Protocol backends
/// enforce that revision's expected size while streaming, including a growing
/// or chunked response, so bounded text reads do not spool arbitrary files.
pub async fn verified_download_limited(
    backend: &dyn Backend,
    remote: &str,
    temp: &Path,
    max_bytes: Option<u64>,
) -> Result<Entry> {
    let before = backend
        .stat(remote)
        .await?
        .context("remote file does not exist")?;
    if before.is_dir {
        bail!("expected a remote file");
    }
    if max_bytes.is_some_and(|maximum| before.size > maximum) {
        bail!("remote file exceeds the requested byte limit; use a download instead");
    }
    let result = async {
        backend.download_checked(remote, temp, &before).await?;
        let after = backend
            .stat(remote)
            .await?
            .context("remote file disappeared during download")?;
        if !before.same_revision(&after) {
            bail!("remote file changed or has no verifiable revision; download was not committed");
        }
        if fs::metadata(temp)?.len() != after.size {
            bail!("download length does not match remote size");
        }
        if let Some(hash) = backend.content_sha256(&before) {
            if state::fingerprint(temp)? != hash {
                bail!("download checksum does not match the protected remote revision");
            }
        }
        Ok(after)
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub fn destination_temp(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        bail!("destination directory does not exist");
    }
    Ok(parent
        .canonicalize()?
        .join(format!(".rdrive-{}.part", uuid::Uuid::new_v4())))
}

pub async fn get(
    backend: &dyn Backend,
    remote: &str,
    destination: &Path,
    overwrite: bool,
) -> Result<Entry> {
    if destination.exists() && !overwrite {
        bail!("destination exists; use --overwrite to replace it");
    }
    let temp = destination_temp(destination)?;
    let entry = verified_download(backend, remote, &temp).await?;
    if let Err(e) = state::commit_download(&temp, destination, overwrite) {
        let _ = fs::remove_file(temp);
        return Err(e);
    }
    Ok(entry)
}

pub async fn cache_file(
    store: &mut Store,
    backend: &dyn Backend,
    profile: &str,
    remote: &str,
    pin: bool,
) -> Result<CacheItem> {
    crate::cache::fetch(store, backend, profile, remote, pin).await
}

pub fn cache_get(
    store: &mut Store,
    profile: &str,
    remote: &str,
    destination: &Path,
    overwrite: bool,
) -> Result<CacheItem> {
    crate::cache::get(store, profile, remote, destination, overwrite)
}

pub fn enqueue(
    store: &mut Store,
    profile: &str,
    local: &Path,
    remote: &str,
    overwrite: bool,
    expected: Option<Entry>,
    version: bool,
) -> Result<Job> {
    enqueue_with_resume(
        store, profile, local, remote, overwrite, expected, version, false,
    )
}

pub fn enqueue_with_resume(
    store: &mut Store,
    profile: &str,
    local: &Path,
    remote: &str,
    overwrite: bool,
    expected: Option<Entry>,
    version: bool,
    resume: bool,
) -> Result<Job> {
    let remote = backend::normalize(remote)?;
    if remote.is_empty() {
        bail!("upload destination cannot be the remote root");
    }
    if !store.config.profiles.contains_key(profile) {
        bail!("unknown profile {profile}");
    }
    if resume {
        let hash = state::fingerprint(local)?;
        if let Some(existing) = store.db.jobs.iter().rev().find(|job| {
            job.resume
                && job.profile == profile
                && job.remote == remote
                && job.overwrite == overwrite
                && job.version == version
                && job.sha256 == hash
                && !["done", "cancelled"].contains(&job.status.as_str())
        }) {
            return Ok(existing.clone());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let dest = store.home.join("spool").join(&id);
    let (sha256, size) = state::snapshot(local, &dest)?;
    let job = Job {
        id: id.clone(),
        profile: profile.into(),
        remote,
        snapshot: id,
        sha256,
        size,
        created: state::now(),
        status: "pending".into(),
        attempts: 0,
        error: None,
        expected,
        overwrite,
        version,
        resume,
        version_prepared: false,
    };
    store.db.jobs.push(job.clone());
    store.save()?;
    Ok(job)
}

pub async fn run_job(store: &mut Store, backend: &dyn Backend, id: &str) -> Result<Entry> {
    let index = store
        .db
        .jobs
        .iter()
        .position(|j| j.id == id)
        .context("unknown queue job")?;
    let job = store.db.jobs[index].clone();
    if job.status == "done" || job.status == "cancelled" {
        bail!("job is already {}", job.status);
    }
    let source = store.home.join("spool").join(&job.snapshot);
    if state::fingerprint(&source)? != job.sha256 {
        bail!("upload snapshot checksum mismatch; refusing to upload");
    }
    store.db.jobs[index].status = "running".into();
    store.db.jobs[index].attempts += 1;
    store.db.jobs[index].error = None;
    store.save()?;
    let result = async {
        if job.version && !job.version_prepared {
            if let Some(version) = archive(store, backend, &job.remote).await? {
                if job.overwrite {
                    store.db.jobs[index].expected = Some(version.original);
                }
            }
            store.db.jobs[index].version_prepared = true;
            store.save()?;
        }
        let condition = store.db.jobs[index].condition();
        if job.resume {
            let checkpoint = store
                .home
                .join("spool")
                .join(format!("{}.upload.json", job.id));
            backend
                .upload_resume(&source, &job.remote, condition, &checkpoint)
                .await
        } else {
            backend.upload(&source, &job.remote, condition).await
        }
    }
    .await;
    match result {
        Ok(entry) => {
            store.db.jobs[index].status = "done".into();
            store.save()?;
            fs::remove_file(source)?;
            store.event(
                "upload",
                &job.profile,
                &job.remote,
                "server confirmed completion",
            )?;
            Ok(entry)
        }
        Err(e) => {
            store.db.jobs[index].status = "failed".into();
            store.db.jobs[index].error = Some(format!("{e:#}"));
            store.save()?;
            Err(e.context(format!("upload preserved in queue job {id}")))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub path: String,
    pub saved: String,
    pub original: Entry,
    pub sha256: String,
}
pub fn version_root(remote: &str) -> String {
    format!(".rdrive-versions/{}", state::key(remote))
}

pub async fn archive(
    store: &Store,
    backend: &dyn Backend,
    remote: &str,
) -> Result<Option<Version>> {
    archive_at(&store.home, backend, remote).await
}

pub async fn archive_at(
    home: &Path,
    backend: &dyn Backend,
    remote: &str,
) -> Result<Option<Version>> {
    let Some(original) = backend.stat(remote).await? else {
        return Ok(None);
    };
    if original.is_dir {
        bail!("custom versions support files only");
    }
    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    );
    let root = format!("{}/{id}", version_root(remote));
    backend::mkdir_all(backend, &root).await?;
    // Store a checked, independent snapshot rather than racing a portable COPY
    // against edits to its source. Transfers remain streamed through disk.
    let content_temp = home.join("tmp").join(uuid::Uuid::new_v4().to_string());
    let snapshot_revision = verified_download(backend, remote, &content_temp).await?;
    if !snapshot_revision.same_revision(&original) {
        let _ = fs::remove_file(&content_temp);
        bail!("remote changed before version snapshot; upload was not started");
    }
    let sha256 = state::fingerprint(&content_temp)?;
    let saved = backend
        .upload(
            &content_temp,
            &format!("{root}/content"),
            WriteCondition::CreateNew,
        )
        .await;
    let _ = fs::remove_file(&content_temp);
    saved?;
    let version = Version {
        id,
        path: remote.into(),
        saved: state::now(),
        original,
        sha256,
    };
    let temp = home.join("tmp").join(uuid::Uuid::new_v4().to_string());
    state::atomic_json(&temp, &version)?;
    let result = backend
        .upload(
            &temp,
            &format!("{root}/manifest.json"),
            WriteCondition::CreateNew,
        )
        .await;
    let _ = fs::remove_file(temp);
    result?;
    Ok(Some(version))
}

/// Idempotent policy backup of one exact revision. A persisted resolution can
/// retry this identity after a lost response without creating another version.
pub async fn archive_expected_at(
    home: &Path,
    backend: &dyn Backend,
    remote: &str,
    expected: &Entry,
    id: &str,
) -> Result<Version> {
    if backend::normalize(remote)? != remote
        || expected.path != remote
        || expected.is_dir
        || id.is_empty()
        || backend::normalize(id)? != id
        || id.contains('/')
        || id.chars().any(char::is_control)
    {
        bail!("invalid guarded version snapshot identity");
    }
    let root = format!("{}/{id}", version_root(remote));
    let manifest = format!("{root}/manifest.json");
    let content = format!("{root}/content");
    let temporary = home.join("tmp").join(uuid::Uuid::new_v4().to_string());
    let result = async {
        if backend.stat(&manifest).await?.is_some() {
            verified_download_limited(backend, &manifest, &temporary, Some(1024 * 1024)).await?;
            let saved: Version = serde_json::from_slice(&fs::read(&temporary)?)?;
            if saved.path != remote || saved.id != id || !saved.original.same_revision(expected) {
                bail!("version_conflict: saved policy version belongs to another revision");
            }
            let _ = fs::remove_file(&temporary);
            verified_download_limited(backend, &content, &temporary, Some(expected.size)).await?;
            if fs::metadata(&temporary)?.len() != expected.size
                || state::fingerprint(&temporary)? != saved.sha256
            {
                bail!("version snapshot checksum mismatch; original files were preserved");
            }
            return Ok(saved);
        }
        let before = backend
            .stat(remote)
            .await?
            .context("version_conflict: version source disappeared")?;
        if !before.same_revision(expected) {
            bail!("version_conflict: version source changed before backup");
        }
        backend
            .download_checked(remote, &temporary, expected)
            .await?;
        let after = backend
            .stat(remote)
            .await?
            .context("version_conflict: version source disappeared during backup")?;
        if !after.same_revision(expected) || fs::metadata(&temporary)?.len() != expected.size {
            bail!("version_conflict: source changed while creating prior revision");
        }
        let sha256 = state::fingerprint(&temporary)?;
        if backend
            .content_sha256(expected)
            .is_some_and(|hash| hash != sha256)
        {
            bail!("version source checksum differs from its guarded revision");
        }
        backend::mkdir_all(backend, &root).await?;
        if backend.stat(&content).await?.is_some() {
            let prior = home.join("tmp").join(uuid::Uuid::new_v4().to_string());
            let checked = async {
                verified_download_limited(backend, &content, &prior, Some(expected.size)).await?;
                if fs::metadata(&prior)?.len() != expected.size
                    || state::fingerprint(&prior)? != sha256
                {
                    bail!("version_conflict: policy backup slot contains different bytes");
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = fs::remove_file(prior);
            checked?;
        } else {
            backend
                .upload(&temporary, &content, WriteCondition::CreateNew)
                .await?;
            let checked = home.join("tmp").join(uuid::Uuid::new_v4().to_string());
            let verified = async {
                verified_download_limited(backend, &content, &checked, Some(expected.size)).await?;
                if fs::metadata(&checked)?.len() != expected.size
                    || state::fingerprint(&checked)? != sha256
                {
                    bail!(
                        "version backup checksum mismatch; destructive replacement was not started"
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = fs::remove_file(checked);
            verified?;
        }
        let saved = Version {
            id: id.into(),
            path: remote.into(),
            saved: state::now(),
            original: expected.clone(),
            sha256,
        };
        state::atomic_json(&temporary, &saved)?;
        backend
            .upload(&temporary, &manifest, WriteCondition::CreateNew)
            .await?;
        Ok(saved)
    }
    .await;
    let _ = fs::remove_file(temporary);
    result
}

pub async fn versions(store: &Store, backend: &dyn Backend, remote: &str) -> Result<Vec<Version>> {
    let root = version_root(remote);
    if backend.stat(&root).await?.is_none() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in backend.list(&root).await? {
        if !entry.is_dir {
            continue;
        }
        let path = format!("{}/manifest.json", entry.path);
        if backend.stat(&path).await?.is_none() {
            continue;
        }
        let temp = store
            .home
            .join("tmp")
            .join(uuid::Uuid::new_v4().to_string());
        verified_download(backend, &path, &temp).await?;
        let data = fs::read(&temp)?;
        let _ = fs::remove_file(temp);
        let v: Version = serde_json::from_slice(&data)?;
        let parent_id = entry
            .path
            .rsplit('/')
            .next()
            .context("invalid version directory")?;
        if v.path != remote || v.id != parent_id {
            bail!("version manifest does not match directory");
        }
        result.push(v);
    }
    result.sort_by(|a, b| b.saved.cmp(&a.saved).then_with(|| b.id.cmp(&a.id)));
    Ok(result)
}

pub async fn restore(
    store: &Store,
    backend: &dyn Backend,
    version: &Version,
    overwrite: bool,
) -> Result<Entry> {
    let temp = store
        .home
        .join("tmp")
        .join(uuid::Uuid::new_v4().to_string());
    let result = async {
        verified_download(
            backend,
            &format!("{}/{}/content", version_root(&version.path), version.id),
            &temp,
        )
        .await?;
        if state::fingerprint(&temp)? != version.sha256 {
            bail!("version content checksum mismatch; destination was preserved");
        }
        backend
            .upload(
                &temp,
                &version.path,
                if overwrite {
                    WriteCondition::Any
                } else {
                    WriteCondition::CreateNew
                },
            )
            .await
    }
    .await;
    let _ = fs::remove_file(temp);
    result
}
