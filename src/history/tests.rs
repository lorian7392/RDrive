use super::*;
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};

pub(super) struct Disk {
    root: PathBuf,
    lose_upload: AtomicBool,
    pub(super) lose_move: AtomicBool,
    pub(super) bad_move_reply: AtomicBool,
    mutate_tree: AtomicBool,
    mutate_download: AtomicBool,
    pub(super) fail: Mutex<Option<String>>,
    pub(super) fail_confirmation: Mutex<Option<PathBuf>>,
    block_state_on_stat: Mutex<Option<PathBuf>>,
    calls: AtomicUsize,
}
impl Disk {
    fn new(root: PathBuf) -> Self {
        fs::create_dir(&root).unwrap();
        Self {
            root,
            lose_upload: false.into(),
            lose_move: false.into(),
            bad_move_reply: false.into(),
            mutate_tree: false.into(),
            mutate_download: false.into(),
            fail: Mutex::new(None),
            fail_confirmation: Mutex::new(None),
            block_state_on_stat: Mutex::new(None),
            calls: 0.into(),
        }
    }
    pub(super) fn at(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }
    pub(super) fn put(&self, path: &str, bytes: &[u8]) {
        let at = self.at(path);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, bytes).unwrap();
    }
    fn check(&self, op: &str) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.lock().unwrap().as_deref() == Some(op) {
            bail!("injected {op} failure");
        }
        Ok(())
    }
    fn entry(&self, path: &str) -> Result<Option<Entry>> {
        let at = self.at(path);
        let info = match fs::metadata(&at) {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Entry {
            path: path.into(),
            is_dir: info.is_dir(),
            size: if info.is_dir() { 0 } else { info.len() },
            modified: None,
            etag: Some(if info.is_dir() {
                format!("dir:{}", info.ino())
            } else {
                format!("{}:{}", info.ino(), state::fingerprint(&at)?)
            }),
        }))
    }
}
#[async_trait]
impl Backend for Disk {
    fn content_sha256(&self, entry: &Entry) -> Option<String> {
        if entry.is_dir {
            None
        } else {
            entry
                .etag
                .as_ref()
                .and_then(|tag| tag.rsplit(':').next())
                .map(str::to_owned)
        }
    }
    fn capabilities(&self) -> backend::Capabilities {
        backend::Capabilities {
            protocol: "test",
            conditional_write: true,
            server_copy: false,
            locking: false,
            notes: vec![],
        }
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        self.check("stat")?;
        if let Some(path) = self.block_state_on_stat.lock().unwrap().take() {
            fs::remove_file(&path)?;
            fs::create_dir(path)?;
        }
        self.entry(path)
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        self.check("list")?;
        let mut result = Vec::new();
        for child in fs::read_dir(self.at(path))? {
            let child = child?;
            result.push(
                self.entry(&backend::join(path, &child.file_name().to_string_lossy())?)?
                    .unwrap(),
            );
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }
    async fn download(&self, remote: &str, local: &Path) -> Result<()> {
        self.check("download")?;
        fs::copy(self.at(remote), local)?;
        if self.mutate_download.swap(false, Ordering::SeqCst) {
            self.put(remote, b"concurrent");
        }
        if self.mutate_tree.swap(false, Ordering::SeqCst) {
            let parent = remote.rsplit_once('/').map_or("", |parts| parts.0);
            self.put(&backend::join(parent, "concurrent").unwrap(), b"new child");
        }
        Ok(())
    }
    async fn upload(&self, local: &Path, remote: &str, condition: WriteCondition) -> Result<Entry> {
        self.check("upload")?;
        let old = self.entry(remote)?;
        match condition {
            WriteCondition::Any => bail!("unconditional writes forbidden in history tests"),
            WriteCondition::CreateNew if old.is_some() => bail!("version_conflict: occupied"),
            WriteCondition::Match(expected)
                if !old
                    .as_ref()
                    .is_some_and(|actual| actual.same_revision(&expected)) =>
            {
                bail!("version_conflict: stale")
            }
            _ => (),
        };
        fs::copy(local, self.at(remote))?;
        let saved = self.entry(remote)?.unwrap();
        if self.lose_upload.swap(false, Ordering::SeqCst) {
            bail!("lost upload response");
        }
        Ok(saved)
    }
    async fn mkdir(&self, path: &str) -> Result<()> {
        self.check("mkdir")?;
        fs::create_dir(self.at(path))?;
        Ok(())
    }
    async fn remove(&self, _: &str, _: bool) -> Result<()> {
        bail!("unconditional remove forbidden")
    }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("unguarded rename forbidden")
    }
    async fn rename_checked(&self, from: &str, to: &str, expected: &Entry) -> Result<Entry> {
        self.check("rename")?;
        if !self
            .entry(from)?
            .as_ref()
            .is_some_and(|actual| actual.same_revision(expected))
            || self.entry(to)?.is_some()
        {
            bail!("version_conflict: guarded move");
        }
        fs::rename(self.at(from), self.at(to))?;
        if let Some(path) = self.fail_confirmation.lock().unwrap().take() {
            let db = Connection::open(path)?;
            db.execute_batch("CREATE TRIGGER fail_confirmation BEFORE UPDATE ON entries BEGIN SELECT RAISE(FAIL,'synthetic confirmation failure'); END;")?;
        }
        let mut saved = self.entry(to)?.unwrap();
        if self.bad_move_reply.swap(false, Ordering::SeqCst) {
            saved.etag = Some("wrong-move-receipt".into());
        }
        if self.lose_move.swap(false, Ordering::SeqCst) {
            bail!("lost move response");
        }
        Ok(saved)
    }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("server copy forbidden")
    }
}
pub(super) fn fixture() -> (tempfile::TempDir, Store, Disk) {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state")).unwrap();
    store.config.profiles.insert(
        "test".into(),
        crate::config::Profile {
            protocol: crate::config::Protocol::Webdav,
            url: "https://example.invalid/".into(),
            username: Some("user".into()),
            password_env: None,
            allow_http: false,
            timeout_secs: 10,
            smb_encryption: true,
            keychain_internet: false,
            nextcloud_upload_url: None,
        },
    );
    store.config.history.default.enabled = true;
    let disk = Disk::new(temp.path().join("remote"));
    (temp, store, disk)
}
pub(super) async fn save(store: &Store, disk: &Disk, path: &str, request: &str) -> HistoryEntry {
    let expected = disk.stat(path).await.unwrap().unwrap();
    capture(
        store,
        disk,
        "test",
        path,
        None,
        "overwrite",
        request,
        &expected,
        false,
    )
    .await
    .unwrap()
    .unwrap()
}
fn edit_archive(store: &Store, id: &str, edit: impl FnOnce(&mut Archive)) {
    let account = binding(&store.config, "test").unwrap();
    let (_, connection) = open(&store.home, true).unwrap().unwrap();
    let mut archive = load(&connection, id, &account).unwrap();
    edit(&mut archive);
    write_archive(&connection, &archive).unwrap();
}

#[tokio::test]
async fn file_history_follows_renames_exports_and_guards_replacements() {
    let (temp, store, disk) = fixture();
    disk.put("original", b"old");
    let first = save(&store, &disk, "original", "first").await;
    assert_eq!(first.state, "prepared");
    assert_eq!(save(&store, &disk, "original", "first").await.id, first.id);
    confirm_request(&store, "test", "first", None).unwrap();
    assert!(confirm_request(&store, "test", "absent", None)
        .unwrap()
        .is_none());
    disk.rename_checked(
        "original",
        "renamed",
        &disk.stat("original").await.unwrap().unwrap(),
    )
    .await
    .unwrap();
    renamed(&store, "test", "original", "renamed").unwrap();
    disk.put("renamed", b"new");
    let next = save(&store, &disk, "renamed", "second").await;
    assert_eq!(next.item_id, first.item_id);
    assert_eq!(
        list(&store.home, &store.config, "test", Some("renamed"), None)
            .unwrap()
            .len(),
        2
    );
    let export_path = temp.path().join("old-copy");
    let result = export(&store, "test", &first.id, &export_path).unwrap();
    assert_eq!(result["verified"], true);
    assert_eq!(fs::read(export_path).unwrap(), b"old");
    let restored = restore(&store, &disk, "test", &first.id, None, None, None, "copy")
        .await
        .unwrap();
    let copied = restored["path"].as_str().unwrap();
    assert_eq!(fs::read(disk.at(copied)).unwrap(), b"old");
    assert_eq!(
        restore(&store, &disk, "test", &first.id, None, None, None, "copy")
            .await
            .unwrap(),
        restored
    );
    let current = disk.stat("renamed").await.unwrap().unwrap();
    let replacement = restore(
        &store,
        &disk,
        "test",
        &first.id,
        Some("renamed"),
        Some(&current),
        Some(None),
        "replace",
    )
    .await
    .unwrap();
    assert_eq!(fs::read(disk.at("renamed")).unwrap(), b"old");
    let prior = replacement["current_version_preserved"].as_str().unwrap();
    assert_eq!(
        lookup(&store.home, &store.config, "test", prior)
            .unwrap()
            .state,
        "confirmed"
    );
    assert!(restore(
        &store,
        &disk,
        "test",
        &first.id,
        Some("renamed"),
        Some(&current),
        Some(None),
        "stale"
    )
    .await
    .is_err());
    assert!(restore(
        &store,
        &disk,
        "test",
        &first.id,
        Some("renamed"),
        Some(&current),
        None,
        "missing-meta"
    )
    .await
    .is_err());
    assert!(restore(
        &store,
        &disk,
        "test",
        &first.id,
        Some("other"),
        None,
        None,
        "copy"
    )
    .await
    .is_err());
    assert!(export(&store, "test", &first.id, &temp.path().join("old-copy")).is_err());
    assert!(export(&store, "test", &first.id, &store.home.join("bad")).is_err());
    assert!(export(&store, "test", &first.id, Path::new("relative")).is_err());
    assert_eq!(
        status(&store.home, &store.config, "test").unwrap()["separate_from_cache"],
        true
    );
}

#[tokio::test]
async fn directory_recycle_export_and_recovery_are_idempotent() {
    let (temp, store, disk) = fixture();
    disk.put("folder/sub/file", b"tree");
    disk.put("folder/empty", b"");
    let archive = save(&store, &disk, "folder", "tree").await;
    let output = temp.path().join("tree-copy");
    let report = export(&store, "test", &archive.id, &output).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 4);
    assert_eq!(fs::read(output.join("sub/file")).unwrap(), b"tree");
    let restored = restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("recovered"),
        None,
        None,
        "tree-restore",
    )
    .await
    .unwrap();
    assert_eq!(restored["directory"], true);
    assert_eq!(fs::read(disk.at("recovered/sub/file")).unwrap(), b"tree");
    assert_eq!(
        restore(
            &store,
            &disk,
            "test",
            &archive.id,
            Some("recovered"),
            None,
            None,
            "tree-restore"
        )
        .await
        .unwrap(),
        restored
    );
    assert!(recycle(
        &store,
        &disk,
        "test",
        "folder",
        false,
        "delete-no-recursive"
    )
    .await
    .is_err());
    disk.lose_move.store(true, Ordering::SeqCst);
    assert!(recycle(&store, &disk, "test", "folder", true, "delete")
        .await
        .is_err());
    let recycled = recycle(&store, &disk, "test", "folder", true, "delete")
        .await
        .unwrap();
    assert_eq!(recycled["recycled"], true);
    assert!(!disk.at("folder").exists());
    assert!(disk
        .at(recycled["remote_backup"].as_str().unwrap())
        .join("sub/file")
        .exists());
    assert_eq!(
        recycle(&store, &disk, "test", "folder", true, "delete")
            .await
            .unwrap(),
        recycled
    );
    assert_eq!(
        list(&store.home, &store.config, "test", Some("folder"), None)
            .unwrap()
            .len(),
        2
    );
    assert!(list(&store.home, &store.config, "test", Some("../bad"), None).is_err());
    let staging = store.home.join("staged");
    private_directory(&staging, true).unwrap();
    assert_eq!(
        export_staged(&store, "test", &archive.id, &staging).unwrap()["kind"],
        "directory"
    );
}

#[tokio::test]
async fn uncertain_content_publish_is_not_replayed_and_inflight_history_is_protected() {
    let (_temp, store, disk) = fixture();
    disk.put("file", b"old");
    let archive = save(&store, &disk, "file", "old").await;
    confirm(&store, "test", &archive.id, None).unwrap();
    disk.lose_upload.store(true, Ordering::SeqCst);
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("copy"),
        None,
        None,
        "lost"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("lost upload"));
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("copy"),
        None,
        None,
        "lost"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("uncertain"));
    let report = clear(&store, "test", true).unwrap();
    assert_eq!(report["protected_prepared"], 1);
    assert!(lookup(&store.home, &store.config, "test", &archive.id).is_ok());
    let account = binding(&store.config, "test").unwrap();
    let (_, connection) = open(&store.home, true).unwrap().unwrap();
    connection
        .execute("UPDATE actions SET result='{}' WHERE binding=?1", [account])
        .unwrap();
    drop(connection);
    assert_eq!(clear(&store, "test", true).unwrap()["removed"], 1);
}

#[tokio::test]
async fn capture_is_fail_closed_on_corruption_capacity_account_and_source_changes() {
    let (_temp, mut store, disk) = fixture();
    disk.put("file", b"old");
    let current = disk.stat("file").await.unwrap().unwrap();
    store.config.history.default.enabled = false;
    assert!(capture(
        &store,
        &disk,
        "test",
        "file",
        None,
        "overwrite",
        "disabled",
        &current,
        false
    )
    .await
    .unwrap()
    .is_none());
    store.config.history.default.enabled = true;
    store.config.history.default.max_bytes = Some(2);
    assert!(capture(
        &store,
        &disk,
        "test",
        "file",
        None,
        "overwrite",
        "quota",
        &current,
        false
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("capacity"));
    assert_eq!(fs::read(disk.at("file")).unwrap(), b"old");
    store.config.history.default.max_bytes = None;
    disk.mutate_download.store(true, Ordering::SeqCst);
    assert!(capture(
        &store,
        &disk,
        "test",
        "file",
        None,
        "overwrite",
        "race",
        &current,
        false
    )
    .await
    .is_err());
    disk.put("file", b"old");
    let archive = save(&store, &disk, "file", "valid").await;
    assert!(capture(
        &store,
        &disk,
        "test",
        "file",
        None,
        "delete",
        "valid",
        &disk.stat("file").await.unwrap().unwrap(),
        false
    )
    .await
    .is_err());
    store.config.profiles.get_mut("test").unwrap().username = Some("other".into());
    assert!(lookup(&store.home, &store.config, "test", &archive.id).is_err());
    store.config.profiles.get_mut("test").unwrap().username = Some("user".into());
    let (root, connection) = open(&store.home, false).unwrap().unwrap();
    let archived = load(
        &connection,
        &archive.id,
        &binding(&store.config, "test").unwrap(),
    )
    .unwrap();
    let content = verify_blob(&root, &archived.nodes[0]).unwrap();
    fs::write(content, b"bad").unwrap();
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        None,
        None,
        None,
        "corrupt"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("checksum"));
    assert!(export_staged(&store, "test", &archive.id, &store.home.join("tmp")).is_err());
}

#[tokio::test]
async fn retention_keeps_prepared_and_prunes_only_confirmed_versions() {
    let (_temp, mut store, disk) = fixture();
    store.config.history.default.keep = 1;
    store.config.history.default.max_bytes = None;
    disk.put("file", b"first");
    let first = save(&store, &disk, "file", "first").await;
    confirm(&store, "test", &first.id, None).unwrap();
    edit_archive(&store, &first.id, |entry| {
        entry.public.created = "2020-01-01T00:00:00Z".into()
    });
    disk.put("file", b"second");
    let second = save(&store, &disk, "file", "second").await;
    confirm(&store, "test", &second.id, None).unwrap();
    disk.put("file", b"third");
    let third = save(&store, &disk, "file", "third").await;
    assert_eq!(prune(&store, "test", false).unwrap()["candidates"], 1);
    assert!(lookup(&store.home, &store.config, "test", &first.id).is_ok());
    assert_eq!(prune(&store, "test", true).unwrap()["removed"], 1);
    assert!(lookup(&store.home, &store.config, "test", &first.id).is_err());
    assert!(lookup(&store.home, &store.config, "test", &third.id).is_ok());
    store.config.history.default.max_bytes = Some(1);
    assert_eq!(prune(&store, "test", true).unwrap()["removed"], 1);
    assert_eq!(
        clear(&store, "test", true).unwrap()["protected_prepared"],
        1
    );
}

#[test]
fn checkpoint_import_is_content_verified_and_replay_bound() {
    let (temp, store, _disk) = fixture();
    let source = temp.path().join("checkpoint");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"snapshot").unwrap();
    let entry = capture_import(&store, "test", "folder", &source, "checkpoint").unwrap();
    assert_eq!(entry.reason, "checkpoint");
    assert_eq!(
        capture_import(&store, "test", "folder", &source, "checkpoint")
            .unwrap()
            .id,
        entry.id
    );
    fs::write(source.join("file"), b"changed").unwrap();
    assert!(capture_import(&store, "test", "folder", &source, "checkpoint").is_err());
    std::os::unix::fs::symlink(source.join("file"), source.join("link")).unwrap();
    assert!(capture_import(&store, "test", "other", &source, "bad-link").is_err());
}

#[test]
fn policies_paths_and_private_files_reject_unsafe_inputs() {
    let (temp, store, _) = fixture();
    let mut policy = HistoryPolicy::default();
    assert!(policy.validate().is_ok());
    for (keep, days, bytes) in [
        (0, 30, None),
        (100001, 30, None),
        (1, 0, None),
        (1, 36501, None),
        (1, 30, Some(0)),
    ] {
        policy.keep = keep;
        policy.max_age_days = days;
        policy.max_bytes = bytes;
        assert!(policy.validate().is_err());
    }
    let mut config = HistoryConfig::default();
    config.profiles.insert("".into(), HistoryPolicy::default());
    assert!(config.validate().is_err());
    config.profiles.clear();
    config
        .profiles
        .insert("test".into(), HistoryPolicy::default());
    assert!(config.validate().is_ok());
    assert!(!config.effective("other").enabled);
    for id in ["", &"A".repeat(64), &"g".repeat(64)] {
        assert!(identifier(id).is_err());
    }
    for path in ["", "a/../b", "./a", "a//b"] {
        assert!(remote_path(path).is_err());
    }
    assert!(private_directory(&temp.path().join("missing"), false)
        .unwrap()
        .is_none());
    assert!(private_directory(&temp.path().join("x".repeat(5000)), false).is_err());
    let public = temp.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_directory(&public, false).is_err());
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&store.home, &link).unwrap();
    assert!(private_directory(&link, false).is_err());
    let source = temp.path().join("source");
    fs::write(&source, b"payload").unwrap();
    assert!(private_file(&source).is_err());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    private_file(&source).unwrap();
    let hard = temp.path().join("hard");
    fs::hard_link(&source, &hard).unwrap();
    assert!(private_file(&source).is_err());
    fs::remove_file(hard).unwrap();
    assert!(list(&store.home, &store.config, "test", None, None)
        .unwrap()
        .is_empty());
    assert!(confirm_request(&store, "test", "empty", None)
        .unwrap()
        .is_none());
    renamed(&store, "test", "missing", "renamed").unwrap();
    assert!(lookup(&store.home, &store.config, "test", &"0".repeat(64)).is_err());
    assert!(open(&temp.path().join("missing-home"), false).is_err());
    let root = store.home.join("history");
    private_directory(&root, true).unwrap();
    private_directory(&root.join("blobs"), true).unwrap();
    let hash = state::fingerprint(&source).unwrap();
    store_blob(&root, &source, &hash).unwrap();
    store_blob(&root, &source, &hash).unwrap();
    fs::write(blob(&root, &hash).unwrap(), b"corrupt").unwrap();
    assert!(store_blob(&root, &source, &hash).is_err());
    fs::remove_file(blob(&root, &hash).unwrap()).unwrap();
    assert!(store_blob(&root, &source, &"0".repeat(64)).is_err());
    assert!(store_blob_with(&root, &source, &hash, |_, destination| {
        fs::write(destination, b"bad")?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
        Err(std::io::ErrorKind::AlreadyExists.into())
    })
    .is_err());
    fs::remove_file(blob(&root, &hash).unwrap()).unwrap();
    store_blob_with(&root, &source, &hash, |_, destination| {
        fs::write(destination, b"payload")?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
        Err(std::io::ErrorKind::AlreadyExists.into())
    })
    .unwrap();
    fs::remove_file(blob(&root, &hash).unwrap()).unwrap();
    assert!(store_blob_with(&root, &source, &hash, |_, _| Err(
        std::io::ErrorKind::PermissionDenied.into()
    ))
    .is_err());
}

#[tokio::test]
async fn manifests_and_capture_identities_are_validated_before_use() {
    let (_temp, store, disk) = fixture();
    disk.put("file", b"old");
    let entry = save(&store, &disk, "file", "valid").await;
    let (root, connection) = open(&store.home, true).unwrap().unwrap();
    let account = binding(&store.config, "test").unwrap();
    let original = load(&connection, &entry.id, &account).unwrap();
    for change in 0..10 {
        let mut bad = original.clone();
        match change {
            0 => bad.version = 2,
            1 => bad.public.source = "other".into(),
            2 => bad.public.reason = "bad".into(),
            3 => bad.nodes.clear(),
            4 => bad.nodes.push(bad.nodes[0].clone()),
            5 => bad.nodes[0].sha256 = None,
            6 => bad.nodes[0].path = "../bad".into(),
            7 => bad.public.bytes += 1,
            8 => bad.public.kind = "directory".into(),
            _ => bad.nodes[0].path = "child".into(),
        };
        assert!(checked_archive(&serde_json::to_string(&bad).unwrap(), &account).is_err());
    }
    assert!(checked_archive("{}", &account).is_err());
    let mut metadata = crate::metadata::capture(
        &verify_blob(&root, &original.nodes[0]).unwrap(),
        &Default::default(),
    )
    .unwrap();
    metadata.content_sha256 = "0".repeat(64);
    let mut bad = original.clone();
    bad.nodes[0].metadata = Some(metadata);
    assert!(checked_archive(&serde_json::to_string(&bad).unwrap(), &account).is_err());
    let mut huge = original.clone();
    huge.nodes[0].entry.size = u64::MAX;
    let mut child = huge.nodes[0].clone();
    child.path = "child".into();
    huge.nodes.push(child);
    assert!(checked_archive(&serde_json::to_string(&huge).unwrap(), &account).is_err());
    let expected = disk.stat("file").await.unwrap().unwrap();
    for (path, reason, request) in [
        ("file", "overwrite", ""),
        ("file", "bad", "request"),
        ("other", "overwrite", "request"),
    ] {
        assert!(
            capture(&store, &disk, "test", path, None, reason, request, &expected, false)
                .await
                .is_err()
        );
    }
    let mut directory = expected.clone();
    directory.path = "folder".into();
    directory.is_dir = true;
    assert!(roots(&directory, vec![expected.clone()]).is_err());
    let mut child = expected.clone();
    child.path = "folder/".into();
    assert!(roots(&directory, vec![child]).is_err());
    let mut child = expected.clone();
    child.path = "folder/file".into();
    assert!(roots(&directory, vec![child.clone(), child]).is_err());
    assert!(observed_tree(&disk, "missing", &expected).await.is_err());
    let mut stale = expected.clone();
    stale.etag = Some("stale".into());
    assert!(observed_tree(&disk, "file", &stale).await.is_err());
    let mismatched = state::key("mismatched");
    let mut bad = original.clone();
    bad.public.id = mismatched;
    connection
        .execute(
            "UPDATE entries SET archive=?1 WHERE id=?2",
            params![serde_json::to_string(&bad).unwrap(), entry.id],
        )
        .unwrap();
    assert!(load(&connection, &entry.id, &account).is_err());
}

fn edit_action(store: &Store, request: &str, edit: impl FnOnce(&mut Value)) {
    let account = binding(&store.config, "test").unwrap();
    let id = state::key(&serde_json::to_string(&(&account, request)).unwrap());
    let (_, connection) = open(&store.home, true).unwrap().unwrap();
    let text: String = connection
        .query_row("SELECT value FROM actions WHERE id=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let mut value: Value = serde_json::from_str(&text).unwrap();
    edit(&mut value);
    connection
        .execute(
            "UPDATE actions SET value=?1,result=?2 WHERE id=?3",
            params![
                serde_json::to_string(&value).unwrap(),
                value
                    .get("result")
                    .filter(|value| !value.is_null())
                    .map(serde_json::to_string)
                    .transpose()
                    .unwrap(),
                id
            ],
        )
        .unwrap();
}
fn record(disk: &Disk, path: &str, value: &str) -> MetadataRecord {
    let mut record = crate::metadata::capture(&disk.at(path), &Default::default()).unwrap();
    record
        .attributes
        .insert("user.history-test".into(), value.into());
    record
}
#[tokio::test]
async fn metadata_is_preserved_and_has_its_own_restore_conflict_guard() {
    let (temp, store, disk) = fixture();
    disk.put("file", b"old");
    let expected = disk.stat("file").await.unwrap().unwrap();
    let tags = record(&disk, "file", "0102");
    crate::metadata_remote::save_record(&disk, "file", &expected, &tags)
        .await
        .unwrap();
    let archive = save(&store, &disk, "file", "with-tags").await;
    let exported = temp.path().join("tagged");
    export(&store, "test", &archive.id, &exported).unwrap();
    assert_eq!(
        crate::metadata::capture(&exported, &Default::default())
            .unwrap()
            .attributes["user.history-test"],
        "0102"
    );
    let restored = restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("copy"),
        None,
        None,
        "tags-copy",
    )
    .await
    .unwrap();
    let copied = disk.stat("copy").await.unwrap().unwrap();
    assert_eq!(
        crate::metadata_remote::load(&disk, "copy", &copied)
            .await
            .unwrap()
            .unwrap()
            .attributes,
        tags.attributes
    );
    edit_action(&store, "tags-copy", |action| {
        action["result"] = Value::Null;
        action["phase"] = json!("content-confirmed");
    });
    assert_eq!(
        restore(
            &store,
            &disk,
            "test",
            &archive.id,
            Some("copy"),
            None,
            None,
            "tags-copy"
        )
        .await
        .unwrap(),
        restored
    );
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("file"),
        Some(&expected),
        Some(None),
        "stale-tags"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("metadata changed"));
    let current = crate::metadata_remote::load(&disk, "file", &expected)
        .await
        .unwrap()
        .unwrap();
    let fingerprint = current.fingerprint().unwrap();
    let restored = restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("file"),
        Some(&expected),
        Some(Some(fingerprint)),
        "replace-tags",
    )
    .await
    .unwrap();
    assert_eq!(restored["metadata_restored"], true);
    disk.lose_upload.store(true, Ordering::SeqCst);
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("other"),
        None,
        None,
        "metadata-race"
    )
    .await
    .is_err());
    let actual = disk.stat("other").await.unwrap().unwrap();
    let concurrent = record(&disk, "other", "0304");
    crate::metadata_remote::save_record(&disk, "other", &actual, &concurrent)
        .await
        .unwrap();
    edit_action(&store, "metadata-race", |action| {
        action["phase"] = json!("content-confirmed");
        action["confirmed"] = serde_json::to_value(&actual).unwrap();
    });
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("other"),
        None,
        None,
        "metadata-race"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("metadata changed"));
}

#[tokio::test]
async fn directory_resume_reconciles_lost_move_and_refuses_uncertain_staging() {
    let (_temp, store, disk) = fixture();
    disk.put("folder/sub/file", b"payload");
    let current = disk.stat("folder/sub/file").await.unwrap().unwrap();
    crate::metadata_remote::save_record(
        &disk,
        "folder/sub/file",
        &current,
        &record(&disk, "folder/sub/file", "ab"),
    )
    .await
    .unwrap();
    let archive = save(&store, &disk, "folder", "folder").await;
    disk.lose_move.store(true, Ordering::SeqCst);
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("recovered"),
        None,
        None,
        "lost-dir-move"
    )
    .await
    .is_err());
    let restored = restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("recovered"),
        None,
        None,
        "lost-dir-move",
    )
    .await
    .unwrap();
    assert_eq!(restored["directory"], true);
    let current = disk.stat("recovered/sub/file").await.unwrap().unwrap();
    assert_eq!(
        crate::metadata_remote::load(&disk, "recovered/sub/file", &current)
            .await
            .unwrap()
            .unwrap()
            .attributes["user.history-test"],
        "ab"
    );
    *disk.fail.lock().unwrap() = Some("mkdir".into());
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("failed"),
        None,
        None,
        "failed-stage"
    )
    .await
    .is_err());
    *disk.fail.lock().unwrap() = None;
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("failed"),
        None,
        None,
        "failed-stage"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("uncertain"));
    let fresh = disk.stat("folder").await.unwrap().unwrap();
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("folder"),
        Some(&fresh),
        Some(None),
        "replace-dir"
    )
    .await
    .is_err());
    let stale = Entry {
        etag: Some("wrong".into()),
        ..fresh.clone()
    };
    assert!(recycle_checked(
        &store,
        &disk,
        "test",
        "folder",
        true,
        "stale-delete",
        Some(&stale)
    )
    .await
    .is_err());
    let deleted = recycle_checked(
        &store,
        &disk,
        "test",
        "folder",
        true,
        "checked-delete",
        Some(&fresh),
    )
    .await
    .unwrap();
    assert_eq!(deleted["recycled"], true);
    edit_action(&store, "checked-delete", |action| {
        action["result"] = Value::Null;
    });
    assert!(recycle_checked(
        &store,
        &disk,
        "test",
        "folder",
        true,
        "checked-delete",
        Some(&stale)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn journals_and_local_imports_refuse_changed_identity_or_invalid_sources() {
    let (temp, mut store, disk) = fixture();
    disk.put("file", b"old");
    let archive = save(&store, &disk, "file", "valid").await;
    restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("copy"),
        None,
        None,
        "journal",
    )
    .await
    .unwrap();
    edit_action(&store, "journal", |action| {
        action["version"] = json!(2);
    });
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("copy"),
        None,
        None,
        "journal"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("journal is corrupt"));
    assert!(restore(
        &store,
        &disk,
        "test",
        &archive.id,
        Some("new"),
        None,
        None,
        ""
    )
    .await
    .is_err());
    assert!(capture_import(&store, "test", "copy", &disk.at("file"), "").is_err());
    let imported = capture_import(&store, "test", "file", &disk.at("file"), "import").unwrap();
    assert_eq!(imported.kind, "file");
    assert!(capture_import(&store, "test", "other", &disk.at("file"), "import").is_err());
    store.config.history.default.max_bytes = Some(1);
    assert!(capture_import(&store, "test", "other", &disk.at("file"), "quota").is_err());
    store.config.history.default.max_bytes = Some(4);
    assert!(capture_import(&store, "test", "other", &disk.at("file"), "global-quota").is_err());
    let weird = temp.path().join("weird");
    fs::create_dir(&weird).unwrap();
    use std::os::unix::ffi::OsStringExt;
    assert!(import_name(std::ffi::OsString::from_vec(vec![0xff])).is_err());
    let pipe = weird.join("pipe");
    let c = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(capture_import(&store, "test", "other", &weird, "invalid-type").is_err());
    store.config.history.default.enabled = false;
    assert!(recycle(&store, &disk, "test", "file", false, "disabled")
        .await
        .is_err());
}

#[tokio::test]
async fn safety_error_paths_preserve_current_data_and_local_payloads() {
    let (temp, mut store, disk) = fixture();
    disk.put("folder/file", b"old");
    let expected = disk.stat("folder").await.unwrap().unwrap();
    disk.mutate_tree.store(true, Ordering::SeqCst);
    assert!(capture(
        &store,
        &disk,
        "test",
        "folder",
        None,
        "delete",
        "tree-race",
        &expected,
        false
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("directory changed"));
    let file = disk.stat("folder/file").await.unwrap().unwrap();
    let mut metadata = record(&disk, "folder/file", "00");
    metadata.content_sha256 = "0".repeat(64);
    let sidecar = crate::metadata_remote::sidecar_path("folder/file", &file).unwrap();
    disk.put(
        &sidecar,
        &serde_json::to_vec(
            &json!({"version":1,"path":"folder/file","entry":file,"record":metadata}),
        )
        .unwrap(),
    );
    assert!(capture(
        &store,
        &disk,
        "test",
        "folder/file",
        None,
        "overwrite",
        "bad-metadata",
        &file,
        false
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("content hash mismatch"));
    fs::remove_file(disk.at(sidecar.as_str())).unwrap();
    let saved = capture(
        &store,
        &disk,
        "test",
        "folder/file",
        Some("stable-system-id"),
        "overwrite",
        "stable",
        &file,
        false,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(saved.item_id, "stable-system-id");
    assert_eq!(
        list(
            &store.home,
            &store.config,
            "test",
            None,
            Some("stable-system-id")
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        list(&store.home, &store.config, "test", Some("folder"), None)
            .unwrap()
            .len(),
        1
    );
    assert!(list(&store.home, &store.config, "test", Some("./folder"), None).is_err());
    store.config.history.default.max_bytes = Some(4);
    assert!(capture(
        &store,
        &disk,
        "test",
        "folder/file",
        None,
        "overwrite",
        "protected-quota",
        &file,
        false
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("capacity"));
    store.config.history.default.max_bytes = None;
    let output = temp.path().join("exists");
    fs::write(&output, b"preserve").unwrap();
    assert!(export_inner(&store, "test", &saved.id, &output).is_err());
    let (root, connection) = open(&store.home, true).unwrap().unwrap();
    let archive = load(
        &connection,
        &saved.id,
        &binding(&store.config, "test").unwrap(),
    )
    .unwrap();
    let mut nohash = archive.nodes[0].clone();
    nohash.sha256 = None;
    assert!(verify_blob(&root, &nohash).is_err());
    nohash.sha256 = Some("invalid".into());
    assert!(verify_blob(&root, &nohash).is_err());
    assert!(copy_node_with(
        &root,
        &archive.nodes[0],
        &temp.path().join("failed-export"),
        |_, output| {
            use std::io::Write;
            output.write_all(b"bad")?;
            Ok(3)
        }
    )
    .is_err());
    assert!(import_blob(
        &root,
        &disk.at("folder/file"),
        archive.nodes[0].sha256.as_deref().unwrap(),
        99
    )
    .is_err());
    assert!(import_limit(1_000_001).is_err());
    import_limit(1_000_000).unwrap();
    connection
        .execute("UPDATE entries SET bytes=1.5", [])
        .unwrap();
    assert!(capture(
        &store,
        &disk,
        "test",
        "folder/file",
        None,
        "overwrite",
        "bad-sql-size",
        &file,
        false
    )
    .await
    .is_err());
    assert!(capture_import(
        &store,
        "test",
        "new",
        &disk.at("folder/file"),
        "bad-import-size"
    )
    .is_err());
    connection
        .execute("UPDATE entries SET bytes=3", [])
        .unwrap();
    drop(connection);
    fs::write(root.join("blobs/.capture-test"), b"temporary").unwrap();
    clear(&store, "test", true).unwrap();
    assert!(root.join("blobs/.capture-test").exists());
    renamed(&store, "test", "folder/file", "renamed").unwrap();
    renamed(&store, "test", "renamed", "again").unwrap();
    let empty = Store::open(temp.path().join("empty-state")).unwrap();
    assert_eq!(
        clear(&empty, "missing", true).unwrap_err().to_string(),
        "history profile not found"
    );
}

#[test]
fn empty_and_unreadable_history_control_paths_are_not_created_or_followed() {
    let (_temp, store, _) = fixture();
    assert_eq!(clear(&store, "test", true).unwrap()["candidates"], 0);
    let root = store.home.join("history");
    private_directory(&root, true).unwrap();
    assert!(open(&store.home, false).unwrap().is_none());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(open(&store.home, false).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let (_root, connection) = open(&store.home, true).unwrap().unwrap();
    drop(connection);
    let db = root.join("history.sqlite3");
    let backup = root.join("db-real");
    fs::rename(&db, &backup).unwrap();
    std::os::unix::fs::symlink(&backup, &db).unwrap();
    assert!(open(&store.home, false).is_err());
    fs::remove_file(&db).unwrap();
    fs::rename(backup, &db).unwrap();
    let sidecar = root.join("history.sqlite3-wal");
    if sidecar.exists() {
        fs::remove_file(&sidecar).unwrap();
    }
    std::os::unix::fs::symlink(&db, &sidecar).unwrap();
    assert!(open(&store.home, false).is_err());
}

#[tokio::test]
async fn queued_history_uploads_guard_existing_and_absent_targets_and_keep_failed_snapshots() {
    let (temp, mut store, disk) = fixture();
    let local = temp.path().join("upload");
    fs::write(&local, b"new").unwrap();
    disk.put("existing", b"old");
    let first =
        crate::operations::enqueue(&mut store, "test", &local, "existing", true, None, false)
            .unwrap();
    crate::operations::run_job(&mut store, &disk, &first.id)
        .await
        .unwrap();
    assert_eq!(fs::read(disk.at("existing")).unwrap(), b"new");
    let archived = list(&store.home, &store.config, "test", Some("existing"), None).unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].state, "confirmed");
    assert!(store
        .db
        .jobs
        .iter()
        .find(|job| job.id == first.id)
        .unwrap()
        .expected
        .is_some());
    let fresh = crate::operations::enqueue(&mut store, "test", &local, "absent", true, None, false)
        .unwrap();
    crate::operations::run_job(&mut store, &disk, &fresh.id)
        .await
        .unwrap();
    assert!(
        !store
            .db
            .jobs
            .iter()
            .find(|job| job.id == fresh.id)
            .unwrap()
            .overwrite
    );
    assert_eq!(fs::read(disk.at("absent")).unwrap(), b"new");
    let create =
        crate::operations::enqueue(&mut store, "test", &local, "existing", false, None, false)
            .unwrap();
    assert!(crate::operations::run_job(&mut store, &disk, &create.id)
        .await
        .is_err());
    assert!(store.home.join("spool").join(&create.snapshot).exists());
    let old = disk.stat("existing").await.unwrap().unwrap();
    disk.put("existing", b"concurrent");
    let stale = crate::operations::enqueue(
        &mut store,
        "test",
        &local,
        "existing",
        false,
        Some(old),
        false,
    )
    .unwrap();
    assert!(crate::operations::run_job(&mut store, &disk, &stale.id)
        .await
        .unwrap_err()
        .to_string()
        .contains("preserved in queue"));
    assert_eq!(fs::read(disk.at("existing")).unwrap(), b"concurrent");
    assert!(store.home.join("spool").join(&stale.snapshot).exists());
}

#[tokio::test]
async fn ordinary_sync_archives_remote_changes_and_never_downloads_recovery_tombstones() {
    let (temp, mut store, disk) = fixture();
    let local = temp.path().join("sync");
    fs::create_dir(&local).unwrap();
    fs::write(local.join("file"), b"old").unwrap();
    disk.put("file", b"old");
    let baseline = store.home.join("baselines/history.json");
    let mut options = crate::sync::SyncOptions {
        apply: false,
        direction: crate::sync::Direction::Both,
        delete: true,
    };
    let preview = crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    assert!(!preview.applied);
    assert!(list(&store.home, &store.config, "test", None, None)
        .unwrap()
        .is_empty());
    options.apply = true;
    crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    fs::write(local.join("file"), b"new").unwrap();
    let uploaded = crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    assert!(uploaded.applied && uploaded.baseline_updated);
    assert_eq!(fs::read(disk.at("file")).unwrap(), b"new");
    let versions = list(&store.home, &store.config, "test", Some("file"), None).unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].reason, "overwrite");
    assert_eq!(versions[0].state, "confirmed");
    fs::remove_file(local.join("file")).unwrap();
    crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    assert!(!disk.at("file").exists());
    let versions = list(&store.home, &store.config, "test", Some("file"), None).unwrap();
    assert!(versions
        .iter()
        .any(|entry| entry.reason == "delete" && entry.remote_backup.is_some()));
    let again = crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    assert!(again.actions.is_empty());
    assert_eq!(fs::read_dir(&local).unwrap().count(), 0);
    store.config.history.default.enabled = false;
    fs::write(local.join("fresh"), b"fresh").unwrap();
    crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false,
    )
    .await
    .unwrap();
    fs::remove_file(local.join("fresh")).unwrap();
    assert!(crate::sync::run_with_history(
        &store, &disk, "test", &local, "", &baseline, &options, false
    )
    .await
    .is_err());
    assert_eq!(fs::read(disk.at("fresh")).unwrap(), b"fresh");
}

#[tokio::test]
async fn alias_scopes_follow_parent_renames_across_domains_and_refuse_ambiguity() {
    let (_temp, store, disk) = fixture();
    assert!(current_path(&store.home, &store.config, "test", "missing")
        .unwrap()
        .is_none());
    disk.put("old-parent/child", b"history");
    let entry = save(&store, &disk, "old-parent/child", "child-history").await;
    renamed(&store, "test", "old-parent", "new-parent").unwrap();
    renamed(&store, "test", "new-parent", "new-parent").unwrap();
    assert_eq!(
        current_path(&store.home, &store.config, "test", &entry.item_id)
            .unwrap()
            .as_deref(),
        Some("new-parent/child")
    );
    assert_eq!(
        list(
            &store.home,
            &store.config,
            "test",
            Some("new-parent"),
            Some("other-domain-folder-id")
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        list(
            &store.home,
            &store.config,
            "test",
            Some("new-parent/child"),
            Some("other-domain-file-id")
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        lookup(&store.home, &store.config, "test", &entry.id)
            .unwrap()
            .path,
        "old-parent/child"
    );
    assert_eq!(
        list(&store.home, &store.config, "test", Some("old-parent"), None)
            .unwrap()
            .len(),
        1
    );
    assert!(
        list(&store.home, &store.config, "test", Some("unrelated"), None)
            .unwrap()
            .is_empty()
    );
    let (_, connection) = open(&store.home, true).unwrap().unwrap();
    let account = binding(&store.config, "test").unwrap();
    connection.execute_batch("CREATE TRIGGER fail_alias_delete BEFORE DELETE ON aliases BEGIN SELECT RAISE(FAIL,'synthetic alias failure'); END;").unwrap();
    assert!(renamed(&store, "test", "new-parent", "failed-parent").is_err());
    connection
        .execute_batch("DROP TRIGGER fail_alias_delete")
        .unwrap();
    connection
        .execute(
            "INSERT INTO aliases(binding,path,item_id) VALUES(?1,'./invalid','invalid-item')",
            [&account],
        )
        .unwrap();
    assert!(current_path(&store.home, &store.config, "test", "invalid-item").is_err());
    connection
        .execute(
            "INSERT INTO aliases(binding,path,item_id) VALUES(?1,'different',?2)",
            params![account, entry.item_id],
        )
        .unwrap();
    assert!(
        current_path(&store.home, &store.config, "test", &entry.item_id)
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    connection.execute_batch("DROP TABLE aliases").unwrap();
    assert!(current_path(&store.home, &store.config, "test", &entry.item_id).is_err());
}

#[tokio::test]
async fn queued_create_guard_persistence_failure_never_starts_a_remote_write() {
    let (temp, mut store, disk) = fixture();
    let local = temp.path().join("upload");
    fs::write(&local, b"retained upload").unwrap();
    let job = crate::operations::enqueue(&mut store, "test", &local, "absent", true, None, false)
        .unwrap();
    let ledger = store.home.join("state.json");
    *disk.block_state_on_stat.lock().unwrap() = Some(ledger.clone());
    assert!(crate::operations::run_job(&mut store, &disk, &job.id)
        .await
        .is_err());
    assert!(!disk.at("absent").exists());
    assert!(store.home.join("spool").join(&job.snapshot).exists());
    fs::remove_dir(ledger).unwrap();
    store.save().unwrap();
    crate::operations::run_job(&mut store, &disk, &job.id)
        .await
        .unwrap();
    assert_eq!(fs::read(disk.at("absent")).unwrap(), b"retained upload");
}

#[tokio::test]
async fn legacy_remote_version_restoration_guards_and_preserves_the_current_version() {
    let (_temp, store, disk) = fixture();
    disk.put("file", b"original");
    let version = crate::operations::archive(&store, &disk, "file")
        .await
        .unwrap()
        .unwrap();
    disk.put("file", b"new current version");
    crate::operations::restore(&store, &disk, &version, true)
        .await
        .unwrap();
    assert_eq!(fs::read(disk.at("file")).unwrap(), b"original");
    let retained = crate::operations::versions(&store, &disk, "file")
        .await
        .unwrap();
    assert_eq!(retained.len(), 2);
    assert!(retained
        .iter()
        .any(|item| item.sha256 == state::key("new current version")));
    fs::remove_file(disk.at("file")).unwrap();
    crate::operations::restore(&store, &disk, &version, true)
        .await
        .unwrap();
    fs::remove_file(disk.at("file")).unwrap();
    crate::operations::restore(&store, &disk, &version, false)
        .await
        .unwrap();
    assert!(crate::operations::restore(&store, &disk, &version, false)
        .await
        .is_err());
    assert_eq!(fs::read(disk.at("file")).unwrap(), b"original");
}
