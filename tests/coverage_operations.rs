//! Observable guarded-download, queue and version failures against a deterministic
//! in-memory protocol peer. Filesystem publication still uses real temporary files.
use anyhow::{bail, Result};
use async_trait::async_trait;
use rdrive::{backend::{Backend, Capabilities, Entry, WriteCondition}, config::{Profile, Protocol}, operations, state::Store};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, fs, path::Path, sync::Mutex};

#[derive(Default)]
struct Peer {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    dirs: Mutex<BTreeSet<String>>,
    sequence: Mutex<VecDeque<Option<Entry>>>,
    download_override: Mutex<Option<Vec<u8>>>,
    checksum: Mutex<Option<String>>,
    corrupt_uploads: Mutex<bool>,
}
impl Peer {
    fn insert(&self, path: &str, bytes: &[u8]) { self.files.lock().unwrap().insert(path.into(), bytes.into()); }
    fn entry(&self, path: &str) -> Option<Entry> {
        if let Some(bytes) = self.files.lock().unwrap().get(path) { return Some(Entry { path: path.into(), is_dir: false, size: bytes.len() as u64, modified: Some(1), etag: Some(format!("{:x}", Sha256::digest(bytes))) }); }
        self.dirs.lock().unwrap().contains(path).then(|| Entry { path: path.into(), is_dir: true, size: 0, modified: Some(1), etag: Some("directory".into()) })
    }
    fn source() -> Self { let peer = Self::default(); peer.insert("file", b"abc"); peer }
}
#[async_trait]
impl Backend for Peer {
    fn capabilities(&self) -> Capabilities { Capabilities { protocol: "mock", conditional_write: true, server_copy: false, locking: false, notes: vec![] } }
    fn content_sha256(&self, _: &Entry) -> Option<String> { self.checksum.lock().unwrap().clone() }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        if path == "file" { if let Some(entry) = self.sequence.lock().unwrap().pop_front() { return Ok(entry); } }
        Ok(self.entry(path))
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let paths: Vec<_> = self.files.lock().unwrap().keys().cloned().chain(self.dirs.lock().unwrap().iter().cloned()).collect();
        Ok(paths.iter().filter(|item| item.rsplit_once('/').map(|p| p.0).unwrap_or("") == path).filter_map(|path| self.entry(path)).collect())
    }
    async fn download(&self, remote: &str, local: &Path) -> Result<()> {
        let bytes = self.download_override.lock().unwrap().clone().or_else(|| self.files.lock().unwrap().get(remote).cloned()).ok_or_else(|| anyhow::anyhow!("mock file disappeared"))?;
        fs::write(local, bytes)?; Ok(())
    }
    async fn upload(&self, local: &Path, remote: &str, condition: WriteCondition) -> Result<Entry> {
        let before = self.entry(remote);
        match condition { WriteCondition::CreateNew if before.is_some() => bail!("mock collision"), WriteCondition::Match(expected) if before.as_ref().is_none_or(|entry| !entry.same_revision(&expected)) => bail!("mock revision conflict"), _ => () }
        let mut bytes = fs::read(local)?;
        if *self.corrupt_uploads.lock().unwrap() { bytes.fill(b'x'); }
        self.insert(remote, &bytes); Ok(self.entry(remote).unwrap())
    }
    async fn mkdir(&self, path: &str) -> Result<()> { self.dirs.lock().unwrap().insert(path.into()); Ok(()) }
    async fn remove(&self, path: &str, _: bool) -> Result<()> { self.files.lock().unwrap().remove(path); Ok(()) }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> { bail!("unused mock operation") }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> { bail!("unused mock operation") }
}
fn store(root: &Path) -> Store {
    let mut store = Store::open(root.join("state")).unwrap();
    store.config.profiles.insert("test".into(), Profile { protocol: Protocol::Webdav, url: "https://example.invalid/".into(), username: None, password_env: None, allow_http: false, timeout_secs: 10, smb_encryption: true, keychain_internet:false,nextcloud_upload_url: None });
    store.save_config().unwrap(); store
}
fn message(result: Result<impl std::fmt::Debug>) -> String { result.unwrap_err().to_string() }

#[tokio::test]
async fn guarded_downloads_reject_disappearance_revision_length_and_hash_changes() {
    let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("download");
    let peer = Peer::source(); let original = peer.entry("file").unwrap();
    peer.dirs.lock().unwrap().insert("directory".into());
    assert!(message(operations::verified_download(&peer, "directory", &path).await).contains("expected a remote file"));
    assert!(message(operations::verified_download(&peer, "absent", &path).await).contains("does not exist"));
    peer.sequence.lock().unwrap().extend([Some(original.clone()), None]);
    assert!(message(operations::verified_download(&peer, "file", &path).await).contains("disappeared"));
    assert!(!path.exists());
    let mut changed = original.clone(); changed.etag = Some("changed".into());
    peer.sequence.lock().unwrap().extend([Some(original.clone()), Some(changed)]);
    assert!(message(operations::verified_download(&peer, "file", &path).await).contains("changed"));
    *peer.download_override.lock().unwrap() = Some(b"shorter than metadata".to_vec());
    assert!(message(operations::verified_download(&peer, "file", &path).await).contains("length"));
    *peer.download_override.lock().unwrap() = None;
    *peer.checksum.lock().unwrap() = Some("wrong expected hash".into());
    assert!(message(operations::verified_download(&peer, "file", &path).await).contains("checksum"));
    *peer.checksum.lock().unwrap() = Some(format!("{:x}", Sha256::digest(b"abc")));
    operations::verified_download(&peer, "file", &path).await.unwrap(); assert_eq!(fs::read(&path).unwrap(), b"abc");
    assert!(message(operations::verified_download_limited(&peer, "file", &path, Some(2)).await).contains("byte limit"));
    assert!(operations::destination_temp(&temp.path().join("absent/out")).is_err());
    assert!(operations::get(&peer, "file", temp.path(), true).await.is_err());
    assert!(operations::get(&peer, "file", &path, false).await.is_err());
}

#[tokio::test]
async fn legacy_cache_wrappers_keep_verified_bytes_and_enqueue_guards_are_enforced() {
    let temp = tempfile::tempdir().unwrap(); let mut store = store(temp.path()); let peer = Peer::source();
    operations::cache_file(&mut store, &peer, "test", "file", true).await.unwrap();
    let destination = temp.path().join("cached");
    operations::cache_get(&mut store, "test", "file", &destination, false).unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"abc");
    let source = temp.path().join("source"); fs::write(&source, b"local").unwrap();
    assert!(operations::enqueue(&mut store, "test", &source, "", false, None, false).is_err());
    assert!(operations::enqueue(&mut store, "unknown", &source, "remote", false, None, false).is_err());
    let job = operations::enqueue(&mut store, "test", &source, "remote", false, None, false).unwrap();
    let resumable = operations::enqueue_with_resume(&mut store, "test", &source, "resumable", false, None, false, true).unwrap();
    let duplicate = operations::enqueue_with_resume(&mut store, "test", &source, "resumable", false, None, false, true).unwrap();
    assert_eq!(resumable.id, duplicate.id);
    let created = operations::enqueue(&mut store, "test", &source, "new", false, None, true).unwrap();
    operations::run_job(&mut store, &peer, &created.id).await.unwrap();
    assert_eq!(peer.files.lock().unwrap()["new"], b"local");
    fs::write(store.home.join("spool").join(&job.snapshot), b"tampered").unwrap();
    assert!(message(operations::run_job(&mut store, &peer, &job.id).await).contains("checksum"));
    assert!(operations::run_job(&mut store, &peer, "missing").await.is_err());
    store.db.jobs[0].status = "cancelled".into();
    assert!(message(operations::run_job(&mut store, &peer, &job.id).await).contains("cancelled"));
}

#[tokio::test]
async fn custom_version_snapshot_races_cannot_replace_the_guarded_revision() {
    let temp = tempfile::tempdir().unwrap(); let store = store(temp.path()); let peer = Peer::source();
    peer.dirs.lock().unwrap().insert("directory".into());
    assert!(operations::archive(&store, &peer, "absent").await.unwrap().is_none());
    assert!(message(operations::archive(&store, &peer, "directory").await).contains("files only"));
    let original = peer.entry("file").unwrap(); let mut changed = original.clone(); changed.etag = Some("other".into());
    peer.sequence.lock().unwrap().extend([Some(original.clone()), Some(changed.clone()), Some(changed)]);
    assert!(message(operations::archive(&store, &peer, "file").await).contains("changed before version snapshot"));
    assert!(fs::read_dir(store.home.join("tmp")).unwrap().next().is_none());
    for id in ["", "../unsafe", "a/b", "line\nfeed"] { assert!(operations::archive_expected_at(&store.home, &peer, "file", &original, id).await.is_err()); }
    peer.sequence.lock().unwrap().push_back(None);
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("source disappeared"));
    let mut other = original.clone(); other.etag = Some("concurrent revision".into());
    peer.sequence.lock().unwrap().push_back(Some(other.clone()));
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("changed before backup"));
    peer.sequence.lock().unwrap().extend([Some(original.clone()), Some(other)]);
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("changed while"));
    *peer.checksum.lock().unwrap() = Some("wrong".into());
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("checksum"));
    *peer.checksum.lock().unwrap() = None;
    *peer.corrupt_uploads.lock().unwrap() = true;
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "corrupt-slot").await).contains("backup checksum mismatch"));
    *peer.corrupt_uploads.lock().unwrap() = false;
    peer.insert(&format!("{}/mismatched-slot/content", operations::version_root("file")), b"bad");
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "mismatched-slot").await).contains("slot contains different bytes"));
    let content = format!("{}/stable/content", operations::version_root("file"));
    peer.insert(&content, b"abc");
    let version = operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await.unwrap();
    assert_eq!(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await.unwrap().id, version.id);
    peer.insert(&content, b"bad");
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("checksum"));
    assert!(message(operations::restore(&store, &peer, &version, true).await).contains("checksum"));
    peer.insert(&content, b"abc");
    assert!(operations::restore(&store, &peer, &version, false).await.is_err());
    assert_eq!(peer.files.lock().unwrap()["file"], b"abc", "create-only restore must preserve an existing destination");
    peer.files.lock().unwrap().remove("file");
    operations::restore(&store, &peer, &version, false).await.unwrap();
    operations::restore(&store, &peer, &version, true).await.unwrap();
    assert_eq!(peer.files.lock().unwrap()["file"], b"abc");
    let mut mismatched = version.clone(); mismatched.path = "unrelated".into();
    peer.insert(&format!("{}/stable/manifest.json", operations::version_root("file")), &serde_json::to_vec(&mismatched).unwrap());
    assert!(message(operations::archive_expected_at(&store.home, &peer, "file", &original, "stable").await).contains("another revision"));
    assert!(message(operations::versions(&store, &peer, "file").await).contains("does not match directory"));
    assert!(operations::versions(&store, &peer, "missing").await.unwrap().is_empty());
}

#[tokio::test]
async fn incomplete_version_directories_and_files_are_ignored() {
    let temp = tempfile::tempdir().unwrap(); let store = store(temp.path()); let peer = Peer::default();
    let root = operations::version_root("file");
    peer.dirs.lock().unwrap().extend([root.clone(), format!("{root}/unfinished")]);
    peer.insert(&format!("{root}/not-a-version"), b"ignored");
    assert!(operations::versions(&store, &peer, "file").await.unwrap().is_empty());
    assert!(fs::read_dir(store.home.join("tmp")).unwrap().next().is_none());
}
