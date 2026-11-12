//! Optional, OS-backed APFS recovery checkpoints. A checkpoint never replays
//! an old state database. Managed file checkpoints have a separate capacity
//! boundary; native system snapshots remain outside automatic cleanup.
use crate::state::{self, Store};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const RECORD_VERSION: u32 = 1;

mod automation;
pub(crate) mod vault;
pub(crate) use automation::scheduled;
pub(crate) use automation::due as scheduled_due;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Storage {
    #[default]
    SystemSnapshot,
    ApfsVault,
}
fn is_system_snapshot(storage: &Storage) -> bool { *storage == Storage::SystemSnapshot }
fn disabled(value: &bool) -> bool { !*value }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub enabled: bool,
    /// Local record limit, not a request to purge Time Machine snapshots.
    pub max_records: usize,
    #[serde(skip_serializing_if="is_system_snapshot")]
    pub storage: Storage,
    /// None keeps existing configurations in manual-only mode.
    #[serde(skip_serializing_if="Option::is_none")]
    pub interval_secs: Option<u64>,
    /// Fixed APFS file-vault capacity; unavailable for a shared system volume.
    #[serde(skip_serializing_if="Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(skip_serializing_if="disabled")]
    pub auto_prune: bool,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_records: 64,
            storage: Storage::SystemSnapshot,
            interval_secs: None,
            max_bytes: None,
            auto_prune: false,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.max_records == 0 || self.max_records > 4096 {
            bail!("checkpoint record limit must be between 1 and 4096");
        }
        match self.storage {
            Storage::SystemSnapshot => {
                if self.interval_secs.is_some() || self.max_bytes.is_some() || self.auto_prune {
                    bail!("scheduled creation, byte capacity and automatic cleanup require apfs_vault storage");
                }
            }
            Storage::ApfsVault => {
                let bytes = self.max_bytes.context("an APFS file vault requires a finite max_bytes capacity")?;
                if bytes < 256 * 1024 * 1024 || bytes > i64::MAX as u64 || bytes % 512 != 0 {
                    bail!("checkpoint vault capacity must be at least 256 MiB, fit signed 64 bits, and be a multiple of 512 bytes");
                }
            }
        }
        if self.interval_secs.is_some_and(|value| !(60..=31_536_000).contains(&value)) {
            bail!("checkpoint interval must be between 60 and 31536000 seconds");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Volume {
    uuid: String,
    device: String,
    mount: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    uuid: String,
    name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Content {
    bytes: u64,
    sha256: String,
    exportable: bool,
    kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    id: String,
    created: String,
    home: String,
    relative_home: PathBuf,
    volume: Volume,
    files: BTreeMap<PathBuf, Content>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    manifest: Manifest,
    snapshot: Option<Snapshot>,
    before: Vec<String>,
    confirmed: bool,
}

trait Host {
    fn supported(&self) -> bool;
    fn volume(&mut self, home: &Path) -> Result<Volume>;
    fn snapshots(&mut self, volume: &Volume) -> Result<Vec<Snapshot>>;
    fn create(&mut self, authorize: bool) -> Result<String>;
    fn mount(
        &mut self,
        volume: &Volume,
        snapshot: &Snapshot,
        target: &Path,
        authorize: bool,
    ) -> Result<()>;
    fn unmount(&mut self, target: &Path, authorize: bool) -> Result<()>;
}

fn private(path: &Path, directory: bool) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.file_type().is_symlink()
        || if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        bail!("checkpoint paths must be real private owned files and directories");
    }
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.mode(0o700);
    match b.create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    };
    private(path, true)
}
fn id(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value)?.to_string() != value {
        bail!("invalid checkpoint identity");
    }
    Ok(())
}
fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        bail!("checkpoint paths must be relative without traversal");
    }
    Ok(())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn validate_volume(v: &Volume) -> Result<()> {
    uuid::Uuid::parse_str(&v.uuid)?;
    if !v.device.starts_with("disk")
        || !v.device[4..].bytes().all(|b| b.is_ascii_alphanumeric())
        || v.device.len() <= 4
        || !v.mount.is_absolute()
    {
        bail!("invalid APFS volume identity");
    }
    Ok(())
}
fn validate_snapshot(s: &Snapshot) -> Result<()> {
    uuid::Uuid::parse_str(&s.uuid)?;
    let date = s
        .name
        .strip_prefix("com.apple.TimeMachine.")
        .and_then(|n| n.strip_suffix(".local"))
        .context("unsupported APFS snapshot name")?;
    chrono::NaiveDateTime::parse_from_str(date, "%Y-%m-%d-%H%M%S")?;
    Ok(())
}
fn root(home: &Path) -> PathBuf {
    home.join("checkpoints")
}
fn lease(home: &Path) -> Result<File> {
    private(home, true)?;
    let lock = home.join("checkpoints.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock)?;
    private(&lock, false)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another checkpoint operation is active");
    }
    Ok(file)
}
fn record_path(home: &Path, checkpoint: &str) -> Result<PathBuf> {
    id(checkpoint)?;
    Ok(root(home).join(checkpoint).join("record.json"))
}
fn load(home: &Path, checkpoint: &str) -> Result<Record> {
    id(checkpoint)?;
    private(home, true)?;
    private(&root(home), true)?;
    private(&root(home).join(checkpoint), true)?;
    let path = record_path(home, checkpoint)?;
    private(&path, false)?;
    if fs::metadata(&path)?.len() > 128 * 1024 * 1024 {
        bail!("checkpoint record exceeds its size bound");
    }
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    let m = &record.manifest;
    id(&m.id)?;
    relative(&m.relative_home)?;
    validate_volume(&m.volume)?;
    if m.version != RECORD_VERSION
        || m.id != checkpoint
        || m.home != state::key(&home.canonicalize()?.to_string_lossy())
        || m.files.len() > 1_000_000
    {
        bail!("checkpoint does not belong to this state directory");
    }
    let mut bytes = 0u64;
    for (name, file) in &m.files {
        relative(name)?;
        if !digest(&file.sha256) || file.kind != kind(name) || file.exportable != exportable(name) {
            bail!("invalid checkpoint content fingerprint or role");
        }
        bytes = bytes
            .checked_add(file.bytes)
            .context("checkpoint content size overflow")?;
    }
    for before in &record.before {
        uuid::Uuid::parse_str(before)?;
    }
    if let Some(s) = &record.snapshot {
        validate_snapshot(s)?;
    } else if record.confirmed {
        bail!("confirmed checkpoint lost its snapshot identity");
    }
    Ok(record)
}
fn records(home: &Path) -> Result<Vec<Record>> {
    private(home, true)?;
    if !root(home).exists() {
        return Ok(vec![]);
    }
    private(&root(home), true)?;
    let mut result = vec![];
    for entry in fs::read_dir(root(home))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("invalid checkpoint record filename")?;
        if name == "mounts" || name == "vault" {
            private(&entry.path(), true)?;
            continue;
        }
        if let Some(forgotten) = name.strip_prefix("forgotten-") {
            id(forgotten)?;
            private(&entry.path(), true)?;
            continue;
        }
        result.push(load(home, name)?);
    }
    result.sort_by(|a, b| b.manifest.created.cmp(&a.manifest.created));
    Ok(result)
}

/// Capability information never creates a checkpoint or changes Time Machine.
pub fn status(home: &Path, policy: &Policy) -> Result<Value> {
    policy.validate()?;
    let mut value = if policy.storage == Storage::ApfsVault {
        let mut value = match vault::status(home, &automation::limits(policy)?) {
            Ok(value) => value,
            Err(error) if automation::busy(&error) => json!({"kind":"apfs_file_vault","busy":true,"configured_capacity_bytes":policy.max_bytes,"max_records":policy.max_records,"records":null}),
            Err(error) => return Err(error),
        };
        value["enabled"] = json!(policy.enabled);
        value["available"] = json!(cfg!(target_os = "macos"));
        value
    } else {
        status_with(home, policy, &mut System::default())?
    };
    value["storage"] = json!(policy.storage);
    value["schedule"] = automation::schedule_status(home, policy)?;
    Ok(value)
}
fn status_with(home: &Path, policy: &Policy, host: &mut dyn Host) -> Result<Value> {
    policy.validate()?;
    private(home, true)?;
    if !host.supported() {
        return Ok(
            json!({"enabled":policy.enabled,"available":false,"reason":"APFS checkpoints require macOS and its system disk tools","system_managed":true}),
        );
    }
    let volume = host.volume(home)?;
    validate_volume(&volume)?;
    let snapshots = host.snapshots(&volume)?;
    Ok(
        json!({"enabled":policy.enabled,"available":true,"volume":volume,"system_snapshot_count":snapshots.len(),"local_record_count":records(home)?.len(),"max_records":policy.max_records,"system_managed":true,"authorization_may_be_required":true,"content_scope":"Verified files inside this state directory only; online-only remote content, external cache paths and materialized CloudStorage files are absent","restore":"Export a verified payload, then submit a new guarded history restore; never roll back the state database"}),
    )
}
pub fn list(home: &Path) -> Result<Value> {
    let mut value = list_with(home, &mut System::default())?;
    if let Some(capacity) = vault::existing_capacity(home)? {
        let managed = vault::list(home, &vault::Limits { max_bytes:capacity, max_records:4096, auto_prune:false })?;
        merge_rows(&mut value, &managed)?;
        value["system_managed"] = json!(false);
    }
    Ok(value)
}
fn merge_rows(value: &mut Value, managed: &Value) -> Result<()> {
    let rows = value["checkpoints"].as_array_mut().context("checkpoint list missing")?;
    for row in managed["checkpoints"].as_array().context("vault checkpoint list missing")? {
        if rows.iter().any(|old| old["id"] == row["id"]) { bail!("ambiguous checkpoint identity across storage backends"); }
        rows.push(row.clone());
    }
    Ok(())
}
fn list_with(home: &Path, host: &mut dyn Host) -> Result<Value> {
    let rows = records(home)?;
    let present = if host.supported() {
        let volume = host.volume(home)?;
        Some(host.snapshots(&volume)?)
    } else {
        None
    };
    Ok(
        json!({"checkpoints":rows.into_iter().map(|r| { let available = present.as_ref().map(|all| r.snapshot.as_ref().is_some_and(|s| all.contains(s))); json!({"id":r.manifest.id,"created":r.manifest.created,"state":if r.confirmed{"confirmed"}else{"unconfirmed"},"snapshot":r.snapshot,"snapshot_present":available,"files":r.manifest.files.len(),"bytes":r.manifest.files.values().map(|f|f.bytes).sum::<u64>()}) }).collect::<Vec<_>>(),"system_managed":true}),
    )
}

fn exportable(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("spool/")
        || text.starts_with("cache/")
        || path
            .strip_prefix("history/blobs")
            .is_ok_and(|leaf| leaf.components().count() == 1 && leaf.to_str().is_some_and(digest))
        || (text.starts_with("smart/")
            && text.contains("/dirty/")
            && path.extension().is_some_and(|x| x == "data"))
}
fn kind(path: &Path) -> String {
    let name = path.to_string_lossy();
    if !exportable(path) {
        "state_metadata"
    } else if name.starts_with("history/blobs/") {
        "history_blob"
    } else if name.starts_with("smart/") {
        "smart_payload"
    } else if name.starts_with("spool/") {
        "queued_upload"
    } else {
        "cached_payload"
    }
    .into()
}
fn skipped(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text == "checkpoints"
        || text == "checkpoint-scheduler"
        || text == "service"
        || text == "tmp"
        || text.starts_with("provider/demands-")
        || text.ends_with(".lock")
        || text.ends_with(".sqlite3-shm")
        || text.ends_with(".sqlite3-wal")
        || path.file_name().is_some_and(|s| {
            let s = s.to_string_lossy();
            s.starts_with(".rdrive-") || s.starts_with(".capture-")
        })
}
fn walk(
    home: &Path,
    path: &Path,
    files: &mut BTreeMap<PathBuf, Content>,
    sqlite: bool,
) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        let name = path.strip_prefix(home)?.to_path_buf();
        if skipped(&name) {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            bail!("checkpoint refuses symbolic links in retained state");
        }
        if meta.is_dir() {
            walk(home, &path, files, sqlite)?;
        } else if meta.is_file() {
            private(&path, false)?;
            if sqlite && path.extension().is_some_and(|s| s == "sqlite3") {
                let connection = rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
                )?;
                connection.busy_timeout(Duration::ZERO)?;
                let busy: i64 =
                    connection
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
                if busy != 0 {
                    bail!("a SQLite writer is busy; checkpoint was not created");
                }
            }
            files.insert(
                name.clone(),
                Content {
                    bytes: fs::metadata(&path)?.len(),
                    sha256: state::fingerprint(&path)?,
                    exportable: exportable(&name),
                    kind: kind(&name),
                },
            );
        } else {
            bail!("checkpoint refuses non-regular state entries");
        }
    }
    Ok(())
}
fn queue_leases(home: &Path) -> Result<Vec<File>> {
    let mut held = vec![];
    let smart = home.join("smart");
    if !smart.exists() {
        return Ok(held);
    }
    for directory in fs::read_dir(smart)? {
        let directory = directory?.path();
        if !directory.is_dir() {
            continue;
        }
        private(&directory, true)?;
        let lock = directory.join("queue.lock");
        if !lock.exists() {
            continue;
        }
        private(&lock, false)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("a Smart request is active; retry checkpoint after it finishes");
        }
        let queue: Value = serde_json::from_slice(&fs::read(directory.join("queue.json"))?)?;
        if queue["mutations"]
            .as_object()
            .context("invalid Smart queue")?
            .values()
            .any(|m| m["state"] == "running")
        {
            bail!("a Smart request is still running; checkpoint was not created");
        }
        held.push(file);
    }
    Ok(held)
}
fn home_relative(home: &Path, volume: &Volume) -> Result<PathBuf> {
    let home = home.canonicalize()?;
    let suffix = if let Ok(path) = home.strip_prefix(&volume.mount) {
        path.to_path_buf()
    } else {
        home.strip_prefix("/")?.to_path_buf()
    };
    relative(&suffix)?;
    let candidate = volume.mount.join(&suffix);
    let a = fs::metadata(&home)?;
    let b = fs::metadata(candidate)?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        bail!("state directory is not on the reported APFS volume");
    }
    Ok(suffix)
}

/// Caller holds Store and its mutation coordinator until this function returns.
/// This method also leases retained queues and checkpoints SQLite WALs.
pub fn create(store: &Store, policy: &Policy, authorize: bool) -> Result<Value> {
    if policy.storage == Storage::ApfsVault {
        return automation::manual(store, policy);
    }
    create_with(store, policy, authorize, &mut System::default())
}
fn create_with(
    store: &Store,
    policy: &Policy,
    authorize: bool,
    host: &mut dyn Host,
) -> Result<Value> {
    policy.validate()?;
    if !policy.enabled {
        bail!("APFS checkpoints are disabled; enable them through checkpoint configuration first");
    }
    let _lease = lease(&store.home)?;
    if !host.supported() {
        bail!("APFS checkpoints are unavailable on this platform");
    }
    if records(&store.home)?.len() >= policy.max_records {
        bail!("checkpoint record limit reached; system snapshots were not deleted");
    }
    let _queues = queue_leases(&store.home)?;
    if store.db.jobs.iter().any(|j| j.status == "running") {
        bail!("an upload is running; checkpoint was not created");
    }
    let volume = host.volume(&store.home)?;
    validate_volume(&volume)?;
    let relative_home = home_relative(&store.home, &volume)?;
    let mut files = BTreeMap::new();
    walk(&store.home, &store.home, &mut files, true)?;
    let manifest = Manifest {
        version: RECORD_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        created: state::now(),
        home: state::key(&store.home.to_string_lossy()),
        relative_home,
        volume: volume.clone(),
        files,
    };
    let before = host.snapshots(&volume)?;
    for s in &before {
        validate_snapshot(s)?;
    }
    directory(&root(&store.home))?;
    let path = root(&store.home).join(&manifest.id);
    directory(&path)?;
    state::atomic_json(&path.join("manifest.json"), &manifest)?;
    let mut record = Record {
        manifest: manifest.clone(),
        snapshot: None,
        before: before.iter().map(|s| s.uuid.clone()).collect(),
        confirmed: false,
    };
    state::atomic_json(&path.join("record.json"), &record)?;
    host.create(authorize).context(
        "APFS creation was not confirmed; the prepared local checkpoint record was retained",
    )?;
    let after = host.snapshots(&volume)?;
    let added = after
        .into_iter()
        .filter(|s| !record.before.contains(&s.uuid))
        .collect::<Vec<_>>();
    if added.len() != 1 {
        bail!("a unique new APFS snapshot was not confirmed; local record remains unconfirmed");
    }
    validate_snapshot(&added[0])?;
    let mut current = BTreeMap::new();
    walk(&store.home, &store.home, &mut current, false)?;
    if current != manifest.files {
        bail!("state changed while creating the checkpoint; local record remains unconfirmed");
    }
    record.snapshot = Some(added[0].clone());
    state::atomic_json(&path.join("record.json"), &record)?;
    mounted(&store.home, &record, authorize, host, |mount| {
        verify_mounted(mount, &manifest, true)
    })?;
    record.confirmed = true;
    state::atomic_json(&path.join("record.json"), &record)?;
    Ok(
        json!({"created":true,"id":manifest.id,"snapshot":record.snapshot,"files":manifest.files.len(),"system_managed":true,"remote_changed":false}),
    )
}

pub fn inspect(home: &Path, checkpoint: &str) -> Result<Value> {
    if managed_id(home, checkpoint)? { return vault::inspect(home, checkpoint, &recovery_limits(home)?) }
    let record = load(home, checkpoint)?;
    Ok(
        json!({"id":record.manifest.id,"created":record.manifest.created,"snapshot":record.snapshot,"state":if record.confirmed{"confirmed"}else{"unconfirmed"},"files":record.manifest.files,"restores_state_database":false}),
    )
}

fn managed_id(home: &Path, checkpoint: &str) -> Result<bool> {
    id(checkpoint)?;
    if record_path(home, checkpoint)?.try_exists()? { return Ok(false); }
    Ok(automation::pending(home, checkpoint)? || vault::contains(home, checkpoint)?)
}
fn recovery_limits(home: &Path) -> Result<vault::Limits> {
    Ok(vault::Limits {
        max_bytes:vault::existing_capacity(home)?.unwrap_or(20*1024*1024*1024),
        max_records:4096,
        auto_prune:false,
    })
}
pub fn prune(store: &Store, apply: bool) -> Result<Value> {
    let policy = &store.config.checkpoints;
    let mut limits = recovery_limits(&store.home)?;
    limits.max_records = policy.max_records;
    vault::prune(&store.home, &limits, apply)
}

fn open_relative(root: &Path, path: &Path) -> Result<File> {
    relative(path)?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)?;
    let parts = path.components().map(|p| p.as_os_str()).collect::<Vec<_>>();
    for (i, part) in parts.iter().enumerate() {
        let name = CString::new(part.as_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if i + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        use std::os::fd::FromRawFd;
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}
fn fingerprint(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut block = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut block)?;
        if read == 0 {
            break;
        }
        hash.update(&block[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn matches_mounted(mount: &Path, manifest: &Manifest, all: bool) -> Result<bool> {
    let marker = match open_relative(
        mount,
        &manifest
            .relative_home
            .join("checkpoints")
            .join(&manifest.id)
            .join("manifest.json"),
    ) {
        Ok(file) => file,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        Err(e) => return Err(e),
    };
    let mut raw = vec![];
    marker.take(128 * 1024 * 1024 + 1).read_to_end(&mut raw)?;
    if raw.len() > 128 * 1024 * 1024 {
        bail!("snapshot checkpoint manifest exceeds its size bound");
    }
    if serde_json::from_slice::<Manifest>(&raw).ok().as_ref() != Some(manifest) {
        return Ok(false);
    }
    if all {
        for (path, expected) in &manifest.files {
            let mut file = match open_relative(mount, &manifest.relative_home.join(path)) {
                Ok(file) => file,
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    return Ok(false)
                }
                Err(e) => return Err(e),
            };
            if !file.metadata()?.is_file()
                || file.metadata()?.len() != expected.bytes
                || fingerprint(&mut file)? != expected.sha256
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
fn verify_mounted(mount: &Path, manifest: &Manifest, all: bool) -> Result<()> {
    if !matches_mounted(mount, manifest, all)? {
        bail!("mounted snapshot does not match the complete prepared checkpoint");
    }
    Ok(())
}
fn mounted<T>(
    home: &Path,
    record: &Record,
    authorize: bool,
    host: &mut dyn Host,
    action: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let snapshot = record
        .snapshot
        .as_ref()
        .context("checkpoint snapshot identity missing")?;
    let mounts = root(home).join("mounts");
    directory(&mounts)?;
    let mount = mounts.join(&record.manifest.id);
    directory(&mount)?;
    if fs::read_dir(&mount)?.next().is_some() {
        bail!("checkpoint mount directory is not empty; no existing content was changed");
    }
    let result = host
        .mount(&record.manifest.volume, snapshot, &mount, authorize)
        .and_then(|()| action(&mount));
    let cleanup = host.unmount(&mount, authorize);
    let _ = fs::remove_dir(&mount);
    cleanup.with_context(|| {
        format!(
            "checkpoint recovery mount cleanup failed at {}",
            mount.display()
        )
    })?;
    result
}
/// Reconcile an uncertain OS command only by verifying the embedded manifest
/// and every recorded payload in an actual read-only snapshot.
pub fn reconcile(store: &Store, checkpoint: &str, authorize: bool) -> Result<Value> {
    if managed_id(&store.home, checkpoint)? { return vault::reconcile(&store.home, checkpoint, &recovery_limits(&store.home)?) }
    reconcile_with(store, checkpoint, authorize, &mut System::default())
}
fn reconcile_with(
    store: &Store,
    checkpoint: &str,
    authorize: bool,
    host: &mut dyn Host,
) -> Result<Value> {
    let _lease = lease(&store.home)?;
    let mut record = load(&store.home, checkpoint)?;
    if !host.supported() {
        bail!("APFS checkpoint reconciliation requires macOS");
    }
    if host.volume(&store.home)? != record.manifest.volume {
        bail!("checkpoint APFS volume changed");
    }
    let mut candidates = host
        .snapshots(&record.manifest.volume)?
        .into_iter()
        .filter(|s| !record.before.contains(&s.uuid))
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    if candidates.len() > 64 {
        bail!("too many new snapshots to reconcile safely");
    }
    for snapshot in candidates {
        validate_snapshot(&snapshot)?;
        record.snapshot = Some(snapshot);
        if mounted(&store.home, &record, authorize, host, |mount| {
            matches_mounted(mount, &record.manifest, true)
        })? {
            record.confirmed = true;
            state::atomic_json(&record_path(&store.home, checkpoint)?, &record)?;
            return Ok(
                json!({"reconciled":true,"id":checkpoint,"snapshot":record.snapshot,"remote_changed":false}),
            );
        }
    }
    bail!("no available APFS snapshot matches the complete prepared checkpoint")
}
/// Forget only an owned local record. System-managed snapshots are untouched.
pub fn forget(store: &Store, checkpoint: &str) -> Result<Value> {
    if managed_id(&store.home, checkpoint)? { return vault::forget(&store.home, checkpoint, &recovery_limits(&store.home)?) }
    let _lease = lease(&store.home)?;
    id(checkpoint)?;
    let path = root(&store.home).join(checkpoint);
    let tombstone = root(&store.home).join(format!("forgotten-{checkpoint}"));
    if path.exists() {
        load(&store.home, checkpoint)?;
        metadata_only(&path)?;
        if tombstone.exists() {
            bail!("an interrupted checkpoint record cleanup needs review");
        }
        fs::rename(&path, &tombstone)?;
        File::open(root(&store.home))?.sync_all()?;
    }
    if tombstone.exists() {
        metadata_only(&tombstone)?;
        for name in ["manifest.json", "record.json"] {
            match fs::remove_file(tombstone.join(name)) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        fs::remove_dir(tombstone)?;
        File::open(root(&store.home))?.sync_all()?;
    }
    Ok(json!({"forgotten":true,"id":checkpoint,"os_snapshot_deleted":false}))
}
fn metadata_only(path: &Path) -> Result<()> {
    private(path, true)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !["manifest.json", "record.json"].contains(&entry.file_name().to_str().unwrap_or("")) {
            bail!("unexpected content in checkpoint metadata directory; nothing was removed");
        }
        private(&entry.path(), false)?;
    }
    Ok(())
}
/// Export verified local payload bytes only. The history restore API must then
/// enqueue a new guarded mutation; state databases are deliberately not exportable.
pub fn export_file(
    home: &Path,
    checkpoint: &str,
    payload: &Path,
    destination: &Path,
    authorize: bool,
) -> Result<Value> {
    if managed_id(home, checkpoint)? { return vault::export_file(home, checkpoint, payload, destination, &recovery_limits(home)?) }
    export_with(
        home,
        checkpoint,
        payload,
        destination,
        authorize,
        &mut System::default(),
    )
}
fn export_with(
    home: &Path,
    checkpoint: &str,
    payload: &Path,
    destination: &Path,
    authorize: bool,
    host: &mut dyn Host,
) -> Result<Value> {
    let _lease = lease(home)?;
    let record = load(home, checkpoint)?;
    if !record.confirmed {
        bail!("checkpoint is unconfirmed; no content recovery was assumed");
    }
    let snapshot = record
        .snapshot
        .as_ref()
        .context("checkpoint snapshot identity missing")?;
    let expected=record.manifest.files.get(payload).filter(|f|f.exportable).context("only recorded local content payloads can be exported; state databases cannot be restored")?;
    if !destination.is_absolute()
        || destination.exists()
        || fs::symlink_metadata(destination).is_ok()
    {
        bail!("checkpoint export requires a new absolute destination");
    }
    if state::resolve_location(destination)?.starts_with(home.canonicalize()?) {
        bail!("checkpoint export cannot write into the live state directory");
    }
    if !host.supported() {
        bail!("APFS checkpoint recovery is unavailable on this platform");
    }
    if host.volume(home)? != record.manifest.volume
        || !host.snapshots(&record.manifest.volume)?.contains(snapshot)
    {
        bail!("the recorded APFS snapshot is no longer available; no recovery was assumed");
    }
    mounted(home, &record, authorize, host, |mount| {
        verify_mounted(mount, &record.manifest, false)?;
        let mut source = open_relative(mount, &record.manifest.relative_home.join(payload))?;
        if !source.metadata()?.is_file() {
            bail!("checkpoint payload is not a regular file");
        }
        publish_payload(&mut source, expected, destination)?;
        Ok(
            json!({"exported":true,"checkpoint_id":checkpoint,"payload":payload,"destination":destination,"sha256":expected.sha256,"remote_changed":false,"new_guarded_restore_required":true}),
        )
    })
}

/// Anchor the destination directory before copying, then publish with linkat.
/// Other users cannot substitute a verified temporary or redirect its parent.
fn publish_payload(source: &mut File, expected: &Content, destination: &Path) -> Result<()> {
    use std::{
        io::{Seek, SeekFrom},
        os::fd::FromRawFd,
    };
    let parent = destination
        .parent()
        .context("export destination requires a parent")?
        .canonicalize()?;
    let directory = if parent == Path::new("/") {
        File::open(&parent)?
    } else {
        open_relative(Path::new("/"), parent.strip_prefix("/")?)?
    };
    let meta = directory.metadata()?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        bail!("checkpoint export requires an owned destination directory that other users cannot modify");
    }
    let name = CString::new(
        destination
            .file_name()
            .context("export destination requires a filename")?
            .as_bytes(),
    )?;
    let temporary = CString::new(format!(".rdrive-checkpoint-{}", uuid::Uuid::new_v4()))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut output = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<()> {
        std::io::copy(source, &mut output)?;
        output.sync_all()?;
        output.seek(SeekFrom::Start(0))?;
        if output.metadata()?.len() != expected.bytes
            || fingerprint(&mut output)? != expected.sha256
        {
            bail!("checkpoint payload checksum changed; destination was preserved");
        }
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
            return Err(std::io::Error::last_os_error())
                .context("export destination exists or cannot be published without overwrite");
        }
        directory.sync_all()?;
        Ok(())
    })();
    let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
    result
}

#[derive(Debug)]
struct Request {
    program: &'static str,
    args: Vec<String>,
    input: Option<Vec<u8>>,
    authorize: bool,
}
type Executor = fn(Request) -> Result<Vec<u8>>;
struct System {
    execute: Executor,
    filesystem: fn(&Path) -> Result<(PathBuf, bool)>,
}
impl Default for System {
    fn default() -> Self {
        Self {
            execute: execute,
            filesystem: filesystem,
        }
    }
}
fn filesystem(path: &Path) -> Result<(PathBuf, bool)> {
    #[cfg(target_os = "macos")]
    {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        return Ok((
            PathBuf::from(unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_str()?),
            info.f_flags & libc::MNT_RDONLY as u32 != 0,
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        bail!("APFS checkpoints require macOS");
    }
}
impl System {
    fn json(&self, args: Vec<String>) -> Result<Value> {
        let raw = (self.execute)(Request {
            program: "/usr/sbin/diskutil",
            args,
            input: None,
            authorize: false,
        })?;
        let raw = (self.execute)(Request {
            program: "/usr/bin/plutil",
            args: vec![
                "-convert".into(),
                "json".into(),
                "-o".into(),
                "-".into(),
                "-".into(),
            ],
            input: Some(raw),
            authorize: false,
        })?;
        Ok(serde_json::from_slice(&raw)?)
    }
}
impl Host for System {
    fn supported(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn volume(&mut self, home: &Path) -> Result<Volume> {
        let (mount, readonly) = (self.filesystem)(home)?;
        if readonly {
            bail!("state is on a read-only filesystem");
        }
        let value = self.json(vec![
            "info".into(),
            "-plist".into(),
            mount.to_string_lossy().into(),
        ])?;
        if value["FilesystemType"] != "apfs"
            || value["Writable"] != true
            || value["APFSSnapshot"] == true
        {
            bail!("state is not on a writable APFS base volume");
        }
        let volume = Volume {
            uuid: value["VolumeUUID"]
                .as_str()
                .context("APFS volume UUID missing")?
                .into(),
            device: value["DeviceIdentifier"]
                .as_str()
                .context("APFS device identifier missing")?
                .into(),
            mount: PathBuf::from(
                value["MountPoint"]
                    .as_str()
                    .context("APFS mount point missing")?,
            ),
        };
        validate_volume(&volume)?;
        Ok(volume)
    }
    fn snapshots(&mut self, volume: &Volume) -> Result<Vec<Snapshot>> {
        let value = self.json(vec![
            "apfs".into(),
            "listSnapshots".into(),
            "-plist".into(),
            volume.device.clone(),
        ])?;
        let mut rows = vec![];
        for row in value["Snapshots"]
            .as_array()
            .context("APFS snapshot list missing")?
        {
            let Some(name) = row["SnapshotName"]
                .as_str()
                .filter(|s| s.starts_with("com.apple.TimeMachine.") && s.ends_with(".local"))
            else {
                continue;
            };
            let snapshot = Snapshot {
                uuid: row["SnapshotUUID"]
                    .as_str()
                    .context("APFS snapshot UUID missing")?
                    .into(),
                name: name.into(),
            };
            validate_snapshot(&snapshot)?;
            rows.push(snapshot);
        }
        Ok(rows)
    }
    fn create(&mut self, authorize: bool) -> Result<String> {
        Ok(String::from_utf8((self.execute)(Request {
            program: "/usr/bin/tmutil",
            args: vec!["localsnapshot".into()],
            input: None,
            authorize,
        })?)?)
    }
    fn mount(
        &mut self,
        volume: &Volume,
        snapshot: &Snapshot,
        target: &Path,
        authorize: bool,
    ) -> Result<()> {
        (self.execute)(Request {
            program: "/sbin/mount_apfs",
            args: vec![
                "-o".into(),
                "ro,nobrowse".into(),
                "-s".into(),
                snapshot.name.clone(),
                volume.mount.to_string_lossy().into(),
                target.to_string_lossy().into(),
            ],
            input: None,
            authorize,
        })?;
        let (mounted, readonly) = (self.filesystem)(target)?;
        if mounted != target.canonicalize()? || !readonly {
            bail!("APFS recovery target was not mounted read-only");
        }
        Ok(())
    }
    fn unmount(&mut self, target: &Path, authorize: bool) -> Result<()> {
        if (self.filesystem)(target)?.0 != target.canonicalize()? {
            return Ok(());
        }
        (self.execute)(Request {
            program: "/sbin/umount",
            args: vec![target.to_string_lossy().into()],
            input: None,
            authorize,
        })?;
        Ok(())
    }
}
fn command(request: &Request, interactive: bool) -> Result<Command> {
    if request.authorize && !interactive {
        bail!("checkpoint authorization requires an interactive terminal; run this command in your terminal with --authorize");
    }
    let mut command = if request.authorize {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("--").arg(request.program);
        c
    } else {
        Command::new(request.program)
    };
    // sudo must stay in the caller's foreground group to read its real TTY.
    if !request.authorize {
        command.process_group(0);
    }
    command
        .args(&request.args)
        .env("LC_ALL", "C")
        .stdin(if request.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
        .stderr(if request.authorize {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
    Ok(command)
}
fn execute(request: Request) -> Result<Vec<u8>> {
    execute_with(
        request,
        Duration::from_secs(120),
        unsafe { libc::isatty(libc::STDIN_FILENO) } == 1,
    )
}
fn execute_with(request: Request, timeout: Duration, interactive: bool) -> Result<Vec<u8>> {
    let owns_group = !request.authorize;
    execute_command(
        command(&request, interactive)?,
        request.input,
        timeout,
        owns_group,
    )
}
fn execute_command(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    owns_group: bool,
) -> Result<Vec<u8>> {
    let mut child = command
        .spawn()
        .context("cannot run the system APFS checkpoint tool")?;
    let writer = if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .context("checkpoint converter input unavailable")?;
        Some(thread::spawn(move || stdin.write_all(&input)))
    } else {
        None
    };
    let stdout = child
        .stdout
        .take()
        .context("checkpoint tool stdout unavailable")?;
    let stderr = child.stderr.take();
    let output = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut data = vec![];
        stdout.take(4 * 1024 * 1024 + 1).read_to_end(&mut data)?;
        Ok(data)
    });
    let errors = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut data = vec![];
        if let Some(stderr) = stderr {
            stderr.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        }
        Ok(data)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            if owns_group {
                let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = output.join();
            let _ = errors.join();
            if let Some(writer) = writer {
                let _ = writer.join();
            }
            bail!("system checkpoint command timed out; no successful checkpoint was assumed");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("checkpoint output reader failed"))??;
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("checkpoint error reader failed"))??;
    let input_result = writer
        .map(|writer| {
            writer
                .join()
                .map_err(|_| anyhow::anyhow!("checkpoint input writer failed"))
                .and_then(|result| result.map_err(Into::into))
        })
        .transpose();
    if output.len() > 4 * 1024 * 1024 || errors.len() > 1024 * 1024 {
        bail!("system checkpoint output exceeded its bound");
    }
    if !status.success() {
        bail!(
            "system checkpoint command failed: {}",
            String::from_utf8_lossy(&errors)
                .chars()
                .take(1000)
                .collect::<String>()
        );
    }
    input_result?;
    Ok(output)
}

#[cfg(test)]
mod tests;
