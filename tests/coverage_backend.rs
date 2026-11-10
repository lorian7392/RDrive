//! Public backend contracts exercised against an isolated, observable test peer.
//! The peer verifies caller behavior; it does not claim to implement SMB or an OS.
use anyhow::{bail, Result};
use async_trait::async_trait;
use rdrive::{
    backend::{self, Backend, Capabilities, Entry, WriteCondition},
    config::{self, Config, IndexPolicy, Profile, Protocol},
};
use std::{collections::BTreeMap, path::Path, sync::Mutex};

#[derive(Default)]
struct Peer {
    rows: BTreeMap<String, Vec<Entry>>,
    entries: BTreeMap<String, Entry>,
    calls: Mutex<Vec<String>>,
    list_failure: Option<String>,
    stat_failure: Option<String>,
    mkdir_failure: Option<String>,
}

fn entry(path: &str, is_dir: bool) -> Entry {
    Entry {
        path: path.into(),
        is_dir,
        size: if is_dir { 0 } else { 4 },
        modified: Some(17),
        etag: Some("revision-one".into()),
    }
}

#[async_trait]
impl Backend for Peer {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: "test-peer",
            conditional_write: false,
            server_copy: false,
            locking: false,
            notes: vec![],
        }
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        self.calls.lock().unwrap().push(format!("stat:{path}"));
        if self.stat_failure.as_deref() == Some(path) {
            bail!("injected stat failure");
        }
        Ok(self.entries.get(path).cloned())
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        self.calls.lock().unwrap().push(format!("list:{path}"));
        if self.list_failure.as_deref() == Some(path) {
            bail!("injected list failure");
        }
        Ok(self.rows.get(path).cloned().unwrap_or_default())
    }
    async fn download(&self, path: &str, local: &Path) -> Result<()> {
        self.calls.lock().unwrap().push(format!("download:{path}"));
        std::fs::write(local, b"body")?;
        Ok(())
    }
    async fn upload(&self, _: &Path, _: &str, _: WriteCondition) -> Result<Entry> {
        bail!("unexpected upload");
    }
    async fn mkdir(&self, path: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("mkdir:{path}"));
        if self.mkdir_failure.as_deref() == Some(path) {
            bail!("injected mkdir failure");
        }
        Ok(())
    }
    async fn remove(&self, _: &str, _: bool) -> Result<()> {
        bail!("unexpected remove")
    }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("unexpected rename")
    }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
        bail!("unexpected copy")
    }
}

#[test]
fn paths_join_empty_and_normalized_components_without_traversal() {
    assert_eq!(backend::join("", "a//./b/").unwrap(), "a/b");
    assert_eq!(backend::join("a", "").unwrap(), "a");
    assert_eq!(backend::join("a/", "./b").unwrap(), "a/b");
    assert_eq!(backend::normalize("./").unwrap(), "");
    for path in ["../x", "a/../x", "/x", "a\\b", "a\0b"] {
        assert!(
            backend::join(path, "child").is_err(),
            "accepted parent {path:?}"
        );
        assert!(
            backend::join("parent", path).is_err(),
            "accepted child {path:?}"
        );
    }
}

#[test]
fn revision_comparison_requires_matching_type_size_and_available_revision() {
    let original = entry("a", false);
    let mut other = original.clone();
    other.path = "different-path".into();
    assert!(original.same_revision(&other));
    other.is_dir = true;
    assert!(!original.same_revision(&other));
    other = original.clone();
    other.size += 1;
    assert!(!original.same_revision(&other));
    other = original.clone();
    other.etag = Some("changed".into());
    assert!(!original.same_revision(&other));
    other.etag = None;
    assert!(!original.same_revision(&other));
    let mut untagged = original.clone();
    untagged.etag = None;
    assert!(!untagged.same_revision(&original));
    assert!(untagged.same_revision(&other));
    other.modified = Some(18);
    assert!(!untagged.same_revision(&other));
    untagged.modified = None;
    other.modified = None;
    assert!(!untagged.same_revision(&other));
}

#[tokio::test]
async fn backend_defaults_delegate_only_supported_reads_and_reject_unsafe_writes() {
    let mut peer = Peer::default();
    let revision = entry("a", false);
    peer.entries.insert("a".into(), revision.clone());
    peer.rows.insert("".into(), vec![revision.clone()]);
    assert_eq!(peer.diagnostics(), serde_json::json!({}));
    assert_eq!(peer.content_sha256(&revision), None);
    peer.check_sync_recovery("receipt").await.unwrap();
    assert_eq!(
        peer.stat_metadata("a").await.unwrap(),
        Some(revision.clone())
    );
    assert_eq!(
        peer.list_metadata("").await.unwrap(),
        vec![revision.clone()]
    );
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("download");
    peer.download_checked("a", &output, &revision)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"body");
    assert!(peer
        .change_feed("", true, tokio_util::sync::CancellationToken::new())
        .await
        .is_err());
    assert!(peer
        .download_resume("a", &output, &revision, 4)
        .await
        .is_err());
    assert!(peer
        .upload_resume(
            &output,
            "a",
            WriteCondition::CreateNew,
            &temp.path().join("cp")
        )
        .await
        .is_err());
    assert!(peer.remove_checked("a", &revision).await.is_err());
    assert!(peer.rename_checked("a", "b", &revision).await.is_err());
    assert!(peer.lock("a", 60).await.is_err());
    assert!(peer.unlock("a", "token").await.is_err());
    assert!(peer.namespace_matches(&revision, &revision));
    let mut changed = revision.clone();
    changed.size += 1;
    assert!(!peer.namespace_matches(&revision, &changed));
    assert_eq!(
        *peer.calls.lock().unwrap(),
        ["stat:a", "list:", "download:a"]
    );
}

#[tokio::test]
async fn walking_visits_nested_directories_and_sorts_full_paths() {
    let mut peer = Peer::default();
    peer.rows.insert(
        "root".into(),
        vec![entry("root/z", false), entry("root/a", true)],
    );
    peer.rows
        .insert("root/a".into(), vec![entry("root/a/b", false)]);
    let result = backend::walk(&peer, "root/./").await.unwrap();
    assert_eq!(
        result.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["root/a", "root/a/b", "root/z"]
    );
    assert_eq!(*peer.calls.lock().unwrap(), ["list:root", "list:root/a"]);
    assert!(backend::walk(&Peer::default(), "")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn walking_rejects_malformed_children_duplicates_and_transport_failure() {
    for child in ["outside", "root", "root/a/deeper", "../escape", "/absolute"] {
        let mut peer = Peer::default();
        peer.rows.insert("root".into(), vec![entry(child, false)]);
        assert!(
            backend::walk(&peer, "root").await.is_err(),
            "accepted child {child}"
        );
    }
    let mut peer = Peer::default();
    peer.rows.insert(
        "root".into(),
        vec![entry("root/a", true), entry("root/a", true)],
    );
    assert!(backend::walk(&peer, "root")
        .await
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    let peer = Peer {
        list_failure: Some("root".into()),
        ..Peer::default()
    };
    assert!(backend::walk(&peer, "root")
        .await
        .unwrap_err()
        .to_string()
        .contains("injected list"));
    assert!(backend::walk(&peer, "../root").await.is_err());
}

#[tokio::test]
async fn recursive_directory_creation_preserves_existing_parents_and_failure_boundaries() {
    let mut peer = Peer::default();
    peer.entries.insert("a".into(), entry("a", true));
    backend::mkdir_all(&peer, "a/./b/c/").await.unwrap();
    assert_eq!(
        *peer.calls.lock().unwrap(),
        [
            "stat:a",
            "stat:a/b",
            "mkdir:a/b",
            "stat:a/b/c",
            "mkdir:a/b/c"
        ]
    );
    let peer = Peer::default();
    backend::mkdir_all(&peer, "").await.unwrap();
    assert!(peer.calls.lock().unwrap().is_empty());
    assert!(backend::mkdir_all(&peer, "../a").await.is_err());
    let mut peer = Peer::default();
    peer.entries.insert("a".into(), entry("a", false));
    assert!(backend::mkdir_all(&peer, "a/b")
        .await
        .unwrap_err()
        .to_string()
        .contains("is a file"));
    assert_eq!(*peer.calls.lock().unwrap(), ["stat:a"]);
    let peer = Peer {
        stat_failure: Some("a".into()),
        ..Peer::default()
    };
    assert!(backend::mkdir_all(&peer, "a/b")
        .await
        .unwrap_err()
        .to_string()
        .contains("injected stat"));
    let peer = Peer {
        mkdir_failure: Some("a".into()),
        ..Peer::default()
    };
    assert!(backend::mkdir_all(&peer, "a/b")
        .await
        .unwrap_err()
        .to_string()
        .contains("injected mkdir"));
    assert_eq!(*peer.calls.lock().unwrap(), ["stat:a", "mkdir:a"]);
}

fn profile(protocol: Protocol, address: &str) -> Profile {
    Profile {
        protocol,
        url: address.into(),
        username: None,
        password_env: None,
        allow_http: false,
        timeout_secs: 120,
        smb_encryption: true,
        nextcloud_upload_url: None,
    }
}

#[test]
fn configuration_deserialization_preserves_security_and_index_defaults() {
    let value: Config = serde_json::from_value(serde_json::json!({
        "profiles": {"dav": {"protocol": "webdav", "url": "https://example.test/dav"}}
    }))
    .unwrap();
    let dav = &value.profiles["dav"];
    assert_eq!(dav.timeout_secs, 120);
    assert!(dav.smb_encryption);
    assert!(!dav.allow_http);
    assert!(value.index.enabled);
    assert_eq!(value.index.prefetch_depth, None);
    assert_eq!(value.index.refresh_secs, 300);
    assert_eq!(value.index.active_refresh_secs, 30);
    assert_eq!(value.cache.max_bytes, None);
    assert_eq!(value.cache.directory, None);
    config::validate_profile(dav).unwrap();
}

#[test]
fn profile_validation_rejects_insecure_or_ambiguous_addresses() {
    for address in [
        "not a url",
        "https://name@example.test/dav",
        "https://:secret@example.test/dav",
        "https://example.test/dav?query",
        "https://example.test/dav#fragment",
        "file:///tmp/a",
        "http://example.test/dav",
        "ftp://example.test/dav",
    ] {
        assert!(
            config::validate_profile(&profile(Protocol::Webdav, address)).is_err(),
            "accepted {address}"
        );
    }
    let mut dav = profile(Protocol::Webdav, "http://example.test/dav");
    dav.allow_http = true;
    config::validate_profile(&dav).unwrap();
    dav.timeout_secs = 0;
    assert!(config::validate_profile(&dav)
        .unwrap_err()
        .to_string()
        .contains("timeout"));
    for address in [
        "https://example.test/share",
        "smb://example.test/",
        "smb://example.test",
    ] {
        assert!(
            config::validate_profile(&profile(Protocol::Smb, address)).is_err(),
            "accepted {address}"
        );
    }
    config::validate_profile(&profile(Protocol::Smb, "smb://example.test/share/folder")).unwrap();
}

#[test]
fn nextcloud_upload_origin_and_credentials_are_validated_before_connecting() {
    let mut dav = profile(Protocol::Webdav, "https://example.test/dav");
    for endpoint in [
        "not a url",
        "https://elsewhere.test/upload",
        "http://example.test/upload",
        "https://name@example.test/upload",
        "https://:secret@example.test/upload",
        "https://example.test/upload?query",
        "https://example.test/upload#fragment",
    ] {
        dav.nextcloud_upload_url = Some(endpoint.into());
        assert!(
            config::validate_profile(&dav).is_err(),
            "accepted {endpoint}"
        );
    }
    dav.nextcloud_upload_url = Some("https://example.test/uploads/user".into());
    config::validate_profile(&dav).unwrap();
    let mut smb = profile(Protocol::Smb, "smb://example.test/share");
    smb.nextcloud_upload_url = Some("smb://example.test/upload".into());
    assert!(config::validate_profile(&smb).is_err());
}

#[test]
fn index_intervals_enforce_both_bounds_independently() {
    IndexPolicy::default().validate().unwrap();
    let mut policy = IndexPolicy::default();
    for (refresh, active, accepted) in [
        (1, 1, true),
        (31_536_000, 31_536_000, true),
        (0, 1, false),
        (1, 0, false),
        (31_536_001, 1, false),
        (1, 31_536_001, false),
    ] {
        policy.refresh_secs = refresh;
        policy.active_refresh_secs = active;
        assert_eq!(
            policy.validate().is_ok(),
            accepted,
            "intervals {refresh}/{active}"
        );
    }
}

#[test]
fn explicitly_missing_password_environment_does_not_fall_back_to_keychain() {
    let mut dav = profile(Protocol::Webdav, "https://example.test/dav");
    let missing_key = format!("RDRIVE_COVERAGE_MISSING_{}", uuid::Uuid::new_v4().simple());
    dav.password_env = Some(missing_key.clone());
    let failure = config::password(&dav, "coverage-isolated").unwrap_err();
    assert!(failure.to_string().contains(&missing_key));
    assert!(failure.to_string().contains("is missing"));
}

#[test]
fn configuration_environment_inputs_are_isolated_in_subprocess() {
    // Each environment combination runs in a child, so parallel tests and the
    // user's process environment never observe modified HOME or credentials.
    const MODE: &str = "RDRIVE_COVERAGE_ENV_PROBE";
    if let Ok(mode) = std::env::var(MODE) {
        let expected = match mode.as_str() {
            "override" => "/fixture/override",
            "home" => "/fixture/home/Library/Application Support/rdrive",
            "empty-home" => "Library/Application Support/rdrive",
            _ => panic!("unexpected environment test mode"),
        };
        assert_eq!(config::default_home(), std::path::PathBuf::from(expected));
        let mut dav = profile(Protocol::Webdav, "https://example.test/dav");
        dav.password_env = Some("RDRIVE_COVERAGE_SYNTHETIC_PASSWORD".into());
        assert_eq!(
            config::password(&dav, "synthetic-only").unwrap(),
            "synthetic-credential"
        );
        return;
    }
    for mode in ["override", "home", "empty-home"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "configuration_environment_inputs_are_isolated_in_subprocess",
            ])
            .env(MODE, mode)
            .env("RDRIVE_COVERAGE_SYNTHETIC_PASSWORD", "synthetic-credential");
        match mode {
            "override" => {
                child.env("RDRIVE_HOME", "/fixture/override");
            }
            "home" => {
                child.env_remove("RDRIVE_HOME").env("HOME", "/fixture/home");
            }
            "empty-home" => {
                child.env_remove("RDRIVE_HOME").env_remove("HOME");
            }
            _ => unreachable!(),
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "environment probe {mode} failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
