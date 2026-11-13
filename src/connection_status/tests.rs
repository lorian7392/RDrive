use super::*;
use crate::config::{Config, Protocol};
use std::{
    io::Write,
    net::TcpListener,
    os::unix::fs::{symlink, PermissionsExt},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::io::AsyncWriteExt;

fn profile() -> Profile {
    Profile {
        protocol: Protocol::Webdav,
        url: "https://example.invalid/dav/".into(),
        username: Some("fixture-user".into()),
        password_env: Some("RDRIVE_CONNECTION_STATUS_TEST_PASSWORD".into()),
        keychain_internet: false,
        allow_http: false,
        timeout_secs: 2,
        smb_encryption: true,
        nextcloud_upload_url: None,
    }
}
fn home() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("state");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    (t, home)
}
fn write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn set_config(home: &Path, profiles: Vec<(&str, Profile)>) {
    let mut c = Config::default();
    c.profiles = profiles.into_iter().map(|(k, p)| (k.into(), p)).collect();
    state::atomic_json(&home.join("config.json"), &c).unwrap();
}
fn paths(home: &Path, p: &Profile, name: &str) -> (PathBuf, PathBuf) {
    let key = filename(name, &binding(p).unwrap());
    (
        home.join("connection-health").join(format!("{key}.json")),
        home.join("connection-health").join(format!("{key}.lock")),
    )
}
fn record(home: &Path, p: &Profile, name: &str) -> Record {
    read(&paths(home, p, name).0, &binding(p).unwrap(), name)
        .unwrap()
        .unwrap()
}
fn script(root: &Path, body: &str) -> PathBuf {
    let path = root.join(format!("worker-{}", uuid::Uuid::new_v4()));
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn reply(profile: &Profile, name: &str, error: Option<Code>) -> Reply {
    let now = now_ms();
    Reply {
        version: VERSION,
        binding: binding(profile).unwrap(),
        profile_hash: state::key(name),
        started_at_ms: now,
        finished_at_ms: now,
        error,
    }
}
fn output_script(root: &Path, value: &Reply) -> PathBuf {
    script(
        root,
        &format!(
            "cat <<'REPLY'\n{}\nREPLY",
            serde_json::to_string(value).unwrap()
        ),
    )
}

#[test]
fn absent_and_scope_changed_observations_never_initialize_or_recover_state() {
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("absent");
    let p = profile();
    let unknown = snapshot(&missing, "profile", &p, 10, 300).unwrap();
    assert_eq!(unknown["state"], "unknown");
    assert!(!missing.exists());
    let (_t, home) = home();
    let before = fs::read_dir(&home).unwrap().count();
    assert_eq!(
        snapshot(&home, "profile", &p, 10, 300).unwrap()["source"],
        "no_current_observation"
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), before);
    write(&home.join("state.json"), b"must not be opened or recovered");
    let bytes = fs::read(home.join("state.json")).unwrap();
    observe_at(&home, "profile", &p, "cli.ls", 100, 200, None).unwrap();
    assert_eq!(
        snapshot(&home, "profile", &p, 200, 300).unwrap()["state"],
        "online"
    );
    let mut changed = p.clone();
    changed.username = Some("other-account".into());
    assert_ne!(binding(&changed).unwrap(), binding(&p).unwrap());
    assert_eq!(
        snapshot(&home, "profile", &changed, 200, 300).unwrap()["state"],
        "unknown"
    );
    changed = p.clone();
    changed.password_env = Some("DIFFERENT_REFERENCE".into());
    assert_eq!(
        snapshot(&home, "profile", &changed, 200, 300).unwrap()["state"],
        "unknown"
    );
    observe_at(&home, "profile", &changed, "cli.ls", 300, 400, None).unwrap();
    observe_at(
        &home,
        "profile",
        &p,
        "cli.ls",
        500,
        600,
        Some(Code::Network),
    )
    .unwrap();
    assert_eq!(
        snapshot(&home, "profile", &changed, 600, 300).unwrap()["state"],
        "online"
    );
    assert_eq!(fs::read(home.join("state.json")).unwrap(), bytes);
}

#[test]
fn real_operation_order_success_history_freshness_and_clock_skew_are_distinct() {
    let (_t, home) = home();
    let p = profile();
    observe_at(&home, "profile", &p, "index.list", 1000, 1500, None).unwrap();
    let fresh = snapshot(&home, "profile", &p, 301500, 300).unwrap();
    assert_eq!(fresh["state"], "online");
    assert_eq!(fresh["age_seconds"], 300);
    assert_eq!(
        snapshot(&home, "profile", &p, 301501, 300).unwrap()["state"],
        "stale"
    );
    assert_eq!(
        snapshot(&home, "profile", &p, 1000, 300).unwrap()["state"],
        "unknown"
    );
    observe_at(
        &home,
        "profile",
        &p,
        "probe.root_metadata",
        2000,
        2500,
        Some(Code::AuthenticationFailed),
    )
    .unwrap();
    let failure = snapshot(&home, "profile", &p, 2600, 300).unwrap();
    assert_eq!(failure["state"], "offline");
    assert_eq!(failure["last_success_at_ms"], 1500);
    assert_eq!(
        failure["last_error"]["detail"]["code"],
        "authentication_failed"
    );
    observe_at(&home, "profile", &p, "index.list", 3000, 3500, None).unwrap();
    let before = fs::read(paths(&home, &p, "profile").0).unwrap();
    observe_at(
        &home,
        "profile",
        &p,
        "probe.root_metadata",
        2000,
        5000,
        Some(Code::Timeout),
    )
    .unwrap();
    assert_eq!(fs::read(paths(&home, &p, "profile").0).unwrap(), before);
    observe_at(
        &home,
        "profile",
        &p,
        "cli.rm",
        4000,
        4500,
        Some(Code::Operation),
    )
    .unwrap();
    let business = snapshot(&home, "profile", &p, 4600, 300).unwrap();
    assert_eq!(business["state"], "online");
    assert_eq!(business["last_success_at_ms"], 3500);
    assert_eq!(business["checked_at_ms"], 3500);
    assert_eq!(
        business["last_error"]["detail"]["connection_effect"],
        "none"
    );
    observe_at(
        &home,
        "profile",
        &p,
        "probe.root_metadata",
        5000,
        5500,
        Some(Code::CredentialsUnavailable),
    )
    .unwrap();
    assert_eq!(
        snapshot(&home, "profile", &p, 5600, 300).unwrap()["state"],
        "unknown"
    );
    observe_at(&home, "profile", &p, "index.list", 6000, 6500, None).unwrap();
    let restored = snapshot(&home, "profile", &p, 6600, 300).unwrap();
    assert!(restored["last_error"].is_null());
    assert_eq!(
        restored["last_failure"]["error"]["code"],
        "credentials_unavailable"
    );
    assert_eq!(restored["last_success_at_ms"], 6500);
    assert!(stamp(i64::MAX).is_none());
    assert!(valid_time(-1).is_err());
    assert!(valid_time(i64::MAX).is_err());
    assert!(snapshot(&home, "profile", &p, -1, 300).is_err());
    for seconds in [0, 31_536_001] {
        assert!(snapshot(&home, "profile", &p, 0, seconds).is_err());
    }
    for op in ["", "space not label", "remote/path", &"a".repeat(65)] {
        assert!(observe_at(&home, "profile", &p, op, 0, 1, None).is_err());
    }
    assert!(observe_at(&home, "profile", &p, "cli.ls", 10, 1, None).is_err());
    let error = anyhow::anyhow!("HTTP 503: password=secret");
    observe(&home, "profile", &p, "cli.ls", now_ms(), Err(&error)).unwrap();
    observe(&home, "profile", &p, "cli.ls", now_ms(), Ok(())).unwrap();
    assert_eq!(record(&home, &p, "profile").last_attempt.error, None);
}

#[test]
fn errors_explain_categories_without_echoing_server_or_credential_material() {
    for (text, code) in [
        (
            "credential environment variable TOP_SECRET is missing",
            Code::CredentialsUnavailable,
        ),
        (
            "macOS Keychain request denied TOP_SECRET",
            Code::CredentialsUnavailable,
        ),
        ("network timed out password TOP_SECRET", Code::Timeout),
        ("invalid peer certificate TOP_SECRET", Code::Tls),
        ("DNS name lookup TOP_SECRET", Code::Dns),
        (
            "HTTP 401 Unauthorized TOP_SECRET",
            Code::AuthenticationFailed,
        ),
        (
            "SMB connection or authentication failed: STATUS_LOGON_FAILURE TOP_SECRET",
            Code::AuthenticationFailed,
        ),
        (
            "authentication failed TOP_SECRET",
            Code::AuthenticationFailed,
        ),
        ("HTTP 403 TOP_SECRET", Code::AccessDenied),
        ("HTTP 503 TOP_SECRET", Code::Server),
        (
            "SMB connection or authentication failed: Transport error Not connected TOP_SECRET",
            Code::Network,
        ),
        ("version_conflict TOP_SECRET", Code::Conflict),
        ("unclassified problem TOP_SECRET", Code::Operation),
        ("path must be private owned TOP_SECRET", Code::UnsafeState),
    ] {
        let result = safe_error(&anyhow::anyhow!(text.to_owned()));
        assert_eq!(result["code"], json!(code), "{text}");
        assert!(!result.to_string().contains("TOP_SECRET"));
    }
    for (kind, code) in [
        (std::io::ErrorKind::TimedOut, Code::Timeout),
        (std::io::ErrorKind::WouldBlock, Code::Busy),
        (std::io::ErrorKind::PermissionDenied, Code::PermissionDenied),
        (std::io::ErrorKind::NotFound, Code::NotFound),
        (std::io::ErrorKind::Other, Code::Operation),
    ] {
        assert_eq!(
            classify(&std::io::Error::new(kind, "TOP_SECRET").into()),
            code
        );
    }
    assert_eq!(
        classify(&serde_json::from_str::<Value>("{").unwrap_err().into()),
        Code::InvalidData
    );
    for code in [
        Code::RootUnavailable,
        Code::ConfigurationChanged,
        Code::ProbeProcess,
        Code::InvalidResponse,
        Code::Busy,
        Code::PermissionDenied,
        Code::NotFound,
        Code::InvalidData,
        Code::UnsafeState,
        Code::Operation,
    ] {
        assert!(!code.value()["message"].as_str().unwrap().is_empty());
    }
}

#[test]
fn unsafe_status_paths_links_invalid_records_and_busy_writer_fail_closed() {
    let (t, home) = home();
    let p = profile();
    let alias = t.path().join("alias");
    symlink(&home, &alias).unwrap();
    assert!(snapshot(&alias, "p", &p, 0, 300).is_err());
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(snapshot(&home, "p", &p, 0, 300).is_err());
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = home.join("connection-health");
    write(&dir, b"not directory");
    assert!(snapshot(&home, "p", &p, 0, 300).is_err());
    fs::remove_file(&dir).unwrap();
    symlink(t.path(), &dir).unwrap();
    assert!(snapshot(&home, "p", &p, 0, 300).is_err());
    fs::remove_file(&dir).unwrap();
    observe_at(&home, "p", &p, "cli.ls", 100, 200, None).unwrap();
    let (path, lock) = paths(&home, &p, "p");
    let valid = fs::read(&path).unwrap();
    for body in [b"{".to_vec(), vec![b' '; MAX_RECORD as usize + 1]] {
        write(&path, &body);
        assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    }
    write(&path, &valid);
    for mutation in ["version", "scope", "name_hash", "clock"] {
        let mut value: Value = serde_json::from_slice(&valid).unwrap();
        match mutation {
            "version" => value["version"] = json!(2),
            "scope" => value["scope"] = json!("foreign"),
            "name_hash" => value["name_hash"] = json!("other"),
            _ => value["last_attempt"]["started_at_ms"] = json!(999),
        };
        write(&path, &serde_json::to_vec(&value).unwrap());
        assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    }
    write(&path, &valid);
    let hard = t.path().join("hard");
    fs::hard_link(&path, &hard).unwrap();
    assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    fs::remove_file(hard).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let saved = t.path().join("saved");
    fs::rename(&path, &saved).unwrap();
    symlink(&saved, &path).unwrap();
    assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    fs::remove_dir(&path).unwrap();
    fs::rename(saved, &path).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(observe_at(&home, "p", &p, "cli.ls", 300, 400, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), valid);
    drop(held);
    let lock_hard = t.path().join("lock-hard");
    fs::hard_link(&lock, &lock_hard).unwrap();
    assert!(observe_at(&home, "p", &p, "cli.ls", 300, 400, None).is_err());
    fs::remove_file(lock_hard).unwrap();
    let absent = t.path().join("absent");
    assert!(observe_at(&absent, "p", &p, "cli.ls", 0, 1, None).is_err());
    fs::set_permissions(&home, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(snapshot(&home, "p", &p, 300, 300).is_err());
    assert!(observe_at(&home, "p", &p, "cli.ls", 300, 400, None).is_err());
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
}

struct Server {
    url: String,
    seen: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(status: u16, directory: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut b = [0; 1];
                if stream.read(&mut b).unwrap() == 0 {
                    return;
                }
                request.push(b[0]);
            }
            let text = String::from_utf8(request).unwrap();
            let length = text
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, n)| n.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert!(text.starts_with("PROPFIND /dav/ HTTP/1.1"));
            assert!(text.to_ascii_lowercase().contains("authorization: basic"));
            count.fetch_add(1, Ordering::SeqCst);
            let body = if status == 207 {
                format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:resourcetype>{}</d:resourcetype><d:getcontentlength>0</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",if directory{"<d:collection/>"}else{""})
            } else {
                "untrusted server body TOP_SECRET".into()
            };
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        Self {
            url: format!("http://{address}/dav/"),
            seen,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
#[tokio::test]
async fn worker_authenticates_root_metadata_without_reading_or_writing_file_content() {
    let (_t, home) = home();
    let key = format!("RDRIVE_CONNECTION_WORKER_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "fixture-password");
    for (status, dir, expected) in [
        (207, true, None),
        (207, false, Some(Code::RootUnavailable)),
        (401, true, Some(Code::AuthenticationFailed)),
        (503, true, Some(Code::Server)),
    ] {
        let server = Server::new(status, dir);
        let mut p = profile();
        p.url = server.url.clone();
        p.allow_http = true;
        p.password_env = Some(key.clone());
        set_config(&home, vec![("fixture", p.clone())]);
        let value = worker(&home, "fixture", &binding(&p).unwrap())
            .await
            .unwrap();
        let value: Reply = serde_json::from_value(value).unwrap();
        assert_eq!(value.error, expected);
        assert_eq!(server.seen.load(Ordering::SeqCst), 1);
        assert!(!home.join("connection-health").exists());
    }
    let p = profile();
    set_config(&home, vec![("fixture", p.clone())]);
    let value: Reply =
        serde_json::from_value(worker(&home, "fixture", &"0".repeat(64)).await.unwrap()).unwrap();
    assert_eq!(value.error, Some(Code::ConfigurationChanged));
    assert!(worker(&home, "fixture", "invalid").await.is_err());
    assert!(worker(&home, "missing", &binding(&p).unwrap())
        .await
        .is_err());
    let mut missing = p.clone();
    missing.password_env = Some("RDRIVE_CONNECTION_MUST_NOT_EXIST_319098".into());
    set_config(&home, vec![("fixture", missing.clone())]);
    let value: Reply = serde_json::from_value(
        worker(&home, "fixture", &binding(&missing).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value.error, Some(Code::CredentialsUnavailable));
    std::env::remove_var(key);
}

#[tokio::test]
async fn owned_child_bounds_output_time_process_failures_and_profile_identity() {
    let (t, home) = home();
    let p = profile();
    let scope = binding(&p).unwrap();
    let good = output_script(t.path(), &reply(&p, "p", None));
    let outcome = child(&good, &home, "p", &scope, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(outcome.error, None);
    let missing = t.path().join("missing");
    assert_eq!(
        child(&missing, &home, "p", &scope, Duration::from_secs(1))
            .await
            .unwrap_err(),
        Code::ProbeProcess
    );
    for body in [
        "exit 9",
        "printf invalid",
        r#"python3 -c 'import sys; sys.stdout.write("x"*65537)'"#,
    ] {
        let program = script(t.path(), body);
        let result = child(&program, &home, "p", &scope, Duration::from_secs(2)).await;
        assert!(matches!(
            result,
            Err(Code::ProbeProcess | Code::InvalidResponse)
        ));
    }
    for field in ["version", "binding", "profile_hash", "clock", "future"] {
        let mut value = reply(&p, "p", None);
        match field {
            "version" => value.version = 2,
            "binding" => value.binding = "0".repeat(64),
            "profile_hash" => value.profile_hash = "other".into(),
            "clock" => value.started_at_ms = value.finished_at_ms + 1,
            _ => value.finished_at_ms = now_ms() + 5000,
        };
        let program = output_script(t.path(), &value);
        assert_eq!(
            child(&program, &home, "p", &scope, Duration::from_secs(1))
                .await
                .unwrap_err(),
            Code::InvalidResponse
        );
    }
    let descendant = t.path().join("descendant.pid");
    let program = script(
        t.path(),
        &format!(
            "sleep 60 &\nprintf '%s' $! > '{}'\nwait",
            descendant.display()
        ),
    );
    let start = std::time::Instant::now();
    assert_eq!(
        child(&program, &home, "p", &scope, Duration::from_secs(2))
            .await
            .unwrap_err(),
        Code::Timeout
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: libc::pid_t = fs::read_to_string(descendant).unwrap().parse().unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    let (mut tx, rx) = tokio::io::duplex(32);
    let io = tokio::spawn(async move {
        tx.shutdown().await.unwrap();
    });
    assert!(bounded_read(rx).await.unwrap().is_empty());
    io.await.unwrap();
}

#[tokio::test]
async fn explicit_probe_uses_scoped_results_and_preserves_per_profile_error_reasons() {
    let (t, home) = home();
    let p = profile();
    set_config(&home, vec![("p", p.clone())]);
    let program = output_script(t.path(), &reply(&p, "p", None));
    let result = probe_with(&program, &home, Some("p"), 2, 1).await.unwrap();
    assert_eq!(result["profiles"][0]["state"], "online");
    assert_eq!(result["profiles"][0]["checked_now"], true);
    let failed = output_script(t.path(), &reply(&p, "p", Some(Code::AuthenticationFailed)));
    let result = probe_with(&failed, &home, None, 2, 1).await.unwrap();
    assert_eq!(result["profiles"][0]["state"], "offline");
    assert!(!result["profiles"][0]["last_success_at"].is_null());
    for (timeout, parallel) in [(0, 1), (121, 1), (1, 0), (1, 5)] {
        assert!(probe_with(&program, &home, None, timeout, parallel)
            .await
            .is_err());
    }
    assert!(probe_with(&program, &home, Some("missing"), 1, 1)
        .await
        .is_err());
    let unknown = probe_with(&t.path().join("absent"), &home, Some("p"), 1, 1)
        .await
        .unwrap();
    assert_eq!(unknown["profiles"][0]["state"], "unknown");
    assert_eq!(
        unknown["profiles"][0]["probe_error"]["code"],
        "probe_process"
    );
    let empty = t.path().join("missing-home");
    let value = probe_profiles(&empty, None, 1, 1).await.unwrap();
    assert_eq!(value["profiles"], json!([]));
    assert!(!empty.exists());
}

#[tokio::test]
async fn failed_observation_storage_does_not_hide_a_successful_current_probe() {
    let (t, home) = home();
    let p = profile();
    set_config(&home, vec![("p", p.clone())]);
    observe_at(&home, "p", &p, "cli.ls", 100, 200, None).unwrap();
    let (path, _) = paths(&home, &p, "p");
    write(&path, b"{");
    let program = output_script(t.path(), &reply(&p, "p", None));
    let result = probe_with(&program, &home, None, 2, 1).await.unwrap();
    assert_eq!(result["profiles"][0]["state"], "online");
    assert_eq!(
        result["profiles"][0]["source"],
        "current_probe_not_persisted"
    );
    assert!(!result["profiles"][0]["observation_error"].is_null());
    assert_eq!(fs::read(path).unwrap(), b"{");
}

#[tokio::test]
async fn configuration_changes_removal_and_corruption_never_adopt_a_stale_probe() {
    for mode in ["changed", "removed", "corrupt"] {
        let (t, home) = home();
        let p = profile();
        set_config(&home, vec![("p", p.clone())]);
        let gate = t.path().join("ready");
        let raw = serde_json::to_string(&reply(&p, "p", None)).unwrap();
        let program = script(
            t.path(),
            &format!(
                "touch '{}'\nsleep 0.4\ncat <<'REPLY'\n{raw}\nREPLY",
                gate.display()
            ),
        );
        let task = {
            let home = home.clone();
            tokio::spawn(async move { probe_with(&program, &home, Some("p"), 3, 1).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !gate.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        match mode {
            "changed" => {
                let mut changed = p.clone();
                changed.smb_encryption = false;
                set_config(&home, vec![("p", changed)]);
            }
            "removed" => set_config(&home, vec![]),
            _ => write(&home.join("config.json"), b"{"),
        }
        let result = task.await.unwrap().unwrap();
        assert_eq!(result["profiles"][0]["state"], "unknown");
        assert!(!home.join("connection-health").exists());
        if mode != "corrupt" {
            assert_eq!(
                result["profiles"][0]["probe_error"]["code"],
                "configuration_changed"
            );
        }
    }
}

#[test]
fn inconsistent_observation_event_roles_are_rejected_as_local_corruption() {
    let (_t, home) = home();
    let p = profile();
    observe_at(&home, "p", &p, "cli.ls", 100, 200, None).unwrap();
    let (path, _) = paths(&home, &p, "p");
    let base: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for field in [
        "success_error",
        "failure_success",
        "business_evidence",
        "newer_evidence",
    ] {
        let mut value = base.clone();
        match field {
            "success_error" => value["last_success"]["error"] = json!("timeout"),
            "failure_success" => value["last_failure"] = value["last_attempt"].clone(),
            "business_evidence" => value["evidence"]["error"] = json!("operation"),
            _ => value["evidence"]["finished_at_ms"] = json!(300),
        }
        write(&path, &serde_json::to_vec(&value).unwrap());
        assert!(snapshot(&home, "p", &p, 400, 300).is_err());
    }
}

#[test]
fn host_metadata_failures_and_replaced_open_descriptors_are_not_adopted() {
    let (t, home) = home();
    let error = directory_with(&home, false, |_| {
        Err(std::io::Error::from_raw_os_error(libc::EIO))
    })
    .unwrap_err();
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert!(!home.join("connection-health").exists());
    let path = t.path().join("original");
    write(&path, b"before");
    let opened = File::open(&path).unwrap();
    let replacement = t.path().join("replacement");
    write(&replacement, b"after");
    fs::rename(&path, t.path().join("retained-original")).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(same_identity(&fs::metadata(&path).unwrap(), &opened.metadata().unwrap()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"after");
}

struct FakeSmb {
    reads: Arc<AtomicUsize>,
    directory: Option<bool>,
    path: &'static str,
    error: Option<&'static str>,
}
#[async_trait::async_trait]
impl crate::backend::Backend for FakeSmb {
    fn capabilities(&self) -> crate::backend::Capabilities {
        panic!("probe must only request root metadata")
    }
    async fn stat(&self, _: &str) -> Result<Option<crate::backend::Entry>> {
        panic!("must not read strong content")
    }
    async fn stat_metadata(&self, path: &str) -> Result<Option<crate::backend::Entry>> {
        assert!(path.is_empty());
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.error {
            bail!(error);
        }
        Ok(self.directory.map(|is_dir| crate::backend::Entry {
            path: self.path.into(),
            is_dir,
            size: 0,
            modified: None,
            etag: None,
        }))
    }
    async fn list(&self, _: &str) -> Result<Vec<crate::backend::Entry>> {
        panic!("must not enumerate payloads")
    }
    async fn download(&self, _: &str, _: &Path) -> Result<()> {
        panic!("must not download")
    }
    async fn upload(
        &self,
        _: &Path,
        _: &str,
        _: crate::backend::WriteCondition,
    ) -> Result<crate::backend::Entry> {
        panic!("must not upload")
    }
    async fn mkdir(&self, _: &str) -> Result<()> {
        panic!("must not mutate remote")
    }
    async fn remove(&self, _: &str, _: bool) -> Result<()> {
        panic!("must not mutate remote")
    }
    async fn rename(&self, _: &str, _: &str, _: bool) -> Result<()> {
        panic!("must not mutate remote")
    }
    async fn copy(&self, _: &str, _: &str, _: bool) -> Result<()> {
        panic!("must not mutate remote")
    }
}
struct FakeSmbConnector {
    backend: Arc<FakeSmb>,
    connect_error: Option<&'static str>,
}
#[async_trait::async_trait]
impl Connector for FakeSmbConnector {
    async fn connect(
        &self,
        profile: &Profile,
        _: &str,
    ) -> Result<Arc<dyn crate::backend::Backend>> {
        assert!(matches!(profile.protocol, Protocol::Smb));
        if let Some(error) = self.connect_error {
            bail!(error);
        }
        Ok(self.backend.clone())
    }
}
#[tokio::test]
async fn smb_boundary_probes_only_authenticated_root_metadata_and_preserves_typed_failures() {
    let (_t, home) = home();
    let mut p = profile();
    p.protocol = Protocol::Smb;
    p.url = "smb://nas.invalid/homes".into();
    set_config(&home, vec![("nas", p.clone())]);
    for (directory, path, read_error, connect_error, expected, reads) in [
        (Some(true), "", None, None, None, 1),
        (None, "", None, None, Some(Code::RootUnavailable), 1),
        (
            Some(true),
            "outside",
            None,
            None,
            Some(Code::RootUnavailable),
            1,
        ),
        (
            Some(true),
            "",
            Some("Transport error: Not connected"),
            None,
            Some(Code::Network),
            1,
        ),
        (
            Some(true),
            "",
            None,
            Some("SMB connection or authentication failed: STATUS_LOGON_FAILURE"),
            Some(Code::AuthenticationFailed),
            0,
        ),
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(FakeSmb {
            reads: count.clone(),
            directory,
            path,
            error: read_error,
        });
        let connector = FakeSmbConnector {
            backend,
            connect_error,
        };
        let response: Reply = serde_json::from_value(
            worker_with(&home, "nas", &binding(&p).unwrap(), &connector)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response.error, expected);
        assert_eq!(count.load(Ordering::SeqCst), reads);
        assert!(!home.join("connection-health").exists());
    }
}

#[test]
fn local_conflicts_quotas_and_filters_never_mark_a_reachable_server_offline() {
    let (_t, home) = home();
    let p = profile();
    observe_at(&home, "p", &p, "index.list", 100, 200, None).unwrap();
    for (message, expected) in [
        ("version_conflict: TOP_SECRET", Code::Conflict),
        ("insufficient_quota: TOP_SECRET", Code::Quota),
        ("excluded_by_filter: TOP_SECRET", Code::Filtered),
    ] {
        let error = anyhow::anyhow!(message.to_owned());
        assert_eq!(classify(&error), expected);
        let safe = safe_error(&error);
        assert!(!safe.to_string().contains("TOP_SECRET"));
        assert_eq!(safe["connection_effect"], "none");
        observe_at(&home, "p", &p, "smart.replay", 300, 400, Some(expected)).unwrap();
        assert_eq!(
            snapshot(&home, "p", &p, 500, 300).unwrap()["state"],
            "online"
        );
    }
}

#[tokio::test]
async fn cancelling_a_check_terminates_its_owned_credential_helper_group() {
    let (t, home) = home();
    let p = profile();
    let scope = binding(&p).unwrap();
    let descendant = t.path().join("cancel-descendant.pid");
    let program = script(
        t.path(),
        &format!(
            "sleep 60 &\nprintf '%s' $! > '{}'\nwait",
            descendant.display()
        ),
    );
    let task =
        tokio::spawn(
            async move { child(&program, &home, "p", &scope, Duration::from_secs(60)).await },
        );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !descendant.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid: libc::pid_t = fs::read_to_string(descendant).unwrap().parse().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[tokio::test]
async fn blocked_credential_lookup_timeout_is_unknown_and_preserves_the_last_success() {
    let (t, home) = home();
    let p = profile();
    set_config(&home, vec![("p", p.clone())]);
    observe_at(&home, "p", &p, "index.list", 100, 200, None).unwrap();
    // The same whole-process deadline covers credential lookup and network I/O.
    // A blocked helper cannot prove that the remote endpoint is offline.
    let program = script(t.path(), "sleep 60");
    let checked = probe_with(&program, &home, Some("p"), 1, 1).await.unwrap();
    let row = &checked["profiles"][0];
    assert_eq!(row["state"], "unknown");
    assert_eq!(row["probe_error"]["code"], "timeout");
    assert_eq!(row["probe_error"]["connection_effect"], "unknown");
    assert_eq!(row["last_success_at_ms"], 200);
    assert_eq!(row["last_error"]["detail"]["code"], "timeout");
    assert_eq!(
        snapshot(&home, "p", &p, now_ms(), 300).unwrap()["state"],
        "unknown"
    );
}
