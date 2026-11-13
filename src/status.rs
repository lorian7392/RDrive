//! Bounded, read-only local status. User metadata is never recovered, migrated,
//! mounted or initialized by this collector. SQLite reads use private copies of
//! an unchanged main database and WAL, so inspection never creates live sidecars.
use crate::{config::Config, provider::Domain, state};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_JSON: u64 = 64 * 1024 * 1024;
const MAX_SQLITE: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const DEFAULT_ITEMS: usize = 10;

#[derive(Debug)]
struct Unavailable(&'static str);
impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Unavailable {}
fn unavailable<T>(code: &'static str) -> Result<T> {
    Err(Unavailable(code).into())
}
fn failure(error: &anyhow::Error) -> Value {
    let code = error
        .downcast_ref::<Unavailable>()
        .map(|v| v.0)
        .unwrap_or_else(|| {
            if error.to_string() == "Smart domain/backend scope changed; local edits preserved" {
                "scope_changed"
            } else {
                "metadata_unavailable"
            }
        });
    let message=match code {
        "snapshot_changed"=>"Metadata changed during inspection; retry for a fresh snapshot.",
        "private_path_required"=>"Status metadata must be private, owned regular files and real directories without links.",
        "limit_exceeded"=>"Status metadata exceeds the bounded inspection limit.",
        "scope_changed"=>"Stored metadata belongs to another connection or account; it was not used.",
        "schema_unsupported"=>"Existing metadata needs initialization or migration; status did not change it.",
        "invalid_configuration"=>"The connection configuration is invalid or unreadable; scoped status is unavailable.",
        "invalid_metadata"=>"The local status record is invalid; existing data was preserved.",
        _=>"This component could not be read; existing data was preserved.",
    };
    json!({"code":code,"message":message})
}
fn section(source: &str, scope: &str, result: Result<Option<Value>>) -> Value {
    match result {
        Ok(data) => {
            json!({"status":if data.as_ref().is_some_and(|v|v["error_components"].as_u64().unwrap_or(0)>0){"partial"}else if data.is_some(){"ok"}else{"missing"},"scope":scope,"source":source,"observed_at":state::now(),"data":data})
        }
        Err(error) => {
            json!({"status":"error","scope":scope,"source":source,"observed_at":state::now(),"data":null,"error":failure(&error)})
        }
    }
}
fn metadata_valid(meta: &fs::Metadata, directory: bool, owner: u32) -> bool {
    !meta.file_type().is_symlink()
        && meta.uid() == owner
        && meta.mode() & 0o077 == 0
        && if directory {
            meta.is_dir()
        } else {
            meta.is_file() && meta.nlink() == 1
        }
}
fn optional_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn private(meta: &fs::Metadata, directory: bool) -> Result<()> {
    if !metadata_valid(meta, directory, unsafe { libc::geteuid() }) {
        return unavailable("private_path_required");
    }
    Ok(())
}
fn checked(home: &Path, relative: &Path, directory: bool) -> Result<Option<fs::Metadata>> {
    checked_with(home, relative, directory, || {})
}
fn checked_with(
    home: &Path,
    relative: &Path,
    directory: bool,
    observed: impl FnOnce(),
) -> Result<Option<fs::Metadata>> {
    let parts = relative.components().collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|p| !matches!(p, Component::Normal(_))) {
        return unavailable("private_path_required");
    }
    let mut path = home.to_path_buf();
    let mut last = None;
    for (position, part) in parts.iter().enumerate() {
        path.push(part.as_os_str());
        let Some(meta) = optional_metadata(&path)? else {
            return Ok(None);
        };
        private(&meta, position + 1 < parts.len() || directory)?;
        last = Some(stamp(&meta));
    }
    observed();
    let after = optional_metadata(&path)?;
    if after.as_ref().map(stamp) != last {
        return unavailable("snapshot_changed");
    }
    Ok(after)
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
fn stamp(meta: &fs::Metadata) -> Stamp {
    Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    }
}
struct Source {
    home: PathBuf,
    relative: PathBuf,
    path: PathBuf,
    file: File,
    stamp: Stamp,
}
impl Source {
    fn open(home: &Path, name: &Path, limit: u64) -> Result<Option<Self>> {
        Self::open_with(home, name, limit, |_| {})
    }
    fn open_with(
        home: &Path,
        name: &Path,
        limit: u64,
        observed: impl FnOnce(&Path),
    ) -> Result<Option<Self>> {
        let Some(meta) = checked(home, name, false)? else {
            return Ok(None);
        };
        if meta.len() > limit {
            return unavailable("limit_exceeded");
        }
        let path = home.join(name);
        observed(&path);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        private(&file.metadata()?, false)?;
        let before = stamp(&meta);
        if stamp(&file.metadata()?) != before {
            return unavailable("snapshot_changed");
        }
        Ok(Some(Self {
            home: home.into(),
            relative: name.into(),
            path,
            file,
            stamp: before,
        }))
    }
    fn unchanged(&self) -> Result<()> {
        if stamp(&self.file.metadata()?) != self.stamp
            || checked(&self.home, &self.relative, false)?
                .as_ref()
                .map(stamp)
                != Some(self.stamp.clone())
        {
            return unavailable("snapshot_changed");
        }
        Ok(())
    }
}
fn read(home: &Path, name: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    read_with(home, name, limit, |_| {})
}
fn read_with(
    home: &Path,
    name: &Path,
    limit: u64,
    opened: impl FnOnce(&Path),
) -> Result<Option<Vec<u8>>> {
    let Some(mut source) = Source::open(home, name, limit)? else {
        return Ok(None);
    };
    opened(&source.path);
    let mut data = Vec::new();
    source
        .file
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return unavailable("limit_exceeded");
    }
    source.unchanged()?;
    Ok(Some(data))
}
fn value(home: &Path, name: &str) -> Result<Option<Value>> {
    read(home, Path::new(name), MAX_JSON)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| Unavailable("invalid_metadata").into())
        })
        .transpose()
}
fn names(home: &Path, relative: &str) -> Result<Vec<String>> {
    names_with(home, relative, || {})
}
fn names_with(home: &Path, relative: &str, observed: impl FnOnce()) -> Result<Vec<String>> {
    if checked(home, Path::new(relative), true)?.is_none() {
        return Ok(Vec::new());
    }
    let before = stamp(&fs::symlink_metadata(home.join(relative))?);
    let mut result = Vec::new();
    for entry in fs::read_dir(home.join(relative))? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| Unavailable("invalid_metadata"))?;
        if result.len() >= MAX_FILES {
            return unavailable("limit_exceeded");
        }
        result.push(name);
    }
    observed();
    if stamp(&fs::symlink_metadata(home.join(relative))?) != before {
        return unavailable("snapshot_changed");
    }
    result.sort();
    Ok(result)
}
struct Temporary(PathBuf);
impl Temporary {
    fn new(home: &Path) -> Result<Self> {
        let root = std::env::temp_dir().canonicalize()?;
        if root.starts_with(home) {
            return unavailable("private_path_required");
        }
        let path = root.join(format!("rdrive-status-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let result = Self(path);
        private(&fs::symlink_metadata(&result.0)?, true)?;
        Ok(result)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn sql<T>(
    home: &Path,
    name: &str,
    query: impl FnOnce(&Connection) -> Result<T>,
) -> Result<Option<T>> {
    sql_with(home, name, query, |source, output, limit| {
        std::io::copy(&mut source.take(limit), output)
    })
}
fn sql_with<T>(
    home: &Path,
    name: &str,
    query: impl FnOnce(&Connection) -> Result<T>,
    copy: impl Fn(&mut File, &mut File, u64) -> std::io::Result<u64>,
) -> Result<Option<T>> {
    let Some(mut main) = Source::open(home, Path::new(name), MAX_SQLITE)? else {
        return Ok(None);
    };
    let mut wal = Source::open(home, Path::new(&format!("{name}-wal")), MAX_SQLITE)?;
    // Validate every live SQLite sidecar before opening even a temporary copy.
    checked(home, Path::new(&format!("{name}-shm")), false)?;
    if checked(home, Path::new(&format!("{name}-journal")), false)?.is_some_and(|m| m.len() > 0) {
        return unavailable("snapshot_changed");
    }
    let temporary = Temporary::new(home)?;
    let target = temporary.0.join("snapshot.sqlite3");
    for (source, target) in std::iter::once((&mut main, target.clone())).chain(
        wal.as_mut()
            .map(|w| (w, temporary.0.join("snapshot.sqlite3-wal"))),
    ) {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(target)?;
        let copied = copy(&mut source.file, &mut output, source.stamp.len + 1)?;
        if copied != source.stamp.len {
            return unavailable("snapshot_changed");
        }
    }
    main.unchanged()?;
    if let Some(wal) = &wal {
        wal.unchanged()?;
    }
    let connection = Connection::open_with_flags(
        &target,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(200))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.pragma_update(None, "query_only", true)?;
    let result = query(&connection)?;
    main.unchanged()?;
    if let Some(wal) = &wal {
        wal.unchanged()?;
    }
    Ok(Some(result))
}
fn checked_sum(mut values: impl Iterator<Item = u64>) -> Result<u64> {
    values.try_fold(0u64, |sum, value| {
        sum.checked_add(value).context("status byte count overflow")
    })
}
fn selected(name: &str, profile: Option<&str>) -> bool {
    profile.is_none_or(|wanted| wanted == name)
}
fn configuration(config: &Config, home: &Path, filter: Option<&str>) -> Value {
    let profiles=config.profiles.iter().filter(|(name,_)|selected(name,filter)).map(|(name,p)|json!({"name":name,"protocol":p.protocol,"endpoint":p.url,"credential_source":if p.password_env.is_some(){"environment"}else if p.keychain_internet{"internet_keychain"}else{"keychain_or_anonymous"},"timeout_seconds":p.timeout_secs,"smb_encryption":p.smb_encryption})).collect::<Vec<_>>();
    json!({"state_directory":home,"configuration_file":home.join("config.json"),"selected_profile":filter,"configured_profiles":config.profiles.len(),"profiles":profiles,"index_policy":config.index,"cache_policy":config.cache,"history_policy":config.history,"checkpoint_policy":config.checkpoints,"filters":config.filters,"sync_policy":config.policies,"credentials_redacted":true})
}
fn cause(error: Option<&str>) -> Value {
    let code = if error.is_some_and(|e| e.contains("synchronization filters exclude")) {
        "excluded_by_filter"
    } else {
        error
            .and_then(|text| text.split_once(':').map(|v| v.0))
            .unwrap_or("operation_failed")
    };
    let (code, reason) = match code {
        "version_conflict" => (
            "version_conflict",
            "The item changed since the saved revision; review the retained edit.",
        ),
        "excluded_by_filter" => (
            "excluded_by_filter",
            "Synchronization rules exclude this operation; its retained edit is preserved.",
        ),
        "insufficient_quota" => (
            "insufficient_quota",
            "The configured capacity cannot admit this operation; retained contents are preserved.",
        ),
        "not_authenticated" => (
            "not_authenticated",
            "Credentials were unavailable or the server refused authentication.",
        ),
        "server_unreachable" => (
            "server_unreachable",
            "The remote server was unavailable during this recorded operation.",
        ),
        "unsupported" => (
            "unsupported",
            "The backend or item does not support the requested operation.",
        ),
        _ => (
            "operation_failed",
            "The operation failed; inspect the corresponding retained task before retrying.",
        ),
    };
    json!({"code":code,"message":reason})
}
fn database(home: &Path) -> Result<Option<state::Database>> {
    read(home, Path::new("state.json"), MAX_JSON)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| Unavailable("invalid_metadata").into())
        })
        .transpose()
}
fn queue(db: &state::Database, profile: Option<&str>, limit: usize) -> Result<Value> {
    let jobs = db
        .jobs
        .iter()
        .filter(|j| selected(&j.profile, profile))
        .collect::<Vec<_>>();
    let mut counts = BTreeMap::<&str, usize>::new();
    for job in &jobs {
        if ![
            "pending",
            "running",
            "interrupted",
            "failed",
            "done",
            "cancelled",
        ]
        .contains(&job.status.as_str())
        {
            return unavailable("invalid_metadata");
        }
        *counts.entry(&job.status).or_default() += 1;
    }
    let examples=jobs.iter().filter(|j|j.error.is_some()||j.status=="failed").take(limit).map(|j|json!({"id":j.id,"profile":j.profile,"state":j.status,"attempts":j.attempts,"error":cause(j.error.as_deref())})).collect::<Vec<_>>();
    Ok(
        json!({"counts":counts,"total":jobs.len(),"pending":jobs.iter().filter(|j|!matches!(j.status.as_str(),"done"|"cancelled")).count(),"retained_bytes":checked_sum(jobs.iter().filter(|j|!matches!(j.status.as_str(),"done"|"cancelled")).map(|j|j.size))?,"examples":examples,"examples_limit":limit}),
    )
}

fn lease_outcome(result: i32, error: std::io::Error) -> Result<bool> {
    if result == 0 {
        Ok(false)
    } else if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(true)
    } else {
        Err(error.into())
    }
}
fn lease_held(file: &File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let error = std::io::Error::last_os_error();
    if result == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
    lease_outcome(result, error)
}
struct CacheInventory {
    sizes: BTreeMap<String, u64>,
    used: u64,
    leased: u64,
    unmanaged: u64,
    pinned: u64,
}
fn cache_inventory(home: &Path, config: &Config, db: &state::Database) -> Result<CacheInventory> {
    cache_inventory_with(home, config, db, || {})
}
fn cache_inventory_with(
    home: &Path,
    config: &Config,
    db: &state::Database,
    observed: impl FnOnce(),
) -> Result<CacheInventory> {
    let mut result = CacheInventory {
        sizes: BTreeMap::new(),
        used: 0,
        leased: 0,
        unmanaged: 0,
        pinned: 0,
    };
    if config.cache.directory.is_none() && checked(home, Path::new("cache"), true)?.is_none() {
        return Ok(result);
    }
    let directory = crate::cache::configured_directory(home, config)?;
    let before = stamp(&fs::symlink_metadata(&directory)?);
    let start = std::time::Instant::now();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Unavailable("invalid_metadata"))?;
        if crate::cache_directory::is_marker(&name) {
            continue;
        }
        if result.sizes.len() >= MAX_FILES || start.elapsed() > Duration::from_secs(2) {
            return unavailable("limit_exceeded");
        }
        let source = Source::open(&directory, Path::new(&name), u64::MAX)?
            .context("cache payload disappeared")?;
        let size = source.stamp.len;
        result.used = checked_sum([result.used, size].into_iter())?;
        if lease_held(&source.file)? {
            result.leased = checked_sum([result.leased, size].into_iter())?;
        }
        let indexed = db.cache.values().find(|item| item.object == name);
        if indexed.is_some_and(|item| item.pinned) {
            result.pinned = checked_sum([result.pinned, size].into_iter())?;
        }
        if indexed.is_none()
            && !db.cache_garbage.contains(&name)
            && !db.cache_reservations.contains_key(&name)
        {
            result.unmanaged = checked_sum([result.unmanaged, size].into_iter())?;
        }
        source.unchanged()?;
        result.sizes.insert(name, size);
    }
    observed();
    if stamp(&fs::symlink_metadata(&directory)?) != before {
        return unavailable("snapshot_changed");
    }
    Ok(result)
}
fn cache(
    home: &Path,
    config: &Config,
    db: &state::Database,
    profile: Option<&str>,
    now: i64,
) -> Result<Value> {
    let inventory = cache_inventory(home, config, db)?;
    let mut bytes = 0u64;
    let mut pinned = 0u64;
    let mut files = 0usize;
    let mut pinned_files = 0usize;
    let mut unbound = 0usize;
    let mut missing = 0usize;
    for item in db
        .cache
        .values()
        .filter(|item| selected(&item.profile, profile))
    {
        let p = config
            .profiles
            .get(&item.profile)
            .context("cached profile no longer exists")?;
        let binding = state::key(&serde_json::to_string(&(&p.protocol, &p.url, &p.username))?);
        if item.scope.as_deref() != Some(binding.as_str()) {
            unbound += 1;
            continue;
        }
        files += 1;
        bytes = bytes
            .checked_add(item.entry.size)
            .context("cache size overflow")?;
        if !inventory.sizes.contains_key(&item.object) {
            missing += 1;
        }
        if item.pinned {
            pinned_files += 1;
            pinned = pinned
                .checked_add(item.entry.size)
                .context("pinned size overflow")?;
        }
    }
    let reservations = checked_sum(db.cache_reservations.iter().map(|(object, v)| {
        v.size
            .saturating_sub(inventory.sizes.get(object).copied().unwrap_or(0))
    }))?;
    let native = value(home, "native-cache-usage.json")?;
    let mut used = 0u64;
    let mut reserved = 0u64;
    let mut native_pinned = 0u64;
    let mut reports = 0usize;
    let mut updated = None;
    if let Some(native) = native {
        if native["version"] != 1 {
            return unavailable("invalid_metadata");
        }
        for report in native["reports"]
            .as_object()
            .context("native cache reports missing")?
            .values()
        {
            let (u, r, p, t) = (
                report["used"]
                    .as_u64()
                    .context("invalid native used bytes")?,
                report["reserved"]
                    .as_u64()
                    .context("invalid native reservation")?,
                report["pinned"]
                    .as_u64()
                    .context("invalid native pin bytes")?,
                report["updated"]
                    .as_i64()
                    .context("invalid native update time")?,
            );
            if p > u || t < 0 {
                return unavailable("invalid_metadata");
            }
            used = checked_sum([used, u].into_iter())?;
            native_pinned = checked_sum([native_pinned, p].into_iter())?;
            if now.saturating_sub(t) <= 120 {
                reserved = checked_sum([reserved, r].into_iter())?;
            }
            reports += 1;
            updated = Some(updated.unwrap_or(0).max(t));
        }
    }
    let combined = checked_sum([inventory.used, used].into_iter())?;
    let total = checked_sum([combined, reservations, reserved].into_iter())?;
    Ok(
        json!({"max_bytes":config.cache.max_bytes,"directory":config.cache.directory.clone().unwrap_or_else(||home.join("cache")),"indexed_files":files,"indexed_bytes":bytes,"pinned_files":pinned_files,"pinned_bytes":pinned,"missing_files":missing,"unbound_entries":unbound,"used_bytes":inventory.used,"leased_bytes":inventory.leased,"unmanaged_bytes":inventory.unmanaged,"global_pinned_bytes":inventory.pinned,"physical_inventory_scope":"global","reserved_bytes":reservations,"rust_reserved_bytes":reservations,"native_used_bytes":used,"native_reserved_bytes":reserved,"native_pinned_bytes":native_pinned,"native":{"scope":"global","used_bytes":used,"reserved_bytes":reserved,"pinned_bytes":native_pinned,"reports":reports,"reported_at":updated},"combined_used_bytes":combined,"accounted_bytes":total,"accounted_upper_bound_bytes":total,"over_budget":config.cache.max_bytes.is_some_and(|limit|total>limit),"accounting":"global payload file sizes, remaining Rust reservations and reported native usage/admissions; no content hashing","reservations_scope":"global","excludes":["dirty edits","upload snapshots","history","checkpoint vault"]}),
    )
}
fn domains(home: &Path, config: &Config) -> Result<Vec<Domain>> {
    let Some(raw) = read(home, Path::new("provider-domains.json"), MAX_JSON)? else {
        return Ok(Vec::new());
    };
    let result: Vec<Domain> =
        serde_json::from_slice(&raw).map_err(|_| Unavailable("invalid_metadata"))?;
    if result.len() > MAX_FILES {
        return unavailable("limit_exceeded");
    }
    for d in &result {
        crate::provider::journal_binding(config, d)?;
        if d.id.is_empty()
            || d.id.len() > 120
            || d.id.chars().any(char::is_control)
            || !crate::backend::normalize(&d.remote).is_ok_and(|v| v == d.remote)
        {
            return unavailable("invalid_metadata");
        }
    }
    Ok(result)
}
fn index_header(connection: &Connection, domain: &Domain, binding: &str) -> Result<i64> {
    let schema: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let app: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if schema != 2 || app != 0x52444958 {
        return unavailable("schema_unsupported");
    }
    let (id, stored, anchor): (String, String, i64) = connection.query_row(
        "SELECT domain_id,binding,anchor FROM meta WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if id != domain.id || stored != binding {
        return unavailable("scope_changed");
    }
    Ok(anchor)
}
fn index_result(domain: &Domain, result: Result<Value>) -> Value {
    let mut value = match result {
        Ok(mut v) => {
            v["status"] = json!("ok");
            v
        }
        Err(error) => {
            json!({"domain":domain.id,"profile":domain.profile,"status":"error","error":failure(&error)})
        }
    };
    value["observed_at"] = json!(state::now());
    value
}
fn index(home: &Path, config: &Config, domain: &Domain) -> Result<Value> {
    let binding = crate::provider::journal_binding(config, domain)?;
    let path = format!("provider/{}.sqlite3", state::key(&domain.id));
    let result = sql(home, &path, |connection| {
        let anchor = index_header(connection, domain, &binding)?;
        let (items,dirs):(i64,i64)=connection.query_row("SELECT count(*),coalesce(sum(json_extract(item_json,'$.kind')='directory'),0) FROM items",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let (scanned,last):(i64,Option<i64>)=connection.query_row("SELECT count(*),max(scanned_at) FROM directory_scans WHERE id='root' OR id IN (SELECT id FROM items WHERE json_extract(item_json,'$.kind')='directory')",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok(
            json!({"domain":domain.id,"profile":domain.profile,"initialized":true,"items":items,"files":items-dirs,"directories":dirs,"scanned":scanned,"pending":(dirs+1-scanned).max(0),"last_success_at_ms":last,"anchor":anchor.to_string(),"database":home.join(&path),"schema_version":2}),
        )
    })?;
    Ok(result.unwrap_or_else(||json!({"domain":domain.id,"profile":domain.profile,"initialized":false,"items":0,"last_success_at_ms":null})))
}
fn item_paths(
    home: &Path,
    config: &Config,
    domain: &Domain,
    ids: &[String],
) -> Result<BTreeMap<String, String>> {
    let binding = crate::provider::journal_binding(config, domain)?;
    sql(
        home,
        &format!("provider/{}.sqlite3", state::key(&domain.id)),
        |connection| {
            index_header(connection, domain, &binding)?;
            let mut paths = BTreeMap::new();
            for id in ids {
                use rusqlite::OptionalExtension;
                let path: Option<String> = connection
                    .query_row("SELECT path FROM items WHERE id=?1", [id], |r| r.get(0))
                    .optional()?;
                if let Some(path) = path.filter(|p| {
                    p.len() <= 4096 && crate::backend::normalize(p).is_ok_and(|v| v == *p)
                }) {
                    paths.insert(id.clone(), path);
                }
            }
            Ok(paths)
        },
    )
    .map(|value| value.unwrap_or_default())
}

fn history(home: &Path, config: &Config, filter: Option<&str>) -> Result<Value> {
    let profiles=config.profiles.keys().filter(|p|selected(p,filter)).map(|name| {
        let result=(||->Result<Value>{let binding=crate::history::status_binding(config,name)?;
            let stats=sql(home,"history/history.sqlite3",|connection| {
                connection.query_row("SELECT count(*),coalesce(sum(bytes),0),coalesce(sum(state='prepared'),0),max(created) FROM entries WHERE binding=?1",[&binding],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?))).map_err(Into::into)
            })?.unwrap_or((0,0,0,None));
            if stats.0<0||stats.1<0||stats.2<0{return unavailable("invalid_metadata");}
            Ok(json!({"profile":name,"status":"ok","policy":config.history.effective(name),"entries":stats.0,"bytes":stats.1,"prepared":stats.2,"latest_created_at":stats.3}))})();
        match result {Ok(value)=>value,Err(error)=>json!({"profile":name,"status":"error","policy":config.history.effective(name),"error":failure(&error)})}
    }).collect::<Vec<_>>();
    let errors = profiles.iter().filter(|v| v["status"] == "error").count();
    Ok(
        json!({"directory":home.join("history"),"profiles":profiles,"error_components":errors,"separate_from_cache":true,"budget_is_logical":true}),
    )
}
fn smart_snapshot(home: &Path, domain: &str, binding: &str) -> Result<Vec<crate::smart::Mutation>> {
    smart_snapshot_with(home, domain, binding, || {})
}
fn smart_snapshot_with(
    home: &Path,
    domain: &str,
    binding: &str,
    observed: impl FnOnce(),
) -> Result<Vec<crate::smart::Mutation>> {
    let path = format!("smart/{}/queue.json", state::key(domain));
    let before = read(home, Path::new(&path), MAX_JSON)?;
    let result = crate::smart::read_only(home, domain, binding)?;
    observed();
    if read(home, Path::new(&path), MAX_JSON)? != before {
        return unavailable("snapshot_changed");
    }
    Ok(result)
}
fn conflicts(
    home: &Path,
    config: &Config,
    domains: &[Domain],
    filter: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let mut entries = Vec::new();
    let mut summaries = Vec::new();
    let mut total = 0usize;
    let mut conflicts = 0usize;
    let mut errors = 0usize;
    for domain in domains
        .iter()
        .filter(|d| d.smart && selected(&d.profile, filter))
    {
        let binding = crate::provider::journal_binding(config, domain)?;
        let mutations = match smart_snapshot(home, &domain.id, &binding) {
            Ok(v) => v,
            Err(error) => {
                summaries.push(json!({"domain":domain.id,"profile":domain.profile,"status":"error","error":failure(&error)}));
                errors += 1;
                continue;
            }
        };
        let mut counts = BTreeMap::<String, usize>::new();
        let mut dirty = 0u64;
        let mut retained = 0u64;
        let mut missing = 0usize;
        let attention = mutations
            .iter()
            .filter(|m| {
                matches!(
                    m.state,
                    crate::smart::MutationState::Conflict
                        | crate::smart::MutationState::Failed
                        | crate::smart::MutationState::Interrupted
                )
            })
            .collect::<Vec<_>>();
        let ids = attention
            .iter()
            .take(limit.saturating_sub(entries.len()))
            .filter_map(|m| {
                m.request["id"]
                    .as_str()
                    .or_else(|| m.request["client_item_id"].as_str())
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let paths = if ids.is_empty() {
            BTreeMap::new()
        } else {
            item_paths(home, config, domain, &ids).unwrap_or_default()
        };
        for m in &mutations {
            *counts.entry(m.state.as_str().into()).or_default() += 1;
            if let Some(snapshot) = m.snapshot.as_ref().filter(|_| !m.payload_discarded) {
                retained = retained
                    .checked_add(snapshot.size)
                    .context("retained content byte overflow")?;
                if m.state.active() {
                    dirty = dirty
                        .checked_add(snapshot.size)
                        .context("dirty content byte overflow")?;
                }
            }
            if m.state == crate::smart::MutationState::Conflict {
                conflicts += 1;
            }
        }
        total += attention.len();
        for m in attention
            .into_iter()
            .take(limit.saturating_sub(entries.len()))
        {
            let item = m.request["id"]
                .as_str()
                .or_else(|| m.request["client_item_id"].as_str());
            let retained_content =
                if let Some(snapshot) = m.snapshot.as_ref().filter(|_| !m.payload_discarded) {
                    let present = checked(
                        home,
                        Path::new(&format!(
                            "smart/{}/dirty/{}",
                            state::key(&domain.id),
                            snapshot.file
                        )),
                        false,
                    )?
                    .is_some_and(|meta| meta.len() == snapshot.size);
                    if !present {
                        missing += 1;
                    }
                    Some(present)
                } else {
                    None
                };
            entries.push(json!({"domain":domain.id,"profile":domain.profile,"mutation":m.id,"state":m.state,"attempts":m.attempts,"updated":m.updated,"name":m.request["name"].as_str().map(|v|v.chars().take(1024).collect::<String>()),"item_id":item.map(|v|v.chars().take(512).collect::<String>()),"path":item.and_then(|id|paths.get(id)),"source_path_unknown":item.is_none_or(|id|!paths.contains_key(id)),"retained_content":retained_content,"error":cause(m.error.as_deref())}));
        }
        summaries.push(json!({"domain":domain.id,"profile":domain.profile,"status":"ok","counts":counts,"dirty_bytes":dirty,"retained_snapshot_bytes_recorded":retained,"missing_example_payloads":missing}));
    }
    let known = domains
        .iter()
        .map(|d| state::key(&d.id))
        .collect::<std::collections::BTreeSet<_>>();
    let unbound = names(home, "smart")?
        .into_iter()
        .filter(|name| {
            name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) && !known.contains(name)
        })
        .count();
    if unbound > 0 {
        errors += 1;
    }
    let control=value(home,"smart/control.json")?.map(|v|json!({"scope":"global","version":v["version"].as_u64(),"paused":v["paused"].as_bool(),"updated":v["updated"].as_str()}));
    Ok(
        json!({"control":control,"domains":summaries,"error_components":errors,"needs_attention":total,"conflicts":conflicts,"examples":entries,"examples_limit":limit,"ordinary_sync":ordinary_conflicts(home,config,filter,limit)?,"unbound_journal_directories":unbound,"unbound_journals_scope":"global; account binding unknown","unbound_journal_state":if unbound>0{"unknown; not read without a configured account binding"}else{"none"}}),
    )
}
fn ordinary_conflicts(
    home: &Path,
    config: &Config,
    filter: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let Some(db) = database(home)? else {
        return Ok(json!({"status":"not_recorded","plans":[],"current_conflicts":"unknown"}));
    };
    let mut latest = BTreeMap::new();
    let mut ignored = 0usize;
    for (order, event) in db
        .history
        .iter()
        .enumerate()
        .filter(|(_, e)| e.operation == "sync-status" && selected(&e.profile, filter))
    {
        let Some(profile) = config.profiles.get(&event.profile) else {
            ignored += 1;
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&event.detail) else {
            ignored += 1;
            continue;
        };
        if v["version"] != 1
            || v["binding"].as_str() != Some(crate::connection_status::binding(profile)?.as_str())
        {
            ignored += 1;
            continue;
        }
        let (Some(local), Some(remote), Some(phase)) = (
            v["local_root"].as_str(),
            v["remote_root"].as_str(),
            v["phase"].as_str(),
        ) else {
            ignored += 1;
            continue;
        };
        if !Path::new(local).is_absolute()
            || !crate::backend::normalize(remote).is_ok_and(|p| p == remote)
            || !["content_plan", "metadata_plan", "repair"].contains(&phase)
        {
            ignored += 1;
            continue;
        }
        let conflicts=v["conflicts"].as_array().map(|items|items.iter().take(limit).map(|item|json!({"path":item["path"].as_str(),"reason":"Both sides changed; review the sync plan."})).collect::<Vec<_>>()).unwrap_or_default();
        latest.insert((event.profile.clone(),local.to_owned(),remote.to_owned()),(order,json!({"profile":event.profile,"local_root":local,"remote_root":remote,"phase":phase,"observed_at":event.time,"applied":v["applied"].as_bool(),"action_count":v["action_count"].as_u64(),"conflict_count":v["conflict_count"].as_u64(),"metadata_conflict_count":v["metadata_conflict_count"].as_u64(),"replan_required":phase=="repair","conflicts":conflicts,"freshness":"last_observed_plan; current remote/local state was not checked"})));
    }
    let total = latest.len();
    let mut plans = latest.into_values().collect::<Vec<_>>();
    plans.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(
        json!({"status":if total==0{"not_recorded"}else{"last_observed"},"plans":plans.into_iter().take(limit).map(|(_,plan)|plan).collect::<Vec<_>>(),"total_plans":total,"examples_limit":limit,"ignored_stale_or_invalid_reports":ignored,"current_conflicts":"unknown"}),
    )
}

fn transfers(home: &Path, filter: Option<&str>, limit: usize) -> Result<Value> {
    transfers_with(home, filter, limit, || {})
}
fn transfers_with(
    home: &Path,
    filter: Option<&str>,
    limit: usize,
    observed: impl FnOnce(),
) -> Result<Value> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut total = 0usize;
    let mut examples = Vec::new();
    let files = names(home, "transfers")?;
    observed();
    for name in files.into_iter().filter(|n| n.ends_with(".json")) {
        let Some(v) = value(home, &format!("transfers/{name}"))? else {
            return unavailable("snapshot_changed");
        };
        let profile = v["profile"].as_str().context("transfer profile missing")?;
        if !selected(profile, filter) {
            continue;
        }
        let phase = v["phase"].as_str().context("transfer phase missing")?;
        if !["downloading", "publishing", "complete"].contains(&phase) {
            return unavailable("invalid_metadata");
        }
        *counts.entry(phase.into()).or_default() += 1;
        total += 1;
        if examples.len() < limit {
            examples.push(json!({"id":v["id"].as_str(),"profile":profile,"phase":phase,"verified_bytes":v["bytes"].as_u64(),"total_bytes":v["expected"]["size"].as_u64()}));
        }
    }
    Ok(
        json!({"counts":counts,"total":total,"examples":examples,"examples_limit":limit,"source":"durable download checkpoints; current network activity requires a live broker observation"}),
    )
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    uid: u32,
    parent: u32,
    executable: PathBuf,
}
#[cfg(target_os = "macos")]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    if unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    } != size
    {
        return None;
    }
    let mut bytes = [0u8; 4096];
    let length =
        unsafe { libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    Some(ProcessInfo {
        uid: info.pbi_uid,
        parent: info.pbi_ppid,
        executable: process_path_result(&bytes, length)?,
    })
}
#[cfg(target_os = "macos")]
fn process_path_result(bytes: &[u8], length: i32) -> Option<PathBuf> {
    if length <= 0 || length as usize > bytes.len() {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    let end = bytes
        .iter()
        .take(length as usize + 1)
        .position(|b| *b == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}
#[cfg(not(target_os = "macos"))]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    let mut source = File::open(root.join("status")).ok()?;
    let mut data = String::new();
    source
        .by_ref()
        .take(16 * 1024)
        .read_to_string(&mut data)
        .ok()?;
    let uid = data
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().nth(1))?
        .parse()
        .ok()?;
    let parent = data
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()?;
    Some(ProcessInfo {
        uid,
        parent,
        executable: fs::read_link(root.join("exe")).ok()?,
    })
}
fn process(
    pid: &Value,
    executable: Option<&Path>,
    parent: Option<u32>,
    probe: fn(u32) -> Option<ProcessInfo>,
) -> Value {
    let id = pid
        .as_u64()
        .filter(|id| *id > 0 && *id <= i32::MAX as u64)
        .map(|id| id as u32);
    let identity = id.and_then(probe);
    let matches = identity
        .as_ref()
        .zip(executable)
        .is_some_and(|(actual, wanted)| {
            actual.uid == unsafe { libc::geteuid() }
                && actual.executable == wanted
                && parent.is_none_or(|pid| pid == actual.parent)
        });
    let present=id.map(|pid|unsafe{libc::kill(pid as i32,0)}==0||std::io::Error::last_os_error().raw_os_error()==Some(libc::EPERM));
    json!({"pid":id,"process_alive":present,"identity_verified":matches,"state":if matches{"running"}else if present==Some(false){"stopped"}else{"unknown"}})
}

fn service(home: &Path, now: i64) -> Result<Option<Value>> {
    service_with(home, now, process_info)
}
fn service_with(
    home: &Path,
    now: i64,
    probe: fn(u32) -> Option<ProcessInfo>,
) -> Result<Option<Value>> {
    let Some(installation) = value(home, "service/installation.json")? else {
        return Ok(None);
    };
    let runtime = value(home, "service/status.json")?;
    let lease = Source::open(home, Path::new("service/supervisor.lock"), 4096)?;
    let held = lease
        .as_ref()
        .map(|source| lease_held(&source.file))
        .transpose()?;
    let label = format!(
        "io.rdrive.service.{}.{}",
        unsafe { libc::geteuid() },
        &state::key(&home.to_string_lossy())[..20]
    );
    if installation["format"] != 1
        || installation["options"]["home"].as_str() != home.to_str()
        || installation["label"] != label
    {
        return unavailable("scope_changed");
    }
    let data = runtime.as_ref();
    if data.is_some_and(|r| r["format"] != 1 || r["label"] != installation["label"]) {
        return unavailable("scope_changed");
    }
    let reported_state = data
        .and_then(|r| r["state"].as_str())
        .filter(|state| matches!(*state, "running" | "stopped"));
    let reported_at = data
        .and_then(|r| r["updated"].as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
    let age = reported_at
        .as_ref()
        .map(|t| now.saturating_sub(t.timestamp()));
    let parent = data
        .and_then(|r| r["supervisor_pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok());
    let mut roles = serde_json::Map::new();
    for role in ["broker", "native_watcher"] {
        let r = data.map(|r| &r["children"][role]).unwrap_or(&Value::Null);
        let executable = if role == "broker" {
            installation["options"]["executable"]
                .as_str()
                .map(PathBuf::from)
        } else {
            installation["options"]["app"]
                .as_str()
                .map(|p| Path::new(p).join("Contents/MacOS/RDriveCompanion"))
        };
        let mut current = process(&r["pid"], executable.as_deref(), parent, probe);
        current["last_exit"] = json!(r["last_exit"].as_i64());
        current["starts"] = json!(r["starts"].as_u64());
        roles.insert(role.into(), current);
    }
    let supervisor = process(
        data.map(|r| &r["supervisor_pid"]).unwrap_or(&Value::Null),
        installation["options"]["executable"]
            .as_str()
            .map(Path::new),
        None,
        probe,
    );
    Ok(Some(
        json!({"installed":true,"label":installation["label"].as_str(),"ipc_root":installation["options"]["ipc_root"].as_str(),"reported_state":reported_state,"reported_at":reported_at.map(|t|t.to_rfc3339()),"heartbeat_age_seconds":age,"heartbeat_stale":age.is_none_or(|age|!(0..=15).contains(&age)),"supervisor_lease_held":held,"supervisor":supervisor,"supervisor_running_observed":held==Some(true)&&supervisor["identity_verified"]==true&&age.is_some_and(|age|(0..=15).contains(&age)),"children":roles,"launchd_state":"not_queried","evidence":"owned supervisor lease, recorded runtime identity, heartbeat and UID/parent/executable verification; remote connectivity is separate"}),
    ))
}
fn checkpoints(home: &Path, config: Option<&Config>, now: i64) -> Result<Value> {
    checkpoints_with(home, config, now, |_| {})
}
fn checkpoints_with(
    home: &Path,
    config: Option<&Config>,
    now: i64,
    observed: impl Fn(&str),
) -> Result<Value> {
    let descriptor = value(home, "checkpoints/vault/descriptor.json")?;
    let scheduler = value(home, "checkpoint-scheduler/state.json")?;
    let image = checked(home, Path::new("checkpoints/vault/vault.dmg"), false)?;
    if let Some(d) = &descriptor {
        let expected = state::key(&home.canonicalize()?.to_string_lossy());
        if d["version"] != 1 || d["home_binding"] != expected {
            return unavailable("scope_changed");
        }
        if d["volume_uuid"]
            .as_str()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
            || d["ready"].as_bool().is_none()
        {
            return unavailable("invalid_metadata");
        }
        if let Some(image) = &image {
            if d["capacity_bytes"].as_u64() != Some(image.len())
                || d["image"]["device"].as_u64() != Some(image.dev())
                || d["image"]["inode"].as_u64() != Some(image.ino())
            {
                return unavailable("scope_changed");
            }
        } else if d["ready"] == true || !d["image"].is_null() {
            return unavailable("invalid_metadata");
        }
    }
    if scheduler.as_ref().is_some_and(|s| s["version"] != 1) {
        return unavailable("schema_unsupported");
    }
    let mut legacy = 0usize;
    let files = names(home, "checkpoints")?;
    observed("records_listed");
    for name in files.into_iter().filter(|n| n != "mounts" && n != "vault") {
        if uuid::Uuid::parse_str(&name).is_err() {
            return unavailable("invalid_metadata");
        }
        let Some(record) = value(home, &format!("checkpoints/{name}"))? else {
            return unavailable("snapshot_changed");
        };
        if record["manifest"]["home"].as_str() != home.to_str() {
            return unavailable("scope_changed");
        }
        legacy += 1;
    }
    let schedule = if let Some(config) = config.filter(|_| home.exists()) {
        let managed = config.checkpoints.storage == crate::checkpoints::Storage::ApfsVault;
        let options = crate::checkpoint_schedule::Options {
            enabled: config.checkpoints.enabled && managed,
            interval_secs: config.checkpoints.interval_secs.filter(|_| managed),
            backend_key: if managed {
                format!(
                    "apfs_vault:{}",
                    config
                        .checkpoints
                        .max_bytes
                        .context("vault capacity missing")?
                )
            } else {
                "system_snapshot".into()
            },
        };
        let state = crate::checkpoint_schedule::status(home, &options, now)?;
        let attempt = |a: &Value| -> Value {
            if a.is_null() {
                Value::Null
            } else {
                json!({"id":a["id"].as_str(),"checkpoint_id":a["checkpoint_id"].as_str(),"started_at":a["started_at"].as_i64(),"completed_at":a["completed_at"].as_i64()})
            }
        };
        json!({"operation":state["operation"].as_str(),"enabled":state["enabled"].as_bool(),"interval_secs":state["interval_secs"].as_u64(),"next_due_at":state["next_due_at"].as_i64(),"retry_at":state["retry_at"].as_i64(),"failure_count":state["failure_count"].as_u64(),"inflight":attempt(&state["inflight"]),"last_success":attempt(&state["last_success"]),"last_error":state["last_error"].as_str().map(|_|cause(state["last_error"].as_str())),"last_deferred":state["last_deferred"].as_str().map(|_|json!({"code":"busy","message":"The checkpoint is waiting for an active operation to finish."}))})
    } else {
        Value::Null
    };
    observed("schedule_observed");
    if value(home, "checkpoint-scheduler/state.json")? != scheduler {
        return unavailable("snapshot_changed");
    }
    Ok(
        json!({"policy":config.map(|c|&c.checkpoints),"image_path":home.join("checkpoints/vault/vault.dmg"),"image_present":image.is_some(),"fixed_image_bytes":image.as_ref().map(|m|m.len()),"host_allocated_bytes":image.as_ref().map(|m|m.blocks().saturating_mul(512)),"identity_ready":descriptor.as_ref().and_then(|d|d["ready"].as_bool()),"volume_uuid":descriptor.as_ref().and_then(|d|d["volume_uuid"].as_str()),"last_confirmed_checkpoint":schedule["last_success"],"next_due_at":schedule["next_due_at"],"schedule":schedule,"legacy_records":legacy,"vault_records":null,"internal_used_bytes":null,"internal_available_bytes":null,"payloads_verified":false,"inspection":"metadata only; internal capacity usage and record availability require explicit read-only mounting","scope":"global fixed image; history, cache and independent system backups use additional space"}),
    )
}

/// Read a per-component snapshot while preserving active writers and retained
/// contents. Only the optional profile selection scopes account-bound sections;
/// service, global policies and checkpoint-image limits remain global.
pub fn collect(home: &Path, profile: Option<&str>) -> Result<Value> {
    collect_with_limit(home, profile, DEFAULT_ITEMS)
}
pub fn collect_with_limit(home: &Path, profile: Option<&str>, max_items: usize) -> Result<Value> {
    collect_at(home, profile, chrono::Utc::now().timestamp(), max_items)
}
fn collect_at(home: &Path, profile: Option<&str>, now: i64, limit: usize) -> Result<Value> {
    collect_observed(home, profile, now, limit, || {})
}
fn collect_observed(
    home: &Path,
    profile: Option<&str>,
    now: i64,
    limit: usize,
    observed: impl FnOnce(),
) -> Result<Value> {
    if !(1..=100).contains(&limit) {
        bail!("status max-items must be between 1 and 100");
    }
    if let Some(meta) = optional_metadata(home)? {
        private(&meta, true)?;
    }
    let home = if home.exists() {
        home.canonicalize()?
    } else {
        std::path::absolute(home)?
    };
    let config_bytes = read(&home, Path::new("config.json"), MAX_JSON)?;
    let config_result = (|| -> Result<Config> {
        let config: Config = match &config_bytes {
            Some(raw) => {
                serde_json::from_slice(raw).map_err(|_| Unavailable("invalid_configuration"))?
            }
            None => Config::default(),
        };
        if config.profiles.len() > MAX_FILES {
            return unavailable("limit_exceeded");
        }
        config.index.validate()?;
        config.filters.validate()?;
        config.policies.validate()?;
        config.history.validate()?;
        config.checkpoints.validate()?;
        for p in config.profiles.values() {
            crate::config::validate_profile(p)?;
        }
        Ok(config)
    })();
    let mut report = json!({"schema_version":1,"observed_at":state::now(),"state_directory":home,"selected_profile":profile,"read_only":true,"snapshot":"independent bounded component observations, not a global transaction"});
    report["service"] = section(
        "service/installation.json, service/status.json and existing supervisor lease",
        "global",
        service(&home, now),
    );
    report["checkpoints"] = section(
        "checkpoint image descriptor, scheduler record and image stat; no mounting",
        "global",
        checkpoints(&home, config_result.as_ref().ok(), now).map(Some),
    );
    if let Ok(config) = &config_result {
        if profile.is_some_and(|p| !config.profiles.contains_key(p)) {
            bail!("unknown status profile; use profile list");
        }
        report["profiles"] = json!(config
            .profiles
            .keys()
            .filter(|name| selected(name, profile))
            .count());
        report["configuration"] = section(
            "config.json",
            "global",
            Ok(config_bytes
                .as_ref()
                .map(|_| configuration(config, &home, profile))),
        );
        let domain_result = domains(&home, config);
        report["index"] = section(
            "private read-only SQLite copies of provider indexes",
            "selected_profiles",
            domain_result
                .as_ref()
                .map_err(|_| Unavailable("invalid_metadata").into())
                .map(|all| {
                    let domains = all
                        .iter()
                        .filter(|d| d.smart && selected(&d.profile, profile))
                        .map(|d| index_result(d, index(&home, config, d)))
                        .collect::<Vec<_>>();
                    let errors = domains.iter().filter(|d| d["status"] == "error").count();
                    Some(json!({"policy":config.index,"domains":domains,"error_components":errors}))
                }),
        );
        report["conflicts"] = section(
            "account-bound retained Smart mutation snapshots",
            "selected_profiles",
            domain_result
                .as_ref()
                .map_err(|_| Unavailable("invalid_metadata").into())
                .and_then(|all| conflicts(&home, config, all, profile, limit))
                .map(Some),
        );
        match database(&home) {
            Ok(db) => {
                report["queue"] = section(
                    "state.json",
                    "selected_profiles",
                    db.as_ref().map(|db| queue(db, profile, limit)).transpose(),
                );
                report["cache"] = section(
                    "state.json and native-cache-usage.json",
                    "selected_profiles_with_global_admissions",
                    db.as_ref()
                        .map(|db| cache(&home, config, db, profile, now))
                        .transpose(),
                );
                if let Some(db) = db {
                    let entries = db
                        .cache
                        .values()
                        .filter(|c| selected(&c.profile, profile))
                        .collect::<Vec<_>>();
                    report["queued"] = report["queue"]["data"]["pending"].clone();
                    report["cache_files"] = json!(entries.len());
                    report["cache_bytes"] = json!(entries
                        .iter()
                        .try_fold(0u64, |sum, c| sum.checked_add(c.entry.size)));
                    report["pinned"] = json!(entries.iter().filter(|c| c.pinned).count());
                }
            }
            Err(error) => {
                report["queue"] = section(
                    "state.json",
                    "selected_profiles",
                    Err(anyhow::anyhow!(error.to_string())),
                );
                report["cache"] = section(
                    "state.json and native-cache-usage.json",
                    "selected_profiles_with_global_admissions",
                    Err(error),
                );
            }
        }
        report["history"] = section(
            "private read-only SQLite copy of history/history.sqlite3",
            "selected_profiles",
            history(&home, config, profile).map(Some),
        );
        report["transfers"] = section(
            "transfers/*.json",
            "selected_profiles",
            transfers(&home, profile, limit).map(Some),
        );
    } else {
        for name in [
            "configuration",
            "index",
            "cache",
            "history",
            "conflicts",
            "queue",
            "transfers",
        ] {
            report[name] = section(
                "local metadata",
                "unknown",
                unavailable("invalid_configuration"),
            );
        }
    }
    observed();
    if read(&home, Path::new("config.json"), MAX_JSON)? != config_bytes {
        for name in [
            "configuration",
            "index",
            "cache",
            "history",
            "conflicts",
            "queue",
            "transfers",
            "checkpoints",
        ] {
            report[name] = section("local metadata", "unknown", unavailable("snapshot_changed"));
        }
        for name in ["profiles", "queued", "cache_files", "cache_bytes", "pinned"] {
            report.as_object_mut().unwrap().remove(name);
        }
    }
    let errors = [
        "configuration",
        "service",
        "index",
        "cache",
        "history",
        "checkpoints",
        "conflicts",
        "queue",
        "transfers",
    ]
    .into_iter()
    .filter(|name| report[name]["status"] == "error" || report[name]["status"] == "partial")
    .count();
    report["status"] = json!(if errors == 0 { "ok" } else { "partial" });
    report["unavailable_components"] = json!(errors);
    Ok(report)
}

#[cfg(test)]
#[path = "status/tests.rs"]
mod tests;
