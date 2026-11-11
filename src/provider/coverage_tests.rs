//! Isolated catalog and namespace regressions; no system domains or credentials.
use super::*;
use async_trait::async_trait;
use std::sync::Mutex;

pub(super) struct Tree {
    pub(super) entries: Mutex<Vec<Entry>>,
    pub(super) unsafe_listing: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Backend for Tree {
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
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.path == path)
            .cloned())
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        if self
            .unsafe_listing
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(vec![entry("wrong/child", false)]);
        }
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.path.rsplit_once('/').map_or("", |pair| pair.0) == path)
            .cloned()
            .collect())
    }
    async fn download(&self, _: &str, _: &Path) -> Result<()> {
        bail!("fixture has no file data")
    }
    async fn upload(&self, _: &Path, _: &str, _: WriteCondition) -> Result<Entry> {
        bail!("fixture is read-only")
    }
    async fn mkdir(&self, _: &str) -> Result<()> {
        bail!("fixture is read-only")
    }
    async fn remove(&self, _: &str, _: bool) -> Result<()> {
        bail!("fixture is read-only")
    }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("fixture is read-only")
    }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("fixture is read-only")
    }
}
pub(super) fn setup() -> (tempfile::TempDir, Store, Domain) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().join("state")).unwrap();
    store.config.profiles.insert(
        "p".into(),
        config::Profile {
            protocol: config::Protocol::Webdav,
            url: "https://example.invalid/".into(),
            username: None,
            password_env: None,
            allow_http: false,
            timeout_secs: 1,
            smb_encryption: true,
            keychain_internet: false,
            nextcloud_upload_url: None,
        },
    );
    store.save_config().unwrap();
    let domain = Domain {
        id: "test-domain".into(),
        profile: "p".into(),
        remote: String::new(),
        display_name: "Test".into(),
        repository: false,
        smart: false,
        local_root: None,
    };
    (directory, store, domain)
}
pub(super) fn entry(path: &str, directory: bool) -> Entry {
    Entry {
        path: path.into(),
        is_dir: directory,
        size: if directory { 0 } else { 3 },
        modified: Some(1_700_000_000_000),
        etag: Some(format!("\"{path}\"")),
    }
}
fn request(value: Value) -> Request {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn full_catalog_refresh_preserves_identity_pins_and_emits_deleted_items() {
    let (_temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let tree = Tree {
        entries: Mutex::new(vec![
            entry("folder", true),
            entry("folder/file", false),
            entry(".rdrive-internal", false),
        ]),
        unsafe_listing: std::sync::atomic::AtomicBool::new(false),
    };
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    assert!(refresh(&store, &tree, &domain, &mut journal)
        .await
        .unwrap()
        .is_none());
    assert_eq!(journal.items.len(), 2);
    assert_eq!(journal.events.len(), 2);
    let id = journal
        .items
        .iter()
        .find(|(_, record)| record.path == "folder/file")
        .unwrap()
        .0
        .clone();
    let parent = journal.items[&id].item.parent_id.clone();
    assert_ne!(parent, "root");
    journal.pins.insert(id.clone());
    journal.items.get_mut(&id).unwrap().client_id = Some("stable-client".into());
    refresh(&store, &tree, &domain, &mut journal).await.unwrap();
    assert!(journal.items[&id].item.pinned);
    assert_eq!(
        journal.items[&id].client_id.as_deref(),
        Some("stable-client")
    );
    assert_eq!(
        request_parent(
            &journal,
            &domain,
            &request(json!({"v":1,"op":"modify","id":id}))
        )
        .unwrap(),
        parent
    );
    tree.entries.lock().unwrap().retain(|record| record.is_dir);
    refresh(&store, &tree, &domain, &mut journal).await.unwrap();
    assert!(!journal.items.contains_key(&id));
    assert_eq!(
        journal.events.last().unwrap().deleted.as_deref(),
        Some(id.as_str())
    );
    tree.unsafe_listing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(provider_scan(&tree, "")
        .await
        .unwrap_err()
        .to_string()
        .contains("unsafe provider child path"));
}

#[test]
fn catalog_root_working_set_and_retained_change_stream_have_consistent_versions() {
    let (_temporary, _store, domain) = setup();
    let mut journal = Journal::default();
    let mut item = root_item(&domain);
    item.id = "file".into();
    item.kind = "file".into();
    item.name = "file".into();
    journal.items.insert(
        "file".into(),
        Record {
            path: "file".into(),
            item: item.clone(),
            entry: None,
            strong: None,
            client_id: None,
            metadata_known: false,
            metadata_checked: 0,
        },
    );
    journal.pins.insert("root".into());
    assert_eq!(
        catalog(&domain, &journal, &request(json!({"v":1,"op":"item"}))).unwrap()["item"]
            ["sync_state"],
        "pinned"
    );
    assert_eq!(
        catalog(
            &domain,
            &journal,
            &request(json!({"v":1,"op":"item","id":"file"}))
        )
        .unwrap()["item"]["id"],
        "file"
    );
    assert!(catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"item","id":"missing"}))
    )
    .is_err());
    assert_eq!(
        catalog(
            &domain,
            &journal,
            &request(json!({"v":1,"op":"list","parent_id":"working_set"}))
        )
        .unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    push_event(&mut journal, Some(item.clone()), None);
    push_event(&mut journal, None, Some("file".into()));
    let changes = catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"0"})),
    )
    .unwrap();
    assert!(changes["items"].as_array().unwrap().is_empty());
    assert_eq!(changes["deleted"], json!(["file"]));
    push_event(&mut journal, Some(item.clone()), None);
    let changes = catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"0"})),
    )
    .unwrap();
    assert_eq!(changes["items"][0]["id"], "file");
    assert!(changes["deleted"].as_array().unwrap().is_empty());
    assert!(catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"current"}))
    )
    .unwrap()["items"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"99"}))
    )
    .is_err());
    assert!(catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"bad"}))
    )
    .is_err());
    assert!(catalog(&domain, &journal, &request(json!({"v":1,"op":"unknown"}))).is_err());
    for _ in 0..10_002 {
        push_event(&mut journal, Some(item.clone()), None);
    }
    assert_eq!(journal.events.len(), 10_000);
    assert!(catalog(
        &domain,
        &journal,
        &request(json!({"v":1,"op":"changes","anchor":"1"}))
    )
    .unwrap_err()
    .to_string()
    .contains("sync_anchor_expired"));
}

#[test]
fn requests_resolve_retained_delete_parents_and_validate_tags_and_extended_attrs() {
    let (_temporary, _store, domain) = setup();
    let mut journal = Journal::default();
    let mut folder = root_item(&domain);
    folder.id = "folder".into();
    folder.name = "folder".into();
    journal.items.insert(
        "folder".into(),
        Record {
            path: "folder".into(),
            item: folder,
            entry: None,
            strong: None,
            client_id: None,
            metadata_known: false,
            metadata_checked: 0,
        },
    );
    assert_eq!(
        request_parent(
            &journal,
            &domain,
            &request(json!({"v":1,"op":"create","parent_id":"folder"}))
        )
        .unwrap(),
        "folder"
    );
    assert_eq!(
        request_parent(
            &journal,
            &domain,
            &request(json!({"v":1,"op":"list","parent_id":"working_set"}))
        )
        .unwrap(),
        "root"
    );
    assert_eq!(
        request_parent(
            &journal,
            &domain,
            &request(json!({"v":1,"op":"list","parent_id":"folder"}))
        )
        .unwrap(),
        "folder"
    );
    for (id, path, expected) in [
        ("nested", "folder/deleted", "folder"),
        ("direct", "deleted", "root"),
        ("unknown", "gone/deleted", "root"),
    ] {
        journal.requests.insert(
            format!("{}:{id}", state::key(&domain.id)),
            serde_json::from_value(json!({"fingerprint":"test","deletion_path":path})).unwrap(),
        );
        assert_eq!(
            request_parent(
                &journal,
                &domain,
                &request(json!({"v":1,"op":"delete","request_id":id}))
            )
            .unwrap(),
            expected
        );
    }
    let metadata = crate::metadata::MetadataRecord {
        version: 1,
        content_sha256: "0".repeat(64),
        attributes: BTreeMap::new(),
        selection: crate::metadata::MetadataPolicy::default().include,
        excluded: vec![],
    };
    for tags in ["0", "gg", &"0".repeat(128 * 1024 + 2)] {
        assert!(apply_tags(metadata.clone(), Some(tags)).is_err());
    }
    let attrs = BTreeMap::from([("user.example#S".into(), "00".into())]);
    assert_eq!(
        apply_xattrs(metadata.clone(), Some(&attrs))
            .unwrap()
            .attributes,
        attrs
    );
    for name in [
        "user.example",
        "security.example#S",
        crate::metadata::FINDER_TAGS,
    ] {
        assert!(apply_xattrs(
            metadata.clone(),
            Some(&BTreeMap::from([(name.into(), "00".into())]))
        )
        .is_err());
    }
}

#[test]
fn domain_registration_is_local_validates_roots_and_removes_only_requested_domain() {
    use std::os::unix::fs::symlink;
    let (temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    assert!(add_domain(&store, domain.clone()).is_err());
    for id in ["", "bad\n", &"x".repeat(121)] {
        let mut invalid = domain.clone();
        invalid.id = id.into();
        assert!(add_domain(&store, invalid).is_err());
    }
    let mut invalid = domain.clone();
    invalid.id = "other".into();
    invalid.display_name.clear();
    assert!(add_domain(&store, invalid).is_err());
    let mut invalid = domain.clone();
    invalid.id = "other".into();
    invalid.profile = "unknown".into();
    assert!(add_domain(&store, invalid).is_err());
    let mut invalid = domain.clone();
    invalid.id = "other".into();
    invalid.remote = "../unsafe".into();
    assert!(add_domain(&store, invalid).is_err());
    let mut invalid = domain.clone();
    invalid.id = "other".into();
    invalid.local_root = Some("relative".into());
    assert!(add_domain(&store, invalid).is_err());
    let alias = temporary.path().join("alias");
    symlink(&store.home, &alias).unwrap();
    let mut invalid = domain.clone();
    invalid.id = "other".into();
    invalid.local_root = Some(alias);
    assert!(add_domain(&store, invalid).is_err());
    let file_root = temporary.path().join("regular-file");
    fs::write(&file_root, b"not a Finder root").unwrap();
    let mut invalid = domain.clone();
    invalid.id = "file-root".into();
    invalid.local_root = Some(file_root);
    assert!(add_domain(&store, invalid).is_err());
    remove_domain(&store, &domain.id).unwrap();
    assert!(domains(&store.home)
        .unwrap()
        .iter()
        .all(|item| item.id != domain.id));
    assert!(remove_domain(&store, &domain.id).is_err());
}

#[test]
fn native_cache_chunks_and_accounting_validate_identity_and_merge_only_bound_versions() {
    let (_temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let base = json!({"v":1,"op":"cache_budget","domain":domain.id,"native_accounting_id":format!("fileprovider:{}",domain.id),"native_used_bytes":3,"native_reserved_bytes":2,"native_pinned_bytes":1,"native_cached_versions":{"one":"v1"}});
    let response = quick(&store.home, &request(base.clone())).unwrap();
    assert_eq!(response["used_bytes"], 3);
    assert_eq!(response["reserved_bytes"], 2);
    let mut append = base.clone();
    append["native_cached_versions_reset"] = json!(false);
    append["native_cached_versions"] = json!({"two":"v2"});
    quick(&store.home, &request(append)).unwrap();
    assert_eq!(
        read_native_cached(&store.home, &store.config, &domain).unwrap(),
        BTreeMap::from([("one".into(), "v1".into()), ("two".into(), "v2".into())])
    );
    let mut invalid = base.clone();
    invalid["native_accounting_id"] = json!("another-domain");
    assert!(quick(&store.home, &request(invalid)).is_err());
    let mut invalid = base.clone();
    invalid["native_cached_versions"] = json!({"bad\n":"v"});
    assert!(quick(&store.home, &request(invalid)).is_err());
    let mut invalid = base.clone();
    invalid["native_cached_versions"] = json!({"id":"v".repeat(129)});
    assert!(quick(&store.home, &request(invalid)).is_err());
    let mut invalid = base.clone();
    invalid["native_cached_versions"] = serde_json::to_value(
        (0..513)
            .map(|i| (i.to_string(), "v"))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap();
    assert!(quick(&store.home, &request(invalid)).is_err());
    let cleared = request(
        json!({"v":1,"op":"cache_budget","domain":"unregistered","native_accounting_id":"fileprovider:unregistered","native_used_bytes":0,"native_confirmed_unregistered":true}),
    );
    assert!(quick(&store.home, &cleared).is_ok());
    let unconfirmed = request(
        json!({"v":1,"op":"cache_budget","domain":"unregistered","native_accounting_id":"fileprovider:unregistered","native_used_bytes":0}),
    );
    assert!(quick(&store.home, &unconfirmed).is_err());
}

#[test]
fn native_attention_is_bound_to_current_content_and_clears_without_network() {
    let (_temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    let mut item = root_item(&domain);
    item.id = "file".into();
    item.kind = "file".into();
    item.name = "file".into();
    item.content_version = "current".into();
    journal.items.insert(
        "file".into(),
        Record {
            path: "file".into(),
            item,
            entry: None,
            strong: None,
            client_id: None,
            metadata_known: false,
            metadata_checked: 0,
        },
    );
    save_journal(&store, &domain.id, &journal).unwrap();
    let home = store.home.clone();
    drop(store);
    let base = json!({"v":1,"op":"native_error","domain":domain.id,"id":"file","base_content_version":"current","native_message":"Synthetic failed download"});
    let mut stale = base.clone();
    stale["base_content_version"] = json!("old");
    assert!(local_native_error(&home, &request(stale)).is_err());
    for message in ["bad\n".to_owned(), "x".repeat(1025)] {
        let mut invalid = base.clone();
        invalid["native_message"] = json!(message);
        assert!(local_native_error(&home, &request(invalid)).is_err());
    }
    assert_eq!(
        local_native_error(&home, &request(base)).unwrap()["item"]["sync_state"],
        "error"
    );
    let clear = request(json!({"v":1,"op":"native_error","domain":domain.id,"id":"file"}));
    assert_eq!(
        local_native_error(&home, &clear).unwrap()["item"]["sync_state"],
        "online"
    );
}

#[tokio::test]
async fn raw_delete_receipt_recovery_never_repeats_deletion_against_a_replacement() {
    use super::test_backend::{Connected, Disk};
    for replacement in [false, true] {
        let (temporary, store, domain) = setup();
        add_domain(&store, domain.clone()).unwrap();
        let disk = Disk::new(temporary.path().join("remote"));
        fs::write(disk.root.join("file"), b"original").unwrap();
        let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
        refresh(&store, disk.as_ref(), &domain, &mut journal)
            .await
            .unwrap();
        let item = journal
            .items
            .values()
            .find(|record| record.path == "file")
            .unwrap()
            .item
            .clone();
        let stage = temporary.path().join("stage");
        private(&stage).unwrap();
        let fd = open_staging(&stage).unwrap();
        let req = request(
            json!({"v":1,"op":"delete","domain":domain.id,"id":item.id,"request_id":"delete-receipt","base_content_version":item.content_version,"base_metadata_version":item.metadata_version}),
        );
        let home = store.home.clone();
        drop(store);
        let connector = Connected(disk.clone());
        online_process_with(
            &home,
            &stage,
            &fd,
            req.clone(),
            Default::default(),
            &connector,
        )
        .await
        .unwrap();
        assert_eq!(disk.removals.load(std::sync::atomic::Ordering::SeqCst), 1);
        let store = Store::open(home.clone()).unwrap();
        let mut journal = checked_journal(&home, &store.config, &domain).unwrap();
        let key = format!("{}:delete-receipt", state::key(&domain.id));
        journal.requests.get_mut(&key).unwrap().response = None;
        save_journal(&store, &domain.id, &journal).unwrap();
        drop(store);
        if replacement {
            fs::write(disk.root.join("file"), b"new owner").unwrap();
        }
        let replay =
            online_process_with(&home, &stage, &fd, req, Default::default(), &connector).await;
        if replacement {
            assert!(replay.unwrap_err().to_string().contains("replacement"));
            assert_eq!(fs::read(disk.root.join("file")).unwrap(), b"new owner");
        } else {
            replay.unwrap();
            assert!(!disk.root.join("file").exists());
        }
        assert_eq!(
            disk.removals.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an uncertain delete must never be blindly repeated"
        );
    }
}

#[tokio::test]
async fn empty_raw_file_creation_and_metadata_only_save_keep_content_and_safe_fetch() {
    use super::test_backend::{Connected, Disk};
    let (temporary, store, domain) = setup();
    add_domain(&store, domain.clone()).unwrap();
    let home = store.home.clone();
    drop(store);
    let disk = Disk::new(temporary.path().join("remote"));
    let connector = Connected(disk.clone());
    let stage = temporary.path().join("stage");
    private(&stage).unwrap();
    let fd = open_staging(&stage).unwrap();
    let created=online_process_with(&home,&stage,&fd,request(json!({"v":1,"op":"create","domain":domain.id,"parent_id":"root","name":"empty","kind":"file","client_item_id":"empty-create"})),Default::default(),&connector).await.unwrap()["item"].clone();
    assert_eq!(fs::read(disk.root.join("empty")).unwrap(), b"");
    let modified=online_process_with(&home,&stage,&fd,request(json!({"v":1,"op":"modify","domain":domain.id,"id":created["id"],"request_id":"metadata-only","base_content_version":created["content_version"],"base_metadata_version":created["metadata_version"],"extended_attributes":{"user.example#S":"01"}})),Default::default(),&connector).await.unwrap()["item"].clone();
    assert_eq!(modified["id"], created["id"]);
    assert_eq!(modified["extended_attributes"]["user.example#S"], "01");
    assert_eq!(fs::read(disk.root.join("empty")).unwrap(), b"");
    let fetched=online_process_with(&home,&stage,&fd,request(json!({"v":1,"op":"fetch","domain":domain.id,"id":modified["id"],"base_content_version":modified["content_version"]})),Default::default(),&connector).await.unwrap();
    assert_eq!(
        fs::read(fetched["content_path"].as_str().unwrap()).unwrap(),
        b""
    );
}

#[tokio::test]
async fn managed_smart_background_refresh_uses_bound_connection_cache_and_retains_index_on_error() {
    use super::test_backend::Disk;
    let (temporary, store, mut domain) = setup();
    domain.smart = true;
    domain.repository = true;
    add_domain(&store, domain.clone()).unwrap();
    let disk = Disk::new(temporary.path().join("remote"));
    repository::init(&store, disk.as_ref(), "").await.unwrap();
    let local = temporary.path().join("content");
    fs::write(&local, b"managed content").unwrap();
    let tx = repository::begin(&store, disk.as_ref(), "p", "")
        .await
        .unwrap();
    repository::put(&store, &tx.id, &local, "file").unwrap();
    repository::commit(&store, disk.as_ref(), &tx.id)
        .await
        .unwrap();
    let profile = store.config.profiles["p"].clone();
    let binding = journal_binding(&store.config, &domain).unwrap();
    let key = state::key(&serde_json::to_string(&(&binding, &profile)).unwrap());
    let home = store.home.clone();
    drop(store);
    let stage = temporary.path().join("stage");
    private(&stage).unwrap();
    let fd = open_staging(&stage).unwrap();
    let mut backends: IndexBackends = BTreeMap::new();
    backends.insert(domain.id.clone(), (key.clone(), disk.clone()));
    background_once(&home, &stage, &fd, &mut backends)
        .await
        .unwrap();
    let store = Store::open(home.clone()).unwrap();
    let mut journal = checked_journal(&home, &store.config, &domain).unwrap();
    assert_eq!(journal.items.len(), 1);
    assert!(journal.directory_scans["root"] > 0);
    let scanned = journal.directory_scans["root"];
    drop(store);
    background_once(&home, &stage, &fd, &mut backends)
        .await
        .unwrap();
    let store = Store::open(home.clone()).unwrap();
    journal = checked_journal(&home, &store.config, &domain).unwrap();
    assert_eq!(journal.directory_scans["root"], scanned);
    let format = disk.root.join(".rdrive-repository/FORMAT.json");
    let original = fs::read(&format).unwrap();
    fs::write(&format, b"invalid format").unwrap();
    journal.directory_scans.insert("root".into(), 0);
    journal.directory_attempts.get_mut("root").unwrap().next_try = 0;
    save_journal(&store, &domain.id, &journal).unwrap();
    drop(store);
    background_once(&home, &stage, &fd, &mut backends)
        .await
        .unwrap();
    assert!(!backends.contains_key(&domain.id));
    let store = Store::open(home.clone()).unwrap();
    journal = checked_journal(&home, &store.config, &domain).unwrap();
    assert_eq!(journal.items.len(), 1);
    assert_eq!(journal.directory_attempts["root"].failures, 1);
    journal.directory_attempts.get_mut("root").unwrap().next_try = 0;
    save_journal(&store, &domain.id, &journal).unwrap();
    drop(store);
    fs::write(format, original).unwrap();
    backends.insert(domain.id.clone(), (key, disk));
    background_once(&home, &stage, &fd, &mut backends)
        .await
        .unwrap();
    let store = Store::open(home.clone()).unwrap();
    assert_eq!(
        checked_journal(&home, &store.config, &domain)
            .unwrap()
            .directory_attempts["root"]
            .failures,
        0
    );
}

fn namespace_record(d: &Domain, entry: Entry) -> Record {
    let mut item = root_item(d);
    item.id = "source".into();
    item.parent_id = "root".into();
    item.name = entry.path.clone();
    item.kind = if entry.is_dir { "directory" } else { "file" }.into();
    item.content_version = version(&entry).unwrap();
    Record {
        path: entry.path.clone(),
        item,
        entry: Some(entry.clone()),
        strong: (!entry.is_dir).then_some(entry),
        client_id: None,
        metadata_known: false,
        metadata_checked: 0,
    }
}

fn namespace_request(d: &Domain) -> Request {
    request(
        json!({"v":1,"op":"modify","domain":d.id,"id":"source","request_id":"namespace-fault","name":"moved"}),
    )
}

async fn namespace_call(
    store: Store,
    peer: &dyn Backend,
    d: &Domain,
    journal: &mut Journal,
    stage: &Path,
    fd: &File,
    req: &Request,
) -> Result<Value> {
    namespace::process(store, peer, d, journal, stage, fd, req, Default::default()).await
}

fn namespace_plan_update(journal: &mut Journal, d: &Domain, update: impl FnOnce(&mut Value)) {
    let key = format!("{}:namespace-fault", state::key(&d.id));
    let pending = journal.requests.get_mut(&key).unwrap();
    let mut value = serde_json::to_value(pending.namespace.as_ref().unwrap()).unwrap();
    update(&mut value);
    pending.namespace = Some(serde_json::from_value(value).unwrap());
}

struct FaultDisk {
    disk: Arc<super::test_backend::Disk>,
    mode: &'static str,
    observed: std::sync::atomic::AtomicBool,
    renames: std::sync::atomic::AtomicUsize,
}
impl FaultDisk {
    fn new(disk: Arc<super::test_backend::Disk>, mode: &'static str) -> Arc<Self> {
        Arc::new(Self {
            disk,
            mode,
            observed: std::sync::atomic::AtomicBool::new(false),
            renames: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl Backend for FaultDisk {
    fn capabilities(&self) -> backend::Capabilities {
        let mut caps = self.disk.capabilities();
        if self.mode == "weak" {
            caps.protocol = "webdav";
        }
        caps
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let mut result = self.disk.stat(path).await?;
        if path == "file" && ["metadata-race", "weak"].contains(&self.mode) {
            self.observed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if self.mode == "weak" {
                if let Some(entry) = &mut result {
                    entry.etag = Some("W/\"weak\"".into());
                }
            }
        }
        Ok(result)
    }
    async fn stat_metadata(&self, path: &str) -> Result<Option<Entry>> {
        let mut result = self.disk.stat(path).await?;
        if (self.mode == "metadata-race"
            && path == "file"
            && self.observed.load(std::sync::atomic::Ordering::SeqCst))
            || (self.mode == "catalog-race" && path == "moved")
        {
            if let Some(entry) = &mut result {
                entry.size += 1;
            }
        }
        Ok(result)
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        self.disk.list(path).await
    }
    async fn download(&self, path: &str, local: &Path) -> Result<()> {
        self.disk.download(path, local).await
    }
    async fn upload(&self, local: &Path, path: &str, condition: WriteCondition) -> Result<Entry> {
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
    async fn rename_checked(&self, from: &str, to: &str, expected: &Entry) -> Result<Entry> {
        let call = self
            .renames
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.mode == "stop" || (self.mode == "restore-fails" && call > 0) {
            bail!("synthetic guarded rename unavailable");
        }
        let actual = self
            .disk
            .stat(from)
            .await?
            .context("fixture move source missing")?;
        if !actual.same_revision(expected) {
            bail!("fixture guarded rename conflict");
        }
        self.disk.rename(from, to, false).await?;
        let result = self.disk.stat(to).await?.unwrap();
        if call == 0 && ["restore-fails", "vanished", "restore-succeeds"].contains(&self.mode) {
            if self.mode == "vanished" {
                fs::remove_file(self.disk.root.join(to))?;
            } else {
                fs::write(self.disk.root.join(to), b"external changed body")?;
            }
        }
        Ok(result)
    }
}
struct FaultConnection {
    peer: Arc<dyn Backend>,
    remove_snapshot: Option<PathBuf>,
}
#[async_trait]
impl Connector for FaultConnection {
    async fn connect(
        &self,
        _: &config::Profile,
        _: &str,
        _: crate::transfer::TransferContext,
    ) -> Result<Arc<dyn Backend>> {
        if let Some(path) = &self.remove_snapshot {
            fs::remove_file(path)?;
        }
        Ok(self.peer.clone())
    }
}

#[tokio::test]
async fn namespace_preparation_retains_invalid_filtered_colliding_and_unindexed_subtrees() {
    for case in [
        "invalid-parent",
        "strong-change",
        "collision",
        "unindexed",
        "filtered-child",
    ] {
        let (temporary, mut store, d) = setup();
        let mut journal = checked_journal(&store.home, &store.config, &d).unwrap();
        let directory = case != "strong-change";
        let original = entry("folder", directory);
        journal
            .items
            .insert("source".into(), namespace_record(&d, original.clone()));
        let tree = Tree {
            entries: Mutex::new(vec![original, entry("folder/sub", false)]),
            unsafe_listing: std::sync::atomic::AtomicBool::new(false),
        };
        let mut req = namespace_request(&d);
        let expected = match case {
            "invalid-parent" => {
                req.parent_id = Some("absent".into());
                "parent missing"
            }
            "strong-change" => {
                journal
                    .items
                    .get_mut("source")
                    .unwrap()
                    .strong
                    .as_mut()
                    .unwrap()
                    .etag = Some("\"other-content\"".into());
                "content changed since its verified snapshot"
            }
            "collision" => {
                tree.entries
                    .lock()
                    .unwrap()
                    .extend([entry("folder/A", false), entry("folder/a", false)]);
                "collision"
            }
            "unindexed" => {
                req.op = "delete".into();
                req.recursive = true;
                "unindexed new child"
            }
            _ => {
                store.config.filters.exclude.push("moved/sub".into());
                store.save_config().unwrap();
                "filters exclude moved/sub"
            }
        };
        let stage = temporary.path().canonicalize().unwrap().join("stage");
        private(&stage).unwrap();
        let fd = open_staging(&stage).unwrap();
        let error = namespace_call(store, &tree, &d, &mut journal, &stage, &fd, &req)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{case}: {error:#}");
        assert!(journal.requests.is_empty());
        assert_eq!(journal.items["source"].path, "folder");
    }
}

#[tokio::test]
async fn pending_namespace_plans_validate_identity_filters_receipts_and_source_ownership() {
    for case in [
        "identity",
        "receipt",
        "restored",
        "filtered",
        "occupied",
        "source-changed",
        "rename-error",
    ] {
        let (temporary, store, d) = setup();
        let disk = super::test_backend::Disk::new(temporary.path().join("remote"));
        fs::write(disk.root.join("file"), b"abc").unwrap();
        let peer = FaultDisk::new(disk.clone(), "stop");
        let mut journal = checked_journal(&store.home, &store.config, &d).unwrap();
        journal.items.insert(
            "source".into(),
            namespace_record(&d, disk.stat("file").await.unwrap().unwrap()),
        );
        let stage = temporary.path().canonicalize().unwrap().join("stage");
        private(&stage).unwrap();
        let fd = open_staging(&stage).unwrap();
        let mut req = namespace_request(&d);
        let home = store.home.clone();
        let first = namespace_call(store, peer.as_ref(), &d, &mut journal, &stage, &fd, &req)
            .await
            .unwrap_err();
        assert!(first.to_string().contains("synthetic guarded rename"));
        let mut store = Store::open(home).unwrap();
        let key = format!("{}:namespace-fault", state::key(&d.id));
        let expected = match case {
            "identity" => {
                req.name = Some("different".into());
                "request identity was reused"
            }
            "receipt" => {
                journal.requests.get_mut(&key).unwrap().response =
                    Some(json!({"confirmed":"saved"}));
                ""
            }
            "restored" => {
                namespace_plan_update(&mut journal, &d, |value| value["phase"] = json!("restored"));
                "publication was restored"
            }
            "filtered" => {
                store.config.filters.exclude.push("moved".into());
                store.save_config().unwrap();
                "filters exclude moved"
            }
            "occupied" => {
                fs::write(disk.root.join("moved"), b"another owner's file").unwrap();
                "destination was occupied"
            }
            "source-changed" => {
                fs::write(disk.root.join("file"), b"new source").unwrap();
                "source changed or disappeared"
            }
            _ => "synthetic guarded rename",
        };
        let result =
            namespace_call(store, peer.as_ref(), &d, &mut journal, &stage, &fd, &req).await;
        if case == "receipt" {
            assert_eq!(result.unwrap(), json!({"confirmed":"saved"}));
        } else {
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{case}: {error:#}");
        }
        assert!(disk.root.join("file").exists());
        if case == "occupied" {
            assert_eq!(
                fs::read(disk.root.join("moved")).unwrap(),
                b"another owner's file"
            );
        }
    }
}

#[tokio::test]
async fn namespace_publication_keeps_recoverable_evidence_when_verification_or_catalog_changes() {
    for mode in [
        "restore-fails",
        "vanished",
        "restore-succeeds",
        "catalog-race",
    ] {
        let (temporary, store, d) = setup();
        let disk = super::test_backend::Disk::new(temporary.path().join("remote"));
        fs::write(disk.root.join("file"), b"abc").unwrap();
        let peer = FaultDisk::new(disk.clone(), mode);
        let mut journal = checked_journal(&store.home, &store.config, &d).unwrap();
        journal.items.insert(
            "source".into(),
            namespace_record(&d, disk.stat("file").await.unwrap().unwrap()),
        );
        let stage = temporary.path().canonicalize().unwrap().join("stage");
        private(&stage).unwrap();
        let fd = open_staging(&stage).unwrap();
        let req = namespace_request(&d);
        let error = namespace_call(store, peer.as_ref(), &d, &mut journal, &stage, &fd, &req)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(if mode == "catalog-race" {
                "moved catalog item changed"
            } else if mode == "vanished" {
                "source disappeared"
            } else {
                "moved source identity/content changed"
            }),
            "{mode}: {message}"
        );
        let recovery = namespace::recovery(&journal);
        assert_eq!(recovery.as_array().unwrap().len(), 1);
        assert!(!disk.root.join("file").exists() || mode == "restore-succeeds");
        if mode == "restore-succeeds" {
            assert_eq!(recovery[0]["phase"], "restored");
            assert_eq!(
                fs::read(disk.root.join("file")).unwrap(),
                b"external changed body"
            );
        }
        if mode == "restore-fails" {
            assert_eq!(
                fs::read(disk.root.join("moved")).unwrap(),
                b"external changed body"
            );
        }
    }
}

#[tokio::test]
async fn conflict_decisions_reject_invalid_locations_changed_observations_and_lost_local_snapshots()
{
    use crate::{
        policies::ConflictStrategy,
        smart::{FailureKind, Queue},
    };
    for mode in [
        "invalid-parent",
        "missing-origin",
        "metadata-race",
        "weak",
        "lost-snapshot",
        "managed",
        "managed-invalid",
        "missing-index",
    ] {
        let (temporary, store, mut d) = setup();
        d.smart = true;
        d.repository = mode.starts_with("managed");
        add_domain(&store, d.clone()).unwrap();
        let disk = super::test_backend::Disk::new(temporary.path().join("remote"));
        let source = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("retained-edit");
        fs::write(&source, b"retained local edit").unwrap();
        if mode == "managed" {
            repository::init(&store, disk.as_ref(), "").await.unwrap();
            let tx = repository::begin(&store, disk.as_ref(), "p", "")
                .await
                .unwrap();
            repository::put(&store, &tx.id, &source, "file").unwrap();
            repository::commit(&store, disk.as_ref(), &tx.id)
                .await
                .unwrap();
        } else if mode != "managed-invalid" {
            fs::write(disk.root.join("file"), b"remote original").unwrap();
        }
        let mut journal = checked_journal(&store.home, &store.config, &d).unwrap();
        if mode != "managed-invalid" {
            refresh(&store, disk.as_ref(), &d, &mut journal)
                .await
                .unwrap();
        }
        let mut item = journal
            .items
            .values()
            .find(|record| record.path == "file")
            .map(|record| record.item.clone())
            .unwrap_or_else(|| {
                let mut item = root_item(&d);
                item.id = "absent-file".into();
                item.kind = "file".into();
                item.name = "file".into();
                item
            });
        if mode == "invalid-parent" {
            journal.items.get_mut(&item.id).unwrap().item.parent_id = "absent-parent".into();
            item.parent_id = "absent-parent".into();
        }
        save_journal(&store, &d.id, &journal).unwrap();
        let mut queue =
            Queue::open(&store, &d.id, &journal_binding(&store.config, &d).unwrap()).unwrap();
        let mutation=queue.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":item.id,"request_id":"conflicting-edit","base_content_version":item.content_version,"base_metadata_version":item.metadata_version,"name":"file"}),Some(&File::open(&source).unwrap())).unwrap();
        let mut failed = queue
            .fail(
                &mutation.id,
                FailureKind::Conflict,
                "synthetic external edit",
            )
            .unwrap();
        if mode == "missing-origin" {
            failed.request["op"] = json!("create");
            failed.request["parent_id"] = json!("absent-parent");
        }
        if mode == "managed-invalid" {
            failed.request["op"] = json!("create");
        }
        if mode == "missing-index" {
            failed.request["id"] = json!("no-longer-indexed");
        }
        let snapshot = store
            .home
            .join("smart")
            .join(state::key(&d.id))
            .join("dirty")
            .join(&failed.snapshot.as_ref().unwrap().file);
        let peer = FaultDisk::new(disk.clone(), mode);
        let connector = FaultConnection {
            peer,
            remove_snapshot: (mode == "lost-snapshot").then_some(snapshot.clone()),
        };
        let strategy = if ["missing-origin", "lost-snapshot", "managed-invalid"].contains(&mode) {
            ConflictStrategy::KeepBoth
        } else {
            ConflictStrategy::Replace
        };
        let result =
            resolution::plan_for_test(&store, &d, &mut queue, &failed, strategy, true, &connector)
                .await;
        if mode == "managed" || mode == "missing-index" {
            let plan = result.unwrap().resolution.unwrap();
            assert_eq!(plan.strategy, ConflictStrategy::Replace);
            if mode == "managed" {
                assert!(plan
                    .repository_snapshot
                    .as_ref()
                    .is_some_and(|hash| hash.len() == 64));
                assert!(plan.version.is_none());
            } else {
                assert!(plan.repository_snapshot.is_none());
                assert!(plan.version.is_some());
            }
            assert_eq!(plan.request["op"], "modify");
        } else {
            let error = result.unwrap_err();
            let message = format!("{error:#}");
            let expected = match mode {
                "invalid-parent" | "missing-origin" => "parent missing",
                "metadata-race" => "replace target changed while observing",
                "weak" => "strong guarded WebDAV revision",
                "lost-snapshot" => "No such file",
                _ => "repository",
            };
            assert!(message.contains(expected), "{mode}: {message}");
            assert!(queue.get(&failed.id).unwrap().resolution.is_none());
            if mode == "lost-snapshot" {
                assert!(!snapshot.exists());
            }
        }
        if !d.repository {
            assert_eq!(
                fs::read(disk.root.join("file")).unwrap(),
                b"remote original"
            );
        }
        assert_eq!(fs::read(&source).unwrap(), b"retained local edit");
    }
}

#[tokio::test]
async fn namespace_checkpoint_io_failures_retain_the_move_and_do_not_replay_publication() {
    for followup in [false, true] {
        let (temporary, store, d) = setup();
        let disk = super::test_backend::Disk::new(temporary.path().join("remote"));
        fs::write(disk.root.join("file"), b"abc").unwrap();
        let peer = FaultDisk::new(disk.clone(), "normal");
        let mut journal = checked_journal(&store.home, &store.config, &d).unwrap();
        journal.items.insert(
            "source".into(),
            namespace_record(&d, disk.stat("file").await.unwrap().unwrap()),
        );
        save_journal(&store, &d.id, &journal).unwrap();
        let db = index::path(&store.home, &d.id).canonicalize().unwrap();
        let sql = if followup {
            "CREATE TRIGGER fail_namespace_checkpoint BEFORE UPDATE OF control_json ON meta WHEN instr(NEW.control_json,'\"followup\":{')>0 BEGIN SELECT RAISE(FAIL,'synthetic namespace followup checkpoint failure'); END;"
        } else {
            "CREATE TRIGGER fail_namespace_checkpoint BEFORE UPDATE OF control_json ON meta WHEN instr(NEW.control_json,'\"phase\":\"moved\"')>0 BEGIN SELECT RAISE(FAIL,'synthetic namespace moved checkpoint failure'); END;"
        };
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch(sql).unwrap();
        drop(raw);
        let stage = temporary.path().canonicalize().unwrap().join("stage");
        private(&stage).unwrap();
        let fd = open_staging(&stage).unwrap();
        let mut req = namespace_request(&d);
        if followup {
            req.tag_data_hex = Some(String::new());
        }
        let home = store.home.clone();
        let error = namespace_call(store, peer.as_ref(), &d, &mut journal, &stage, &fd, &req)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(if followup {
                "synthetic namespace followup"
            } else {
                "synthetic namespace moved"
            }),
            "{error:#}"
        );
        assert_eq!(fs::read(disk.root.join("moved")).unwrap(), b"abc");
        assert!(!disk.root.join("file").exists());
        let raw = rusqlite::Connection::open(db).unwrap();
        raw.execute_batch("DROP TRIGGER fail_namespace_checkpoint")
            .unwrap();
        drop(raw);
        let store = Store::open(home).unwrap();
        let mut durable = checked_journal(&store.home, &store.config, &d).unwrap();
        assert_eq!(
            namespace::recovery(&durable)[0]["phase"],
            if followup { "moved" } else { "prepared" }
        );
        if !followup {
            let completed =
                namespace_call(store, peer.as_ref(), &d, &mut durable, &stage, &fd, &req)
                    .await
                    .unwrap();
            assert_eq!(completed["item"]["name"], "moved");
            assert!(namespace::recovery(&durable).as_array().unwrap().is_empty());
        } else {
            // The synthetic domain is deliberately not registered. Resume the
            // retained metadata followup twice: it must reuse the moved plan
            // and stop at domain validation before any connection is opened.
            let home = store.home.clone();
            drop(store);
            for _ in 0..2 {
                let store = Store::open(home.clone()).unwrap();
                let mut durable = checked_journal(&home, &store.config, &d).unwrap();
                let error =
                    namespace_call(store, peer.as_ref(), &d, &mut durable, &stage, &fd, &req)
                        .await
                        .unwrap_err();
                assert!(
                    error.to_string().contains("no_such_item: domain missing"),
                    "{error:#}"
                );
                assert_eq!(namespace::recovery(&durable)[0]["phase"], "moved");
                assert_eq!(fs::read(disk.root.join("moved")).unwrap(), b"abc");
            }
        }
        assert_eq!(peer.renames.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
