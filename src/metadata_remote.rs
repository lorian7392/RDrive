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
// Selection order and duplicate patterns do not change the metadata snapshot.
fn equivalent_records(left: &MetadataRecord, right: &MetadataRecord) -> bool {
    left.version == right.version
        && left.content_sha256 == right.content_sha256
        && left.attributes == right.attributes
        && left.excluded == right.excluded
        && left.selection.iter().collect::<BTreeSet<_>>()
            == right.selection.iter().collect::<BTreeSet<_>>()
}
async fn save_checked(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    record: &MetadataRecord,
    previous: Option<&Loaded>,
) -> Result<SaveReport> {
    save_checked_with(b, remote, entry, record, previous, state::atomic_json).await
}
async fn save_checked_with(
    b: &dyn Backend,
    remote: &str,
    entry: &Entry,
    record: &MetadataRecord,
    previous: Option<&Loaded>,
    write: impl FnOnce(&Path, &Envelope) -> Result<()>,
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
    if previous.is_some_and(|old| equivalent_records(&old.record, &publication)) {
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
    write(
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
    if !equivalent_records(&loaded.record, &publication) {
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
    record.validate()?;
    let current = load_inner(b, remote, entry).await?;
    if current
        .as_ref()
        .is_some_and(|current| equivalent_records(&current.record, record))
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
        let mut publication = incoming.clone();
        publication.selection.sort();
        publication.selection.dedup();
        return Ok(publication);
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
        .is_some_and(|loaded| equivalent_records(&loaded.record, &publication))
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
    let baseline_abs = crate::state::resolve_location(baseline_path)?;
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
        #[cfg(not(unix))]
        {
            let _ = (directory, name);
            bail!("metadata sync requires Unix descriptors")
        }
    }
    fn scan(&self, policy: &MetadataPolicy) -> Result<(BTreeMap<String, LocalNode>, Vec<String>)> {
        self.scan_with(policy, directory_names)
    }
    fn scan_with(
        &self,
        policy: &MetadataPolicy,
        enumerate: impl Fn(&File) -> Result<Vec<String>>,
    ) -> Result<(BTreeMap<String, LocalNode>, Vec<String>)> {
        let mut pending = vec![String::new()];
        let mut result = BTreeMap::new();
        let mut all = Vec::new();
        while let Some(parent) = pending.pop() {
            for name in enumerate(&self.directory(&parent)?)? {
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
use crate::sync::directory_names;
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
    type Hook = Box<dyn FnOnce(&Memory, &str, &Path) -> Result<()> + Send>;
    struct Memory {
        nodes: Mutex<BTreeMap<String, Node>>,
        reject_replace: AtomicBool,
        stat_overrides: Mutex<BTreeMap<String, Entry>>,
        download_overrides: Mutex<BTreeMap<String, Vec<u8>>>,
        after_download: Mutex<Option<Hook>>,
        after_upload: Mutex<Option<Hook>>,
        hash_override: Mutex<Option<String>>,
        listed: Mutex<Option<Vec<Entry>>>,
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
                stat_overrides: Mutex::new(BTreeMap::new()),
                download_overrides: Mutex::new(BTreeMap::new()),
                after_download: Mutex::new(None),
                after_upload: Mutex::new(None),
                hash_override: Mutex::new(None),
                listed: Mutex::new(None),
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
        fn content_sha256(&self, _: &Entry) -> Option<String> {
            self.hash_override.lock().unwrap().clone()
        }
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
            if let Some(entry) = self.stat_overrides.lock().unwrap().get(path) {
                return Ok(Some(entry.clone()));
            }
            Ok(self
                .nodes
                .lock()
                .unwrap()
                .get(path)
                .map(|node| Self::entry(path, node)))
        }
        async fn list(&self, path: &str) -> Result<Vec<Entry>> {
            if let Some(entries) = self.listed.lock().unwrap().take() {
                return Ok(entries);
            }
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
            let mut bytes = self
                .nodes
                .lock()
                .unwrap()
                .get(remote)
                .and_then(|node| node.bytes.clone())
                .context("missing file")?;
            if let Some(override_bytes) = self.download_overrides.lock().unwrap().get(remote) {
                bytes = override_bytes.clone();
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(local)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            let hook = self.after_download.lock().unwrap().take();
            if let Some(hook) = hook {
                hook(self, remote, local)?;
            }
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
            drop(nodes);
            let hook = self.after_upload.lock().unwrap().take();
            if let Some(hook) = hook {
                hook(self, remote, local)?;
            }
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
        first.selection.sort();
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
    #[tokio::test]
    async fn invalid_or_changed_sidecars_never_apply_partial_local_metadata() {
        let (_guard, _root, file) = fixture();
        let original = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        let entry = Memory::new().stat("file").await.unwrap().unwrap();
        for remote in ["", "wrong"] {
            assert!(sidecar_path(remote, &entry)
                .unwrap_err()
                .to_string()
                .contains("non-root file"));
        }
        let mut directory = entry.clone();
        directory.is_dir = true;
        assert!(sidecar_path("file", &directory).is_err());
        for case in [
            "directory",
            "advertised-size",
            "actual-size",
            "changed",
            "version",
            "path",
            "entry",
        ] {
            let backend = Memory::new();
            let sidecar = sidecar_path("file", &entry).unwrap();
            let mut envelope = Envelope {
                version: 1,
                path: "file".into(),
                entry: entry.clone(),
                record: changed(&original, "remote"),
            };
            match case {
                "version" => envelope.version = 2,
                "path" => envelope.path = "other".into(),
                "entry" => envelope.entry.etag = Some("different".into()),
                _ => (),
            }
            backend.nodes.lock().unwrap().insert(
                sidecar.clone(),
                Node {
                    bytes: Some(serde_json::to_vec(&envelope).unwrap()),
                    revision: 1,
                },
            );
            let expected = match case {
                "directory" => {
                    backend
                        .nodes
                        .lock()
                        .unwrap()
                        .get_mut(&sidecar)
                        .unwrap()
                        .bytes = None;
                    "invalid or oversized"
                }
                "advertised-size" => {
                    let mut entry = backend.stat(&sidecar).await.unwrap().unwrap();
                    entry.size = MAX_SIDECAR + 1;
                    backend
                        .stat_overrides
                        .lock()
                        .unwrap()
                        .insert(sidecar.clone(), entry);
                    "invalid or oversized"
                }
                "actual-size" => {
                    backend
                        .download_overrides
                        .lock()
                        .unwrap()
                        .insert(sidecar.clone(), vec![0; MAX_SIDECAR as usize + 1]);
                    "oversized"
                }
                "changed" => {
                    *backend.after_download.lock().unwrap() = Some(Box::new(|peer, path, _| {
                        peer.nodes.lock().unwrap().get_mut(path).unwrap().revision += 1;
                        Ok(())
                    }));
                    "changed while reading"
                }
                _ => "bound to another",
            };
            assert!(
                apply(&backend, "file", &entry, &file, &MetadataPolicy::default())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected),
                "{case}"
            );
            assert_eq!(
                metadata::capture(&file, &MetadataPolicy::default())
                    .unwrap()
                    .fingerprint()
                    .unwrap(),
                original.fingerprint().unwrap()
            );
        }
    }
    #[tokio::test]
    async fn metadata_source_hash_and_publication_receipt_guards_reject_peer_races() {
        let (_guard, _root, file) = fixture();
        let record = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        let entry = Memory::new().stat("file").await.unwrap().unwrap();
        let backend = Memory::new();
        *backend.hash_override.lock().unwrap() = Some(record.content_sha256.clone());
        assert_eq!(
            remote_hash(&backend, "file", &entry).await.unwrap(),
            record.content_sha256
        );
        *backend.hash_override.lock().unwrap() = None;
        backend
            .download_overrides
            .lock()
            .unwrap()
            .insert("file".into(), b"short".to_vec());
        assert!(remote_hash(&backend, "file", &entry)
            .await
            .unwrap_err()
            .to_string()
            .contains("length changed"));
        backend.download_overrides.lock().unwrap().clear();
        let mut wrong = record.clone();
        wrong.content_sha256 = "0".repeat(64);
        assert!(save_record(&backend, "file", &entry, &wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("source content differs"));
        assert!(merge_record(&record, Some(&wrong))
            .unwrap_err()
            .to_string()
            .contains("different content binding"));
        for remove in [false, true] {
            let backend = Memory::new();
            *backend.after_upload.lock().unwrap() = Some(Box::new(move |peer, path, _| {
                if remove {
                    peer.nodes.lock().unwrap().remove(path);
                } else {
                    let mut nodes = peer.nodes.lock().unwrap();
                    let node = nodes.get_mut(path).unwrap();
                    let mut envelope: Envelope =
                        serde_json::from_slice(node.bytes.as_ref().unwrap())?;
                    envelope.record = changed(&envelope.record, "external");
                    node.bytes = Some(serde_json::to_vec(&envelope)?);
                    node.revision += 1;
                }
                Ok(())
            }));
            let error = save_record(&backend, "file", &entry, &changed(&record, "ours"))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(if remove {
                    "not confirmed"
                } else {
                    "changed before confirmation"
                }),
                "{error:#}"
            );
            assert_eq!(
                backend.nodes.lock().unwrap()["file"].bytes.as_deref(),
                Some(b"content".as_slice())
            );
        }
    }
    #[tokio::test]
    async fn narrow_or_repeated_saves_keep_unselected_attributes_without_republication() {
        let (_guard, _root, file) = fixture();
        let backend = Memory::new();
        let entry = backend.stat("file").await.unwrap().unwrap();
        let mut previous = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        previous.selection = vec!["user.*".into(), "com.example.*".into()];
        previous.attributes = BTreeMap::from([
            ("user.a".into(), "00".into()),
            ("com.example.keep".into(), "01".into()),
        ]);
        previous.excluded = vec![metadata::ExcludedAttribute {
            name: "com.example.omitted".into(),
            reason: "excluded by source policy".into(),
        }];
        save_record(&backend, "file", &entry, &previous)
            .await
            .unwrap();
        assert!(
            !save_record(&backend, "file", &entry, &previous)
                .await
                .unwrap()
                .changed
        );
        assert!(
            !save_record_if_fingerprint(&backend, "file", &entry, &previous, None)
                .await
                .unwrap()
                .changed
        );
        let loaded = load_inner(&backend, "file", &entry).await.unwrap().unwrap();
        previous = loaded.record.clone();
        let mut narrow = previous.clone();
        narrow.selection = vec!["user.*".into()];
        narrow.attributes.remove("com.example.keep");
        narrow.excluded.clear();
        assert!(
            !save_checked(&backend, "file", &entry, &narrow, Some(&loaded))
                .await
                .unwrap()
                .changed
        );
        assert_eq!(
            load(&backend, "file", &entry).await.unwrap().unwrap(),
            previous
        );
        backend
            .nodes
            .lock()
            .unwrap()
            .get_mut(&sidecar_path("file", &entry).unwrap())
            .unwrap()
            .revision += 1;
        assert!(save_checked(
            &backend,
            "file",
            &entry,
            &changed(&narrow, "new"),
            Some(&loaded)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("changed after planning"));
        assert!(save_checked(&backend, "file", &entry, &narrow, None)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn baseline_roots_and_invalid_remote_enumeration_fail_before_metadata_changes() {
        use std::os::unix::fs::symlink;
        let (_guard, root, file) = fixture();
        let local = file.parent().unwrap();
        let backend = Memory::new();
        let options = SyncOptions {
            apply: true,
            direction: crate::sync::Direction::Both,
        };
        let baseline = root.join("baseline");
        assert!(sync(&backend, local, ROOT, &baseline, &options)
            .await
            .unwrap_err()
            .to_string()
            .contains("internal directory"));
        assert!(sync(&backend, local, "", &local.join("baseline"), &options)
            .await
            .unwrap_err()
            .to_string()
            .contains("outside local"));
        fs::create_dir(&baseline).unwrap();
        assert!(sync(&backend, local, "", &baseline, &options)
            .await
            .is_err());
        fs::remove_dir(&baseline).unwrap();
        let mut old = Baseline {
            version: 2,
            local_root: local.to_path_buf(),
            remote_root: String::new(),
            policy: MetadataPolicy::default().include,
            entries: BTreeMap::new(),
        };
        state::atomic_json(&baseline, &old).unwrap();
        assert!(sync(&backend, local, "", &baseline, &options)
            .await
            .unwrap_err()
            .to_string()
            .contains("another root or selection"));
        old.version = 1;
        old.policy = vec!["user.different".into()];
        state::atomic_json(&baseline, &old).unwrap();
        assert!(sync(&backend, local, "", &baseline, &options)
            .await
            .is_err());
        fs::remove_file(&baseline).unwrap();
        let alias = root.join("alias");
        symlink(local, &alias).unwrap();
        assert!(LocalTree::open(&alias)
            .err()
            .unwrap()
            .to_string()
            .contains("cannot be a symlink"));
        let tree = LocalTree::open(local).unwrap();
        assert!(tree.file("").is_err());
        assert!(tree.file("missing/file").is_err());
        assert!(tree.file("missing").is_err());
        fs::create_dir(local.join("directory")).unwrap();
        assert!(tree.file("directory").is_err());
        let own = backend.stat("").await.unwrap().unwrap();
        let file_entry = backend.stat("file").await.unwrap().unwrap();
        for entries in [
            vec![own],
            vec![Entry {
                path: "outside/file".into(),
                ..file_entry.clone()
            }],
            vec![file_entry.clone(), file_entry],
        ] {
            *backend.listed.lock().unwrap() = Some(entries);
            assert!(remote_files(&backend, "", &mut vec![]).await.is_err());
        }
        assert!(remote_files(&backend, "file", &mut vec![])
            .await
            .unwrap_err()
            .to_string()
            .contains("must be a directory"));
    }

    #[tokio::test]
    async fn legacy_unsorted_sidecar_selection_is_an_idempotent_receipt_and_invalid_input_is_rejected(
    ) {
        let (_guard, _root, file) = fixture();
        let backend = Memory::new();
        let entry = backend.stat("file").await.unwrap().unwrap();
        let mut record = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        record.selection = vec!["user.*".into(), "com.example.*".into(), "user.*".into()];
        let path = sidecar_path("file", &entry).unwrap();
        backend.nodes.lock().unwrap().insert(
            path.clone(),
            Node {
                revision: 1,
                bytes: Some(
                    serde_json::to_vec(&Envelope {
                        version: 1,
                        path: "file".into(),
                        entry: entry.clone(),
                        record: record.clone(),
                    })
                    .unwrap(),
                ),
            },
        );
        assert!(
            !save_record_if_fingerprint(&backend, "file", &entry, &record, None)
                .await
                .unwrap()
                .changed
        );
        record.selection.sort();
        record.selection.dedup();
        assert!(
            !save_record(&backend, "file", &entry, &record)
                .await
                .unwrap()
                .changed
        );
        assert_eq!(backend.nodes.lock().unwrap()[&path].revision, 1);
        record.selection = vec!["user.*".into(); 129];
        assert!(save_record(&backend, "file", &entry, &record)
            .await
            .is_err());
        assert_eq!(backend.nodes.lock().unwrap()[&path].revision, 1);
    }

    fn after_nth_download(count: usize, action: Hook) -> Hook {
        Box::new(move |peer, path, local| {
            if count == 1 {
                action(peer, path, local)
            } else {
                *peer.after_download.lock().unwrap() = Some(after_nth_download(count - 1, action));
                Ok(())
            }
        })
    }
    #[tokio::test]
    async fn metadata_sync_pull_directions_baselines_and_empty_sources_keep_explicit_intent() {
        for case in [
            "pull-new",
            "pull-baseline",
            "pull-skipped",
            "push-only",
            "pull-only",
            "local-outside-direction",
            "remote-outside-direction",
            "same-both",
            "different-both",
            "missing-remote",
            "different-content",
        ] {
            let (_guard, root, file) = fixture();
            let backend = Memory::new();
            let local = file.parent().unwrap();
            let baseline = root.join("baseline.json");
            let empty = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
            let entry = backend.stat("file").await.unwrap().unwrap();
            if [
                "pull-baseline",
                "local-outside-direction",
                "remote-outside-direction",
                "same-both",
            ]
            .contains(&case)
            {
                sync(
                    &backend,
                    local,
                    "",
                    &baseline,
                    &SyncOptions {
                        apply: true,
                        direction: crate::sync::Direction::Both,
                    },
                )
                .await
                .unwrap();
            }
            if [
                "pull-only",
                "local-outside-direction",
                "same-both",
                "different-both",
            ]
            .contains(&case)
            {
                metadata::apply(&file, &changed(&empty, "local"), &MetadataPolicy::default())
                    .unwrap();
            }
            if [
                "pull-new",
                "pull-baseline",
                "push-only",
                "remote-outside-direction",
                "same-both",
                "different-both",
            ]
            .contains(&case)
            {
                save_record(
                    &backend,
                    "file",
                    &entry,
                    &changed(
                        &empty,
                        if case == "same-both" {
                            "local"
                        } else {
                            "remote"
                        },
                    ),
                )
                .await
                .unwrap();
            }
            if case == "pull-skipped" {
                let mut record = empty.clone();
                record.selection = vec!["com.example.*".into()];
                record.attributes = BTreeMap::from([("com.example.skip".into(), "01".into())]);
                record.excluded.push(metadata::ExcludedAttribute {
                    name: "security.omitted".into(),
                    reason: "never copied".into(),
                });
                save_record(&backend, "file", &entry, &record)
                    .await
                    .unwrap();
            }
            if case == "missing-remote" {
                backend.nodes.lock().unwrap().remove("file");
            }
            if case == "different-content" {
                backend.nodes.lock().unwrap().get_mut("file").unwrap().bytes =
                    Some(b"different".to_vec());
            }
            let direction = if ["push-only", "remote-outside-direction"].contains(&case) {
                crate::sync::Direction::Push
            } else if ["pull-only", "local-outside-direction"].contains(&case) {
                crate::sync::Direction::Pull
            } else {
                crate::sync::Direction::Both
            };
            let report = sync(
                &backend,
                local,
                "",
                &baseline,
                &SyncOptions {
                    apply: true,
                    direction,
                },
            )
            .await
            .unwrap();
            match case {
                "pull-new" | "pull-baseline" => {
                    assert_eq!(report.actions[0].direction, "pull");
                    assert_eq!(
                        metadata::capture(&file, &MetadataPolicy::default())
                            .unwrap()
                            .attributes["user.rdrive.tag"],
                        "72656d6f7465"
                    );
                }
                "pull-skipped" => {
                    assert!(report
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("skipped")));
                    assert!(!metadata::capture(&file, &MetadataPolicy::default())
                        .unwrap()
                        .attributes
                        .contains_key("com.example.skip"));
                }
                "different-both" => assert_eq!(report.conflicts.len(), 1),
                "same-both" => {
                    assert!(report.actions.is_empty());
                    assert!(report.baseline_updated);
                }
                _ => assert!(!report.warnings.is_empty(), "{case}"),
            }
        }
    }
    #[tokio::test]
    async fn changed_content_with_two_empty_metadata_sources_retains_prior_nonempty_baseline() {
        let (_guard, root, file) = fixture();
        let backend = Memory::new();
        let baseline = root.join("baseline");
        let original = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        metadata::apply(
            &file,
            &changed(&original, "old"),
            &MetadataPolicy::default(),
        )
        .unwrap();
        let options = SyncOptions {
            apply: true,
            direction: crate::sync::Direction::Both,
        };
        sync(&backend, file.parent().unwrap(), "", &baseline, &options)
            .await
            .unwrap();
        let before = fs::read(&baseline).unwrap();
        fs::write(&file, b"new content").unwrap();
        let mut cleared = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        cleared.attributes.clear();
        metadata::apply(&file, &cleared, &MetadataPolicy::default()).unwrap();
        let mut nodes = backend.nodes.lock().unwrap();
        let remote = nodes.get_mut("file").unwrap();
        remote.bytes = Some(b"new content".to_vec());
        remote.revision += 1;
        drop(nodes);
        let report = sync(&backend, file.parent().unwrap(), "", &baseline, &options)
            .await
            .unwrap();
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("both metadata sources are empty")));
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn local_replacement_or_attribute_changes_during_planning_keep_prior_baseline_and_remote()
    {
        for case in ["replacement", "attributes", "completion"] {
            let (_guard, root, file) = fixture();
            let backend = Memory::new();
            let original = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
            if case != "completion" {
                metadata::apply(
                    &file,
                    &changed(&original, "local"),
                    &MetadataPolicy::default(),
                )
                .unwrap();
            }
            let target = file.clone();
            let changed_record = changed(&original, "concurrent");
            let replace = case == "replacement";
            *backend.after_download.lock().unwrap() = Some(Box::new(move |_, _, _| {
                if replace {
                    fs::rename(&target, target.with_extension("previous"))?;
                    fs::write(&target, b"content")?;
                } else {
                    metadata::apply(&target, &changed_record, &MetadataPolicy::default())?;
                }
                Ok(())
            }));
            let baseline = root.join("baseline");
            let error = sync(
                &backend,
                file.parent().unwrap(),
                "",
                &baseline,
                &SyncOptions {
                    apply: true,
                    direction: crate::sync::Direction::Both,
                },
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains(match case {
                    "replacement" => "replaced after planning",
                    "attributes" => "changed after planning",
                    _ => "changed before completion",
                }),
                "{error:#}"
            );
            assert!(!baseline.exists());
            assert_eq!(
                backend.nodes.lock().unwrap()["file"].bytes.as_deref(),
                Some(b"content".as_slice())
            );
        }
    }
    #[tokio::test]
    async fn remote_metadata_changes_between_plan_and_pull_never_overwrite_local_attributes() {
        for remove in [false, true] {
            let (_guard, root, file) = fixture();
            let backend = Memory::new();
            let original = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
            let entry = backend.stat("file").await.unwrap().unwrap();
            save_record(&backend, "file", &entry, &changed(&original, "remote"))
                .await
                .unwrap();
            let sidecar = sidecar_path("file", &entry).unwrap();
            *backend.after_download.lock().unwrap() = Some(after_nth_download(
                2,
                Box::new(move |peer, _, _| {
                    if remove {
                        peer.nodes.lock().unwrap().remove(&sidecar);
                    } else {
                        let mut nodes = peer.nodes.lock().unwrap();
                        let node = nodes.get_mut(&sidecar).unwrap();
                        let mut envelope: Envelope =
                            serde_json::from_slice(node.bytes.as_ref().unwrap())?;
                        envelope.record = changed(&envelope.record, "second writer");
                        node.bytes = Some(serde_json::to_vec(&envelope)?);
                        node.revision += 1;
                    }
                    Ok(())
                }),
            ));
            let baseline = root.join("baseline");
            let error = sync(
                &backend,
                file.parent().unwrap(),
                "",
                &baseline,
                &SyncOptions {
                    apply: true,
                    direction: crate::sync::Direction::Pull,
                },
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains(if remove {
                    "disappeared after planning"
                } else {
                    "changed after planning"
                }),
                "{error:#}"
            );
            assert!(!baseline.exists());
            assert_eq!(
                metadata::capture(&file, &MetadataPolicy::default())
                    .unwrap()
                    .fingerprint()
                    .unwrap(),
                original.fingerprint().unwrap()
            );
        }
    }
    #[tokio::test]
    async fn nested_internal_unsafe_and_colliding_metadata_paths_are_handled_conservatively() {
        let (_guard, root, file) = fixture();
        let backend = Memory::new();
        let local = file.parent().unwrap();
        fs::create_dir(local.join("folder")).unwrap();
        fs::write(local.join("folder/file"), b"content").unwrap();
        fs::create_dir(local.join(ROOT)).unwrap();
        fs::write(local.join(ROOT).join("private"), b"internal").unwrap();
        backend.nodes.lock().unwrap().insert(
            "folder".into(),
            Node {
                bytes: None,
                revision: 1,
            },
        );
        backend.nodes.lock().unwrap().insert(
            "folder/file".into(),
            Node {
                bytes: Some(b"content".to_vec()),
                revision: 1,
            },
        );
        let options = SyncOptions {
            apply: true,
            direction: crate::sync::Direction::Both,
        };
        assert!(
            sync(&backend, local, "", &root.join("baseline"), &options)
                .await
                .unwrap()
                .baseline_updated
        );
        assert_eq!(
            remote_files(&backend, "folder", &mut vec![])
                .await
                .unwrap()
                .len(),
            1
        );
        fs::write(local.join("bad\n"), b"unsafe").unwrap();
        assert!(sync(&backend, local, "", &root.join("baseline"), &options)
            .await
            .unwrap_err()
            .to_string()
            .contains("unsafe metadata filename"));
        fs::remove_file(local.join("bad\n")).unwrap();
        backend.nodes.lock().unwrap().insert(
            "FILE".into(),
            Node {
                bytes: Some(b"content".to_vec()),
                revision: 1,
            },
        );
        assert!(sync(&backend, local, "", &root.join("baseline"), &options)
            .await
            .unwrap_err()
            .to_string()
            .contains("filename collision"));
    }

    #[tokio::test]
    async fn public_save_apply_and_guarded_edit_complete_idempotently() {
        let (_guard, root, file) = fixture();
        let backend = Memory::new();
        let entry = backend.stat("file").await.unwrap().unwrap();
        let policy = MetadataPolicy::default();
        assert!(apply(&backend, "file", &entry, &file, &policy)
            .await
            .unwrap()
            .applied
            .is_empty());
        let empty = metadata::capture(&file, &policy).unwrap();
        metadata::apply(&file, &changed(&empty, "local"), &policy).unwrap();
        assert!(
            save(&backend, &file, "file", &entry, &policy)
                .await
                .unwrap()
                .changed
        );
        let previous = metadata::capture(&file, &policy).unwrap();
        let updated = changed(&previous, "edited");
        assert!(
            save_record_if_fingerprint(
                &backend,
                "file",
                &entry,
                &updated,
                Some(&previous.fingerprint().unwrap())
            )
            .await
            .unwrap()
            .changed
        );
        assert_eq!(
            apply(&backend, "file", &entry, &file, &policy)
                .await
                .unwrap()
                .applied,
            vec!["user.rdrive.tag"]
        );
        assert_eq!(
            metadata::capture(&file, &policy).unwrap().attributes["user.rdrive.tag"],
            "656469746564"
        );
        let options = SyncOptions {
            apply: true,
            direction: crate::sync::Direction::Both,
        };
        let baseline = root.join("baseline");
        sync(&backend, file.parent().unwrap(), "", &baseline, &options)
            .await
            .unwrap();
        let old_baseline = fs::read(&baseline).unwrap();
        let repeat = sync(&backend, file.parent().unwrap(), "", &baseline, &options)
            .await
            .unwrap();
        assert!(repeat.actions.is_empty());
        assert!(repeat.baseline_updated);
        assert_eq!(fs::read(&baseline).unwrap(), old_baseline);
    }

    #[tokio::test]
    async fn failed_staging_does_not_publish_a_metadata_sidecar() {
        let (_guard, _root, file) = fixture();
        let backend = Memory::new();
        let entry = backend.stat("file").await.unwrap().unwrap();
        let record = metadata::capture(&file, &MetadataPolicy::default()).unwrap();
        let error = save_checked_with(&backend, "file", &entry, &record, None, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "synthetic staging disk full",
            )
            .into())
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("staging disk full"));
        assert!(backend
            .stat(&sidecar_path("file", &entry).unwrap())
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            backend.nodes.lock().unwrap()["file"].bytes.as_deref(),
            Some(b"content".as_slice())
        );
    }

    #[cfg(unix)]
    #[test]
    fn disappearing_local_metadata_source_fails_the_scan() {
        let (_guard, _root, file) = fixture();
        let tree = LocalTree::open(file.parent().unwrap()).unwrap();
        let error = tree
            .scan_with(&MetadataPolicy::default(), |directory| {
                let names = directory_names(directory)?;
                fs::remove_file(&file)?;
                Ok(names)
            })
            .err()
            .expect("the disappeared entry must not be treated as an empty source");
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!file.exists());
    }
}
