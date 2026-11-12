use super::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::{ffi::OsStringExt, fs::symlink},
    sync::{Mutex, MutexGuard},
};
static TEST_LOCK: Mutex<()> = Mutex::new(());
static STATE: Mutex<Option<Mock>> = Mutex::new(None);
struct Mock {
    image: PathBuf,
    mount: PathBuf,
    label: String,
    attached: bool,
    readonly: bool,
    version: Vec<u8>,
    help: Vec<u8>,
    overrides: BTreeMap<&'static str, Value>,
    requests: Vec<Request>,
    failure: Option<String>,
    partial_failure: bool,
    keep_attached: bool,
    honor_mode: bool,
    statistics: Statistics,
}
struct Fixture {
    _lock: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    home: PathBuf,
    system: System,
}
fn update<T>(body: impl FnOnce(&mut Mock) -> T) -> T {
    body(STATE.lock().unwrap().as_mut().unwrap())
}
fn output(stdout: Vec<u8>) -> Output {
    Output {
        code: Some(0),
        stdout,
        stderr: vec![],
    }
}
fn fake(request: Request) -> Result<Output> {
    update(|s| {
        let mut recorded = request.clone();
        recorded.lease = None;
        s.requests.push(recorded);
        let key = format!("{} {}", request.program, request.args.join(" "));
        let failed = s.failure.as_ref().is_some_and(|f| key.contains(f));
        if failed && !s.partial_failure {
            return Ok(Output {
                code: Some(3),
                stdout: vec![],
                stderr: b"synthetic image failure".to_vec(),
            });
        }
        if request.program == "/usr/bin/plutil" {
            return Ok(output(request.input.unwrap()));
        }
        if request.program == "/usr/bin/sw_vers" {
            return Ok(output(s.version.clone()));
        }
        if request.args.last().is_some_and(|v| v == "--help") {
            return Ok(output(s.help.clone()));
        }
        if request.args.first().is_some_and(|v| v == "create") {
            let pos = request.args.iter().position(|x| x == "-sectors").unwrap();
            let size = request.args[pos + 1].parse::<u64>()? * 512;
            let pos = request.args.iter().position(|x| x == "-volname").unwrap();
            s.label = request.args[pos + 1].clone();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&s.image)?;
            file.set_len(size)?;
            return Ok(output(vec![]));
        }
        if request.args.iter().any(|v| v == "attach") {
            s.attached = true;
            s.readonly = s.honor_mode
                && request
                    .args
                    .iter()
                    .any(|v| v == "-readonly" || v == "--readOnly");
            if failed {
                return Ok(Output {
                    code: Some(7),
                    stdout: vec![],
                    stderr: b"partial attachment failed".to_vec(),
                });
            }
            return Ok(output(vec![]));
        }
        if request.args.first().is_some_and(|v| v == "detach") {
            if !s.keep_attached {
                s.attached = false;
                s.overrides.remove("images");
            }
            return Ok(output(vec![]));
        }
        if request.program == "/usr/bin/hdiutil" {
            let value=s.overrides.get("images").cloned().unwrap_or_else(||json!({"images":if s.attached{vec![json!({"image-path":s.image,"system-entities":[{"dev-entry":"/dev/disk90","content-hint":"GUID_partition_scheme"},{"dev-entry":"/dev/disk91s1","mount-point":s.mount}]})]}else{vec![]}}));
            return Ok(output(serde_json::to_vec(&value)?));
        }
        let value=s.overrides.get("volume").cloned().unwrap_or_else(||json!({"FilesystemType":"apfs","APFSSnapshot":false,"MountPoint":s.mount,"DeviceIdentifier":"disk91s1","VolumeName":s.label,"VolumeUUID":"56468B81-9999-4F15-B2B7-1D4341330D00"}));
        Ok(output(serde_json::to_vec(&value)?))
    })
}
fn stats(_: &Path) -> Result<Statistics> {
    Ok(update(|s| Statistics {
        readonly: s.readonly,
        ..s.statistics
    }))
}
impl Fixture {
    fn new() -> Self {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        directory(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let p = home.join("checkpoints/vault");
        *STATE.lock().unwrap() = Some(Mock {
            image: p.join("vault.dmg"),
            mount: p.join("mount"),
            label: String::new(),
            attached: false,
            readonly: false,
            version: b"27.2\n".to_vec(),
            help: b"--mountPoint --readOnly --mountOptions".to_vec(),
            overrides: BTreeMap::new(),
            requests: vec![],
            failure: None,
            partial_failure: false,
            keep_attached: false,
            honor_mode: true,
            statistics: Statistics {
                total: MIN_BYTES - 40960,
                used: 4096,
                available: MIN_BYTES - 45056,
                readonly: false,
            },
        });
        Self {
            _lock: guard,
            _temp: temp,
            home,
            system: System {
                execute: fake,
                statistics: stats,
                supported: || true,
                family: None,
                operation_lock: None,
            },
        }
    }
    fn mount(&mut self) -> Mounted {
        self.system
            .mount(&self.home, MIN_BYTES, true, false)
            .unwrap()
            .unwrap()
    }
    fn paths(&self) -> Paths {
        paths(&self.home, false).unwrap().unwrap()
    }
    fn value(&self) -> Descriptor {
        descriptor(&self.paths()).unwrap().unwrap()
    }
    fn edit(&self, body: impl FnOnce(&mut Descriptor)) {
        let p = self.paths();
        let mut d = descriptor(&p).unwrap().unwrap();
        body(&mut d);
        save(&p, &d).unwrap();
    }
}
#[test]
fn capacity_and_platform_preconditions_are_checked_without_creating_storage() {
    let mut f = Fixture::new();
    assert_eq!(System::capacity(&f.home).unwrap(), None);
    System::validate_capacity(&f.home, MIN_BYTES).unwrap();
    assert!(f
        .system
        .mount(&f.home, MIN_BYTES, false, true)
        .unwrap()
        .is_none());
    assert!(!f.home.join("checkpoints").exists());
    for value in [1, MIN_BYTES + 1, u64::MAX] {
        assert!(System::validate_capacity(&f.home, value).is_err());
    }
    assert!(f.system.mount(&f.home, MIN_BYTES, true, true).is_err());
    f.system.supported = || false;
    assert!(f.system.mount(&f.home, MIN_BYTES, false, true).is_err());
    assert!(device("disk9").is_err());
    assert!(device("/dev/disk").is_err());
    assert!(device("/dev/disk9;").is_err());
    assert!(private(Path::new("/"), true).is_err());
    assert!(directory(&f.home.join("missing/child")).is_err());
    let alias = f._temp.path().join("alias");
    symlink(&f.home, &alias).unwrap();
    assert!(paths(&alias, false).is_err());
    let nul = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0]));
    assert!(statistics(&nul).is_err());
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(home_binding(&invalid).is_err());
    assert!(text(&invalid).is_err());
}
#[test]
fn owned_vault_roundtrip_and_readonly_recovery_do_not_resize_or_reinitialize() {
    let mut f = Fixture::new();
    let m = f.mount();
    assert_eq!(System::capacity(&f.home).unwrap(), Some(MIN_BYTES));
    System::validate_capacity(&f.home, MIN_BYTES).unwrap();
    assert!(System::validate_capacity(&f.home, MIN_BYTES + 512).is_err());
    let space = f.system.space(&m).unwrap();
    assert_eq!(space.used_bytes, 4096);
    assert_eq!(space.available_bytes, MIN_BYTES - 45056);
    let again = f
        .system
        .mount(&f.home, MIN_BYTES, false, false)
        .unwrap()
        .unwrap();
    assert_eq!(again.instance_id, m.instance_id);
    let ro = f
        .system
        .mount(&f.home, MIN_BYTES, false, true)
        .unwrap()
        .unwrap();
    assert!(ro.readonly);
    f.system.space(&ro).unwrap();
    f.system.unmount(&ro).unwrap();
    assert!(f.system.space(&m).is_err());
    assert!(f.system.unmount(&m).is_err());
    let req = update(|s| s.requests.clone());
    assert_eq!(
        req.iter()
            .filter(|r| r.args.first().is_some_and(|a| a == "create"))
            .count(),
        1
    );
    assert!(req
        .iter()
        .any(|r| r.program == "/usr/sbin/diskutil" && r.args.iter().any(|a| a == "--readOnly")));
    assert!(f
        .system
        .mount(&f.home, MIN_BYTES + 512, false, true)
        .is_err());
}
#[test]
fn legacy_systems_use_owner_parameters_and_modern_capability_is_checked() {
    let mut f = Fixture::new();
    update(|s| s.version = b"26.6".to_vec());
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    let ro = f
        .system
        .mount(&f.home, MIN_BYTES, false, true)
        .unwrap()
        .unwrap();
    f.system.unmount(&ro).unwrap();
    assert!(update(|s| s.requests.iter().any(|r| r.program
        == "/usr/bin/hdiutil"
        && r.args.iter().any(|a| a == "-readonly"))));
    for version in [b"12.1".as_slice(), b"bad", b"\xff"] {
        f.system.family = None;
        update(|s| s.version = version.to_vec());
        assert!(f.system.family().is_err());
    }
    f.system.family = None;
    update(|s| {
        s.version = b"27.2".to_vec();
        s.help = b"unsupported".to_vec();
    });
    assert!(f.system.family().is_err());
}
#[test]
fn crash_preparation_is_resumed_by_owned_writable_reconciliation_and_ownership_is_rechecked() {
    let mut f = Fixture::new();
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    f.edit(|d| {
        d.ready = false;
        d.volume_uuid = None;
    });
    assert!(f.system.mount(&f.home, MIN_BYTES, false, true).is_err());
    let recovered = f
        .system
        .mount(&f.home, MIN_BYTES, false, false)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.instance_id, m.instance_id);
    f.system.unmount(&recovered).unwrap();
    let p = f.paths();
    fs::remove_file(p.mount.join(".rdrive-vault.json")).unwrap();
    f.edit(|d| {
        d.ready = false;
        d.volume_uuid = None;
        d.image = None;
    });
    let recovered = f.mount();
    f.system.unmount(&recovered).unwrap();
    // A crash before the mount completes leaves only a mapped device. Reap only
    // that exact private image before attaching its one expected volume again.
    update(|s| {
        s.attached = true;
        s.overrides.insert("images",json!({"images":[{"image-path":s.image,"system-entities":[{"dev-entry":"/dev/disk90"}]}]}));
    });
    // Clear the synthetic incomplete mapping after its precise detach.
    let image = update(|s| s.image.clone());
    let current = f.system.attached(&p).unwrap().unwrap();
    assert!(current.1.is_empty());
    let repaired = f.mount();
    f.system.unmount(&repaired).unwrap();
    update(|s| {
        s.overrides.remove("images");
    });
    assert_eq!(image, p.image);
    let valid = f.mount();
    f.system.unmount(&valid).unwrap();
}
#[test]
fn tampered_descriptors_images_links_and_marker_are_rejected() {
    let mut f = Fixture::new();
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    let p = f.paths();
    let original = fs::read(&p.descriptor).unwrap();
    for field in [
        "version",
        "home_binding",
        "instance_id",
        "capacity_bytes",
        "volume_uuid",
    ] {
        let mut d: Value = serde_json::from_slice(&original).unwrap();
        d[field] = match field {
            "version" => json!(2),
            "home_binding" => json!("foreign"),
            "instance_id" => json!(uuid::Uuid::new_v4().to_string().to_uppercase()),
            "capacity_bytes" => json!(MIN_BYTES + 512),
            _ => json!("invalid"),
        };
        crate::state::atomic_json(&p.descriptor, &d).unwrap();
        assert!(descriptor(&p).is_err());
        fs::write(&p.descriptor, &original).unwrap();
    }
    fs::write(&p.descriptor, vec![b' '; 16385]).unwrap();
    assert!(descriptor(&p).is_err());
    fs::write(&p.descriptor, &original).unwrap();
    fs::write(&p.descriptor, b"bad").unwrap();
    assert!(descriptor(&p).is_err());
    fs::write(&p.descriptor, &original).unwrap();
    let linked = p.vault.join("hardlink");
    fs::hard_link(&p.image, &linked).unwrap();
    assert!(descriptor(&p).is_err());
    fs::remove_file(&linked).unwrap();
    let image_backup = p.vault.join("old-image");
    fs::rename(&p.image, &image_backup).unwrap();
    symlink(&image_backup, &p.image).unwrap();
    assert!(descriptor(&p).is_err());
    fs::remove_file(&p.image).unwrap();
    fs::rename(&image_backup, &p.image).unwrap();
    let marker = p.mount.join(".rdrive-vault.json");
    let raw = fs::read(&marker).unwrap();
    fs::write(&marker, b"{}").unwrap();
    assert!(f.system.mount(&f.home, MIN_BYTES, false, true).is_err());
    assert!(!update(|s| s.attached));
    fs::write(&marker, &raw).unwrap();
    fs::set_permissions(&p.descriptor, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(descriptor(&p).is_err());
    fs::set_permissions(&p.descriptor, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&p.descriptor).unwrap();
    assert!(descriptor(&p).is_err());
}
#[test]
fn absent_preparing_image_is_created_and_incorrect_sizes_do_not_publish_ready() {
    let mut f = Fixture::new();
    let p = paths(&f.home, true).unwrap().unwrap();
    let pending = Descriptor {
        version: 1,
        home_binding: home_binding(&f.home).unwrap(),
        instance_id: uuid::Uuid::new_v4().to_string(),
        capacity_bytes: MIN_BYTES,
        host_lock: {
            let _lease = operation_lock(&p, None).unwrap();
            lock_identity(&p.host_lock).unwrap()
        },
        image: None,
        volume_uuid: None,
        ready: false,
    };
    save(&p, &pending).unwrap();
    let m = f
        .system
        .mount(&f.home, MIN_BYTES, false, false)
        .unwrap()
        .unwrap();
    f.system.unmount(&m).unwrap();
    f.edit(|d| {
        d.image = None;
        d.ready = false;
        d.volume_uuid = None;
    });
    OpenOptions::new()
        .write(true)
        .open(&p.image)
        .unwrap()
        .set_len(32)
        .unwrap();
    assert!(f.system.mount(&f.home, MIN_BYTES, true, false).is_err());
}
#[test]
fn attachment_and_volume_metadata_refuse_foreign_or_ambiguous_devices() {
    let mut f = Fixture::new();
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    let p = f.paths();
    for images in [
        json!({}),
        json!({"images":[{"image-path":p.image},{"image-path":p.image}]}),
        json!({"images":[{"image-path":p.image}]}),
        json!({"images":[{"image-path":p.image,"system-entities":[{"dev-entry":"bad","mount-point":p.mount}]}]}),
        json!({"images":[{"image-path":p.image,"system-entities":[{"dev-entry":"/dev/disk90"},{"dev-entry":"/dev/disk91s1","mount-point":"/elsewhere"}]}]}),
        json!({"images":[{"image-path":p.image,"system-entities":[{"dev-entry":"/dev/disk90"},{"mount-point":p.mount}]}]}),
        json!({"images":[{"image-path":p.image,"system-entities":[{"dev-entry":"/dev/disk90"},{"dev-entry":"/dev/not-disk","mount-point":p.mount}]}]}),
    ] {
        update(|s| {
            s.overrides.insert("images", images);
        });
        assert!(f.system.attached(&p).is_err());
    }
    update(|s| {
        s.overrides.remove("images");
    });
    let good = f.mount();
    let base = f
        .system
        .json(
            "/usr/sbin/diskutil",
            vec!["info".into(), "-plist".into(), "/dev/disk91s1".into()],
        )
        .unwrap();
    for (field, value) in [
        ("VolumeUUID", Value::Null),
        ("VolumeUUID", json!("bad")),
        ("VolumeUUID", json!(uuid::Uuid::new_v4().to_string())),
        ("FilesystemType", json!("hfs")),
        ("APFSSnapshot", json!(true)),
        ("MountPoint", json!("/elsewhere")),
        ("DeviceIdentifier", json!("disk5")),
        ("VolumeName", json!("foreign")),
    ] {
        let mut v = base.clone();
        v[field] = value;
        update(|s| {
            s.overrides.insert("volume", v);
        });
        assert!(f
            .system
            .verify(&p, &f.value(), &good.device, "/dev/disk91s1", false)
            .is_err());
    }
    update(|s| {
        s.overrides.remove("volume");
    });
    f.system.unmount(&good).unwrap();
}
#[test]
fn state_handle_changes_space_and_detach_fail_closed() {
    let mut f = Fixture::new();
    let m = f.mount();
    let p = f.paths();
    let mut wrong = m.clone();
    wrong.root = PathBuf::from("/bad");
    assert!(f.system.space(&wrong).is_err());
    wrong = m.clone();
    wrong.instance_id = uuid::Uuid::new_v4().to_string();
    assert!(f.system.space(&wrong).is_err());
    wrong = m.clone();
    wrong.device = "/dev/disk95".into();
    assert!(f.system.space(&wrong).is_err());
    update(|s| s.readonly = true);
    assert!(f.system.space(&m).is_err());
    update(|s| {
        s.readonly = false;
        s.statistics.total = MIN_BYTES + 1;
    });
    assert!(f.system.space(&m).is_err());
    update(|s| s.statistics.total = 0);
    assert!(f.system.space(&m).is_err());
    update(|s| {
        s.statistics.total = MIN_BYTES - 40960;
        s.keep_attached = true;
    });
    assert!(f.system.unmount(&m).is_err());
    update(|s| s.keep_attached = false);
    assert!(f.system.detach_exact(&p, "/dev/disk95").is_err());
    f.system.unmount(&m).unwrap();
    f.system.detach_exact(&p, &m.device).unwrap();
}
#[test]
fn partial_attach_errors_are_reaped_without_hiding_the_original_failure() {
    let mut f = Fixture::new();
    update(|s| {
        s.failure = Some(" attach ".into());
        s.partial_failure = true;
    });
    let error = f.system.mount(&f.home, MIN_BYTES, true, false).unwrap_err();
    assert!(error.to_string().contains("partial attachment"));
    assert!(!update(|s| s.attached));
    update(|s| {
        s.keep_attached = true;
    });
    assert!(f
        .system
        .mount(&f.home, MIN_BYTES, true, false)
        .unwrap_err()
        .to_string()
        .contains("cleanup"));
    update(|s| {
        s.failure = None;
        s.partial_failure = false;
        s.keep_attached = false;
    });
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    update(|s| s.failure = Some("/usr/bin/plutil".into()));
    assert!(f
        .system
        .json("/usr/bin/hdiutil", vec!["info".into(), "-plist".into()])
        .is_err());
}
struct ErrorReader(std::io::ErrorKind);
impl Read for ErrorReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(self.0, "fixture"))
    }
}
#[test]
fn bounded_runner_preserves_exit_codes_stdin_and_reaps_timed_out_descendants() {
    let request = |script: &str, input| Request {
        program: "/bin/sh",
        args: vec!["-c".into(), script.into()],
        input,
        lease: None,
    };
    let result = execute(request(
        "cat; printf diagnostic >&2; exit 7",
        Some(b"data".to_vec()),
    ))
    .unwrap();
    assert_eq!(result.code, Some(7));
    assert_eq!(result.stdout, b"data");
    assert_eq!(result.stderr, b"diagnostic");
    assert!(execute_with(request("sleep 10", None), Duration::from_millis(30)).is_err());
    assert!(execute_with(
        request("(sleep 10)& exit 0", None),
        Duration::from_millis(30)
    )
    .is_err());
    assert!(execute(request("yes data", None)).is_err());
    assert!(execute(request("cat", Some(vec![0; MAX_OUTPUT + 1]))).is_err());
    assert!(execute(Request {
        program: "/nonexistent-rdrive-tool",
        args: vec![],
        input: None,
        lease: None
    })
    .is_err());
    assert!(nonblocking(-1).is_err());
    assert!(statistics(Path::new("/no-such-rdrive-vault")).is_err());
    assert!(statistics(&std::env::temp_dir()).is_ok());
    let mut v = vec![];
    assert!(!drain(&mut ErrorReader(std::io::ErrorKind::WouldBlock), &mut v).unwrap());
    assert!(!drain(&mut ErrorReader(std::io::ErrorKind::Interrupted), &mut v).unwrap());
    assert!(drain(
        &mut ErrorReader(std::io::ErrorKind::PermissionDenied),
        &mut v
    )
    .is_err());
    assert!(drain(&mut &b""[..], &mut v).unwrap());
}
#[test]
fn configuration_boundary_and_filesystem_failures_preserve_pending_state() {
    let mut f = Fixture::new();
    let _default = System::default();
    assert_eq!((_default.supported)(), cfg!(target_os = "macos"));
    assert!(paths(&f.home.join("../home"), false).is_err());
    fs::set_permissions(&f.home, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(paths(&f.home, false).is_err());
    fs::set_permissions(&f.home, fs::Permissions::from_mode(0o700)).unwrap();
    let p = paths(&f.home, true).unwrap().unwrap();
    assert!(f
        .system
        .mount(&f.home, MIN_BYTES, false, true)
        .unwrap()
        .is_none());
    fs::set_permissions(&p.vault, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(descriptor(&p).is_err());
    fs::set_permissions(&p.vault, fs::Permissions::from_mode(0o700)).unwrap();
    update(|s| s.failure = Some("--help".into()));
    assert!(f.system.family().is_err());
    update(|s| s.failure = None);
    let m = f.mount();
    let value = f.value();
    update(|s| s.failure = Some("/usr/sbin/diskutil info".into()));
    assert!(f
        .system
        .verify(&p, &value, &m.device, "/dev/disk91s1", false)
        .is_err());
    update(|s| s.failure = None);
    let mut wrong = m.clone();
    wrong.root = p.vault.join("alternative");
    assert!(f.system.space(&wrong).is_err());
    let moved = p.vault.join("saved-mount");
    fs::rename(&p.mount, &moved).unwrap();
    symlink(&moved, &p.mount).unwrap();
    assert!(f
        .system
        .verify(&p, &value, &m.device, "/dev/disk91s1", false)
        .is_err());
    fs::remove_file(&p.mount).unwrap();
    fs::rename(&moved, &p.mount).unwrap();
    let marker = p.mount.join(".rdrive-vault.json");
    let bytes = fs::read(&marker).unwrap();
    let mut bad: Descriptor = serde_json::from_slice(&bytes).unwrap();
    bad.home_binding = "foreign".into();
    crate::state::atomic_json(&marker, &bad).unwrap();
    assert!(f
        .system
        .verify(&p, &value, &m.device, "/dev/disk91s1", false)
        .is_err());
    fs::write(&marker, vec![b' '; 16385]).unwrap();
    assert!(f
        .system
        .verify(&p, &value, &m.device, "/dev/disk91s1", false)
        .is_err());
    fs::write(&marker, &bytes).unwrap();
    update(|s| s.honor_mode = false);
    assert!(f.system.mount(&f.home, MIN_BYTES, false, true).is_err());
    assert!(!update(|s| s.attached));
}
struct ZeroWriter;
impl Write for ZeroWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Ok(0)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct ErrorWriter(std::io::ErrorKind);
impl Write for ErrorWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(self.0, "fixture"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[test]
fn conversion_io_and_untrusted_space_arithmetic_are_bounded() {
    assert!(block_statistics(0, 2, 1, 1, false).is_err());
    assert!(block_statistics(1, 1, 2, 1, false).is_err());
    assert!(block_statistics(1, 2, 1, 2, false).is_err());
    assert!(block_statistics(u64::MAX, 2, 1, 1, false).is_err());
    assert!(feed(&mut ZeroWriter, b"input").is_err());
    assert_eq!(
        feed(&mut ErrorWriter(std::io::ErrorKind::WouldBlock), b"input").unwrap(),
        0
    );
    assert_eq!(
        feed(&mut ErrorWriter(std::io::ErrorKind::Interrupted), b"input").unwrap(),
        0
    );
    assert!(feed(&mut ErrorWriter(std::io::ErrorKind::BrokenPipe), b"input").is_err());
    let mut bytes = vec![];
    assert_eq!(feed(&mut bytes, b"input").unwrap(), 5);
    let r = Request {
        program: "/bin/sh",
        args: vec![
            "-c".into(),
            "exec 1>&-; sleep 0.05; exec 2>&-; sleep 0.05".into(),
        ],
        input: Some(vec![]),
        lease: None,
    };
    assert_eq!(execute(r).unwrap().code, Some(0));
}
#[test]
fn initial_attachment_failure_and_foreign_existing_marker_do_not_detach_unrelated_state() {
    let mut f = Fixture::new();
    update(|s| s.failure = Some(" attach ".into()));
    assert!(f.system.mount(&f.home, MIN_BYTES, true, false).is_err());
    assert!(!update(|s| s.attached));
    update(|s| s.failure = None);
    let m = f.mount();
    let p = f.paths();
    let marker = p.mount.join(".rdrive-vault.json");
    let bytes = fs::read(&marker).unwrap();
    let mut bad: Descriptor = serde_json::from_slice(&bytes).unwrap();
    bad.home_binding = "foreign".into();
    crate::state::atomic_json(&marker, &bad).unwrap();
    assert!(f.system.mount(&f.home, MIN_BYTES, false, false).is_err());
    assert!(update(|s| s.attached));
    fs::write(&marker, bytes).unwrap();
    f.system.unmount(&m).unwrap();
}
#[test]
fn mount_placeholder_refuses_foreign_filesystems() {
    let f = Fixture::new();
    unmounted_directory(&f.home, f.home.parent().unwrap()).unwrap();
    assert!(unmounted_directory(Path::new("/dev"), &f.home).is_err());
    assert!(unmounted_directory(Path::new("/missing-rdrive-mount"), &f.home).is_err());
}
#[test]
fn external_shadow_storage_cannot_bypass_fixed_image_capacity() {
    let mut f = Fixture::new();
    let m = f.mount();
    let p = f.paths();
    for value in [
        json!("/private/tmp/foreign.shadow"),
        json!(["/private/tmp/foreign.shadow"]),
        json!(true),
        json!({}),
        json!(1),
    ] {
        update(|s| {
            s.overrides.insert("images",json!({"images":[{"image-path":s.image,"shadow-path":value,"system-entities":[{"dev-entry":"/dev/disk90"},{"dev-entry":"/dev/disk91s1","mount-point":s.mount}]}]}));
        });
        assert!(f.system.attached(&p).is_err());
        assert!(f.system.space(&m).is_err());
        assert!(f.system.unmount(&m).is_err());
        assert!(update(|s| s.attached));
    }
    for value in [Value::Null, json!(false), json!(""), json!([])] {
        assert!(!active_shadow(&value));
    }
    update(|s| {
        s.overrides.remove("images");
    });
    f.system.unmount(&m).unwrap();
}
#[test]
fn host_operation_lease_survives_only_its_recorded_inode_and_rejects_busy_operations() {
    let mut f = Fixture::new();
    let m = f.mount();
    f.system.unmount(&m).unwrap();
    let p = f.paths();
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&p.host_lock)
        .unwrap();
    flock(&held, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    let count = update(|s| s.requests.len());
    let error = f
        .system
        .mount(&f.home, MIN_BYTES, false, false)
        .unwrap_err();
    assert!(format!("{error:#}").contains("still active"));
    assert_eq!(update(|s| s.requests.len()), count);
    f.system.operation_lock = Some(Arc::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p.host_lock)
            .unwrap(),
    ));
    assert!(f.system.run("/usr/bin/true", vec![], None).is_err());
    flock(&held, libc::LOCK_UN).unwrap();
    let stored = f.value();
    assert!(operation_lock(
        &p,
        Some(&Identity {
            device: 0,
            inode: 0
        })
    )
    .is_err());
    let saved = p.vault.join("saved-host.lock");
    fs::rename(&p.host_lock, &saved).unwrap();
    assert!(System::capacity(&f.home).is_err());
    assert!(operation_lock(&p, Some(&stored.host_lock)).is_err());
    assert!(
        !p.host_lock.exists(),
        "A missing recorded lease is never recreated"
    );
    File::create(&p.host_lock)
        .unwrap()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    assert!(System::capacity(&f.home).is_err());
    fs::remove_file(&p.host_lock).unwrap();
    symlink(&saved, &p.host_lock).unwrap();
    assert!(f.system.mount(&f.home, MIN_BYTES, false, false).is_err());
    fs::remove_file(&p.host_lock).unwrap();
    fs::rename(saved, &p.host_lock).unwrap();
    fs::write(&p.host_lock, b"unexpected contents").unwrap();
    assert!(System::capacity(&f.home).is_err());
    fs::write(&p.host_lock, b"").unwrap();
    let recovered = f
        .system
        .mount(&f.home, MIN_BYTES, false, false)
        .unwrap()
        .unwrap();
    f.system.unmount(&recovered).unwrap();
}
#[test]
fn unbound_images_are_never_adopted_but_an_empty_prepublication_lease_can_resume() {
    let mut f = Fixture::new();
    let p = paths(&f.home, true).unwrap().unwrap();
    let file = File::create(&p.image).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o640))
        .unwrap();
    file.set_len(MIN_BYTES).unwrap();
    assert!(f.system.mount(&f.home, MIN_BYTES, true, false).is_err());
    assert!(!p.descriptor.exists() && !p.host_lock.exists());
    assert_eq!(fs::metadata(&p.image).unwrap().mode() & 0o777, 0o640);
    assert_eq!(fs::metadata(&p.image).unwrap().len(), MIN_BYTES);
    fs::remove_file(&p.image).unwrap();
    let initial = operation_lock(&p, None).unwrap();
    let identity = lock_identity(&p.host_lock).unwrap();
    drop(initial);
    assert!(descriptor(&p).unwrap().is_none());
    let m = f.mount();
    assert_eq!(f.value().host_lock, identity);
    f.system.unmount(&m).unwrap();
    let moved = p.vault.join("saved-mount");
    fs::rename(&p.mount, &moved).unwrap();
    File::create(&p.mount).unwrap();
    let error = f
        .system
        .verify(&p, &f.value(), &m.device, "/dev/disk91s1", false)
        .unwrap_err();
    assert!(error.to_string().contains("mount root is not owned"));
    fs::remove_file(&p.mount).unwrap();
    fs::rename(moved, &p.mount).unwrap();
    assert!(inheritable(-1).is_err());
    let inherit_file = File::open(&p.host_lock).unwrap();
    let fd = unsafe { libc::fcntl(inherit_file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(fd >= 0);
    inheritable(fd).unwrap();
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(unsafe { libc::close(fd) }, 0);
    let bad = paths(&f.home, false).unwrap().unwrap();
    fs::remove_file(&bad.host_lock).unwrap();
    fs::create_dir(&bad.host_lock).unwrap();
    assert!(operation_lock(&bad, None).is_err());
    fs::remove_dir(&bad.host_lock).unwrap();
    fs::set_permissions(&bad.vault, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(operation_lock(&bad, None).is_err());
    fs::set_permissions(&bad.vault, fs::Permissions::from_mode(0o700)).unwrap();
    let unsupported = System::default();
    assert!(unsupported.run("/usr/bin/true", vec![], None).is_err());
}
#[test]
#[cfg(target_os = "macos")]
fn inherited_lockf_runner_holds_a_separate_lease_and_releases_it_after_completion_or_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let lock = temp.path().join("lease");
    let lease = Arc::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lock)
            .unwrap(),
    );
    let other = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    let fd_flags = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) };
    flock(&lease, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    let request = |script: &str| Request {
        program: "/bin/sh",
        args: vec!["-c".into(), script.into()],
        input: None,
        lease: Some(lease.clone()),
    };
    let output = execute(request("printf held")).unwrap();
    assert_eq!(output.code, Some(0));
    assert_eq!(output.stdout, b"held");
    assert_eq!(
        unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) },
        fd_flags
    );
    flock(&other, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    flock(&other, libc::LOCK_UN).unwrap();
    flock(&lease, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    assert!(execute_with(request("sleep 10"), Duration::from_millis(40)).is_err());
    // SIGKILL skips lockf's cleanup. The live caller still owns its original
    // description and explicitly unlocks it only after the group is reaped.
    assert!(flock(&other, libc::LOCK_EX | libc::LOCK_NB).is_err());
    flock(&lease, libc::LOCK_UN).unwrap();
    flock(&other, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    flock(&other, libc::LOCK_UN).unwrap();
}
