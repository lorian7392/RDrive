use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Fixture {
    temp: tempfile::TempDir,
    store: Store,
    host: Fake,
    limits: Limits,
}
struct Fake {
    root: PathBuf,
    mounted: bool,
    missing: bool,
    capacity: u64,
    available: u64,
    unmounts: usize,
    fail_unmount: bool,
    fail_space: bool,
    space_sequence: std::collections::VecDeque<u64>,
}
impl Fake {
    fn mounted(&self, home: &Path, readonly: bool) -> Mounted {
        Mounted {
            root: self.root.clone(),
            instance_id: "00000000-0000-4000-8000-000000000001".into(),
            volume_uuid: "00000000-0000-4000-8000-000000000002".into(),
            capacity_bytes: self.capacity,
            home_binding: binding(home).unwrap(),
            device: "fixture".into(),
            readonly,
        }
    }
}
impl Host for Fake {
    fn mount(
        &mut self,
        home: &Path,
        _: u64,
        initialize: bool,
        readonly: bool,
    ) -> Result<Option<Mounted>> {
        if self.missing && !initialize {
            return Ok(None);
        }
        self.mounted = true;
        let _ = home;
        Ok(Some(self.mounted(home, readonly)))
    }
    fn space(&mut self, _: &Mounted) -> Result<Space> {
        if self.fail_space {
            bail!("space failure");
        }
        Ok(Space {
            used_bytes: 10,
            available_bytes: self.space_sequence.pop_front().unwrap_or(self.available),
        })
    }
    fn unmount(&mut self, _: &Mounted) -> Result<()> {
        self.mounted = false;
        self.unmounts += 1;
        if self.fail_unmount {
            bail!("detach failure");
        }
        Ok(())
    }
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("state")).unwrap();
    let root = temp.path().join("mount");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let limits = Limits {
        max_bytes: 256 * 1024 * 1024,
        max_records: 2,
        auto_prune: true,
    };
    let host = Fake {
        root,
        mounted: false,
        missing: false,
        capacity: limits.max_bytes,
        available: 1024 * 1024 * 128,
        unmounts: 0,
        fail_unmount: false,
        fail_space: false,
        space_sequence: std::collections::VecDeque::new(),
    };
    Fixture {
        temp,
        store,
        host,
        limits,
    }
}
fn payload(f: &Fixture, name: &str, bytes: &[u8]) -> PathBuf {
    let path = f.store.home.join("spool").join(name);
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}
fn uid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}
#[test]
fn creates_deduplicated_checkpoint_and_exports_verified_payload() {
    let mut f = fixture();
    payload(&f, "a", b"original");
    payload(&f, "b", b"original");
    let a = create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    assert_eq!(a["files"], 2);
    assert_eq!(fs::read_dir(f.host.root.join("blobs")).unwrap().count(), 1);
    let out = f.temp.path().join("out");
    export_with(
        &f.store.home,
        &uid(1),
        Path::new("spool/a"),
        &out,
        &f.limits,
        &mut f.host,
    )
    .unwrap();
    assert_eq!(fs::read(out).unwrap(), b"original");
    assert_eq!(
        contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap(),
        true
    );
    assert_eq!(
        contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap(),
        false
    );
    assert_eq!(
        list_with(&f.store.home, &f.limits, &mut f.host).unwrap()["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        inspect_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap()["files"]["spool/a"]
            ["bytes"],
        8
    );
    assert_eq!(
        status_with(&f.store.home, &f.limits, &mut f.host).unwrap()["used_bytes"],
        10
    );
    assert!(!f.host.mounted);
}
#[test]
fn completed_attempt_is_adopted_and_reconciliation_cleans_only_own_partial_stage() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let path = f.host.root.join("staging").join(uid(1));
    directory(&path).unwrap();
    assert_eq!(
        reconcile_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap()["confirmed"],
        true
    );
    assert!(!path.exists());
    directory(&path).unwrap();
    assert_eq!(
        create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap()["adopted"],
        true
    );
    let mount = f.host.mounted(&f.store.home, false);
    let mut record = load(&mount, &uid(1)).unwrap();
    record.id = uid(2);
    let stage = f.host.root.join("staging").join(uid(2));
    directory(&stage).unwrap();
    state::atomic_json(&stage.join("intent.json"), &record).unwrap();
    fs::write(
        stage.join(format!(
            "blob-{}.tmp",
            record.files.values().next().unwrap().sha256
        )),
        b"partial",
    )
    .unwrap();
    for e in fs::read_dir(&stage).unwrap() {
        fs::set_permissions(e.unwrap().path(), fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(create_with(&f.store, &uid(3), &f.limits, &mut f.host).is_err());
    assert_eq!(
        reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()
            ["safe_to_retry_same_id"],
        true
    );
    assert!(!stage.exists());
}
#[test]
fn retention_removes_only_owned_old_records_and_keeps_latest_across_clock_rollback() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    payload(&f, "a", b"two");
    create_with(&f.store, &uid(2), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let mut record = load(&mount, &uid(2)).unwrap();
    record.created = "2001-01-01T00:00:00+00:00".into();
    state::atomic_json(
        &f.host
            .root
            .join("checkpoints")
            .join(format!("{}.json", uid(2))),
        &record,
    )
    .unwrap();
    payload(&f, "a", b"three");
    create_with(&f.store, &uid(3), &f.limits, &mut f.host).unwrap();
    assert!(!contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert!(contains_with(&f.store.home, &uid(3), &f.limits, &mut f.host).unwrap());
    assert_eq!(fs::read_dir(f.host.root.join("blobs")).unwrap().count(), 2);
    f.limits.max_records = 1;
    assert_eq!(
        prune_with(&f.store.home, &f.limits, false, &mut f.host).unwrap()["candidates"],
        1
    );
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert_eq!(
        prune_with(&f.store.home, &f.limits, true, &mut f.host).unwrap()["removed"],
        1
    );
    assert!(contains_with(&f.store.home, &uid(3), &f.limits, &mut f.host).unwrap());
    assert_eq!(
        forget_with(&f.store.home, &uid(3), &f.limits, &mut f.host).unwrap()["forgotten"],
        true
    );
    assert_eq!(fs::read_dir(f.host.root.join("blobs")).unwrap().count(), 0);
}
#[test]
fn insufficient_space_never_discards_the_last_verified_recovery_point() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    payload(&f, "a", b"two");
    create_with(&f.store, &uid(2), &f.limits, &mut f.host).unwrap();
    f.host.available = 0;
    payload(&f, "a", b"three");
    let error = create_with(&f.store, &uid(3), &f.limits, &mut f.host).unwrap_err();
    assert!(error.to_string().contains("insufficient_quota"));
    assert!(!contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert_eq!(fs::read(f.store.home.join("spool/a")).unwrap(), b"three");
    assert_eq!(
        reconcile_with(&f.store.home, &uid(3), &f.limits, &mut f.host).unwrap()["missing"],
        true
    );
    f.limits.auto_prune = false;
    f.host.available = 0;
    assert!(create_with(&f.store, &uid(3), &f.limits, &mut f.host)
        .unwrap_err()
        .to_string()
        .contains("insufficient_quota"));
}
#[test]
fn limits_count_and_missing_store_are_explicit() {
    let mut f = fixture();
    let mut l = f.limits.clone();
    l.max_bytes = 1;
    assert!(l.validate().is_err());
    l.max_bytes = 256 * 1024 * 1024 + 1;
    assert!(l.validate().is_err());
    l.max_bytes = f.limits.max_bytes;
    l.max_records = 0;
    assert!(l.validate().is_err());
    l.max_records = 4097;
    assert!(l.validate().is_err());
    f.host.missing = true;
    assert_eq!(
        status_with(&f.store.home, &f.limits, &mut f.host).unwrap()["initialized"],
        false
    );
    assert_eq!(
        list_with(&f.store.home, &f.limits, &mut f.host).unwrap()["checkpoints"],
        json!([])
    );
    assert!(!contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    assert!(inspect_with(&f.store.home, &uid(1), &f.limits, &mut f.host).is_err());
    assert!(forget_with(&f.store.home, &uid(1), &f.limits, &mut f.host).is_err());
    assert_eq!(
        prune_with(&f.store.home, &f.limits, true, &mut f.host).unwrap()["removed"],
        0
    );
    assert_eq!(
        reconcile_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap()
            ["safe_to_retry_same_id"],
        true
    );
    assert!(export_with(
        &f.store.home,
        &uid(1),
        Path::new("spool/a"),
        &f.temp.path().join("out"),
        &f.limits,
        &mut f.host
    )
    .is_err());
    f.host.missing = false;
    payload(&f, "a", b"one");
    f.limits.auto_prune = false;
    f.limits.max_records = 1;
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    assert!(create_with(&f.store, &uid(2), &f.limits, &mut f.host)
        .unwrap_err()
        .to_string()
        .contains("record limit"));
}
#[test]
fn unsafe_payloads_and_path_replacement_are_refused() {
    let f = fixture();
    let path = payload(&f, "a", b"original");
    let error = checked_file_with(&path, |_| {
        fs::remove_file(&path)?;
        fs::write(&path, b"replacement")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("changed while"));
    let linked = f.store.home.join("spool/b");
    fs::hard_link(&path, &linked).unwrap();
    assert!(source_file(&path).is_err());
    fs::remove_file(linked).unwrap();
    let link = f.store.home.join("spool/link");
    symlink(&path, &link).unwrap();
    assert!(capture(&f.store)
        .unwrap_err()
        .to_string()
        .contains("symbolic"));
    fs::remove_file(link).unwrap();
    assert!(source(path.clone(), Path::new("spool/a"), Some("wrong")).is_err());
    let blobs = f.store.home.join("history/blobs");
    fs::create_dir_all(blobs.join("nested")).unwrap();
    for p in [
        f.store.home.join("history"),
        blobs.clone(),
        blobs.join("nested"),
    ] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(capture(&f.store).unwrap_err().to_string().contains("flat"));
    fs::remove_dir(blobs.join("nested")).unwrap();
    fs::write(blobs.join("bad-name"), b"bad").unwrap();
    fs::set_permissions(blobs.join("bad-name"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(capture(&f.store).is_err());
}
#[test]
fn corrupt_or_foreign_records_and_blobs_are_preserved_and_never_exported() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let original = load(&mount, &uid(1)).unwrap();
    let record_path = f
        .host
        .root
        .join("checkpoints")
        .join(format!("{}.json", uid(1)));
    let mut changed = original.clone();
    changed.id = uid(2);
    state::atomic_json(&record_path, &changed).unwrap();
    assert!(list_with(&f.store.home, &f.limits, &mut f.host).is_err());
    state::atomic_json(&record_path, &original).unwrap();
    for mutate in [0, 1, 2, 3, 4, 5] {
        let mut record = original.clone();
        match mutate {
            0 => record.version = 2,
            1 => record.home = "foreign".into(),
            2 => record.sequence = 0,
            3 => record.created = "bad".into(),
            4 => record.files.get_mut(Path::new("spool/a")).unwrap().sha256 = "bad".into(),
            _ => {
                let item = record.files.remove(Path::new("spool/a")).unwrap();
                record.files.insert(PathBuf::from("../outside"), item);
            }
        }
        assert!(manifest(&mount, record).is_err());
    }
    let blob = f
        .host
        .root
        .join("blobs")
        .join(&original.files[Path::new("spool/a")].sha256);
    fs::write(&blob, b"two").unwrap();
    assert!(export_with(
        &f.store.home,
        &uid(1),
        Path::new("spool/a"),
        &f.temp.path().join("out"),
        &f.limits,
        &mut f.host
    )
    .is_err());
    assert!(reconcile_with(&f.store.home, &uid(1), &f.limits, &mut f.host).is_err());
    assert_eq!(fs::read(blob).unwrap(), b"two");
    assert!(record_path.exists());
}
#[test]
fn unexpected_files_and_staging_are_never_cleaned() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let mut record = load(&mount, &uid(1)).unwrap();
    record.id = uid(2);
    let stage = f.host.root.join("staging").join(uid(2));
    directory(&stage).unwrap();
    state::atomic_json(&stage.join("intent.json"), &record).unwrap();
    fs::write(stage.join("user.data"), b"keep").unwrap();
    fs::set_permissions(stage.join("user.data"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).is_err());
    assert_eq!(fs::read(stage.join("user.data")).unwrap(), b"keep");
    fs::remove_file(stage.join("user.data")).unwrap();
    let mut mismatch = record.clone();
    mismatch.id = uid(3);
    state::atomic_json(&stage.join("intent.json"), &mismatch).unwrap();
    assert!(stages(&mount).is_err());
    state::atomic_json(&stage.join("intent.json"), &record).unwrap();
    clean_stage(&mount, &record).unwrap();
    fs::write(f.host.root.join("blobs/user.data"), b"keep").unwrap();
    fs::set_permissions(
        f.host.root.join("blobs/user.data"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(garbage_collect(&mount).is_err());
    assert!(f.host.root.join("blobs/user.data").exists());
    fs::remove_file(f.host.root.join("blobs/user.data")).unwrap();
    fs::write(f.host.root.join("checkpoints/readme"), b"keep").unwrap();
    assert!(rows(&mount).is_err());
}
#[test]
fn detachment_and_read_failures_do_not_claim_success() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    f.host.fail_space = true;
    assert!(status_with(&f.store.home, &f.limits, &mut f.host).is_err());
    assert!(!f.host.mounted);
    f.host.fail_space = false;
    f.host.fail_unmount = true;
    assert!(status_with(&f.store.home, &f.limits, &mut f.host)
        .unwrap_err()
        .to_string()
        .contains("detach"));
    assert!(inspect_with(&f.store.home, &uid(2), &f.limits, &mut f.host).is_err());
    f.host.fail_unmount = false;
    f.host.capacity -= 512;
    assert!(status_with(&f.store.home, &f.limits, &mut f.host).is_err());
}
#[test]
fn export_guards_and_live_running_work_are_preserved() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    for out in [PathBuf::from("relative"), f.store.home.join("overwrite")] {
        assert!(export_with(
            &f.store.home,
            &uid(1),
            Path::new("spool/a"),
            &out,
            &f.limits,
            &mut f.host
        )
        .is_err());
    }
    let out = f.temp.path().join("exists");
    fs::write(&out, b"keep").unwrap();
    assert!(export_with(
        &f.store.home,
        &uid(1),
        Path::new("spool/a"),
        &out,
        &f.limits,
        &mut f.host
    )
    .is_err());
    assert_eq!(fs::read(out).unwrap(), b"keep");
    assert!(export_with(
        &f.store.home,
        &uid(1),
        Path::new("spool/missing"),
        &f.temp.path().join("out"),
        &f.limits,
        &mut f.host
    )
    .is_err());
    f.store.db.jobs.push(crate::state::Job {
        id: "active".into(),
        profile: "test".into(),
        remote: "doc".into(),
        snapshot: "a".into(),
        sha256: state::key("one"),
        size: 3,
        created: state::now(),
        status: "running".into(),
        attempts: 1,
        error: None,
        expected: None,
        overwrite: false,
        version: false,
        resume: false,
        version_prepared: false,
    });
    assert!(create_with(&f.store, &uid(2), &f.limits, &mut f.host)
        .unwrap_err()
        .to_string()
        .contains("running"));
}
#[test]
fn public_uninitialized_queries_do_not_create_or_modify_payloads() {
    let f = fixture();
    assert!(!contains(&f.store.home, &uid(1)).unwrap());
    assert_eq!(existing_capacity(&f.store.home).unwrap(), None);
    validate_capacity(&f.store.home, f.limits.max_bytes).unwrap();
    assert_eq!(
        status(&f.store.home, &f.limits).unwrap()["initialized"],
        false
    );
    assert_eq!(
        list(&f.store.home, &f.limits).unwrap()["checkpoints"],
        json!([])
    );
    assert!(inspect(&f.store.home, &uid(1), &f.limits).is_err());
    assert_eq!(
        reconcile(&f.store.home, &uid(1), &f.limits).unwrap()["missing"],
        true
    );
    assert!(forget(&f.store.home, &uid(1), &f.limits).is_err());
    assert_eq!(
        prune(&f.store.home, &f.limits, false).unwrap()["candidates"],
        0
    );
    assert!(export_file(
        &f.store.home,
        &uid(1),
        Path::new("spool/a"),
        &f.temp.path().join("out"),
        &f.limits
    )
    .is_err());
    assert!(create_planned(&f.store, "bad", &f.limits).is_err());
    assert!(contains(&f.store.home, "bad").is_err());
}
#[test]
fn empty_prepared_stage_is_safely_retryable_without_removing_unknown_content() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let stage = f.host.root.join("staging").join(uid(2));
    directory(&stage).unwrap();
    assert_eq!(
        reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()["missing"],
        true
    );
    assert!(!stage.exists());
    directory(&stage).unwrap();
    fs::write(stage.join("user.txt"), b"keep").unwrap();
    assert!(reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).is_err());
    assert_eq!(fs::read(stage.join("user.txt")).unwrap(), b"keep");
}
#[test]
fn platform_identity_payload_bounds_busy_files_and_failed_unlink_are_checked() {
    let mut f = fixture();
    let path = payload(&f, "a", b"one");
    let exclusive = File::open(&path).unwrap();
    assert_eq!(
        unsafe { libc::flock(exclusive.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(source_file(&path).unwrap_err().to_string().contains("busy"));
    drop(exclusive);
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let record = load(&mount, &uid(1)).unwrap();
    assert!(same_device(1, 2).is_err());
    assert!(validate_capture_size(1_000_001).is_err());
    assert!(validate_capture_size(1_000_000).is_ok());
    assert!(remove_record_with(&mount, &record, |_| bail!("synthetic unlink failure")).is_err());
    assert!(load(&mount, &uid(1)).is_ok());
    let oversized = f.temp.path().join("oversized");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&oversized)
        .unwrap();
    file.set_len(128 * 1024 * 1024 + 1).unwrap();
    assert!(read_json::<Manifest>(&oversized).is_err());
    let mut foreign = mount.clone();
    foreign.home_binding = "foreign".into();
    assert!(layout(&foreign, &f.store.home, false).is_err());
    let mut bad = record.clone();
    bad.version = 2;
    state::atomic_json(
        &f.host
            .root
            .join("checkpoints")
            .join(format!("{}.json", uid(1))),
        &bad,
    )
    .unwrap();
    assert!(load(&mount, &uid(1)).is_err());
}
#[test]
fn immutable_tree_variants_and_current_account_cache_are_verified() {
    use crate::{
        backend::Entry,
        config::{Profile, Protocol},
        state::CacheItem,
    };
    let mut f = fixture();
    let dirty = f.store.home.join("smart/domain/dirty");
    fs::create_dir_all(&dirty).unwrap();
    for p in [
        f.store.home.join("smart"),
        f.store.home.join("smart/domain"),
        dirty.clone(),
    ] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(dirty.join("edit.data"), b"offline").unwrap();
    fs::set_permissions(dirty.join("edit.data"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(f.store.home.join("smart/domain/ignored.json"), b"metadata").unwrap();
    let blobs = f.store.home.join("history/blobs");
    fs::create_dir_all(&blobs).unwrap();
    for p in [f.store.home.join("history"), blobs.clone()] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let hash = state::key("history");
    fs::write(blobs.join(&hash), b"history").unwrap();
    fs::set_permissions(blobs.join(&hash), fs::Permissions::from_mode(0o600)).unwrap();
    f.store.config.profiles.insert(
        "cache".into(),
        Profile {
            protocol: Protocol::Webdav,
            url: "http://127.0.0.1:9/dav".into(),
            username: None,
            password_env: None,
            keychain_internet: false,
            allow_http: true,
            timeout_secs: 5,
            smb_encryption: true,
            nextcloud_upload_url: None,
        },
    );
    let cached = f.store.home.join("cache/object");
    fs::write(&cached, b"cached").unwrap();
    fs::set_permissions(&cached, fs::Permissions::from_mode(0o600)).unwrap();
    let cache_item = CacheItem {
        profile: "cache".into(),
        remote: "document".into(),
        object: "object".into(),
        entry: Entry {
            path: "document".into(),
            is_dir: false,
            size: 6,
            etag: None,
            modified: None,
        },
        sha256: state::key("cached"),
        pinned: false,
        accessed: state::now(),
        scope: Some(crate::cache::scope(&f.store, "cache").unwrap()),
    };
    f.store.db.cache.insert("cached".into(), cache_item.clone());
    let out = capture(&f.store).unwrap();
    assert_eq!(out.len(), 3);
    assert!(out.contains_key(Path::new("cache/object")));
    assert!(out.contains_key(Path::new("smart/domain/dirty/edit.data")));
    f.store.db.cache.get_mut("cached").unwrap().scope = None;
    assert!(capture(&f.store)
        .unwrap_err()
        .to_string()
        .contains("unbound"));
    f.store.db.cache.insert("cached".into(), cache_item);
    f.store.db.cache.get_mut("cached").unwrap().entry.size = 7;
    assert!(capture(&f.store).unwrap_err().to_string().contains("size"));
    let mut out = BTreeMap::new();
    assert!(collect_tree(
        Path::new("/dev"),
        &f.store.home.join("spool"),
        Path::new("spool"),
        &mut out,
        false
    )
    .is_err());
}
#[test]
fn source_copy_races_and_duplicate_sequences_do_not_publish_or_prune() {
    let mut f = fixture();
    let source_path = payload(&f, "a", b"one");
    let selected = source(source_path.clone(), Path::new("spool/a"), None).unwrap();
    fs::write(&source_path, b"new content").unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    layout(&mount, &f.store.home, true).unwrap();
    let stage = mount.root.join("staging").join(uid(1));
    directory(&stage).unwrap();
    assert!(copy_blob(&mount, &stage, &selected)
        .unwrap_err()
        .to_string()
        .contains("changed"));
    for e in fs::read_dir(&stage).unwrap() {
        fs::remove_file(e.unwrap().path()).unwrap();
    }
    fs::remove_dir(stage).unwrap();
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let first = load(&mount, &uid(1)).unwrap();
    let mut duplicate = first.clone();
    duplicate.id = uid(2);
    state::atomic_json(
        &mount
            .root
            .join("checkpoints")
            .join(format!("{}.json", uid(2))),
        &duplicate,
    )
    .unwrap();
    assert!(rows(&mount).unwrap_err().to_string().contains("sequence"));
    fs::remove_file(
        mount
            .root
            .join("checkpoints")
            .join(format!("{}.json", uid(2))),
    )
    .unwrap();
    let stage = mount.root.join("staging").join(uid(1));
    directory(&stage).unwrap();
    fs::write(stage.join("unexpected"), b"keep").unwrap();
    assert!(clean_committed_stage(&mount, &uid(1)).is_err());
    assert!(clean_committed_stage(&mount, &uid(3)).is_ok());
}
#[test]
fn prepared_atomic_manifest_write_failure_recovers_without_touching_last_checkpoint() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let stage = f.host.root.join("staging").join(uid(2));
    directory(&stage).unwrap();
    let name = format!(".rdrive-{}.tmp", uid(50));
    fs::write(stage.join(&name), b"partial JSON").unwrap();
    fs::set_permissions(stage.join(&name), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()["missing"],
        true
    );
    assert!(!stage.exists());
    assert!(contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    let mount = f.host.mounted(&f.store.home, false);
    let mut record = load(&mount, &uid(1)).unwrap();
    record.id = uid(2);
    directory(&stage).unwrap();
    state::atomic_json(&stage.join("intent.json"), &record).unwrap();
    fs::write(stage.join(&name), b"partial").unwrap();
    fs::set_permissions(stage.join(&name), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        reconcile_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()["missing"],
        true
    );
    let blobs = f.store.home.join("history/blobs");
    fs::create_dir_all(&blobs).unwrap();
    for p in [f.store.home.join("history"), blobs.clone()] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let temporary = blobs.join(format!(".capture-{}", uid(60)));
    fs::write(&temporary, b"mutable partial capture").unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!capture(&f.store)
        .unwrap()
        .keys()
        .any(|p| p.to_string_lossy().contains(".capture-")));
    assert!(temporary.exists());
}
#[test]
fn unconfirmed_ids_remain_discoverable_and_routable_for_recovery() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let mut record = load(&mount, &uid(1)).unwrap();
    record.id = uid(2);
    let stage = mount.root.join("staging").join(uid(2));
    directory(&stage).unwrap();
    state::atomic_json(&stage.join("intent.json"), &record).unwrap();
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert_eq!(
        inspect_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()["state"],
        "unconfirmed"
    );
    assert_eq!(
        list_with(&f.store.home, &f.limits, &mut f.host).unwrap()["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    clean_stage(&mount, &record).unwrap();
    directory(&stage).unwrap();
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert_eq!(
        inspect_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap()["state"],
        "preparing"
    );
    assert_eq!(
        list_with(&f.store.home, &f.limits, &mut f.host).unwrap()["checkpoints"][1]["state"],
        "preparing"
    );
    assert!(create_with(&f.store, &uid(3), &f.limits, &mut f.host).is_err());
}
#[test]
fn persisted_vault_capacity_routes_ids_and_corrupt_latest_stops_automatic_cleanup() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    assert!(contains_capacity(
        &f.store.home,
        &uid(1),
        Some(f.limits.max_bytes),
        &mut f.host
    )
    .unwrap());
    assert!(!contains_capacity(&f.store.home, &uid(1), None, &mut f.host).unwrap());
    payload(&f, "a", b"two");
    create_with(&f.store, &uid(2), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    let record = load(&mount, &uid(2)).unwrap();
    fs::write(
        mount
            .root
            .join("blobs")
            .join(&record.files[Path::new("spool/a")].sha256),
        b"bad",
    )
    .unwrap();
    f.host.available = 0;
    assert!(create_with(&f.store, &uid(3), &f.limits, &mut f.host)
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    assert!(contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    f.limits.max_records = 1;
    assert!(prune_with(&f.store.home, &f.limits, true, &mut f.host).is_err());
    assert!(contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
}
#[test]
fn truncated_record_and_commit_before_stage_cleanup_are_handled_without_duplicate_listing() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    let mount = f.host.mounted(&f.store.home, false);
    directory(&mount.root.join("staging").join(uid(1))).unwrap();
    assert_eq!(
        list_with(&f.store.home, &f.limits, &mut f.host).unwrap()["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reconcile_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap()["confirmed"],
        true
    );
    let path = mount
        .root
        .join("checkpoints")
        .join(format!("{}.json", uid(1)));
    fs::write(&path, b"{ truncated").unwrap();
    assert!(load(&mount, &uid(1)).is_err());
    assert!(list_with(&f.store.home, &f.limits, &mut f.host).is_err());
    assert!(path.exists());
}
#[test]
fn capacity_cleanup_retries_space_measurement_before_publishing_new_checkpoint() {
    let mut f = fixture();
    payload(&f, "a", b"one");
    create_with(&f.store, &uid(1), &f.limits, &mut f.host).unwrap();
    payload(&f, "a", b"two");
    create_with(&f.store, &uid(2), &f.limits, &mut f.host).unwrap();
    f.host.space_sequence = [0, 128 * 1024 * 1024].into();
    payload(&f, "a", b"three");
    create_with(&f.store, &uid(3), &f.limits, &mut f.host).unwrap();
    assert!(!contains_with(&f.store.home, &uid(1), &f.limits, &mut f.host).unwrap());
    assert!(contains_with(&f.store.home, &uid(2), &f.limits, &mut f.host).unwrap());
    assert!(contains_with(&f.store.home, &uid(3), &f.limits, &mut f.host).unwrap());
    assert_eq!(fs::read(f.store.home.join("spool/a")).unwrap(), b"three");
}
