//! Conditional namespace operations and post-commit failures on real HTTP peers.
use super::test_peer::*;
use super::*;

async fn finished(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the peer must receive exactly the expected requests")
        .unwrap();
}

#[tokio::test]
async fn confirmed_put_keeps_its_receipt_when_the_post_commit_stat_is_unavailable() {
    for status in ["404 Not Found", "500 Internal Server Error"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().canonicalize().unwrap().join("source");
        std::fs::write(&source, b"abc").unwrap();
        let (backend, requests, task) =
            server(vec![confirmed_put("\"our-commit\""), reply(status, "")]).await;
        let context = crate::transfer::TransferContext::default();
        let backend = backend.with_transfer_context(context.clone());
        let committed = backend
            .upload(&source, "file", WriteCondition::CreateNew)
            .await
            .unwrap();
        finished(task).await;
        assert_eq!(committed.path, "file");
        assert_eq!(committed.size, 3);
        assert_eq!(committed.etag.as_deref(), Some("\"our-commit\""));
        assert_eq!(committed.modified, None);
        assert_eq!(context.snapshot().completed_jobs, 1);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(b"PUT /root/file "));
        assert!(requests[1].starts_with(b"PROPFIND /root/file "));
        assert_eq!(std::fs::read(&source).unwrap(), b"abc");
    }
}

#[tokio::test]
async fn nonrecursive_directory_delete_stops_after_metadata_without_sending_delete() {
    let directory = entry_xml("folder", 0).replace(
        "<D:resourcetype/>",
        "<D:resourcetype><D:collection/></D:resourcetype>",
    );
    let (backend, requests, task) = server(vec![reply("207 Multi-Status", &directory)]).await;
    let error = backend.remove("folder", false).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("WebDAV directory deletion requires --recursive"),
        "{error:#}"
    );
    finished(task).await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(b"PROPFIND /root/folder "));
}

#[tokio::test]
async fn accepted_lock_without_its_token_is_not_recorded_as_owned() {
    let (backend, requests, task) = server(vec![reply("200 OK", "")]).await;
    let error = backend.lock("file", 60).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("server accepted LOCK but omitted Lock-Token"),
        "{error:#}"
    );
    finished(task).await;
    assert!(backend.owned_locks.lock().unwrap().is_empty());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(b"LOCK /root/file "));
}

#[test]
fn canonical_local_parent_paths_still_reject_traversal_components() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("child")).unwrap();
    std::fs::write(root.join("source"), b"abc").unwrap();
    let error = reject_symlink_components(&root.join("child/../source")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("local transfer path must not contain parent traversal"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(root.join("source")).unwrap(), b"abc");
}

#[tokio::test]
async fn guarded_move_validates_source_identity_and_revision_before_network_io() {
    let (backend, requests, task) = server(vec![]).await;
    let expected = resume_entry(3);
    for (from, to) in [
        ("", "moved"),
        ("file", ""),
        ("file", "file"),
        ("file", "file/child"),
        ("other", "moved"),
    ] {
        let error = backend
            .rename_checked(from, to, &expected)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("invalid guarded WebDAV move"),
            "{from} -> {to}: {error:#}"
        );
    }
    let mut weak = expected.clone();
    weak.etag = Some("W/\"weak\"".into());
    let error = backend
        .rename_checked("file", "moved", &weak)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("guarded WebDAV move requires a strong source ETag"),
        "{error:#}"
    );
    finished(task).await;
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guarded_move_checks_source_etag_and_creates_the_destination_atomically() {
    for token in [None, Some("opaquelocktoken:guarded-source")] {
        let (backend, requests, task) = server(vec![
            reply("201 Created", ""),
            reply("207 Multi-Status", &entry_xml("moved", 3)),
        ])
        .await;
        let source_url = backend.url("file").unwrap();
        let destination_url = backend.url("moved").unwrap();
        let backend = backend.with_lock_token(token.map(str::to_owned)).unwrap();
        let expected = resume_entry(3);
        let actual = backend
            .rename_checked("file", "moved", &expected)
            .await
            .unwrap();
        finished(task).await;
        assert_eq!(actual.path, "moved");
        assert!(actual.same_revision(&expected));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let request = String::from_utf8(requests[0].clone()).unwrap();
        assert!(request.starts_with("MOVE /root/file "));
        assert!(
            request.contains(&format!("destination: {destination_url}\r\n")),
            "{request}"
        );
        assert!(request.contains("overwrite: F\r\n"), "{request}");
        assert!(request.contains("depth: infinity\r\n"), "{request}");
        let condition = match token {
            Some(token) => format!("if: <{source_url}> ([\"revision-1\"] <{token}>)\r\n"),
            None => format!("if: <{source_url}> ([\"revision-1\"])\r\n"),
        };
        assert!(request.contains(&condition), "{request}");
        assert!(!request.contains("if-match:"));
        assert!(requests[1].starts_with(b"PROPFIND /root/moved "));
    }
}

#[tokio::test]
async fn guarded_move_reports_conflict_missing_target_and_changed_target_without_retrying() {
    for case in 0..3 {
        let (responses, expected_error) = match case {
            0 => (
                vec![reply("412 Precondition Failed", "")],
                "WebDAV guarded MOVE: revision conflict (HTTP 412)",
            ),
            1 => (
                vec![reply("201 Created", ""), reply("404 Not Found", "")],
                "version_conflict: moved WebDAV target disappeared",
            ),
            _ => (
                vec![
                    reply("201 Created", ""),
                    reply(
                        "207 Multi-Status",
                        &entry_xml("moved", 3).replace("revision-1", "revision-2"),
                    ),
                ],
                "version_conflict: target revision changed during guarded WebDAV move",
            ),
        };
        let (backend, requests, task) = server(responses).await;
        let error = backend
            .rename_checked("file", "moved", &resume_entry(3))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected_error),
            "case {case}: {error:#}"
        );
        finished(task).await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), if case == 0 { 1 } else { 2 });
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with(b"MOVE "))
                .count(),
            1
        );
        assert!(!requests
            .iter()
            .any(|request| request.starts_with(b"DELETE ")));
    }
}
