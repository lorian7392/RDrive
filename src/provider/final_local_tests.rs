//! Local provider fallback/recovery paths; synthetic temporary state only.
use super::*;
use super::coverage_tests::setup;
use super::test_backend::Disk;

fn request(value: Value) -> Request { serde_json::from_value(value).unwrap() }
fn pending(response: Option<Value>, deletion_path: Option<&str>) -> Pending {
    serde_json::from_value(json!({"fingerprint":"fixture", "response":response, "deletion_path":deletion_path})).unwrap()
}
fn record(domain: &Domain, id: &str, path: &str, entry: Entry) -> Record {
    let mut item = root_item(domain);
    item.id = id.into(); item.name = path.rsplit('/').next().unwrap().into();
    item.kind = if entry.is_dir { "directory" } else { "file" }.into();
    item.size = entry.size; item.content_version = version(&entry).unwrap();
    item.metadata_version = metadata_version(path, &None, &BTreeMap::new(), false);
    Record { path:path.into(), item, entry:Some(entry.clone()), strong:Some(entry),
        client_id:None, metadata_known:false, metadata_checked:0 }
}
fn mutation(domain: &Domain, id: &str, identity: &str) -> Value {
    json!({"v":1,"op":"modify","domain":domain.id,"id":id,"request_id":identity,
        "base_content_version":"original-content","base_metadata_version":"original-metadata"})
}

#[test]
fn malformed_or_oversized_browse_hints_are_replaced_and_unknown_deletion_parents_fall_back_to_root() {
    let (_temp, store, mut domain) = setup(); domain.smart = true;
    checked_journal(&store.home, &store.config, &domain).unwrap();
    request_index_demand(&store.home, &store.config, &domain, "root", false).unwrap();
    let file = demand_directory(&store.home, &domain.id).join(format!("directory-{}.json", state::key("root")));
    for bytes in [b"invalid JSON".to_vec(), vec![b'x'; 16*1024+1]] {
        fs::write(&file, bytes).unwrap();
        request_index_demand(&store.home, &store.config, &domain, "root", false).unwrap();
        let repaired: CatalogDemand = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(repaired.id, "root"); assert!(repaired.path.is_empty());
        assert_eq!(repaired.binding, journal_binding(&store.config, &domain).unwrap());
    }
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    journal.requests.insert(format!("{}:deleted", state::key(&domain.id)), pending(None, Some("unknown-parent/deleted")));
    assert_eq!(request_parent(&journal, &domain, &request(json!({"v":1,"op":"delete","request_id":"deleted"}))).unwrap(), "root");
    for value in [json!({"v":1,"op":"delete"}), json!({"v":1,"op":"delete","request_id":"missing"}), json!({"v":1,"op":"fetch"})] {
        assert_eq!(request_parent(&journal, &domain, &request(value)).unwrap(), "root");
    }
}

#[test]
fn replay_does_not_adopt_an_unrelated_current_revision_or_discard_unconfirmed_provider_payloads() {
    let (temp, store, mut domain) = setup(); domain.smart = true;
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    let entry = super::coverage_tests::entry("file", false);
    let mut current = record(&domain, "file", "file", entry);
    current.item.content_version = "external-content".into(); current.item.metadata_version = "external-metadata".into();
    journal.items.insert("file".into(), current);
    save_journal(&store, &domain.id, &journal).unwrap();
    let mut queue = crate::smart::Queue::open(&store, &domain.id, &journal_binding(&store.config, &domain).unwrap()).unwrap();
    let source = temp.path().join("source"); fs::write(&source, b"retained content").unwrap();
    let intent = queue.enqueue(mutation(&domain, "file", "modify"), Some(&File::open(&source).unwrap())).unwrap();
    let (replayed, receipt) = chained_request(&store, &domain, &queue, &intent).unwrap();
    assert!(receipt.is_none());
    assert_eq!(replayed.base_content_version.as_deref(), Some("original-content"));
    assert_eq!(replayed.base_metadata_version.as_deref(), Some("original-metadata"));
    assert_eq!(queue.get(&intent.id).unwrap().request, intent.request);
    // An item missing from the local projection also cannot supply a newer
    // base. Keep the original intent for the ordinary fresh-scan/CAS check.
    journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    journal.items.clear(); journal.smart_replays.clear();
    save_journal(&store, &domain.id, &journal).unwrap();
    let (missing_item, receipt) = chained_request(&store, &domain, &queue, &intent).unwrap();
    assert!(receipt.is_none());
    assert_eq!(missing_item.base_content_version.as_deref(), Some("original-content"));
    assert_eq!(missing_item.base_metadata_version.as_deref(), Some("original-metadata"));
    journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    let key = format!("{}:modify", state::key(&domain.id));
    journal.requests.insert(key.clone(), pending(None, None));
    save_journal(&store, &domain.id, &journal).unwrap();
    let payloads = ["content", "empty"].map(|prefix| store.home.join("provider").join(format!("{prefix}-{}", state::key(&key))));
    for file in &payloads { fs::write(file, b"unconfirmed provider payload").unwrap(); }
    queue.start(&intent.id).unwrap(); queue.complete(&intent.id, json!({"receipt":"confirmed queue operation"})).unwrap();
    let home = store.home.clone(); drop(store);
    release_confirmed_payload(&home, &domain, &mut queue, &intent);
    for file in &payloads { assert_eq!(fs::read(file).unwrap(), b"unconfirmed provider payload"); }
    let store = Store::open(home.clone()).unwrap();
    journal = checked_journal(&home, &store.config, &domain).unwrap();
    journal.requests.get_mut(&key).unwrap().response = Some(json!({"receipt":"confirmed provider publication"}));
    save_journal(&store, &domain.id, &journal).unwrap(); drop(store);
    release_confirmed_payload(&home, &domain, &mut queue, &intent);
    assert!(payloads.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn first_managed_smart_catalog_read_initializes_local_sqlite_without_remote_access() {
    let (temp, store, mut domain) = setup(); domain.smart = true; domain.repository = true;
    add_domain(&store, domain.clone()).unwrap(); let home = store.home.clone(); drop(store);
    let stage = private(&temp.path().join("stage")).unwrap(); let fd = open_staging(&stage).unwrap();
    assert!(!index::path(&home, &domain.id).exists());
    let result = process(&home, &stage, &fd, request(json!({"v":1,"op":"changes","domain":domain.id,"anchor":"0"})), Default::default()).await.unwrap();
    assert!(index::path(&home, &domain.id).is_file());
    assert_eq!(result["items"], json!([])); assert_eq!(result["deleted"], json!([]));
    assert_eq!(result["anchor"], "0"); assert!(fs::read_dir(stage).unwrap().next().is_none());
}

#[tokio::test]
async fn retained_local_edits_block_fetch_before_any_credentials_or_network_access() {
    let (temp, store, mut domain) = setup(); domain.smart = true;
    add_domain(&store, domain.clone()).unwrap();
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    journal.items.insert("file".into(), record(&domain, "file", "file", super::coverage_tests::entry("file", false)));
    save_journal(&store, &domain.id, &journal).unwrap();
    let mut queue = crate::smart::Queue::open(&store, &domain.id, &journal_binding(&store.config, &domain).unwrap()).unwrap();
    let intent = queue.enqueue(mutation(&domain, "file", "pending"), None).unwrap();
    let home = store.home.clone(); drop(store);
    let stage = private(&temp.path().join("stage")).unwrap(); let fd = open_staging(&stage).unwrap();
    let error = process(&home, &stage, &fd, request(json!({"v":1,"op":"fetch","domain":domain.id,"id":"file"})), Default::default()).await.unwrap_err();
    assert!(error.to_string().contains("retained local edit"));
    assert_eq!(queue.get(&intent.id).unwrap().state, crate::smart::MutationState::Pending);
    assert!(fs::read_dir(stage).unwrap().next().is_none());
}

#[tokio::test]
async fn missing_items_and_stale_cached_revisions_fall_back_without_delivering_wrong_bytes() {
    let (temp, mut store, mut domain) = setup(); domain.smart = true;
    let missing = format!("RDRIVE_UNSET_PROVIDER_{}", uuid::Uuid::new_v4().simple());
    assert!(std::env::var_os(&missing).is_none());
    store.config.profiles.get_mut("p").unwrap().password_env = Some(missing.clone()); store.save_config().unwrap();
    add_domain(&store, domain.clone()).unwrap();
    let disk = Disk::new(temp.path().join("remote")); fs::write(disk.root.join("file"), b"cached data").unwrap();
    crate::cache::fetch(&mut store, disk.as_ref(), "p", "file", false).await.unwrap();
    let cached = disk.stat("file").await.unwrap().unwrap();
    let mut changed = cached.clone(); changed.etag = Some("\"external replacement\"".into());
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    journal.items.insert("file".into(), record(&domain, "file", "file", changed));
    save_journal(&store, &domain.id, &journal).unwrap();
    let home = store.home.clone(); drop(store);
    let stage = private(&temp.path().join("stage")).unwrap(); let fd = open_staging(&stage).unwrap();
    for id in [None, Some("missing"), Some("file")] {
        let error = process(&home, &stage, &fd, request(json!({"v":1,"op":"fetch","domain":domain.id,"id":id})), Default::default()).await.unwrap_err();
        assert!(error.to_string().contains(&missing), "must fail at the explicit missing credential boundary: {error:#}");
        assert!(fs::read_dir(&stage).unwrap().next().is_none(), "stale cache must never be delivered");
    }
    // Unknown operations must also pass through the explicit, isolated
    // credential boundary without returning cached data or mutating state.
    let error = process(&home, &stage, &fd, request(json!({"v":1,"op":"unsupported","domain":domain.id})), Default::default()).await.unwrap_err();
    assert!(error.to_string().contains(&missing));
    assert!(fs::read_dir(&stage).unwrap().next().is_none());
}

#[tokio::test]
async fn metadata_only_background_pass_preserves_directory_backoff_and_uses_bound_cached_backend() {
    let (temp, store, mut domain) = setup(); domain.smart = true;
    add_domain(&store, domain.clone()).unwrap();
    let disk = Disk::new(temp.path().join("remote")); fs::write(disk.root.join("file"), b"metadata target").unwrap();
    let entry = disk.stat("file").await.unwrap().unwrap();
    let mut journal = checked_journal(&store.home, &store.config, &domain).unwrap();
    journal.items.insert("file".into(), record(&domain, "file", "file", entry));
    journal.directory_attempts.insert("root".into(), ScanAttempt {last:1, failures:2, next_try:i64::MAX});
    save_journal(&store, &domain.id, &journal).unwrap();
    request_index_demand(&store.home, &store.config, &domain, "file", true).unwrap();
    let binding = journal_binding(&store.config, &domain).unwrap();
    let key = state::key(&serde_json::to_string(&(&binding, &store.config.profiles["p"])).unwrap());
    let home = store.home.clone(); drop(store);
    let stage = private(&temp.path().join("stage")).unwrap(); let fd = open_staging(&stage).unwrap();
    let mut backends: IndexBackends = BTreeMap::new(); backends.insert(domain.id.clone(), (key, disk));
    background_once(&home, &stage, &fd, &mut backends).await.unwrap();
    let store = Store::open(home.clone()).unwrap(); journal = checked_journal(&home, &store.config, &domain).unwrap();
    assert!(journal.items["file"].metadata_known); assert!(journal.items["file"].metadata_checked > 0);
    assert_eq!(journal.directory_attempts["root"].next_try, i64::MAX); assert_eq!(journal.directory_turn, 0);
    assert_eq!(journal.metadata_attempts["file"].failures, 0); assert!(backends.contains_key(&domain.id));
}

#[tokio::test]
async fn unknown_deletion_needs_a_complete_guarded_receipt_and_never_deletes_other_files() {
    use super::test_backend::Connected;
    use std::sync::atomic::Ordering;
    let (temp, store, domain) = setup(); add_domain(&store, domain.clone()).unwrap();
    let disk = Disk::new(temp.path().join("remote")); fs::write(disk.root.join("unrelated"), b"preserve unrelated bytes").unwrap();
    checked_journal(&store.home, &store.config, &domain).unwrap();
    let home = store.home.clone(); drop(store);
    let stage = private(&temp.path().join("stage")).unwrap(); let fd = open_staging(&stage).unwrap();
    let req = request(json!({"v":1,"op":"delete","domain":domain.id,"id":"unknown-id","request_id":"unknown-delete","base_content_version":"old-content","base_metadata_version":"old-metadata"}));
    let connector = Connected(disk.clone());
    let error = online_process_with(&home, &stage, &fd, req.clone(), Default::default(), &connector).await.unwrap_err();
    assert!(error.to_string().contains("no_such_item"));
    let store = Store::open(home.clone()).unwrap();
    let mut journal = checked_journal(&home, &store.config, &domain).unwrap();
    let mut incomplete = pending(None, Some("deleted"));
    let no_metadata: Option<String> = None;
    incomplete.fingerprint = state::key(&serde_json::to_string(&(&req.op, &req.domain, &req.id,
        &req.parent_id, &req.name, &req.kind, &req.base_content_version, &req.base_metadata_version,
        &req.tag_data_hex, &req.extended_attributes, &no_metadata, req.recursive)).unwrap());
    let key = format!("{}:unknown-delete", state::key(&domain.id));
    journal.requests.insert(key.clone(), incomplete); save_journal(&store, &domain.id, &journal).unwrap(); drop(store);
    let error = online_process_with(&home, &stage, &fd, req, Default::default(), &connector).await.unwrap_err();
    assert!(error.to_string().contains("no_such_item"));
    assert_eq!(fs::read(disk.root.join("unrelated")).unwrap(), b"preserve unrelated bytes");
    assert_eq!(disk.removals.load(Ordering::SeqCst), 0);
    let store = Store::open(home.clone()).unwrap();
    let journal = checked_journal(&home, &store.config, &domain).unwrap();
    assert!(journal.requests[&key].response.is_none(), "incomplete deletion proof must not create a success receipt");
}
