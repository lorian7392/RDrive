//! Durable ranged downloads. Checkpoints record only fsynced, hashed prefixes;
//! bytes written after the last checkpoint are discarded safely after a crash.
use crate::{
    backend::{self, Backend, Entry},
    state::{self, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadCheckpoint {
    pub version: u32,
    pub id: String,
    pub profile: String,
    pub identity: String,
    pub remote: String,
    pub destination: PathBuf,
    pub directory: PathBuf,
    pub expected: Entry,
    pub bytes: u64,
    pub sha256: String,
    pub overwrite: bool,
    pub original_sha256: Option<String>,
    pub phase: String,
}

#[derive(Debug, Serialize)]
pub struct DownloadReport {
    pub entry: Entry,
    pub resumed_from: u64,
    pub transferred: u64,
    pub sha256: String,
    pub previous_local: Option<PathBuf>,
}

struct Prefix {
    hasher: Sha256,
    bytes: u64,
}
impl Prefix {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            bytes: 0,
        }
    }
    fn extend(&mut self, file: &mut File, length: u64) -> Result<()> {
        if length < self.bytes {
            bail!("partial file shrank during transfer");
        }
        file.seek(SeekFrom::Start(self.bytes))?;
        let mut remaining = length - self.bytes;
        let mut buf = [0u8; 128 * 1024];
        while remaining > 0 {
            let take = remaining.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..take])
                .context("partial file changed while verifying checkpoint")?;
            self.hasher.update(&buf[..take]);
            remaining -= take as u64;
        }
        self.bytes = length;
        Ok(())
    }
    fn digest(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }
}
fn regular(path: &Path, write: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = options.open(path)?;
    if !f.metadata()?.is_file() {
        bail!("expected a regular partial file");
    }
    Ok(f)
}
fn checkpoint_path(store: &Store, id: &str) -> PathBuf {
    store.home.join("transfers").join(format!("{id}.json"))
}
fn private_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("partial directory is not a real directory");
    }
    #[cfg(unix)]
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        bail!("partial directory must be private and owned by current user");
    }
    Ok(())
}
fn canonical_destination(destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .context("download needs a file destination")?;
    if name.to_str().is_none() {
        bail!("download filename must be UTF-8");
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(name))
}
fn validate(checkpoint: &DownloadCheckpoint) -> Result<()> {
    if checkpoint.version != 1
        || checkpoint.id.len() != 64
        || !checkpoint.id.bytes().all(|b| b.is_ascii_hexdigit())
        || checkpoint.expected.is_dir
        || checkpoint.expected.path != checkpoint.remote
        || backend::normalize(&checkpoint.remote)? != checkpoint.remote
        || checkpoint.bytes > checkpoint.expected.size
        || !checkpoint.destination.is_absolute()
    {
        bail!("invalid download checkpoint");
    }
    let dir_name = checkpoint
        .directory
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid partial directory")?;
    let uuid = dir_name
        .strip_prefix(".rdrive-partial-")
        .context("invalid partial directory")?;
    uuid::Uuid::parse_str(uuid).context("invalid partial directory UUID")?;
    if checkpoint.directory.parent() != checkpoint.destination.parent()
        || !["downloading", "publishing", "complete"].contains(&checkpoint.phase.as_str())
    {
        bail!("download checkpoint points outside its destination directory");
    }
    private_directory(&checkpoint.directory)?;
    Ok(())
}
fn save_progress(
    path: &Path,
    checkpoint: &mut DownloadCheckpoint,
    prefix: &mut Prefix,
) -> Result<()> {
    let mut file = regular(&checkpoint.directory.join("partial"), false)?;
    let length = file.metadata()?.len();
    if length > checkpoint.expected.size {
        bail!("partial file exceeds the expected remote size");
    }
    prefix.extend(&mut file, length)?;
    file.sync_all()?;
    checkpoint.bytes = prefix.bytes;
    checkpoint.sha256 = prefix.digest();
    state::atomic_json(path, checkpoint)
}
fn local_hash(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            bail!("local destination is a symlink or non-file")
        }
        Ok(_) => Ok(Some(state::fingerprint(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn commit(path: &Path, cp: &mut DownloadCheckpoint) -> Result<Option<PathBuf>> {
    commit_with(path, cp, &LocalPublication)
}

trait Publication {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn link(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn fingerprint(&self, path: &Path) -> Result<String>;
}
struct LocalPublication;
impl Publication for LocalPublication {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> { fs::rename(from, to) }
    fn link(&self, from: &Path, to: &Path) -> std::io::Result<()> { fs::hard_link(from, to) }
    fn fingerprint(&self, path: &Path) -> Result<String> { state::fingerprint(path) }
}
fn commit_with(path: &Path, cp: &mut DownloadCheckpoint, filesystem: &dyn Publication) -> Result<Option<PathBuf>> {
    let partial = cp.directory.join("partial");
    let previous = cp.directory.join("previous");
    cp.phase = "publishing".into();
    state::atomic_json(path, cp)?;
    let current = local_hash(&cp.destination)?;
    if current.as_deref() == Some(cp.sha256.as_str()) {
        // The prior process may have linked the completed file and then died.
        return Ok(previous.exists().then_some(previous));
    }
    if !partial.is_file() || filesystem.fingerprint(&partial)? != cp.sha256 {
        bail!("completed partial is missing or corrupt; local destination was preserved");
    }
    if let Some(expected) = &cp.original_sha256 {
        if previous.exists() {
            if filesystem.fingerprint(&previous)? != *expected || current.is_some() {
                bail!("destination changed during interrupted commit; prior local contents retained at {}",previous.display());
            }
        } else {
            if current.as_ref() != Some(expected) {
                bail!("local destination changed since download started; partial data retained");
            }
            state::atomic_json(&cp.directory.join("RECOVERY.json"), cp)?;
            filesystem.rename(&cp.destination, &previous)?;
            File::open(cp.destination.parent().context("missing parent")?)?.sync_all()?;
            File::open(&cp.directory)?.sync_all()?;
            if filesystem.fingerprint(&previous)? != *expected {
                bail!(
                    "local destination changed during commit; contents retained at {}",
                    previous.display()
                );
            }
        }
    } else if current.is_some() {
        bail!("local destination appeared during download; it was preserved");
    }
    File::open(&partial)?.sync_all()?;
    filesystem.link(&partial, &cp.destination).context(
        "destination appeared during commit; completed partial and any prior local copy retained",
    )?;
    File::open(cp.destination.parent().context("missing parent")?)?.sync_all()?;
    Ok(previous.exists().then_some(previous))
}
fn remove_checkpoint(store: &Store, cp: &DownloadCheckpoint) -> Result<()> {
    let path = checkpoint_path(store, &cp.id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    File::open(store.home.join("transfers"))?.sync_all()?;
    let partial = cp.directory.join("partial");
    if partial.exists() {
        fs::remove_file(partial)?;
    }
    if !cp.directory.join("previous").exists() {
        let recovery = cp.directory.join("RECOVERY.json");
        if recovery.exists() {
            fs::remove_file(recovery)?;
        }
        fs::remove_dir(&cp.directory)?;
    }
    Ok(())
}

pub async fn get(
    store: &mut Store,
    backend: &dyn Backend,
    profile: &str,
    remote: &str,
    destination: &Path,
    overwrite: bool,
) -> Result<DownloadReport> {
    let remote = backend::normalize(remote)?;
    let destination = canonical_destination(destination)?;
    let p = store
        .config
        .profiles
        .get(profile)
        .context("unknown profile")?;
    let identity = state::key(&format!(
        "{profile}\0{}\0{}",
        p.url,
        p.username.as_deref().unwrap_or("")
    ));
    let id = state::key(&format!("{identity}\0{remote}\0{}", destination.display()));
    let path = checkpoint_path(store, &id);
    let mut checkpoint: DownloadCheckpoint = if path.exists() {
        let cp: DownloadCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
        validate(&cp)?;
        if cp.id != id
            || cp.identity != identity
            || cp.profile != profile
            || cp.remote != remote
            || cp.destination != destination
            || cp.overwrite != overwrite
        {
            bail!("resume arguments differ from the saved transfer; use the original flags or discard its checkpoint");
        }
        cp
    } else {
        let original_sha256 = local_hash(&destination)?;
        if original_sha256.is_some() && !overwrite {
            bail!("destination exists; use --overwrite to replace it");
        }
        let expected = backend
            .stat(&remote)
            .await?
            .context("remote file does not exist")?;
        if expected.is_dir || expected.etag.as_ref().is_none_or(|e| e.starts_with("W/")) {
            bail!("resume requires a file with a strong server revision");
        }
        let directory = destination
            .parent()
            .context("destination has no parent")?
            .join(format!(".rdrive-partial-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(directory.join("partial"))?.sync_all()?;
        File::open(&directory)?.sync_all()?;
        let cp = DownloadCheckpoint {
            version: 1,
            id,
            profile: profile.into(),
            identity,
            remote: remote.clone(),
            destination,
            directory,
            expected,
            bytes: 0,
            sha256: format!("{:x}", Sha256::digest([])),
            overwrite,
            original_sha256,
            phase: "downloading".into(),
        };
        state::atomic_json(&path, &cp)?;
        cp
    };
    let resumed_from = checkpoint.bytes;
    let partial = checkpoint.directory.join("partial");
    let mut prefix = Prefix::new();
    if checkpoint.phase == "downloading" {
        let mut f = regular(&partial, true)?;
        if f.metadata()?.len() < checkpoint.bytes {
            bail!("partial file is shorter than its durable checkpoint");
        }
        prefix.extend(&mut f, checkpoint.bytes)?;
        if prefix.digest() != checkpoint.sha256 {
            bail!("partial file checksum mismatch; no remote data was requested");
        }
        f.set_len(checkpoint.bytes)?;
        f.sync_all()?;
        let current = backend
            .stat(&remote)
            .await?
            .context("remote file disappeared")?;
        if !current.same_revision(&checkpoint.expected) {
            bail!("remote revision changed; saved partial was preserved; discard checkpoint to restart");
        }
        let expected = checkpoint.expected.clone();
        let operation = backend.download_resume(&remote, &partial, &expected, checkpoint.bytes);
        tokio::pin!(operation);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                result=&mut operation=>break result,
                _=interval.tick()=>save_progress(&path,&mut checkpoint,&mut prefix)?,
            }
        };
        save_progress(&path, &mut checkpoint, &mut prefix)?;
        result.with_context(|| {
            format!(
                "partial download retained as transfer {}; rerun get --resume",
                checkpoint.id
            )
        })?;
        if checkpoint.bytes != checkpoint.expected.size {
            bail!("download did not reach expected length; checkpoint retained");
        }
        let current = backend
            .stat(&remote)
            .await?
            .context("remote file disappeared after download")?;
        if !current.same_revision(&checkpoint.expected) {
            bail!("remote revision changed after transfer; destination preserved");
        }
    } else {
        if checkpoint.bytes != checkpoint.expected.size {
            bail!("incomplete publishing checkpoint");
        }
        if partial.exists() && state::fingerprint(&partial)? != checkpoint.sha256 {
            bail!("completed partial checksum mismatch");
        }
    }
    if backend
        .content_sha256(&checkpoint.expected)
        .as_ref()
        .is_some_and(|hash| hash != &checkpoint.sha256)
    {
        bail!(
            "downloaded bytes do not match the server content fingerprint; destination preserved"
        );
    }
    let previous_local = commit(&path, &mut checkpoint)?;
    checkpoint.phase = "complete".into();
    state::atomic_json(&path, &checkpoint)?;
    let report = DownloadReport {
        entry: checkpoint.expected.clone(),
        resumed_from,
        transferred: checkpoint.bytes.saturating_sub(resumed_from),
        sha256: checkpoint.sha256.clone(),
        previous_local,
    };
    store.event(
        "download-resume",
        profile,
        &remote,
        &format!(
            "resumed from {resumed_from}; previous local: {:?}",
            report.previous_local
        ),
    )?;
    remove_checkpoint(store, &checkpoint)?;
    Ok(report)
}

pub fn list(store: &Store) -> Result<Vec<serde_json::Value>> {
    let mut results = Vec::new();
    for entry in fs::read_dir(store.home.join("transfers"))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let cp: DownloadCheckpoint = serde_json::from_slice(&fs::read(entry.path())?)?;
        validate(&cp)?;
        results.push(serde_json::json!({"direction":"download","id":cp.id,"profile":cp.profile,"remote":cp.remote,"destination":cp.destination,"bytes":cp.bytes,"size":cp.expected.size,"phase":cp.phase}));
    }
    for job in &store.db.jobs {
        if job.resume {
            let p = store
                .home
                .join("spool")
                .join(format!("{}.upload.json", job.id));
            let progress = if p
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() <= 4 * 1024 * 1024)
            {
                let value: serde_json::Value = serde_json::from_slice(&fs::read(&p)?)?;
                let confirmed = value
                    .get("confirmed_bytes")
                    .and_then(|v| v.as_u64())
                    .or_else(|| {
                        value
                            .get("chunks")
                            .and_then(|v| v.as_object())
                            .map(|chunks| {
                                chunks
                                    .values()
                                    .filter(|v| v.get("etag").is_some_and(|e| e.is_string()))
                                    .filter_map(|v| v.get("size").and_then(|v| v.as_u64()))
                                    .sum::<u64>()
                            })
                    });
                serde_json::json!({"phase":value.get("phase"),"confirmed_bytes":confirmed})
            } else {
                serde_json::Value::Null
            };
            results.push(serde_json::json!({"direction":"upload","id":job.id,"profile":job.profile,"remote":job.remote,"size":job.size,"status":job.status,"progress":progress,"checkpoint":p.exists().then_some(p)}));
        }
    }
    Ok(results)
}
pub fn discard(store: &Store, id: &str) -> Result<Option<PathBuf>> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid download transfer ID; use queue cancel for uploads");
    }
    let path = checkpoint_path(store, id);
    let cp: DownloadCheckpoint = serde_json::from_slice(&fs::read(&path)?)?;
    validate(&cp)?;
    if cp.id != id {
        bail!("checkpoint identity mismatch");
    }
    let previous = cp.directory.join("previous");
    let backup = previous.exists().then_some(previous);
    remove_checkpoint(store, &cp)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{Capabilities, WriteCondition},
        config::{Profile, Protocol},
    };
    use async_trait::async_trait;
    use std::{
        io::Write,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };

    struct RangeServer {
        bytes: Mutex<Vec<u8>>,
        offsets: Mutex<Vec<u64>>,
        fail: AtomicBool,
        pause: AtomicBool,
        weak: AtomicBool,
        wrong_hash: AtomicBool,
        short: AtomicBool,
        change_after: AtomicBool,
    }
    impl RangeServer {
        fn new() -> Self {
            Self {
                bytes: Mutex::new((0..=255).cycle().take(512 * 1024).collect()),
                offsets: Mutex::new(Vec::new()),
                fail: AtomicBool::new(true),
                pause: AtomicBool::new(false),
                weak: AtomicBool::new(false),
                wrong_hash: AtomicBool::new(false),
                short: AtomicBool::new(false),
                change_after: AtomicBool::new(false),
            }
        }
        fn entry(&self) -> Entry {
            let b = self.bytes.lock().unwrap();
            Entry {
                path: "file".into(),
                is_dir: false,
                size: b.len() as u64,
                modified: Some(1),
                etag: Some(if self.weak.load(Ordering::SeqCst) { "W/revision".into() } else { format!("\"{:x}\"", Sha256::digest(&*b)) }),
            }
        }
    }
    #[async_trait]
    impl Backend for RangeServer {
        fn content_sha256(&self, _: &Entry) -> Option<String> {
            self.wrong_hash.load(Ordering::SeqCst).then(|| "wrong server fingerprint".into())
        }
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
            Ok((path == "file").then(|| self.entry()))
        }
        async fn list(&self, _: &str) -> Result<Vec<Entry>> {
            bail!("unused")
        }
        async fn download(&self, _: &str, _: &Path) -> Result<()> {
            bail!("unused")
        }
        async fn download_resume(
            &self,
            _: &str,
            path: &Path,
            expected: &Entry,
            offset: u64,
        ) -> Result<()> {
            if !self.entry().same_revision(expected) {
                bail!("changed");
            }
            self.offsets.lock().unwrap().push(offset);
            let bytes = self.bytes.lock().unwrap().clone();
            let halfway = bytes.len() / 2;
            let mut f = OpenOptions::new().append(true).open(path)?;
            if offset == 0 {
                f.write_all(&bytes[..halfway])?;
                f.sync_all()?;
                if self.pause.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
                if self.fail.swap(false, Ordering::SeqCst) {
                    bail!("simulated connection loss");
                }
                if self.short.load(Ordering::SeqCst) { return Ok(()); }
                f.write_all(&bytes[halfway..])?;
            } else {
                f.write_all(&bytes[offset as usize..])?;
            }
            f.sync_all()?;
            if self.change_after.load(Ordering::SeqCst) { self.bytes.lock().unwrap()[0] ^= 1; }
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
    fn store(root: &Path) -> Store {
        let mut s = Store::open(root.join("state")).unwrap();
        s.config.profiles.insert(
            "dav".into(),
            Profile {
                protocol: Protocol::Webdav,
                url: "https://example.test/root/".into(),
                username: None,
                password_env: None,
                allow_http: false,
                timeout_secs: 10,
                smb_encryption: true,
                nextcloud_upload_url: None,
            },
        );
        s.save_config().unwrap();
        s
    }
    fn saved(store: &Store) -> (PathBuf, DownloadCheckpoint) {
        let path = fs::read_dir(store.home.join("transfers"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let cp = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        (path, cp)
    }
    fn checkpoint(root: &Path) -> DownloadCheckpoint {
        use std::os::unix::fs::PermissionsExt;
        let directory = root.join(format!(".rdrive-partial-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.join("partial"), b"new").unwrap();
        DownloadCheckpoint { version: 1, id: "a".repeat(64), profile: "dav".into(), identity: "identity".into(), remote: "file".into(), destination: root.join("destination"), directory, expected: Entry { path: "file".into(), is_dir: false, size: 3, modified: Some(1), etag: Some("revision".into()) }, bytes: 3, sha256: format!("{:x}", Sha256::digest(b"new")), overwrite: true, original_sha256: None, phase: "downloading".into() }
    }

    #[test]
    fn invalid_checkpoint_paths_permissions_and_prefix_lengths_fail_closed() {
        use std::os::unix::{ffi::OsStringExt, fs::{PermissionsExt, symlink}};
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let good = checkpoint(&root); validate(&good).unwrap();
        let mut bad = good.clone(); bad.version = 2; assert!(validate(&bad).is_err());
        let mut bad = good.clone(); bad.directory = root.join("unexpected"); assert!(validate(&bad).is_err());
        let mut bad = good.clone(); bad.directory = root.join(".rdrive-partial-not-a-uuid"); assert!(validate(&bad).is_err());
        let mut bad = good.clone(); bad.destination = root.join("other").join("destination"); assert!(validate(&bad).is_err());
        let mut bad = good.clone(); bad.directory = PathBuf::from("/"); assert!(validate(&bad).is_err());
        fs::set_permissions(&good.directory, fs::Permissions::from_mode(0o755)).unwrap(); assert!(validate(&good).is_err());
        fs::set_permissions(&good.directory, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.join("alias"); symlink(&good.directory, &link).unwrap(); assert!(private_directory(&link).is_err());
        assert!(private_directory(&good.directory.join("partial")).is_err());
        assert!(regular(&good.directory, false).is_err());
        assert!(local_hash(&good.directory).is_err());
        assert!(local_hash(&good.directory.join("partial/child")).is_err());
        assert!(canonical_destination(Path::new("/")).is_err());
        let invalid = root.join(std::ffi::OsString::from_vec(vec![255])); assert!(canonical_destination(&invalid).is_err());
        let mut prefix = Prefix::new(); let mut file = regular(&good.directory.join("partial"), false).unwrap();
        prefix.extend(&mut file, 3).unwrap(); assert!(prefix.extend(&mut file, 1).is_err());
        let mut prefix = Prefix::new(); assert!(prefix.extend(&mut file, 4).is_err());
        let mut oversized = good.clone(); oversized.expected.size = 2;
        assert!(save_progress(&root.join("checkpoint.json"), &mut oversized, &mut Prefix::new()).is_err());
    }

    #[test]
    fn interrupted_publication_preserves_destinations_and_retains_prior_recovery() {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let mut cp = checkpoint(&root); let path = root.join("receipt.json");
        fs::write(&cp.destination, b"appeared").unwrap();
        assert!(commit(&path, &mut cp).unwrap_err().to_string().contains("appeared"));
        assert_eq!(fs::read(&cp.destination).unwrap(), b"appeared");
        fs::remove_file(&cp.destination).unwrap();
        fs::write(cp.directory.join("partial"), b"corrupt").unwrap();
        assert!(commit(&path, &mut cp).unwrap_err().to_string().contains("corrupt"));
        fs::write(cp.directory.join("partial"), b"new").unwrap();
        cp.original_sha256 = Some(format!("{:x}", Sha256::digest(b"old")));
        fs::write(cp.directory.join("previous"), b"wrong prior copy").unwrap();
        assert!(commit(&path, &mut cp).unwrap_err().to_string().contains("prior local"));
        fs::write(cp.directory.join("previous"), b"old").unwrap();
        let previous = commit(&path, &mut cp).unwrap().unwrap();
        assert_eq!(fs::read(previous).unwrap(), b"old");
        assert_eq!(fs::read(&cp.destination).unwrap(), b"new");
        assert!(commit(&path, &mut cp).unwrap().is_some(), "commit replay must accept its own published bytes");
    }

    struct RacingPublication { change_after_rename: bool, create_before_link: bool }
    impl Publication for RacingPublication {
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            LocalPublication.rename(from, to)?;
            if self.change_after_rename { fs::write(to, b"concurrent local write")?; }
            Ok(())
        }
        fn link(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            if self.create_before_link { fs::write(to, b"appeared at link time")?; }
            LocalPublication.link(from, to)
        }
        fn fingerprint(&self, path: &Path) -> Result<String> { LocalPublication.fingerprint(path) }
    }
    #[test]
    fn publication_races_and_cleanup_errors_keep_all_recovery_bytes() {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let mut cp = checkpoint(&root); let path = root.join("receipt.json");
        fs::write(&cp.destination, b"old").unwrap(); cp.original_sha256 = Some(format!("{:x}", Sha256::digest(b"old")));
        let error = commit_with(&path, &mut cp, &RacingPublication { change_after_rename: true, create_before_link: false }).unwrap_err();
        assert!(error.to_string().contains("changed during commit"));
        assert_eq!(fs::read(cp.directory.join("previous")).unwrap(), b"concurrent local write");
        assert_eq!(fs::read(cp.directory.join("partial")).unwrap(), b"new");
        assert!(!cp.destination.exists());
        let mut cp = checkpoint(&root);
        assert!(commit_with(&path, &mut cp, &RacingPublication { change_after_rename: false, create_before_link: true }).unwrap_err().to_string().contains("appeared during commit"));
        assert_eq!(fs::read(&cp.destination).unwrap(), b"appeared at link time");
        assert_eq!(fs::read(cp.directory.join("partial")).unwrap(), b"new");
        let s = store(&root); let checkpoint_path = checkpoint_path(&s, &cp.id);
        fs::create_dir(&checkpoint_path).unwrap();
        assert!(remove_checkpoint(&s, &cp).is_err());
        fs::remove_dir(checkpoint_path).unwrap();
        fs::remove_file(cp.directory.join("partial")).unwrap(); fs::create_dir(cp.directory.join("partial")).unwrap();
        assert!(remove_checkpoint(&s, &cp).is_err());
        assert!(cp.directory.exists());
        fs::remove_dir(cp.directory.join("partial")).unwrap();
        remove_checkpoint(&s, &cp).unwrap();
        assert!(!cp.directory.exists(), "cleanup must recover when the partial was already removed");
    }

    #[test]
    fn upload_checkpoints_report_confirmed_chunks_and_missing_receipts_without_writes() {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let mut s = store(&root); let source = root.join("source"); fs::write(&source, b"payload").unwrap();
        let job = crate::operations::enqueue_with_resume(&mut s, "dav", &source, "remote", false, None, false, true).unwrap();
        let path = s.home.join("spool").join(format!("{}.upload.json", job.id));
        assert!(list(&s).unwrap()[0]["progress"].is_null());
        state::atomic_json(&path, &serde_json::json!({"phase":"uploading","confirmed_bytes":4})).unwrap();
        assert_eq!(list(&s).unwrap()[0]["progress"]["confirmed_bytes"], 4);
        state::atomic_json(&path, &serde_json::json!({"phase":"chunks","chunks":{"one":{"etag":"ok","size":3},"two":{"etag":"ok","size":4},"pending":{"size":50},"invalid":{"etag":true,"size":100}}})).unwrap();
        assert_eq!(list(&s).unwrap()[0]["progress"]["confirmed_bytes"], 7);
        state::atomic_json(&path, &serde_json::json!({"phase":"starting"})).unwrap();
        assert!(list(&s).unwrap()[0]["progress"]["confirmed_bytes"].is_null());
        fs::write(s.home.join("transfers").join("unrelated.txt"), b"not a checkpoint").unwrap();
        fs::create_dir(s.home.join("transfers").join("directory.json")).unwrap();
        assert_eq!(list(&s).unwrap().len(), 1);
        assert!(discard(&s, "not-an-id").is_err());
        let mut cp = checkpoint(&root); cp.id = "b".repeat(64);
        let forged_path = checkpoint_path(&s, &"a".repeat(64)); state::atomic_json(&forged_path, &cp).unwrap();
        assert!(discard(&s, &"a".repeat(64)).unwrap_err().to_string().contains("identity mismatch"));
        fs::remove_file(forged_path).unwrap();
        cp.id = "c".repeat(64); let cp_path = checkpoint_path(&s, &cp.id); state::atomic_json(&cp_path, &cp).unwrap();
        state::atomic_json(&cp.directory.join("RECOVERY.json"), &cp).unwrap();
        assert!(discard(&s, &cp.id).unwrap().is_none());
        assert!(!cp.directory.exists());
    }

    #[tokio::test]
    async fn resuming_refuses_changed_flags_short_partial_and_incomplete_publication() {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let server = RangeServer::new(); let destination = root.join("out"); let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &destination, false).await.is_err());
        assert!(get(&mut s, &server, "dav", "file", &destination, true).await.unwrap_err().to_string().contains("arguments differ"));
        let (path, mut cp) = saved(&s);
        fs::write(cp.directory.join("partial"), b"short").unwrap();
        assert!(get(&mut s, &server, "dav", "file", &destination, false).await.unwrap_err().to_string().contains("shorter"));
        cp.phase = "publishing".into(); state::atomic_json(&path, &cp).unwrap();
        assert!(get(&mut s, &server, "dav", "file", &destination, false).await.unwrap_err().to_string().contains("incomplete publishing"));
        cp.bytes = cp.expected.size; state::atomic_json(&path, &cp).unwrap();
        assert!(get(&mut s, &server, "dav", "file", &destination, false).await.unwrap_err().to_string().contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn download_refuses_weak_revisions_short_success_and_changed_or_misidentified_content() {
        for case in ["existing", "weak", "short", "changed", "checksum"] {
            let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
            let server = RangeServer::new(); server.fail.store(false, Ordering::SeqCst);
            let destination = root.join("out"); let mut s = store(&root);
            let expected = match case {
                "existing" => { fs::write(&destination, b"preserve").unwrap(); "destination exists" },
                "weak" => { server.weak.store(true, Ordering::SeqCst); "strong server revision" },
                "short" => { server.short.store(true, Ordering::SeqCst); "expected length" },
                "changed" => { server.change_after.store(true, Ordering::SeqCst); "revision changed after" },
                "checksum" => { server.wrong_hash.store(true, Ordering::SeqCst); "server content fingerprint" },
                _ => unreachable!(),
            };
            let error = get(&mut s, &server, "dav", "file", &destination, false).await.unwrap_err();
            assert!(error.to_string().contains(expected), "case {case}: {error:#}");
            if case == "existing" { assert_eq!(fs::read(&destination).unwrap(), b"preserve"); } else { assert!(!destination.exists()); }
        }
    }

    #[tokio::test]
    async fn failed_postpublication_history_write_retains_a_replayable_complete_checkpoint() {
        let temp = tempfile::tempdir().unwrap(); let root = temp.path().canonicalize().unwrap();
        let mut s = store(&root); let destination = root.join("out"); let server = RangeServer::new();
        server.fail.store(false, Ordering::SeqCst);
        let state = s.home.join("state.json"); if state.exists() { fs::remove_file(&state).unwrap(); } fs::create_dir(&state).unwrap();
        assert!(get(&mut s, &server, "dav", "file", &destination, false).await.is_err());
        assert_eq!(fs::read(&destination).unwrap(), *server.bytes.lock().unwrap());
        let (_, saved) = saved(&s); assert_eq!(saved.phase, "complete");
        fs::remove_dir(&state).unwrap();
        let report = get(&mut s, &server, "dav", "file", &destination, false).await.unwrap();
        assert_eq!(report.transferred, 0); assert_eq!(server.offsets.lock().unwrap().len(), 1);
        assert!(list(&s).unwrap().is_empty());
    }
    #[tokio::test]
    async fn interruption_restart_resumes_exact_verified_prefix_and_commits() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = RangeServer::new();
        let dest = root.join("out");
        let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .is_err());
        let (_, cp) = saved(&s);
        assert_eq!(cp.bytes, 256 * 1024);
        assert!(!dest.exists());
        drop(s);
        let mut s = Store::open(root.join("state")).unwrap();
        let report = get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .unwrap();
        assert_eq!(report.resumed_from, 256 * 1024);
        assert_eq!(*server.offsets.lock().unwrap(), vec![0, 256 * 1024]);
        assert_eq!(fs::read(dest).unwrap(), *server.bytes.lock().unwrap());
        assert!(list(&s).unwrap().is_empty());
    }
    #[tokio::test]
    async fn changed_remote_and_corrupt_partial_are_never_appended() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = RangeServer::new();
        let dest = root.join("out");
        let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .is_err());
        server.bytes.lock().unwrap()[0] ^= 1;
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("revision changed"));
        assert_eq!(server.offsets.lock().unwrap().len(), 1);
        server.bytes.lock().unwrap()[0] ^= 1;
        let (_, cp) = saved(&s);
        let partial = cp.directory.join("partial");
        let mut f = OpenOptions::new().write(true).open(&partial).unwrap();
        f.write_all(b"corrupt").unwrap();
        f.sync_all().unwrap();
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("checksum"));
        assert!(!dest.exists());
        assert_eq!(server.offsets.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn unsaved_crash_tail_is_truncated_before_resuming() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = RangeServer::new();
        let dest = root.join("out");
        let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .is_err());
        let (_, cp) = saved(&s);
        let mut f = OpenOptions::new()
            .append(true)
            .open(cp.directory.join("partial"))
            .unwrap();
        f.write_all(b"unsaved tail").unwrap();
        f.sync_all().unwrap();
        get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .unwrap();
        assert_eq!(fs::read(dest).unwrap(), *server.bytes.lock().unwrap());
    }
    #[tokio::test]
    async fn local_edit_is_preserved_and_successful_overwrite_has_recovery_copy() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = RangeServer::new();
        let dest = root.join("out");
        fs::write(&dest, b"before").unwrap();
        let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &dest, true)
            .await
            .is_err());
        fs::write(&dest, b"local edit").unwrap();
        assert!(get(&mut s, &server, "dav", "file", &dest, true)
            .await
            .is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"local edit");
        let (_, cp) = saved(&s);
        discard(&s, &cp.id).unwrap();
        let report = get(&mut s, &server, "dav", "file", &dest, true)
            .await
            .unwrap();
        assert_eq!(
            fs::read(report.previous_local.unwrap()).unwrap(),
            b"local edit"
        );
    }
    #[tokio::test]
    async fn completed_commit_receipt_recovers_without_retransfer() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = RangeServer::new();
        let dest = root.join("out");
        let mut s = store(&root);
        assert!(get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .is_err());
        let (path, mut cp) = saved(&s);
        let bytes = server.bytes.lock().unwrap().clone();
        fs::write(cp.directory.join("partial"), &bytes).unwrap();
        cp.bytes = bytes.len() as u64;
        cp.sha256 = format!("{:x}", Sha256::digest(&bytes));
        cp.phase = "publishing".into();
        state::atomic_json(&path, &cp).unwrap();
        fs::hard_link(cp.directory.join("partial"), &dest).unwrap();
        let report = get(&mut s, &server, "dav", "file", &dest, false)
            .await
            .unwrap();
        assert_eq!(report.transferred, 0);
        assert_eq!(server.offsets.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn periodic_checkpoint_survives_task_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let server = Arc::new(RangeServer::new());
        server.pause.store(true, Ordering::SeqCst);
        server.fail.store(false, Ordering::SeqCst);
        drop(store(&root));
        let dest = root.join("out");
        let task_root = root.clone();
        let task_dest = dest.clone();
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            let mut s = Store::open(task_root.join("state")).unwrap();
            get(
                &mut s,
                task_server.as_ref(),
                "dav",
                "file",
                &task_dest,
                false,
            )
            .await
        });
        let mut bytes = 0;
        for _ in 0..200 {
            if let Some(entry) = fs::read_dir(root.join("state/transfers")).unwrap().next() {
                if let Ok(data) = fs::read(entry.unwrap().path()) {
                    if let Ok(cp) = serde_json::from_slice::<DownloadCheckpoint>(&data) {
                        bytes = cp.bytes;
                    }
                }
            }
            if bytes > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(bytes, 256 * 1024);
        task.abort();
        let _ = task.await;
        server.pause.store(false, Ordering::SeqCst);
        let mut s = Store::open(root.join("state")).unwrap();
        let report = get(&mut s, server.as_ref(), "dav", "file", &dest, false)
            .await
            .unwrap();
        assert_eq!(report.resumed_from, bytes);
    }
}
