//! Real HTTP upload races and conditional publication against temporary peers.
use super::test_peer::*;
use super::*;
use std::sync::Arc;

fn request_text(request: &[u8]) -> String {
    String::from_utf8(request.to_vec()).unwrap()
}

fn saved_upload(backend: &WebDav, source: &Path, guard: UploadGuard) -> NextcloudUpload {
    NextcloudUpload {
        version: 1,
        session: format!("rdrive-{}", uuid::Uuid::new_v4()),
        root: backend.root.to_string(),
        endpoint: backend.nextcloud_upload_url.as_ref().unwrap().to_string(),
        username: backend.username.clone(),
        remote: "file".into(),
        source_path: source.to_str().unwrap().to_owned(),
        source_size: std::fs::metadata(source).unwrap().len(),
        source_sha256: crate::state::fingerprint(source).unwrap(),
        guard,
        chunk_size: MIN_NEXTCLOUD_CHUNK,
        phase: UploadPhase::Uploading,
        chunks: BTreeMap::new(),
        committed: None,
    }
}

async fn finished(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the peer must receive exactly the expected requests")
        .unwrap();
}

#[tokio::test]
async fn ordinary_upload_detects_source_growth_after_the_peer_consumes_the_body() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source");
    std::fs::write(&source, b"abc").unwrap();
    let changed = source.clone();
    let hook = Arc::new(move |request: &[u8]| {
        assert!(request.starts_with(b"PUT /root/file "));
        assert!(request.ends_with(b"\r\n\r\nabc"));
        std::fs::write(&changed, b"abcd").unwrap();
    });
    let (backend, requests, task) =
        server_with_request_hook(vec![confirmed_put("\"committed\"")], false, hook).await;
    let error = backend
        .upload(&source, "file", WriteCondition::CreateNew)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("upload source changed during transfer"),
        "{error:#}"
    );
    finished(task).await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(&source).unwrap(), b"abcd");
}

#[tokio::test]
async fn nextcloud_rechecks_chunk_bytes_and_the_complete_source_before_publication() {
    for append in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let checkpoint = root.join("checkpoint");
        std::fs::write(&source, b"abc").unwrap();
        let changed = source.clone();
        let hook = Arc::new(move |request: &[u8]| {
            if request.starts_with(b"PUT ") {
                assert!(request.ends_with(b"\r\n\r\nabc"));
                if append {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&changed)
                        .unwrap();
                    std::io::Write::write_all(&mut file, b"tail").unwrap();
                } else {
                    std::fs::write(&changed, b"xyz").unwrap();
                }
            }
        });
        let (backend, requests, task) = server_with_request_hook(
            vec![reply("201 Created", ""), confirmed_put("\"chunk\"")],
            true,
            hook,
        )
        .await;
        let error = backend
            .nextcloud_upload(&source, "file", WriteCondition::CreateNew, &checkpoint)
            .await
            .unwrap_err();
        let expected = if append {
            "source changed during chunk transfer"
        } else {
            "source changed while sending a chunk"
        };
        assert!(error.to_string().contains(expected), "{error:#}");
        finished(task).await;
        let state: NextcloudUpload =
            serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
        assert_eq!(state.phase, UploadPhase::Uploading);
        assert_eq!(state.chunks.len(), usize::from(append));
        assert!(!requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.starts_with(b"MOVE ")));
        assert_eq!(
            std::fs::read(&source).unwrap(),
            if append {
                b"abctail".as_slice()
            } else {
                b"xyz".as_slice()
            }
        );
    }
}

#[tokio::test]
async fn changed_or_missing_saved_chunks_are_retransmitted_before_a_guarded_move() {
    for missing in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let checkpoint = root.join("checkpoint");
        std::fs::write(&source, b"abc").unwrap();
        let changed_chunk = if missing {
            reply("404 Not Found", "")
        } else {
            reply(
                "207 Multi-Status",
                &upload_metadata_xml(false, 3, Some("\"new-chunk\"")),
            )
        };
        let (backend, requests, task) = server_configured(
            vec![
                reply(
                    "207 Multi-Status",
                    &upload_metadata_xml(true, 0, Some("\"session\"")),
                ),
                changed_chunk,
                confirmed_put("\"replacement-chunk\""),
                reply("404 Not Found", ""),
                reply("500 Failed", ""),
            ],
            true,
        )
        .await;
        let mut state = saved_upload(&backend, &source, UploadGuard::CreateNew);
        state.chunks.insert(
            1,
            ChunkReceipt {
                size: 3,
                sha256: crate::state::fingerprint(&source).unwrap(),
                etag: Some("\"saved-chunk\"".into()),
            },
        );
        crate::state::atomic_json(&checkpoint, &state).unwrap();
        let error = backend
            .nextcloud_upload(&source, "file", WriteCondition::CreateNew, &checkpoint)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Nextcloud final MOVE failed (HTTP 500"),
            "{error:#}"
        );
        finished(task).await;
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with(b"PROPFIND /uploads/user/"));
        assert!(requests[1].starts_with(b"PROPFIND /uploads/user/"));
        assert!(requests[2].starts_with(b"PUT /uploads/user/"));
        assert!(requests[2].ends_with(b"\r\n\r\nabc"));
        let final_request = request_text(&requests[4]).to_lowercase();
        assert!(final_request.starts_with("move /uploads/user/"));
        assert!(final_request.contains("\r\noverwrite: f\r\n"));
        let state: NextcloudUpload =
            serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
        assert_eq!(state.phase, UploadPhase::Publishing);
        assert_eq!(
            state.chunks[&1].etag.as_deref(),
            Some("\"replacement-chunk\"")
        );
    }
}

#[tokio::test]
async fn nextcloud_final_destination_guards_preserve_directories_existing_files_and_revisions() {
    for case in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let checkpoint = root.join("checkpoint");
        std::fs::write(&source, b"abc").unwrap();
        let mut entry = resume_entry(3);
        let (condition, metadata, expected) = match case {
            0 => (
                WriteCondition::Any,
                entry_xml("file", 3).replace(
                    "<D:resourcetype/>",
                    "<D:resourcetype><D:collection/></D:resourcetype>",
                ),
                "upload destination is a directory",
            ),
            1 => (
                WriteCondition::CreateNew,
                entry_xml("file", 3),
                "upload destination already exists",
            ),
            2 => {
                entry.etag = Some("\"original\"".into());
                (
                    WriteCondition::Match(entry),
                    entry_xml("file", 3),
                    "upload destination revision changed",
                )
            }
            _ => (
                WriteCondition::Any,
                entry_xml("file", 3).replace("&quot;revision-1&quot;", "W/&quot;weak&quot;"),
                "safe Nextcloud overwrite requires a strong destination ETag",
            ),
        };
        let (backend, requests, task) = server_configured(
            vec![
                reply("201 Created", ""),
                confirmed_put("\"chunk\""),
                reply("207 Multi-Status", &metadata),
            ],
            true,
        )
        .await;
        let error = backend
            .nextcloud_upload(&source, "file", condition, &checkpoint)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "case {case}: {error:#}"
        );
        finished(task).await;
        assert_eq!(requests.lock().unwrap().len(), 3);
        let state: NextcloudUpload =
            serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
        assert_eq!(state.phase, UploadPhase::Uploading);
        assert_eq!(std::fs::read(&source).unwrap(), b"abc");
    }
}

#[tokio::test]
async fn explicit_nextcloud_overwrite_is_converted_to_a_conditional_move_with_the_lock_token() {
    for exists in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let checkpoint = root.join("checkpoint");
        std::fs::write(&source, b"abc").unwrap();
        let target = if exists {
            reply("207 Multi-Status", &entry_xml("file", 3))
        } else {
            reply("404 Not Found", "")
        };
        let (backend, requests, task) = server_configured(
            vec![
                reply("201 Created", ""),
                confirmed_put("\"chunk\""),
                target,
                reply("500 Failed", ""),
            ],
            true,
        )
        .await;
        let destination = backend.url("file").unwrap().to_string();
        let backend = backend
            .with_lock_token(Some("opaquelocktoken:test-upload".into()))
            .unwrap();
        let error = backend
            .nextcloud_upload(&source, "file", WriteCondition::Any, &checkpoint)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Nextcloud final MOVE failed (HTTP 500"),
            "{error:#}"
        );
        finished(task).await;
        let requests = requests.lock().unwrap();
        let final_request = request_text(&requests[3]);
        assert!(final_request.starts_with("MOVE /uploads/user/"));
        let lowercase = final_request.to_lowercase();
        assert!(lowercase.contains(if exists {
            "\r\noverwrite: t\r\n"
        } else {
            "\r\noverwrite: f\r\n"
        }));
        let conditions = if exists {
            "[\"revision-1\"] <opaquelocktoken:test-upload>"
        } else {
            "<opaquelocktoken:test-upload>"
        };
        assert!(
            final_request.contains(&format!("if: <{destination}> ({conditions})\r\n")),
            "{final_request}"
        );
        assert!(
            final_request.contains(&format!("destination: {destination}\r\n")),
            "{final_request}"
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"abc");
    }
}

#[tokio::test]
async fn upload_registry_failure_is_propagated_before_sending_untracked_body_bytes() {
    for nextcloud in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let checkpoint = root.join("checkpoint");
        std::fs::write(&source, b"abc").unwrap();
        let responses = if nextcloud {
            vec![reply("201 Created", "")]
        } else {
            vec![]
        };
        let (backend, requests, task) = server_configured(responses, nextcloud).await;
        let context = crate::transfer::TransferContext::default();
        context.poison_registry_for_test();
        let backend = backend.with_transfer_context(context);
        let result = if nextcloud {
            backend
                .nextcloud_upload(&source, "file", WriteCondition::CreateNew, &checkpoint)
                .await
        } else {
            backend
                .upload(&source, "file", WriteCondition::CreateNew)
                .await
        };
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transfer registry is unavailable"),
            "{error:#}"
        );
        finished(task).await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), usize::from(nextcloud));
        assert!(!requests.iter().any(|request| request.starts_with(b"PUT ")));
        if nextcloud {
            let state: NextcloudUpload =
                serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
            assert_eq!(state.phase, UploadPhase::Uploading);
            assert!(state.chunks.is_empty());
        }
    }
}
