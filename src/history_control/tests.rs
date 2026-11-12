use super::*;
use crate::{
    backend::{Capabilities, WriteCondition},
    config::{Profile, Protocol},
};
use async_trait::async_trait;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Disk(PathBuf);
impl Disk {
    fn put(&self, name: &str, data: &[u8]) {
        let file = self.0.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, data).unwrap();
    }
    fn entry(&self, name: &str) -> Result<Option<Entry>> {
        let path = self.0.join(name);
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Entry {
            path: name.into(),
            is_dir: metadata.is_dir(),
            size: if metadata.is_dir() { 0 } else { metadata.len() },
            modified: None,
            etag: Some(if metadata.is_dir() {
                format!("directory:{}", metadata.ino())
            } else {
                state::fingerprint(&path)?
            }),
        }))
    }
}
#[async_trait]
impl Backend for Disk {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: "test",
            conditional_write: true,
            server_copy: false,
            locking: false,
            notes: vec![],
        }
    }
    fn content_sha256(&self, e: &Entry) -> Option<String> {
        if e.is_dir {
            None
        } else {
            e.etag.clone()
        }
    }
    async fn stat(&self, p: &str) -> Result<Option<Entry>> {
        self.entry(p)
    }
    async fn list(&self, p: &str) -> Result<Vec<Entry>> {
        let mut rows = vec![];
        for e in fs::read_dir(self.0.join(p))? {
            let e = e?;
            let path = backend::join(p, &e.file_name().to_string_lossy())?;
            rows.push(self.entry(&path)?.unwrap());
        }
        Ok(rows)
    }
    async fn download(&self, p: &str, local: &Path) -> Result<()> {
        fs::copy(self.0.join(p), local)?;
        Ok(())
    }
    async fn upload(&self, local: &Path, p: &str, condition: WriteCondition) -> Result<Entry> {
        let old = self.entry(p)?;
        match condition {
            WriteCondition::Any => bail!("unconditional write forbidden"),
            WriteCondition::CreateNew if old.is_some() => bail!("occupied"),
            WriteCondition::Match(e) if !old.as_ref().is_some_and(|a| a.same_revision(&e)) => {
                bail!("stale")
            }
            _ => (),
        };
        fs::copy(local, self.0.join(p))?;
        Ok(self.entry(p)?.unwrap())
    }
    async fn mkdir(&self, p: &str) -> Result<()> {
        fs::create_dir(self.0.join(p))?;
        Ok(())
    }
    async fn remove(&self, _: &str, _: bool) -> Result<()> {
        bail!("no remove")
    }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("no rename")
    }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("no copy")
    }
}
fn fixture() -> (tempfile::TempDir, Store, Disk, String) {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state")).unwrap();
    store.config.profiles.insert(
        "test".into(),
        Profile {
            protocol: Protocol::Webdav,
            url: "https://example.invalid/".into(),
            username: Some("test".into()),
            password_env: Some("RDRIVE_HISTORY_CONTROL_TEST_PASSWORD".into()),
            keychain_internet: false,
            allow_http: false,
            timeout_secs: 1,
            smb_encryption: true,
            nextcloud_upload_url: None,
        },
    );
    store.config.history.default.enabled = true;
    let disk = Disk(temp.path().join("remote"));
    fs::create_dir(&disk.0).unwrap();
    disk.put("original", b"historical bytes");
    let entry = history::capture_import(
        &store,
        "test",
        "original",
        &disk.0.join("original"),
        "import-original",
    )
    .unwrap();
    store.save_config().unwrap();
    store.save().unwrap();
    (temp, store, disk, entry.id)
}
fn plan_id(value: &Value) -> &str {
    value["plan_id"].as_str().unwrap()
}
fn modify_plan(store: &Store, id: &str, edit: impl FnOnce(&mut Value)) {
    let path = plan_path(&store.home, id).unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut value);
    state::atomic_json(&path, &value).unwrap();
}
#[tokio::test]
async fn restore_preview_copy_and_guarded_replace_are_bound_and_idempotent() {
    let (_temp, store, disk, version) = fixture();
    let preview = prepare(&store, &disk, "test", &version, None, false, "cli")
        .await
        .unwrap();
    assert!(preview["target"].as_str().unwrap().contains("recovered-"));
    assert_eq!(preview["mode"], "copy");
    let result = apply(&store, &disk, "test", &version, plan_id(&preview), "cli")
        .await
        .unwrap();
    assert_eq!(result["restored"], true);
    assert_eq!(
        apply(&store, &disk, "test", &version, plan_id(&preview), "cli")
            .await
            .unwrap(),
        result
    );
    assert_eq!(
        fs::read(disk.0.join(preview["target"].as_str().unwrap())).unwrap(),
        b"historical bytes"
    );
    disk.put("original", b"current bytes");
    let replacement = prepare(
        &store,
        &disk,
        "test",
        &version,
        None,
        true,
        "finder:domain:item",
    )
    .await
    .unwrap();
    assert_eq!(replacement["target"], "original");
    assert_eq!(replacement["mode"], "replace");
    let result = apply(
        &store,
        &disk,
        "test",
        &version,
        plan_id(&replacement),
        "finder:domain:item",
    )
    .await
    .unwrap();
    assert_eq!(result["mode"], "replace");
    assert!(result["current_version_preserved"].is_string());
    assert_eq!(
        fs::read(disk.0.join("original")).unwrap(),
        b"historical bytes"
    );
}
#[tokio::test]
async fn preview_rejects_root_existing_copy_and_directory_replacement() {
    let (_temp, store, disk, version) = fixture();
    assert!(
        prepare(&store, &disk, "test", &version, Some(""), false, "cli")
            .await
            .unwrap_err()
            .to_string()
            .contains("remote root")
    );
    assert!(prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("../escape"),
        false,
        "cli"
    )
    .await
    .is_err());
    assert!(prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("original"),
        false,
        "cli"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("already exists"));
    assert!(
        prepare(&store, &disk, "test", &version, Some("absent"), true, "cli")
            .await
            .unwrap_err()
            .to_string()
            .contains("existing file")
    );
    fs::create_dir(disk.0.join("folder")).unwrap();
    assert!(
        prepare(&store, &disk, "test", &version, Some("folder"), true, "cli")
            .await
            .is_err()
    );
    assert!(
        prepare(&store, &disk, "missing", &version, None, false, "cli")
            .await
            .is_err()
    );
}
#[tokio::test]
async fn plan_guards_reject_scope_account_version_time_and_format_changes() {
    let (_temp, mut store, disk, version) = fixture();
    let preview = prepare(&store, &disk, "test", &version, Some("copy"), false, "cli")
        .await
        .unwrap();
    let id = plan_id(&preview);
    let path = plan_path(&store.home, id).unwrap();
    let bytes = fs::read(&path).unwrap();
    let changes: Vec<Box<dyn Fn(&mut Value)>> = vec![
        Box::new(|v| v["version"] = json!(2)),
        Box::new(|v| v["id"] = json!(uuid::Uuid::new_v4().to_string())),
        Box::new(|v| v["profile"] = json!("other")),
        Box::new(|v| v["entry_id"] = json!("other")),
        Box::new(|v| v["binding"] = json!("other")),
        Box::new(|v| v["scope"] = json!("finder:other")),
        Box::new(|v| v["created"] = json!(chrono::Utc::now().timestamp() - 86401)),
        Box::new(|v| v["created"] = json!(chrono::Utc::now().timestamp() + 60)),
    ];
    for change in changes {
        state::atomic_write(&path, &bytes).unwrap();
        modify_plan(&store, id, change);
        assert!(apply(&store, &disk, "test", &version, id, "cli")
            .await
            .unwrap_err()
            .to_string()
            .contains("expired or does not match"));
        assert!(!disk.0.join("copy").exists());
    }
    state::atomic_write(&path, &bytes).unwrap();
    store.config.profiles.get_mut("test").unwrap().username = Some("new-account".into());
    assert!(apply(&store, &disk, "test", &version, id, "cli")
        .await
        .is_err());
    assert!(apply(&store, &disk, "missing", &version, id, "cli")
        .await
        .is_err());
    assert!(plan_path(&store.home, "NOT-A-UUID").is_err());
    assert!(plan_path(&store.home, "154108C8-1CBB-4FAE-81F4-C6C53161F2AE").is_err());
}
#[tokio::test]
async fn stale_destination_and_corrupt_payload_stop_before_upload() {
    let (_temp, store, disk, version) = fixture();
    let p = prepare(&store, &disk, "test", &version, Some("copy"), false, "cli")
        .await
        .unwrap();
    disk.put("copy", b"concurrent");
    assert!(apply(&store, &disk, "test", &version, plan_id(&p), "cli")
        .await
        .unwrap_err()
        .to_string()
        .contains("version_conflict"));
    assert_eq!(fs::read(disk.0.join("copy")).unwrap(), b"concurrent");
    let p = prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("another"),
        false,
        "cli",
    )
    .await
    .unwrap();
    let blob = store
        .home
        .join("history/blobs")
        .join(state::key("historical bytes"));
    fs::write(blob, b"corrupt").unwrap();
    assert!(apply(&store, &disk, "test", &version, plan_id(&p), "cli")
        .await
        .is_err());
    assert!(!disk.0.join("another").exists());
}
#[tokio::test]
async fn plan_storage_requires_private_regular_owned_files_and_directories() {
    let (temp, store, disk, version) = fixture();
    let p = prepare(&store, &disk, "test", &version, Some("copy"), false, "cli")
        .await
        .unwrap();
    let path = plan_path(&store.home, plan_id(&p)).unwrap();
    let data = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(apply(&store, &disk, "test", &version, plan_id(&p), "cli")
        .await
        .is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let hard = temp.path().join("hard");
    fs::hard_link(&path, &hard).unwrap();
    assert!(plan_file(&path).is_err());
    fs::remove_file(hard).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    assert!(plan_file(&path).is_err());
    state::atomic_write(&path, &data).unwrap();
    let link = temp.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(plan_file(&link).is_err());
    assert!(plan_file(&store.home).is_err());
    let directory = store.home.join("history/plans");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        prepare(&store, &disk, "test", &version, Some("next"), false, "cli")
            .await
            .unwrap_err()
            .to_string()
            .contains("directory must be private")
    );
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
}
#[tokio::test]
async fn readonly_history_remains_available_under_store_writer_and_paginates() {
    let (_temp, store, _disk, version) = fixture();
    assert_eq!(
        files(&store.home, &store.config, "test", None, None, 0, 1).unwrap()["total"],
        1
    );
    assert!(files(&store.home, &store.config, "test", None, None, 0, 0).is_err());
    assert!(files(&store.home, &store.config, "test", None, None, 0, 1001).is_err());
    let last = files(
        &store.home,
        &store.config,
        "test",
        Some("original"),
        None,
        usize::MAX,
        1,
    )
    .unwrap();
    assert_eq!(last["has_more"], false);
    assert!(last["entries"].as_array().unwrap().is_empty());
    for command in [
        Command::Files {
            profile: "test".into(),
            remote: None,
            item_id: None,
            offset: 0,
            page_size: 1,
        },
        Command::Status {
            profile: "test".into(),
        },
    ] {
        assert!(read(&store.home, &command).unwrap().is_some());
        assert!(execute(&store.home, &command).await.is_ok());
    }
    assert!(read(
        &store.home,
        &Command::Export {
            profile: "test".into(),
            version,
            local: PathBuf::from("unused")
        }
    )
    .unwrap()
    .is_none());
    assert!(read(&_temp.path().join("missing"), &Command::CheckpointStatus).is_err());
    assert!(read(&_temp.path().join("missing"), &Command::CheckpointList).is_err());
    assert!(read(
        &store.home,
        &Command::CheckpointInspect {
            id: "invalid".into()
        }
    )
    .is_err());
}
#[tokio::test]
async fn execute_dispatches_local_content_and_checkpoint_guards() {
    let (temp, store, _disk, version) = fixture();
    let home = store.home.clone();
    drop(store);
    let export = temp.path().join("export");
    assert!(execute(
        &home,
        &Command::Export {
            profile: "test".into(),
            version: version.clone(),
            local: export.clone()
        }
    )
    .await
    .is_ok());
    assert_eq!(fs::read(&export).unwrap(), b"historical bytes");
    assert!(execute(
        &home,
        &Command::Prune {
            profile: "test".into(),
            apply: false
        }
    )
    .await
    .is_ok());
    assert!(execute(
        &home,
        &Command::Clear {
            profile: "test".into(),
            apply: false
        }
    )
    .await
    .is_ok());
    let source = temp.path().join("import");
    fs::write(&source, b"new checkpoint").unwrap();
    assert!(execute(
        &home,
        &Command::Import {
            profile: "test".into(),
            remote: "new".into(),
            local: source,
            request_id: "second".into()
        }
    )
    .await
    .is_ok());
    let cases = [
        Command::Restore {
            profile: "test".into(),
            version: version.clone(),
            target: None,
            replace: false,
            plan: None,
            apply: true,
        },
        Command::Restore {
            profile: "test".into(),
            version: version.clone(),
            target: None,
            replace: false,
            plan: Some(uuid::Uuid::new_v4().to_string()),
            apply: false,
        },
        Command::Restore {
            profile: "test".into(),
            version: version.clone(),
            target: Some("wrong".into()),
            replace: false,
            plan: Some(uuid::Uuid::new_v4().to_string()),
            apply: true,
        },
        Command::Restore {
            profile: "missing".into(),
            version,
            target: None,
            replace: false,
            plan: None,
            apply: false,
        },
        Command::CheckpointCreate { authorize: false },
        Command::CheckpointExport {
            id: "invalid".into(),
            payload: "spool/file".into(),
            local: temp.path().join("out"),
            authorize: false,
        },
        Command::CheckpointReconcile {
            id: "invalid".into(),
            authorize: false,
        },
        Command::CheckpointForget {
            id: "invalid".into(),
            apply: false,
        },
    ];
    for command in cases {
        assert!(execute(&home, &command).await.is_err());
    }
    assert_eq!(
        execute(
            &home,
            &Command::CheckpointForget {
                id: uuid::Uuid::new_v4().to_string(),
                apply: true
            }
        )
        .await
        .unwrap()["forgotten"],
        true
    );
}

#[tokio::test]
async fn plan_parent_symlinks_and_changed_metadata_are_rejected_or_bound() {
    let (_temp, store, disk, version) = fixture();
    let p = prepare(&store, &disk, "test", &version, Some("copy"), false, "cli")
        .await
        .unwrap();
    let id = plan_id(&p);
    let plans = store.home.join("history/plans");
    let saved = store.home.join("history/plans-original");
    fs::rename(&plans, &saved).unwrap();
    symlink(&saved, &plans).unwrap();
    assert!(apply(&store, &disk, "test", &version, id, "cli")
        .await
        .unwrap_err()
        .to_string()
        .contains("directory must be private"));
    fs::remove_file(&plans).unwrap();
    fs::rename(saved, &plans).unwrap();
    let history = store.home.join("history");
    let saved = store.home.join("history-original");
    fs::rename(&history, &saved).unwrap();
    symlink(&saved, &history).unwrap();
    assert!(apply(&store, &disk, "test", &version, id, "cli")
        .await
        .unwrap_err()
        .to_string()
        .contains("directory must be private"));
    fs::remove_file(&history).unwrap();
    fs::rename(saved, &history).unwrap();
    let p = prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("original"),
        true,
        "cli",
    )
    .await
    .unwrap();
    apply(&store, &disk, "test", &version, plan_id(&p), "cli")
        .await
        .unwrap();
    let preview = prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("original"),
        true,
        "cli",
    )
    .await
    .unwrap();
    let saved: Plan = serde_json::from_slice(
        &fs::read(plan_path(&store.home, plan_id(&preview)).unwrap()).unwrap(),
    )
    .unwrap();
    assert!(saved.expected_metadata.is_some());
    let path =
        crate::metadata_remote::sidecar_path("original", &disk.entry("original").unwrap().unwrap())
            .unwrap();
    let mut record: Value = serde_json::from_slice(&fs::read(disk.0.join(&path)).unwrap()).unwrap();
    record["record"]["selection"] = json!(["user.changed"]);
    disk.put(&path, &serde_json::to_vec(&record).unwrap());
    assert!(
        apply(&store, &disk, "test", &version, plan_id(&preview), "cli")
            .await
            .is_err()
    );
}
#[tokio::test]
async fn execute_valid_restore_branches_fail_before_any_network_if_plan_or_version_is_invalid() {
    let (_temp, store, _disk, _version) = fixture();
    let home = store.home.clone();
    drop(store);
    // This named test-only environment value is never a user credential. WebDAV
    // construction performs no request; invalid local records fail first.
    std::env::set_var("RDRIVE_HISTORY_CONTROL_TEST_PASSWORD", "test-only-unused");
    let commands = [
        Command::Restore {
            profile: "test".into(),
            version: state::key("missing-version"),
            target: None,
            replace: false,
            plan: None,
            apply: false,
        },
        Command::Restore {
            profile: "test".into(),
            version: state::key("missing-version"),
            target: None,
            replace: false,
            plan: Some(uuid::Uuid::new_v4().to_string()),
            apply: true,
        },
    ];
    for command in commands {
        assert!(execute(&home, &command).await.is_err());
    }
    std::env::remove_var("RDRIVE_HISTORY_CONTROL_TEST_PASSWORD");
}

#[tokio::test]
async fn read_import_and_replacement_guard_failures_preserve_current_content() {
    let (temp, store, disk, version) = fixture();
    let p = prepare(
        &store,
        &disk,
        "test",
        &version,
        Some("original"),
        true,
        "cli",
    )
    .await
    .unwrap();
    state::atomic_write(&store.home.join("provider-domains.json"), b"not JSON").unwrap();
    assert!(apply(&store, &disk, "test", &version, plan_id(&p), "cli")
        .await
        .is_err());
    assert_eq!(
        fs::read(disk.0.join("original")).unwrap(),
        b"historical bytes"
    );
    assert!(read(
        &store.home,
        &Command::Files {
            profile: "test".into(),
            remote: None,
            item_id: None,
            offset: 0,
            page_size: 0
        }
    )
    .is_err());
    let source = temp.path().join("source");
    fs::write(&source, b"never imported").unwrap();
    let home = store.home.clone();
    drop(store);
    assert!(execute(
        &home,
        &Command::Import {
            profile: "test".into(),
            remote: "other".into(),
            local: source,
            request_id: String::new()
        }
    )
    .await
    .is_err());
}
#[tokio::test]
async fn default_recovery_names_reject_control_characters_in_imported_logical_name() {
    let (_temp, store, disk, _version) = fixture();
    let entry = history::capture_import(
        &store,
        "test",
        "unsafe\nname",
        &disk.0.join("original"),
        "control-name",
    )
    .unwrap();
    assert!(
        prepare(&store, &disk, "test", &entry.id, None, false, "cli")
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid conflict")
    );
}

#[tokio::test]
async fn nested_recovery_name_retains_parent_and_pages_have_more() {
    let (_temp, store, disk, _version) = fixture();
    let nested = history::capture_import(
        &store,
        "test",
        "folder/file.txt",
        &disk.0.join("original"),
        "nested-file",
    )
    .unwrap();
    let preview = prepare(&store, &disk, "test", &nested.id, None, false, "cli")
        .await
        .unwrap();
    assert!(preview["target"]
        .as_str()
        .unwrap()
        .starts_with("folder/file (recovered-"));
    assert!(preview["target"].as_str().unwrap().ends_with(".txt"));
    assert_eq!(
        files(&store.home, &store.config, "test", None, None, 0, 1).unwrap()["has_more"],
        true
    );
}
