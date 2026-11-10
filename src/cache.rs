//! Bounded, account-bound clean file payloads. Upload snapshots and offline edits
//! live in other directories and are never candidates for eviction here.
pub use crate::cache_directory::{
    cache_directory, configured_directory, directory_status, object_path, path_is_within,
    paths_overlap, plan_directory, protected_directories, set_directory,
};
use crate::{
    backend::{self, Backend, Entry},
    state::{self, CacheItem, CacheReservation, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct CacheStatus {
    pub max_bytes: Option<u64>,
    /// Actual file payloads, including crash remnants, inside home/cache.
    pub used_bytes: u64,
    /// Admitted bytes not written yet; avoids double counting partial payloads.
    pub reserved_bytes: u64,
    pub rust_accounted_bytes: u64,
    pub native_used_bytes: u64,
    pub native_reserved_bytes: u64,
    pub native_pinned_bytes: u64,
    pub native_accounting_id: String,
    pub combined_used_bytes: u64,
    pub combined_reserved_bytes: u64,
    pub accounted_bytes: u64,
    pub pinned_bytes: u64,
    pub leased_bytes: u64,
    pub unmanaged_bytes: u64,
    pub files: usize,
    pub pinned_files: usize,
    pub missing_files: usize,
    pub over_budget: bool,
    pub scope: &'static str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeUsage {
    pub native_used_bytes: u64,
    pub native_reserved_bytes: u64,
    pub native_pinned_bytes: u64,
    pub native_accounting_id: String,
}

#[derive(Serialize, Deserialize)]
struct NativeReport {
    used: u64,
    reserved: u64,
    pinned: u64,
    updated: i64,
}
#[derive(Serialize, Deserialize)]
struct NativeLedger {
    version: u32,
    reports: BTreeMap<String, NativeReport>,
}
impl Default for NativeLedger {
    fn default() -> Self {
        Self {
            version: 1,
            reports: BTreeMap::new(),
        }
    }
}
const NATIVE_RESERVATION_TTL: i64 = 120;

fn flock(fd: std::os::fd::RawFd, operation: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::flock(fd, operation) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}
pub(crate) fn budget_lock(home: &Path, exclusive: bool) -> Result<File> {
    budget_lock_with(home, exclusive, flock)
}
fn budget_lock_with(home: &Path, exclusive: bool, lock: impl FnOnce(std::os::fd::RawFd,libc::c_int)->std::io::Result<()>) -> Result<File> {
    let metadata = fs::symlink_metadata(home)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("cache accounting requires an owned private state directory");
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home.join("cache-budget.lock"))?;
    if !file.metadata()?.is_file() {
        bail!("cache accounting lock is not a regular file");
    }
    lock(file.as_raw_fd(), if exclusive { libc::LOCK_EX } else { libc::LOCK_SH })?;
    Ok(file)
}

fn read_native(home: &Path) -> Result<NativeLedger> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home.join("native-cache-usage.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NativeLedger::default())
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > 1024 * 1024
    {
        bail!("invalid native cache accounting file");
    }
    let ledger: NativeLedger = serde_json::from_reader(file)?;
    if ledger.version != 1
        || ledger.reports.len() > 4096
        || ledger.reports.iter().any(|(id, report)| {
            id.is_empty()
                || id.len() > 256
                || id.chars().any(char::is_control)
                || report.pinned > report.used
                || report.updated < 0
        })
    {
        bail!("invalid native cache accounting records");
    }
    Ok(ledger)
}

fn aggregate_native(ledger: &NativeLedger) -> Result<NativeUsage> {
    let now = chrono::Utc::now().timestamp();
    let mut usage = NativeUsage {
        native_accounting_id: if ledger.reports.len() == 1 {
            ledger.reports.keys().next().unwrap().clone()
        } else {
            "aggregate".into()
        },
        ..NativeUsage::default()
    };
    for report in ledger.reports.values() {
        usage.native_used_bytes = add(usage.native_used_bytes, report.used)?;
        usage.native_pinned_bytes = add(usage.native_pinned_bytes, report.pinned)?;
        if now.saturating_sub(report.updated) <= NATIVE_RESERVATION_TTL {
            usage.native_reserved_bytes = add(usage.native_reserved_bytes, report.reserved)?;
        }
    }
    Ok(usage)
}

pub fn native_usage(home: &Path) -> Result<NativeUsage> {
    let _guard = budget_lock(home, false)?;
    aggregate_native(&read_native(home)?)
}

/// Does not take Store's global writer lock. Rust admission and native reports
/// serialize quota changes on the small, dedicated accounting lock instead.
/// Observed used bytes never expire; ongoing reservations require heartbeats.
pub fn report_native_usage(
    home: &Path,
    id: &str,
    used: u64,
    reserved: u64,
    pinned: u64,
) -> Result<NativeUsage> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) || pinned > used {
        bail!("invalid native cache usage report");
    }
    let _guard = budget_lock(home, true)?;
    let mut ledger = read_native(home)?;
    if !ledger.reports.contains_key(id) && ledger.reports.len() >= 4096 {
        bail!("too many native cache accounting identities");
    }
    let now = chrono::Utc::now().timestamp();
    let previous_reserved = ledger
        .reports
        .get(id)
        .filter(|report| now.saturating_sub(report.updated) <= NATIVE_RESERVATION_TTL)
        .map(|report| report.reserved)
        .unwrap_or(0);
    ledger.reports.insert(
        id.into(),
        NativeReport {
            used,
            reserved,
            pinned,
            updated: now,
        },
    );
    let usage = aggregate_native(&ledger)?;
    let config: crate::config::Config = state::read_json(&home.join("config.json"))?;
    let db: crate::state::Database = state::read_json(&home.join("state.json"))?;
    let payloads = inventory_parts(home, &config, &db.cache)?;
    let rust_used = payloads
        .iter()
        .try_fold(0, |total, payload| add(total, payload.size))?;
    let rust_accounted = add(
        rust_used,
        reservation_remaining_db(&db.cache_reservations, &payloads)?,
    )?;
    let combined = add(
        rust_accounted,
        add(usage.native_used_bytes, usage.native_reserved_bytes)?,
    )?;
    if config.cache.max_bytes.is_some_and(|limit| combined > limit) && reserved > previous_reserved
    {
        // Still record the observed OS payloads, but do not grant extra bytes.
        ledger.reports.get_mut(id).unwrap().reserved = previous_reserved;
        state::atomic_json(&home.join("native-cache-usage.json"), &ledger)?;
        bail!("cache capacity is insufficient for the requested native reservation; existing usage was retained");
    }
    state::atomic_json(&home.join("native-cache-usage.json"), &ledger)?;
    Ok(usage)
}

#[derive(Debug, Clone, Serialize)]
pub struct CachePruneReport {
    pub evicted: usize,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
    pub max_bytes: u64,
    pub over_budget: bool,
}

/// A verified read-only FD with a shared payload lock. It remains valid after
/// Store is dropped, and cooperating evictors leave it in place until all
/// duplicate FDs close. The filesystem path is advisory; copy from this FD.
pub struct CacheLease {
    pub item: CacheItem,
    file: File,
    path: PathBuf,
}
impl CacheLease {
    pub fn file(&self) -> &File {
        &self.file
    }
    pub fn try_clone_file(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

struct Payload {
    object: String,
    path: PathBuf,
    size: u64,
    key: Option<String>,
    pinned: bool,
    accessed: i64,
}

pub fn scope(store: &Store, profile: &str) -> Result<String> {
    let profile = store
        .config
        .profiles
        .get(profile)
        .context("cache profile does not exist")?;
    Ok(state::key(&serde_json::to_string(&(
        &profile.protocol,
        &profile.url,
        &profile.username,
    ))?))
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).context("cache byte count overflow")
}

fn payload_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("cache payload must be an owned regular file");
    }
    Ok(file)
}

fn lock_exclusive(path: &Path) -> Result<Option<File>> {
    lock_exclusive_with(path, flock)
}
fn lock_exclusive_with(path: &Path, lock: impl FnOnce(std::os::fd::RawFd,libc::c_int)->std::io::Result<()>) -> Result<Option<File>> {
    let file = match payload_file(path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let error = match lock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => return Ok(Some(file)),
        Err(error) => error,
    };
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(error.into())
}

fn inventory(store: &Store) -> Result<Vec<Payload>> {
    inventory_parts(&store.home, &store.config, &store.db.cache)
}
fn inventory_parts(
    home: &Path,
    config: &crate::config::Config,
    cache: &BTreeMap<String, CacheItem>,
) -> Result<Vec<Payload>> {
    inventory_at(configured_directory(home, config)?, cache)
}
fn inventory_at(directory: PathBuf, cache: &BTreeMap<String,CacheItem>) -> Result<Vec<Payload>> {
    let metadata = fs::symlink_metadata(&directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("cache directory was replaced or is not a directory");
    }
    let indexed: BTreeMap<_, _> = cache
        .iter()
        .map(|(key, item)| (item.object.as_str(), (key, item)))
        .collect();
    let mut payloads = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let object = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 object in cache directory"))?;
        if crate::cache_directory::is_marker(&object) {
            continue;
        }
        let file = payload_file(&entry.path())
            .context("unsafe or unreadable cache payload; no eviction was performed")?;
        let (key, pinned, accessed) = indexed
            .get(object.as_str())
            .map(|(key, item)| {
                (
                    Some((*key).clone()),
                    item.pinned,
                    chrono::DateTime::parse_from_rfc3339(&item.accessed)
                        .map(|t| t.timestamp_millis())
                        .unwrap_or(i64::MIN),
                )
            })
            .unwrap_or((None, false, i64::MIN));
        payloads.push(Payload {
            object,
            path: entry.path(),
            size: file.metadata()?.len(),
            key,
            pinned,
            accessed,
        });
    }
    Ok(payloads)
}

fn reservation_remaining(store: &Store, payloads: &[Payload]) -> Result<u64> {
    reservation_remaining_db(&store.db.cache_reservations, payloads)
}
fn reservation_remaining_db(
    reservations: &BTreeMap<String, CacheReservation>,
    payloads: &[Payload],
) -> Result<u64> {
    let actual: BTreeMap<_, _> = payloads
        .iter()
        .map(|p| (p.object.as_str(), p.size))
        .collect();
    reservations
        .iter()
        .try_fold(0, |total, (object, reservation)| {
            add(
                total,
                reservation
                    .size
                    .saturating_sub(actual.get(object.as_str()).copied().unwrap_or(0)),
            )
        })
}

pub fn status(store: &Store) -> Result<CacheStatus> {
    let _guard = budget_lock(&store.home, false)?;
    status_parts(&store.home, &store.config, &store.db)
}

/// Available to provider quick requests while a different operation owns Store.
/// Atomic manifests and the dedicated accounting lock provide a consistent
/// admission snapshot without taking the long-lived network-operation lock.
pub fn budget(home: &Path) -> Result<CacheStatus> {
    let _guard = budget_lock(home, false)?;
    let config = state::read_json(&home.join("config.json"))?;
    let db = state::read_json(&home.join("state.json"))?;
    status_parts(home, &config, &db)
}

fn status_parts(
    home: &Path,
    config: &crate::config::Config,
    db: &crate::state::Database,
) -> Result<CacheStatus> {
    let native = aggregate_native(&read_native(home)?)?;
    let payloads = inventory_parts(home, config, &db.cache)?;
    let mut used_bytes = 0;
    let mut pinned_bytes = 0;
    let mut leased_bytes = 0;
    let mut unmanaged_bytes = 0;
    for payload in &payloads {
        used_bytes = add(used_bytes, payload.size)?;
        if payload.pinned {
            pinned_bytes = add(pinned_bytes, payload.size)?;
        }
        if lock_exclusive(&payload.path)?.is_none() {
            leased_bytes = add(leased_bytes, payload.size)?;
        }
        if payload.key.is_none()
            && !db.cache_garbage.contains(&payload.object)
            && !db.cache_reservations.contains_key(&payload.object)
        {
            unmanaged_bytes = add(unmanaged_bytes, payload.size)?;
        }
    }
    let reserved_bytes = reservation_remaining_db(&db.cache_reservations, &payloads)?;
    let rust_accounted_bytes = add(used_bytes, reserved_bytes)?;
    let combined_used_bytes = add(used_bytes, native.native_used_bytes)?;
    let combined_reserved_bytes = add(reserved_bytes, native.native_reserved_bytes)?;
    let accounted_bytes = add(combined_used_bytes, combined_reserved_bytes)?;
    let present: BTreeSet<_> = payloads.iter().map(|p| p.object.as_str()).collect();
    Ok(CacheStatus {
        max_bytes: config.cache.max_bytes, used_bytes, reserved_bytes, accounted_bytes,
        rust_accounted_bytes, native_used_bytes: native.native_used_bytes, native_reserved_bytes: native.native_reserved_bytes,
        native_pinned_bytes: native.native_pinned_bytes, native_accounting_id: native.native_accounting_id,
        combined_used_bytes, combined_reserved_bytes,
        pinned_bytes, leased_bytes, unmanaged_bytes, files: db.cache.len(),
        pinned_files: db.cache.values().filter(|item| item.pinned).count(),
        missing_files: db.cache.values().filter(|item| !present.contains(item.object.as_str())).count(),
        over_budget: config.cache.max_bytes.is_some_and(|limit| accounted_bytes > limit),
        scope: "clean Rust cache and reported macOS materialized payloads/admissions; excludes dirty edits, upload snapshots and temporary transfer copies",
    })
}

/// Called with the Store writer lock held. No cache download can still be using
/// an old admission after its owning Store was dropped or its future cancelled.
pub fn recover(store: &mut Store) -> Result<()> {
    let _guard = budget_lock(&store.home, true)?;
    let payloads = inventory(store)?;
    let mut removed = false;
    let mut retained = BTreeSet::new();
    for payload in payloads {
        if payload.key.is_none()
            && (store.db.cache_garbage.contains(&payload.object)
                || store.db.cache_reservations.contains_key(&payload.object))
        {
            if let Some(_guard) = lock_exclusive(&payload.path)? {
                fs::remove_file(&payload.path)?;
                removed = true;
            } else {
                retained.insert(payload.object);
            }
        }
    }
    if removed {
        File::open(cache_directory(store)?)?.sync_all()?;
    }
    if !store.db.cache_reservations.is_empty() || store.db.cache_garbage != retained {
        // Retain ownership of interrupted reservations with active readers.
        store.db.cache_garbage = retained;
        store.db.cache_reservations.clear();
        store.save()?;
    }
    Ok(())
}

/// Plan while holding exclusive object locks, so quota rejection never first
/// discards usable entries. Persist removal of references before unlinking;
/// a crash can leave reclaimable payloads, never advertised missing content.
fn make_room(
    store: &mut Store,
    limit: u64,
    additional: u64,
    protected: &BTreeSet<String>,
    strict: bool,
) -> Result<CachePruneReport> {
    make_room_with_remover(store,limit,additional,protected,strict,|path|fs::remove_file(path))
}
fn make_room_with_remover(store:&mut Store,limit:u64,additional:u64,protected:&BTreeSet<String>,strict:bool,mut remove:impl FnMut(&Path)->std::io::Result<()>) -> Result<CachePruneReport> {
    let mut payloads = inventory(store)?;
    let actual = payloads
        .iter()
        .try_fold(0, |total, payload| add(total, payload.size))?;
    let native = aggregate_native(&read_native(&store.home)?)?;
    let before = add(
        add(actual, reservation_remaining(store, &payloads)?)?,
        add(native.native_used_bytes, native.native_reserved_bytes)?,
    )?;
    let mut projected = add(before, additional)?;
    payloads.sort_by(|a, b| (a.accessed, &a.object).cmp(&(b.accessed, &b.object)));
    let mut candidates = Vec::new();
    for payload in payloads {
        if projected <= limit {
            break;
        }
        if payload.pinned
            || protected.contains(&payload.object)
            || store.db.cache_reservations.contains_key(&payload.object)
            || (payload.key.is_none() && !store.db.cache_garbage.contains(&payload.object))
        {
            continue;
        }
        if let Some(guard) = lock_exclusive(&payload.path)? {
            projected = projected.saturating_sub(payload.size);
            candidates.push((payload, guard));
        }
    }
    if strict && projected > limit {
        bail!("cache capacity is insufficient: pinned, active or unmanaged payloads and the requested reservation cannot fit; increase the limit or release/unpin existing content");
    }
    let previous_garbage = store.db.cache_garbage.clone();
    let mut removed_items = Vec::new();
    for (payload, _) in &candidates {
        store.db.cache_garbage.insert(payload.object.clone());
        if let Some(key) = &payload.key {
            if let Some(item) = store.db.cache.remove(key) {
                removed_items.push((key.clone(), item));
            }
        }
    }
    if !candidates.is_empty() {
        if let Err(error) = store.save() {
            store.db.cache.extend(removed_items);
            store.db.cache_garbage = previous_garbage;
            return Err(error);
        }
    }
    let mut freed_bytes = 0;
    for (payload, _guard) in &candidates {
        remove(&payload.path).context(
            "cache entry was unindexed but its clean payload could not be removed; retry cleanup",
        )?;
        freed_bytes = add(freed_bytes, payload.size)?;
    }
    if !candidates.is_empty() {
        File::open(cache_directory(store)?)?.sync_all()?;
        for (payload, _) in &candidates {
            store.db.cache_garbage.remove(&payload.object);
        }
        store.save()?;
    }
    let remaining_bytes = before.saturating_sub(freed_bytes);
    Ok(CachePruneReport {
        evicted: candidates.len(),
        freed_bytes,
        remaining_bytes,
        max_bytes: limit,
        over_budget: add(remaining_bytes, additional)? > limit,
    })
}

pub fn prune(store: &mut Store, max_bytes: u64) -> Result<CachePruneReport> {
    recover(store)?;
    let _guard = budget_lock(&store.home, true)?;
    make_room(store, max_bytes, 0, &BTreeSet::new(), false)
}

pub fn set_limit(store: &mut Store, max_bytes: Option<u64>) -> Result<CacheStatus> {
    recover(store)?;
    let guard = budget_lock(&store.home, true)?;
    if let Some(limit) = max_bytes {
        make_room(store, limit, 0, &BTreeSet::new(), true)?;
    }
    let previous = store.config.cache.max_bytes;
    store.config.cache.max_bytes = max_bytes;
    if let Err(error) = store.save_config() {
        store.config.cache.max_bytes = previous;
        return Err(error);
    }
    drop(guard);
    status(store)
}

/// Preview or remove only indexed, unpinned clean payloads. Hold every selected
/// object's exclusive lock before changing state; active readers keep both the
/// index entry and its payload. Foreign files and native OS payloads are ignored.
pub fn clear(store: &mut Store, apply: bool) -> Result<serde_json::Value> {
    let _guard = budget_lock(&store.home, true)?;
    let directory = cache_directory(store)?;
    let mut objects: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, item) in &store.db.cache {
        objects
            .entry(item.object.clone())
            .or_default()
            .push(key.clone());
    }
    let mut candidates = Vec::new();
    let mut skipped_pinned = 0usize;
    let mut skipped_active = 0usize;
    let mut missing_files = 0usize;
    let mut candidate_bytes = 0u64;
    for (object, keys) in objects {
        if keys.iter().any(|key| store.db.cache[key].pinned) {
            skipped_pinned += 1;
            continue;
        }
        let path = object_path(store, &object)?;
        if fs::symlink_metadata(&path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            missing_files += 1;
            continue;
        }
        let Some(file) = lock_exclusive(&path)? else {
            skipped_active += 1;
            continue;
        };
        candidate_bytes = add(candidate_bytes, file.metadata()?.len())?;
        candidates.push((object, keys, path, file));
    }
    if apply && !candidates.is_empty() {
        let previous = store.db.cache.clone();
        let previous_garbage = store.db.cache_garbage.clone();
        for (object, keys, _, _) in &candidates {
            for key in keys {
                store.db.cache.remove(key);
            }
            store.db.cache_garbage.insert(object.clone());
        }
        if let Err(error) = store.save() {
            store.db.cache = previous;
            store.db.cache_garbage = previous_garbage;
            return Err(error);
        }
        for (_, _, path, _) in &candidates {
            fs::remove_file(path)?;
        }
        File::open(&directory)?.sync_all()?;
        for (object, _, _, _) in &candidates {
            store.db.cache_garbage.remove(object);
        }
        store.save()?;
    }
    Ok(serde_json::json!({
        "apply":apply,"directory":directory,"candidates":candidates.len(),"candidate_bytes":candidate_bytes,
        "evicted":if apply {candidates.len()} else {0},"freed_bytes":if apply {candidate_bytes} else {0},
        "skipped_pinned":skipped_pinned,"skipped_active":skipped_active,"missing_files":missing_files,
        "remaining_files":store.db.cache.len(),
        "scope":"indexed clean Rust payloads only; pinned files, active readers, native OS cache, upload snapshots, pending edits and untracked files are retained"
    }))
}

pub(crate) fn hash_fd(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(format!("{:x}", hash.finalize()))
}

fn acquire_item(store: &Store, item: &CacheItem) -> Result<CacheLease> {
    acquire_item_with_hash(store,item,hash_fd)
}
fn acquire_item_with_hash(store:&Store,item:&CacheItem,hash:impl FnOnce(&mut File)->Result<String>)->Result<CacheLease> {
    if item.scope.as_deref() != Some(scope(store, &item.profile)?.as_str()) {
        bail!("cached file belongs to another or an unbound legacy account scope; fetch it online before offline use");
    }
    let path = object_path(store, &item.object)?;
    let mut file =
        payload_file(&path).context("cached file is missing or unsafe; fetch it online")?;
    if flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB).is_err() {
        bail!("cached file is being evicted; retry the read");
    }
    let before = file.metadata()?;
    if before.len() != item.entry.size || hash(&mut file)? != item.sha256 {
        bail!("cache checksum mismatch; local cached copy is corrupt");
    }
    let after = file.metadata()?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    ) {
        bail!("cached file changed while verifying its content");
    }
    Ok(CacheLease {
        item: item.clone(),
        file,
        path,
    })
}

pub fn acquire(store: &mut Store, profile: &str, remote: &str) -> Result<CacheLease> {
    let remote = backend::normalize(remote)?;
    let key = Store::cache_key(profile, &remote);
    let item = store
        .db
        .cache
        .get(&key)
        .context("file is not cached; fetch or pin it while online")?
        .clone();
    let mut lease = acquire_item(store, &item)?;
    lease.item.accessed = state::now();
    store.db.cache.get_mut(&key).unwrap().accessed = lease.item.accessed.clone();
    store.save()?;
    Ok(lease)
}

pub fn copy_lease(lease: &CacheLease, destination: &Path, overwrite: bool) -> Result<()> {
    let temporary = crate::operations::destination_temp(destination)?;
    let result = (|| {
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        let mut hash = Sha256::new();
        let mut offset = 0u64;
        let mut buffer = [0u8; 128 * 1024];
        while offset < lease.item.entry.size {
            let wanted = (lease.item.entry.size - offset).min(buffer.len() as u64) as usize;
            let count = lease.file.read_at(&mut buffer[..wanted], offset)?;
            if count == 0 {
                bail!("cached content ended early while reading");
            }
            target.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            offset = add(offset, count as u64)?;
        }
        if lease.file.metadata()?.len() != offset
            || format!("{:x}", hash.finalize()) != lease.item.sha256
        {
            bail!("cached content changed while reading");
        }
        target.sync_all()?;
        state::commit_download(&temporary, destination, overwrite)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn get(
    store: &mut Store,
    profile: &str,
    remote: &str,
    destination: &Path,
    overwrite: bool,
) -> Result<CacheItem> {
    let lease = acquire(store, profile, remote)?;
    copy_lease(&lease, destination, overwrite)?;
    Ok(lease.item)
}

pub fn set_pinned(
    store: &mut Store,
    profile: &str,
    remote: &str,
    pinned: bool,
    recursive: bool,
) -> Result<usize> {
    recover(store)?;
    let _guard = budget_lock(&store.home, true)?;
    let remote = backend::normalize(remote)?;
    let keys: Vec<_> = store
        .db
        .cache
        .iter()
        .filter(|(_, item)| {
            item.profile == profile
                && (item.remote == remote
                    || recursive
                        && (remote.is_empty() || item.remote.starts_with(&format!("{remote}/"))))
        })
        .map(|(key, _)| key.clone())
        .collect();
    if pinned {
        if let Some(limit) = store.config.cache.max_bytes {
            let protected = keys
                .iter()
                .map(|key| store.db.cache[key].object.clone())
                .collect();
            make_room(store, limit, 0, &protected, true)?;
        }
    }
    for key in &keys {
        store.db.cache.get_mut(key).unwrap().pinned = pinned;
    }
    store.save()?;
    if !pinned {
        if let Some(limit) = store.config.cache.max_bytes {
            make_room(store, limit, 0, &BTreeSet::new(), false)?;
        }
    }
    Ok(keys.len())
}

pub async fn fetch(
    store: &mut Store,
    backend: &dyn Backend,
    profile: &str,
    remote: &str,
    pin: bool,
) -> Result<CacheItem> {
    let remote = backend::normalize(remote)?;
    let expected = backend
        .stat(&remote)
        .await?
        .context("remote file does not exist")?;
    fetch_expected(store, backend, profile, &remote, &expected, pin).await
}

/// Bind both the admission and streamed read to this exact size/revision. A
/// backend must not write beyond expected.size in download_checked.
pub async fn fetch_expected(
    store: &mut Store,
    backend: &dyn Backend,
    profile: &str,
    remote: &str,
    expected: &Entry,
    pin: bool,
) -> Result<CacheItem> {
    recover(store)?;
    let remote = backend::normalize(remote)?;
    if expected.is_dir || expected.path != remote {
        bail!("cache admission requires the matching regular file revision");
    }
    let scope = scope(store, profile)?;
    let key = Store::cache_key(profile, &remote);
    if let Some(cached) = store.db.cache.get(&key).cloned() {
        if expected.same_revision(&cached.entry) && cached.scope.as_deref() == Some(scope.as_str())
        {
            if let Ok(lease) = acquire_item(store, &cached) {
                let _guard = budget_lock(&store.home, true)?;
                if let Some(limit) = store.config.cache.max_bytes {
                    make_room(store, limit, 0, &BTreeSet::new(), true)?;
                }
                let mut item = cached;
                item.pinned |= pin;
                item.accessed = state::now();
                store.db.cache.insert(key, item.clone());
                store.save()?;
                drop(lease);
                return Ok(item);
            }
        }
    }
    let guard = budget_lock(&store.home, true)?;
    if let Some(limit) = store.config.cache.max_bytes {
        make_room(store, limit, expected.size, &BTreeSet::new(), true)?;
    }
    let object = uuid::Uuid::new_v4().to_string();
    let target = object_path(store, &object)?;
    store.db.cache_reservations.insert(
        object.clone(),
        CacheReservation {
            size: expected.size,
            created: state::now(),
        },
    );
    if let Err(error) = store.save() {
        store.db.cache_reservations.remove(&object);
        return Err(error);
    }
    drop(guard);
    let downloaded: Result<CacheItem> = async {
        backend.download_checked(&remote, &target, expected).await?;
        let after = backend.stat(&remote).await?.context("remote file disappeared during cache fetch")?;
        if !expected.same_revision(&after) { bail!("remote file changed or lacks a verifiable revision; cache admission was not committed"); }
        let mut file = payload_file(&target)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        if file.metadata()?.len() != expected.size { bail!("cache payload length differs from its reservation"); }
        let sha256 = hash_fd(&mut file)?;
        if backend.content_sha256(expected).is_some_and(|expected_hash| expected_hash != sha256) { bail!("cache payload hash differs from its protected remote revision"); }
        file.sync_all()?;
        File::open(cache_directory(store)?)?.sync_all()?;
        Ok(CacheItem { profile: profile.into(), remote: remote.clone(), object: object.clone(), entry: after, sha256, pinned: pin || store.db.cache.get(&key).is_some_and(|item| item.pinned), accessed: state::now(), scope: Some(scope) })
    }.await;
    let item = match downloaded {
        Ok(item) => item,
        Err(error) => {
            let _guard = budget_lock(&store.home, true)?;
            if target.exists() {
                fs::remove_file(&target)?;
                File::open(cache_directory(store)?)?.sync_all()?;
            }
            store.db.cache_reservations.remove(&object);
            store.save()?;
            return Err(error);
        }
    };
    let guard = budget_lock(&store.home, true)?;
    let previous = store.db.cache.insert(key.clone(), item.clone());
    if let Some(previous) = &previous {
        store.db.cache_garbage.insert(previous.object.clone());
    }
    store.db.cache_reservations.remove(&object);
    if let Err(error) = store.save() {
        store.db.cache.remove(&key);
        if let Some(previous) = previous {
            store.db.cache_garbage.remove(&previous.object);
            store.db.cache.insert(key, previous);
        }
        store.db.cache_reservations.insert(
            object,
            CacheReservation {
                size: expected.size,
                created: state::now(),
            },
        );
        return Err(error);
    }
    drop(guard);
    // Old payloads can still have active readers. Recovery only removes them
    // after the exclusive payload lock is available, and always counts them.
    recover(store)?;
    let _guard = budget_lock(&store.home, true)?;
    if let Some(limit) = store.config.cache.max_bytes {
        make_room(
            store,
            limit,
            0,
            &BTreeSet::from([item.object.clone()]),
            true,
        )?;
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{Capabilities, WriteCondition},
        config::{Profile, Protocol},
    };
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Memory {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
        downloads: AtomicUsize,
        fail: AtomicBool,
        pause: AtomicBool,
        started: Notify,
        poison_state: Mutex<Option<PathBuf>>,
        payload_override: Mutex<Option<Vec<u8>>>,
        hash_override: Mutex<Option<String>>,
        after_download: Mutex<Option<Option<Vec<u8>>>>,
        fail_before: AtomicBool,
        after_usage: Mutex<Option<(PathBuf,u64)>>,
    }
    impl Memory {
        fn put(&self, path: &str, data: &[u8]) {
            self.files.lock().unwrap().insert(path.into(), data.into());
        }
    }
    #[async_trait]
    impl Backend for Memory {
        fn content_sha256(&self,_:&Entry)->Option<String> { self.hash_override.lock().unwrap().clone() }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                protocol: "test",
                conditional_write: true,
                server_copy: false,
                locking: false,
                notes: vec![],
            }
        }
        async fn stat(&self, path: &str) -> Result<Option<Entry>> {
            Ok(self.files.lock().unwrap().get(path).map(|data| Entry {
                path: path.into(),
                is_dir: false,
                size: data.len() as u64,
                modified: None,
                etag: Some(format!("\"{:x}\"", Sha256::digest(data))),
            }))
        }
        async fn list(&self, _: &str) -> Result<Vec<Entry>> {
            bail!("unused")
        }
        async fn download(&self, remote: &str, local: &Path) -> Result<()> {
            if self.fail_before.load(Ordering::SeqCst) {bail!("synthetic failure before payload creation");}
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let data = self.payload_override.lock().unwrap().clone().unwrap_or_else(||self.files.lock().unwrap()[remote].clone());
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(local)?;
            if self.pause.load(Ordering::SeqCst) || self.fail.load(Ordering::SeqCst) {
                file.write_all(&data[..data.len().min(2)])?;
                file.sync_all()?;
                self.started.notify_one();
                if self.pause.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                bail!("injected interrupted payload");
            }
            file.write_all(&data)?;
            if let Some((home,used))=self.after_usage.lock().unwrap().take() {
                state::atomic_json(&home.join("native-cache-usage.json"),&NativeLedger{version:1,reports:BTreeMap::from([("native-race".into(),NativeReport{used,reserved:0,pinned:0,updated:chrono::Utc::now().timestamp()})])})?;
            }
            if let Some(next) = self.after_download.lock().unwrap().take() {
                if let Some(bytes)=next { self.files.lock().unwrap().insert(remote.into(),bytes); } else {self.files.lock().unwrap().remove(remote);}
            }
            if let Some(path) = self.poison_state.lock().unwrap().take() {
                fs::remove_file(&path)?;
                fs::create_dir(path)?;
            }
            Ok(())
        }
        async fn upload(&self, _: &Path, _: &str, _: WriteCondition) -> Result<Entry> {
            bail!("unused")
        }
        async fn mkdir(&self, _: &str) -> Result<()> {
            bail!("unused")
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
    fn setup(home: &Path) -> Store {
        let mut store = Store::open(home.to_owned()).unwrap();
        store.config.profiles.insert(
            "test".into(),
            Profile {
                protocol: Protocol::Webdav,
                url: "https://example.com/root/".into(),
                username: Some("account-a".into()),
                password_env: None,
                allow_http: false,
                timeout_secs: 5,
                smb_encryption: true,
                nextcloud_upload_url: None,
            },
        );
        store.save_config().unwrap();
        store
    }

    #[tokio::test]
    async fn persistent_limit_evicts_lru_preserves_pins_and_never_touches_dirty_spool() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("state");
        let mut store = setup(&home);
        let backend = Memory::default();
        for (path, data) in [("pinned", b"aaa"), ("old", b"bbb"), ("new", b"ccc")] {
            backend.put(path, data);
        }
        let pinned = fetch(&mut store, &backend, "test", "pinned", true)
            .await
            .unwrap();
        let old = fetch(&mut store, &backend, "test", "old", false)
            .await
            .unwrap();
        fs::write(
            store.home.join("spool/dirty-upload"),
            b"pending offline edits remain durable",
        )
        .unwrap();
        set_limit(&mut store, Some(6)).unwrap();
        let newest = fetch(&mut store, &backend, "test", "new", false)
            .await
            .unwrap();
        assert_eq!(status(&store).unwrap().used_bytes, 6);
        assert!(!store.home.join("cache").join(old.object).exists());
        assert!(store.home.join("cache").join(pinned.object).exists());
        assert!(store.home.join("cache").join(newest.object).exists());
        assert!(store.home.join("spool/dirty-upload").exists());
        drop(store);
        let restored = Store::open(home).unwrap();
        assert_eq!(restored.config.cache.max_bytes, Some(6));
        assert_eq!(status(&restored).unwrap().pinned_bytes, 3);
    }

    #[tokio::test]
    async fn impossible_reduction_and_oversize_fetch_preserve_existing_cache() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default();
        backend.put("pinned", b"aaa");
        backend.put("other", b"bbb");
        backend.put("oversize", b"xxxxxxx");
        fetch(&mut store, &backend, "test", "pinned", true)
            .await
            .unwrap();
        fetch(&mut store, &backend, "test", "other", false)
            .await
            .unwrap();
        assert!(set_limit(&mut store, Some(2)).is_err());
        assert_eq!(store.config.cache.max_bytes, None);
        assert_eq!(store.db.cache.len(), 2);
        assert_eq!(status(&store).unwrap().used_bytes, 6);
        set_limit(&mut store, Some(6)).unwrap();
        assert!(fetch(&mut store, &backend, "test", "oversize", false)
            .await
            .is_err());
        assert_eq!(
            backend.downloads.load(Ordering::SeqCst),
            2,
            "over-budget file was downloaded before admission"
        );
        assert_eq!(
            store.db.cache.len(),
            2,
            "rejected admission evicted usable files"
        );
        assert!(store.db.cache_reservations.is_empty());
    }

    #[tokio::test]
    async fn leased_fd_survives_store_restart_and_prevents_eviction_until_all_clones_close() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("state");
        let mut store = setup(&home);
        let backend = Memory::default();
        backend.put("file", b"hello");
        let item = fetch(&mut store, &backend, "test", "file", false)
            .await
            .unwrap();
        let lease = acquire(&mut store, "test", "file").unwrap();
        let mut reader = lease.try_clone_file().unwrap();
        drop(lease);
        drop(store);
        let mut store = Store::open(home).unwrap();
        assert!(set_limit(&mut store, Some(0)).is_err());
        let report = prune(&mut store, 0).unwrap();
        assert!(report.over_budget && report.evicted == 0);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"hello");
        assert!(store.home.join("cache").join(&item.object).exists());
        drop(reader);
        assert_eq!(prune(&mut store, 0).unwrap().freed_bytes, 5);
        assert!(!store.home.join("cache").join(item.object).exists());
    }

    #[tokio::test]
    async fn account_changes_and_legacy_entries_cannot_reuse_same_etag_offline() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default();
        backend.put("file", b"private");
        fetch(&mut store, &backend, "test", "file", false)
            .await
            .unwrap();
        store.config.profiles.get_mut("test").unwrap().username = Some("account-b".into());
        assert!(acquire(&mut store, "test", "file").is_err());
        fetch(&mut store, &backend, "test", "file", false)
            .await
            .unwrap();
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 2);
        assert_eq!(
            acquire(&mut store, "test", "file").unwrap().item.scope,
            Some(scope(&store, "test").unwrap())
        );
        store.db.cache.values_mut().next().unwrap().scope = None;
        assert!(acquire(&mut store, "test", "file").is_err());
    }

    #[tokio::test]
    async fn interruption_releases_reservation_and_keeps_pinned_old_revision() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default();
        backend.put("file", b"old");
        let old = fetch(&mut store, &backend, "test", "file", true)
            .await
            .unwrap();
        set_limit(&mut store, Some(6)).unwrap();
        backend.put("file", b"new");
        backend.fail.store(true, Ordering::SeqCst);
        assert!(fetch(&mut store, &backend, "test", "file", false)
            .await
            .is_err());
        assert!(store.db.cache_reservations.is_empty());
        assert_eq!(status(&store).unwrap().used_bytes, 3);
        assert_eq!(
            fs::read(store.home.join("cache").join(old.object)).unwrap(),
            b"old"
        );
        assert!(acquire(&mut store, "test", "file").is_ok());
    }

    #[tokio::test]
    async fn cancelled_fetch_and_native_quick_reports_share_atomic_admission_budget() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("state");
        let mut store = setup(&home);
        set_limit(&mut store, Some(10)).unwrap();
        fs::write(
            store.home.join("spool/pending-edit"),
            b"dirty content is excluded",
        )
        .unwrap();
        let backend = Arc::new(Memory::default());
        backend.put("file", b"123456");
        backend.pause.store(true, Ordering::SeqCst);
        let source = backend.clone();
        let task =
            tokio::spawn(
                async move { fetch(&mut store, source.as_ref(), "test", "file", false).await },
            );
        backend.started.notified().await;
        let live = budget(&home).unwrap();
        assert_eq!(
            (live.used_bytes, live.reserved_bytes, live.accounted_bytes),
            (2, 4, 6)
        );
        assert!(report_native_usage(&home, "domain-a", 0, 5, 0).is_err());
        assert_eq!(
            report_native_usage(&home, "domain-a", 0, 4, 0)
                .unwrap()
                .native_reserved_bytes,
            4
        );
        assert_eq!(budget(&home).unwrap().accounted_bytes, 10);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let store = Store::open(home).unwrap();
        assert!(store.db.cache_reservations.is_empty());
        assert_eq!(status(&store).unwrap().used_bytes, 0);
        assert_eq!(
            fs::read(store.home.join("spool/pending-edit")).unwrap(),
            b"dirty content is excluded"
        );
    }

    #[test]
    fn native_reservation_race_grants_only_one_and_expired_reservations_keep_used_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        set_limit(&mut store, Some(10)).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|id| {
                let home = store.home.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    report_native_usage(&home, id, 0, 6, 0).is_ok()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|succeeded| *succeeded)
                .count(),
            1
        );
        assert_eq!(native_usage(&store.home).unwrap().native_reserved_bytes, 6);
        let old = NativeLedger {
            version: 1,
            reports: BTreeMap::from([(
                "old-domain".into(),
                NativeReport {
                    used: 4,
                    reserved: 6,
                    pinned: 2,
                    updated: chrono::Utc::now().timestamp() - 121,
                },
            )]),
        };
        state::atomic_json(&store.home.join("native-cache-usage.json"), &old).unwrap();
        let usage = native_usage(&store.home).unwrap();
        assert_eq!(
            (
                usage.native_used_bytes,
                usage.native_reserved_bytes,
                usage.native_pinned_bytes
            ),
            (4, 0, 2)
        );
        assert!(set_limit(&mut store, Some(3)).is_err());
        assert_eq!(store.config.cache.max_bytes, Some(10));
        set_limit(&mut store, Some(4)).unwrap();
    }

    #[test]
    fn native_observed_overage_is_reported_and_unknown_payload_is_not_silently_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        set_limit(&mut store, Some(5)).unwrap();
        report_native_usage(&store.home, "domain", 7, 0, 2).unwrap();
        assert!(budget(&store.home).unwrap().over_budget);
        assert!(report_native_usage(&store.home, "domain", 7, 1, 2).is_err());
        assert_eq!(native_usage(&store.home).unwrap().native_used_bytes, 7);
        report_native_usage(&store.home, "domain", 0, 0, 0).unwrap();
        let unknown = store.home.join("cache/user-file");
        fs::write(&unknown, b"123456").unwrap();
        assert!(set_limit(&mut store, Some(5)).is_err());
        assert!(unknown.exists());
        assert_eq!(status(&store).unwrap().unmanaged_bytes, 6);
    }

    #[test]
    fn legacy_configuration_keeps_unlimited_until_explicitly_set() {
        let config: crate::config::Config = serde_json::from_str(r#"{"profiles":{}}"#).unwrap();
        assert_eq!(config.cache.max_bytes, None);
    }

    #[tokio::test]
    async fn cache_reuse_recursive_pins_and_copy_read_from_verified_lease() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default();
        for path in ["folder/a", "folder/b", "other"] { backend.put(path, b"abc"); }
        for path in ["folder/a", "folder/b", "other"] { fetch(&mut store, &backend, "test", path, false).await.unwrap(); }
        set_limit(&mut store, Some(9)).unwrap();
        assert_eq!(set_pinned(&mut store, "test", "folder", true, true).unwrap(), 2);
        assert_eq!(set_pinned(&mut store, "test", "folder/a", false, false).unwrap(), 1);
        assert_eq!(set_pinned(&mut store, "test", "missing", true, false).unwrap(), 0);
        assert_eq!(status(&store).unwrap().pinned_files, 1);
        let reuse = fetch(&mut store, &backend, "test", "folder/a", true).await.unwrap();
        assert!(reuse.pinned);
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 3, "same revision must not redownload");
        let lease = acquire(&mut store, "test", "folder/a").unwrap();
        assert_eq!(lease.path(), object_path(&store, &reuse.object).unwrap());
        assert_eq!(lease.file().metadata().unwrap().len(), 3);
        assert_eq!(status(&store).unwrap().leased_bytes, 3);
        let output = directory.path().join("copy");
        copy_lease(&lease, &output, false).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"abc");
        assert!(copy_lease(&lease, &output, false).is_err());
        drop(lease);
        let output2 = directory.path().join("get");
        assert_eq!(get(&mut store, "test", "folder/a", &output2, false).unwrap().object, reuse.object);
        assert_eq!(fs::read(output2).unwrap(), b"abc");
        let previous_reader=acquire(&mut store,"test","folder/a").unwrap();backend.put("folder/a",b"new");
        let replacement=fetch(&mut store,&backend,"test","folder/a",true).await.unwrap();
        assert_ne!(replacement.object,reuse.object);assert!(store.db.cache_garbage.contains(&reuse.object));
        assert_eq!(status(&store).unwrap().leased_bytes,3);drop(previous_reader);recover(&mut store).unwrap();
        assert!(!store.db.cache_garbage.contains(&reuse.object));assert!(!object_path(&store,&reuse.object).unwrap().exists());
        set_pinned(&mut store, "test", "", false, true).unwrap();
        assert_eq!(status(&store).unwrap().pinned_files, 0);
        set_limit(&mut store, None).unwrap();
    }

    #[tokio::test]
    async fn corrupt_payload_and_concurrent_exclusive_lease_are_refused_without_copying() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default(); backend.put("file", b"abc");
        let item = fetch(&mut store, &backend, "test", "file", false).await.unwrap();
        let path = object_path(&store, &item.object).unwrap();
        let lock = lock_exclusive(&path).unwrap().unwrap();
        assert!(acquire(&mut store, "test", "file").err().unwrap().to_string().contains("being evicted"));
        drop(lock);
        let lease = acquire(&mut store, "test", "file").unwrap();
        fs::write(&path, b"").unwrap();
        let output = directory.path().join("refused-short");
        assert!(copy_lease(&lease, &output, false).unwrap_err().to_string().contains("ended early"));
        assert!(!output.exists());
        fs::write(&path, b"xyz").unwrap();
        assert!(copy_lease(&lease, &output, false).unwrap_err().to_string().contains("changed while reading"));
        assert!(acquire(&mut store, "test", "file").err().unwrap().to_string().contains("checksum mismatch"));
        drop(lease);
        let repaired = fetch(&mut store, &backend, "test", "file", false).await.unwrap();
        assert_ne!(repaired.object, item.object);
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 2);
        fs::remove_file(object_path(&store, &repaired.object).unwrap()).unwrap();
        assert_eq!(clear(&mut store, false).unwrap()["missing_files"], 1);
        assert_eq!(status(&store).unwrap().missing_files, 1);
    }

    #[tokio::test]
    async fn state_write_failures_restore_cache_index_limits_and_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let backend = Memory::default(); backend.put("file", b"abc");
        let original = fetch(&mut store, &backend, "test", "file", false).await.unwrap();
        let state_path = store.home.join("state.json");
        let saved = fs::read(&state_path).unwrap();
        fs::remove_file(&state_path).unwrap(); fs::create_dir(&state_path).unwrap();
        assert!(clear(&mut store, true).is_err());
        assert_eq!(store.db.cache.len(), 1); assert!(store.db.cache_garbage.is_empty());
        assert!(prune(&mut store, 0).is_err());
        assert_eq!(store.db.cache.len(), 1); assert!(store.db.cache_garbage.is_empty());
        assert!(object_path(&store, &original.object).unwrap().exists());
        backend.put("new", b"new");
        assert!(fetch(&mut store, &backend, "test", "new", false).await.is_err());
        assert!(store.db.cache_reservations.is_empty());
        fs::remove_dir(&state_path).unwrap(); fs::write(&state_path, &saved).unwrap();
        let config_path = store.home.join("config.json");
        let config = fs::read(&config_path).unwrap();
        fs::remove_file(&config_path).unwrap(); fs::create_dir(&config_path).unwrap();
        assert!(set_limit(&mut store, Some(99)).is_err());
        assert_eq!(store.config.cache.max_bytes, None);
        fs::remove_dir(&config_path).unwrap(); fs::write(config_path, config).unwrap();
        backend.put("file", b"updated");
        *backend.poison_state.lock().unwrap() = Some(state_path.clone());
        assert!(fetch(&mut store, &backend, "test", "file", false).await.is_err());
        assert_eq!(store.db.cache[&Store::cache_key("test", "file")].object, original.object);
        assert_eq!(store.db.cache_reservations.len(), 1);
        assert!(store.db.cache_garbage.is_empty());
        fs::remove_dir(&state_path).unwrap(); fs::write(&state_path, saved).unwrap();
        recover(&mut store).unwrap();
        assert!(store.db.cache_reservations.is_empty());
        assert_eq!(fs::read(object_path(&store, &original.object).unwrap()).unwrap(), b"abc");
    }

    #[test]
    fn unsafe_accounting_files_and_malformed_native_reports_fail_closed() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let mut store = setup(&directory.path().join("state"));
        let path = store.home.join("native-cache-usage.json");
        assert!(add(u64::MAX, 1).is_err());
        assert!(lock_exclusive(&directory.path().join("absent")).unwrap().is_none());
        assert!(payload_file(directory.path()).is_err());
        assert!(lock_exclusive(directory.path()).is_err());
        let accounting_lock=store.home.join("cache-budget.lock");fs::remove_file(&accounting_lock).unwrap();
        let fifo=std::ffi::CString::new(accounting_lock.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe {libc::mkfifo(fifo.as_ptr(),0o600)},0);
        assert!(budget_lock(&store.home,false).unwrap_err().to_string().contains("not a regular file"));
        fs::remove_file(accounting_lock).unwrap();
        for id in ["", "\n", &"x".repeat(257)] { assert!(report_native_usage(&store.home, id, 0, 0, 0).is_err()); }
        assert!(report_native_usage(&store.home, "x", 0, 0, 1).is_err());
        for value in [serde_json::json!({"version":2,"reports":{}}), serde_json::json!({"version":1,"reports":{"":{"used":0,"reserved":0,"pinned":0,"updated":1}}}), serde_json::json!({"version":1,"reports":{"x":{"used":0,"reserved":0,"pinned":1,"updated":1}}}), serde_json::json!({"version":1,"reports":{"x":{"used":0,"reserved":0,"pinned":0,"updated":-1}}})] {
            state::atomic_json(&path, &value).unwrap(); assert!(read_native(&store.home).is_err());
        }
        fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap(); assert!(read_native(&store.home).is_err());
        fs::remove_file(&path).unwrap(); symlink(directory.path().join("missing"), &path).unwrap(); assert!(read_native(&store.home).is_err());
        fs::remove_file(&path).unwrap();
        let mut ledger = NativeLedger::default();
        for i in 0..4096 { ledger.reports.insert(i.to_string(), NativeReport { used:0, reserved:0, pinned:0, updated:1 }); }
        state::atomic_json(&path, &ledger).unwrap();
        assert!(report_native_usage(&store.home, "overflow", 0, 0, 0).unwrap_err().to_string().contains("too many"));
        ledger.reports.insert("overflow".into(), NativeReport { used:0, reserved:0, pinned:0, updated:1 });
        state::atomic_json(&path, &ledger).unwrap(); assert!(read_native(&store.home).is_err());
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&store.home, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(budget_lock(&store.home, false).is_err());
        fs::set_permissions(&store.home, fs::Permissions::from_mode(0o700)).unwrap();
        let cache = store.home.join("cache");
        fs::remove_dir(&cache).unwrap(); fs::write(&cache, b"foreign").unwrap();
        assert!(inventory(&store).is_err());
        fs::remove_file(&cache).unwrap(); fs::create_dir(&cache).unwrap(); fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
        let foreign = cache.join("directory"); fs::create_dir(&foreign).unwrap();
        assert!(inventory(&store).is_err()); fs::remove_dir(foreign).unwrap();
        assert!(set_limit(&mut store, Some(0)).is_ok());
    }

    #[tokio::test]
    async fn invalid_downloads_are_removed_and_never_advertised_as_cached() {
        let temporary=tempfile::tempdir().unwrap();let mut store=setup(&temporary.path().join("state"));let backend=Memory::default();backend.put("file",b"abc");
        let mut invalid=backend.stat("file").await.unwrap().unwrap();invalid.is_dir=true;
        assert!(fetch_expected(&mut store,&backend,"test","file",&invalid,false).await.unwrap_err().to_string().contains("matching regular file"));
        *backend.payload_override.lock().unwrap()=Some(b"ab".to_vec());
        assert!(fetch(&mut store,&backend,"test","file",false).await.unwrap_err().to_string().contains("length differs"));
        *backend.payload_override.lock().unwrap()=None;*backend.hash_override.lock().unwrap()=Some("0".repeat(64));
        assert!(fetch(&mut store,&backend,"test","file",false).await.unwrap_err().to_string().contains("hash differs"));
        *backend.hash_override.lock().unwrap()=None;*backend.after_download.lock().unwrap()=Some(Some(b"new".to_vec()));
        assert!(fetch(&mut store,&backend,"test","file",false).await.unwrap_err().to_string().contains("remote file changed"));
        *backend.after_download.lock().unwrap()=Some(None);
        assert!(fetch(&mut store,&backend,"test","file",false).await.unwrap_err().to_string().contains("disappeared"));
        assert!(store.db.cache.is_empty());assert!(store.db.cache_reservations.is_empty());assert_eq!(status(&store).unwrap().used_bytes,0);
    }
    #[test]
    fn lock_failures_remain_errors_without_misclassifying_other_process_readers() {
        let temporary=tempfile::tempdir().unwrap();let store=setup(&temporary.path().join("state"));
        assert!(flock(-1,libc::LOCK_EX).is_err());
        assert!(budget_lock_with(&store.home,true,|_,mode|{assert_eq!(mode,libc::LOCK_EX);Err(std::io::Error::other("synthetic locking failure"))}).is_err());
        let payload=store.home.join("cache/payload");fs::write(&payload,b"data").unwrap();
        assert!(lock_exclusive_with(&payload,|_,_|Err(std::io::Error::other("synthetic locking failure"))).is_err());
        assert!(lock_exclusive_with(&payload,|_,_|Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))).unwrap().is_none());
    }
    #[tokio::test]
    async fn unlimited_pins_reuse_and_native_overage_keep_verified_cache_records() {
        let temporary=tempfile::tempdir().unwrap();let mut store=setup(&temporary.path().join("state"));let backend=Memory::default();backend.put("file",b"abc");
        fetch(&mut store,&backend,"test","file",false).await.unwrap();set_pinned(&mut store,"test","file",true,false).unwrap();set_pinned(&mut store,"test","file",false,false).unwrap();fetch(&mut store,&backend,"test","file",false).await.unwrap();assert_eq!(backend.downloads.load(Ordering::SeqCst),1);
        backend.fail_before.store(true,Ordering::SeqCst);backend.put("failed",b"xyz");assert!(fetch(&mut store,&backend,"test","failed",false).await.is_err());assert!(store.db.cache_reservations.is_empty());backend.fail_before.store(false,Ordering::SeqCst);
        set_limit(&mut store,Some(3)).unwrap();backend.put("new",b"new");*backend.after_usage.lock().unwrap()=Some((store.home.clone(),1));
        assert!(fetch(&mut store,&backend,"test","new",false).await.unwrap_err().to_string().contains("cache capacity"));assert!(store.db.cache.contains_key(&Store::cache_key("test","new")));assert!(status(&store).unwrap().over_budget);
    }
    #[tokio::test]
    async fn cache_mutation_during_hashing_and_failed_unlink_preserve_recovery_ownership() {
        let temporary=tempfile::tempdir().unwrap();let mut store=setup(&temporary.path().join("state"));let backend=Memory::default();backend.put("file",b"abc");let item=fetch(&mut store,&backend,"test","file",false).await.unwrap();let path=object_path(&store,&item.object).unwrap();
        let error=acquire_item_with_hash(&store,&item,|file|{let hash=hash_fd(file)?;fs::write(&path,b"new")?;File::open(&path)?.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))?;Ok(hash)}).err().unwrap();assert!(error.to_string().contains("changed while verifying"));
        fs::write(&path,b"abc").unwrap();
        let result=make_room_with_remover(&mut store,0,0,&BTreeSet::new(),false,|_|Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)));assert!(result.unwrap_err().to_string().contains("could not be removed"));assert!(path.exists());assert!(store.db.cache.is_empty());assert!(store.db.cache_garbage.contains(&item.object));
        let recovered=make_room(&mut store,0,0,&BTreeSet::new(),false).unwrap();assert_eq!(recovered.freed_bytes,3);assert!(!path.exists());assert!(store.db.cache_garbage.is_empty());
        let foreign=store.home.join("foreign-file");fs::write(&foreign,b"not a directory").unwrap();assert!(inventory_at(foreign,&BTreeMap::new()).is_err());
    }
    #[test]
    fn native_and_reserved_byte_counter_overflows_are_rejected_without_wrapping() {
        let temporary=tempfile::tempdir().unwrap();let mut store=setup(&temporary.path().join("state"));fs::write(store.home.join("cache/untracked"),b"x").unwrap();
        store.db.cache_reservations.insert("reservation".into(),CacheReservation{size:u64::MAX,created:state::now()});store.save().unwrap();assert!(report_native_usage(&store.home,"native",0,0,0).unwrap_err().to_string().contains("overflow"));store.db.cache_reservations.clear();store.save().unwrap();
        assert!(report_native_usage(&store.home,"native",u64::MAX,0,0).unwrap_err().to_string().contains("overflow"));
        state::atomic_json(&store.home.join("native-cache-usage.json"),&NativeLedger{version:1,reports:BTreeMap::from([("native".into(),NativeReport{used:u64::MAX,reserved:1,pinned:0,updated:chrono::Utc::now().timestamp()})])}).unwrap();assert!(make_room(&mut store,u64::MAX,0,&BTreeSet::new(),false).unwrap_err().to_string().contains("overflow"));
        // Each accounting source fits individually, but their combined total
        // must also fail without unlinking the existing local payload.
        state::atomic_json(&store.home.join("native-cache-usage.json"),&NativeLedger{version:1,reports:BTreeMap::from([("native".into(),NativeReport{used:u64::MAX,reserved:0,pinned:0,updated:chrono::Utc::now().timestamp()})])}).unwrap();
        assert!(make_room(&mut store,u64::MAX,0,&BTreeSet::new(),false).unwrap_err().to_string().contains("overflow"));
        assert_eq!(fs::read(store.home.join("cache/untracked")).unwrap(),b"x");
    }

}
