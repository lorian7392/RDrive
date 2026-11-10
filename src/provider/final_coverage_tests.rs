//! Final provider guards exercised only with temporary files and mock storage.
use super::coverage_tests::setup;
use super::test_backend::Disk;
use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn request(value: Value) -> Request {
    serde_json::from_value(value).unwrap()
}
fn stage(root: &Path) -> (PathBuf, File) {
    let path = private(&root.join("stage")).unwrap();
    let descriptor = open_staging(&path).unwrap();
    (path, descriptor)
}

#[test]
fn metadata_version_keeps_the_exact_original_json_encoding() {
    let attributes = BTreeMap::from([
        ("user.z#S".into(), "ff".into()),
        ("user.a#S".into(), "00".into()),
    ]);
    assert_eq!(
        metadata_version("文档/a", &None, &attributes, false),
        state::key(r#"["文档/a",null,{"user.a#S":"00","user.z#S":"ff"},false]"#)
    );
    for tags in [None, Some(String::new()), Some("quote\"\\用户".into())] {
        for pinned in [false, true] {
            let original =
                serde_json::to_string(&("folder/file", &tags, &attributes, pinned)).unwrap();
            assert_eq!(
                metadata_version("folder/file", &tags, &attributes, pinned),
                state::key(&original)
            );
        }
    }
}

#[test]
fn quick_root_status_and_unmapped_urls_require_no_backend() {
    let (temporary, store, mut domain) = setup();
    let visible = temporary.path().join("visible");
    fs::create_dir(&visible).unwrap();
    domain.smart = true;
    domain.local_root = Some(visible);
    add_domain(&store, domain.clone()).unwrap();
    checked_journal(&store.home, &store.config, &domain).unwrap();
    let home = store.home.clone();
    drop(store);
    let status = request(json!({"v":1,"op":"status","domain":domain.id,"id":"root"}));
    assert_eq!(quick(&home, &status).unwrap()["state"], "online");
    local_pin(
        &home,
        &request(json!({"v":1,"op":"pin","domain":domain.id,"id":"root","pinned":true})),
    )
    .unwrap();
    assert_eq!(quick(&home, &status).unwrap()["state"], "pinned");
    let outside = url::Url::from_file_path(temporary.path().join("outside")).unwrap();
    let error = quick(
        &home,
        &request(json!({"v":1,"op":"resolve","file_url":outside.as_str()})),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("outside configured Finder roots")
    );
}

struct Peer {
    disk: Arc<Disk>,
    weak: bool,
    change_metadata: bool,
    sidecar_reads: AtomicUsize,
    uploads: AtomicUsize,
    changed: AtomicBool,
}
impl Peer {
    fn new(disk: Arc<Disk>, weak: bool, change_metadata: bool) -> Arc<Self> {
        Arc::new(Self {
            disk,
            weak,
            change_metadata,
            sidecar_reads: AtomicUsize::new(0),
            uploads: AtomicUsize::new(0),
            changed: AtomicBool::new(false),
        })
    }
    fn project(&self, mut entry: Entry) -> Entry {
        if self.weak && !entry.is_dir {
            entry.etag = Some("W/\"fixture-revision\"".into());
        }
        entry
    }
}
#[async_trait]
impl Backend for Peer {
    fn capabilities(&self) -> backend::Capabilities {
        self.disk.capabilities()
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        Ok(self.disk.stat(path).await?.map(|entry| self.project(entry)))
    }
    async fn stat_metadata(&self, path: &str) -> Result<Option<Entry>> {
        if path == "document"
            && self.change_metadata
            && self.sidecar_reads.load(Ordering::SeqCst) >= 2
            && !self.changed.swap(true, Ordering::SeqCst)
        {
            // Another writer replaces the sidecar after namespace publication,
            // before the final metadata read, while file contents stay intact.
            let entry = self.disk.stat(path).await?.unwrap();
            let sidecar = self
                .disk
                .root
                .join(crate::metadata_remote::sidecar_path(path, &entry)?);
            let mut envelope: Value = state::read_json(&sidecar)?;
            envelope["record"]["attributes"]["user.final#S"] = json!("02");
            state::atomic_json(&sidecar, &envelope)?;
        }
        self.stat(path).await
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        Ok(self
            .disk
            .list(path)
            .await?
            .into_iter()
            .map(|entry| self.project(entry))
            .collect())
    }
    async fn download(&self, path: &str, local: &Path) -> Result<()> {
        if path.starts_with(".rdrive-metadata/") {
            self.sidecar_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.disk.download(path, local).await
    }
    async fn upload(&self, local: &Path, path: &str, condition: WriteCondition) -> Result<Entry> {
        self.uploads.fetch_add(1, Ordering::SeqCst);
        self.disk.upload(local, path, condition).await
    }
    async fn mkdir(&self, path: &str) -> Result<()> {
        self.disk.mkdir(path).await
    }
    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        self.disk.remove(path, recursive).await
    }
    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        self.disk.rename(from, to, overwrite).await
    }
    async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        self.disk.copy(from, to, overwrite).await
    }
}
struct Connected(Arc<dyn Backend>);
#[async_trait]
impl Connector for Connected {
    async fn connect(
        &self,
        _: &config::Profile,
        _: &str,
        _: crate::transfer::TransferContext,
    ) -> Result<Arc<dyn Backend>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn ordinary_domain_dispatch_stops_at_its_explicit_missing_credential_boundary() {
    let (temporary, mut store, domain) = setup();
    assert!(!domain.smart);
    let missing = format!("RDRIVE_UNSET_PROVIDER_{}", uuid::Uuid::new_v4().simple());
    assert!(std::env::var_os(&missing).is_none());
    store.config.profiles.get_mut("p").unwrap().password_env = Some(missing.clone());
    store.save_config().unwrap();
    add_domain(&store, domain.clone()).unwrap();
    let home = store.home.clone();
    drop(store);
    let (staging, fd) = stage(temporary.path());
    let error = process(
        &home,
        &staging,
        &fd,
        request(json!({"v":1,"op":"item","domain":domain.id,"id":"root"})),
        Default::default(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains(&missing),
        "ordinary dispatch must stop before Keychain or remote access: {error:#}"
    );
    assert!(fs::read_dir(staging).unwrap().next().is_none());
}

#[tokio::test]
async fn weak_revision_fetch_does_not_adopt_a_metadata_write_guard() {
    let (temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let disk = Disk::new(temporary.path().join("remote"));
    fs::write(disk.root.join("document"), b"read-only weak revision").unwrap();
    let peer = Peer::new(disk, true, false);
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    refresh(&store, peer.as_ref(), &domain, &mut journal)
        .await
        .unwrap();
    let item = journal.items.values().next().unwrap().item.clone();
    let home = store.home.clone();
    drop(store);
    let (staging, fd) = stage(temporary.path());
    let response = online_process_with(&home, &staging, &fd, request(json!({"v":1,"op":"fetch","domain":domain.id,"id":item.id,"base_content_version":item.content_version})), Default::default(), &Connected(peer.clone())).await.unwrap();
    assert_eq!(
        fs::read(response["content_path"].as_str().unwrap()).unwrap(),
        b"read-only weak revision"
    );
    let store = Store::open(home.clone()).unwrap();
    assert!(
        checked_journal(&home, &store.config, &domain)
            .unwrap()
            .items[&item.id]
            .strong
            .is_none()
    );
    assert_eq!(peer.sidecar_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn empty_create_and_content_free_modify_preserve_contents_without_reupload() {
    let (temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let home = store.home.clone();
    drop(store);
    let (staging, fd) = stage(temporary.path());
    let disk = Disk::new(temporary.path().join("remote"));
    let peer = Peer::new(disk.clone(), false, false);
    let connector = Connected(peer.clone());
    let created = online_process_with(&home, &staging, &fd, request(json!({"v":1,"op":"create","domain":domain.id,"name":"document","kind":"file","client_item_id":"empty-create"})), Default::default(), &connector).await.unwrap()["item"].clone();
    assert_eq!(fs::read(disk.root.join("document")).unwrap(), b"");
    let uploads = peer.uploads.load(Ordering::SeqCst);
    let modified = online_process_with(&home, &staging, &fd, request(json!({"v":1,"op":"modify","domain":domain.id,"id":created["id"],"request_id":"no-content-change","base_content_version":created["content_version"],"base_metadata_version":created["metadata_version"]})), Default::default(), &connector).await.unwrap()["item"].clone();
    assert_eq!(modified["id"], created["id"]);
    assert_eq!(modified["content_version"], created["content_version"]);
    assert_eq!(
        peer.uploads.load(Ordering::SeqCst),
        uploads,
        "content-free modification must not repeat file or metadata uploads"
    );
    assert_eq!(fs::read(disk.root.join("document")).unwrap(), b"");
}

#[tokio::test]
async fn independently_changed_final_metadata_never_records_a_success_receipt() {
    let (temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let home = store.home.clone();
    drop(store);
    let (staging, fd) = stage(temporary.path());
    let source = staging.join("source");
    fs::write(&source, b"confirmed file bytes").unwrap();
    let disk = Disk::new(temporary.path().join("remote"));
    let peer = Peer::new(disk.clone(), false, true);
    let error = online_process_with(&home, &staging, &fd, request(json!({"v":1,"op":"create","domain":domain.id,"name":"document","kind":"file","client_item_id":"raced-create","content_path":source,"extended_attributes":{"user.final#S":"01"}})), Default::default(), &Connected(peer.clone())).await.unwrap_err();
    assert!(
        peer.changed.load(Ordering::SeqCst),
        "the sidecar race must actually execute"
    );
    assert!(
        error
            .to_string()
            .contains("file metadata changed after the confirmed write"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(disk.root.join("document")).unwrap(),
        b"confirmed file bytes"
    );
    let store = Store::open(home.clone()).unwrap();
    let journal = checked_journal(&home, &store.config, &domain).unwrap();
    let pending = &journal.requests[&format!("{}:create:raced-create", state::key(&domain.id))];
    assert!(pending.confirmed.is_some());
    assert!(pending.response.is_none());
}

#[tokio::test]
async fn in_memory_connection_dispatches_local_and_valid_nonce_requests() {
    let (temporary, store, _) = setup();
    let home = store.home.clone();
    drop(store);
    let (staging, fd) = stage(temporary.path());
    for token in [Some("private-test-nonce"), None] {
    let (mut client, server) = tokio::io::duplex(8192);
    let exchange = async {
        client
            .write_all(b"{\"v\":1,\"op\":\"domains\",\"auth_token\":\"private-test-nonce\"}\n")
            .await
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["ok"], true);
    };
    let (result, ()) = tokio::join!(
        connection(
            server,
            &home,
            &staging,
            &fd,
            token,
            Arc::new(tokio::sync::Mutex::new(()))
        ),
        exchange
    );
    result.unwrap();
    }
}
