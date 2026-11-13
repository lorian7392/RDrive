use super::*;
use crate::{
    backend::Entry,
    config::Profile,
    state::{CacheItem, CacheReservation, Event, Job, Store},
};
use std::{
    fs::Permissions,
    os::unix::fs::{symlink, PermissionsExt},
};
const NOW: i64 = 1_800_000_000;
fn profile() -> Profile {
    Profile {
        protocol: crate::config::Protocol::Webdav,
        url: "https://example.invalid/dav/".into(),
        username: Some("user".into()),
        password_env: Some("STATUS_TEST_PASSWORD".into()),
        keychain_internet: false,
        allow_http: false,
        timeout_secs: 2,
        smb_encryption: true,
        nextcloud_upload_url: None,
    }
}
fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
        let mut cur = Some(parent);
        while let Some(p) = cur {
            if p == std::env::temp_dir() {
                break;
            }
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".tmp"))
            {
                break;
            }
            fs::set_permissions(p, Permissions::from_mode(0o700)).unwrap();
            cur = p.parent();
            if p == Path::new("/private/tmp") || p == Path::new("/tmp") {
                break;
            }
        }
    }
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
}
fn object(path: &Path, v: Value) {
    write(path, &serde_json::to_vec(&v).unwrap());
}
struct Fixture {
    _temporary: tempfile::TempDir,
    store: Store,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = Store::open(temporary.path().join("state")).unwrap();
        store.config.profiles.insert("a".into(), profile());
        let mut b = profile();
        b.url = "https://other.invalid/dav/".into();
        store.config.profiles.insert("b".into(), b);
        store.config.cache.max_bytes = Some(20);
        store.save_config().unwrap();
        Self {
            _temporary: temporary,
            store,
        }
    }
    fn home(&self) -> &Path {
        &self.store.home
    }
    fn path(&self, name: &str) -> PathBuf {
        self.home().join(name)
    }
    fn domain(&self, name: &str, profile: &str) -> Domain {
        Domain {
            id: name.into(),
            profile: profile.into(),
            remote: String::new(),
            display_name: name.into(),
            repository: false,
            local_root: None,
            smart: true,
        }
    }
    fn db(&mut self) {
        self.store.save().unwrap();
    }
    fn sqlite(&self, name: &str) -> Connection {
        write(&self.path(name), b"");
        let c = Connection::open(self.path(name)).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c
    }
    fn index(&self, d: &Domain) -> Connection {
        let c = self.sqlite(&format!("provider/{}.sqlite3", state::key(&d.id)));
        c.execute_batch("PRAGMA application_id=1380206936; PRAGMA user_version=2; CREATE TABLE meta(id INTEGER PRIMARY KEY,schema_version INTEGER,domain_id TEXT,binding TEXT,anchor INTEGER); CREATE TABLE items(id TEXT PRIMARY KEY,item_json TEXT,path TEXT); CREATE TABLE directory_scans(id TEXT PRIMARY KEY,scanned_at INTEGER);").unwrap();
        c.execute(
            "INSERT INTO meta VALUES(1,2,?1,?2,9)",
            (
                &d.id,
                crate::provider::journal_binding(&self.store.config, d).unwrap(),
            ),
        )
        .unwrap();
        c.execute(
            "INSERT INTO items VALUES('file','{\"kind\":\"file\"}','documents/file.txt')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO items VALUES('dir','{\"kind\":\"directory\"}','documents')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO directory_scans VALUES('root',1000)", [])
            .unwrap();
        c
    }
    fn history(&self) -> Connection {
        let c = self.sqlite("history/history.sqlite3");
        c.execute_batch(
            "CREATE TABLE entries(binding TEXT,created TEXT,state TEXT,bytes INTEGER);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO entries VALUES(?1,'2027-01-15T00:00:00Z','prepared',17)",
            [crate::history::status_binding(&self.store.config, "a").unwrap()],
        )
        .unwrap();
        c
    }
}
fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn add(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.insert(p.strip_prefix(root).unwrap().to_path_buf(), vec![]);
                add(root, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(p).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    add(root, root, &mut out);
    out
}
fn entry(size: u64) -> Entry {
    Entry {
        path: "file".into(),
        is_dir: false,
        size,
        modified: None,
        etag: Some("etag".into()),
    }
}
fn cache_item(config: &Config, profile: &str, object: &str, size: u64, pinned: bool) -> CacheItem {
    let p = &config.profiles[profile];
    CacheItem {
        profile: profile.into(),
        remote: "file".into(),
        object: object.into(),
        entry: entry(size),
        sha256: "a".repeat(64),
        pinned,
        accessed: "2027-01-15T00:00:00Z".into(),
        scope: Some(state::key(
            &serde_json::to_string(&(&p.protocol, &p.url, &p.username)).unwrap(),
        )),
    }
}
fn job(profile: &str, state: &str) -> Job {
    Job {
        id: format!("job-{profile}-{state}"),
        profile: profile.into(),
        remote: "file".into(),
        snapshot: "snapshot".into(),
        sha256: "a".repeat(64),
        size: 5,
        created: "now".into(),
        status: state.into(),
        attempts: 2,
        error: Some("version_conflict: Authorization: Bearer SECRET".into()),
        expected: None,
        overwrite: false,
        version: false,
        resume: false,
        version_prepared: false,
    }
}
#[test]
fn empty_home_is_not_initialized_and_invalid_filters_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("absent");
    let result = collect(&home, None).unwrap();
    assert_eq!(result["read_only"], true);
    assert_eq!(result["profiles"], 0);
    assert_eq!(result["service"]["status"], "missing");
    assert!(!home.exists());
    assert!(collect_with_limit(&home, None, 0).is_err());
    assert!(collect_with_limit(&home, None, 101).is_err());
    assert!(collect(&home, Some("unknown")).is_err());
}
#[test]
fn current_sqlite_wal_rows_are_visible_without_touching_live_files_or_locks() {
    let f = Fixture::new();
    let d = f.domain("smart-a", "a");
    object(&f.path("provider-domains.json"), json!([d]));
    let _index = f.index(&d);
    let _history = f.history();
    let before = inventory(f.home());
    let result = collect_at(f.home(), None, NOW, 10).unwrap();
    assert_eq!(result["index"]["data"]["domains"][0]["items"], 2);
    assert_eq!(result["index"]["data"]["domains"][0]["pending"], 1);
    assert_eq!(result["history"]["data"]["profiles"][0]["bytes"], 17);
    assert_eq!(result["history"]["data"]["profiles"][1]["entries"], 0);
    assert_eq!(inventory(f.home()), before);
    assert!(Store::open(f.home().to_path_buf()).is_err());
}
#[test]
fn cache_admission_accounts_partial_reservations_native_ttl_pins_leases_and_orphans() {
    let mut f = Fixture::new();
    let mut a = cache_item(&f.store.config, "a", "one", 4, true);
    f.store.db.cache.insert("one".into(), a.clone());
    a.profile = "b".into();
    a.object = "missing".into();
    a.pinned = false;
    f.store.db.cache.insert("missing".into(), a);
    let mut stale = cache_item(&f.store.config, "a", "stale", 7, false);
    stale.scope = None;
    f.store.db.cache.insert("stale".into(), stale);
    f.store.db.cache_reservations.insert(
        "partial".into(),
        CacheReservation {
            size: 10,
            created: "now".into(),
        },
    );
    write(&f.path("cache/one"), b"data");
    write(&f.path("cache/partial"), b"xx");
    write(&f.path("cache/orphan"), b"ooo");
    f.store.db.cache_garbage.insert("garbage".into());
    write(&f.path("cache/garbage"), b"g");
    f.db();
    object(
        &f.path("native-cache-usage.json"),
        json!({"version":1,"reports":{"current":{"used":5,"reserved":2,"pinned":1,"updated":NOW},"old":{"used":2,"reserved":90,"pinned":0,"updated":NOW-121}}}),
    );
    let lease = File::open(f.path("cache/one")).unwrap();
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) }, 0);
    let before = inventory(f.home());
    let result = collect_at(f.home(), Some("a"), NOW, 1).unwrap();
    let c = &result["cache"]["data"];
    assert_eq!(c["indexed_files"], 1);
    assert_eq!(c["unbound_entries"], 1);
    assert_eq!(c["used_bytes"], 10);
    assert_eq!(c["leased_bytes"], 4);
    assert_eq!(c["unmanaged_bytes"], 3);
    assert_eq!(c["reserved_bytes"], 8);
    assert_eq!(c["native_reserved_bytes"], 2);
    assert_eq!(c["accounted_bytes"], 27);
    assert_eq!(c["over_budget"], true);
    assert_eq!(result["profiles"], 1);
    assert_eq!(inventory(f.home()), before);
}
#[test]
fn missing_and_corrupt_sources_do_not_hide_healthy_components_or_leak_credentials() {
    let mut f = Fixture::new();
    write(&f.path("state.json"), b"{SECRET");
    let result = collect(f.home(), None).unwrap();
    assert_eq!(result["status"], "partial");
    assert_eq!(result["queue"]["status"], "error");
    assert_eq!(result["cache"]["status"], "error");
    assert_eq!(result["history"]["status"], "ok");
    assert!(!result.to_string().contains("SECRET"));
    f.db();
    object(
        &f.path("config.json"),
        json!({"profiles":{"invalid":{"protocol":"SECRET","url":"SECRET"}}}),
    );
    let result = collect(f.home(), None).unwrap();
    assert_eq!(result["configuration"]["status"], "error");
    assert_eq!(result["transfers"]["status"], "error");
    assert!(!result.to_string().contains("SECRET"));
}
#[test]
fn queue_and_download_state_are_bounded_and_profile_scoped() {
    let mut f = Fixture::new();
    f.store.db.jobs = vec![job("a", "running"), job("a", "failed"), job("b", "done")];
    f.db();
    object(
        &f.path("transfers/a.json"),
        json!({"id":"download-a","profile":"a","phase":"downloading","bytes":2,"expected":{"size":10},"destination":"SECRET"}),
    );
    object(
        &f.path("transfers/b.json"),
        json!({"id":"download-b","profile":"b","phase":"complete","bytes":5,"expected":{"size":5}}),
    );
    let r = collect_at(f.home(), Some("a"), NOW, 1).unwrap();
    assert_eq!(r["queued"], 2);
    assert_eq!(r["queue"]["data"]["examples"].as_array().unwrap().len(), 1);
    assert_eq!(r["transfers"]["data"]["total"], 1);
    assert_eq!(r["transfers"]["data"]["examples"][0]["verified_bytes"], 2);
    assert!(!r.to_string().contains("SECRET"));
}
#[test]
fn smart_conflicts_retain_running_state_and_resolve_only_bound_paths() {
    let f = Fixture::new();
    let d = f.domain("smart-a", "a");
    object(&f.path("provider-domains.json"), json!([d]));
    let _index = f.index(&d);
    let binding = crate::provider::journal_binding(&f.store.config, &d).unwrap();
    let mut q = crate::smart::Queue::open(&f.store, &d.id, &binding).unwrap();
    let source = f.path("source");
    write(&source, b"retained");
    let mutation=q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"file","name":"file.txt","request_id":"one","base_content_version":"c","base_metadata_version":"m"}),Some(&File::open(source).unwrap())).unwrap();
    q.fail(
        &mutation.id,
        crate::smart::FailureKind::Conflict,
        "version_conflict: Authorization: Bearer SECRET",
    )
    .unwrap();
    let m=q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"unknown","request_id":"two","base_content_version":"c","base_metadata_version":"m"}),None).unwrap();
    q.fail(
        &m.id,
        crate::smart::FailureKind::Offline,
        "server_unreachable: SECRET",
    )
    .unwrap();
    let active=q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"active","request_id":"three","base_content_version":"c","base_metadata_version":"m"}),None).unwrap();
    q.start(&active.id).unwrap();
    let before = inventory(f.home());
    let r = collect_at(f.home(), None, NOW, 10).unwrap();
    let c = &r["conflicts"]["data"];
    assert_eq!(c["domains"][0]["counts"]["running"], 1);
    assert_eq!(c["conflicts"], 1);
    assert_eq!(c["needs_attention"], 2);
    assert_eq!(c["examples"][0]["path"], "documents/file.txt");
    assert_eq!(c["examples"][0]["retained_content"], true);
    assert_eq!(c["examples"][1]["source_path_unknown"], true);
    assert!(!r.to_string().contains("SECRET"));
    assert_eq!(inventory(f.home()), before);
    drop(q);
}
#[test]
fn ordinary_sync_reports_are_latest_scoped_observations_not_live_conflict_assertions() {
    let mut f = Fixture::new();
    let binding = crate::connection_status::binding(&f.store.config.profiles["a"]).unwrap();
    let detail = |phase, conflicts| {
        json!({"version":1,"binding":binding,"local_root":"/tmp/work","remote_root":"work","phase":phase,"applied":false,"action_count":2,"conflict_count":conflicts,"metadata_conflict_count":null,"conflicts":[{"path":"file","reason":"SECRET"}]}).to_string()
    };
    let event = |profile: &str, detail: String| Event {
        time: "2027-01-15T00:00:00Z".into(),
        operation: "sync-status".into(),
        profile: profile.into(),
        path: "work".into(),
        detail,
    };
    f.store.db.history = vec![
        event("a", detail("content_plan", json!(2))),
        event("a", detail("repair", Value::Null)),
        event("b", detail("content_plan", json!(5))),
        event("a", "SECRET invalid".into()),
    ];
    f.db();
    let r = collect(f.home(), None).unwrap();
    let ordinary = &r["conflicts"]["data"]["ordinary_sync"];
    assert_eq!(ordinary["total_plans"], 1);
    assert_eq!(ordinary["plans"][0]["phase"], "repair");
    assert_eq!(ordinary["plans"][0]["replan_required"], true);
    assert_eq!(ordinary["plans"][0]["conflict_count"], Value::Null);
    assert_eq!(ordinary["current_conflicts"], "unknown");
    assert!(!r.to_string().contains("SECRET"));
}
fn fake_process(pid: u32) -> Option<ProcessInfo> {
    match pid {
        10 => Some(ProcessInfo {
            uid: unsafe { libc::geteuid() },
            parent: 1,
            executable: "/test/rdrive".into(),
        }),
        11 => Some(ProcessInfo {
            uid: unsafe { libc::geteuid() },
            parent: 10,
            executable: "/test/rdrive".into(),
        }),
        12 => Some(ProcessInfo {
            uid: unsafe { libc::geteuid() },
            parent: 10,
            executable: "/test/RDrive.app/Contents/MacOS/RDriveCompanion".into(),
        }),
        _ => None,
    }
}
fn service_files(f: &Fixture) {
    let label = format!(
        "io.rdrive.service.{}.{}",
        unsafe { libc::geteuid() },
        &state::key(&f.home().to_string_lossy())[..20]
    );
    object(
        &f.path("service/installation.json"),
        json!({"format":1,"label":label,"options":{"home":f.home(),"executable":"/test/rdrive","app":"/test/RDrive.app","ipc_root":"/private/tmp/ipc"}}),
    );
    object(
        &f.path("service/status.json"),
        json!({"format":1,"label":label,"supervisor_pid":10,"state":"running","updated":chrono::DateTime::from_timestamp(NOW,0).unwrap().to_rfc3339(),"children":{"broker":{"pid":11,"last_exit":1,"starts":3},"native_watcher":{"pid":12}},"credentials":{"password":"SECRET"}}),
    );
    write(&f.path("service/supervisor.lock"), b"");
}
#[test]
fn service_verifies_lease_heartbeat_uid_executable_and_parent_without_launchctl() {
    let f = Fixture::new();
    service_files(&f);
    let lease = File::open(f.path("service/supervisor.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);
    let r = service_with(f.home(), NOW, fake_process).unwrap().unwrap();
    assert_eq!(r["supervisor_running_observed"], true);
    assert_eq!(r["children"]["broker"]["identity_verified"], true);
    assert_eq!(r["children"]["native_watcher"]["state"], "running");
    assert!(!r.to_string().contains("SECRET"));
    let stale = service_with(f.home(), NOW + 20, fake_process)
        .unwrap()
        .unwrap();
    assert_eq!(stale["heartbeat_stale"], true);
    assert_eq!(stale["supervisor_running_observed"], false);
    drop(lease);
    assert_eq!(
        service_with(f.home(), NOW, fake_process).unwrap().unwrap()["supervisor_lease_held"],
        false
    );
    assert_eq!(
        process(
            &json!(11),
            Some(Path::new("/wrong")),
            Some(10),
            fake_process
        )["identity_verified"],
        false
    );
    assert_eq!(
        process(
            &json!(11),
            Some(Path::new("/test/rdrive")),
            Some(99),
            fake_process
        )["identity_verified"],
        false
    );
    assert_eq!(
        process(&json!(u64::MAX), None, None, fake_process)["state"],
        "unknown"
    );
    assert_eq!(
        process(&json!(i32::MAX), None, None, fake_process)["state"],
        "stopped"
    );
}
#[test]
fn native_process_probe_matches_the_current_owned_test_process() {
    let info = process_info(std::process::id()).unwrap();
    assert_eq!(info.uid, unsafe { libc::geteuid() });
    assert_eq!(
        info.executable.canonicalize().unwrap(),
        std::env::current_exe().unwrap().canonicalize().unwrap()
    );
    assert!(process_info(i32::MAX as u32).is_none());
}
#[test]
fn detached_checkpoint_image_is_only_reported_as_metadata_not_falsely_verified() {
    let mut f = Fixture::new();
    f.store.config.checkpoints = crate::checkpoints::Policy {
        enabled: true,
        max_records: 48,
        storage: crate::checkpoints::Storage::ApfsVault,
        interval_secs: Some(3600),
        max_bytes: Some(256 * 1024 * 1024),
        auto_prune: true,
    };
    f.store.save_config().unwrap();
    let image = f.path("checkpoints/vault/vault.dmg");
    write(&image, b"");
    File::options()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let image_metadata = fs::metadata(&image).unwrap();
    object(
        &f.path("checkpoints/vault/descriptor.json"),
        json!({"version":1,"home_binding":state::key(&f.home().to_string_lossy()),"ready":true,"capacity_bytes":256*1024*1024,"volume_uuid":uuid::Uuid::new_v4().to_string(),"image":{"device":image_metadata.dev(),"inode":image_metadata.ino()}}),
    );
    object(
        &f.path("checkpoint-scheduler/state.json"),
        json!({"version":1,"inflight":null,"last_success":{"checkpoint_id":id,"backend_key":"apfs_vault:268435456","completed_at":NOW-100},"failure_count":0,"retry_at":null,"last_error":null,"last_deferred":null}),
    );
    let before = inventory(f.home());
    let r = collect_at(f.home(), None, NOW, 10).unwrap();
    let c = &r["checkpoints"]["data"];
    assert_eq!(c["fixed_image_bytes"], 256 * 1024 * 1024);
    assert_eq!(c["payloads_verified"], false);
    assert_eq!(c["internal_used_bytes"], Value::Null);
    assert_eq!(c["last_confirmed_checkpoint"]["checkpoint_id"], id);
    assert_eq!(c["next_due_at"], NOW + 3500);
    assert_eq!(inventory(f.home()), before);
}
#[test]
fn unsafe_state_metadata_links_modes_special_files_and_bounds_are_rejected() {
    let f = Fixture::new();
    let p = f.path("unsafe");
    write(&p, b"x");
    let alias = f.path("alias");
    symlink(&p, &alias).unwrap();
    assert!(read(f.home(), Path::new("alias"), 2).is_err());
    fs::hard_link(&p, f.path("hard")).unwrap();
    assert!(read(f.home(), Path::new("unsafe"), 2).is_err());
    fs::remove_file(f.path("hard")).unwrap();
    fs::set_permissions(&p, Permissions::from_mode(0o644)).unwrap();
    assert!(read(f.home(), Path::new("unsafe"), 2).is_err());
    fs::set_permissions(&p, Permissions::from_mode(0o600)).unwrap();
    assert!(read(f.home(), Path::new("unsafe"), 0).is_err());
    assert!(checked(f.home(), Path::new("../unsafe"), false).is_err());
    assert!(checked(f.home(), Path::new(""), false).is_err());
    assert!(!metadata_valid(
        &fs::metadata(&p).unwrap(),
        false,
        unsafe { libc::geteuid() }.wrapping_add(1)
    ));
    let fifo = f.path("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(read(f.home(), Path::new("fifo"), 2).is_err());
    fs::create_dir(f.path("directory")).unwrap();
    fs::set_permissions(f.path("directory"), Permissions::from_mode(0o700)).unwrap();
    assert!(read(f.home(), Path::new("directory"), 2).is_err());
    let alias_home = f._temporary.path().join("alias-home");
    symlink(f.home(), &alias_home).unwrap();
    assert!(collect(&alias_home, None).is_err());
}
#[test]
fn changed_or_missing_sources_fail_closed_and_sqlite_queries_never_modify_the_source() {
    let f = Fixture::new();
    write(&f.path("one"), b"before");
    let source = Source::open(f.home(), Path::new("one"), 99)
        .unwrap()
        .unwrap();
    write(&f.path("one"), b"after");
    assert!(source.unchanged().is_err());
    fs::remove_file(f.path("one")).unwrap();
    assert!(source.unchanged().is_err());
    let c = f.sqlite("example.sqlite3");
    c.execute_batch("CREATE TABLE values_table(value TEXT);INSERT INTO values_table VALUES('a');")
        .unwrap();
    assert!(sql::<()>(f.home(), "example.sqlite3", |_| {
        c.execute("INSERT INTO values_table VALUES('b')", [])
            .unwrap();
        Ok(())
    })
    .is_err());
    assert!(sql::<()>(f.home(), "example.sqlite3", |_| bail!("query error")).is_err());
    object(&f.path("example.sqlite3-journal"), json!("hot journal"));
    assert!(sql::<()>(f.home(), "example.sqlite3", |_| Ok(())).is_err());
    assert!(Temporary::new(Path::new("/")).is_err());
    assert!(checked_sum([u64::MAX, 1].into_iter()).is_err());
}
#[test]
fn unsupported_and_wrong_account_indexes_are_not_migrated_or_adopted() {
    let f = Fixture::new();
    let d = f.domain("smart-a", "a");
    object(&f.path("provider-domains.json"), json!([d]));
    let c = f.index(&d);
    c.pragma_update(None, "user_version", 1).unwrap();
    let before = inventory(f.home());
    let r = collect(f.home(), None).unwrap();
    assert_eq!(r["index"]["status"], "partial");
    assert_eq!(
        r["index"]["data"]["domains"][0]["error"]["code"],
        "schema_unsupported"
    );
    assert_eq!(inventory(f.home()), before);
    c.pragma_update(None, "user_version", 2).unwrap();
    c.execute("UPDATE meta SET binding='other'", []).unwrap();
    assert_eq!(
        index(f.home(), &f.store.config, &d)
            .unwrap_err()
            .downcast_ref::<Unavailable>()
            .unwrap()
            .0,
        "scope_changed"
    );
}
#[test]
fn recorded_error_categories_are_safe_and_filter_legacy_wrappers_are_recognized() {
    for code in [
        "version_conflict",
        "excluded_by_filter",
        "insufficient_quota",
        "not_authenticated",
        "server_unreachable",
        "unsupported",
        "unknown",
    ] {
        let v = cause(Some(&format!("{code}: Authorization: Bearer SECRET")));
        assert!(!v.to_string().contains("SECRET"));
    }
    assert_eq!(
        cause(Some(
            "server_unreachable: synchronization filters exclude SECRET"
        ))["code"],
        "excluded_by_filter"
    );
    assert_eq!(cause(None)["code"], "operation_failed");
    for code in [
        "snapshot_changed",
        "private_path_required",
        "limit_exceeded",
        "scope_changed",
        "schema_unsupported",
        "invalid_configuration",
        "invalid_metadata",
    ] {
        let v = failure(&Unavailable(code).into());
        assert_eq!(v["code"], code);
    }
    assert!(!failure(&anyhow::anyhow!("SECRET"))
        .to_string()
        .contains("SECRET"));
}
#[test]
fn malformed_component_records_remain_unavailable_without_hiding_other_profiles() {
    let mut f = Fixture::new();
    let a = f.domain("a-domain", "a");
    let b = f.domain("b-domain", "b");
    object(&f.path("provider-domains.json"), json!([a, b]));
    let _a = f.index(&a);
    let _b = f.index(&b);
    write(
        &f.path(&format!("smart/{}/queue.json", state::key(&a.id))),
        b"{",
    );
    let report = collect(f.home(), None).unwrap();
    assert_eq!(report["conflicts"]["status"], "partial");
    assert_eq!(report["conflicts"]["data"]["domains"][1]["status"], "ok");
    object(
        &f.path("native-cache-usage.json"),
        json!({"version":2,"reports":{}}),
    );
    assert!(cache(f.home(), &f.store.config, &f.store.db, None, NOW).is_err());
    object(
        &f.path("native-cache-usage.json"),
        json!({"version":1,"reports":{"x":{"used":1,"pinned":2,"reserved":0,"updated":NOW}}}),
    );
    assert!(cache(f.home(), &f.store.config, &f.store.db, None, NOW).is_err());
    fs::remove_file(f.path("native-cache-usage.json")).unwrap();
    f.store.db.jobs.push(job("a", "invalid"));
    assert!(queue(&f.store.db, None, 1).is_err());
    f.store.db.jobs[0].status = "pending".into();
    f.store.db.jobs[0].size = u64::MAX;
    f.store.db.jobs.push(job("a", "pending"));
    f.db();
    let report = collect(f.home(), None).unwrap();
    assert_eq!(report["queue"]["status"], "error");
    object(
        &f.path("transfers/bad.json"),
        json!({"profile":"a","phase":"invalid"}),
    );
    assert!(transfers(f.home(), None, 1).is_err());
}
#[test]
fn service_and_checkpoint_scope_and_schema_fail_closed() {
    let f = Fixture::new();
    service_files(&f);
    let mut v = value(f.home(), "service/status.json").unwrap().unwrap();
    v["label"] = json!("wrong");
    object(&f.path("service/status.json"), v);
    assert!(service_with(f.home(), NOW, fake_process).is_err());
    fs::remove_file(f.path("service/status.json")).unwrap();
    assert!(
        service_with(f.home(), NOW, fake_process).unwrap().unwrap()["heartbeat_stale"]
            .as_bool()
            .unwrap()
    );
    let mut v = value(f.home(), "service/installation.json")
        .unwrap()
        .unwrap();
    v["options"]["home"] = json!("/different");
    object(&f.path("service/installation.json"), v);
    assert!(service_with(f.home(), NOW, fake_process).is_err());
    object(
        &f.path("checkpoints/vault/descriptor.json"),
        json!({"version":1,"home_binding":"wrong"}),
    );
    assert!(checkpoints(f.home(), Some(&f.store.config), NOW).is_err());
    fs::remove_file(f.path("checkpoints/vault/descriptor.json")).unwrap();
    object(
        &f.path("checkpoint-scheduler/state.json"),
        json!({"version":2}),
    );
    assert!(checkpoints(f.home(), Some(&f.store.config), NOW).is_err());
    fs::remove_file(f.path("checkpoint-scheduler/state.json")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    object(
        &f.path(&format!("checkpoints/{id}")),
        json!({"manifest":{"home":"/other"}}),
    );
    assert!(checkpoints(f.home(), Some(&f.store.config), NOW).is_err());
    object(
        &f.path(&format!("checkpoints/{id}")),
        json!({"manifest":{"home":f.home()}}),
    );
    assert_eq!(
        checkpoints(f.home(), Some(&f.store.config), NOW).unwrap()["legacy_records"],
        1
    );
    write(&f.path("checkpoints/unknown"), b"{}");
    assert!(checkpoints(f.home(), Some(&f.store.config), NOW).is_err());
}
#[test]
fn bounded_directory_walk_and_custom_cache_location_preserve_all_files() {
    let mut f = Fixture::new();
    let custom = f._temporary.path().canonicalize().unwrap().join("custom");
    crate::cache::set_directory(&mut f.store, Some(&custom)).unwrap();
    write(&custom.join("payload"), b"data");
    let before = inventory(f.home());
    let r = cache(f.home(), &f.store.config, &f.store.db, None, NOW).unwrap();
    assert_eq!(r["directory"], json!(custom));
    assert_eq!(r["used_bytes"], 4);
    assert_eq!(inventory(f.home()), before);
    for i in 0..=MAX_FILES {
        write(&f.path(&format!("transfers/{i:05}.skip")), b"");
    }
    assert!(names(f.home(), "transfers").is_err());
}
#[test]
fn cross_component_configuration_changes_clear_scoped_results_instead_of_mixing_accounts() {
    let mut f = Fixture::new();
    f.store.db.jobs.push(job("a", "pending"));
    f.db();
    let result = collect_observed(f.home(), None, NOW, 10, || {
        object(&f.path("config.json"), json!({"profiles":{}}));
    })
    .unwrap();
    assert_eq!(result["status"], "partial");
    assert_eq!(result["cache"]["error"]["code"], "snapshot_changed");
    assert!(result.get("profiles").is_none());
    assert!(result.get("queued").is_none());
    assert_eq!(result["service"]["status"], "missing");
}
#[test]
fn a_growing_metadata_file_cannot_exceed_the_read_limit_after_open() {
    let f = Fixture::new();
    write(&f.path("growing"), b"x");
    let result = read_with(f.home(), Path::new("growing"), 2, |p| {
        write(p, b"more than the limit");
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<Unavailable>().unwrap().0,
        "limit_exceeded"
    );
}

#[test]
fn replaced_checkpoint_image_and_unbound_smart_journals_are_not_reported_as_valid() {
    let f = Fixture::new();
    let image = f.path("checkpoints/vault/vault.dmg");
    write(&image, b"image");
    let meta = fs::metadata(&image).unwrap();
    let descriptor = json!({"version":1,"home_binding":state::key(&f.home().to_string_lossy()),"ready":true,"capacity_bytes":5,"volume_uuid":uuid::Uuid::new_v4().to_string(),"image":{"device":meta.dev(),"inode":meta.ino()+1}});
    object(
        &f.path("checkpoints/vault/descriptor.json"),
        descriptor.clone(),
    );
    assert_eq!(
        checkpoints(f.home(), Some(&f.store.config), NOW)
            .unwrap_err()
            .downcast_ref::<Unavailable>()
            .unwrap()
            .0,
        "scope_changed"
    );
    fs::remove_file(&image).unwrap();
    assert_eq!(
        checkpoints(f.home(), Some(&f.store.config), NOW)
            .unwrap_err()
            .downcast_ref::<Unavailable>()
            .unwrap()
            .0,
        "invalid_metadata"
    );
    fs::remove_file(f.path("checkpoints/vault/descriptor.json")).unwrap();
    object(
        &f.path(&format!("smart/{}/queue.json", state::key("old-domain"))),
        json!({"authorization":"SECRET"}),
    );
    let before = inventory(f.home());
    let report = collect(f.home(), None).unwrap();
    assert_eq!(report["conflicts"]["status"], "partial");
    assert_eq!(
        report["conflicts"]["data"]["unbound_journal_directories"],
        1
    );
    assert!(!report.to_string().contains("SECRET"));
    assert_eq!(inventory(f.home()), before);
}
#[test]
fn smart_journal_changes_during_observation_are_rejected_without_recovery() {
    let f = Fixture::new();
    let d = f.domain("smart", "a");
    let binding = crate::provider::journal_binding(&f.store.config, &d).unwrap();
    let mut q = crate::smart::Queue::open(&f.store, &d.id, &binding).unwrap();
    let result = smart_snapshot_with(f.home(), &d.id, &binding, || {
        q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"file","request_id":"new","base_content_version":"c","base_metadata_version":"m"}),None).unwrap();
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<Unavailable>().unwrap().0,
        "snapshot_changed"
    );
    service_files(&f);
    let mut runtime = value(f.home(), "service/status.json").unwrap().unwrap();
    runtime["state"] = json!("SECRET");
    runtime["updated"] = json!("SECRET");
    object(&f.path("service/status.json"), runtime);
    let service = service_with(f.home(), NOW, fake_process).unwrap().unwrap();
    assert_eq!(service["reported_state"], Value::Null);
    assert_eq!(service["reported_at"], Value::Null);
    assert!(!service.to_string().contains("SECRET"));
}

#[test]
fn filesystem_replacement_and_scan_boundaries_reject_inconsistent_snapshots() {
    let f = Fixture::new();
    write(&f.path("one"), b"old");
    assert!(checked_with(f.home(), Path::new("one"), false, || write(
        &f.path("one"),
        b"changed"
    ))
    .is_err());
    assert!(Source::open_with(f.home(), Path::new("one"), 99, |p| write(
        p,
        b"changed again"
    ))
    .is_err());
    assert!(names_with(f.home(), "cache", || write(&f.path("cache/new"), b"x")).is_err());
    assert!(
        cache_inventory_with(f.home(), &f.store.config, &f.store.db, || write(
            &f.path("cache/newer"),
            b"x"
        ))
        .is_err()
    );
    let loop_path = f.path("loop");
    symlink("loop", &loop_path).unwrap();
    assert!(optional_metadata(&loop_path.join("child")).is_err());
    assert!(lease_outcome(
        -1,
        std::io::Error::from(std::io::ErrorKind::PermissionDenied)
    )
    .is_err());
    assert_eq!(
        failure(&anyhow::anyhow!(
            "Smart domain/backend scope changed; local edits preserved"
        ))["code"],
        "scope_changed"
    );
}
#[test]
fn sqlite_failed_short_and_unreadable_copies_never_become_successful_observations() {
    let f = Fixture::new();
    let c = f.sqlite("test.sqlite3");
    c.execute_batch("CREATE TABLE t(i INTEGER);INSERT INTO t VALUES(1);")
        .unwrap();
    assert!(sql_with::<()>(
        f.home(),
        "test.sqlite3",
        |_| Ok(()),
        |_, _, _| Err(std::io::ErrorKind::Other.into())
    )
    .is_err());
    assert!(sql_with::<()>(f.home(), "test.sqlite3", |_| Ok(()), |_, _, _| Ok(0)).is_err());
    let count = std::cell::Cell::new(0);
    assert!(sql_with::<()>(
        f.home(),
        "test.sqlite3",
        |_| Ok(()),
        |source, output, limit| {
            let copied = std::io::copy(&mut source.take(limit), output)?;
            count.set(count.get() + 1);
            if count.get() == 2 {
                c.execute("INSERT INTO t VALUES(2)", []).unwrap();
            }
            Ok(copied)
        }
    )
    .is_err());
    assert!(sql::<()>(f.home(), "test.sqlite3", |_| {
        c.execute("INSERT INTO t VALUES(3)", []).unwrap();
        Ok(())
    })
    .is_err());
    assert!(sql_with::<()>(
        f.home(),
        "test.sqlite3",
        |_| Ok(()),
        |source, output, limit| {
            let copied = std::io::copy(&mut source.take(limit), output)?;
            output.set_permissions(Permissions::from_mode(0))?;
            Ok(copied)
        }
    )
    .is_err());
}
#[test]
fn empty_cache_missing_unpinned_content_and_scan_capacity_are_explicit() {
    let mut f = Fixture::new();
    fs::remove_dir(f.path("cache")).unwrap();
    assert_eq!(
        cache_inventory(f.home(), &f.store.config, &f.store.db)
            .unwrap()
            .used,
        0
    );
    f.store.db.cache.insert(
        "absent".into(),
        cache_item(&f.store.config, "a", "absent", 4, false),
    );
    let r = cache(f.home(), &f.store.config, &f.store.db, None, NOW).unwrap();
    assert_eq!(r["missing_files"], 1);
    assert_eq!(r["pinned_files"], 0);
    for n in 0..=MAX_FILES {
        write(&f.path(&format!("cache/{n:05}")), b"");
    }
    assert!(cache_inventory(f.home(), &f.store.config, &f.store.db).is_err());
}
#[test]
fn malformed_domain_identity_capacity_and_index_headers_are_never_adopted() {
    let f = Fixture::new();
    let mut d = f.domain("", "a");
    object(&f.path("provider-domains.json"), json!([d]));
    assert!(domains(f.home(), &f.store.config).is_err());
    d.id = "valid".into();
    let many = vec![d.clone(); MAX_FILES + 1];
    object(&f.path("provider-domains.json"), json!(many));
    assert!(domains(f.home(), &f.store.config).is_err());
    let c = f.index(&d);
    c.execute("DROP TABLE meta", []).unwrap();
    assert!(index(f.home(), &f.store.config, &d).is_err());
}
#[test]
fn retained_completed_and_missing_smart_payloads_are_reported_without_rewriting_tasks() {
    let f = Fixture::new();
    let d = f.domain("smart", "a");
    object(&f.path("provider-domains.json"), json!([d]));
    let binding = crate::provider::journal_binding(&f.store.config, &d).unwrap();
    let mut q = crate::smart::Queue::open(&f.store, &d.id, &binding).unwrap();
    write(&f.path("input"), b"retained");
    let first=q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"first","request_id":"first","base_content_version":"c","base_metadata_version":"m"}),Some(&File::open(f.path("input")).unwrap())).unwrap();
    q.start(&first.id).unwrap();
    q.complete(&first.id, json!({"confirmed":true})).unwrap();
    let missing=q.enqueue(json!({"v":1,"op":"modify","domain":d.id,"id":"missing","request_id":"missing","base_content_version":"c","base_metadata_version":"m"}),Some(&File::open(f.path("input")).unwrap())).unwrap();
    q.fail(
        &missing.id,
        crate::smart::FailureKind::Conflict,
        "version_conflict: secret",
    )
    .unwrap();
    let payload = f.path(&format!(
        "smart/{}/dirty/{}",
        state::key(&d.id),
        missing.snapshot.unwrap().file
    ));
    fs::remove_file(&payload).unwrap();
    let r = conflicts(f.home(), &f.store.config, &[d.clone()], None, 10).unwrap();
    assert_eq!(r["domains"][0]["counts"]["done"], 1);
    assert_eq!(r["examples"][0]["retained_content"], false);
    assert_eq!(r["domains"][0]["missing_example_payloads"], 1);
    write(&payload, b"retained");
    fs::set_permissions(&payload, Permissions::from_mode(0o644)).unwrap();
    assert!(conflicts(f.home(), &f.store.config, &[d], None, 10).is_err());
}
#[test]
fn ordinary_reports_with_missing_accounts_roots_and_unsupported_phases_are_ignored() {
    let mut f = Fixture::new();
    let binding = crate::connection_status::binding(&f.store.config.profiles["a"]).unwrap();
    let good = json!({"version":1,"binding":binding,"local_root":"/tmp/local","remote_root":"remote","phase":"content_plan"});
    let mut absent = good.clone();
    absent.as_object_mut().unwrap().remove("local_root");
    let mut invalid = good.clone();
    invalid["phase"] = json!("unsupported");
    f.store.db.history = vec![("unknown", good), ("a", absent), ("a", invalid)]
        .into_iter()
        .map(|(profile, detail)| Event {
            time: state::now(),
            operation: "sync-status".into(),
            profile: profile.into(),
            path: "remote".into(),
            detail: detail.to_string(),
        })
        .collect();
    f.db();
    let result = ordinary_conflicts(f.home(), &f.store.config, None, 10).unwrap();
    assert_eq!(result["status"], "not_recorded");
    assert_eq!(result["ignored_stale_or_invalid_reports"], 3);
}
#[test]
fn disappearing_transfer_checkpoint_and_changed_schedule_are_partial_observations() {
    let f = Fixture::new();
    object(
        &f.path("transfers/one.json"),
        json!({"profile":"a","phase":"downloading"}),
    );
    assert!(transfers_with(f.home(), None, 10, || fs::remove_file(
        f.path("transfers/one.json")
    )
    .unwrap())
    .is_err());
    let id = uuid::Uuid::new_v4().to_string();
    let record = f.path(&format!("checkpoints/{id}"));
    object(&record, json!({"manifest":{"home":f.home()}}));
    assert!(
        checkpoints_with(f.home(), Some(&f.store.config), NOW, |phase| {
            if phase == "records_listed" {
                fs::remove_file(&record).unwrap()
            }
        })
        .is_err()
    );
    assert!(
        checkpoints_with(f.home(), Some(&f.store.config), NOW, |phase| {
            if phase == "schedule_observed" {
                object(
                    &f.path("checkpoint-scheduler/state.json"),
                    json!({"changed":true}),
                )
            }
        })
        .is_err()
    );
    fs::remove_file(f.path("checkpoint-scheduler/state.json")).unwrap();
    object(
        &f.path("checkpoints/vault/descriptor.json"),
        json!({"version":1,"home_binding":state::key(&f.home().to_string_lossy()),"ready":false,"volume_uuid":"invalid"}),
    );
    assert!(checkpoints(f.home(), Some(&f.store.config), NOW).is_err());
    object(
        &f.path("checkpoints/vault/descriptor.json"),
        json!({"version":1,"home_binding":state::key(&f.home().to_string_lossy()),"ready":false,"image":null}),
    );
    assert_eq!(
        checkpoints(f.home(), Some(&f.store.config), NOW).unwrap()["identity_ready"],
        false
    );
}
#[cfg(target_os = "macos")]
#[test]
fn macos_pid_path_results_reject_unavailable_nonterminated_and_oversized_output() {
    assert!(process_path_result(b"/path\0", 0).is_none());
    assert!(process_path_result(b"/path\0", 99).is_none());
    assert!(process_path_result(b"/path", 5).is_none());
    assert_eq!(
        process_path_result(b"/path\0", 5).unwrap(),
        Path::new("/path")
    );
    assert_eq!(
        process_path_result(b"/path\0", 6).unwrap(),
        Path::new("/path")
    );
}

#[test]
fn standalone_sqlite_without_wal_is_read_without_sidecars_and_profile_count_is_bounded() {
    let mut f = Fixture::new();
    let path = f.path("standalone.sqlite3");
    write(&path, b"");
    let c = Connection::open(&path).unwrap();
    c.execute_batch("CREATE TABLE t(i INTEGER);INSERT INTO t VALUES(7);")
        .unwrap();
    drop(c);
    let before = inventory(f.home());
    let value = sql(f.home(), "standalone.sqlite3", |c| {
        c.query_row("SELECT i FROM t", [], |r| r.get::<_, i64>(0))
            .map_err(Into::into)
    })
    .unwrap();
    assert_eq!(value, Some(7));
    assert_eq!(inventory(f.home()), before);
    for index in 0..=MAX_FILES {
        f.store
            .config
            .profiles
            .insert(format!("connection-{index}"), profile());
    }
    f.store.save_config().unwrap();
    let report = collect(f.home(), None).unwrap();
    assert_eq!(report["configuration"]["status"], "error");
    assert!(report.get("profiles").is_none());
}
