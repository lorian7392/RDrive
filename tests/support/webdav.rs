use rdrive::{
    backend::Entry,
    config::{Profile, Protocol},
    webdav::WebDav,
};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(crate) async fn server(
    responses: Vec<String>,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    server_configured(responses, false).await
}
pub(crate) async fn server_configured(
    responses: Vec<String>,
    nextcloud: bool,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    server_with_disconnect_policy(responses, nextcloud, false).await
}
pub(crate) async fn server_with_disconnect_policy(
    responses: Vec<String>,
    nextcloud: bool,
    allow_early_disconnect: bool,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    server_with_options(responses, nextcloud, allow_early_disconnect, None).await
}

pub(crate) async fn server_with_request_hook(
    responses: Vec<String>,
    nextcloud: bool,
    hook: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> (
    WebDav,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    server_with_options(responses, nextcloud, false, Some(hook)).await
}

async fn server_with_options(
    responses: Vec<String>,
    nextcloud: bool,
    allow_early_disconnect: bool,
    hook: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
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
            // Observe a complete request before acknowledging it. Tests can
            // reproduce a source-file race without relying on stream timing.
            if let Some(hook) = &hook {
                hook(&data);
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
            let sent = socket.write_all(response.as_bytes()).await;
            let closed = socket.shutdown().await;
            if !allow_early_disconnect {
                sent.unwrap();
                closed.unwrap();
            }
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
pub(crate) fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
pub(crate) fn entry_xml(path: &str, size: u64) -> String {
    format!(
        r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/root/{path}</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength><D:getetag>&quot;revision-1&quot;</D:getetag><D:getlastmodified>Mon, 09 Nov 2026 08:00:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}
pub(crate) fn resume_entry(size: u64) -> Entry {
    Entry {
        path: "file".into(),
        is_dir: false,
        size,
        modified: None,
        etag: Some("\"revision-1\"".into()),
    }
}
pub(crate) fn range_reply(start: u64, end: u64, total: u64, body: &str) -> String {
    format!("HTTP/1.1 206 Partial Content\r\nETag: \"revision-1\"\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", end - start + 1)
}
pub(crate) fn upload_metadata_xml(is_dir: bool, size: u64, etag: Option<&str>) -> String {
    let kind = if is_dir { "<D:collection/>" } else { "" };
    let etag = etag
        .map(|etag| format!("<D:getetag>{}</D:getetag>", etag.replace('"', "&quot;")))
        .unwrap_or_default();
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>{{{{request_path}}}}</D:href><D:propstat><D:prop><D:resourcetype>{kind}</D:resourcetype><D:getcontentlength>{size}</D:getcontentlength>{etag}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}
pub(crate) fn confirmed_put(etag: &str) -> String {
    format!(
        "HTTP/1.1 201 Created\r\nETag: {etag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}
