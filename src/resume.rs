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
    let partial = cp.directory.join("partial");
    let previous = cp.directory.join("previous");
    cp.phase = "publishing".into();
    state::atomic_json(path, cp)?;
    let current = local_hash(&cp.destination)?;
    if current.as_deref() == Some(cp.sha256.as_str()) {
        // The prior process may have linked the completed file and then died.
        return Ok(previous.exists().then_some(previous));
    }
    if !partial.is_file() || state::fingerprint(&partial)? != cp.sha256 {
        bail!("completed partial is missing or corrupt; local destination was preserved");
    }
    if let Some(expected) = &cp.original_sha256 {
        if previous.exists() {
            if state::fingerprint(&previous)? != *expected || current.is_some() {
                bail!("destination changed during interrupted commit; prior local contents retained at {}",previous.display());
            }
        } else {
            if current.as_ref() != Some(expected) {
                bail!("local destination changed since download started; partial data retained");
            }
            state::atomic_json(&cp.directory.join("RECOVERY.json"), cp)?;
            fs::rename(&cp.destination, &previous)?;
            File::open(cp.destination.parent().context("missing parent")?)?.sync_all()?;
            File::open(&cp.directory)?.sync_all()?;
            if state::fingerprint(&previous)? != *expected {
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
    fs::hard_link(&partial, &cp.destination).context(
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
    }
    impl RangeServer {
        fn new() -> Self {
            Self {
                bytes: Mutex::new((0..=255).cycle().take(512 * 1024).collect()),
                offsets: Mutex::new(Vec::new()),
                fail: AtomicBool::new(true),
                pause: AtomicBool::new(false),
            }
        }
        fn entry(&self) -> Entry {
            let b = self.bytes.lock().unwrap();
            Entry {
                path: "file".into(),
                is_dir: false,
                size: b.len() as u64,
                modified: Some(1),
                etag: Some(format!("\"{:x}\"", Sha256::digest(&*b))),
            }
        }
    }
    #[async_trait]
    impl Backend for RangeServer {
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
                f.write_all(&bytes[halfway..])?;
            } else {
                f.write_all(&bytes[offset as usize..])?;
            }
            f.sync_all()?;
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
