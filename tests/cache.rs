use rdrive::{
    cache,
    config::{Profile, Protocol},
    state::Store,
    webdav::WebDav,
};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn metadata(path: &str, size: u64) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/root/{path}</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength><D:getetag>&quot;{path}-revision&quot;</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}
fn response(status: &str, body: &str, etag: Option<&str>) -> String {
    let tag = etag
        .map(|tag| format!("ETag: {tag}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{tag}Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn real_http_cache_rejects_oversize_before_get_without_discarding_old_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let replies = vec![
        response("207 Multi-Status", &metadata("a", 3), None),
        response("200 OK", "abc", Some("\"a-revision\"")),
        response("207 Multi-Status", &metadata("a", 3), None),
        response("207 Multi-Status", &metadata("b", 4), None),
    ];
    let server = tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let boundary = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break position + 4;
                }
                assert!(request.len() < 64 * 1024);
            };
            let head = String::from_utf8_lossy(&request[..boundary]);
            let length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while request.len() < boundary + length {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            captured.lock().unwrap().push(
                String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap()
                    .to_string(),
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().join("state")).unwrap();
    let profile = Profile {
        protocol: Protocol::Webdav,
        url: format!("http://{address}/root/"),
        username: None,
        password_env: None,
        allow_http: true,
        timeout_secs: 5,
        smb_encryption: true,
        keychain_internet: false,
        nextcloud_upload_url: None,
    };
    let backend = WebDav::new(&profile, "").unwrap();
    store.config.profiles.insert("test".into(), profile);
    store.save_config().unwrap();
    cache::set_limit(&mut store, Some(3)).unwrap();
    let original = cache::fetch(&mut store, &backend, "test", "a", false)
        .await
        .unwrap();
    let error = cache::fetch(&mut store, &backend, "test", "b", false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("capacity"));
    assert_eq!(cache::status(&store).unwrap().used_bytes, 3);
    assert!(store.db.cache_reservations.is_empty());
    assert_eq!(
        std::fs::read(store.home.join("cache").join(original.object)).unwrap(),
        b"abc"
    );
    server.await.unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        [
            "PROPFIND /root/a HTTP/1.1",
            "GET /root/a HTTP/1.1",
            "PROPFIND /root/a HTTP/1.1",
            "PROPFIND /root/b HTTP/1.1"
        ]
    );
}
