//! Conservative, baseline-based synchronization. Planning never mutates either tree.
//! Local files replaced or deleted by sync are retained in `.rdrive-versions`.
use crate::backend::{normalize, Backend, Entry, WriteCondition};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const INTERNAL: &str = ".rdrive-versions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Push,
    Pull,
    Both,
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub apply: bool,
    pub direction: Direction,
    pub delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Upload,
    Download,
    CreateLocalDirectory,
    CreateRemoteDirectory,
    DeleteLocal,
    DeleteRemote,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub path: String,
    pub kind: ActionKind,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub applied: bool,
    pub actions: Vec<Action>,
    pub conflicts: Vec<Conflict>,
    pub warnings: Vec<String>,
    pub baseline_updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LocalVersion {
    is_dir: bool,
    size: u64,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pair {
    local: LocalVersion,
    remote: Entry,
}

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    version: u32,
    local_root: String,
    remote_root: String,
    entries: BTreeMap<String, Pair>,
}

#[derive(Clone)]
struct Pending {
    action: Action,
    local: Option<LocalVersion>,
    remote: Option<Entry>,
}

/// Run a complete plan, or apply a conflict-free plan and durably record its baseline.
/// The caller must use a different baseline for each backend/profile and pair of roots.
/// Both roots must already exist. A missing or unreadable remote root is an error,
/// never an empty tree. On any apply error the previous baseline remains intact.
pub async fn run(
    backend: &dyn Backend,
    local: &Path,
    remote: &str,
    baseline_path: &Path,
    options: &SyncOptions,
) -> Result<SyncReport> {
    run_with_metadata(backend, local, remote, baseline_path, options, false).await
}

/// Include revision-bound remote attributes when installing downloaded content.
/// A content-only remote edit preserves the existing selected local attributes.
pub async fn run_with_metadata(
    backend: &dyn Backend,
    local: &Path,
    remote: &str,
    baseline_path: &Path,
    options: &SyncOptions,
    metadata: bool,
) -> Result<SyncReport> {
    let root = LocalRoot::open(local)?;
    let remote = normalize(remote)?;
    if internal(&remote) {
        bail!("the internal version directory cannot be a sync root");
    }
    let baseline_abs = std::path::absolute(baseline_path)?;
    if baseline_abs.starts_with(&root.path) {
        bail!("sync baseline must be stored outside the local sync tree");
    }
    let mut baseline = match fs::read(baseline_path) {
        Ok(bytes) => serde_json::from_slice::<Baseline>(&bytes)
            .context("invalid sync baseline; refusing to guess deletion intent")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Baseline {
            version: 1,
            local_root: root.path.to_string_lossy().into_owned(),
            remote_root: remote.clone(),
            entries: BTreeMap::new(),
        },
        Err(e) => return Err(e).context("cannot read sync baseline"),
    };
    if baseline.version != 1
        || baseline.local_root != root.path.to_string_lossy()
        || baseline.remote_root != remote
    {
        bail!("sync baseline belongs to a different root or unsupported format");
    }
    for (path, pair) in &baseline.entries {
        validate_relative(path)?;
        if internal(path)
            || pair.remote.path != remote_path(&remote, path)
            || pair.local.is_dir != pair.remote.is_dir
            || pair.local.sha256.is_some() == pair.local.is_dir
        {
            bail!("invalid entry in sync baseline: {path}");
        }
    }
    let local_entries = root.scan()?;
    let remote_entries = remote_scan(backend, &remote).await?;
    validate_collisions(
        local_entries
            .keys()
            .chain(remote_entries.keys())
            .chain(baseline.entries.keys()),
    )?;
    let paths: BTreeSet<String> = local_entries
        .keys()
        .chain(remote_entries.keys())
        .chain(baseline.entries.keys())
        .cloned()
        .collect();
    let mut report = SyncReport::default();
    let mut pending = Vec::new();
    let mut confirmed = BTreeMap::<String, Pair>::new();
    let pushes = options.direction != Direction::Pull;
    let pulls = options.direction != Direction::Push;
    for path in paths {
        let l = local_entries.get(&path);
        let r = remote_entries.get(&path);
        let before = baseline.entries.get(&path);
        let mut add = |kind, reason: &str| {
            pending.push(Pending {
                action: Action {
                    path: path.clone(),
                    kind,
                    reason: reason.into(),
                },
                local: l.cloned(),
                remote: r.cloned(),
            })
        };
        match (l, r) {
            (None, None) => {
                baseline.entries.remove(&path);
            }
            (Some(a), Some(b)) if a.is_dir != b.is_dir => report.conflicts.push(Conflict {
                path,
                reason: "file/directory types differ; explicit resolution is required".into(),
            }),
            (Some(a), Some(b)) if a.is_dir => {
                confirmed.insert(
                    path,
                    Pair {
                        local: a.clone(),
                        remote: b.clone(),
                    },
                );
            }
            (Some(a), Some(b)) => {
                if let Some(old) = before {
                    if old.local.is_dir {
                        report.conflicts.push(Conflict {
                            path,
                            reason:
                                "a directory was replaced by files; explicit resolution is required"
                                    .into(),
                        });
                        continue;
                    }
                    let lc = a != &old.local;
                    let rc = !matches_baseline(backend, b, old);
                    match (lc, rc) {
                        (false, false) => { confirmed.insert(path, Pair { local: a.clone(), remote: b.clone() }); }
                        (true, true) => {
                            if verified_same_contents(backend, &root, &path, a, b).await? {
                                confirmed.insert(path, Pair { local: a.clone(), remote: b.clone() });
                            } else {
                                report.conflicts.push(Conflict { path, reason: "both sides changed since the baseline; neither file was overwritten".into() });
                            }
                        }
                        (true, false) if pushes => {
                            if backend.capabilities().conditional_write { add(ActionKind::Upload, "local content changed; remote still matches baseline"); }
                            else { report.conflicts.push(Conflict { path, reason: "backend cannot guard replacement of the remote file".into() }); }
                        }
                        (false, true) if pulls => {
                            if has_revision(b) { add(ActionKind::Download, "remote revision changed; local still matches baseline"); }
                            else { report.conflicts.push(Conflict { path, reason: "remote has no revision token or timestamp to verify a download".into() }); }
                        }
                        _ => report.warnings.push(format!("{path}: change is outside the selected direction; original baseline retained")),
                    }
                } else {
                    if verified_same_contents(backend, &root, &path, a, b).await? {
                        confirmed.insert(
                            path,
                            Pair {
                                local: a.clone(),
                                remote: b.clone(),
                            },
                        );
                    } else {
                        report.conflicts.push(Conflict { path, reason: "file exists on both sides without a common baseline and verified identical contents; explicit resolution is required".into() });
                    }
                }
            }
            (Some(a), None) => {
                if let Some(old) = before {
                    if a != &old.local {
                        report.conflicts.push(Conflict {
                            path,
                            reason: "remote was deleted while local content/type changed".into(),
                        });
                    } else if pulls && options.delete {
                        add(ActionKind::DeleteLocal, "remote deletion; local still matches baseline (local files are retained in .rdrive-versions)");
                    } else {
                        report.warnings.push(format!("{path}: remote deletion not propagated; requires pull/both and --delete"));
                    }
                } else if pushes {
                    add(
                        if a.is_dir {
                            ActionKind::CreateRemoteDirectory
                        } else {
                            ActionKind::Upload
                        },
                        "new local item; remote destination must not exist",
                    );
                } else {
                    report
                        .warnings
                        .push(format!("{path}: new local item is outside pull direction"));
                }
            }
            (None, Some(b)) => {
                if let Some(old) = before {
                    if !matches_baseline(backend, b, old) {
                        report.conflicts.push(Conflict {
                            path,
                            reason: "local was deleted while remote content/type changed".into(),
                        });
                    } else if pushes && options.delete {
                        if b.is_dir {
                            report.warnings.push(format!("{path}: empty remote directory retained; recursive remote directory deletion cannot be guarded safely"));
                        } else {
                            add(
                                ActionKind::DeleteRemote,
                                "local deletion; remote revision must be deleted conditionally",
                            );
                        }
                    } else {
                        report.warnings.push(format!("{path}: local deletion not propagated; requires push/both and --delete"));
                    }
                } else if pulls {
                    if b.is_dir {
                        add(ActionKind::CreateLocalDirectory, "new remote directory");
                    } else if has_revision(b) {
                        add(
                            ActionKind::Download,
                            "new remote file; local destination must not exist",
                        );
                    } else {
                        report.conflicts.push(Conflict {
                            path,
                            reason:
                                "remote has no revision token or timestamp to verify a download"
                                    .into(),
                        });
                    }
                } else {
                    report.warnings.push(format!("{path}: new remote item is outside push direction; first sync does not infer deletion"));
                }
            }
        }
    }
    pending.sort_by(|a, b| operation_order(a).cmp(&operation_order(b)));
    report.actions = pending.iter().map(|p| p.action.clone()).collect();
    if !options.apply || !report.conflicts.is_empty() {
        return Ok(report);
    }
    let mut absent_local = BTreeSet::new();
    let mut absent_remote = BTreeSet::new();
    for p in pending {
        let path = &p.action.path;
        let remote_file = remote_path(&remote, path);
        root.ensure_version(path, p.local.as_ref())?;
        let current = backend
            .stat(&remote_file)
            .await
            .with_context(|| format!("cannot verify remote {path}"))?;
        ensure_remote(&current, p.remote.as_ref())
            .with_context(|| format!("remote changed after planning: {path}"))?;
        match p.action.kind {
            ActionKind::CreateLocalDirectory => {
                root.mkdir(path)?;
                confirmed.insert(
                    path.clone(),
                    Pair {
                        local: root
                            .version(path)?
                            .context("local directory was not created")?,
                        remote: p.remote.context("missing planned remote directory")?,
                    },
                );
            }
            ActionKind::CreateRemoteDirectory => {
                backend.mkdir(&remote_file).await?;
                let r = backend
                    .stat(&remote_file)
                    .await?
                    .filter(|e| e.is_dir)
                    .context("remote directory creation not confirmed")?;
                confirmed.insert(
                    path.clone(),
                    Pair {
                        local: p.local.context("missing planned local directory")?,
                        remote: r,
                    },
                );
            }
            ActionKind::Upload => {
                let expected_local = p.local.as_ref().context("upload has no source")?;
                let stage = root.stage()?;
                root.snapshot(path, expected_local, &stage.path)?;
                let condition = match p.remote {
                    Some(e) => WriteCondition::Match(e),
                    None => WriteCondition::CreateNew,
                };
                let written = backend
                    .upload(&stage.path, &remote_file, condition)
                    .await
                    .with_context(|| {
                        format!("upload failed for {path}; baseline was not advanced")
                    })?;
                root.ensure_version(path, Some(expected_local))?;
                let observed = backend
                    .stat(&remote_file)
                    .await?
                    .context("uploaded file disappeared")?;
                if observed.path != remote_file
                    || observed.is_dir
                    || !observed.same_revision(&written)
                    || observed.size != expected_local.size
                {
                    bail!("uploaded revision could not be confirmed: {path}");
                }
                confirmed.insert(
                    path.clone(),
                    Pair {
                        local: expected_local.clone(),
                        remote: observed,
                    },
                );
            }
            ActionKind::Download => {
                let expected_remote = p.remote.as_ref().context("download has no source")?;
                let stage = root.stage()?;
                backend
                    .download_checked(&remote_file, &stage.path, expected_remote)
                    .await?;
                let downloaded = file_version(&stage.path)?;
                if downloaded.size != expected_remote.size {
                    bail!("downloaded size differs from remote metadata: {path}");
                }
                if backend
                    .content_sha256(expected_remote)
                    .as_ref()
                    .is_some_and(|hash| downloaded.sha256.as_ref() != Some(hash))
                {
                    bail!("downloaded content differs from protected remote fingerprint: {path}");
                }
                let observed = backend.stat(&remote_file).await?;
                ensure_remote(&observed, Some(expected_remote))
                    .context("remote changed while downloading; local file was preserved")?;
                root.ensure_version(path, p.local.as_ref())
                    .context("local changed while downloading; local file was preserved")?;
                let remote_attributes = if metadata {
                    crate::metadata_remote::load(backend, &remote_file, expected_remote).await?
                } else { None };
                let attributes = if let Some(record) = remote_attributes { Some(record) }
                    else if p.local.is_some() {
                        let mut record = root.selected_metadata(path)?;
                        record.content_sha256 = downloaded.sha256.clone().context("download hash missing")?;
                        Some(record)
                    } else { None };
                if let Some(attributes) = attributes {
                    crate::metadata::apply_with_recovery(&stage.path, &attributes, &Default::default(), &stage.directory.join("xattr-recovery.json"))?;
                }
                if let Some(old) = &p.local {
                    let backup = root.archive(path, old)?;
                    report.warnings.push(format!(
                        "{path}: previous local contents retained at {backup}"
                    ));
                }
                root.install_new(&stage.path, path)?;
                confirmed.insert(
                    path.clone(),
                    Pair {
                        local: downloaded,
                        remote: observed.context("download source disappeared")?,
                    },
                );
            }
            ActionKind::DeleteLocal => {
                let old = p.local.as_ref().context("delete has no local source")?;
                if old.is_dir {
                    root.remove_empty_directory(path)?;
                } else {
                    report.warnings.push(format!(
                        "{path}: deleted local file retained at {}",
                        root.archive(path, old)?
                    ));
                }
                baseline.entries.remove(path);
                absent_local.insert(path.clone());
                absent_remote.insert(path.clone());
            }
            ActionKind::DeleteRemote => {
                let old = p.remote.as_ref().context("delete has no remote source")?;
                if old.is_dir {
                    bail!("remote directories cannot be conditionally deleted safely");
                }
                backend.remove_checked(&remote_file, old).await?;
                if backend.stat(&remote_file).await?.is_some() {
                    bail!("remote delete was not confirmed: {path}");
                }
                baseline.entries.remove(path);
                absent_local.insert(path.clone());
                absent_remote.insert(path.clone());
            }
        }
    }
    // Re-enumerate rather than treating an inaccessible or changed tree as success.
    let final_local = root.scan()?;
    let final_remote = remote_scan(backend, &remote).await?;
    validate_collisions(final_local.keys().chain(final_remote.keys()))?;
    for (path, pair) in confirmed {
        if final_local.get(&path) != Some(&pair.local)
            || !final_remote
                .get(&path)
                .is_some_and(|r| remote_equal(r, &pair.remote))
        {
            bail!("{path} changed before sync completion; previous baseline retained");
        }
        baseline.entries.insert(path, pair);
    }
    for path in absent_local {
        if final_local.contains_key(&path) {
            bail!("{path} reappeared locally; previous baseline retained");
        }
    }
    for path in absent_remote {
        if final_remote.contains_key(&path) {
            bail!("{path} reappeared remotely; previous baseline retained");
        }
    }
    crate::state::atomic_json(baseline_path, &baseline)?;
    report.applied = true;
    report.baseline_updated = true;
    Ok(report)
}

fn operation_order(p: &Pending) -> (u8, usize, String) {
    let depth = p.action.path.matches('/').count();
    match p.action.kind {
        ActionKind::CreateLocalDirectory | ActionKind::CreateRemoteDirectory => {
            (0, depth, p.action.path.clone())
        }
        ActionKind::Upload | ActionKind::Download => (1, depth, p.action.path.clone()),
        ActionKind::DeleteLocal | ActionKind::DeleteRemote => {
            (2, usize::MAX - depth, p.action.path.clone())
        }
    }
}
fn remote_path(root: &str, relative: &str) -> String {
    if root.is_empty() {
        relative.into()
    } else {
        format!("{root}/{relative}")
    }
}
fn internal(path: &str) -> bool {
    path.split('/').any(|p| {
        p == INTERNAL || p==".rdrive-metadata" || p==".rdrive-repository"
            || [
                ".rdrive-commit-",
                ".rdrive-upload-",
                ".rdrive-copy-",
                ".rdrive-backup-",
                ".rdrive-partial-",
            ]
            .iter()
            .any(|prefix| {
                p.strip_prefix(*prefix)
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            })
            || recovery_name(p)
            || p.strip_prefix(".rdrive-recovery-done-")
                .and_then(|id| id.strip_suffix(".json"))
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    })
}
fn recovery_name(name: &str) -> bool {
    name.strip_prefix(".rdrive-recovery-")
        .and_then(|id| id.strip_suffix(".json"))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
fn matches_baseline(backend: &dyn Backend, current: &Entry, old: &Pair) -> bool {
    remote_equal(current, &old.remote)
        || (!current.is_dir
            && !old.remote.is_dir
            && old.remote.etag.is_none()
            && backend
                .content_sha256(current)
                .as_ref()
                .is_some_and(|hash| Some(hash) == old.local.sha256.as_ref()))
}
fn has_revision(e: &Entry) -> bool {
    e.etag.is_some() || e.modified.is_some()
}
fn remote_equal(a: &Entry, b: &Entry) -> bool {
    a.is_dir && b.is_dir || a.same_revision(b)
}
fn ensure_remote(actual: &Option<Entry>, expected: Option<&Entry>) -> Result<()> {
    match (actual, expected) {
        (None, None) => Ok(()),
        (Some(a), Some(b)) if a.path == b.path && remote_equal(a, b) => Ok(()),
        _ => bail!("remote entry no longer matches planned revision"),
    }
}
async fn verified_same_contents(
    backend: &dyn Backend,
    root: &LocalRoot,
    path: &str,
    local: &LocalVersion,
    remote: &Entry,
) -> Result<bool> {
    if !has_revision(remote) || local.size != remote.size {
        return Ok(false);
    }
    root.ensure_version(path, Some(local))?;
    ensure_remote(&backend.stat(&remote.path).await?, Some(remote))?;
    let stage = root.stage()?;
    backend
        .download_checked(&remote.path, &stage.path, remote)
        .await?;
    let downloaded = file_version(&stage.path)?;
    if backend
        .content_sha256(remote)
        .as_ref()
        .is_some_and(|hash| downloaded.sha256.as_ref() != Some(hash))
    {
        bail!("downloaded content differs from protected remote fingerprint: {path}");
    }
    ensure_remote(&backend.stat(&remote.path).await?, Some(remote))
        .context("remote changed while comparing existing files")?;
    root.ensure_version(path, Some(local))
        .context("local changed while comparing existing files")?;
    Ok(downloaded == *local)
}
fn validate_relative(path: &str) -> Result<()> {
    if path.is_empty() || normalize(path)? != path {
        bail!("non-canonical remote-relative path: {path:?}");
    }
    for part in path.split('/') {
        if part.chars().any(|c| c.is_control()) {
            bail!("control characters in sync filename");
        }
        if part != INTERNAL && collision_key(part)? == collision_key(INTERNAL)? {
            bail!("filename collides with reserved internal directory");
        }
    }
    Ok(())
}
async fn remote_scan(backend: &dyn Backend, root: &str) -> Result<BTreeMap<String, Entry>> {
    let initial = backend
        .stat(root)
        .await?
        .context("remote sync root does not exist; create it explicitly before syncing")?;
    if !initial.is_dir {
        bail!("remote sync root is not a directory");
    }
    let mut pending = vec![root.to_owned()];
    let mut result = BTreeMap::new();
    while let Some(parent) = pending.pop() {
        for e in backend.list(&parent).await.with_context(|| {
            format!("cannot list remote directory {parent}; no deletions inferred")
        })? {
            if normalize(&e.path)? != e.path
                || e.path.rsplit_once('/').map(|x| x.0).unwrap_or("") != parent
            {
                bail!("server returned unsafe/non-child path: {:?}", e.path);
            }
            let relative = if root.is_empty() {
                e.path.clone()
            } else {
                e.path
                    .strip_prefix(&format!("{root}/"))
                    .context("server returned entry outside sync root")?
                    .to_owned()
            };
            validate_relative(&relative)?;
            if !e.is_dir && relative.rsplit('/').next().is_some_and(recovery_name) {
                backend.check_sync_recovery(&e.path).await.context(
                    "unfinished remote replacement; sync stopped before inferring deletions",
                )?;
            }
            if internal(&relative) {
                continue;
            }
            if e.is_dir {
                pending.push(e.path.clone());
            }
            if result.insert(relative.clone(), e).is_some() {
                bail!("server returned duplicate path: {relative}");
            }
        }
    }
    Ok(result)
}
fn validate_collisions<'a>(paths: impl Iterator<Item = &'a String>) -> Result<()> {
    let mut seen = BTreeMap::new();
    for path in paths {
        let key = collision_key(path)?;
        if let Some(old) = seen.insert(key, path) {
            if old != path {
                bail!("case/canonical filename collision: {old:?} and {path:?}");
            }
        }
    }
    Ok(())
}
pub(crate) fn validate_path_collisions<'a>(paths:impl Iterator<Item=&'a String>)->Result<()> {validate_collisions(paths)}

#[cfg(not(target_os = "macos"))]
pub(crate) fn collision_key(value: &str) -> Result<String> {
    #[cfg(windows)]
    {
        return Ok(value.to_lowercase());
    }
    #[cfg(not(windows))]
    {
        Ok(value.to_owned())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn collision_key(value: &str) -> Result<String> {
    use std::ffi::{c_char, c_void};
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            a: *const c_void,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> *const c_void;
        fn CFStringCreateMutableCopy(
            a: *const c_void,
            capacity: isize,
            source: *const c_void,
        ) -> *mut c_void;
        fn CFStringNormalize(string: *mut c_void, form: isize);
        fn CFStringFold(string: *mut c_void, flags: usize, locale: *const c_void);
        fn CFStringGetLength(string: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }
    const UTF8: u32 = 0x0800_0100;
    unsafe {
        let source = CFStringCreateWithBytes(
            std::ptr::null(),
            value.as_ptr(),
            value.len() as isize,
            UTF8,
            0,
        );
        if source.is_null() {
            bail!("cannot normalize filename");
        }
        let normalized = CFStringCreateMutableCopy(std::ptr::null(), 0, source);
        CFRelease(source);
        if normalized.is_null() {
            bail!("cannot normalize filename");
        }
        CFStringNormalize(normalized, 0); // canonical decomposition (NFD), as used by macOS filesystems
        CFStringFold(normalized, 1, std::ptr::null()); // case insensitive, without stripping accents
        CFStringNormalize(normalized, 0);
        let capacity = CFStringGetMaximumSizeForEncoding(CFStringGetLength(normalized), UTF8) + 1;
        let mut buffer = vec![0u8; capacity as usize];
        let ok = CFStringGetCString(normalized, buffer.as_mut_ptr().cast(), capacity, UTF8);
        CFRelease(normalized);
        if ok == 0 {
            bail!("cannot encode normalized filename");
        }
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        Ok(String::from_utf8(buffer[..end].to_vec())?)
    }
}

fn file_version(path: &Path) -> Result<LocalVersion> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    version_from_file(options.open(path)?)
}
fn version_from_file(mut file: File) -> Result<LocalVersion> {
    let before = file.metadata()?;
    if !before.is_file() {
        bail!("expected regular file");
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        bail!("local file changed while hashing");
    }
    Ok(LocalVersion {
        is_dir: false,
        size: after.len(),
        sha256: Some(format!("{:x}", hasher.finalize())),
    })
}

#[cfg(unix)]
fn open_file_at(directory: &File, name: &std::ffi::CStr) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        bail!("refusing non-regular file");
    }
    Ok(file)
}

#[cfg(unix)]
fn directory_names(directory: &File) -> Result<Vec<String>> {
    use std::{ffi::CStr, os::fd::AsRawFd};
    // A fresh open-file description avoids sharing readdir offsets with the root.
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
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error.into());
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = DirectoryStream(stream);
    let mut result = Vec::new();
    loop {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error.into());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_str()
            .context("non-UTF-8 local filename")?;
        if name != "." && name != ".." {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

/// File descriptors anchor Unix local operations; symlink ancestors cannot
/// redirect commits. Backends receive a genuine path in a private temp directory.
struct LocalRoot {
    path: PathBuf,
    #[cfg(unix)]
    directory: File,
}
struct Stage {
    path: PathBuf,
    directory: PathBuf,
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

impl LocalRoot {
    fn open(path: &Path) -> Result<Self> {
        let meta = fs::symlink_metadata(path).context("local sync root must already exist")?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!("local sync root must be a real directory, not a symlink");
        }
        let path = fs::canonicalize(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&path)?;
            Ok(Self { path, directory })
        }
        #[cfg(not(unix))]
        {
            Ok(Self { path })
        }
    }
    #[cfg(unix)]
    fn directory(&self, relative: &str, create: bool) -> Result<File> {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
        };
        let mut directory = self.directory.try_clone()?;
        for part in relative.split('/').filter(|s| !s.is_empty()) {
            let name = CString::new(part)?;
            let open = || unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            let mut fd = open();
            if fd < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if result != 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                directory.sync_all()?;
                fd = open();
            }
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("refusing missing/symlink directory component {relative:?}")
                });
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        Ok(directory)
    }
    #[cfg(unix)]
    fn parent(&self, path: &str) -> Result<(File, String)> {
        validate_relative(path)?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok((self.directory(parent, false)?, name.into()))
    }
    fn scan(&self) -> Result<BTreeMap<String, LocalVersion>> {
        let mut result = BTreeMap::new();
        let mut pending = vec![String::new()];
        while let Some(relative) = pending.pop() {
            #[cfg(unix)]
            let names = directory_names(&self.directory(&relative, false)?)?;
            #[cfg(not(unix))]
            let names = fs::read_dir(self.checked_path(&relative)?)?
                .map(|e| {
                    e?.file_name()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF-8 local filename"))
                })
                .collect::<Result<Vec<_>>>()?;
            for name in names {
                let path = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                validate_relative(&path)?;
                if internal(&path) {
                    continue;
                }
                let version = self
                    .version(&path)?
                    .context("local file disappeared during scan")?;
                if version.is_dir {
                    pending.push(path.clone());
                }
                result.insert(path, version);
            }
        }
        Ok(result)
    }
    fn version(&self, path: &str) -> Result<Option<LocalVersion>> {
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let (directory, name) = self.parent(path)?;
            let name = CString::new(name)?;
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(e.into());
            }
            let metadata = unsafe { metadata.assume_init() };
            match metadata.st_mode & libc::S_IFMT {
                libc::S_IFDIR => Ok(Some(LocalVersion {
                    is_dir: true,
                    size: 0,
                    sha256: None,
                })),
                libc::S_IFREG => Ok(Some(version_from_file(open_file_at(&directory, &name)?)?)),
                _ => bail!("refusing local symlink/special file: {path}"),
            }
        }
        #[cfg(not(unix))]
        {
            let target = self.checked_path(path)?;
            let metadata = match fs::symlink_metadata(&target) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if metadata.file_type().is_symlink() {
                bail!("refusing local symlink: {path}");
            }
            if metadata.is_dir() {
                return Ok(Some(LocalVersion {
                    is_dir: true,
                    size: 0,
                    sha256: None,
                }));
            }
            Ok(Some(file_version(&target)?))
        }
    }
    fn ensure_version(&self, path: &str, expected: Option<&LocalVersion>) -> Result<()> {
        if self.version(path)?.as_ref() != expected {
            bail!("local changed after planning: {path}");
        }
        Ok(())
    }
    fn stage(&self) -> Result<Stage> {
        let directory = fs::canonicalize(std::env::temp_dir())?
            .join(format!("rdrive-sync-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Stage {
            path: directory.join("content"),
            directory,
        })
    }
    fn snapshot(&self, path: &str, expected: &LocalVersion, destination: &Path) -> Result<()> {
        self.ensure_version(path, Some(expected))?;
        #[cfg(unix)]
        let mut input = {
            let (directory, name) = self.parent(path)?;
            open_file_at(&directory, &std::ffi::CString::new(name)?)?
        };
        #[cfg(not(unix))]
        let mut input = File::open(self.checked_path(path)?)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        if file_version(destination)? != *expected {
            bail!("local changed while preparing upload: {path}");
        }
        self.ensure_version(path, Some(expected))
    }
    fn selected_metadata(&self,path:&str)->Result<crate::metadata::MetadataRecord>{
        #[cfg(unix)] {let (directory,name)=self.parent(path)?;let file=open_file_at(&directory,&std::ffi::CString::new(name)?)?;crate::metadata::capture_file(&file,&Default::default())}
        #[cfg(not(unix))] {crate::metadata::capture(&self.checked_path(path)?,&Default::default())}
    }
    fn mkdir(&self, path: &str) -> Result<()> {
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let (directory, name) = self.parent(path)?;
            let name = CString::new(name)?;
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            directory.sync_all()?;
        }
        #[cfg(not(unix))]
        fs::create_dir(self.checked_path(path)?)?;
        Ok(())
    }
    fn install_new(&self, staged: &Path, path: &str) -> Result<()> {
        #[cfg(unix)]
        {
            use std::{
                ffi::CString,
                os::fd::{AsRawFd, FromRawFd},
            };
            let (directory, name) = self.parent(path)?;
            let temporary = CString::new(format!(".rdrive-commit-{}", uuid::Uuid::new_v4()))?;
            let name = CString::new(name)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let result: Result<()> = (|| {
                let mut output = unsafe { File::from_raw_fd(fd) };
                std::io::copy(&mut File::open(staged)?, &mut output)?;
                let attributes = crate::metadata::capture(staged, &Default::default())?;
                crate::metadata::apply_file(&output, &attributes, &Default::default())?;
                output.sync_all()?;
                if unsafe {
                    libc::linkat(
                        directory.as_raw_fd(),
                        temporary.as_ptr(),
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error()).context(
                        "destination appeared while downloading; existing file was preserved",
                    );
                }
                directory.sync_all()?;
                Ok(())
            })();
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
            }
            result?;
        }
        #[cfg(not(unix))]
        {
            let mut input = File::open(staged)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(self.checked_path(path)?)?;
            std::io::copy(&mut input, &mut output)?;
            let attributes = crate::metadata::capture(staged, &Default::default())?;
            crate::metadata::apply_file(&output, &attributes, &Default::default())?;
            output.sync_all()?;
        }
        Ok(())
    }
    fn archive(&self, path: &str, expected: &LocalVersion) -> Result<String> {
        self.ensure_version(path, Some(expected))?;
        let backup = format!("{INTERNAL}/{}/original", uuid::Uuid::new_v4());
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let (source_dir, name) = self.parent(path)?;
            let backup_dir = self.directory(backup.rsplit_once('/').unwrap().0, true)?;
            let source = CString::new(name)?;
            let target = c"original";
            if unsafe {
                libc::renameat(
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    backup_dir.as_raw_fd(),
                    target.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            source_dir.sync_all()?;
            backup_dir.sync_all()?;
            let observed = open_file_at(&backup_dir, target).and_then(|file| {
                file.sync_all()?;
                version_from_file(file)
            });
            if !observed.is_ok_and(|version| version == *expected) {
                unsafe {
                    libc::linkat(
                        backup_dir.as_raw_fd(),
                        target.as_ptr(),
                        source_dir.as_raw_fd(),
                        source.as_ptr(),
                        0,
                    );
                }
                bail!("local changed during commit; preserved original at {backup}");
            }
        }
        #[cfg(not(unix))]
        {
            let source = self.checked_path(path)?;
            let target = self.path.join(&backup);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::rename(&source, &target)?;
            if file_version(&target)? != *expected {
                let _ = fs::hard_link(&target, &source);
                bail!("local changed during commit; preserved original at {backup}");
            }
        }
        Ok(backup)
    }
    fn remove_empty_directory(&self, path: &str) -> Result<()> {
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd};
            let (directory, name) = self.parent(path)?;
            let name = CString::new(name)?;
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(std::io::Error::last_os_error()).context(
                    "local directory contains new/untracked items; refusing recursive deletion",
                );
            }
            directory.sync_all()?;
        }
        #[cfg(not(unix))]
        fs::remove_dir(self.checked_path(path)?)
            .context("local directory contains new/untracked items; refusing recursive deletion")?;
        Ok(())
    }
    #[cfg(not(unix))]
    fn checked_path(&self, relative: &str) -> Result<PathBuf> {
        let mut path = self.path.clone();
        for part in relative.split('/').filter(|p| !p.is_empty()) {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => bail!("refusing symlink traversal"),
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Capabilities;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Node {
        bytes: Option<Vec<u8>>,
        revision: u64,
    }
    enum DuringDownload {
        ChangeRemote(String, Vec<u8>),
        ChangeLocal(PathBuf, Vec<u8>),
    }
    struct MemoryBackend {
        nodes: Mutex<BTreeMap<String, Node>>,
        fail_list: Mutex<Option<String>>,
        injected_entry: Mutex<Option<Entry>>,
        during_download: Mutex<Option<DuringDownload>>,
        conditional: bool,
        conditional_delete: bool,
        unfinished_receipt: Mutex<bool>,
    }
    impl MemoryBackend {
        fn new() -> Self {
            Self {
                nodes: Mutex::new(BTreeMap::new()),
                fail_list: Mutex::new(None),
                injected_entry: Mutex::new(None),
                during_download: Mutex::new(None),
                conditional: true,
                conditional_delete: true,
                unfinished_receipt: Mutex::new(false),
            }
        }
        fn file(&self, path: &str, bytes: &[u8]) {
            let mut nodes = self.nodes.lock().unwrap();
            let revision = nodes.get(path).map_or(1, |n| n.revision + 1);
            nodes.insert(
                path.into(),
                Node {
                    bytes: Some(bytes.to_vec()),
                    revision,
                },
            );
        }
        fn dir(&self, path: &str) {
            self.nodes.lock().unwrap().insert(
                path.into(),
                Node {
                    bytes: None,
                    revision: 1,
                },
            );
        }
        fn bytes(&self, path: &str) -> Vec<u8> {
            self.nodes
                .lock()
                .unwrap()
                .get(path)
                .unwrap()
                .bytes
                .clone()
                .unwrap()
        }
        fn entry(path: &str, node: &Node) -> Entry {
            Entry {
                path: path.into(),
                is_dir: node.bytes.is_none(),
                size: node.bytes.as_ref().map_or(0, |v| v.len() as u64),
                modified: Some(node.revision as i64),
                etag: Some(format!("\"{}\"", node.revision)),
            }
        }
    }
    #[async_trait]
    impl Backend for MemoryBackend {
        fn content_sha256(&self, entry: &Entry) -> Option<String> {
            let nodes = self.nodes.lock().unwrap();
            let node = nodes.get(&entry.path)?;
            if !Self::entry(&entry.path, node).same_revision(entry) {
                return None;
            }
            node.bytes
                .as_ref()
                .map(|b| format!("{:x}", Sha256::digest(b)))
        }
        async fn check_sync_recovery(&self, _: &str) -> Result<()> {
            if *self.unfinished_receipt.lock().unwrap() {
                bail!("unresolved replacement receipt");
            }
            Ok(())
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                protocol: "mock",
                conditional_write: self.conditional,
                server_copy: false,
                locking: false,
                notes: vec![],
            }
        }
        async fn stat(&self, path: &str) -> Result<Option<Entry>> {
            if path.is_empty() {
                return Ok(Some(Entry {
                    path: "".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    etag: None,
                }));
            }
            Ok(self
                .nodes
                .lock()
                .unwrap()
                .get(path)
                .map(|n| Self::entry(path, n)))
        }
        async fn list(&self, path: &str) -> Result<Vec<Entry>> {
            if self.fail_list.lock().unwrap().as_deref() == Some(path) {
                bail!("listing denied");
            }
            if let Some(entry) = self.injected_entry.lock().unwrap().take() {
                return Ok(vec![entry]);
            }
            Ok(self
                .nodes
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p.rsplit_once('/').map(|x| x.0).unwrap_or("") == path)
                .map(|(p, n)| Self::entry(p, n))
                .collect())
        }
        async fn download(&self, remote: &str, local: &Path) -> Result<()> {
            fs::write(local, self.bytes(remote))?;
            let mutation = self.during_download.lock().unwrap().take();
            match mutation {
                Some(DuringDownload::ChangeRemote(p, bytes)) => self.file(&p, &bytes),
                Some(DuringDownload::ChangeLocal(p, bytes)) => fs::write(p, bytes)?,
                None => (),
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
            let existing = nodes.get(remote);
            match condition {
                WriteCondition::CreateNew if existing.is_some() => bail!("destination exists"),
                WriteCondition::Match(ref expected)
                    if !self.conditional
                        || !existing
                            .is_some_and(|n| Self::entry(remote, n).same_revision(expected)) =>
                {
                    bail!("revision mismatch or unsupported guard")
                }
                WriteCondition::Any => bail!("sync must not use unguarded writes"),
                _ => (),
            }
            let node = Node {
                bytes: Some(bytes),
                revision: existing.map_or(1, |n| n.revision + 1),
            };
            let result = Self::entry(remote, &node);
            nodes.insert(remote.into(), node);
            Ok(result)
        }
        async fn mkdir(&self, path: &str) -> Result<()> {
            if self.nodes.lock().unwrap().contains_key(path) {
                bail!("directory already exists");
            }
            self.dir(path);
            Ok(())
        }
        async fn remove(&self, _: &str, _: bool) -> Result<()> {
            bail!("sync must not use unguarded removal")
        }
        async fn remove_checked(&self, path: &str, expected: &Entry) -> Result<()> {
            if !self.conditional_delete {
                bail!("conditional delete unsupported");
            }
            let mut nodes = self.nodes.lock().unwrap();
            if !nodes
                .get(path)
                .is_some_and(|n| Self::entry(path, n).same_revision(expected))
            {
                bail!("delete revision mismatch");
            }
            nodes.remove(path);
            Ok(())
        }
        async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
            bail!("unexpected rename")
        }
        async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
            bail!("unexpected copy")
        }
    }
    fn options(direction: Direction, delete: bool) -> SyncOptions {
        SyncOptions {
            apply: true,
            direction,
            delete,
        }
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        fs::create_dir(&local).unwrap();
        let baseline = temp.path().join("state/baseline.json");
        (temp, local, baseline)
    }
    async fn seed(backend: &MemoryBackend, local: &Path, baseline: &Path) {
        fs::write(local.join("a.txt"), b"first").unwrap();
        let r = run(
            backend,
            local,
            "",
            baseline,
            &options(Direction::Push, false),
        )
        .await
        .unwrap();
        assert!(r.baseline_updated);
    }
    #[tokio::test]
    async fn plan_is_read_only_and_apply_recurses_in_both_directions() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::create_dir(local.join("localdir")).unwrap();
        fs::write(local.join("localdir/a"), b"local").unwrap();
        backend.dir("remotedir");
        backend.file("remotedir/b", b"remote");
        let plan = run(
            &backend,
            &local,
            "",
            &baseline,
            &SyncOptions {
                apply: false,
                direction: Direction::Both,
                delete: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(plan.actions.len(), 4);
        assert!(!plan.applied);
        assert!(!baseline.exists());
        assert!(!local.join("remotedir").exists());
        assert!(backend.stat("localdir").await.unwrap().is_none());
        let applied = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true),
        )
        .await
        .unwrap();
        assert!(applied.conflicts.is_empty());
        assert!(applied.baseline_updated);
        assert_eq!(backend.bytes("localdir/a"), b"local");
        assert_eq!(fs::read(local.join("remotedir/b")).unwrap(), b"remote");
        let unchanged = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true),
        )
        .await
        .unwrap();
        assert!(unchanged.actions.is_empty());
    }
    #[tokio::test]
    async fn same_size_local_edits_are_found_by_sha256() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        fs::write(local.join("a.txt"), b"other").unwrap();
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, false),
        )
        .await
        .unwrap();
        assert_eq!(r.actions[0].kind, ActionKind::Upload);
        assert_eq!(backend.bytes("a.txt"), b"other");
    }
    #[tokio::test]
    async fn identical_preexisting_files_and_interrupted_uploads_adopt_baselines() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::write(local.join("a.txt"), b"identical").unwrap();
        backend.file("a.txt", b"identical");
        let plan = run(
            &backend,
            &local,
            "",
            &baseline,
            &SyncOptions {
                apply: false,
                direction: Direction::Both,
                delete: false,
            },
        )
        .await
        .unwrap();
        assert!(plan.conflicts.is_empty());
        assert!(plan.actions.is_empty());
        assert!(!baseline.exists());
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        fs::write(local.join("a.txt"), b"same new contents").unwrap();
        backend.file("a.txt", b"same new contents");
        let recovered = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        assert!(recovered.conflicts.is_empty());
        assert!(recovered.actions.is_empty());
        assert!(recovered.baseline_updated);
    }
    #[tokio::test]
    async fn no_common_baseline_and_dual_edits_preserve_both_sides() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::write(local.join("a.txt"), b"local").unwrap();
        backend.file("a.txt", b"remote");
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        assert_eq!(r.conflicts.len(), 1);
        assert!(!r.applied);
        assert!(!baseline.exists());
        assert_eq!(backend.bytes("a.txt"), b"remote");
        assert_eq!(fs::read(local.join("a.txt")).unwrap(), b"local");
        backend.nodes.lock().unwrap().remove("a.txt");
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, false),
        )
        .await
        .unwrap();
        let before = fs::read(&baseline).unwrap();
        fs::write(local.join("a.txt"), b"second local").unwrap();
        backend.file("a.txt", b"second remote");
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn deletion_requires_opt_in_and_skipping_retains_deletion_intent() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        fs::remove_file(local.join("a.txt")).unwrap();
        let skipped = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        assert!(skipped.actions.is_empty());
        assert_eq!(backend.bytes("a.txt"), b"first");
        let deleted = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true),
        )
        .await
        .unwrap();
        assert_eq!(deleted.actions[0].kind, ActionKind::DeleteRemote);
        assert!(backend.stat("a.txt").await.unwrap().is_none());
    }
    #[tokio::test]
    async fn first_sync_never_treats_missing_side_as_deletion() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        backend.file("remote", b"keep");
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, true),
        )
        .await
        .unwrap();
        assert!(r.actions.is_empty());
        assert_eq!(backend.bytes("remote"), b"keep");
    }
    #[tokio::test]
    async fn inaccessible_remote_listing_is_not_an_empty_tree() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        let before = fs::read(&baseline).unwrap();
        *backend.fail_list.lock().unwrap() = Some("".into());
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true)
        )
        .await
        .is_err());
        assert_eq!(fs::read(local.join("a.txt")).unwrap(), b"first");
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn missing_remote_root_is_not_an_empty_tree() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::write(local.join("keep"), b"safe").unwrap();
        assert!(run(
            &backend,
            &local,
            "missing",
            &baseline,
            &options(Direction::Both, true)
        )
        .await
        .is_err());
        assert!(!baseline.exists());
    }
    #[tokio::test]
    async fn unsupported_conditional_replacement_is_a_conflict() {
        let (_temp, local, baseline) = fixture();
        let mut backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        backend.conditional = false;
        fs::write(local.join("a.txt"), b"edited").unwrap();
        let before = fs::read(&baseline).unwrap();
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, false),
        )
        .await
        .unwrap();
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(backend.bytes("a.txt"), b"first");
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn unsupported_conditional_delete_preserves_remote_and_baseline() {
        let (_temp, local, baseline) = fixture();
        let mut backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        backend.conditional_delete = false;
        fs::remove_file(local.join("a.txt")).unwrap();
        let before = fs::read(&baseline).unwrap();
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, true)
        )
        .await
        .is_err());
        assert_eq!(backend.bytes("a.txt"), b"first");
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn remote_mutation_during_download_never_replaces_local() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        backend.file("a.txt", b"remote 2");
        let before = fs::read(&baseline).unwrap();
        *backend.during_download.lock().unwrap() = Some(DuringDownload::ChangeRemote(
            "a.txt".into(),
            b"remote 3".to_vec(),
        ));
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Pull, false)
        )
        .await
        .is_err());
        assert_eq!(fs::read(local.join("a.txt")).unwrap(), b"first");
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn local_edit_during_download_never_gets_overwritten() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        backend.file("a.txt", b"remote 2");
        let before = fs::read(&baseline).unwrap();
        *backend.during_download.lock().unwrap() = Some(DuringDownload::ChangeLocal(
            local.join("a.txt"),
            b"local edit during download".to_vec(),
        ));
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Pull, false)
        )
        .await
        .is_err());
        assert_eq!(
            fs::read(local.join("a.txt")).unwrap(),
            b"local edit during download"
        );
        assert_eq!(fs::read(&baseline).unwrap(), before);
    }
    #[tokio::test]
    async fn local_replacements_and_deletes_have_recoverable_originals() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        seed(&backend, &local, &baseline).await;
        backend.file("a.txt", b"remote update");
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Pull, false),
        )
        .await
        .unwrap();
        assert_eq!(fs::read(local.join("a.txt")).unwrap(), b"remote update");
        let originals: Vec<_> = fs::read_dir(local.join(INTERNAL))
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path().join("original");
                fs::read(p).ok()
            })
            .collect();
        assert!(originals.contains(&b"first".to_vec()));
        backend.nodes.lock().unwrap().remove("a.txt");
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Pull, true),
        )
        .await
        .unwrap();
        assert!(!local.join("a.txt").exists());
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true)
        )
        .await
        .unwrap()
        .actions
        .is_empty());
    }
    #[tokio::test]
    async fn version_directories_are_excluded_before_remote_recursion() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::create_dir(local.join(INTERNAL)).unwrap();
        fs::write(local.join(INTERNAL).join("saved"), b"do not sync").unwrap();
        for prefix in [".rdrive-commit-", ".rdrive-upload-", ".rdrive-copy-"] {
            fs::write(
                local.join(format!("{prefix}{}", uuid::Uuid::new_v4())),
                b"partial local",
            )
            .unwrap();
            backend.file(
                &format!("{prefix}{}", uuid::Uuid::new_v4()),
                b"partial remote",
            );
        }
        backend.dir(INTERNAL);
        *backend.fail_list.lock().unwrap() = Some(INTERNAL.into());
        let r = run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, false),
        )
        .await
        .unwrap();
        assert!(r.actions.is_empty());
        assert!(r.conflicts.is_empty());
    }
    #[tokio::test]
    async fn unfinished_replacement_is_never_inferred_as_user_deletion() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::write(local.join("file"), b"original").unwrap();
        backend.file("file", b"original");
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true),
        )
        .await
        .unwrap();
        let previous = fs::read(&baseline).unwrap();
        backend.nodes.lock().unwrap().remove("file");
        backend.file(
            &format!(".rdrive-recovery-{}.json", uuid::Uuid::new_v4()),
            b"receipt",
        );
        *backend.unfinished_receipt.lock().unwrap() = true;
        let mut opts = options(Direction::Both, true);
        opts.delete = true;
        assert!(run(&backend, &local, "", &baseline, &opts)
            .await
            .unwrap_err()
            .to_string()
            .contains("unfinished"));
        assert_eq!(fs::read(local.join("file")).unwrap(), b"original");
        assert_eq!(fs::read(baseline).unwrap(), previous);
    }
    #[tokio::test]
    async fn legacy_smb_baseline_upgrades_only_when_current_content_matches_old_hash() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        fs::write(local.join("file"), b"old").unwrap();
        backend.file("file", b"old");
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true),
        )
        .await
        .unwrap();
        let mut old: Baseline = serde_json::from_slice(&fs::read(&baseline).unwrap()).unwrap();
        old.entries.get_mut("file").unwrap().remote.etag = None;
        crate::state::atomic_json(&baseline, &old).unwrap();
        fs::write(local.join("file"), b"new").unwrap();
        run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, true),
        )
        .await
        .unwrap();
        assert_eq!(backend.bytes("file"), b"new");
    }
    #[tokio::test]
    async fn malicious_remote_path_is_rejected_before_any_apply() {
        let (_temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        *backend.injected_entry.lock().unwrap() = Some(Entry {
            path: "../escape".into(),
            is_dir: false,
            size: 1,
            modified: Some(1),
            etag: None,
        });
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Both, true)
        )
        .await
        .is_err());
        assert!(!baseline.exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn local_symlinks_are_rejected_and_anchored_root_survives_parent_swap() {
        use std::os::unix::fs::symlink;
        let (temp, local, baseline) = fixture();
        let backend = MemoryBackend::new();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        symlink(&outside, local.join("link")).unwrap();
        assert!(run(
            &backend,
            &local,
            "",
            &baseline,
            &options(Direction::Push, false)
        )
        .await
        .is_err());
        fs::remove_file(local.join("link")).unwrap();
        let root = LocalRoot::open(&local).unwrap();
        let moved = temp.path().join("moved");
        fs::rename(&local, &moved).unwrap();
        symlink(&outside, &local).unwrap();
        root.mkdir("created").unwrap();
        assert!(moved.join("created").is_dir());
        assert!(!outside.join("created").exists());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn canonical_and_case_collisions_are_detected() {
        assert_eq!(
            collision_key("Caf\u{e9}").unwrap(),
            collision_key("CAFE\u{301}").unwrap()
        );
        let paths = ["folder/Caf\u{e9}".into(), "folder/CAFE\u{301}".into()];
        assert!(validate_collisions(paths.iter()).is_err());
    }
}
