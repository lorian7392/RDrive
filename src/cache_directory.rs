//! A cache location is an owned private directory, never a redirected state or
//! File Provider directory. Migration copies first and changes only config.json;
//! state.json object names remain unchanged. A durable intent rolls incomplete
//! copies back or finishes old-copy cleanup after the configuration commit.
use crate::{
    cache,
    config::Config,
    state::{self, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Component, Path, PathBuf},
};

const MARKER: &str = ".rdrive-cache.json";
const INTENT: &str = "cache-directory-migration.json";
#[derive(Serialize, Deserialize)]
struct Marker {
    version: u32,
    home: String,
    device: u64,
    inode: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Object {
    name: String,
    size: u64,
    sha256: String,
    source_identity: (u64, u64),
    #[serde(default)]
    copy_identity: Option<(u64, u64)>,
}
#[derive(Serialize, Deserialize)]
struct Intent {
    version: u32,
    home: String,
    id: String,
    previous: Option<PathBuf>,
    next: Option<PathBuf>,
    source: PathBuf,
    destination: PathBuf,
    objects: Vec<Object>,
}
struct Plan {
    source: PathBuf,
    destination: PathBuf,
    next: Option<PathBuf>,
    objects: Vec<Object>,
}

fn binding(home: &Path) -> String {
    state::key(&home.to_string_lossy())
}
fn owned_directory(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        bail!("cache directory must be an owned private real directory (mode 0700)");
    }
    Ok(meta)
}
fn private_file(path: &Path) -> Result<File> {
    owned_file(path, true)
}
fn owned_file(path: &Path, require_private: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || require_private && meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        bail!("cache migration requires private owned regular files without hard links");
    }
    Ok(file)
}
fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = private_file(path)?;
    if file.metadata()?.len() > 32 * 1024 * 1024 {
        bail!("cache metadata is too large");
    }
    Ok(serde_json::from_reader(file)?)
}
fn marker(home: &Path, directory: &Path, required: bool) -> Result<()> {
    let meta = owned_directory(directory)?;
    let path = directory.join(MARKER);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(()),
        Err(e) => return Err(e).context("cache directory ownership marker is missing"),
        Ok(_) => {}
    }
    let m: Marker = read_private(&path)?;
    if m.version != 1 || m.home != binding(home) || m.device != meta.dev() || m.inode != meta.ino()
    {
        bail!("cache directory belongs to another state home or was replaced");
    }
    Ok(())
}
fn write_marker(home: &Path, directory: &Path) -> Result<()> {
    let meta = owned_directory(directory)?;
    state::atomic_json(
        &directory.join(MARKER),
        &Marker {
            version: 1,
            home: binding(home),
            device: meta.dev(),
            inode: meta.ino(),
        },
    )
}
fn lexical(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|c| {
            matches!(
                c,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        bail!("cache directory requires an absolute path without '.' or '..'");
    }
    // Check every supplied component before canonicalizing; canonicalize alone
    // would silently turn a symlink into permission to manage another tree.
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                bail!("cache directory components cannot be symlinks or files")
            }
            Ok(meta) => {
                let uid = meta.uid();
                let sticky_root = uid == 0 && meta.mode() & libc::S_ISVTX as u32 != 0;
                if uid != 0 && uid != unsafe { libc::geteuid() }
                    || meta.mode() & 0o022 != 0 && !sticky_root
                {
                    bail!(
                        "cache directory ancestors must be trusted and not writable by other users"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && current == path => {}
            Err(e) => return Err(e).context("cache directory parent must already exist"),
        }
    }
    Ok(())
}
/// Filesystem identity also catches case aliases on case-insensitive APFS.
/// Comparing the root to each candidate ancestor avoids treating unrelated
/// siblings as overlapping merely because they share an ancestor.
pub fn path_is_within(candidate: &Path, root: &Path) -> Result<bool> {
    if !candidate.is_absolute() || !root.is_absolute() {
        bail!("cache path comparisons require absolute paths");
    }
    if candidate.starts_with(root) {
        return Ok(true);
    }
    let root_meta = match fs::metadata(root) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for ancestor in candidate.ancestors() {
        match fs::metadata(ancestor) {
            Ok(meta) if (meta.dev(), meta.ino()) == (root_meta.dev(), root_meta.ino()) => {
                return Ok(true)
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}
pub fn paths_overlap(left: &Path, right: &Path) -> Result<bool> {
    Ok(path_is_within(left, right)? || path_is_within(right, left)?)
}
fn validate_path(home: &Path, directory: &Path) -> Result<()> {
    lexical(directory)?;
    let default = home.join("cache");
    if directory != default && paths_overlap(directory, home)? {
        bail!("custom cache must be outside the state home, spool and pending-edit directories");
    }
    if directory.components().any(|c| {
        c.as_os_str().to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case("CloudStorage") || name.eq_ignore_ascii_case("FileProvider")
        })
    }) {
        bail!("cache cannot use an operating-system-managed File Provider directory");
    }
    Ok(())
}
fn validate_location(home: &Path, directory: &Path) -> Result<()> {
    validate_path(home, directory)?;
    for entry in fs::read_dir(home.join("baselines"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let value: Value = state::read_json(&entry.path())?;
            if let Some(root) = value.get("local_root").and_then(Value::as_str) {
                let root = fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
                if paths_overlap(directory, &root)? {
                    bail!("cache directory cannot overlap a known synchronization tree");
                }
            }
        }
    }
    Ok(())
}

pub fn configured_directory(home: &Path, config: &Config) -> Result<PathBuf> {
    // Native quick requests may receive macOS's /var alias for a Store home
    // that was already canonicalized when created. User-supplied custom cache
    // paths still go through the strict component checks below.
    let canonical_home = fs::canonicalize(home)?;
    let home = canonical_home.as_path();
    let directory = config
        .cache
        .directory
        .clone()
        .unwrap_or_else(|| home.join("cache"));
    validate_path(home, &directory)?;
    marker(home, &directory, config.cache.directory.is_some())?;
    Ok(directory)
}
pub fn cache_directory(store: &Store) -> Result<PathBuf> {
    configured_directory(&store.home, &store.config)
}
/// Both sides of an interrupted migration remain internal storage, including
/// while the selected configuration still points at only one of them.
pub fn protected_directories(home: &Path, config: &Config) -> Result<Vec<PathBuf>> {
    let canonical_home = fs::canonicalize(home)?;
    let home = canonical_home.as_path();
    let mut paths = vec![home.join("cache")];
    if let Some(directory) = &config.cache.directory {
        paths.push(directory.clone());
    }
    let path = home.join(INTENT);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            let intent: Intent = read_private(&path)?;
            if intent.version != 1 || intent.home != binding(home) {
                bail!("invalid cache migration intent; local file access is refused");
            }
            validate_location(home, &intent.source)?;
            validate_location(home, &intent.destination)?;
            paths.push(intent.source);
            paths.push(intent.destination);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
pub fn object_path(store: &Store, object: &str) -> Result<PathBuf> {
    if object.is_empty()
        || object.len() > 128
        || !object
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("invalid cache object name");
    }
    Ok(cache_directory(store)?.join(object))
}
pub(crate) fn is_marker(name: &str) -> bool {
    name == MARKER
}

fn target(store: &Store, path: Option<&Path>) -> Result<(PathBuf, Option<PathBuf>)> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| store.home.join("cache"));
    validate_location(&store.home, &path)?;
    let default = store.home.join("cache");
    let next = (path != default).then_some(path.clone());
    Ok((path, next))
}
fn locked_plan(store: &Store, path: Option<&Path>) -> Result<(Plan, Vec<File>)> {
    let source = cache_directory(store)?;
    let (destination, next) = target(store, path)?;
    let mut result = Plan {
        source,
        destination,
        next,
        objects: Vec::new(),
    };
    if result.source == result.destination {
        return Ok((result, Vec::new()));
    }
    if paths_overlap(&result.source, &result.destination)? {
        bail!("old and new cache directories cannot contain one another");
    }
    if !store.db.cache_reservations.is_empty() {
        bail!("cache downloads are active; retry after they finish");
    }
    if result.destination.exists() {
        marker(&store.home, &result.destination, false)?;
        for entry in fs::read_dir(&result.destination)? {
            if entry?.file_name() != MARKER {
                bail!("new cache directory must be empty; existing data will not be adopted or removed");
            }
        }
    }
    let indexed: BTreeSet<_> = store
        .db
        .cache
        .values()
        .map(|item| item.object.as_str())
        .collect();
    for entry in fs::read_dir(&result.source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == MARKER {
            continue;
        }
        if !name.to_str().is_some_and(|name| indexed.contains(name)) {
            bail!("old cache contains untracked files; migration left all files and settings in place");
        }
    }
    let mut guards = Vec::new();
    for item in store.db.cache.values() {
        if result
            .objects
            .iter()
            .any(|object| object.name == item.object)
        {
            continue;
        }
        let path = result.source.join(&item.object);
        // Earlier WebDAV versions created 0644 files inside the private 0700
        // cache. Reading that legacy owned file is safe; every copied payload
        // is newly created as 0600, without changing source files in previews.
        let mut file = owned_file(&path, false).context(
            "cached object is missing or unsafe; migration left the old location in place",
        )?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("cached content has an active reader; release it before changing the cache directory");
        }
        if file.metadata()?.len() != item.entry.size || cache::hash_fd(&mut file)? != item.sha256 {
            bail!("cached content checksum differs; migration left the old location in place");
        }
        result.objects.push(Object {
            name: item.object.clone(),
            size: item.entry.size,
            sha256: item.sha256.clone(),
            source_identity: (file.metadata()?.dev(), file.metadata()?.ino()),
            copy_identity: None,
        });
        guards.push(file);
    }
    Ok((result, guards))
}
fn report(store: &Store, plan: &Plan, apply: bool) -> Value {
    json!({"apply":apply,"changed":plan.source != plan.destination,"from_directory":plan.source,"directory":plan.destination,"default_directory":store.home.join("cache"),"custom_directory":plan.next.is_some(),"configured_directory":plan.next,"files":plan.objects.len(),"bytes":plan.objects.iter().map(|o| o.size).sum::<u64>(),"scope":"clean Rust file payloads only; excludes macOS native cache, upload snapshots and pending offline edits"})
}
pub fn directory_status(store: &Store) -> Result<Value> {
    Ok(
        json!({"directory":cache_directory(store)?,"default_directory":store.home.join("cache"),"custom_directory":store.config.cache.directory.is_some(),"configured_directory":store.config.cache.directory,"scope":"clean Rust file payloads only; excludes macOS native cache, upload snapshots and pending offline edits"}),
    )
}
pub fn plan_directory(store: &Store, path: Option<&Path>) -> Result<Value> {
    let _guard = cache::budget_lock(&store.home, false)?;
    let (plan, _files) = locked_plan(store, path)?;
    Ok(report(store, &plan, false))
}
fn stage(intent: &Intent) -> PathBuf {
    intent
        .destination
        .join(format!(".rdrive-migration-{}", intent.id))
}
fn verify(path: &Path, object: &Object) -> Result<File> {
    let mut file = private_file(path)?;
    if file.metadata()?.len() != object.size || cache::hash_fd(&mut file)? != object.sha256 {
        bail!("migration copy checksum mismatch; source payloads were retained");
    }
    Ok(file)
}
fn verify_source(path: &Path, object: &Object) -> Result<File> {
    let mut file = owned_file(path, false)?;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != object.source_identity
        || meta.len() != object.size
        || cache::hash_fd(&mut file)? != object.sha256
    {
        bail!("migration source changed; both copies were retained");
    }
    Ok(file)
}
fn verify_copy(path: &Path, object: &Object) -> Result<File> {
    let file = verify(path, object)?;
    let meta = file.metadata()?;
    if object.copy_identity != Some((meta.dev(), meta.ino())) {
        bail!("cache migration destination was replaced; source and unrelated destination files were retained");
    }
    Ok(file)
}
pub(crate) fn install(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("atomic cache installation is not supported on this operating system");
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cache destination changed; no existing destination file was overwritten");
    }
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Into::into)
}
fn ensure_members(directory: &Path, objects: &[Object], marker_allowed: bool) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name();
        if marker_allowed && name == MARKER {
            continue;
        }
        if !name
            .to_str()
            .is_some_and(|name| objects.iter().any(|o| o.name == name))
        {
            bail!("unexpected file in cache migration directory; all files were retained");
        }
    }
    Ok(())
}
/// Incomplete copies never become the selected cache. Cleanup is deliberately
/// narrow: only this intent's staging files and checksum-verified copies.
fn rollback(home: &Path, intent: &Intent) -> Result<()> {
    marker(home, &intent.destination, true)?;
    let staging = stage(intent);
    if staging.exists() {
        owned_directory(&staging)?;
        ensure_members(&staging, &intent.objects, false)?;
        for entry in fs::read_dir(&staging)? {
            private_file(&entry?.path())?;
        }
    }
    for object in &intent.objects {
        let path = intent.destination.join(&object.name);
        if fs::symlink_metadata(&path).is_ok() {
            verify_copy(&path, object)?;
        }
    }
    for object in &intent.objects {
        let path = intent.destination.join(&object.name);
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(path)?;
        }
    }
    if staging.exists() {
        for object in &intent.objects {
            let path = staging.join(&object.name);
            if fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(path)?;
            }
        }
        fs::remove_dir(staging)?;
    }
    sync_dir(&intent.destination)?;
    fs::remove_file(home.join(INTENT))?;
    sync_dir(home)
}
fn finish(home: &Path, intent: &Intent) -> Result<()> {
    marker(home, &intent.destination, true)?;
    marker(home, &intent.source, intent.previous.is_some())?;
    let mut guards = Vec::new();
    for object in &intent.objects {
        verify_copy(&intent.destination.join(&object.name), object)?;
        let source = intent.source.join(&object.name);
        if fs::symlink_metadata(&source).is_ok() {
            let file = verify_source(&source, object)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                bail!("old cache still has an active reader; both copies were retained");
            }
            guards.push((source, file));
        }
    }
    for (path, _guard) in &guards {
        fs::remove_file(path)?;
    }
    sync_dir(&intent.source)?;
    let staging = stage(intent);
    if staging.exists() {
        owned_directory(&staging)?;
        fs::remove_dir(staging)?;
    }
    fs::remove_file(home.join(INTENT))?;
    sync_dir(home)
}

trait MigrationIo {
    fn install(&self, source: &Path, destination: &Path) -> Result<()>;
    fn save_config(&self, store: &Store) -> Result<()>;
}
struct LocalMigration;
impl MigrationIo for LocalMigration {
    fn install(&self, source: &Path, destination: &Path) -> Result<()> { install(source,destination) }
    fn save_config(&self, store: &Store) -> Result<()> { store.save_config() }
}
pub fn set_directory(store: &mut Store, path: Option<&Path>) -> Result<Value> {
    set_directory_with(store,path,&LocalMigration)
}
fn set_directory_with(store: &mut Store, path: Option<&Path>, filesystem: &impl MigrationIo) -> Result<Value> {
    let _budget = cache::budget_lock(&store.home, true)?;
    let (plan, files) = locked_plan(store, path)?;
    if plan.source == plan.destination {
        return Ok(report(store, &plan, true));
    }
    if !plan.destination.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&plan.destination)?;
        sync_dir(plan.destination.parent().context("cache needs a parent")?)?;
    }
    write_marker(&store.home, &plan.destination)?;
    let mut intent = Intent {
        version: 1,
        home: binding(&store.home),
        id: uuid::Uuid::new_v4().to_string(),
        previous: store.config.cache.directory.clone(),
        next: plan.next.clone(),
        source: plan.source.clone(),
        destination: plan.destination.clone(),
        objects: plan.objects.clone(),
    };
    state::atomic_json(&store.home.join(INTENT), &intent)?;
    let copied: Result<()> = (|| {
        let staging = stage(&intent);
        fs::DirBuilder::new().mode(0o700).create(&staging)?;
        sync_dir(&intent.destination)?;
        for (object, source) in intent.objects.iter_mut().zip(&files) {
            let path = staging.join(&object.name);
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)?;
            let mut input = source.try_clone()?;
            input.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut input, &mut output)?;
            output.flush()?;
            output.sync_all()?;
            let copy = verify(&path, object)?;
            let meta = copy.metadata()?;
            object.copy_identity = Some((meta.dev(), meta.ino()));
        }
        sync_dir(&staging)?;
        state::atomic_json(&store.home.join(INTENT), &intent)?;
        for object in &intent.objects {
            filesystem.install(
                &staging.join(&object.name),
                &intent.destination.join(&object.name),
            )?;
        }
        sync_dir(&intent.destination)?;
        for object in &intent.objects {
            verify_source(&intent.source.join(&object.name), object)?;
        }
        Ok(())
    })();
    if let Err(error) = copied {
        rollback(&store.home, &intent).context("migration failed; the old location is still selected and all source content is retained")?;
        return Err(error);
    }
    store.config.cache.directory = plan.next.clone();
    if let Err(error) = filesystem.save_config(store) {
        let current: Config = state::read_json(&store.home.join("config.json"))?;
        store.config.cache.directory = current.cache.directory;
        if store.config.cache.directory == intent.previous {
            rollback(&store.home, &intent)?;
        }
        return Err(error).context("cache settings could not be confirmed; recovery intent and source data were retained when necessary");
    }
    // Keep the locks already held above: reopening and flocking these same
    // files would conflict with our own exclusive locks on some platforms.
    for object in &intent.objects {
        verify_copy(&intent.destination.join(&object.name), object)?;
        verify_source(&intent.source.join(&object.name), object)?;
    }
    for object in &intent.objects {
        fs::remove_file(intent.source.join(&object.name))?;
    }
    sync_dir(&intent.source)?;
    fs::remove_dir(stage(&intent))?;
    fs::remove_file(store.home.join(INTENT))?;
    sync_dir(&store.home)?;
    Ok(report(store, &plan, true))
}

/// Store holds its writer lock before calling this, before normal cache recovery.
pub(crate) fn recover(store: &mut Store) -> Result<()> {
    let path = store.home.join(INTENT);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let _guard = cache::budget_lock(&store.home, true)?;
    let intent: Intent = read_private(&path)?;
    if intent.version != 1
        || intent.home != binding(&store.home)
        || uuid::Uuid::parse_str(&intent.id).is_err()
        || intent.objects.len() > 100_000
        || intent.source == intent.destination
        || intent.source
            != intent
                .previous
                .clone()
                .unwrap_or_else(|| store.home.join("cache"))
        || intent.destination
            != intent
                .next
                .clone()
                .unwrap_or_else(|| store.home.join("cache"))
        || paths_overlap(&intent.source, &intent.destination)?
    {
        bail!("invalid cache migration intent; all payloads were retained");
    }
    validate_location(&store.home, &intent.source)?;
    validate_location(&store.home, &intent.destination)?;
    let mut names = BTreeSet::new();
    for object in &intent.objects {
        if object.name.is_empty()
            || object.name.len() > 128
            || !object
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || object.sha256.len() != 64
            || !object.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || !names.insert(&object.name)
        {
            bail!("invalid cache migration object");
        }
    }
    if store.config.cache.directory == intent.previous {
        rollback(&store.home, &intent)
    } else if store.config.cache.directory == intent.next {
        finish(&store.home, &intent)
    } else {
        bail!("cache configuration changed during a pending migration; source and destination were retained")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::Entry, state::CacheItem};
    use std::os::unix::fs::PermissionsExt;
    fn home() -> (tempfile::TempDir, Store) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let store = Store::open(root.join("state")).unwrap();
        (temporary, store)
    }
    fn seed(store: &mut Store, name: &str, bytes: &[u8], pinned: bool) -> CacheItem {
        let item = CacheItem {
            profile: "test".into(),
            remote: name.into(),
            object: uuid::Uuid::new_v4().to_string(),
            entry: Entry {
                path: name.into(),
                is_dir: false,
                size: bytes.len() as u64,
                modified: None,
                etag: Some("revision".into()),
            },
            sha256: state::key(std::str::from_utf8(bytes).unwrap()),
            pinned,
            accessed: state::now(),
            scope: None,
        };
        let path = object_path(store, &item.object).unwrap();
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        output.write_all(bytes).unwrap();
        output.sync_all().unwrap();
        store
            .db
            .cache
            .insert(Store::cache_key("test", name), item.clone());
        store.save().unwrap();
        store.save_config().unwrap();
        item
    }
    fn destination(store: &Store) -> PathBuf {
        store.home.parent().unwrap().join("external-cache")
    }
    fn intent(store: &Store, destination: PathBuf, item: &CacheItem) -> Intent {
        Intent {
            version: 1,
            home: binding(&store.home),
            id: uuid::Uuid::new_v4().to_string(),
            previous: None,
            next: Some(destination.clone()),
            source: store.home.join("cache"),
            destination,
            objects: vec![Object {
                name: item.object.clone(),
                size: item.entry.size,
                sha256: item.sha256.clone(),
                source_identity: {
                    let meta = fs::metadata(store.home.join("cache").join(&item.object)).unwrap();
                    (meta.dev(), meta.ino())
                },
                copy_identity: None,
            }],
        }
    }
    fn prepare(store: &Store, intent: &Intent) {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&intent.destination)
            .unwrap();
        write_marker(&store.home, &intent.destination).unwrap();
        state::atomic_json(&store.home.join(INTENT), intent).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(stage(intent))
            .unwrap();
    }
    #[test]
    fn migrates_verified_payloads_and_returns_to_default_without_moving_dirty_data() {
        let (_temporary, mut store) = home();
        let item = seed(&mut store, "clean", b"verified payload", true);
        let legacy = object_path(&store, &item.object).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(store.home.join("spool/pending"), b"unsent user data").unwrap();
        let next = destination(&store);
        let preview = plan_directory(&store, Some(&next)).unwrap();
        assert_eq!(preview["files"], 1);
        assert!(!next.exists());
        assert_eq!(fs::metadata(&legacy).unwrap().mode() & 0o777, 0o644);
        set_directory(&mut store, Some(&next)).unwrap();
        assert_eq!(
            fs::metadata(object_path(&store, &item.object).unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(object_path(&store, &item.object).unwrap()).unwrap(),
            b"verified payload"
        );
        assert!(!store.home.join("cache").join(&item.object).exists());
        assert_eq!(
            fs::read(store.home.join("spool/pending")).unwrap(),
            b"unsent user data"
        );
        let home = store.home.clone();
        drop(store);
        let mut store = Store::open(home).unwrap();
        assert_eq!(directory_status(&store).unwrap()["custom_directory"], true);
        set_directory(&mut store, None).unwrap();
        assert!(store.config.cache.directory.is_none());
        assert!(object_path(&store, &item.object).unwrap().exists());
        assert!(!next.join(item.object).exists());
        assert!(store.db.cache.values().next().unwrap().pinned);
    }
    #[test]
    fn active_readers_and_invalid_targets_leave_location_and_payloads_unchanged() {
        let (_temporary, mut store) = home();
        let item = seed(&mut store, "active", b"active payload", false);
        let file = private_file(&object_path(&store, &item.object).unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        let next = destination(&store);
        assert!(set_directory(&mut store, Some(&next)).is_err());
        assert!(!next.exists());
        assert!(store.config.cache.directory.is_none());
        drop(file);
        fs::create_dir(&next).unwrap();
        fs::set_permissions(&next, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(plan_directory(&store, Some(&next)).is_err());
        fs::set_permissions(&next, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(next.join("user-file"), b"do not delete").unwrap();
        assert!(set_directory(&mut store, Some(&next)).is_err());
        assert_eq!(fs::read(next.join("user-file")).unwrap(), b"do not delete");
        let link = next.with_extension("link");
        std::os::unix::fs::symlink(&next, &link).unwrap();
        assert!(plan_directory(&store, Some(&link)).is_err());
        let spool = store.home.join("spool");
        assert!(set_directory(&mut store, Some(&spool)).is_err());
        assert!(object_path(&store, &item.object).unwrap().exists());
    }
    #[test]
    fn clear_preserves_pins_leases_and_untracked_uuid_files_across_reopen() {
        let (_temporary, mut store) = home();
        let removable = seed(&mut store, "clean", b"remove", false);
        let pinned = seed(&mut store, "pinned", b"pinned", true);
        let active = seed(&mut store, "active", b"leased", false);
        let active_path = object_path(&store, &active.object).unwrap();
        let lease = private_file(&active_path).unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        let foreign = store
            .home
            .join("cache")
            .join(uuid::Uuid::new_v4().to_string());
        fs::write(&foreign, b"untracked user data").unwrap();
        let preview = cache::clear(&mut store, false).unwrap();
        assert_eq!(preview["candidates"], 1);
        assert_eq!(preview["skipped_active"], 1);
        assert_eq!(preview["skipped_pinned"], 1);
        assert!(object_path(&store, &removable.object).unwrap().exists());
        assert_eq!(cache::clear(&mut store, true).unwrap()["evicted"], 1);
        assert!(!object_path(&store, &removable.object).unwrap().exists());
        assert!(object_path(&store, &pinned.object).unwrap().exists());
        assert!(active_path.exists());
        let next = destination(&store);
        assert!(set_directory(&mut store, Some(&next)).is_err());
        let home = store.home.clone();
        drop(store);
        drop(lease);
        let store = Store::open(home).unwrap();
        assert_eq!(fs::read(&foreign).unwrap(), b"untracked user data");
        assert_eq!(cache::status(&store).unwrap().unmanaged_bytes, 19);
    }
    #[test]
    fn restart_rolls_back_partial_copy_before_configuration_commit() {
        let (_temporary, mut store) = home();
        let item = seed(&mut store, "clean", b"original cached payload", false);
        let intent = intent(&store, destination(&store), &item);
        prepare(&store, &intent);
        let mut partial = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage(&intent).join(&item.object))
            .unwrap();
        partial.write_all(b"partial").unwrap();
        partial.sync_all().unwrap();
        drop(partial);
        let home = store.home.clone();
        drop(store);
        let store = Store::open(home).unwrap();
        assert!(store.config.cache.directory.is_none());
        assert_eq!(
            fs::read(object_path(&store, &item.object).unwrap()).unwrap(),
            b"original cached payload"
        );
        assert!(!stage(&intent).exists());
        assert!(!store.home.join(INTENT).exists());
        assert!(intent.destination.join(MARKER).exists());
    }
    #[test]
    fn restart_finishes_committed_migration_but_preserves_source_if_copy_is_corrupt() {
        let (_temporary, mut store) = home();
        let item = seed(&mut store, "clean", b"source retained", false);
        let mut intent = intent(&store, destination(&store), &item);
        prepare(&store, &intent);
        let copied = intent.destination.join(&item.object);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&copied)
            .unwrap();
        output.write_all(b"bad").unwrap();
        let meta = output.metadata().unwrap();
        intent.objects[0].copy_identity = Some((meta.dev(), meta.ino()));
        state::atomic_json(&store.home.join(INTENT), &intent).unwrap();
        drop(output);
        store.config.cache.directory = intent.next.clone();
        store.save_config().unwrap();
        let home = store.home.clone();
        drop(store);
        assert!(Store::open(home.clone()).is_err());
        assert_eq!(
            fs::read(intent.source.join(&item.object)).unwrap(),
            b"source retained"
        );
        fs::write(&copied, b"source retained").unwrap();
        let store = Store::open(home).unwrap();
        assert!(!intent.source.join(&item.object).exists());
        assert_eq!(
            fs::read(object_path(&store, &item.object).unwrap()).unwrap(),
            b"source retained"
        );
        assert!(!store.home.join(INTENT).exists());
    }
    #[test]
    fn destination_conflicts_are_never_overwritten_or_removed_as_rollback_copies() {
        let (_temporary, mut store) = home();
        let item = seed(&mut store, "clean", b"same bytes", false);
        let mut intent = intent(&store, destination(&store), &item);
        prepare(&store, &intent);
        let staged = stage(&intent).join(&item.object);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .unwrap();
        output.write_all(b"same bytes").unwrap();
        output.sync_all().unwrap();
        let meta = output.metadata().unwrap();
        intent.objects[0].copy_identity = Some((meta.dev(), meta.ino()));
        state::atomic_json(&store.home.join(INTENT), &intent).unwrap();
        let foreign = intent.destination.join(&item.object);
        let mut foreign_output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&foreign)
            .unwrap();
        foreign_output.write_all(b"same bytes").unwrap();
        foreign_output.sync_all().unwrap();
        assert!(install(&staged, &foreign).is_err());
        assert!(rollback(&store.home, &intent).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"same bytes");
        assert!(staged.exists());
        assert!(intent.source.join(&item.object).exists());
    }
    #[test]
    fn path_identity_handles_aliases_without_rejecting_unrelated_siblings() {
        let (_temporary, store) = home();
        let sibling = destination(&store);
        fs::DirBuilder::new().mode(0o700).create(&sibling).unwrap();
        assert!(!paths_overlap(&store.home, &sibling).unwrap());
        assert!(path_is_within(&store.home.join("not-created-yet"), &store.home).unwrap());
        let alias = store.home.with_file_name("STATE");
        if alias.exists()
            && fs::metadata(&alias).unwrap().ino() == fs::metadata(&store.home).unwrap().ino()
        {
            assert!(paths_overlap(&alias.join("spool/new-cache"), &store.home).unwrap());
            assert!(paths_overlap(&alias, &store.home.join("cache")).unwrap());
            assert!(plan_directory(&store, Some(&alias.join("tmp"))).is_err());
        }
    }
    #[test]
    fn metadata_markers_and_ancestor_permissions_reject_foreign_cache_locations() {
        use std::os::unix::fs::symlink;
        let (_temporary,store)=home();let next=destination(&store);fs::DirBuilder::new().mode(0o700).create(&next).unwrap();
        assert!(private_file(&next).is_err());assert!(marker(&store.home,&next,true).is_err());
        let sparse=next.join("large.json");let file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&sparse).unwrap();file.set_len(32*1024*1024+1).unwrap();assert!(read_private::<Value>(&sparse).is_err());fs::remove_file(sparse).unwrap();
        write_marker(&store.home,&next).unwrap();assert!(marker(&next,&next,true).is_err());assert_eq!(plan_directory(&store,Some(&next)).unwrap()["files"],0);
        assert!(lexical(Path::new("relative")).is_err());let mut custom=Config::default();custom.cache.directory=Some(next.clone());assert!(protected_directories(&store.home,&custom).unwrap().contains(&next));
        fs::set_permissions(&next,fs::Permissions::from_mode(0o777)).unwrap();assert!(lexical(&next.join("child")).is_err());fs::set_permissions(&next,fs::Permissions::from_mode(0o700)).unwrap();
        assert!(lexical(&next.join("missing/child")).is_err());assert!(path_is_within(Path::new("relative"),&next).is_err());
        let alias=next.with_extension("alias");symlink(&next,&alias).unwrap();assert!(path_is_within(&alias,&next).unwrap());
        let loop_path=next.with_extension("loop");symlink(&loop_path,&loop_path).unwrap();assert!(path_is_within(&next,&loop_path).is_err());assert!(path_is_within(&loop_path,&store.home).is_err());
        let cloud=next.join("CloudStorage");fs::DirBuilder::new().mode(0o700).create(&cloud).unwrap();assert!(validate_path(&store.home,&cloud.join("cache")).is_err());
        state::atomic_json(&store.home.join("baselines/known.json"),&json!({"local_root":next})).unwrap();assert!(validate_location(&store.home,&next.join("child")).is_err());
        for object in ["","../escape","invalid.name",&"x".repeat(129)] {assert!(object_path(&store,object).is_err());}
    }
    #[test]
    fn pending_migrations_protect_both_locations_and_validate_intents_before_cleanup() {
        let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);let mut intent=intent(&store,next.clone(),&item);
        state::atomic_json(&store.home.join(INTENT),&intent).unwrap();let paths=protected_directories(&store.home,&store.config).unwrap();assert!(paths.contains(&intent.source));assert!(paths.contains(&intent.destination));
        intent.version=2;state::atomic_json(&store.home.join(INTENT),&intent).unwrap();assert!(protected_directories(&store.home,&store.config).is_err());assert!(recover(&mut store).is_err());
        intent.version=1;intent.objects[0].name="../escape".into();state::atomic_json(&store.home.join(INTENT),&intent).unwrap();assert!(recover(&mut store).is_err());
        intent.objects[0].name=item.object.clone();state::atomic_json(&store.home.join(INTENT),&intent).unwrap();
        store.config.cache.directory=Some(next.with_extension("unrelated"));assert!(recover(&mut store).unwrap_err().to_string().contains("configuration changed"));
        assert!(intent.source.join(item.object).exists());assert!(!next.exists());
    }
    #[test]
    fn migration_planning_checks_active_admissions_duplicate_objects_and_corruption() {
        let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);
        assert_eq!(plan_directory(&store,None).unwrap()["changed"],false);
        store.db.cache_reservations.insert("active".into(),crate::state::CacheReservation{size:1,created:state::now()});assert!(plan_directory(&store,Some(&next)).unwrap_err().to_string().contains("downloads are active"));store.db.cache_reservations.clear();
        store.db.cache.insert("duplicate-reference".into(),item.clone());assert_eq!(plan_directory(&store,Some(&next)).unwrap()["files"],1);store.db.cache.remove("duplicate-reference");
        fs::write(object_path(&store,&item.object).unwrap(),b"corrupt").unwrap();assert!(plan_directory(&store,Some(&next)).unwrap_err().to_string().contains("checksum differs"));fs::write(object_path(&store,&item.object).unwrap(),b"payload").unwrap();
        set_directory(&mut store,Some(&next)).unwrap();assert!(plan_directory(&store,Some(&next.join("child"))).unwrap_err().to_string().contains("contain one another"));
    }
    #[test]
    fn migration_rollback_and_finish_preserve_active_readers_and_only_remove_owned_copies() {
        let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);let mut intent=intent(&store,next.clone(),&item);prepare(&store,&intent);
        let staged=stage(&intent).join(&item.object);let mut output=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&staged).unwrap();output.write_all(b"partial copy").unwrap();drop(output);
        rollback(&store.home,&intent).unwrap();assert!(!staged.exists());assert!(intent.source.join(&item.object).exists());
        fs::remove_file(next.join(MARKER)).unwrap();fs::remove_dir(&next).unwrap();prepare(&store,&intent);
        let staged=stage(&intent).join(&item.object);let mut output=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&staged).unwrap();output.write_all(b"payload").unwrap();let metadata=output.metadata().unwrap();intent.objects[0].copy_identity=Some((metadata.dev(),metadata.ino()));drop(output);
        install(&staged,&next.join(&item.object)).unwrap();state::atomic_json(&store.home.join(INTENT),&intent).unwrap();store.config.cache.directory=Some(next.clone());
        let reader=private_file(&intent.source.join(&item.object)).unwrap();assert_eq!(unsafe{libc::flock(reader.as_raw_fd(),libc::LOCK_SH|libc::LOCK_NB)},0);
        assert!(recover(&mut store).unwrap_err().to_string().contains("active reader"));assert!(intent.source.join(&item.object).exists());assert!(next.join(&item.object).exists());drop(reader);
        recover(&mut store).unwrap();assert!(!intent.source.join(&item.object).exists());assert_eq!(fs::read(next.join(&item.object)).unwrap(),b"payload");assert!(!stage(&intent).exists());
    }

    struct FailedMigration { install_failure:bool, config_failure:bool, committed_config:bool }
    impl MigrationIo for FailedMigration {
        fn install(&self,source:&Path,destination:&Path)->Result<()> {if self.install_failure {bail!("synthetic destination install failure");}install(source,destination)}
        fn save_config(&self,store:&Store)->Result<()> {if self.committed_config {store.save_config()?;}if self.config_failure {bail!("synthetic config write acknowledgement failure");}store.save_config()}
    }
    #[test]
    fn failed_copy_or_config_publication_preserves_original_data_and_recovers_uncertain_commit() {
        for (installation,acknowledged) in [(true,false),(false,false),(false,true)] {
            let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);let home_path=store.home.clone();let source=home_path.join("cache").join(&item.object);
            let peer=FailedMigration{install_failure:installation,config_failure:!installation,committed_config:acknowledged};
            assert!(set_directory_with(&mut store,Some(&next),&peer).is_err());assert_eq!(fs::read(&source).unwrap(),b"payload");
            if acknowledged {assert_eq!(store.config.cache.directory,Some(next.clone()));assert!(home_path.join(INTENT).exists());drop(store);let reopened=Store::open(home_path).unwrap();assert_eq!(reopened.config.cache.directory,Some(next.clone()));assert!(!source.exists());assert_eq!(fs::read(next.join(&item.object)).unwrap(),b"payload");}
            else {assert!(store.config.cache.directory.is_none());assert!(!next.join(&item.object).exists());assert!(!home_path.join(INTENT).exists());}
        }
    }
    #[test]
    fn cleanup_accepts_already_removed_source_and_staging_but_refuses_extra_members() {
        let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);let mut intent=intent(&store,next.clone(),&item);prepare(&store,&intent);
        fs::remove_dir(stage(&intent)).unwrap();ensure_members(&next,&intent.objects,true).unwrap();fs::DirBuilder::new().mode(0o700).create(stage(&intent)).unwrap();
        fs::write(stage(&intent).join("foreign"),b"unrelated").unwrap();assert!(ensure_members(&stage(&intent),&intent.objects,false).is_err());assert!(rollback(&store.home,&intent).is_err());fs::remove_file(stage(&intent).join("foreign")).unwrap();
        let copy=next.join(&item.object);let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&copy).unwrap();file.write_all(b"payload").unwrap();let meta=file.metadata().unwrap();intent.objects[0].copy_identity=Some((meta.dev(),meta.ino()));drop(file);
        fs::remove_dir(stage(&intent)).unwrap();state::atomic_json(&store.home.join(INTENT),&intent).unwrap();rollback(&store.home,&intent).unwrap();assert!(!copy.exists());assert!(intent.source.join(&item.object).exists());
        fs::remove_file(next.join(MARKER)).unwrap();fs::remove_dir(&next).unwrap();prepare(&store,&intent);
        let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&copy).unwrap();file.write_all(b"payload").unwrap();let meta=file.metadata().unwrap();intent.objects[0].copy_identity=Some((meta.dev(),meta.ino()));drop(file);
        let source=intent.source.join(&item.object);fs::write(&source,b"changed").unwrap();assert!(verify_source(&source,&intent.objects[0]).is_err());fs::remove_file(source).unwrap();fs::remove_dir(stage(&intent)).unwrap();state::atomic_json(&store.home.join(INTENT),&intent).unwrap();store.config.cache.directory=Some(next);recover(&mut store).unwrap();assert!(copy.exists());
    }
    #[test]
    fn inaccessible_state_and_missing_payloads_fail_before_any_cache_mutation() {
        let (_temporary,mut store)=home();let item=seed(&mut store,"file",b"payload",false);let next=destination(&store);
        fs::remove_file(object_path(&store,&item.object).unwrap()).unwrap();assert!(plan_directory(&store,Some(&next)).unwrap_err().to_string().contains("missing or unsafe"));
        assert_eq!(set_directory(&mut store,None).unwrap()["changed"],false);
        state::atomic_json(&store.home.join("baselines/no-root.json"),&json!({"kind":"other"})).unwrap();state::atomic_json(&store.home.join("baselines/unrelated-root.json"),&json!({"local_root":next})).unwrap();
        fs::create_dir(store.home.join("baselines/directory")).unwrap();validate_location(&store.home,&next.with_extension("other")).unwrap();
        fs::set_permissions(&store.home,fs::Permissions::from_mode(0o000)).unwrap();let protected=protected_directories(&store.home,&store.config);let recovered=recover(&mut store);fs::set_permissions(&store.home,fs::Permissions::from_mode(0o700)).unwrap();assert!(protected.is_err());assert!(recovered.is_err());
    }

}
