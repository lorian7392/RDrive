//! Revision-bound portable metadata sidecars and conservative metadata-only sync.
use crate::{
    backend::{self, Backend, Entry, WriteCondition},
    metadata::{self, ApplyReport, MetadataPolicy, MetadataRecord},
    state,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};
const ROOT: &str = ".rdrive-metadata";
const MAX_SIDECAR: u64 = 12 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    path: String,
    entry: Entry,
    record: MetadataRecord,
}
struct Loaded {
    record: MetadataRecord,
    entry: Entry,
}
#[derive(Debug, Serialize)]
pub struct SaveReport {
    pub sidecar: String,
    pub changed: bool,
    pub metadata_sha256: String,
    pub excluded: Vec<metadata::ExcludedAttribute>,
}
pub fn sidecar_path(remote: &str, entry: &Entry) -> Result<String> {
    let remote = backend::normalize(remote)?;
    if remote.is_empty() || entry.path != remote || entry.is_dir {
        bail!("metadata requires the revision of a non-root file");
    }
    let etag = entry
        .etag
        .as_deref()
        .filter(|etag| !etag.is_empty() && !etag.starts_with("W/"))
        .context("portable metadata requires a strong remote revision")?;
    Ok(format!(
        "{ROOT}/{}/{}.json",
        state::key(&remote),
        state::key(&serde_json::to_string(&(etag, entry.size))?)
    ))
}
async fn same_file(b: &dyn Backend, remote: &str, expected: &Entry) -> Result<()> {
    let current = b
        .stat(remote)
        .await?
        .context("remote file disappeared during metadata operation")?;
    if current.path != expected.path || !current.same_revision(expected) {
        bail!("remote content revision changed; metadata was not attached to the new revision");
    }
    Ok(())
}
async fn load_inner(b: &dyn Backend, remote: &str, entry: &Entry) -> Result<Option<Loaded>> {
    let path = sidecar_path(remote, entry)?;
    same_file(b, remote, entry).await?;
    let Some(sidecar) = b.stat(&path).await? else {
        return Ok(None);
    };
    if sidecar.is_dir || sidecar.size > MAX_SIDECAR {
        bail!("invalid or oversized metadata sidecar");
    }
    let temp = Temporary::new()?;
    b.download_checked(&path, &temp.path, &sidecar).await?;
    let after = b
        .stat(&path)
        .await?
        .context("metadata sidecar disappeared")?;
    if !after.same_revision(&sidecar) {
        bail!("metadata sidecar changed while reading");
    }
    let bytes = fs::read(&temp.path)?;
    if bytes.len() as u64 > MAX_SIDECAR {
        bail!("oversized metadata sidecar");
    }
    let envelope: Envelope = serde_json::from_slice(&bytes)
        .context("invalid metadata sidecar; no attributes were applied")?;
    envelope.record.validate()?;
    if envelope.version != 1
        || envelope.path != remote
        || envelope.entry.path != remote
        || !envelope.entry.same_revision(entry)
    {
        bail!("metadata sidecar is bound to another path or content revision");
    }
    same_file(b, remote, entry).await?;
    Ok(Some(Loaded {
        record: envelope.record,
        entry: after,
    }))
}
pub async fn load(b: &dyn Backend, remote: &str, entry: &Entry) -> Result<Option<MetadataRecord>> {
    Ok(load_inner(b, remote, entry)
        .await?
        .map(|loaded| loaded.record))
}
async fn remote_hash(b: &dyn Backend, remote: &str, entry: &Entry) -> Result<String> {
    same_file(b, remote, entry).await?;
    if let Some(hash) = b.content_sha256(entry) {
        return Ok(hash);
    }
    let temp = Temporary::new()?;
    b.download_checked(remote, &temp.path, entry).await?;
    same_file(b, remote, entry).await?;
    if fs::metadata(&temp.path)?.len() != entry.size {
        bail!("remote metadata source length changed");
    }
    state::fingerprint(&temp.path)
}
async fn save_checked(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    record: &MetadataRecord,
    previous: Option<&Loaded>,
) -> Result<SaveReport> {
    record.validate()?;
    let path = sidecar_path(remote, entry)?;
    if remote_hash(b, remote, entry).await? != record.content_sha256 {
        bail!("metadata source content differs from remote file; previous sidecar was preserved");
    }
    let current = b.stat(&path).await?;
    match (&current, previous) {
        (None, None) => (),
        (Some(actual), Some(expected))
            if actual.same_revision(&expected.entry) && actual.path == expected.entry.path =>
        {
            ()
        }
        _ => bail!(
            "remote metadata changed after planning; neither metadata snapshot was overwritten"
        ),
    }
    let publication = merge_record(record, previous.map(|loaded| &loaded.record))?;
    if previous.is_some_and(|old| old.record == publication) {
        return Ok(SaveReport {
            sidecar: path,
            changed: false,
            metadata_sha256: publication.fingerprint()?,
            excluded: publication.excluded,
        });
    }
    let parent = path.rsplit_once('/').unwrap().0;
    backend::mkdir_all(b, parent).await?;
    let temp = Temporary::new()?;
    state::atomic_json(
        &temp.path,
        &Envelope {
            version: 1,
            path: remote.into(),
            entry: entry.clone(),
            record: publication.clone(),
        },
    )?;
    same_file(b, remote, entry).await?;
    b.upload(
        &temp.path,
        &path,
        previous
            .map(|loaded| loaded.entry.clone())
            .map(WriteCondition::Match)
            .unwrap_or(WriteCondition::CreateNew),
    )
    .await?;
    let loaded = load_inner(b, remote, entry)
        .await?
        .context("metadata publication was not confirmed")?;
    if loaded.record != publication {
        bail!("published metadata changed before confirmation");
    }
    Ok(SaveReport {
        sidecar: path,
        changed: true,
        metadata_sha256: publication.fingerprint()?,
        excluded: publication.excluded,
    })
}
pub async fn save_record(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    record: &MetadataRecord,
) -> Result<SaveReport> {
    let current = load_inner(b, remote, entry).await?;
    if current
        .as_ref()
        .is_some_and(|current| current.record == *record)
    {
        return Ok(SaveReport {
            sidecar: sidecar_path(remote, entry)?,
            changed: false,
            metadata_sha256: record.fingerprint()?,
            excluded: record.excluded.clone(),
        });
    }
    save_checked(b, remote, entry, record, current.as_ref()).await
}
pub async fn save(
    b: &dyn Backend,
    local: &Path,
    remote: &str,
    entry: &Entry,
    policy: &MetadataPolicy,
) -> Result<SaveReport> {
    save_record(b, remote, entry, &metadata::capture(local, policy)?).await
}
pub async fn apply(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    local: &Path,
    policy: &MetadataPolicy,
) -> Result<ApplyReport> {
    let Some(record) = load(b, remote, entry).await? else {
        return Ok(ApplyReport::default());
    };
    metadata::apply(local, &record, policy)
}

fn merge_record(
    incoming: &MetadataRecord,
    previous: Option<&MetadataRecord>,
) -> Result<MetadataRecord> {
    let Some(previous) = previous else {
        return Ok(incoming.clone());
    };
    if previous.content_sha256 != incoming.content_sha256 {
        bail!("previous sidecar has a different content binding; metadata was preserved for explicit repair");
    }
    let selected = |name: &str| {
        incoming.selection.iter().any(|pattern| {
            pattern
                .strip_suffix('*')
                .map_or(name == pattern, |prefix| name.starts_with(prefix))
        })
    };
    let mut merged = incoming.clone();
    for (name, value) in &previous.attributes {
        if !selected(name) {
            merged.attributes.insert(name.clone(), value.clone());
        }
    }
    merged.selection.extend(previous.selection.iter().cloned());
    merged.selection.sort();
    merged.selection.dedup();
    for excluded in &previous.excluded {
        if !selected(&excluded.name) && !merged.excluded.contains(excluded) {
            merged.excluded.push(excluded.clone());
        }
    }
    merged.validate()?;
    Ok(merged)
}

pub struct SyncOptions {
    pub apply: bool,
    pub direction: crate::sync::Direction,
}
#[derive(Debug, Serialize)]
pub struct Action {
    pub path: String,
    pub direction: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct Conflict {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Default, Serialize)]
pub struct MetadataSyncReport {
    pub applied: bool,
    pub actions: Vec<Action>,
    pub conflicts: Vec<Conflict>,
    pub warnings: Vec<String>,
    pub baseline_updated: bool,
}
#[derive(Serialize, Deserialize, Clone)]
struct Pair {
    content_sha256: String,
    local: String,
    remote: Option<String>,
    had_attributes: bool,
}
#[derive(Serialize, Deserialize)]
struct Baseline {
    version: u32,
    local_root: PathBuf,
    remote_root: String,
    policy: Vec<String>,
    entries: BTreeMap<String, Pair>,
}
struct Planned {
    path: String,
    local: MetadataRecord,
    remote: Option<Loaded>,
    entry: Entry,
    kind: &'static str,
    identity: String,
}

pub async fn sync(
    b: &dyn Backend,
    local_root: &Path,
    remote_root: &str,
    baseline_path: &Path,
    options: &SyncOptions,
) -> Result<MetadataSyncReport> {
    sync_with_policy(
        b,
        local_root,
        remote_root,
        baseline_path,
        options,
        &MetadataPolicy::default(),
    )
    .await
}

/// Guard an interactive edit against the metadata snapshot shown to its caller.
/// `None` means no sidecar existed; an already achieved desired snapshot is a
/// successful replay and does not overwrite a concurrent writer.
pub async fn save_record_if_fingerprint(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    record: &MetadataRecord,
    expected: Option<&str>,
) -> Result<SaveReport> {
    record.validate()?;
    let current = load_inner(b, remote, entry).await?;
    let actual = current
        .as_ref()
        .map(|loaded| loaded.record.fingerprint())
        .transpose()?;
    let publication = merge_record(record, current.as_ref().map(|loaded| &loaded.record))?;
    if current
        .as_ref()
        .is_some_and(|loaded| loaded.record == publication)
    {
        return Ok(SaveReport {
            sidecar: sidecar_path(remote, entry)?,
            changed: false,
            metadata_sha256: publication.fingerprint()?,
            excluded: publication.excluded,
        });
    }
    if actual.as_deref() != expected {
        bail!("remote metadata revision changed since the editing snapshot; attributes were preserved");
    }
    save_checked(b, remote, entry, record, current.as_ref()).await
}
pub async fn sync_with_policy(
    b: &dyn Backend,
    local_root: &Path,
    remote_root: &str,
    baseline_path: &Path,
    options: &SyncOptions,
    policy: &MetadataPolicy,
) -> Result<MetadataSyncReport> {
    policy.validate()?;
    let tree = LocalTree::open(local_root)?;
    let remote_root = backend::normalize(remote_root)?;
    if is_internal(&remote_root) {
        bail!("metadata internal directory cannot be a sync root");
    }
    let baseline_abs = std::path::absolute(baseline_path)?;
    if baseline_abs.starts_with(&tree.path) {
        bail!("metadata baseline must be outside local sync tree");
    }
    let mut selection = policy.include.clone();
    selection.sort();
    selection.dedup();
    let mut baseline: Baseline = match fs::read(baseline_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid metadata sync baseline")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Baseline {
            version: 1,
            local_root: tree.path.clone(),
            remote_root: remote_root.clone(),
            policy: selection.clone(),
            entries: BTreeMap::new(),
        },
        Err(error) => return Err(error.into()),
    };
    if baseline.version != 1
        || baseline.local_root != tree.path
        || baseline.remote_root != remote_root
        || baseline.policy != selection
    {
        bail!("metadata baseline belongs to another root or selection policy");
    }
    let (local, mut names) = tree.scan(policy)?;
    let remote = remote_files(b, &remote_root, &mut names).await?;
    let mut seen = BTreeMap::new();
    for name in names {
        let folded = crate::sync::collision_key(&name)?;
        if let Some(old) = seen.insert(folded, name.clone()) {
            if old != name {
                bail!(
                    "case/canonical metadata filename collision: {:?} and {:?}",
                    old,
                    name
                );
            }
        }
    }
    let pushes = options.direction != crate::sync::Direction::Pull;
    let pulls = options.direction != crate::sync::Direction::Push;
    let mut report = MetadataSyncReport::default();
    let mut pending = Vec::new();
    let mut confirmed = BTreeMap::new();
    for path in local
        .keys()
        .chain(remote.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        let mut excluded = local
            .get(&path)
            .into_iter()
            .flat_map(|node| node.record.excluded.iter())
            .map(|skipped| {
                format!(
                    "{path}: metadata {} excluded: {}",
                    skipped.name, skipped.reason
                )
            })
            .collect::<BTreeSet<_>>();
        let (Some(local), Some(entry)) = (local.get(&path), remote.get(&path)) else {
            report.warnings.extend(excluded);
            report.warnings.push(format!(
                "{path}: metadata deferred until content exists on both sides"
            ));
            continue;
        };
        let remote_path = entry.path.as_str();
        let local_fp = local.record.fingerprint()?;
        let loaded = load_inner(b, remote_path, entry).await?;
        for skipped in loaded
            .as_ref()
            .into_iter()
            .flat_map(|loaded| loaded.record.excluded.iter())
        {
            let message = format!(
                "{path}: metadata {} excluded: {}",
                skipped.name, skipped.reason
            );
            excluded.insert(message);
        }
        report.warnings.extend(excluded);
        let remote_fp = loaded
            .as_ref()
            .map(|loaded| loaded.record.fingerprint())
            .transpose()?;
        let hash = remote_hash(b, remote_path, entry).await?;
        if hash != local.record.content_sha256
            || loaded
                .as_ref()
                .is_some_and(|loaded| loaded.record.content_sha256 != hash)
        {
            report.warnings.push(format!(
                "{path}: metadata deferred until content hashes agree"
            ));
            continue;
        }
        let before = baseline
            .entries
            .get(&path)
            .filter(|pair| pair.content_sha256 == hash);
        let local_empty = local.record.attributes.is_empty();
        let remote_empty = loaded
            .as_ref()
            .is_none_or(|loaded| loaded.record.attributes.is_empty());
        let kind = if let Some(old) = before {
            let lc = old.local != local_fp;
            let rc = old.remote != remote_fp;
            match (lc, rc) {
                (false, false) => "baseline",
                (true, false) if pushes => "push",
                (false, true) if pulls && loaded.is_some() => "pull",
                (true, true) if remote_fp.as_ref() == Some(&local_fp) => "baseline",
                (true, true) => "conflict",
                _ => "deferred",
            }
        } else if remote_fp.as_ref() == Some(&local_fp) || local_empty && remote_empty {
            if loaded.is_none()
                && local_empty
                && baseline
                    .entries
                    .get(&path)
                    .is_some_and(|old| old.had_attributes)
            {
                "uncertain"
            } else {
                "baseline"
            }
        } else if loaded.is_none() || remote_empty && !local_empty {
            if pushes {
                "push"
            } else {
                "deferred"
            }
        } else if local_empty && !remote_empty {
            if pulls {
                "pull"
            } else {
                "deferred"
            }
        } else {
            "conflict"
        };
        match kind{
            "baseline"=>{confirmed.insert(path,Pair{content_sha256:hash,local:local_fp,remote:remote_fp,had_attributes:!local_empty||!remote_empty});},
            "conflict"=>report.conflicts.push(Conflict{path,reason:"both sides have different metadata changes; neither was overwritten".into()}),
            "deferred"=>report.warnings.push(format!("{path}: metadata change is outside direction or remote metadata was removed; previous baseline retained")),
            "uncertain"=>report.warnings.push(format!("{path}: content changed and both metadata sources are empty; prior nonempty metadata baseline retained for explicit resolution")),
            _=>{report.actions.push(Action{path:path.clone(),direction:kind.into(),reason:"selected metadata differs while content hashes agree".into()});pending.push(Planned{path,local:local.record.clone(),remote:loaded,entry:entry.clone(),kind,identity:local.identity.clone()});}
        }
    }
    if !options.apply || !report.conflicts.is_empty() {
        return Ok(report);
    }
    for planned in pending {
        let file = tree.file(&planned.path)?;
        if file_identity(&file)? != planned.identity {
            bail!("local metadata file was replaced after planning");
        }
        let current = metadata::capture_file(&file, policy)?;
        if current.content_sha256 != planned.local.content_sha256
            || current.fingerprint()? != planned.local.fingerprint()?
        {
            bail!("local metadata changed after planning; no metadata overwrite was attempted");
        }
        let local_after = if planned.kind == "push" {
            save_checked(
                b,
                &planned.entry.path,
                &planned.entry,
                &planned.local,
                planned.remote.as_ref(),
            )
            .await?;
            planned.local.clone()
        } else {
            same_file(b, &planned.entry.path, &planned.entry).await?;
            let observed = load_inner(b, &planned.entry.path, &planned.entry)
                .await?
                .context("remote metadata disappeared after planning")?;
            let expected = planned
                .remote
                .as_ref()
                .context("pull has no metadata source")?;
            if !observed.entry.same_revision(&expected.entry) || observed.record != expected.record
            {
                bail!("remote metadata changed after planning; local attributes were preserved");
            }
            let journal = baseline_path
                .parent()
                .context("metadata baseline needs private parent")?
                .join(format!("metadata-recovery-{}.json", uuid::Uuid::new_v4()));
            let result =
                metadata::apply_file_with_recovery(&file, &observed.record, policy, &journal)?;
            for skipped in result.skipped {
                report.warnings.push(format!(
                    "{}: metadata {} skipped: {}",
                    planned.path, skipped.name, skipped.reason
                ));
            }
            metadata::capture_file(&file, policy)?
        };
        let final_remote = load_inner(b, &planned.entry.path, &planned.entry).await?;
        confirmed.insert(
            planned.path,
            Pair {
                content_sha256: local_after.content_sha256.clone(),
                local: local_after.fingerprint()?,
                remote: final_remote
                    .as_ref()
                    .map(|loaded| loaded.record.fingerprint())
                    .transpose()?,
                had_attributes: !local_after.attributes.is_empty()
                    || final_remote
                        .as_ref()
                        .is_some_and(|loaded| !loaded.record.attributes.is_empty()),
            },
        );
    }
    for (path, pair) in confirmed {
        let file = tree.file(&path)?;
        let actual = metadata::capture_file(&file, policy)?;
        if actual.content_sha256 != pair.content_sha256 || actual.fingerprint()? != pair.local {
            bail!("local metadata changed before completion; previous baseline retained");
        }
        baseline.entries.insert(path, pair);
    }
    state::atomic_json(baseline_path, &baseline)?;
    report.applied = true;
    report.baseline_updated = true;
    Ok(report)
}

struct Temporary {
    directory: PathBuf,
    path: PathBuf,
}
impl Temporary {
    fn new() -> Result<Self> {
        let directory = std::env::temp_dir()
            .canonicalize()?
            .join(format!("rdrive-metadata-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&directory)?;
        Ok(Self {
            path: directory.join("data"),
            directory,
        })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}
struct LocalNode {
    record: MetadataRecord,
    identity: String,
}
struct LocalTree {
    path: PathBuf,
    directory: File,
}
impl LocalTree {
    fn open(path: &Path) -> Result<Self> {
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            bail!("metadata sync root cannot be a symlink");
        }
        let path = path.canonicalize()?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
        let directory = options.open(&path)?;
        Ok(Self { path, directory })
    }
    #[cfg(unix)]
    fn directory(&self, path: &str) -> Result<File> {
        let mut current = self.directory.try_clone()?;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let name = std::ffi::CString::new(part)?;
            let fd = unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("metadata path directory is missing or a symlink");
            }
            current = unsafe { File::from_raw_fd(fd) };
        }
        Ok(current)
    }
    #[cfg(not(unix))]
    fn directory(&self, _: &str) -> Result<File> {
        bail!("metadata sync requires Unix no-follow descriptors")
    }
    fn file(&self, path: &str) -> Result<File> {
        if backend::normalize(path)? != path || path.is_empty() {
            bail!("unsafe metadata relative path");
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let directory = self.directory(parent)?;
        #[cfg(unix)]
        {
            let name = std::ffi::CString::new(name)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                bail!("metadata target is not a regular file");
            }
            return Ok(file);
        }
        #[allow(unreachable_code)]
        {
            let _ = (directory, name);
            bail!("metadata sync requires Unix descriptors")
        }
    }
    fn scan(&self, policy: &MetadataPolicy) -> Result<(BTreeMap<String, LocalNode>, Vec<String>)> {
        let mut pending = vec![String::new()];
        let mut result = BTreeMap::new();
        let mut all = Vec::new();
        while let Some(parent) = pending.pop() {
            for name in directory_names(&self.directory(&parent)?)? {
                let path = if parent.is_empty() {
                    name
                } else {
                    format!("{parent}/{name}")
                };
                if is_internal(&path) {
                    continue;
                }
                if backend::normalize(&path)? != path || path.chars().any(char::is_control) {
                    bail!("unsafe metadata filename");
                }
                all.push(path.clone());
                let directory = self.directory(&parent)?;
                #[cfg(unix)]
                {
                    let name = std::ffi::CString::new(path.rsplit('/').next().unwrap())?;
                    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                    if unsafe {
                        libc::fstatat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } != 0
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let stat = unsafe { stat.assume_init() };
                    match stat.st_mode & libc::S_IFMT {
                        libc::S_IFDIR => pending.push(path),
                        libc::S_IFREG => {
                            let file = self.file(&path)?;
                            result.insert(
                                path,
                                LocalNode {
                                    record: metadata::capture_file(&file, policy)?,
                                    identity: file_identity(&file)?,
                                },
                            );
                        }
                        _ => bail!("metadata sync refuses local symlinks and special files"),
                    }
                }
            }
        }
        Ok((result, all))
    }
}
#[cfg(unix)]
fn file_identity(file: &File) -> Result<String> {
    let m = file.metadata()?;
    Ok(format!("{}:{}", m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn file_identity(_: &File) -> Result<String> {
    bail!("unsupported metadata platform")
}
#[cfg(unix)]
fn directory_names(directory: &File) -> Result<Vec<String>> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::last_os_error().into());
    }
    struct Guard(*mut libc::DIR);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let guard = Guard(stream);
    let mut names = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let item = unsafe { libc::readdir(guard.0) };
        if item.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error.into());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*item).d_name.as_ptr()) }
            .to_str()?
            .to_owned();
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    Ok(names)
}
#[cfg(not(unix))]
fn directory_names(_: &File) -> Result<Vec<String>> {
    bail!("unsupported metadata platform")
}
fn is_internal(path: &str) -> bool {
    path.split('/').any(|part| {
        [
            ROOT,
            ".rdrive-versions",
            ".rdrive-transactions",
            ".rdrive-staging",
        ]
        .contains(&part)
            || [".rdrive-partial-", ".rdrive-commit-", ".rdrive-smb-cas-"]
                .iter()
                .any(|prefix| part.starts_with(prefix))
    })
}
async fn remote_files(
    b: &dyn Backend,
    root: &str,
    names: &mut Vec<String>,
) -> Result<BTreeMap<String, Entry>> {
    let own = b
        .stat(root)
        .await?
        .context("remote metadata sync root is missing")?;
    if !own.is_dir {
        bail!("remote metadata sync root must be a directory");
    }
    let mut pending = vec![root.to_owned()];
    let mut result = BTreeMap::new();
    let mut seen = BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent.clone()) {
            bail!("remote metadata directory cycle");
        }
        for entry in b.list(&parent).await? {
            if backend::normalize(&entry.path)? != entry.path
                || entry.path.rsplit_once('/').map(|pair| pair.0).unwrap_or("") != parent
            {
                bail!("server returned unsafe metadata child");
            }
            let relative = if root.is_empty() {
                entry.path.clone()
            } else {
                entry
                    .path
                    .strip_prefix(&format!("{root}/"))
                    .context("metadata child escaped root")?
                    .to_owned()
            };
            if is_internal(&relative) {
                continue;
            }
            names.push(relative.clone());
            if entry.is_dir {
                pending.push(entry.path);
            } else if result.insert(relative, entry).is_some() {
                bail!("duplicate remote metadata file");
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };
    #[derive(Clone)]
    struct Node {
        bytes: Option<Vec<u8>>,
        revision: u64,
    }
    struct Memory {
        nodes: Mutex<BTreeMap<String, Node>>,
        reject_replace: AtomicBool,
    }
    impl Memory {
        fn new() -> Self {
            Self {
                nodes: Mutex::new(BTreeMap::from([
                    (
                        "".into(),
                        Node {
                            bytes: None,
                            revision: 1,
                        },
                    ),
                    (
                        "file".into(),
                        Node {
                            bytes: Some(b"content".to_vec()),
                            revision: 1,
                        },
                    ),
                ])),
                reject_replace: AtomicBool::new(false),
            }
        }
        fn entry(path: &str, node: &Node) -> Entry {
            Entry {
                path: path.into(),
                is_dir: node.bytes.is_none(),
                size: node.bytes.as_ref().map_or(0, |bytes| bytes.len() as u64),
                modified: None,
                etag: Some(format!("\"{}\"", node.revision)),
            }
        }
    }
    #[async_trait]
    impl Backend for Memory {
        fn capabilities(&self) -> crate::backend::Capabilities {
            crate::backend::Capabilities {
                protocol: "memory",
                conditional_write: true,
                server_copy: false,
                locking: false,
                notes: vec![],
            }
        }
        async fn stat(&self, path: &str) -> Result<Option<Entry>> {
            Ok(self
                .nodes
                .lock()
                .unwrap()
                .get(path)
                .map(|node| Self::entry(path, node)))
        }
        async fn list(&self, path: &str) -> Result<Vec<Entry>> {
            Ok(self
                .nodes
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| {
                    key.as_str() != path
                        && key.rsplit_once('/').map(|pair| pair.0).unwrap_or("") == path
                })
                .map(|(key, node)| Self::entry(key, node))
                .collect())
        }
        async fn download(&self, remote: &str, local: &Path) -> Result<()> {
            let bytes = self
                .nodes
                .lock()
                .unwrap()
                .get(remote)
                .and_then(|node| node.bytes.clone())
                .context("missing file")?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(local)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            Ok(())
        }
        async fn upload(
            &self,
            local: &Path,
            remote: &str,
            condition: WriteCondition,
        ) -> Result<Entry> {
            let bytes = fs::read(local)?;
            let mut nodes = self.nodes.lock().unwrap();
            let old = nodes.get(remote);
            match condition {
                WriteCondition::CreateNew if old.is_some() => bail!("412"),
                WriteCondition::Match(entry)
                    if old.is_none_or(|old| !Self::entry(remote, old).same_revision(&entry))
                        || self.reject_replace.swap(false, Ordering::SeqCst) =>
                {
                    bail!("412")
                }
                WriteCondition::Any => bail!("sidecars must never use unconditional overwrite"),
                _ => (),
            };
            let revision = old.map_or(1, |old| old.revision + 1);
            let node = Node {
                bytes: Some(bytes),
                revision,
            };
            let entry = Self::entry(remote, &node);
            nodes.insert(remote.into(), node);
            Ok(entry)
        }
        async fn mkdir(&self, path: &str) -> Result<()> {
            let mut nodes = self.nodes.lock().unwrap();
            if nodes.contains_key(path) {
                bail!("exists");
            }
            nodes.insert(
                path.into(),
                Node {
                    bytes: None,
                    revision: 1,
                },
            );
            Ok(())
        }
        async fn remove(&self, _: &str, _: bool) -> Result<()> {
            bail!("unused")
        }
        async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
            bail!("unused")
        }
        async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
            bail!("unused")
        }
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let local = root.join("local");
        fs::create_dir(&local).unwrap();
        let file = local.join("file");
        fs::write(&file, b"content").unwrap();
        (guard, root, file)
    }
    fn changed(record: &MetadataRecord, value: &str) -> MetadataRecord {
        let mut record = record.clone();
        record.attributes.insert(
            "user.rdrive.tag".into(),
            value
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        );
        record
    }
    #[tokio::test]
    async fn sidecars_reject_wrong_revisions_and_conflicting_replacements() {
        let (_guard, _root, file) = fixture();
        let backend = Memory::new();
        let entry = backend.stat("file").await.unwrap().unwrap();
        let original = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        let mut first = changed(&original, "first");
        first.selection.push("com.example.*".into());
        first
            .attributes
            .insert("com.example.keep".into(), "00ff".into());
        save_record(&backend, "file", &entry, &first).await.unwrap();
        assert_eq!(
            load(&backend, "file", &entry).await.unwrap().unwrap(),
            first
        );
        save_record(&backend, "file", &entry, &changed(&original, "second"))
            .await
            .unwrap();
        let merged = load(&backend, "file", &entry).await.unwrap().unwrap();
        assert_eq!(merged.attributes["com.example.keep"], "00ff");
        assert_eq!(merged.attributes["user.rdrive.tag"], "7365636f6e64");
        assert!(save_record_if_fingerprint(
            &backend,
            "file",
            &entry,
            &changed(&original, "third"),
            Some(&first.fingerprint().unwrap())
        )
        .await
        .is_err());
        assert_eq!(
            load(&backend, "file", &entry).await.unwrap().unwrap(),
            merged
        );
        backend.reject_replace.store(true, Ordering::SeqCst);
        assert!(
            save_record(&backend, "file", &entry, &changed(&original, "third"))
                .await
                .is_err()
        );
        assert_eq!(
            load(&backend, "file", &entry).await.unwrap().unwrap(),
            merged
        );
        backend
            .nodes
            .lock()
            .unwrap()
            .get_mut("file")
            .unwrap()
            .revision += 1;
        assert!(load(&backend, "file", &entry).await.is_err());
    }
    #[tokio::test]
    async fn metadata_only_sync_plans_pushes_and_preserves_both_conflicting_changes() {
        let (_guard, root, file) = fixture();
        let backend = Memory::new();
        let baseline = root.join("baseline.json");
        let local = root.join("local");
        let options = SyncOptions {
            apply: false,
            direction: crate::sync::Direction::Both,
        };
        let plan = sync(&backend, &local, "", &baseline, &options)
            .await
            .unwrap();
        assert!(!plan.baseline_updated);
        assert!(!baseline.exists());
        let apply_options = SyncOptions {
            apply: true,
            ..options
        };
        sync(&backend, &local, "", &baseline, &apply_options)
            .await
            .unwrap();
        let empty = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        metadata::apply(&file, &changed(&empty, "one"), &MetadataPolicy::default()).unwrap();
        let plan = sync(
            &backend,
            &local,
            "",
            &baseline,
            &SyncOptions {
                apply: false,
                direction: crate::sync::Direction::Both,
            },
        )
        .await
        .unwrap();
        assert_eq!(plan.actions[0].direction, "push");
        sync(&backend, &local, "", &baseline, &apply_options)
            .await
            .unwrap();
        let entry = backend.stat("file").await.unwrap().unwrap();
        assert_eq!(
            load(&backend, "file", &entry)
                .await
                .unwrap()
                .unwrap()
                .attributes
                .get("user.rdrive.tag")
                .unwrap(),
            "6f6e65"
        );
        metadata::apply(&file, &changed(&empty, "local"), &MetadataPolicy::default()).unwrap();
        save_record(&backend, "file", &entry, &changed(&empty, "remote"))
            .await
            .unwrap();
        let conflict = sync(&backend, &local, "", &baseline, &apply_options)
            .await
            .unwrap();
        assert_eq!(conflict.conflicts.len(), 1);
        assert!(!conflict.applied);
        assert_eq!(
            metadata::capture(&file, &MetadataPolicy::default())
                .unwrap()
                .attributes["user.rdrive.tag"],
            "6c6f63616c"
        );
        assert_eq!(
            load(&backend, "file", &entry)
                .await
                .unwrap()
                .unwrap()
                .attributes["user.rdrive.tag"],
            "72656d6f7465"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_sources_are_rejected_before_any_metadata_publication() {
        let (_guard, root, file) = fixture();
        fs::remove_file(&file).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"content").unwrap();
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        let backend = Memory::new();
        assert!(sync(
            &backend,
            &root.join("local"),
            "",
            &root.join("baseline.json"),
            &SyncOptions {
                apply: true,
                direction: crate::sync::Direction::Both
            }
        )
        .await
        .is_err());
        assert!(!backend
            .nodes
            .lock()
            .unwrap()
            .keys()
            .any(|name| name.starts_with(ROOT)));
    }
}
