use crate::{
    backend::{Entry, WriteCondition},
    config::Config,
};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::{
    fs::{OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub fn key(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".rdrive-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn fingerprint(path: &Path) -> Result<String> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW);
    let mut file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("expected a regular file: {}", path.display());
    }
    let mut hash = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub profile: String,
    pub remote: String,
    pub snapshot: String,
    pub sha256: String,
    pub size: u64,
    pub created: String,
    pub status: String,
    pub attempts: u32,
    pub error: Option<String>,
    pub expected: Option<Entry>,
    pub overwrite: bool,
    pub version: bool,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub version_prepared: bool,
}
impl Job {
    pub fn condition(&self) -> WriteCondition {
        if let Some(expected) = &self.expected {
            WriteCondition::Match(expected.clone())
        } else if self.overwrite && !(self.version && self.version_prepared) {
            WriteCondition::Any
        } else {
            WriteCondition::CreateNew
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheItem {
    pub profile: String,
    pub remote: String,
    pub object: String,
    pub entry: Entry,
    pub sha256: String,
    pub pinned: bool,
    pub accessed: String,
    /// Profile/account identity. Legacy unbound payloads need an online refetch
    /// before reuse, rather than trusting ETags across unrelated accounts.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheReservation {
    pub size: u64,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub time: String,
    pub operation: String,
    pub profile: String,
    pub path: String,
    pub detail: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Database {
    pub jobs: Vec<Job>,
    pub cache: BTreeMap<String, CacheItem>,
    #[serde(default)]
    pub cache_reservations: BTreeMap<String, CacheReservation>,
    /// Only durably recorded obsolete clean payloads may be reclaimed.
    #[serde(default)]
    pub cache_garbage: std::collections::BTreeSet<String>,
    pub history: Vec<Event>,
}

pub struct Store {
    pub home: PathBuf,
    pub config: Config,
    pub db: Database,
    _lock: File,
}
impl Store {
    pub fn open(home: PathBuf) -> Result<Self> {
        let is_new = !home.exists();
        fs::create_dir_all(&home)?;
        if fs::symlink_metadata(&home)?.file_type().is_symlink() {
            bail!("state directory cannot be a symlink");
        }
        let home = fs::canonicalize(home)?;
        #[cfg(unix)]
        if is_new {
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(&home)?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                bail!("state directory must be owned by the current user and private (mode 0700); choose a new private subdirectory");
            }
        }
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = opts.open(home.join("client.lock"))?;
        #[cfg(unix)]
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("another rdrive process is using this state directory");
        }
        for name in [
            "cache",
            "spool",
            "tmp",
            "baselines",
            "transfers",
            "transactions",
            "provider",
        ] {
            let path = home.join(name);
            fs::create_dir_all(&path)?;
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                bail!("state subdirectory cannot be a symlink");
            }
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        let config: Config = read_json(&home.join("config.json"))?;
        config.index.validate()?;
        config.filters.validate()?;
        config.policies.validate()?;
        for p in config.profiles.values() {
            crate::config::validate_profile(p)?;
        }
        let mut db: Database = read_json(&home.join("state.json"))?;
        let safe_component = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        let mut ids = std::collections::HashSet::new();
        for job in &mut db.jobs {
            if !safe_component(&job.id)
                || job.snapshot != job.id
                || !ids.insert(job.id.clone())
                || crate::backend::normalize(&job.remote)? != job.remote
                || job.remote.is_empty()
                || ![
                    "pending",
                    "running",
                    "interrupted",
                    "failed",
                    "done",
                    "cancelled",
                ]
                .contains(&job.status.as_str())
            {
                bail!("invalid upload queue state; no job was executed");
            }
            if job.status == "running" {
                job.status = "interrupted".into();
                job.error = Some("previous process ended before confirming completion".into());
            }
        }
        let mut cache_objects = std::collections::HashSet::new();
        for (key, item) in &db.cache {
            if !safe_component(&item.object)
                || !cache_objects.insert(item.object.clone())
                || *key != Self::cache_key(&item.profile, &item.remote)
                || crate::backend::normalize(&item.remote)? != item.remote
                || item.entry.path != item.remote
                || item.entry.is_dir
                || item.sha256.len() != 64
                || !item.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || item.scope.as_ref().is_some_and(|scope| {
                    scope.len() != 64 || !scope.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                bail!("invalid cache state; no cache file was opened");
            }
        }
        for object in db.cache_reservations.keys() {
            if uuid::Uuid::parse_str(object).is_err() || cache_objects.contains(object) {
                bail!("invalid cache reservation state; no cache payload was removed");
            }
        }
        for object in &db.cache_garbage {
            if !safe_component(object) || cache_objects.contains(object) {
                bail!("invalid obsolete cache object state; no cache payload was removed");
            }
        }
        let mut store = Self {
            home,
            config,
            db,
            _lock: lock,
        };
        crate::cache_directory::recover(&mut store)?;
        crate::cache::recover(&mut store)?;
        Ok(store)
    }
    pub fn save_config(&self) -> Result<()> {
        atomic_json(&self.home.join("config.json"), &self.config)
    }
    pub fn save(&self) -> Result<()> {
        atomic_json(&self.home.join("state.json"), &self.db)
    }
    pub fn event(
        &mut self,
        operation: &str,
        profile: &str,
        path: &str,
        detail: &str,
    ) -> Result<()> {
        self.db.history.push(Event {
            time: now(),
            operation: operation.into(),
            profile: profile.into(),
            path: path.into(),
            detail: detail.into(),
        });
        if self.db.history.len() > 1000 {
            self.db.history.drain(..self.db.history.len() - 1000);
        }
        self.save()
    }
    pub fn cache_key(profile: &str, remote: &str) -> String {
        key(&format!("{profile}\0{remote}"))
    }
}

/// Copy a caller file into private storage, then fsync it before advertising it.
pub fn snapshot(source: &Path, dest: &Path) -> Result<(String, u64)> {
    let before = fingerprint(source)?;
    let mut srcopts = OpenOptions::new();
    srcopts.read(true);
    #[cfg(unix)]
    srcopts.custom_flags(libc::O_NOFOLLOW);
    let mut source_file = srcopts.open(source)?;
    let mut dstopts = OpenOptions::new();
    dstopts.write(true).create_new(true);
    #[cfg(unix)]
    dstopts.mode(0o600);
    let mut target = dstopts.open(dest)?;
    let bytes = std::io::copy(&mut source_file, &mut target)?;
    target.sync_all()?;
    File::open(dest.parent().context("snapshot has no parent")?)?.sync_all()?;
    let hash = fingerprint(dest)?;
    if before != hash || fingerprint(source)? != hash {
        let _ = fs::remove_file(dest);
        bail!("local file changed while taking upload snapshot; retry after saving");
    }
    Ok((hash, bytes))
}

/// Commit a completed temporary download without replacing an existing target
/// unless overwrite was explicitly requested. hard_link is atomic no-clobber.
pub fn commit_download(temp: &Path, destination: &Path, overwrite: bool) -> Result<()> {
    File::open(temp)?.sync_all()?;
    if overwrite {
        if destination
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!("refusing to replace a symlink");
        }
        fs::rename(temp, destination)?;
    } else {
        fs::hard_link(temp, destination)
            .context("destination exists or cannot be committed atomically")?;
        fs::remove_file(temp)?;
    }
    File::open(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_and_restart_preserve_snapshot_and_mark_ambiguous_upload() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        fs::write(&input, b"original").unwrap();
        let home = temp.path().join("store");
        let mut store = Store::open(home.clone()).unwrap();
        let snap = store.home.join("spool/test");
        let (sha256, size) = snapshot(&input, &snap).unwrap();
        store.db.jobs.push(Job {
            id: "test".into(),
            profile: "dav".into(),
            remote: "file".into(),
            snapshot: "test".into(),
            sha256,
            size,
            created: now(),
            status: "running".into(),
            attempts: 1,
            error: None,
            expected: None,
            overwrite: false,
            version: false,
            resume: false,
            version_prepared: false,
        });
        store.save().unwrap();
        fs::write(input, b"edited").unwrap();
        drop(store);
        let restored = Store::open(home).unwrap();
        assert_eq!(restored.db.jobs[0].status, "interrupted");
        assert_eq!(fs::read(snap).unwrap(), b"original");
    }
    #[test]
    fn download_does_not_clobber_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("temp");
        let dst = dir.path().join("dst");
        fs::write(&temp, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        assert!(commit_download(&temp, &dst, false).is_err());
        assert_eq!(fs::read(dst).unwrap(), b"old");
    }
    #[test]
    fn store_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let _one = Store::open(dir.path().join("state")).unwrap();
        assert!(Store::open(dir.path().join("state")).is_err());
    }
}
