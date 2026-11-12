//! Account-bound file history, independent of evictable content caches.
//!
//! Immutable verified before-images are committed before a destructive remote
//! operation. Only the caller's durable publication receipt confirms an entry;
//! prepared entries remain recoverable and are never removed by retention.
use crate::{
    backend::{self, Backend, Entry, WriteCondition},
    config::{Config, Profile},
    metadata::MetadataRecord,
    state::{self, Store},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryPolicy {
    pub enabled: bool,
    pub keep: usize,
    pub max_age_days: u32,
    pub max_bytes: Option<u64>,
}
impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            keep: 20,
            max_age_days: 30,
            max_bytes: Some(5 * 1024 * 1024 * 1024),
        }
    }
}
impl HistoryPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.keep == 0
            || self.keep > 100_000
            || self.max_age_days == 0
            || self.max_age_days > 36_500
            || self.max_bytes == Some(0)
        {
            bail!("history requires 1..100000 versions, 1..36500 days, and a positive byte limit");
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub default: HistoryPolicy,
    pub profiles: BTreeMap<String, HistoryPolicy>,
}
impl HistoryConfig {
    pub fn effective(&self, profile: &str) -> HistoryPolicy {
        self.profiles.get(profile).unwrap_or(&self.default).clone()
    }
    pub fn validate(&self) -> Result<()> {
        self.default.validate()?;
        for (name, policy) in &self.profiles {
            if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
                bail!("invalid history profile name");
            }
            policy.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub item_id: String,
    pub profile: String,
    pub path: String,
    pub kind: String,
    pub reason: String,
    pub created: String,
    pub bytes: u64,
    pub files: usize,
    pub state: String,
    pub restorable: bool,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_backup: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    path: String,
    entry: Entry,
    sha256: Option<String>,
    metadata: Option<MetadataRecord>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Archive {
    version: u32,
    binding: String,
    request_id: String,
    public: HistoryEntry,
    original: Entry,
    nodes: Vec<Node>,
}

fn binding(config: &Config, profile: &str) -> Result<String> {
    let Profile {
        protocol,
        url,
        username,
        ..
    } = config
        .profiles
        .get(profile)
        .context("history profile not found")?;
    Ok(state::key(&serde_json::to_string(&(
        profile, protocol, url, username,
    ))?))
}
fn private_directory(path: &Path, create: bool) -> Result<Option<PathBuf>> {
    if create {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        bail!("history directories must be private, real and owned by the current user");
    }
    Ok(Some(path.canonicalize()?))
}
fn private_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        bail!("history files must be private owned regular files without links");
    }
    Ok(())
}
fn open(home: &Path, write: bool) -> Result<Option<(PathBuf, Connection)>> {
    private_directory(home, false)?.context("history state directory does not exist")?;
    let Some(root) = private_directory(&home.join("history"), write)? else {
        return Ok(None);
    };
    let path = root.join("history.sqlite3");
    let exists = match fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !exists && !write {
        return Ok(None);
    }
    if exists {
        private_file(&path)?;
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = root.join(format!("history.sqlite3{suffix}"));
        if fs::symlink_metadata(&sidecar).is_ok() {
            private_file(&sidecar)?;
        }
    }
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    if !exists {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    connection.busy_timeout(Duration::from_secs(3))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    if write {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS entries(id TEXT PRIMARY KEY,binding TEXT NOT NULL,profile TEXT NOT NULL,item_id TEXT NOT NULL,path TEXT NOT NULL,created TEXT NOT NULL,state TEXT NOT NULL,bytes INTEGER NOT NULL,request_id TEXT NOT NULL,archive TEXT NOT NULL,UNIQUE(binding,request_id));
            CREATE INDEX IF NOT EXISTS history_profile ON entries(binding,created DESC,id);
            CREATE INDEX IF NOT EXISTS history_identity ON entries(binding,item_id,created DESC);
            CREATE TABLE IF NOT EXISTS aliases(binding TEXT NOT NULL,path TEXT NOT NULL,item_id TEXT NOT NULL,PRIMARY KEY(binding,path));
            CREATE TABLE IF NOT EXISTS actions(id TEXT PRIMARY KEY,binding TEXT NOT NULL,fingerprint TEXT NOT NULL,value TEXT NOT NULL,result TEXT);")?;
        private_directory(&root.join("blobs"), true)?;
    }
    Ok(Some((root, connection)))
}
fn identifier(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid history identity");
    }
    Ok(())
}
fn remote_path(path: &str) -> Result<()> {
    if path.is_empty() || backend::normalize(path)? != path {
        bail!("history requires a normalized non-root remote path");
    }
    Ok(())
}
fn blob(root: &Path, hash: &str) -> Result<PathBuf> {
    identifier(hash)?;
    Ok(root.join("blobs").join(hash))
}
fn verify_blob(root: &Path, node: &Node) -> Result<PathBuf> {
    let path = blob(
        root,
        node.sha256
            .as_deref()
            .context("history file has no content hash")?,
    )?;
    private_file(&path)?;
    if fs::metadata(&path)?.len() != node.entry.size
        || state::fingerprint(&path)? != node.sha256.as_deref().unwrap()
    {
        bail!("history content checksum mismatch; destination was preserved");
    }
    Ok(path)
}
fn checked_archive(text: &str, expected_binding: &str) -> Result<Archive> {
    let archive: Archive = serde_json::from_str(text).context("invalid history manifest")?;
    identifier(&archive.public.id)?;
    if archive.version != 1
        || archive.binding != expected_binding
        || archive.public.source != "local-history"
        || !["prepared", "confirmed"].contains(&archive.public.state.as_str())
        || !["overwrite", "delete", "restore", "checkpoint"]
            .contains(&archive.public.reason.as_str())
    {
        bail!("history manifest does not match its account or format");
    }
    remote_path(&archive.public.path)?;
    if archive.nodes.is_empty() || archive.nodes.len() > 1_000_000 {
        bail!("invalid history tree");
    }
    let mut paths = BTreeSet::new();
    let mut total = 0u64;
    for node in &archive.nodes {
        if backend::normalize(&node.path)? != node.path
            || !paths.insert(node.path.clone())
            || node.entry.is_dir != node.sha256.is_none()
        {
            bail!("invalid history tree node");
        }
        if let Some(hash) = &node.sha256 {
            identifier(hash)?;
            total = total
                .checked_add(node.entry.size)
                .context("history size overflow")?;
        }
        if let Some(metadata) = &node.metadata {
            metadata.validate()?;
            if Some(metadata.content_sha256.as_str()) != node.sha256.as_deref() {
                bail!("history metadata belongs to different contents");
            }
        }
    }
    if archive.nodes[0].path != ""
        || archive.public.bytes != total
        || archive.public.kind
            != if archive.nodes[0].entry.is_dir {
                "directory"
            } else {
                "file"
            }
    {
        bail!("history tree summary mismatch");
    }
    Ok(archive)
}
fn load(connection: &Connection, id: &str, account: &str) -> Result<Archive> {
    identifier(id)?;
    let text: String = connection
        .query_row(
            "SELECT archive FROM entries WHERE id=?1 AND binding=?2",
            params![id, account],
            |row| row.get(0),
        )
        .optional()?
        .context("history version not found for this connection")?;
    let archive = checked_archive(&text, account)?;
    if archive.public.id != id {
        bail!("history identity does not match manifest");
    }
    Ok(archive)
}
fn write_archive(connection: &Connection, archive: &Archive) -> Result<()> {
    let item = &archive.public;
    connection.execute("INSERT INTO entries(id,binding,profile,item_id,path,created,state,bytes,request_id,archive) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
        ON CONFLICT(id) DO UPDATE SET state=excluded.state,archive=excluded.archive",params![item.id,archive.binding,item.profile,item.item_id,item.path,item.created,item.state,i64::try_from(item.bytes)?,archive.request_id,serde_json::to_string(archive)?])?;
    Ok(())
}

/// Readers do not take Store's writer lease and never create an empty index.
pub fn list(
    home: &Path,
    config: &Config,
    profile: &str,
    path: Option<&str>,
    item_id: Option<&str>,
) -> Result<Vec<HistoryEntry>> {
    let account = binding(config, profile)?;
    if let Some(path) = path {
        if backend::normalize(path)? != path {
            bail!("invalid history query path");
        }
    }
    let Some((_root, connection)) = open(home, false)? else {
        return Ok(Vec::new());
    };
    let mut identities = BTreeSet::new();
    if let Some(id) = item_id {
        identities.insert(id.to_owned());
    }
    if let Some(path) = path {
        let mut aliases=connection.prepare("SELECT item_id FROM aliases WHERE binding=?1 AND (path=?2 OR substr(path,1,length(?2)+1)=?2||'/')")?;
        let rows = aliases.query_map(params![account, path], |row| row.get::<_, String>(0))?;
        for row in rows {
            identities.insert(row?);
        }
    }
    let mut query = connection
        .prepare("SELECT archive FROM entries WHERE binding=?1 ORDER BY created DESC,id DESC")?;
    let rows = query.query_map([&account], |row| row.get::<_, String>(0))?;
    let mut result = Vec::new();
    for row in rows {
        let archive = checked_archive(&row?, &account)?;
        let selected = identities.contains(&archive.public.item_id)
            || path.is_none() && item_id.is_none()
            || path.is_some_and(|path| {
                archive.public.path == path || archive.public.path.starts_with(&format!("{path}/"))
            });
        if selected {
            result.push(archive.public);
        }
    }
    Ok(result)
}
/// Resolve the current logical alias without guessing between two paths.
/// Historical entry paths remain immutable even after the current alias moves.
pub fn current_path(
    home: &Path,
    config: &Config,
    profile: &str,
    item_id: &str,
) -> Result<Option<String>> {
    let account = binding(config, profile)?;
    let Some((_root, connection)) = open(home, false)? else {
        return Ok(None);
    };
    let mut query = connection.prepare(
        "SELECT path FROM aliases WHERE binding=?1 AND item_id=?2 ORDER BY path LIMIT 2",
    )?;
    let rows = query.query_map(params![account, item_id], |row| row.get::<_, String>(0))?;
    let paths = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if paths.len() > 1 {
        bail!("history current path is ambiguous; restore to an explicitly reviewed separate copy");
    }
    if let Some(path) = paths.first() {
        remote_path(path)?;
    }
    Ok(paths.into_iter().next())
}

pub fn status(home: &Path, config: &Config, profile: &str) -> Result<Value> {
    let entries = list(home, config, profile, None, None)?;
    let policy = config.history.effective(profile);
    policy.validate()?;
    let bytes = entries.iter().try_fold(0u64, |sum, item| {
        sum.checked_add(item.bytes).context("history size overflow")
    })?;
    Ok(
        json!({"profile":profile,"policy":policy,"entries":entries.len(),"bytes":bytes,"prepared":entries.iter().filter(|item|item.state=="prepared").count(),"directory":home.join("history"),"separate_from_cache":true}),
    )
}
pub fn lookup(home: &Path, config: &Config, profile: &str, id: &str) -> Result<HistoryEntry> {
    let account = binding(config, profile)?;
    let (_root, connection) = open(home, false)?.context("history is empty")?;
    Ok(load(&connection, id, &account)?.public)
}

fn roots(expected: &Entry, listed: Vec<Entry>) -> Result<Vec<(String, Entry)>> {
    let mut result = vec![(String::new(), expected.clone())];
    for entry in listed {
        let path = entry
            .path
            .strip_prefix(&format!("{}/", expected.path))
            .context("history server returned a child outside its tree")?
            .to_owned();
        if path.is_empty() || backend::normalize(&path)? != path {
            bail!("invalid history descendant path");
        }
        result.push((path, entry));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    if result.windows(2).any(|nodes| nodes[0].0 == nodes[1].0) {
        bail!("history server returned duplicate children");
    }
    Ok(result)
}
async fn observed_tree(
    backend: &dyn Backend,
    path: &str,
    expected: &Entry,
) -> Result<Vec<(String, Entry)>> {
    let current = backend
        .stat(path)
        .await?
        .context("version_conflict: history source disappeared")?;
    if current.path != path || !current.same_revision(expected) {
        bail!("version_conflict: history source changed before capture");
    }
    roots(
        &current,
        if current.is_dir {
            backend::walk(backend, path).await?
        } else {
            Vec::new()
        },
    )
}
fn store_blob(root: &Path, source: &Path, hash: &str) -> Result<()> {
    store_blob_with(root, source, hash, |from, to| fs::hard_link(from, to))
}
fn store_blob_with(
    root: &Path,
    source: &Path,
    hash: &str,
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let destination = blob(root, hash)?;
    if destination.exists() {
        private_file(&destination)?;
        if state::fingerprint(&destination)? != hash {
            bail!("existing history blob is corrupt");
        }
        return Ok(());
    }
    let temporary = root
        .join("blobs")
        .join(format!(".capture-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        if state::fingerprint(&temporary)? != hash {
            bail!("history source changed during immutable capture");
        }
        match link(&temporary, &destination) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                private_file(&destination)?;
                if state::fingerprint(&destination)? != hash {
                    bail!("history blob collision");
                }
            }
            Err(error) => return Err(error.into()),
        }
        fs::remove_file(&temporary)?;
        File::open(root.join("blobs"))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Persist an immutable before-image before any destructive remote request.
pub async fn capture(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    path: &str,
    identity: Option<&str>,
    reason: &str,
    request_id: &str,
    expected: &Entry,
    force: bool,
) -> Result<Option<HistoryEntry>> {
    let policy = store.config.history.effective(profile);
    policy.validate()?;
    if !force && !policy.enabled {
        return Ok(None);
    }
    remote_path(path)?;
    if request_id.is_empty()
        || request_id.len() > 4096
        || !["overwrite", "delete", "restore", "checkpoint"].contains(&reason)
        || expected.path != path
    {
        bail!("invalid history capture identity");
    }
    let account = binding(&store.config, profile)?;
    let id = state::key(&serde_json::to_string(&(&account, request_id))?);
    let (root, mut connection) = open(&store.home, true)?.unwrap();
    if let Some(text) = connection
        .query_row("SELECT archive FROM entries WHERE id=?1", [&id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    {
        let prior = checked_archive(&text, &account)?;
        if prior.public.path != path
            || prior.public.reason != reason
            || !prior.original.same_revision(expected)
        {
            bail!("version_conflict: history request was reused for another source");
        }
        for node in prior.nodes.iter().filter(|node| !node.entry.is_dir) {
            verify_blob(&root, node)?;
        }
        return Ok(Some(prior.public));
    }
    let before = observed_tree(backend, path, expected).await?;
    let bytes = before.iter().try_fold(0u64, |sum, (_, entry)| {
        sum.checked_add(if entry.is_dir { 0 } else { entry.size })
            .context("history size overflow")
    })?;
    if let Some(maximum) = policy.max_bytes {
        if bytes > maximum {
            bail!("insufficient_quota: history capacity is insufficient; remote operation was not started");
        }
        let mut reserved = policy.clone();
        reserved.max_bytes = Some(maximum - bytes);
        retain_policy(store, profile, true, false, reserved)?;
    }
    let used: i64 = connection.query_row(
        "SELECT COALESCE(SUM(bytes),0) FROM entries WHERE binding=?1",
        [&account],
        |row| row.get(0),
    )?;
    let used = u64::try_from(used)?;
    if policy
        .max_bytes
        .is_some_and(|maximum| used.checked_add(bytes).is_none_or(|total| total > maximum))
    {
        bail!("insufficient_quota: history capacity is insufficient; remote operation was not started");
    }
    let mut nodes = Vec::new();
    for (suffix, entry) in &before {
        let (sha256, metadata) = if entry.is_dir {
            (None, None)
        } else {
            let temporary = store
                .home
                .join("tmp")
                .join(format!("history-{}", uuid::Uuid::new_v4()));
            let result = async {
                backend
                    .download_checked(&entry.path, &temporary, entry)
                    .await?;
                let after = backend
                    .stat(&entry.path)
                    .await?
                    .context("version_conflict: history file disappeared")?;
                let hash = state::fingerprint(&temporary)?;
                if !entry.same_revision(&after)
                    || fs::metadata(&temporary)?.len() != entry.size
                    || backend
                        .content_sha256(entry)
                        .is_some_and(|expected| expected != hash)
                {
                    bail!("version_conflict: history contents changed during capture");
                }
                let metadata = crate::metadata_remote::load(backend, &entry.path, entry).await?;
                if metadata
                    .as_ref()
                    .is_some_and(|record| record.content_sha256 != hash)
                {
                    bail!("history metadata content hash mismatch");
                }
                store_blob(&root, &temporary, &hash)?;
                Ok::<_, anyhow::Error>((Some(hash), metadata))
            }
            .await;
            let _ = fs::remove_file(temporary);
            result?
        };
        nodes.push(Node {
            path: suffix.clone(),
            entry: entry.clone(),
            sha256,
            metadata,
        });
    }
    let after = observed_tree(backend, path, expected).await?;
    if before.len() != after.len()
        || before
            .iter()
            .zip(&after)
            .any(|((a, old), (b, new))| a != b || !old.same_revision(new))
    {
        bail!("version_conflict: history directory changed during capture");
    }
    let item_id = connection
        .query_row(
            "SELECT item_id FROM aliases WHERE binding=?1 AND path=?2",
            params![account, path],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .or_else(|| identity.map(str::to_owned))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let public = HistoryEntry {
        id: id.clone(),
        item_id,
        profile: profile.into(),
        path: path.into(),
        kind: if expected.is_dir { "directory" } else { "file" }.into(),
        reason: reason.into(),
        created: state::now(),
        bytes,
        files: nodes.iter().filter(|node| !node.entry.is_dir).count(),
        state: "prepared".into(),
        restorable: true,
        source: "local-history".into(),
        remote_backup: None,
    };
    let archive = Archive {
        version: 1,
        binding: account.clone(),
        request_id: request_id.into(),
        public: public.clone(),
        original: expected.clone(),
        nodes,
    };
    let transaction = connection.transaction()?;
    write_archive(&transaction, &archive)?;
    transaction.execute("INSERT INTO aliases(binding,path,item_id) VALUES(?1,?2,?3) ON CONFLICT(binding,path) DO UPDATE SET item_id=excluded.item_id",params![account,path,public.item_id])?;
    transaction.commit()?;
    Ok(Some(public))
}

pub fn confirm(
    store: &Store,
    profile: &str,
    id: &str,
    remote_backup: Option<&str>,
) -> Result<HistoryEntry> {
    let account = binding(&store.config, profile)?;
    let (_root, connection) = open(&store.home, true)?.unwrap();
    let mut archive = load(&connection, id, &account)?;
    if let Some(path) = remote_backup {
        remote_path(path)?;
        archive.public.remote_backup = Some(path.into());
    }
    archive.public.state = "confirmed".into();
    write_archive(&connection, &archive)?;
    Ok(archive.public)
}
pub fn confirm_request(
    store: &Store,
    profile: &str,
    request_id: &str,
    remote_backup: Option<&str>,
) -> Result<Option<HistoryEntry>> {
    let account = binding(&store.config, profile)?;
    let id = state::key(&serde_json::to_string(&(&account, request_id))?);
    let Some((_root, connection)) = open(&store.home, false)? else {
        return Ok(None);
    };
    if connection
        .query_row(
            "SELECT 1 FROM entries WHERE id=?1 AND binding=?2",
            params![id, account],
            |row| row.get::<_, u32>(0),
        )
        .optional()?
        .is_none()
    {
        return Ok(None);
    }
    drop(connection);
    confirm(store, profile, &id, remote_backup).map(Some)
}
pub fn renamed(store: &Store, profile: &str, from: &str, to: &str) -> Result<()> {
    remote_path(from)?;
    remote_path(to)?;
    if from == to {
        return Ok(());
    }
    let account = binding(&store.config, profile)?;
    let Some((_root, connection)) = open(&store.home, false)? else {
        return Ok(());
    };
    drop(connection);
    let (_, mut writable) = open(&store.home, true)?.unwrap();
    let transaction = writable.transaction()?;
    let rows = {
        let mut query = transaction.prepare("SELECT path,item_id FROM aliases WHERE binding=?1")?;
        let mapped = query.query_map([&account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (path, item) in rows {
        if path == from || path.starts_with(&format!("{from}/")) {
            let target = format!("{to}{}", &path[from.len()..]);
            transaction.execute("INSERT INTO aliases(binding,path,item_id) VALUES(?1,?2,?3) ON CONFLICT(binding,path) DO UPDATE SET item_id=excluded.item_id",params![account,target,item])?;
            transaction.execute(
                "DELETE FROM aliases WHERE binding=?1 AND path=?2",
                params![account, path],
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn local_output(store: &Store, destination: &Path) -> Result<PathBuf> {
    if !destination.is_absolute()
        || destination
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("history export destination must be an absolute contained path");
    }
    let parent = destination
        .parent()
        .context("history export requires a parent directory")?
        .canonicalize()?;
    if parent.starts_with(store.home.canonicalize()?) {
        bail!("history export cannot overwrite application state");
    }
    let destination = parent.join(
        destination
            .file_name()
            .context("history export destination needs a filename")?,
    );
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("history export destination already exists");
    }
    Ok(destination)
}
fn copy_node(root: &Path, node: &Node, destination: &Path) -> Result<()> {
    copy_node_with(root, node, destination, |input, output| {
        std::io::copy(input, output)
    })
}
fn copy_node_with(
    root: &Path,
    node: &Node,
    destination: &Path,
    copy: impl FnOnce(&mut File, &mut File) -> std::io::Result<u64>,
) -> Result<()> {
    let source = verify_blob(root, node)?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    copy(&mut input, &mut output)?;
    output.sync_all()?;
    if output.metadata()?.len() != node.entry.size
        || Some(state::fingerprint(destination)?.as_str()) != node.sha256.as_deref()
    {
        bail!("history export checksum mismatch; existing destinations were preserved");
    }
    if let Some(metadata) = &node.metadata {
        crate::metadata::apply(destination, metadata, &Default::default())?;
    }
    Ok(())
}
pub fn export(store: &Store, profile: &str, id: &str, destination: &Path) -> Result<Value> {
    let destination = local_output(store, destination)?;
    export_inner(store, profile, id, &destination)
}
pub fn export_staged(store: &Store, profile: &str, id: &str, staging: &Path) -> Result<Value> {
    let staging =
        private_directory(staging, false)?.context("history staging directory is missing")?;
    export_inner(
        store,
        profile,
        id,
        &staging.join(uuid::Uuid::new_v4().to_string()),
    )
}
fn export_inner(store: &Store, profile: &str, id: &str, destination: &Path) -> Result<Value> {
    let account = binding(&store.config, profile)?;
    let (root, connection) = open(&store.home, false)?.context("history is empty")?;
    let archive = load(&connection, id, &account)?;
    for node in archive.nodes.iter().filter(|node| !node.entry.is_dir) {
        verify_blob(&root, node)?;
    }
    if fs::symlink_metadata(destination).is_ok() {
        bail!("history export destination already exists");
    }
    if archive.public.kind == "file" {
        copy_node(&root, &archive.nodes[0], &destination)?;
    } else {
        fs::DirBuilder::new().mode(0o700).create(&destination)?;
        for node in &archive.nodes[1..] {
            let target = destination.join(&node.path);
            if node.entry.is_dir {
                fs::DirBuilder::new().mode(0o700).create(&target)?;
            } else {
                copy_node(&root, node, &target)?;
            }
        }
    }
    let files=archive.nodes.iter().map(|node|json!({"path":node.path,"kind":if node.entry.is_dir{"directory"}else{"file"},"bytes":node.entry.size,"sha256":node.sha256,"metadata":node.metadata})).collect::<Vec<_>>();
    let sha256 = if archive.public.kind == "file" {
        archive.nodes[0].sha256.clone().unwrap()
    } else {
        state::key(&serde_json::to_string(&files)?)
    };
    Ok(
        json!({"exported":true,"verified":true,"path":destination,"content_path":destination,"kind":archive.public.kind,"bytes":archive.public.bytes,"sha256":sha256,"files":files,"entry":archive.public}),
    )
}

pub fn prune(store: &Store, profile: &str, apply: bool) -> Result<Value> {
    retain(store, profile, apply, false)
}
pub fn clear(store: &Store, profile: &str, apply: bool) -> Result<Value> {
    retain(store, profile, apply, true)
}
fn retain(store: &Store, profile: &str, apply: bool, clear: bool) -> Result<Value> {
    let policy = store.config.history.effective(profile);
    policy.validate()?;
    retain_policy(store, profile, apply, clear, policy)
}
fn retain_policy(
    store: &Store,
    profile: &str,
    apply: bool,
    clear: bool,
    policy: HistoryPolicy,
) -> Result<Value> {
    let account = binding(&store.config, profile)?;
    let entries = list(&store.home, &store.config, profile, None, None)?;
    let protected = if let Some((_root, connection)) = open(&store.home, false)? {
        let mut query =
            connection.prepare("SELECT value FROM actions WHERE binding=?1 AND result IS NULL")?;
        let rows = query.query_map([&account], |row| row.get::<_, String>(0))?;
        let mut protected = BTreeSet::new();
        for row in rows {
            let value: Value = serde_json::from_str(&row?)?;
            for field in ["entry_id", "prior"] {
                if let Some(id) = value[field].as_str() {
                    identifier(id)?;
                    protected.insert(id.to_owned());
                }
            }
        }
        protected
    } else {
        BTreeSet::new()
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(policy.max_age_days.into());
    let mut counts = BTreeMap::<String, usize>::new();
    let mut keep = Vec::new();
    let mut remove = Vec::new();
    for entry in entries {
        let count = counts.entry(entry.item_id.clone()).or_default();
        let expired = chrono::DateTime::parse_from_rfc3339(&entry.created)?
            .with_timezone(&chrono::Utc)
            < cutoff;
        if entry.state == "prepared" || protected.contains(&entry.id) {
            keep.push(entry);
        } else if clear || *count >= policy.keep || expired {
            remove.push(entry);
        } else {
            *count += 1;
            keep.push(entry);
        }
    }
    let mut bytes = keep.iter().try_fold(0u64, |sum, item| {
        sum.checked_add(item.bytes).context("history size overflow")
    })?;
    if let Some(maximum) = policy.max_bytes {
        let mut index = keep.len();
        while bytes > maximum && index > 0 {
            index -= 1;
            if keep[index].state == "confirmed" && !protected.contains(&keep[index].id) {
                let entry = keep.remove(index);
                bytes -= entry.bytes;
                remove.push(entry);
            }
        }
    }
    if apply && open(&store.home, false)?.is_some() {
        let (root, mut connection) = open(&store.home, true)?.unwrap();
        let transaction = connection.transaction()?;
        for entry in &remove {
            transaction.execute(
                "DELETE FROM entries WHERE id=?1 AND binding=?2 AND state='confirmed'",
                params![entry.id, account],
            )?;
        }
        transaction.commit()?;
        let referenced = {
            let mut query = connection.prepare("SELECT archive FROM entries")?;
            let mapped = query.query_map([], |row| row.get::<_, String>(0))?;
            let mut hashes = BTreeSet::new();
            for row in mapped {
                let archive: Archive = serde_json::from_str(&row?)?;
                for node in archive.nodes {
                    if let Some(hash) = node.sha256 {
                        hashes.insert(hash);
                    }
                }
            }
            hashes
        };
        for entry in fs::read_dir(root.join("blobs"))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid history blob filename"))?;
            if name.starts_with('.') {
                continue;
            }
            identifier(&name)?;
            private_file(&entry.path())?;
            if !referenced.contains(&name) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(
        json!({"apply":apply,"clear":clear,"profile":profile,"candidates":remove.len(),"removed":if apply{remove.len()}else{0},"retained_bytes":bytes,"protected_prepared":keep.iter().filter(|entry|entry.state=="prepared"||protected.contains(&entry.id)).count(),"policy":policy}),
    )
}

fn import_limit(count: usize) -> Result<()> {
    if count > 1_000_000 {
        bail!("checkpoint payload has too many entries");
    }
    Ok(())
}
fn import_blob(root: &Path, local: &Path, hash: &str, bytes: u64) -> Result<()> {
    if fs::symlink_metadata(local)?.len() != bytes {
        bail!("checkpoint payload changed before capture");
    }
    store_blob(root, local, hash)
}

fn import_name(name: std::ffi::OsString) -> Result<String> {
    name.into_string()
        .map_err(|_| anyhow::anyhow!("checkpoint filename is not UTF-8"))
}

/// Import a verified checkpoint payload as a new history entry. This function
/// never opens or replaces an old application database and never writes remote.
pub fn capture_import(
    store: &Store,
    profile: &str,
    path: &str,
    source: &Path,
    request_id: &str,
) -> Result<HistoryEntry> {
    remote_path(path)?;
    let account = binding(&store.config, profile)?;
    if request_id.is_empty() || request_id.len() > 4096 {
        bail!("checkpoint import needs a stable request identity");
    }
    let policy = store.config.history.effective(profile);
    policy.validate()?;
    let (root, mut connection) = open(&store.home, true)?.unwrap();
    let mut todo = vec![(String::new(), source.to_path_buf())];
    let mut nodes = Vec::new();
    let mut bytes = 0u64;
    let mut contents = Vec::new();
    while let Some((suffix, local)) = todo.pop() {
        import_limit(nodes.len() + todo.len())?;
        let info = fs::symlink_metadata(&local)?;
        if info.file_type().is_symlink() || !info.is_dir() && !info.is_file() {
            bail!("checkpoint import accepts only real files and directories");
        }
        let remote = backend::join(path, &suffix)?;
        if info.is_dir() {
            let mut children = fs::read_dir(&local)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children.into_iter().rev() {
                let name = import_name(child.file_name())?;
                todo.push((backend::join(&suffix, &name)?, child.path()));
            }
            nodes.push(Node {
                path: suffix,
                entry: Entry {
                    path: remote,
                    is_dir: true,
                    size: 0,
                    modified: None,
                    etag: Some(format!("checkpoint-{}", state::key(request_id))),
                },
                sha256: None,
                metadata: None,
            });
        } else {
            let metadata = crate::metadata::capture(&local, &Default::default())?;
            bytes = bytes
                .checked_add(info.len())
                .context("history size overflow")?;
            if policy.max_bytes.is_some_and(|maximum| bytes > maximum) {
                bail!("insufficient_quota: history capacity is insufficient for checkpoint payload");
            }
            contents.push((local.clone(), metadata.content_sha256.clone(), info.len()));
            nodes.push(Node {
                path: suffix,
                entry: Entry {
                    path: remote,
                    is_dir: false,
                    size: info.len(),
                    modified: None,
                    etag: Some(format!("checkpoint-sha256:{}", metadata.content_sha256)),
                },
                sha256: Some(metadata.content_sha256.clone()),
                metadata: Some(metadata),
            });
        }
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    let id = state::key(&serde_json::to_string(&(&account, request_id))?);
    let fingerprint = state::key(&serde_json::to_string(&nodes)?);
    if let Some(text) = connection
        .query_row("SELECT archive FROM entries WHERE id=?1", [&id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    {
        let prior = checked_archive(&text, &account)?;
        if prior.public.path != path
            || prior.public.reason != "checkpoint"
            || state::key(&serde_json::to_string(&prior.nodes)?) != fingerprint
        {
            bail!("version_conflict: checkpoint import identity was reused for different data");
        }
        return Ok(prior.public);
    }
    let used: i64 = connection.query_row(
        "SELECT COALESCE(SUM(bytes),0) FROM entries WHERE binding=?1",
        [&account],
        |row| row.get(0),
    )?;
    let used = u64::try_from(used)?;
    if policy
        .max_bytes
        .is_some_and(|maximum| used.checked_add(bytes).is_none_or(|sum| sum > maximum))
    {
        bail!("insufficient_quota: history capacity is insufficient; checkpoint payload was not published");
    }
    for (local, hash, bytes) in contents {
        import_blob(&root, &local, &hash, bytes)?;
    }
    let item_id = connection
        .query_row(
            "SELECT item_id FROM aliases WHERE binding=?1 AND path=?2",
            params![account, path],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let public = HistoryEntry {
        id: id.clone(),
        item_id,
        profile: profile.into(),
        path: path.into(),
        kind: if nodes[0].entry.is_dir {
            "directory"
        } else {
            "file"
        }
        .into(),
        reason: "checkpoint".into(),
        created: state::now(),
        bytes,
        files: nodes.iter().filter(|node| !node.entry.is_dir).count(),
        state: "confirmed".into(),
        restorable: true,
        source: "local-history".into(),
        remote_backup: None,
    };
    let archive = Archive {
        version: 1,
        binding: account.clone(),
        request_id: request_id.into(),
        public: public.clone(),
        original: nodes[0].entry.clone(),
        nodes,
    };
    let transaction = connection.transaction()?;
    write_archive(&transaction, &archive)?;
    transaction.execute("INSERT INTO aliases(binding,path,item_id) VALUES(?1,?2,?3) ON CONFLICT(binding,path) DO UPDATE SET item_id=excluded.item_id",params![account,path,public.item_id])?;
    transaction.commit()?;
    Ok(public)
}

mod restore;
pub use restore::{recycle, recycle_checked, restore};

#[cfg(test)]
mod tests;
