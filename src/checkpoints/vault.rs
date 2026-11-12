//! Capacity-bounded APFS file checkpoints. This vault never creates or deletes
//! Time Machine or native volume snapshots and never rolls a state database back.
use super::{digest, directory, id, private, publish_payload, queue_leases, relative, Content};
use crate::state::{self, Store};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub mod system;

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_records: usize,
    pub auto_prune: bool,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        if self.max_bytes < 256 * 1024 * 1024 || self.max_bytes % 512 != 0 {
            bail!("vault capacity must be at least 256 MiB and a multiple of 512 bytes");
        }
        if !(1..=4096).contains(&self.max_records) {
            bail!("vault record limit must be between 1 and 4096");
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub(super) struct Mounted {
    pub root: PathBuf,
    pub instance_id: String,
    pub volume_uuid: String,
    pub capacity_bytes: u64,
    pub home_binding: String,
    pub device: String,
    pub readonly: bool,
}
#[derive(Debug, Clone)]
pub(super) struct Space {
    pub used_bytes: u64,
    pub available_bytes: u64,
}
pub(super) trait Host {
    fn mount(
        &mut self,
        home: &Path,
        max_bytes: u64,
        initialize: bool,
        readonly: bool,
    ) -> Result<Option<Mounted>>;
    fn space(&mut self, mount: &Mounted) -> Result<Space>;
    fn unmount(&mut self, mount: &Mounted) -> Result<()>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    id: String,
    created: String,
    sequence: u64,
    home: String,
    instance_id: String,
    volume_uuid: String,
    files: BTreeMap<PathBuf, Content>,
}
#[derive(Debug)]
struct Source {
    path: PathBuf,
    content: Content,
}
fn binding(home: &Path) -> Result<String> {
    Ok(state::key(&home.canonicalize()?.to_string_lossy()))
}
fn checked_file(path: &Path) -> Result<File> {
    checked_file_with(path, |_| Ok(()))
}
fn checked_file_with(path: &Path, opened: impl FnOnce(&File) -> Result<()>) -> Result<File> {
    private(path, false)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    opened(&file)?;
    let a = fs::symlink_metadata(path)?;
    let b = file.metadata()?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        bail!("vault file changed while it was opened");
    }
    Ok(file)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = checked_file(path)?;
    if file.metadata()?.len() > 128 * 1024 * 1024 {
        bail!("vault manifest exceeds its size bound");
    }
    Ok(serde_json::from_reader(file)?)
}
const LAYOUT_MARKER: &str = ".rdrive-checkpoint-layout.json";
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LayoutIdentity {
    version: u32,
    home: String,
    instance_id: String,
    volume_uuid: String,
}
fn layout(mount: &Mounted, home: &Path) -> Result<()> {
    private(&mount.root, true)?;
    id(&mount.instance_id)?;
    uuid::Uuid::parse_str(&mount.volume_uuid)?;
    if mount.device.is_empty() || mount.home_binding != binding(home)? {
        bail!("vault belongs to another state directory");
    }
    let expected = LayoutIdentity {
        version: 1,
        home: mount.home_binding.clone(),
        instance_id: mount.instance_id.clone(),
        volume_uuid: mount.volume_uuid.clone(),
    };
    let marker = mount.root.join(LAYOUT_MARKER);
    let committed = match fs::symlink_metadata(&marker) {
        Ok(info) => {
            if info.len() > 16 * 1024 {
                bail!("vault layout marker exceeds its size bound");
            }
            let actual: LayoutIdentity = read_json(&marker)?;
            if actual != expected {
                bail!("vault layout marker identifies a different store");
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !committed && mount.readonly {
        bail!("APFS vault file layout is incomplete; writable reconciliation is required");
    }
    let device = fs::metadata(&mount.root)?.dev();
    for name in ["blobs", "checkpoints", "staging"] {
        let path = mount.root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                private(&path, true)?;
                same_device(fs::metadata(&path)?.dev(), device)?;
                if !committed && fs::read_dir(&path)?.next().is_some() {
                    bail!("unrecognized contents precede the vault layout marker; nothing was removed");
                }
            }
            Err(error) if !committed && error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    if !committed {
        for name in ["blobs", "checkpoints", "staging"] {
            directory(&mount.root.join(name))?;
        }
        File::open(&mount.root)?.sync_all()?;
        state::atomic_json(&marker, &expected)?;
    }
    Ok(())
}

fn same_device(actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        bail!("vault entries cross a filesystem boundary");
    }
    Ok(())
}
fn manifest(mount: &Mounted, value: Manifest) -> Result<Manifest> {
    id(&value.id)?;
    chrono::DateTime::parse_from_rfc3339(&value.created)?;
    if value.version != 1
        || value.sequence == 0
        || value.home != mount.home_binding
        || value.instance_id != mount.instance_id
        || value.volume_uuid != mount.volume_uuid
        || value.files.len() > 1_000_000
    {
        bail!("invalid vault checkpoint identity or scope");
    }
    for (path, content) in &value.files {
        relative(path)?;
        if !super::exportable(path)
            || !content.exportable
            || content.kind != super::kind(path)
            || !digest(&content.sha256)
        {
            bail!("vault manifest contains an invalid payload");
        }
    }
    logical_bytes(&value)?;
    Ok(value)
}
fn logical_bytes(value: &Manifest) -> Result<u64> {
    value.files.values().try_fold(0u64, |sum, content| {
        sum.checked_add(content.bytes)
            .context("vault byte count overflow")
    })
}
fn load(mount: &Mounted, checkpoint: &str) -> Result<Manifest> {
    id(checkpoint)?;
    let value = manifest(
        mount,
        read_json(
            &mount
                .root
                .join("checkpoints")
                .join(format!("{checkpoint}.json")),
        )?,
    )?;
    if value.id != checkpoint {
        bail!("vault record filename differs from its checkpoint identity");
    }
    Ok(value)
}
fn rows(mount: &Mounted) -> Result<Vec<Manifest>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(mount.root.join("checkpoints"))? {
        let entry = entry?;
        let name = entry.file_name();
        let value = name
            .to_str()
            .and_then(|v| v.strip_suffix(".json"))
            .context("unrecognized file in owned checkpoint namespace")?;
        result.push(load(mount, value)?);
    }
    result.sort_by(|a, b| b.sequence.cmp(&a.sequence).then_with(|| b.id.cmp(&a.id)));
    if result.windows(2).any(|w| w[0].sequence == w[1].sequence) {
        bail!("ambiguous vault checkpoint sequence");
    }
    Ok(result)
}
fn preparing_files(path: &Path) -> Result<Vec<PathBuf>> {
    private(path, true)?;
    let mut temporary = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid preparing-stage filename"))?;
        if !atomic_temporary(&name) {
            bail!("unrecognized content in a preparing checkpoint was retained");
        }
        private(&entry.path(), false)?;
        temporary.push(entry.path());
    }
    Ok(temporary)
}
fn staged(mount: &Mounted, checkpoint: &str) -> Result<Option<Manifest>> {
    id(checkpoint)?;
    let path = mount.root.join("staging").join(checkpoint);
    private(&path, true)?;
    if !path.join("intent.json").try_exists()? {
        preparing_files(&path)?;
        return Ok(None);
    }
    let value = manifest(mount, read_json(&path.join("intent.json"))?)?;
    if value.id != checkpoint {
        bail!("staging identity differs from its manifest");
    }
    validate_stage(mount, &value)?;
    Ok(Some(value))
}
fn stages(mount: &Mounted) -> Result<Vec<Manifest>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(mount.root.join("staging"))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid vault staging name"))?;
        if let Some(value) = staged(mount, &name)? {
            result.push(value);
        }
    }
    Ok(result)
}
fn atomic_temporary(name: &str) -> bool {
    name.strip_prefix(".rdrive-")
        .and_then(|v| v.strip_suffix(".tmp"))
        .is_some_and(|v| uuid::Uuid::parse_str(v).is_ok_and(|id| id.to_string() == v))
}
fn validate_stage(mount: &Mounted, value: &Manifest) -> Result<()> {
    let expected = value
        .files
        .values()
        .map(|v| v.sha256.as_str())
        .collect::<BTreeSet<_>>();
    for entry in fs::read_dir(mount.root.join("staging").join(&value.id))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid staged payload name"))?;
        if name != "intent.json"
            && !atomic_temporary(&name)
            && !name
                .strip_prefix("blob-")
                .and_then(|v| v.strip_suffix(".tmp"))
                .is_some_and(|v| expected.contains(v))
        {
            bail!("unrecognized staged content was preserved");
        }
        private(&entry.path(), false)?;
    }
    Ok(())
}
fn clean_stage(mount: &Mounted, value: &Manifest) -> Result<()> {
    validate_stage(mount, value)?;
    let path = mount.root.join("staging").join(&value.id);
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        if entry.file_name() != "intent.json" {
            fs::remove_file(entry.path())?;
        }
    }
    File::open(&path)?.sync_all()?;
    fs::remove_file(path.join("intent.json"))?;
    File::open(&path)?.sync_all()?;
    fs::remove_dir(path)?;
    File::open(mount.root.join("staging"))?.sync_all()?;
    Ok(())
}
fn clean_committed_stage(mount: &Mounted, checkpoint: &str) -> Result<()> {
    let path = mount.root.join("staging").join(checkpoint);
    if !path.try_exists()? {
        return Ok(());
    }
    private(&path, true)?;
    if fs::read_dir(&path)?.next().is_some() {
        bail!("unexpected content remains beside the confirmed checkpoint");
    }
    fs::remove_dir(path)?;
    File::open(mount.root.join("staging"))?.sync_all()?;
    Ok(())
}
fn recover_preparing_stage(mount: &Mounted, checkpoint: &str) -> Result<()> {
    let path = mount.root.join("staging").join(checkpoint);
    if !path.try_exists()? || path.join("intent.json").try_exists()? {
        return Ok(());
    }
    for entry in preparing_files(&path)? {
        fs::remove_file(entry)?;
    }
    File::open(&path)?.sync_all()?;
    clean_committed_stage(mount, checkpoint)
}
fn verify_blob(mount: &Mounted, content: &Content) -> Result<File> {
    let mut file = checked_file(&mount.root.join("blobs").join(&content.sha256))?;
    if file.metadata()?.len() != content.bytes
        || crate::cache::hash_fd(&mut file)? != content.sha256
    {
        bail!("vault payload checksum mismatch; recovery content was preserved");
    }
    Ok(file)
}
fn verify(mount: &Mounted, value: &Manifest) -> Result<()> {
    for content in value.files.values() {
        verify_blob(mount, content)?;
    }
    Ok(())
}
fn protected(mount: &Mounted, records: &[Manifest]) -> Result<BTreeSet<String>> {
    Ok(records
        .iter()
        .chain(stages(mount)?.iter())
        .flat_map(|r| r.files.values().map(|f| f.sha256.clone()))
        .collect())
}
fn garbage_collect(mount: &Mounted) -> Result<u64> {
    let keep = protected(mount, &rows(mount)?)?;
    let mut remove = Vec::new();
    for entry in fs::read_dir(mount.root.join("blobs"))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid vault blob name"))?;
        if !digest(&name) {
            bail!("unrecognized vault content was preserved");
        }
        private(&entry.path(), false)?;
        if !keep.contains(&name) {
            remove.push((entry.path(), fs::metadata(entry.path())?.len()));
        }
    }
    let mut bytes = 0u64;
    for (path, size) in remove {
        fs::remove_file(path)?;
        bytes = bytes.saturating_add(size);
    }
    File::open(mount.root.join("blobs"))?.sync_all()?;
    Ok(bytes)
}
fn remove_record(mount: &Mounted, record: &Manifest) -> Result<()> {
    remove_record_with(mount, record, |p| Ok(fs::remove_file(p)?))
}
fn remove_record_with(
    mount: &Mounted,
    record: &Manifest,
    remove: impl FnOnce(PathBuf) -> Result<()>,
) -> Result<()> {
    load(mount, &record.id)?;
    remove(
        mount
            .root
            .join("checkpoints")
            .join(format!("{}.json", record.id)),
    )?;
    File::open(mount.root.join("checkpoints"))?.sync_all()?;
    Ok(())
}
fn with_mount<T>(
    home: &Path,
    limits: &Limits,
    initialize: bool,
    readonly: bool,
    host: &mut dyn Host,
    action: impl FnOnce(&Mounted, &mut dyn Host) -> Result<T>,
) -> Result<Option<T>> {
    limits.validate()?;
    private(home, true)?;
    let _lease = super::lease(home)?;
    let Some(mount) = host.mount(home, limits.max_bytes, initialize, readonly)? else {
        return Ok(None);
    };
    let result = (|| {
        if mount.readonly != readonly || mount.capacity_bytes != limits.max_bytes {
            bail!("vault mount or capacity differs from the requested operation");
        }
        layout(&mount, home)?;
        action(&mount, host)
    })();
    let detached = host.unmount(&mount);
    match (result, detached) {
        (Ok(value), Ok(())) => Ok(Some(value)),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error.context("vault operation completed but detach failed")),
    }
}
fn existing_limits(home: &Path, limits: &Limits) -> Result<Limits> {
    Ok(Limits {
        max_bytes: system::System::capacity(home)?.unwrap_or(limits.max_bytes),
        ..limits.clone()
    })
}
pub fn existing_capacity(home: &Path) -> Result<Option<u64>> {
    system::System::capacity(home)
}
pub fn validate_capacity(home: &Path, max_bytes: u64) -> Result<()> {
    system::System::validate_capacity(home, max_bytes)
}
pub fn contains(home: &Path, checkpoint: &str) -> Result<bool> {
    id(checkpoint)?;
    contains_capacity(
        home,
        checkpoint,
        system::System::capacity(home)?,
        &mut system::System::default(),
    )
}
fn contains_capacity(
    home: &Path,
    checkpoint: &str,
    capacity: Option<u64>,
    host: &mut dyn Host,
) -> Result<bool> {
    let Some(max_bytes) = capacity else {
        return Ok(false);
    };
    contains_with(
        home,
        checkpoint,
        &Limits {
            max_bytes,
            max_records: 4096,
            auto_prune: false,
        },
        host,
    )
}

fn contains_with(
    home: &Path,
    checkpoint: &str,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<bool> {
    id(checkpoint)?;
    Ok(with_mount(home, limits, false, true, host, |mount, _| {
        let path = mount
            .root
            .join("checkpoints")
            .join(format!("{checkpoint}.json"));
        if path.try_exists()? {
            load(mount, checkpoint)?;
        } else {
            let stage = mount.root.join("staging").join(checkpoint);
            if !stage.try_exists()? {
                return Ok(false);
            }
            staged(mount, checkpoint)?;
        }
        Ok(true)
    })?
    .unwrap_or(false))
}
fn row(value: &Manifest) -> Result<Value> {
    Ok(
        json!({"id":value.id,"created":value.created,"state":"confirmed","kind":"apfs_file_vault","files":value.files.len(),"bytes":logical_bytes(value)?,"restores_state_database":false}),
    )
}
pub fn status(home: &Path, limits: &Limits) -> Result<Value> {
    status_with(
        home,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn status_with(home: &Path, limits: &Limits, host: &mut dyn Host) -> Result<Value> {
    Ok(with_mount(home,limits,false,true,host,|mount,host| {
        let records = rows(mount)?; let space = host.space(mount)?;
        Ok(json!({"kind":"apfs_file_vault","initialized":true,"capacity_bytes":mount.capacity_bytes,"used_bytes":space.used_bytes,"available_bytes":space.available_bytes,"records":records.len(),"over_record_limit":records.len()>limits.max_records,"max_records":limits.max_records,"auto_prune":limits.auto_prune,"scope":"Fixed APFS vault image including its internal filesystem; independent host or Time Machine snapshots are outside this capacity","content_scope":"Verified immutable queued, Smart, history and indexed cache payloads; no state database rollback"}))
    })?.unwrap_or(json!({"kind":"apfs_file_vault","initialized":false,"capacity_bytes":limits.max_bytes,"records":0,"max_records":limits.max_records,"auto_prune":limits.auto_prune})))
}
pub fn list(home: &Path, limits: &Limits) -> Result<Value> {
    list_with(
        home,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn list_with(home: &Path, limits: &Limits, host: &mut dyn Host) -> Result<Value> {
    Ok(with_mount(home, limits, false, true, host, |mount, _| {
        let confirmed = rows(mount)?;
        let ids = confirmed
            .iter()
            .map(|v| v.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut values = confirmed.iter().map(row).collect::<Result<Vec<_>>>()?;
        for entry in fs::read_dir(mount.root.join("staging"))? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid vault staging name"))?;
            if !ids.contains(name.as_str()) {
                values.push(stage_row(mount, &name)?);
            }
        }
        Ok(json!({"checkpoints":values,"kind":"apfs_file_vault"}))
    })?
    .unwrap_or(json!({"checkpoints":[],"kind":"apfs_file_vault"})))
}
fn stage_row(mount: &Mounted, checkpoint: &str) -> Result<Value> {
    if let Some(record) = staged(mount, checkpoint)? {
        let mut value = row(&record)?;
        value["state"] = json!("unconfirmed");
        value["files"] = json!(record.files);
        Ok(value)
    } else {
        Ok(
            json!({"id":checkpoint,"state":"preparing","kind":"apfs_file_vault","files":{},"restores_state_database":false}),
        )
    }
}

pub fn inspect(home: &Path, checkpoint: &str, limits: &Limits) -> Result<Value> {
    inspect_with(
        home,
        checkpoint,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn inspect_with(
    home: &Path,
    checkpoint: &str,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<Value> {
    id(checkpoint)?;
    with_mount(home, limits, false, true, host, |mount, _| {
        if !mount
            .root
            .join("checkpoints")
            .join(format!("{checkpoint}.json"))
            .try_exists()?
        {
            return stage_row(mount, checkpoint);
        }
        let record = load(mount, checkpoint)?;
        let mut result = row(&record)?;
        result["files"] = json!(record.files);
        Ok(result)
    })?
    .context("APFS file vault has not been created")
}
fn source_file(path: &Path) -> Result<File> {
    let file = checked_file(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        bail!("checkpoint payload is busy; retry after it finishes");
    }
    Ok(file)
}
fn identity(file: &File) -> Result<(u64, u64, u64, i64, i64, i64, i64)> {
    let m = file.metadata()?;
    Ok((
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
fn source(path: PathBuf, logical: &Path, expected: Option<&str>, maximum: u64) -> Result<Source> {
    let mut file = source_file(&path)?;
    let before = identity(&file)?;
    if before.2 > maximum {
        bail!("insufficient_quota: a checkpoint payload exceeds the entire vault capacity");
    }
    let hash = crate::cache::hash_fd(&mut file)?;
    if identity(&file)? != before || expected.is_some_and(|v| v != hash) {
        bail!("checkpoint source changed or its verified checksum is invalid");
    }
    Ok(Source {
        path,
        content: Content {
            bytes: before.2,
            sha256: hash,
            exportable: true,
            kind: super::kind(logical),
        },
    })
}
fn collect_tree(
    base: &Path,
    path: &Path,
    logical: &Path,
    out: &mut BTreeMap<PathBuf, Source>,
    hash_name: bool,
    maximum: u64,
) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    private(path, true)?;
    if fs::metadata(path)?.dev() != fs::metadata(base)?.dev() {
        bail!("payload tree crosses a filesystem boundary");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = logical.join(entry.file_name());
        relative(&child)?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() {
            bail!("checkpoint refuses symbolic links in payload trees");
        }
        if meta.is_dir() {
            if hash_name {
                bail!("history blobs must be flat verified files");
            }
            collect_tree(base, &entry.path(), &child, out, hash_name, maximum)?;
        } else {
            if hash_name
                && entry
                    .file_name()
                    .to_str()
                    .and_then(|v| v.strip_prefix(".capture-"))
                    .is_some_and(|v| uuid::Uuid::parse_str(v).is_ok_and(|id| id.to_string() == v))
            {
                private(&entry.path(), false)?;
                continue;
            }
            let expected = if hash_name {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid history blob name"))?;
                if !digest(&name) {
                    bail!("invalid history blob identity");
                }
                Some(name)
            } else {
                None
            };
            if !super::exportable(&child) {
                continue;
            }
            out.insert(
                child.clone(),
                source(entry.path(), &child, expected.as_deref(), maximum)?,
            );
        }
    }
    Ok(())
}
fn capture(store: &Store, maximum: u64) -> Result<BTreeMap<PathBuf, Source>> {
    let mut out = BTreeMap::new();
    for name in ["spool", "smart", "history/blobs"] {
        collect_tree(
            &store.home,
            &store.home.join(name),
            Path::new(name),
            &mut out,
            name == "history/blobs",
            maximum,
        )?;
    }
    for item in store.db.cache.values() {
        if item.scope.as_deref() != Some(crate::cache::scope(store, &item.profile)?.as_str()) {
            bail!("cached payload belongs to another or an unbound account");
        }
        let logical = Path::new("cache").join(&item.object);
        relative(&logical)?;
        let path = crate::cache::object_path(store, &item.object)?;
        let value = source(path, &logical, Some(&item.sha256), maximum)?;
        if value.content.bytes != item.entry.size {
            bail!("cached payload size differs from its verified index");
        }
        out.insert(logical, value);
    }
    validate_capture_size(out.len())?;
    Ok(out)
}
fn validate_capture_size(count: usize) -> Result<()> {
    if count > 1_000_000 {
        bail!("checkpoint contains too many payloads");
    }
    Ok(())
}
fn copy_blob(mount: &Mounted, stage: &Path, input: &Source) -> Result<()> {
    let target = mount.root.join("blobs").join(&input.content.sha256);
    if target.try_exists()? {
        verify_blob(mount, &input.content)?;
        return Ok(());
    }
    let temporary = stage.join(format!("blob-{}.tmp", input.content.sha256));
    let mut original = source_file(&input.path)?;
    let before = identity(&original)?;
    let mut copied = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let count = std::io::copy(&mut original, &mut copied)?;
    copied.sync_all()?;
    copied.seek(SeekFrom::Start(0))?;
    if count != input.content.bytes
        || crate::cache::hash_fd(&mut copied)? != input.content.sha256
        || identity(&original)? != before
        || crate::cache::hash_fd(&mut original)? != input.content.sha256
    {
        bail!("checkpoint source changed during its verified copy");
    }
    crate::cache_directory::install(&temporary, &target)?;
    File::open(stage)?.sync_all()?;
    File::open(mount.root.join("blobs"))?.sync_all()?;
    verify_blob(mount, &input.content)?;
    Ok(())
}
fn prune_one(mount: &Mounted, records: &mut Vec<Manifest>) -> Result<bool> {
    if records.len() <= 1 {
        return Ok(false);
    }
    let old = records.pop().unwrap();
    remove_record(mount, &old)?;
    garbage_collect(mount)?;
    Ok(true)
}
fn reserve(
    mount: &Mounted,
    limits: &Limits,
    host: &mut dyn Host,
    records: &mut Vec<Manifest>,
    needed: u64,
) -> Result<()> {
    loop {
        if host.space(mount)?.available_bytes >= needed {
            return Ok(());
        }
        if !limits.auto_prune || !prune_one(mount, records)? {
            bail!("insufficient_quota: vault cannot preserve both its latest checkpoint and the requested content");
        }
    }
}
pub fn create_planned(store: &Store, checkpoint: &str, limits: &Limits) -> Result<Value> {
    create_with(store, checkpoint, limits, &mut system::System::default())
}
fn create_with(
    store: &Store,
    checkpoint: &str,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<Value> {
    id(checkpoint)?;
    let _queues = queue_leases(&store.home)?;
    if store.db.jobs.iter().any(|j| j.status == "running") {
        bail!("an upload is running; retry checkpoint after it finishes");
    }
    let _budget = crate::cache::budget_lock(&store.home, true)?;
    with_mount(&store.home,limits,true,false,host,|mount,host|{
        let final_path=mount.root.join("checkpoints").join(format!("{checkpoint}.json"));
        if final_path.try_exists()?{let value=load(mount,checkpoint)?;verify(mount,&value)?;clean_committed_stage(mount,checkpoint)?;return Ok(json!({"created":true,"confirmed":true,"id":checkpoint,"kind":"apfs_file_vault","adopted":true,"remote_changed":false}));}
        stages(mount)?;
        if fs::read_dir(mount.root.join("staging"))?.next().is_some(){bail!("an unconfirmed file checkpoint must be reconciled before another capture");}
        let mut old=rows(mount)?;if let Some(latest)=old.first(){verify(mount,latest)?;}
        if !limits.auto_prune && old.len()>=limits.max_records{bail!("checkpoint record limit reached; no checkpoint was removed");}
        garbage_collect(mount)?;
        let sources=capture(store,limits.max_bytes)?;
        let value=Manifest{version:1,id:checkpoint.into(),created:state::now(),sequence:old.iter().map(|v|v.sequence).max().unwrap_or(0).checked_add(1).context("vault checkpoint sequence exhausted")?,home:mount.home_binding.clone(),instance_id:mount.instance_id.clone(),volume_uuid:mount.volume_uuid.clone(),files:sources.iter().map(|(p,s)|(p.clone(),s.content.clone())).collect()};
        let raw_manifest=serde_json::to_vec(&value)?;
        let stage=mount.root.join("staging").join(checkpoint);directory(&stage)?;File::open(mount.root.join("staging"))?.sync_all()?;state::atomic_write(&stage.join("intent.json"),&raw_manifest)?;
        let mut hashes=BTreeSet::new();let mut needed=8*1024*1024u64+raw_manifest.len() as u64*2;
        for input in sources.values(){if hashes.insert(input.content.sha256.clone()) && !mount.root.join("blobs").join(&input.content.sha256).try_exists()?{needed=needed.checked_add(input.content.bytes).and_then(|v|v.checked_add(16384)).context("vault byte reservation overflow")?;}}
        reserve(mount,limits,host,&mut old,needed)?;
        for input in sources.values(){copy_blob(mount,&stage,input)?;}
        verify(mount,&value)?;
        crate::cache_directory::install(&stage.join("intent.json"),&final_path)?;File::open(mount.root.join("checkpoints"))?.sync_all()?;
        fs::remove_dir(&stage)?;File::open(mount.root.join("staging"))?.sync_all()?;
        let mut all=rows(mount)?;
        while all.len()>limits.max_records {if !prune_one(mount,&mut all)?{break;}}
        garbage_collect(mount)?;
        Ok(json!({"created":true,"confirmed":true,"id":checkpoint,"kind":"apfs_file_vault","files":value.files.len(),"bytes":logical_bytes(&value)?,"remote_changed":false}))
    })?.context("APFS file vault could not be initialized")
}
pub fn reconcile(home: &Path, checkpoint: &str, limits: &Limits) -> Result<Value> {
    reconcile_with(
        home,
        checkpoint,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn reconcile_with(
    home: &Path,
    checkpoint: &str,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<Value> {
    id(checkpoint)?;
    Ok(with_mount(home,limits,false,false,host,|mount,_|{
        if mount.root.join("checkpoints").join(format!("{checkpoint}.json")).try_exists()?{let value=load(mount,checkpoint)?;verify(mount,&value)?;clean_committed_stage(mount,checkpoint)?;let mut result=row(&value)?;result["confirmed"]=json!(true);return Ok(result);}
        recover_preparing_stage(mount,checkpoint)?;
        if let Some(value)=stages(mount)?.iter().find(|v|v.id==checkpoint){clean_stage(mount,value)?;garbage_collect(mount)?;}
        Ok(json!({"id":checkpoint,"state":"missing","missing":true,"safe_to_retry_same_id":true,"kind":"apfs_file_vault"}))
    })?.unwrap_or(json!({"id":checkpoint,"state":"missing","missing":true,"safe_to_retry_same_id":true,"kind":"apfs_file_vault"})))
}
pub fn export_file(
    home: &Path,
    checkpoint: &str,
    payload: &Path,
    destination: &Path,
    limits: &Limits,
) -> Result<Value> {
    export_with(
        home,
        checkpoint,
        payload,
        destination,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn export_with(
    home: &Path,
    checkpoint: &str,
    payload: &Path,
    destination: &Path,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<Value> {
    id(checkpoint)?;
    relative(payload)?;
    if !destination.is_absolute()
        || fs::symlink_metadata(destination).is_ok()
        || state::resolve_location(destination)?.starts_with(home.canonicalize()?)
    {
        bail!("checkpoint export requires a new absolute destination outside live state");
    }
    with_mount(home,limits,false,true,host,|mount,_|{
        let record=load(mount,checkpoint)?;let content=record.files.get(payload).context("payload was not recorded by this checkpoint")?;let mut source=verify_blob(mount,content)?;publish_payload(&mut source,content,destination)?;
        Ok(json!({"exported":true,"checkpoint_id":checkpoint,"payload":payload,"destination":destination,"sha256":content.sha256,"remote_changed":false,"new_guarded_restore_required":true,"kind":"apfs_file_vault"}))
    })?.context("APFS file vault has not been created")
}
pub fn forget(home: &Path, checkpoint: &str, limits: &Limits) -> Result<Value> {
    forget_with(
        home,
        checkpoint,
        &existing_limits(home, limits)?,
        &mut system::System::default(),
    )
}
fn forget_with(
    home: &Path,
    checkpoint: &str,
    limits: &Limits,
    host: &mut dyn Host,
) -> Result<Value> {
    id(checkpoint)?;
    with_mount(home,limits,false,false,host,|mount,_|{let value=load(mount,checkpoint)?;remove_record(mount,&value)?;garbage_collect(mount)?;Ok(json!({"forgotten":true,"id":checkpoint,"os_snapshot_deleted":false,"kind":"apfs_file_vault"}))})?.context("APFS file vault has not been created")
}
pub fn prune(home: &Path, limits: &Limits, apply: bool) -> Result<Value> {
    prune_with(
        home,
        &existing_limits(home, limits)?,
        apply,
        &mut system::System::default(),
    )
}
fn prune_with(home: &Path, limits: &Limits, apply: bool, host: &mut dyn Host) -> Result<Value> {
    Ok(with_mount(home,limits,false,!apply,host,|mount,_|{let mut records=rows(mount)?;if let Some(latest)=records.first(){verify(mount,latest)?;}stages(mount)?;let selected=records.len().saturating_sub(limits.max_records.max(1));if apply{while records.len()>limits.max_records.max(1){prune_one(mount,&mut records)?;}garbage_collect(mount)?;}Ok(json!({"apply":apply,"candidates":selected,"removed":if apply{selected}else{0},"retained":records.len(),"kind":"apfs_file_vault","os_snapshots_deleted":false}))})?.unwrap_or(json!({"apply":apply,"candidates":0,"removed":0,"retained":0,"kind":"apfs_file_vault","os_snapshots_deleted":false})))
}

#[cfg(test)]
mod tests;
