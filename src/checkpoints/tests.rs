use super::*;
use std::os::unix::{
    ffi::OsStringExt,
    fs::{symlink, PermissionsExt},
};

struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    host: FakeHost,
}
struct FakeHost {
    base: PathBuf,
    home: PathBuf,
    images: BTreeMap<String, PathBuf>,
    snapshots: Vec<Snapshot>,
    supported: bool,
    add: usize,
    lost_reply: bool,
    changed: bool,
    corrupt: bool,
    bad_marker: bool,
    mount_error: bool,
    unmount_error: bool,
    calls: Vec<String>,
}
fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes.as_ref()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn copy_tree(from: &Path, to: &Path) {
    directory(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let meta = entry.file_type().unwrap();
        if meta.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if meta.is_symlink() {
            symlink(fs::read_link(entry.path()).unwrap(), target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("state")).unwrap();
        store.save().unwrap();
        store.save_config().unwrap();
        write(&store.home.join("spool/upload"), b"retained upload");
        let blob = state::key("history bytes");
        write(
            &store.home.join("history/blobs").join(blob),
            b"history bytes",
        );
        // These directories are made by their production subsystems with 0700.
        fs::set_permissions(
            store.home.join("history"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(
            store.home.join("history/blobs"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let host = FakeHost {
            base: temp.path().canonicalize().unwrap(),
            home: store.home.clone(),
            images: BTreeMap::new(),
            snapshots: vec![],
            supported: true,
            add: 1,
            lost_reply: false,
            changed: false,
            corrupt: false,
            bad_marker: false,
            mount_error: false,
            unmount_error: false,
            calls: vec![],
        };
        Self {
            _temp: temp,
            store,
            host,
        }
    }
    fn policy() -> Policy {
        Policy {
            enabled: true,
            max_records: 64,
        }
    }
    fn create(&mut self) -> String {
        create_with(&self.store, &Self::policy(), false, &mut self.host).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn destination(&self, name: &str) -> PathBuf {
        self._temp.path().join(name)
    }
}
impl Host for FakeHost {
    fn supported(&self) -> bool {
        self.supported
    }
    fn volume(&mut self, _: &Path) -> Result<Volume> {
        Ok(Volume {
            uuid: "154108c8-1cbb-4fae-81f4-c6c53161f2ae".into(),
            device: "disk3s5".into(),
            mount: self.base.clone(),
        })
    }
    fn snapshots(&mut self, _: &Volume) -> Result<Vec<Snapshot>> {
        Ok(self.snapshots.clone())
    }
    fn create(&mut self, authorize: bool) -> Result<String> {
        self.calls.push(format!("create:{authorize}"));
        for i in 0..self.add {
            let snapshot = Snapshot {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: format!(
                    "com.apple.TimeMachine.2026-11-12-0102{:02}.local",
                    self.snapshots.len() + i
                ),
            };
            let image = self.base.join(format!("image-{}", snapshot.uuid));
            directory(&image).unwrap();
            copy_tree(&self.home, &image.join("state"));
            if self.corrupt {
                write(&image.join("state/spool/upload"), b"changed");
            }
            if self.bad_marker {
                let checkpoint = records(&self.home)?[0].manifest.id.clone();
                write(
                    &image
                        .join("state/checkpoints")
                        .join(checkpoint)
                        .join("manifest.json"),
                    b"{}",
                );
            }
            self.images.insert(snapshot.uuid.clone(), image);
            self.snapshots.push(snapshot);
        }
        if self.changed {
            write(
                &self.home.join("spool/upload"),
                b"changed while snapshotting",
            );
        }
        if self.lost_reply {
            bail!("injected lost OS response");
        }
        Ok("Created local snapshot".into())
    }
    fn mount(
        &mut self,
        _: &Volume,
        snapshot: &Snapshot,
        target: &Path,
        authorize: bool,
    ) -> Result<()> {
        self.calls.push(format!("mount:{authorize}"));
        if self.mount_error {
            bail!("injected mount denied");
        }
        let source = self
            .images
            .get(&snapshot.uuid)
            .context("fake image missing")?;
        copy_tree(source, target);
        Ok(())
    }
    fn unmount(&mut self, target: &Path, authorize: bool) -> Result<()> {
        self.calls.push(format!("unmount:{authorize}"));
        if self.unmount_error {
            bail!("injected unmount denied");
        }
        for entry in fs::read_dir(target)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}
fn error(result: Result<Value>, text: &str) {
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains(text), "expected {text:?}, got {error:?}");
}

#[test]
fn policy_and_default_disabled_never_create() {
    assert!(!Policy::default().enabled);
    Policy::default().validate().unwrap();
    for max_records in [0, 4097] {
        assert!(Policy {
            enabled: true,
            max_records
        }
        .validate()
        .is_err());
    }
    let mut f = Fixture::new();
    error(
        create_with(&f.store, &Policy::default(), true, &mut f.host),
        "disabled",
    );
    assert!(f.host.calls.is_empty());
    f.host.supported = false;
    assert_eq!(
        status_with(&f.store.home, &Fixture::policy(), &mut f.host).unwrap()["available"],
        false
    );
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "unavailable",
    );
}
#[test]
fn create_inspect_export_and_forget_preserve_live_state() {
    let mut f = Fixture::new();
    let before = fs::read(f.store.home.join("state.json")).unwrap();
    let id = f.create();
    let status = status_with(&f.store.home, &Fixture::policy(), &mut f.host).unwrap();
    assert_eq!(status["local_record_count"], 1);
    assert!(status["content_scope"]
        .as_str()
        .unwrap()
        .contains("external"));
    let list = list_with(&f.store.home, &mut f.host).unwrap();
    assert_eq!(list["checkpoints"][0]["snapshot_present"], true);
    let info = inspect(&f.store.home, &id).unwrap();
    assert_eq!(info["state"], "confirmed");
    assert_eq!(info["files"]["spool/upload"]["kind"], "queued_upload");
    assert_eq!(info["files"]["state.json"]["exportable"], false);
    let destination = f.destination("recovered");
    let exported = export_with(
        &f.store.home,
        &id,
        Path::new("spool/upload"),
        &destination,
        false,
        &mut f.host,
    )
    .unwrap();
    assert_eq!(exported["new_guarded_restore_required"], true);
    assert_eq!(fs::read(&destination).unwrap(), b"retained upload");
    assert_eq!(fs::read(f.store.home.join("state.json")).unwrap(), before);
    assert_eq!(
        f.host.calls,
        vec![
            "create:false",
            "mount:false",
            "unmount:false",
            "mount:false",
            "unmount:false"
        ]
    );
    assert_eq!(forget(&f.store, &id).unwrap()["os_snapshot_deleted"], false);
    forget(&f.store, &id).unwrap();
    assert!(records(&f.store.home).unwrap().is_empty());
    assert_eq!(f.host.snapshots.len(), 1);
}
#[test]
fn incomplete_os_reply_can_be_reconciled_only_from_exact_snapshot() {
    let mut f = Fixture::new();
    f.host.lost_reply = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "not confirmed",
    );
    let id = records(&f.store.home).unwrap()[0].manifest.id.clone();
    assert!(!load(&f.store.home, &id).unwrap().confirmed);
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("x"),
            false,
            &mut f.host,
        ),
        "unconfirmed",
    );
    assert_eq!(
        reconcile_with(&f.store, &id, true, &mut f.host).unwrap()["reconciled"],
        true
    );
    assert!(load(&f.store.home, &id).unwrap().confirmed);
}
#[test]
fn nonmatching_reconciliation_never_promotes() {
    let mut f = Fixture::new();
    f.host.lost_reply = true;
    f.host.corrupt = true;
    assert!(create_with(&f.store, &Fixture::policy(), false, &mut f.host).is_err());
    let id = records(&f.store.home).unwrap()[0].manifest.id.clone();
    error(
        reconcile_with(&f.store, &id, false, &mut f.host),
        "no available",
    );
    assert!(!load(&f.store.home, &id).unwrap().confirmed);
    f.host.supported = false;
    error(
        reconcile_with(&f.store, &id, false, &mut f.host),
        "requires macOS",
    );
    f.host.supported = true;
    f.host.base = f.destination("wrong-volume");
    error(
        reconcile_with(&f.store, &id, false, &mut f.host),
        "volume changed",
    );
}
#[test]
fn creation_refuses_zero_multiple_or_changed_snapshot_state() {
    for count in [0, 2] {
        let mut f = Fixture::new();
        f.host.add = count;
        error(
            create_with(&f.store, &Fixture::policy(), false, &mut f.host),
            "unique new",
        );
        assert!(!records(&f.store.home).unwrap()[0].confirmed);
    }
    let mut f = Fixture::new();
    f.host.changed = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "state changed",
    );
    let mut f = Fixture::new();
    f.host.corrupt = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "does not match",
    );
    let mut f = Fixture::new();
    f.host.bad_marker = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "does not match",
    );
}
#[test]
fn mount_failure_and_cleanup_failure_leave_unconfirmed_records() {
    let mut f = Fixture::new();
    f.host.mount_error = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "mount denied",
    );
    assert!(f.host.calls.last().unwrap().starts_with("unmount:"));
    let mut f = Fixture::new();
    f.host.unmount_error = true;
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "cleanup failed",
    );
    assert!(!records(&f.store.home).unwrap()[0].confirmed);
}
#[test]
fn exports_are_new_payloads_only_and_never_use_missing_snapshots() {
    let mut f = Fixture::new();
    let id = f.create();
    let destination = f.destination("already");
    write(&destination, b"original");
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &destination,
            false,
            &mut f.host,
        ),
        "new absolute",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"original");
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            Path::new("relative"),
            false,
            &mut f.host,
        ),
        "new absolute",
    );
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("state.json"),
            &f.destination("db"),
            false,
            &mut f.host,
        ),
        "state databases",
    );
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.store.home.join("forbidden"),
            false,
            &mut f.host,
        ),
        "live state",
    );
    let alias = f.destination("alias");
    symlink(&f.store.home, &alias).unwrap();
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &alias.join("forbidden"),
            false,
            &mut f.host,
        ),
        "live state",
    );
    f.host.supported = false;
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("platform"),
            false,
            &mut f.host,
        ),
        "unavailable",
    );
    f.host.supported = true;
    f.host.snapshots.clear();
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("missing"),
            false,
            &mut f.host,
        ),
        "no longer available",
    );
    f.host.supported = false;
    assert_eq!(
        list_with(&f.store.home, &mut f.host).unwrap()["checkpoints"][0]["snapshot_present"],
        Value::Null
    );
}
#[test]
fn export_verifies_actual_snapshot_bytes() {
    let mut f = Fixture::new();
    let id = f.create();
    let image = f.host.images.values().next().unwrap().clone();
    write(&image.join("state/spool/upload"), b"corrupt bytes");
    let destination = f.destination("recovered");
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &destination,
            false,
            &mut f.host,
        ),
        "checksum changed",
    );
    assert!(!destination.exists());
    assert!(!fs::read_dir(f._temp.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".rdrive-checkpoint-")));
}
#[test]
fn symlinks_cannot_escape_snapshot_or_redirect_recovery() {
    let mut f = Fixture::new();
    let id = f.create();
    let image = f.host.images.values().next().unwrap().clone();
    fs::remove_file(image.join("state/spool/upload")).unwrap();
    symlink(
        f.store.home.join("spool/upload"),
        image.join("state/spool/upload"),
    )
    .unwrap();
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("out"),
            false,
            &mut f.host,
        ),
        "Too many levels",
    );
    fs::remove_file(image.join("state/spool/upload")).unwrap();
    fs::remove_dir(image.join("state/spool")).unwrap();
    symlink(f.store.home.join("spool"), image.join("state/spool")).unwrap();
    assert!(export_with(
        &f.store.home,
        &id,
        Path::new("spool/upload"),
        &f.destination("out"),
        false,
        &mut f.host
    )
    .is_err());
    let destination = f.destination("dangling");
    symlink(f.destination("absent"), &destination).unwrap();
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &destination,
            false,
            &mut f.host,
        ),
        "new absolute",
    );
}
#[test]
fn mount_targets_cannot_contain_existing_content() {
    let mut f = Fixture::new();
    let id = f.create();
    let mounts = root(&f.store.home).join("mounts").join(&id);
    directory(&mounts).unwrap();
    write(&mounts.join("keep"), b"original");
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("out"),
            false,
            &mut f.host,
        ),
        "not empty",
    );
    assert_eq!(fs::read(mounts.join("keep")).unwrap(), b"original");
}
#[test]
fn record_cap_and_administration_leases_are_enforced() {
    let mut f = Fixture::new();
    f.create();
    error(
        create_with(
            &f.store,
            &Policy {
                enabled: true,
                max_records: 1,
            },
            false,
            &mut f.host,
        ),
        "record limit",
    );
    let _lease = lease(&f.store.home).unwrap();
    assert!(lease(&f.store.home).is_err());
    assert!(forget(&f.store, &uuid::Uuid::new_v4().to_string()).is_err());
}
#[test]
fn running_upload_and_smart_queue_prevent_checkpoint() {
    let mut f = Fixture::new();
    let mut job:crate::state::Job=serde_json::from_value(json!({"id":"a","profile":"p","remote":"file","snapshot":"a","sha256":state::key("a"),"size":1,"created":state::now(),"status":"running","attempts":0,"error":null,"expected":null,"overwrite":false,"version":false})).unwrap();
    f.store.db.jobs.push(job.clone());
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "upload is running",
    );
    f.store.db.jobs.clear();
    job.status = "pending".into();
    let queue = f.store.home.join("smart/domain");
    directory(&f.store.home.join("smart")).unwrap();
    directory(&queue).unwrap();
    write(&queue.join("queue.lock"), []);
    write(
        &queue.join("queue.json"),
        br#"{"mutations":{"one":{"state":"running"}}}"#,
    );
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "still running",
    );
    write(&queue.join("queue.json"), br#"{"mutations":{}}"#);
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(queue.join("queue.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    error(
        create_with(&f.store, &Fixture::policy(), false, &mut f.host),
        "Smart request is active",
    );
    drop(held);
    write(&queue.join("queue.json"), b"{}");
    assert!(queue_leases(&f.store.home).is_err());
    write(&queue.join("queue.json"), br#"{"mutations":{}}"#);
    write(
        &f.store.home.join("smart/non-directory"),
        b"ignored here but scanned later",
    );
    assert_eq!(queue_leases(&f.store.home).unwrap().len(), 1);
}
#[test]
fn sqlite_wals_are_flushed_and_busy_readers_are_refused() {
    let f = Fixture::new();
    let path = f.store.home.join("history/history.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL;CREATE TABLE retained(value);INSERT INTO retained VALUES(1);",
    )
    .unwrap();
    let mut files = BTreeMap::new();
    walk(&f.store.home, &f.store.home, &mut files, true).unwrap();
    assert!(files.contains_key(Path::new("history/history.sqlite3")));
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN;SELECT * FROM retained;")
        .unwrap();
    db.execute_batch("INSERT INTO retained VALUES(2)").unwrap();
    assert!(walk(&f.store.home, &f.store.home, &mut files, true)
        .unwrap_err()
        .to_string()
        .contains("SQLite writer is busy"));
}
#[test]
fn walker_excludes_transients_and_classifies_only_payloads() {
    let f = Fixture::new();
    let home = &f.store.home;
    for name in [
        "tmp/excluded",
        "service/control",
        "provider/demands-one",
        "history/blobs/.capture-a",
        "cache/.rdrive-a.tmp",
        "some.lock",
        "history/history.sqlite3-wal",
        "history/history.sqlite3-shm",
    ] {
        write(&home.join(name), b"ignored");
    }
    for name in [
        "cache/content",
        "smart/domain/dirty/one.data",
        "smart/domain/queue.json",
        "history/restore-plans/a",
    ] {
        write(&home.join(name), b"included");
    }
    let mut files = BTreeMap::new();
    walk(home, home, &mut files, false).unwrap();
    assert_eq!(files[Path::new("cache/content")].kind, "cached_payload");
    assert_eq!(
        files[Path::new("smart/domain/dirty/one.data")].kind,
        "smart_payload"
    );
    assert!(!files[Path::new("history/restore-plans/a")].exportable);
    assert!(!files.keys().any(|p| skipped(p)));
    assert!(!exportable(Path::new("history/blobs/not-a-sha")));
    symlink("/etc/passwd", home.join("bad-link")).unwrap();
    assert!(walk(home, home, &mut files, false)
        .unwrap_err()
        .to_string()
        .contains("symbolic links"));
    fs::remove_file(home.join("bad-link")).unwrap();
    let fifo = CString::new(home.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(walk(home, home, &mut files, false)
        .unwrap_err()
        .to_string()
        .contains("non-regular"));
}
#[test]
fn validators_reject_unsafe_names_permissions_and_wrong_volume() {
    let f = Fixture::new();
    assert!(id("ABC").is_err());
    assert!(id("154108C8-1CBB-4FAE-81F4-C6C53161F2AE").is_err());
    for value in ["", "../escape", "/absolute", "a/../b"] {
        assert!(relative(Path::new(value)).is_err());
    }
    assert!(!digest(&"F".repeat(64)));
    assert!(!digest("a"));
    let volume = Volume {
        uuid: uuid::Uuid::new_v4().to_string(),
        device: "disk3s1".into(),
        mount: f._temp.path().canonicalize().unwrap(),
    };
    validate_volume(&volume).unwrap();
    assert_eq!(
        home_relative(&f.store.home, &volume).unwrap(),
        Path::new("state")
    );
    let mut bad = volume.clone();
    bad.device = "disk;evil".into();
    assert!(validate_volume(&bad).is_err());
    bad = volume.clone();
    bad.mount = f.store.home.join("cache");
    assert!(home_relative(&f.store.home, &bad).is_err());
    let marker = f.destination("public");
    write(&marker, b"x");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(private(&marker, false).is_err());
    let hard = f.destination("hard");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&marker, &hard).unwrap();
    assert!(private(&marker, false).is_err());
    assert!(directory(&marker).is_err());
    assert!(directory(&f.destination("missing/child")).is_err());
    assert!(validate_snapshot(&Snapshot {
        uuid: uuid::Uuid::new_v4().to_string(),
        name: "arbitrary".into()
    })
    .is_err());
    assert!(validate_snapshot(&Snapshot {
        uuid: uuid::Uuid::new_v4().to_string(),
        name: "com.apple.TimeMachine.2026-99-99-000000.local".into()
    })
    .is_err());
}
#[test]
fn record_validation_refuses_corruption_and_never_deletes_unknown_files() {
    let mut f = Fixture::new();
    let id = f.create();
    let path = record_path(&f.store.home, &id).unwrap();
    let good = fs::read(&path).unwrap();
    let edits: Vec<Box<dyn Fn(&mut Value)>> = vec![
        Box::new(|v| v["manifest"]["version"] = json!(2)),
        Box::new(|v| v["manifest"]["home"] = json!("other")),
        Box::new(|v| v["manifest"]["files"]["state.json"]["sha256"] = json!("bad")),
        Box::new(|v| v["manifest"]["files"]["state.json"]["kind"] = json!("history_blob")),
        Box::new(|v| v["manifest"]["files"]["state.json"]["exportable"] = json!(true)),
        Box::new(|v| v["snapshot"] = Value::Null),
        Box::new(|v| v["before"] = json!(["bad-id"])),
    ];
    for edit in edits {
        let mut value: Value = serde_json::from_slice(&good).unwrap();
        edit(&mut value);
        state::atomic_json(&path, &value).unwrap();
        assert!(load(&f.store.home, &id).is_err());
    }
    let mut value: Value = serde_json::from_slice(&good).unwrap();
    value["manifest"]["files"]["state.json"]["bytes"] = json!(u64::MAX);
    state::atomic_json(&path, &value).unwrap();
    assert!(load(&f.store.home, &id)
        .unwrap_err()
        .to_string()
        .contains("overflow"));
    state::atomic_write(&path, &good).unwrap();
    let extra = path.parent().unwrap().join("unexpected");
    write(&extra, b"keep");
    error(forget(&f.store, &id), "unexpected content");
    assert!(extra.exists());
    fs::remove_file(extra).unwrap();
    let tomb = root(&f.store.home).join(format!("forgotten-{id}"));
    directory(&tomb).unwrap();
    error(forget(&f.store, &id), "needs review");
    fs::remove_dir(&tomb).unwrap();
    fs::rename(path.parent().unwrap(), &tomb).unwrap();
    assert!(records(&f.store.home).unwrap().is_empty());
    fs::remove_file(tomb.join("manifest.json")).unwrap();
    forget(&f.store, &id).unwrap();
}
#[test]
fn mounted_manifest_is_verified_without_trusting_marker_names() {
    let mut f = Fixture::new();
    let id = f.create();
    let record = load(&f.store.home, &id).unwrap();
    let image = f.host.images.values().next().unwrap().clone();
    assert!(matches_mounted(&image, &record.manifest, true).unwrap());
    let marker = image
        .join("state/checkpoints")
        .join(&id)
        .join("manifest.json");
    fs::remove_file(&marker).unwrap();
    assert!(!matches_mounted(&image, &record.manifest, false).unwrap());
    write(&marker, b"not json");
    assert!(!matches_mounted(&image, &record.manifest, false).unwrap());
    state::atomic_json(&marker, &record.manifest).unwrap();
    fs::remove_file(image.join("state/spool/upload")).unwrap();
    assert!(!matches_mounted(&image, &record.manifest, true).unwrap());
    assert!(open_relative(&image, Path::new("../escape")).is_err());
    let mut missing = record.clone();
    missing.snapshot = None;
    assert!(mounted(&f.store.home, &missing, false, &mut f.host, |_| Ok(())).is_err());
}
#[test]
fn reconciliation_is_bounded() {
    let mut f = Fixture::new();
    f.host.lost_reply = true;
    assert!(create_with(&f.store, &Fixture::policy(), false, &mut f.host).is_err());
    let id = records(&f.store.home).unwrap()[0].manifest.id.clone();
    for _ in 0..65 {
        f.host.snapshots.push(Snapshot {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "com.apple.TimeMachine.2026-11-12-000000.local".into(),
        });
    }
    error(
        reconcile_with(&f.store, &id, false, &mut f.host),
        "too many new",
    );
}

use std::cell::RefCell;
thread_local! {
    static COMMANDS:RefCell<Vec<(String,Vec<String>,bool,Result<Vec<u8>,String>)>>=const{RefCell::new(vec![])};
    static FILESYSTEM:RefCell<Option<(PathBuf,bool)>>=const{RefCell::new(None)};
}
fn raw(request: Request) -> Result<Vec<u8>> {
    COMMANDS.with(|rows| {
        let (program, args, authorize, result) = rows.borrow_mut().remove(0);
        assert_eq!(request.program, program);
        assert_eq!(request.args, args);
        assert_eq!(request.authorize, authorize);
        if request.program == "/usr/bin/plutil" {
            assert!(request.input.is_some());
        }
        result.map_err(anyhow::Error::msg)
    })
}
fn fs_mock(_: &Path) -> Result<(PathBuf, bool)> {
    FILESYSTEM.with(|value| {
        value
            .borrow()
            .clone()
            .context("injected filesystem unavailable")
    })
}
fn queue_json(value: Value) {
    COMMANDS.with(|rows| {
        *rows.borrow_mut() = vec![
            (
                "/usr/sbin/diskutil".into(),
                vec!["info".into(), "-plist".into(), "/fake".into()],
                false,
                Ok(vec![1]),
            ),
            (
                "/usr/bin/plutil".into(),
                vec![
                    "-convert".into(),
                    "json".into(),
                    "-o".into(),
                    "-".into(),
                    "-".into(),
                ],
                false,
                Ok(serde_json::to_vec(&value).unwrap()),
            ),
        ]
    });
}
fn volume_json() -> Value {
    json!({"FilesystemType":"apfs","Writable":true,"APFSSnapshot":false,"VolumeUUID":"154108c8-1cbb-4fae-81f4-c6c53161f2ae","DeviceIdentifier":"disk3s5","MountPoint":"/fake"})
}
#[test]
fn system_adapter_validates_volume_capability_and_plist_fields() {
    let mut system = System {
        execute: raw,
        filesystem: fs_mock,
    };
    assert_eq!(system.supported(), cfg!(target_os = "macos"));
    FILESYSTEM.with(|v| *v.borrow_mut() = Some((PathBuf::from("/fake"), false)));
    queue_json(volume_json());
    assert_eq!(system.volume(Path::new("/home")).unwrap().device, "disk3s5");
    for (key, value) in [
        ("FilesystemType", json!("exfat")),
        ("Writable", json!(false)),
        ("APFSSnapshot", json!(true)),
        ("VolumeUUID", Value::Null),
        ("DeviceIdentifier", Value::Null),
        ("MountPoint", Value::Null),
        ("DeviceIdentifier", json!("bad-device")),
    ] {
        let mut v = volume_json();
        v[key] = value;
        queue_json(v);
        assert!(system.volume(Path::new("/home")).is_err());
    }
    FILESYSTEM.with(|v| *v.borrow_mut() = Some((PathBuf::from("/fake"), true)));
    assert!(system
        .volume(Path::new("/home"))
        .unwrap_err()
        .to_string()
        .contains("read-only"));
    FILESYSTEM.with(|v| *v.borrow_mut() = None);
    assert!(system.volume(Path::new("/home")).is_err());
}
fn snapshot_json(value: Value) {
    queue_json(value);
    COMMANDS.with(|rows| {
        rows.borrow_mut()[0].1 = vec![
            "apfs".into(),
            "listSnapshots".into(),
            "-plist".into(),
            "disk3s5".into(),
        ]
    });
}
#[test]
fn system_adapter_filters_unowned_snapshot_names_and_validates_identity() {
    let mut system = System {
        execute: raw,
        filesystem: fs_mock,
    };
    let volume = Volume {
        uuid: uuid::Uuid::new_v4().to_string(),
        device: "disk3s5".into(),
        mount: "/fake".into(),
    };
    snapshot_json(
        json!({"Snapshots":[{"SnapshotName":"unrelated"},{"SnapshotName":"com.apple.TimeMachine.2026-11-12-010203.local","SnapshotUUID":"154108c8-1cbb-4fae-81f4-c6c53161f2ae"}]}),
    );
    assert_eq!(system.snapshots(&volume).unwrap().len(), 1);
    snapshot_json(json!({}));
    assert!(system.snapshots(&volume).is_err());
    snapshot_json(
        json!({"Snapshots":[{"SnapshotName":"com.apple.TimeMachine.2026-11-12-010203.local"}]}),
    );
    assert!(system.snapshots(&volume).is_err());
    snapshot_json(
        json!({"Snapshots":[{"SnapshotName":"com.apple.TimeMachine.bad.local","SnapshotUUID":"154108c8-1cbb-4fae-81f4-c6c53161f2ae"}]}),
    );
    assert!(system.snapshots(&volume).is_err());
}
#[test]
fn system_adapter_uses_only_fixed_readonly_mount_and_creation_commands() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().canonicalize().unwrap();
    let mut system = System {
        execute: raw,
        filesystem: fs_mock,
    };
    let volume = Volume {
        uuid: uuid::Uuid::new_v4().to_string(),
        device: "disk3s5".into(),
        mount: "/fake".into(),
    };
    let snapshot = Snapshot {
        uuid: uuid::Uuid::new_v4().to_string(),
        name: "com.apple.TimeMachine.2026-11-12-010203.local".into(),
    };
    COMMANDS.with(|r| {
        *r.borrow_mut() = vec![(
            "/usr/bin/tmutil".into(),
            vec!["localsnapshot".into()],
            true,
            Ok(b"Created".to_vec()),
        )]
    });
    assert_eq!(system.create(true).unwrap(), "Created");
    COMMANDS.with(|r| {
        *r.borrow_mut() = vec![(
            "/usr/bin/tmutil".into(),
            vec!["localsnapshot".into()],
            false,
            Ok(vec![255]),
        )]
    });
    assert!(system.create(false).is_err());
    let args = vec![
        "-o".into(),
        "ro,nobrowse".into(),
        "-s".into(),
        snapshot.name.clone(),
        "/fake".into(),
        target.to_string_lossy().into(),
    ];
    for (mount, readonly, success) in [
        (target.clone(), true, true),
        (target.clone(), false, false),
        (PathBuf::from("/fake"), true, false),
    ] {
        COMMANDS.with(|r| {
            *r.borrow_mut() = vec![("/sbin/mount_apfs".into(), args.clone(), true, Ok(vec![]))]
        });
        FILESYSTEM.with(|v| *v.borrow_mut() = Some((mount, readonly)));
        assert_eq!(
            system.mount(&volume, &snapshot, &target, true).is_ok(),
            success
        );
    }
    FILESYSTEM.with(|v| *v.borrow_mut() = Some((PathBuf::from("/fake"), false)));
    system.unmount(&target, false).unwrap();
    FILESYSTEM.with(|v| *v.borrow_mut() = Some((target.clone(), true)));
    COMMANDS.with(|r| {
        *r.borrow_mut() = vec![(
            "/sbin/umount".into(),
            vec![target.to_string_lossy().into()],
            true,
            Ok(vec![]),
        )]
    });
    system.unmount(&target, true).unwrap();
}
#[test]
fn command_authorization_requires_terminal_and_never_invokes_arbitrary_sudo() {
    let request = Request {
        program: "/usr/bin/tmutil",
        args: vec!["localsnapshot".into()],
        input: None,
        authorize: true,
    };
    assert!(command(&request, false)
        .unwrap_err()
        .to_string()
        .contains("interactive terminal"));
    let authorized = command(&request, true).unwrap();
    assert_eq!(authorized.get_program(), "/usr/bin/sudo");
    assert_eq!(
        authorized.get_args().collect::<Vec<_>>(),
        vec!["--", "/usr/bin/tmutil", "localsnapshot"]
    );
    let normal = command(
        &Request {
            authorize: false,
            ..request
        },
        false,
    )
    .unwrap();
    assert_eq!(normal.get_program(), "/usr/bin/tmutil");
}
#[test]
fn system_command_runner_handles_input_errors_limits_and_owned_child_timeout() {
    let request = |program, args: Vec<&str>, input| Request {
        program,
        args: args.into_iter().map(String::from).collect(),
        input,
        authorize: false,
    };
    assert_eq!(
        execute(request("/usr/bin/printf", vec!["hello"], None)).unwrap(),
        b"hello"
    );
    assert_eq!(
        execute_with(
            request("/bin/cat", vec![], Some(b"converted".to_vec())),
            Duration::from_secs(5),
            false
        )
        .unwrap(),
        b"converted"
    );
    assert!(execute_with(
        request("/missing-rdrive-checkpoint-tool", vec![], None),
        Duration::from_secs(1),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("cannot run"));
    assert!(execute_with(
        request("/bin/sh", vec!["-c", "printf denied >&2; exit 1"], None),
        Duration::from_secs(5),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("denied"));
    let started = Instant::now();
    assert!(execute_with(
        request("/bin/sh", vec!["-c", "sleep 10"], None),
        Duration::from_millis(30),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(execute_with(
        request("/usr/bin/yes", vec!["x"], None),
        Duration::from_secs(5),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("exceeded"));
    assert!(execute_with(
        request("/bin/sh", vec!["-c", "/usr/bin/yes x >&2"], None),
        Duration::from_secs(5),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("exceeded"));
    assert!(execute_with(
        request("/usr/bin/true", vec![], Some(vec![0; 8 * 1024 * 1024])),
        Duration::from_secs(5),
        false
    )
    .is_err());
}
#[test]
fn public_entrypoints_validate_inputs_before_any_os_mutation() {
    let f = Fixture::new();
    let invalid = Policy {
        enabled: true,
        max_records: 0,
    };
    assert!(status(&f.store.home, &invalid).is_err());
    assert!(create(&f.store, &Policy::default(), false).is_err());
    assert!(list(&f.destination("absent")).is_err());
    assert!(reconcile(&f.store, "not-an-id", false).is_err());
    assert!(export_file(
        &f.store.home,
        "not-an-id",
        Path::new("spool/a"),
        &f.destination("out"),
        false
    )
    .is_err());
    assert!(filesystem(&f.destination("absent")).is_err());
    #[cfg(target_os = "macos")]
    {
        let (mount, _) = filesystem(&f.store.home).unwrap();
        assert!(mount.is_absolute());
        let nul = std::ffi::OsString::from_vec(b"nul\0".to_vec());
        assert!(filesystem(Path::new(&nul)).is_err());
    }
}
#[test]
fn metadata_records_are_bounded_and_nonutf8_paths_are_rejected() {
    let mut f = Fixture::new();
    let id = f.create();
    let path = record_path(&f.store.home, &id).unwrap();
    let good = fs::read(&path).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(128 * 1024 * 1024 + 1)
        .unwrap();
    assert!(load(&f.store.home, &id)
        .unwrap_err()
        .to_string()
        .contains("size bound"));
    state::atomic_write(&path, &good).unwrap();
    #[cfg(target_os = "linux")]
    {
        let bad = root(&f.store.home).join(std::ffi::OsString::from_vec(vec![255]));
        write(&bad, b"bad");
        assert!(records(&f.store.home)
            .unwrap_err()
            .to_string()
            .contains("filename"));
        fs::remove_file(bad).unwrap();
    }
    let record = load(&f.store.home, &id).unwrap();
    let image = f.host.images.values().next().unwrap().clone();
    let marker = image
        .join("state/checkpoints")
        .join(&id)
        .join("manifest.json");
    OpenOptions::new()
        .write(true)
        .open(marker)
        .unwrap()
        .set_len(128 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches_mounted(&image, &record.manifest, false)
        .unwrap_err()
        .to_string()
        .contains("size bound"));
}

#[test]
fn complete_checkpoint_verification_checks_preexisting_snapshots_and_error_paths() {
    let mut f = Fixture::new();
    f.host.snapshots.push(Snapshot {
        uuid: uuid::Uuid::new_v4().to_string(),
        name: "com.apple.TimeMachine.2026-11-11-000000.local".into(),
    });
    let id = f.create();
    let record = load(&f.store.home, &id).unwrap();
    let image = f.host.images.values().next().unwrap().clone();
    let marker = image
        .join("state/checkpoints")
        .join(&id)
        .join("manifest.json");
    fs::remove_file(&marker).unwrap();
    symlink(f.store.home.join("config.json"), &marker).unwrap();
    assert!(matches_mounted(&image, &record.manifest, false).is_err());
    fs::remove_file(&marker).unwrap();
    state::atomic_json(&marker, &record.manifest).unwrap();
    let payload = image.join("state/spool/upload");
    fs::remove_file(&payload).unwrap();
    symlink(f.store.home.join("spool/upload"), &payload).unwrap();
    assert!(matches_mounted(&image, &record.manifest, true).is_err());
    fs::remove_file(&payload).unwrap();
    directory(&payload).unwrap();
    error(
        export_with(
            &f.store.home,
            &id,
            Path::new("spool/upload"),
            &f.destination("out"),
            false,
            &mut f.host,
        ),
        "not a regular file",
    );
    let wrong = f.destination("substitute-volume");
    fs::create_dir_all(wrong.join(f.store.home.strip_prefix("/").unwrap())).unwrap();
    let mut volume = f.host.volume(&f.store.home).unwrap();
    volume.mount = wrong;
    assert!(home_relative(&f.store.home, &volume)
        .unwrap_err()
        .to_string()
        .contains("not on the reported"));
    let smart = f.store.home.join("smart");
    directory(&smart).unwrap();
    directory(&smart.join("empty-domain")).unwrap();
    assert!(queue_leases(&f.store.home).unwrap().is_empty());
}
#[test]
fn record_forget_surfaces_permission_errors_without_touching_system_snapshot() {
    let mut f = Fixture::new();
    let id = f.create();
    let dir = root(&f.store.home).join(&id);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(forget(&f.store, &id).is_err());
    let tomb = root(&f.store.home).join(format!("forgotten-{id}"));
    fs::set_permissions(&tomb, fs::Permissions::from_mode(0o700)).unwrap();
    forget(&f.store, &id).unwrap();
    assert_eq!(f.host.snapshots.len(), 1);
}
#[test]
fn command_runner_handles_inherited_error_stream_and_timeout_with_input() {
    let mut inherited = Command::new("/usr/bin/printf");
    inherited
        .arg("ok")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    assert_eq!(
        execute_command(inherited, None, Duration::from_secs(5), false).unwrap(),
        b"ok"
    );
    let request = Request {
        program: "/bin/sh",
        args: vec!["-c".into(), "sleep 10".into()],
        input: Some(vec![0; 8 * 1024 * 1024]),
        authorize: false,
    };
    assert!(execute_with(request, Duration::from_millis(30), false)
        .unwrap_err()
        .to_string()
        .contains("timed out"));
}

#[test]
fn system_adapter_propagates_disk_tool_failures_without_claiming_capability() {
    let mut system = System {
        execute: raw,
        filesystem: fs_mock,
    };
    FILESYSTEM.with(|v| *v.borrow_mut() = Some((PathBuf::from("/fake"), false)));
    queue_json(volume_json());
    COMMANDS.with(|rows| rows.borrow_mut()[0].3 = Err("disk tool denied".into()));
    assert!(system
        .volume(Path::new("/home"))
        .unwrap_err()
        .to_string()
        .contains("disk tool denied"));
    let volume = Volume {
        uuid: uuid::Uuid::new_v4().to_string(),
        device: "disk3s5".into(),
        mount: "/fake".into(),
    };
    snapshot_json(json!({"Snapshots":[]}));
    COMMANDS.with(|rows| rows.borrow_mut()[0].3 = Err("snapshot query denied".into()));
    assert!(system
        .snapshots(&volume)
        .unwrap_err()
        .to_string()
        .contains("query denied"));
}
#[test]
fn sqlite_open_permission_failure_aborts_before_snapshot_creation() {
    let f = Fixture::new();
    let path = f.store.home.join("history/history.sqlite3");
    write(&path, b"not opened");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    let mut files = BTreeMap::new();
    assert!(walk(&f.store.home, &f.store.home, &mut files, true).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn payload_publication_anchors_owned_directory_and_never_clobbers() {
    let f = Fixture::new();
    let source = f.store.home.join("spool/upload");
    let expected = Content {
        bytes: 15,
        sha256: state::fingerprint(&source).unwrap(),
        exportable: true,
        kind: "queued_upload".into(),
    };
    let mut input = File::open(&source).unwrap();
    let destination = f.destination("existing-direct");
    write(&destination, b"keep");
    assert!(publish_payload(&mut input, &expected, &destination)
        .unwrap_err()
        .to_string()
        .contains("without overwrite"));
    assert_eq!(fs::read(destination).unwrap(), b"keep");
    let public = f.destination("writable-parent");
    directory(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o722)).unwrap();
    let mut input = File::open(&source).unwrap();
    assert!(publish_payload(&mut input, &expected, &public.join("out"))
        .unwrap_err()
        .to_string()
        .contains("other users"));
    fs::set_permissions(&public, fs::Permissions::from_mode(0o500)).unwrap();
    let mut input = File::open(&source).unwrap();
    assert!(publish_payload(&mut input, &expected, &public.join("out")).is_err());
    fs::set_permissions(&public, fs::Permissions::from_mode(0o700)).unwrap();
    let mut input = File::open(&source).unwrap();
    assert!(publish_payload(&mut input, &expected, Path::new("/rdrive-unwritable-root")).is_err());
}

#[test]
fn payload_publication_rejects_invalid_c_filename() {
    let f = Fixture::new();
    let path = f.store.home.join("spool/upload");
    let expected = Content {
        bytes: 15,
        sha256: state::fingerprint(&path).unwrap(),
        exportable: true,
        kind: "queued_upload".into(),
    };
    let destination = f
        ._temp
        .path()
        .join(std::ffi::OsString::from_vec(b"nul\0".to_vec()));
    let mut input = File::open(path).unwrap();
    assert!(publish_payload(&mut input, &expected, &destination).is_err());
}

#[test]
fn command_timeout_without_owned_group_never_signals_caller() {
    let mut child = Command::new("/bin/sleep");
    child
        .arg("10")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let group = unsafe { libc::getpgrp() };
    assert!(
        execute_command(child, None, Duration::from_millis(30), false)
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert_eq!(unsafe { libc::getpgrp() }, group);
}
