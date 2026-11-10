use rdrive::{
    backend::{Backend, Entry, WriteCondition},
    config::{Profile, Protocol},
    webdav::WebDav,
};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn server(
    responses: Vec<String>,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    server_configured(responses, false).await
}
async fn server_configured(
    responses: Vec<String>,
    nextcloud: bool,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            let boundary = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0, "request closed before headers");
                data.extend_from_slice(&buffer[..count]);
                if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(data.len() < 64 * 1024);
            };
            let headers = String::from_utf8_lossy(&data[..boundary]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while data.len() < boundary + length {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0, "request closed before body");
                data.extend_from_slice(&buffer[..count]);
            }
            let response = if response.contains("{{request_path}}") {
                let target = String::from_utf8_lossy(&data[..boundary])
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let (head, body) = response.split_once("\r\n\r\n").unwrap();
                let body = body.replace("{{request_path}}", &target);
                let head = head
                    .lines()
                    .filter(|line| !line.to_lowercase().starts_with("content-length:"))
                    .collect::<Vec<_>>()
                    .join("\r\n");
                format!("{head}\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            } else {
                response
            };
            captured.lock().unwrap().push(data);
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let profile = Profile {
        protocol: Protocol::Webdav,
        url: format!("http://{address}/root/"),
        username: Some("user".into()),
        password_env: None,
        allow_http: true,
        timeout_secs: 5,
        smb_encryption: true,
        nextcloud_upload_url: nextcloud.then(|| format!("http://{address}/uploads/user/")),
    };
    (
        WebDav::new(&profile, "test-secret").unwrap(),
        requests,
        task,
    )
}
fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
fn entry_xml(path: &str, size: u64) -> String {
    format!(
        r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/root/{path}</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength><D:getetag>&quot;revision-1&quot;</D:getetag><D:getlastmodified>Mon, 09 Nov 2026 08:00:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}
fn resume_entry(size: u64) -> Entry {
    Entry {
        path: "file".into(),
        is_dir: false,
        size,
        modified: None,
        etag: Some("\"revision-1\"".into()),
    }
}
fn range_reply(start: u64, end: u64, total: u64, body: &str) -> String {
    format!("HTTP/1.1 206 Partial Content\r\nETag: \"revision-1\"\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", end - start + 1)
}
fn upload_metadata_xml(is_dir: bool, size: u64, etag: Option<&str>) -> String {
    let kind = if is_dir { "<D:collection/>" } else { "" };
    let etag = etag
        .map(|etag| format!("<D:getetag>{}</D:getetag>", etag.replace('"', "&quot;")))
        .unwrap_or_default();
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>{{{{request_path}}}}</D:href><D:propstat><D:prop><D:resourcetype>{kind}</D:resourcetype><D:getcontentlength>{size}</D:getcontentlength>{etag}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}
fn confirmed_put(etag: &str) -> String {
    format!(
        "HTTP/1.1 201 Created\r\nETag: {etag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[tokio::test]
async fn auth_error_is_never_interpreted_as_missing_resource() {
    let (backend, requests, task) = server(vec![reply("401 Unauthorized", "")]).await;
    let error = backend.stat("file").await.unwrap_err().to_string();
    assert!(error.contains("401"));
    assert!(!error.contains("test-secret"));
    task.await.unwrap();
    let request = String::from_utf8(requests.lock().unwrap()[0].clone()).unwrap();
    assert!(request.starts_with("PROPFIND /root/file HTTP/1.1\r\n"));
    assert!(request.to_lowercase().contains("authorization: basic "));
}

#[tokio::test]
async fn directory_listing_accepts_empty_lengths_before_or_after_resource_type() {
    let xml = r#"<D:multistatus xmlns:D="DAV:">
        <D:response><D:href>/root/</D:href><D:propstat><D:prop>
            <D:getcontentlength></D:getcontentlength><D:resourcetype><D:collection/></D:resourcetype>
        </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
        <D:response><D:href>/root/child/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype><D:getcontentlength>  </D:getcontentlength>
        </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
        <D:response><D:href>/root/separate/</D:href><D:propstat><D:prop>
            <D:getcontentlength></D:getcontentlength>
        </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype>
        </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
        <D:response><D:href>/root/empty.txt</D:href><D:propstat><D:prop>
            <D:getcontentlength>0</D:getcontentlength><D:resourcetype/>
        </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    </D:multistatus>"#;
    let (backend, requests, task) = server(vec![reply("207 Multi-Status", xml)]).await;
    let entries = backend.list("").await.unwrap();
    task.await.unwrap();
    assert_eq!(entries.len(), 3);
    for name in ["child", "separate"] {
        let entry = entries.iter().find(|entry| entry.path == name).unwrap();
        assert!(entry.is_dir);
        assert_eq!(entry.size, 0);
    }
    let file = entries
        .iter()
        .find(|entry| entry.path == "empty.txt")
        .unwrap();
    assert!(!file.is_dir);
    assert_eq!(file.size, 0);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn files_keep_rejecting_empty_missing_or_invalid_content_lengths() {
    for length in [
        "",
        "<D:getcontentlength/>",
        "<D:getcontentlength></D:getcontentlength>",
        "<D:getcontentlength> </D:getcontentlength>",
        "<D:getcontentlength>invalid</D:getcontentlength>",
        "<D:getcontentlength>-1</D:getcontentlength>",
        "<D:getcontentlength>1</D:getcontentlength><D:getcontentlength></D:getcontentlength>",
    ] {
        let xml = format!(
            r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/root/file</D:href><D:propstat><D:prop>{length}<D:resourcetype/><D:getetag>&quot;revision-1&quot;</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
        );
        let (backend, _, task) = server(vec![reply("207 Multi-Status", &xml)]).await;
        assert!(
            backend.stat("file").await.is_err(),
            "accepted file size from {length:?}"
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn checked_download_rejects_chunked_overrun_and_short_payloads() {
    for (body, expected_size) in [
        ("3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n", 3),
        ("2\r\nab\r\n0\r\n\r\n", 3),
    ] {
        let response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nETag: \"revision-1\"\r\nConnection: close\r\n\r\n{body}");
        let (backend, _, task) = server(vec![response]).await;
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().canonicalize().unwrap().join("payload");
        assert!(backend
            .download_checked("file", &local, &resume_entry(expected_size))
            .await
            .is_err());
        assert!(
            !local.exists(),
            "failed bounded read retained a cache payload"
        );
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_bypasses_proxy_environment_and_remote_hosts_keep_it() {
    const CHILD_URL: &str = "RDRIVE_PROXY_BYPASS_TEST_URL";
    if let Some(url) = std::env::var_os(CHILD_URL) {
        let profile = Profile {
            protocol: Protocol::Webdav,
            url: url.to_str().unwrap().into(),
            username: Some("fixture-user".into()),
            password_env: None,
            allow_http: true,
            timeout_secs: 2,
            smb_encryption: true,
            nextcloud_upload_url: None,
        };
        assert_eq!(
            WebDav::new(&profile, "fixture-only-password")
                .unwrap()
                .stat("file")
                .await
                .unwrap()
                .unwrap()
                .size,
            3
        );
        return;
    }
    for host in ["127.0.0.1", "LOCALHOST", "example.invalid"] {
        let recipient = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = recipient.local_addr().unwrap();
        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        let remote_proxy_case = host == "example.invalid";
        let proxy_address = if remote_proxy_case {
            address
        } else {
            trap.local_addr().unwrap()
        };
        let url = if remote_proxy_case {
            "http://example.invalid/root/".to_string()
        } else {
            format!("http://{host}:{}/root/", address.port())
        };
        let recipient_task = tokio::spawn(async move {
            let (mut socket, _) = recipient.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            let boundary = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buffer[..count]);
                if let Some(position) = data.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break position + 4;
                }
                assert!(data.len() < 64 * 1024);
            };
            let header = String::from_utf8_lossy(&data[..boundary]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while data.len() < boundary + length {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buffer[..count]);
            }
            socket
                .write_all(reply("207 Multi-Status", &entry_xml("file", 3)).as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
            String::from_utf8(data).unwrap()
        });
        // Environment changes are confined to the child process; parallel tests
        // never race process-global HTTP_PROXY/NO_PROXY mutation.
        let output = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "loopback_bypasses_proxy_environment_and_remote_hosts_keep_it",
                    "--nocapture",
                ])
                .env(CHILD_URL, url)
                .env("HTTP_PROXY", format!("http://{proxy_address}"))
                .env("HTTPS_PROXY", format!("http://{proxy_address}"))
                .env("ALL_PROXY", format!("http://{proxy_address}"));
            for key in [
                "NO_PROXY",
                "no_proxy",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env_remove(key);
            }
            command.output().unwrap()
        })
        .await
        .unwrap();
        if !output.status.success() {
            recipient_task.abort();
        }
        assert!(
            output.status.success(),
            "{host}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let request = recipient_task.await.unwrap();
        assert!(request.to_lowercase().contains("authorization: basic "));
        if remote_proxy_case {
            assert!(request.starts_with("PROPFIND http://example.invalid/root/file "));
        } else {
            assert!(request.starts_with("PROPFIND /root/file "));
        }
        assert_eq!(
            trap.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "loopback credentials were sent to a proxy"
        );
    }
}

#[tokio::test]
async fn create_new_upload_is_streamed_and_conditionally_guarded() {
    let (backend, requests, task) = server(vec!["HTTP/1.1 201 Created\r\nETag: \"revision-1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(), reply("207 Multi-Status", &entry_xml("a%20b.txt", 5))]).await;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, b"hello").unwrap();
    let result = backend
        .upload(&source, "a b.txt", WriteCondition::CreateNew)
        .await
        .unwrap();
    assert_eq!(result.etag.as_deref(), Some("\"revision-1\""));
    assert_eq!(result.size, 5);
    task.await.unwrap();
    let data = requests.lock().unwrap();
    let request = String::from_utf8(data[0].clone()).unwrap();
    assert!(request.starts_with("PUT /root/a%20b.txt HTTP/1.1\r\n"));
    assert!(request.to_lowercase().contains("if-none-match: *\r\n"));
    assert!(request.ends_with("\r\n\r\nhello"));
}

#[tokio::test]
async fn conflict_does_not_fall_back_to_unconditional_write() {
    let (backend, requests, task) = server(vec![reply("412 Precondition Failed", "")]).await;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, b"hello").unwrap();
    let expected = Entry {
        path: "file".into(),
        is_dir: false,
        size: 5,
        modified: None,
        etag: Some("\"old\"".into()),
    };
    assert!(backend
        .upload(&source, "file", WriteCondition::Match(expected))
        .await
        .unwrap_err()
        .to_string()
        .contains("412"));
    task.await.unwrap();
    let data = requests.lock().unwrap();
    assert_eq!(data.len(), 1);
    let request = String::from_utf8(data[0].clone()).unwrap().to_lowercase();
    assert!(request.contains("if-match: \"old\"\r\n"));
}

#[tokio::test]
async fn successful_put_without_commit_etag_does_not_invent_a_revision() {
    let (backend, requests, task) = server(vec![reply("201 Created", "")]).await;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, b"hello").unwrap();
    let committed = backend
        .upload(&source, "file", WriteCondition::CreateNew)
        .await
        .unwrap();
    assert_eq!(committed.size, 5);
    assert_eq!(committed.etag, None);
    assert_eq!(committed.modified, None);
    task.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn put_does_not_claim_a_later_writers_revision() {
    let response = "HTTP/1.1 201 Created\r\nETag: \"our-commit\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (backend, _, task) = server(vec![
        response.into(),
        reply("207 Multi-Status", &entry_xml("file", 5)),
    ])
    .await;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, b"hello").unwrap();
    let committed = backend
        .upload(&source, "file", WriteCondition::CreateNew)
        .await
        .unwrap();
    assert_eq!(committed.etag.as_deref(), Some("\"our-commit\""));
    assert_eq!(committed.modified, None);
    task.await.unwrap();
}

#[tokio::test]
async fn interrupted_download_removes_partial_destination() {
    let (backend, _, task) = server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc".into(),
    ])
    .await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().canonicalize().unwrap().join("partial");
    assert!(backend.download("file", &destination).await.is_err());
    assert!(!destination.exists());
    task.await.unwrap();
}

#[tokio::test]
async fn checked_get_is_bound_to_the_requested_server_revision() {
    let (backend, requests, task) = server(vec![reply("412 Precondition Failed", "")]).await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().canonicalize().unwrap().join("download");
    let expected = Entry {
        path: "file".into(),
        is_dir: false,
        size: 5,
        modified: None,
        etag: Some("\"old\"".into()),
    };
    assert!(backend
        .download_checked("file", &destination, &expected)
        .await
        .unwrap_err()
        .to_string()
        .contains("412"));
    assert!(!destination.exists());
    task.await.unwrap();
    let request = String::from_utf8(requests.lock().unwrap()[0].clone())
        .unwrap()
        .to_lowercase();
    assert!(request.starts_with("get /root/file http/1.1\r\n"));
    assert!(request.contains("if-match: \"old\"\r\n"));
}

#[tokio::test]
async fn server_conditional_delete_uses_revision_without_preflight_race() {
    let (backend, requests, task) = server(vec![reply("204 No Content", "")]).await;
    let expected = Entry {
        path: "file".into(),
        is_dir: false,
        size: 5,
        modified: None,
        etag: Some("\"old\"".into()),
    };
    backend.remove_checked("file", &expected).await.unwrap();
    task.await.unwrap();
    let data = requests.lock().unwrap();
    assert_eq!(data.len(), 1);
    let request = String::from_utf8(data[0].clone()).unwrap().to_lowercase();
    assert!(request.starts_with("delete /root/file http/1.1\r\n"));
    assert!(request.contains("if-match: \"old\"\r\n"));
}

#[tokio::test]
async fn redirect_is_not_followed_to_another_origin() {
    let (backend, requests, task) = server(vec!["HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]).await;
    assert!(backend
        .stat("file")
        .await
        .unwrap_err()
        .to_string()
        .contains("redirect"));
    task.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lock_token_is_used_for_writes_and_unlock() {
    let locked = "HTTP/1.1 200 OK\r\nLock-Token: <opaquelocktoken:test-lock>\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (backend, requests, task) = server(vec![
        locked.into(),
        reply("204 No Content", ""),
        reply("204 No Content", ""),
    ])
    .await;
    let token = backend.lock("file", 600).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, b"hello").unwrap();
    backend
        .upload(&source, "file", WriteCondition::Any)
        .await
        .unwrap();
    backend.unlock("file", &token).await.unwrap();
    task.await.unwrap();
    let data = requests.lock().unwrap();
    let put = String::from_utf8(data[1].clone()).unwrap().to_lowercase();
    let unlock = String::from_utf8(data[2].clone()).unwrap().to_lowercase();
    assert!(put.contains("if: (<opaquelocktoken:test-lock>)\r\n"));
    assert!(unlock.contains("lock-token: <opaquelocktoken:test-lock>\r\n"));
}

#[tokio::test]
async fn resume_requests_only_missing_suffix_and_preserves_existing_prefix() {
    let (backend, requests, task) = server(vec![range_reply(3, 7, 8, "defgh")]).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    backend
        .download_resume("file", &partial, &resume_entry(8), 3)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&partial).unwrap(), b"abcdefgh");
    task.await.unwrap();
    let request = String::from_utf8(requests.lock().unwrap()[0].clone())
        .unwrap()
        .to_lowercase();
    assert!(request.contains("range: bytes=3-\r\n"));
    assert!(request.contains("if-match: \"revision-1\"\r\n"));
    assert!(request.contains("accept-encoding: identity\r\n"));
}

#[tokio::test]
async fn interrupted_resume_keeps_completed_bytes_and_next_attempt_uses_new_offset() {
    let (backend, requests, task) = server(vec![
        range_reply(3, 7, 8, "de"),
        range_reply(5, 7, 8, "fgh"),
    ])
    .await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    assert!(backend
        .download_resume("file", &partial, &resume_entry(8), 3)
        .await
        .is_err());
    assert_eq!(std::fs::read(&partial).unwrap(), b"abcde");
    backend
        .download_resume("file", &partial, &resume_entry(8), 5)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&partial).unwrap(), b"abcdefgh");
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    let second = String::from_utf8(requests[1].clone())
        .unwrap()
        .to_lowercase();
    assert!(second.contains("range: bytes=5-\r\n"));
}

#[tokio::test]
async fn ignored_range_never_appends_a_second_full_representation() {
    let (backend, _, task) = server(vec![reply("200 OK", "abcdefgh")]).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    let error = backend
        .download_resume("file", &partial, &resume_entry(8), 3)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ignored"));
    assert_eq!(std::fs::read(&partial).unwrap(), b"abc");
    task.await.unwrap();
}

#[tokio::test]
async fn malformed_content_range_never_modifies_checkpoint_bytes() {
    let (backend, _, task) = server(vec![range_reply(0, 7, 8, "abcdefgh")]).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    let error = backend
        .download_resume("file", &partial, &resume_entry(8), 3)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Content-Range"));
    assert_eq!(std::fs::read(&partial).unwrap(), b"abc");
    task.await.unwrap();
}

#[tokio::test]
async fn changed_etag_never_mixes_two_remote_versions() {
    let response = range_reply(3, 7, 8, "defgh").replace("\"revision-1\"", "\"new-revision\"");
    let (backend, _, task) = server(vec![response]).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    assert!(backend
        .download_resume("file", &partial, &resume_entry(8), 3)
        .await
        .unwrap_err()
        .to_string()
        .contains("ETag"));
    assert_eq!(std::fs::read(&partial).unwrap(), b"abc");
    task.await.unwrap();
}

#[tokio::test]
async fn resume_requires_a_strong_revision_and_matching_local_length() {
    let (backend, requests, task) = server(Vec::new()).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"abc").unwrap();
    let mut weak = resume_entry(8);
    weak.etag = Some("W/\"revision-1\"".into());
    assert!(backend
        .download_resume("file", &partial, &weak, 3)
        .await
        .unwrap_err()
        .to_string()
        .contains("strong ETag"));
    assert!(backend
        .download_resume("file", &partial, &resume_entry(8), 4)
        .await
        .unwrap_err()
        .to_string()
        .contains("length"));
    assert_eq!(std::fs::read(&partial).unwrap(), b"abc");
    task.await.unwrap();
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn complete_checkpoint_verifies_revision_without_redownloading_body() {
    let (backend, requests, task) =
        server(vec![reply("207 Multi-Status", &entry_xml("file", 5))]).await;
    let temp = tempfile::tempdir().unwrap();
    let partial = temp.path().canonicalize().unwrap().join("partial");
    std::fs::write(&partial, b"hello").unwrap();
    backend
        .download_resume("file", &partial, &resume_entry(5), 5)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&partial).unwrap(), b"hello");
    task.await.unwrap();
    let request = String::from_utf8(requests.lock().unwrap()[0].clone()).unwrap();
    assert!(request.starts_with("PROPFIND "));
}

#[tokio::test]
async fn generic_webdav_resume_upload_rejects_without_mutation() {
    let (backend, requests, task) = server(Vec::new()).await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"data").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("Nextcloud"));
    assert!(!checkpoint.exists());
    task.await.unwrap();
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nextcloud_resume_skips_only_confirmed_unchanged_chunks() {
    let mut bytes = vec![b'a'; 5 * 1024 * 1024];
    bytes.extend_from_slice(b"end");
    let body = String::from_utf8(bytes.clone()).unwrap();
    let file_xml = entry_xml("file", bytes.len() as u64);
    let responses = vec![
        reply("201 Created", ""),
        confirmed_put("\"part-1\""),
        reply("503 Service Unavailable", ""),
        reply("207 Multi-Status", &upload_metadata_xml(true, 0, None)),
        reply(
            "207 Multi-Status",
            &upload_metadata_xml(false, 5 * 1024 * 1024, Some("\"part-1\"")),
        ),
        confirmed_put("\"part-2\""),
        reply("404 Not Found", ""),
        reply("201 Created", ""),
        reply("207 Multi-Status", &file_xml),
        reply("200 OK", &body),
        reply("207 Multi-Status", &file_xml),
    ];
    let (backend, requests, task) = server_configured(responses, true).await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, bytes).unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    let interrupted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    assert_eq!(interrupted["phase"], "uploading");
    assert_eq!(interrupted["chunks"].as_object().unwrap().len(), 1);
    let result = backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap();
    assert_eq!(result.size, body.len() as u64);
    let complete: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    assert_eq!(complete["phase"], "complete");
    // A completed receipt is durable even if the process ended before queue
    // completion; replaying it sends neither chunks nor another MOVE.
    backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap();
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    let first_lines = requests
        .iter()
        .map(|data| {
            String::from_utf8_lossy(data)
                .lines()
                .next()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        first_lines
            .iter()
            .filter(|line| line.starts_with("PUT ") && line.contains("/00001 "))
            .count(),
        1
    );
    assert_eq!(
        first_lines
            .iter()
            .filter(|line| line.starts_with("PUT ") && line.contains("/00002 "))
            .count(),
        2
    );
    let moved = requests
        .iter()
        .find(|data| data.starts_with(b"MOVE "))
        .unwrap();
    let moved = String::from_utf8_lossy(moved).to_lowercase();
    assert!(moved.contains("overwrite: f\r\n"));
    assert!(moved.contains(&format!("oc-total-length: {}\r\n", body.len())));
}

#[tokio::test]
async fn nextcloud_does_not_reuse_a_chunk_without_a_confirmed_validator() {
    let mut bytes = vec![b'a'; 5 * 1024 * 1024];
    bytes.extend_from_slice(b"end");
    let body = String::from_utf8(bytes.clone()).unwrap();
    let file_xml = entry_xml("file", bytes.len() as u64);
    let responses = vec![
        reply("201 Created", ""),
        reply("201 Created", ""),
        reply("503 Service Unavailable", ""),
        reply("207 Multi-Status", &upload_metadata_xml(true, 0, None)),
        confirmed_put("\"part-1\""),
        confirmed_put("\"part-2\""),
        reply("404 Not Found", ""),
        reply("201 Created", ""),
        reply("207 Multi-Status", &file_xml),
        reply("200 OK", &body),
        reply("207 Multi-Status", &file_xml),
    ];
    let (backend, requests, task) = server_configured(responses, true).await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, bytes).unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap();
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    let first_lines = requests
        .iter()
        .map(|data| {
            String::from_utf8_lossy(data)
                .lines()
                .next()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    // Size alone is not evidence that these bytes are our saved chunk.
    assert_eq!(
        first_lines
            .iter()
            .filter(|line| line.starts_with("PUT ") && line.contains("/00001 "))
            .count(),
        2
    );
}

#[tokio::test]
async fn nextcloud_resume_rejects_a_changed_source_before_network_mutation() {
    let (backend, requests, task) = server_configured(
        vec![
            reply("201 Created", ""),
            reply("503 Service Unavailable", ""),
        ],
        true,
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"old").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    let saved = std::fs::read(&checkpoint).unwrap();
    std::fs::write(&source, b"new").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("different source"));
    assert_eq!(std::fs::read(&checkpoint).unwrap(), saved);
    task.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn nextcloud_rejects_session_metadata_outside_explicit_upload_endpoint() {
    let escaped =
        upload_metadata_xml(true, 0, None).replace("{{request_path}}", "/outside/session");
    let (backend, requests, task) = server_configured(
        vec![
            reply("201 Created", ""),
            reply("503 Service Unavailable", ""),
            reply("207 Multi-Status", &escaped),
        ],
        true,
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"new").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("outside"));
    task.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn configured_context_throttles_the_actual_http_body_before_server_receipt() {
    let (backend, requests, server_task) = server(vec![reply("201 Created", "")]).await;
    let context = rdrive::transfer::TransferContext::new(
        Some(32 * 1024),
        rdrive::transfer::ProgressOutput::Off,
    )
    .unwrap();
    let backend = backend.with_transfer_context(context.clone());
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().canonicalize().unwrap().join("source");
    std::fs::write(&source, vec![7u8; 32 * 1024]).unwrap();
    let upload = tokio::spawn(async move {
        backend
            .upload(&source, "file", WriteCondition::CreateNew)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        requests.lock().unwrap().is_empty(),
        "server received a full body before its bandwidth budget was available"
    );
    upload.await.unwrap().unwrap();
    server_task.await.unwrap();
    let report = context.snapshot();
    assert_eq!(report.transferred, 32 * 1024);
    assert_eq!(report.completed_jobs, 1);
    assert_eq!(report.active_jobs, 0);
}

#[tokio::test]
async fn nextcloud_final_move_guards_destination_with_tagged_if() {
    let old_xml = entry_xml("file", 3).replace("revision-1", "old");
    let (backend, requests, task) = server_configured(
        vec![
            reply("201 Created", ""),
            confirmed_put("\"part\""),
            reply("207 Multi-Status", &old_xml),
            reply("412 Precondition Failed", ""),
        ],
        true,
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"new").unwrap();
    let old = Entry {
        path: "file".into(),
        is_dir: false,
        size: 3,
        modified: None,
        etag: Some("\"old\"".into()),
    };
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::Match(old), &checkpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("412"));
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    let request = String::from_utf8_lossy(requests.last().unwrap()).to_lowercase();
    assert!(request.starts_with("move "));
    assert!(request.contains("overwrite: t\r\n"));
    assert!(request
        .lines()
        .any(|line| line.starts_with("if: <http://127.0.0.1:")
            && line.ends_with("/root/file> ([\"old\"])")));
    assert!(!request.contains("if-match:"));
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(checkpoint).unwrap()).unwrap();
    assert_eq!(state["phase"], "uploading");
}

#[tokio::test]
async fn ambiguous_nextcloud_move_is_verified_without_repeating_the_write() {
    let xml = entry_xml("file", 3);
    let responses = vec![
        reply("201 Created", ""),
        confirmed_put("\"part\""),
        reply("404 Not Found", ""),
        reply("500 Internal Server Error", ""),
        reply("207 Multi-Status", &xml),
        reply("200 OK", "new"),
        reply("207 Multi-Status", &xml),
    ];
    let (backend, requests, task) = server_configured(responses, true).await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"new").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    assert_eq!(state["phase"], "publishing");
    assert_eq!(
        backend
            .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
            .await
            .unwrap()
            .size,
        3
    );
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|data| data.starts_with(b"MOVE "))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|data| data.starts_with(b"PUT "))
            .count(),
        1
    );
}

#[tokio::test]
async fn ambiguous_nextcloud_move_never_overwrites_a_different_destination() {
    let xml = entry_xml("file", 3);
    let responses = vec![
        reply("201 Created", ""),
        confirmed_put("\"part\""),
        reply("404 Not Found", ""),
        reply("500 Internal Server Error", ""),
        reply("207 Multi-Status", &xml),
        reply("200 OK", "old"),
        reply("207 Multi-Status", &xml),
    ];
    let (backend, requests, task) = server_configured(responses, true).await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let source = directory.join("source");
    let checkpoint = directory.join("checkpoint.json");
    std::fs::write(&source, b"new").unwrap();
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .is_err());
    assert!(backend
        .upload_resume(&source, "file", WriteCondition::CreateNew, &checkpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|data| data.starts_with(b"MOVE "))
            .count(),
        1
    );
}
