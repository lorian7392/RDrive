//! WebDAV over HTTP(S). Authentication is Basic; Digest and browser OAuth are
//! deliberately not inferred from a failed login. Redirects are disabled.
use crate::{
    backend::{normalize, Backend, Capabilities, Entry, WriteCondition},
    config::{validate_profile, Profile},
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use reqwest::{
    header::{
        HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
        CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, RANGE,
    },
    Client, Method, RequestBuilder, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Component, Path},
    sync::Mutex,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use url::Url;

const PROPERTIES: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:getetag/></d:prop></d:propfind>"#;
const MAX_XML_BYTES: usize = 16 * 1024 * 1024;
const MIN_NEXTCLOUD_CHUNK: u64 = 5 * 1024 * 1024;
const MAX_NEXTCLOUD_CHUNK: u64 = 4 * 1024 * 1024 * 1024;

pub struct WebDav {
    client: Client,
    root: Url,
    root_parts: Vec<String>,
    username: Option<String>,
    password: String,
    lock_token: Option<String>,
    owned_locks: Mutex<BTreeMap<String, String>>,
    nextcloud_upload_url: Option<Url>,
    transfer: crate::transfer::TransferContext,
}

impl WebDav {
    pub fn new(profile: &Profile, password: &str) -> Result<Self> {
        validate_profile(profile)?;
        let mut root = Url::parse(&profile.url)?;
        if !matches!(root.scheme(), "http" | "https") {
            bail!("WebDAV URL must use HTTP or HTTPS");
        }
        let root_parts = decode_path(root.path())?;
        if !root.path().ends_with('/') {
            root.path_segments_mut()
                .map_err(|_| anyhow!("URL cannot be a base"))?
                .push("");
        }
        let mut client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(profile.timeout_secs.min(30)))
            .timeout(Duration::from_secs(profile.timeout_secs))
            .user_agent(concat!("rdrive/", env!("CARGO_PKG_VERSION")));
        // hyper-util's macOS proxy discovery does not import ExceptionsList.
        // Local services must never receive credentials through a proxy; remote
        // hosts, including private-network addresses, retain configured routing.
        let loopback = match root.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
        if loopback {
            client = client.no_proxy();
        }
        let client = client
            .build()
            .context("unable to initialize HTTPS client")?;
        let nextcloud_upload_url = profile
            .nextcloud_upload_url
            .as_deref()
            .map(|value| -> Result<Url> {
                let mut url = Url::parse(value)?;
                decode_path(url.path())?;
                if !url.path().ends_with('/') {
                    url.path_segments_mut()
                        .map_err(|_| anyhow!("invalid Nextcloud upload endpoint"))?
                        .push("");
                }
                Ok(url)
            })
            .transpose()?;
        Ok(Self {
            client,
            root,
            root_parts,
            username: profile.username.clone(),
            password: password.to_owned(),
            lock_token: None,
            owned_locks: Mutex::new(BTreeMap::new()),
            nextcloud_upload_url,
            transfer: crate::transfer::global(),
        })
    }

    /// Supplies a previously acquired token. The caller should obtain tokens
    /// from an environment variable or private state, rather than shell history.
    pub fn with_lock_token(mut self, token: Option<String>) -> Result<Self> {
        if let Some(value) = &token {
            validate_token(value)?;
        }
        self.lock_token = token;
        Ok(self)
    }

    pub fn with_transfer_context(mut self, context: crate::transfer::TransferContext) -> Self {
        self.transfer = context;
        self
    }

    fn url(&self, path: &str) -> Result<Url> {
        let trailing_slash = path.ends_with('/');
        let path = normalize(path)?;
        let mut result = self.root.clone();
        if !path.is_empty() {
            result
                .path_segments_mut()
                .map_err(|_| anyhow!("invalid base URL"))?
                .pop_if_empty()
                .extend(path.split('/'));
            if trailing_slash {
                result
                    .path_segments_mut()
                    .map_err(|_| anyhow!("invalid base URL"))?
                    .push("");
            }
        }
        Ok(result)
    }

    fn request(&self, method: Method, path: &str, mutating: bool) -> Result<RequestBuilder> {
        let url = self.url(path)?;
        let path = normalize(path)?;
        let mut request = self.client.request(method, url);
        if let Some(username) = &self.username {
            request = request.basic_auth(username, Some(&self.password));
        }
        if mutating {
            let locks = self
                .owned_locks
                .lock()
                .map_err(|_| anyhow!("lock state unavailable"))?;
            let token = locks.get(&path).or(self.lock_token.as_ref());
            if let Some(token) = token {
                validate_token(token)?;
                request = request.header("If", format!("(<{token}>)"));
            }
        }
        Ok(request)
    }

    fn relative_href(&self, href: &str, requested: &str) -> Result<String> {
        // Validate before URL parsing, which otherwise normalizes dot segments.
        if href.contains(['?', '#']) {
            bail!("server returned a WebDAV href containing a query or fragment");
        }
        let href_path = if let Some((_, rest)) = href.split_once("://") {
            rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
        } else {
            href
        };
        validate_raw_path(href_path)?;
        let requested_url = self.url(requested)?;
        let resolved = requested_url
            .join(href)
            .context("server returned an invalid WebDAV href")?;
        if resolved.origin() != self.root.origin()
            || !resolved.username().is_empty()
            || resolved.password().is_some()
        {
            bail!("server returned a WebDAV href outside this connection");
        }
        let parts = decode_path(resolved.path())?;
        if !parts.starts_with(&self.root_parts) {
            bail!("server returned a WebDAV href outside the configured root");
        }
        normalize(&parts[self.root_parts.len()..].join("/"))
    }

    async fn propfind(&self, path: &str, depth: u8) -> Result<Option<Vec<Entry>>> {
        let path = normalize(path)?;
        let mut request_path = if depth == 1 && !path.is_empty() {
            format!("{path}/")
        } else {
            path.clone()
        };
        let mut response = send(
            self.request(method("PROPFIND"), &request_path, false)?
                .header("Depth", depth.to_string())
                .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(PROPERTIES),
        )
        .await?;
        // Some servers canonicalize collections by appending '/'. Only that
        // exact same-origin, same-resource redirect is safe to repeat with auth.
        if matches!(response.status().as_u16(), 301 | 308)
            && !request_path.ends_with('/')
            && !path.is_empty()
        {
            let next = format!("{path}/");
            let expected = self.url(&next)?;
            let current = self.url(&request_path)?;
            let location = response
                .headers()
                .get("Location")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| current.join(value).ok());
            if location.as_ref() == Some(&expected) {
                request_path = next;
                response = send(
                    self.request(method("PROPFIND"), &request_path, false)?
                        .header("Depth", depth.to_string())
                        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                        .body(PROPERTIES),
                )
                .await?;
            }
        }
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::MULTI_STATUS {
            return Err(status_error("PROPFIND", response.status()));
        }
        let xml = bounded_body(response).await?;
        let rows = parse_multistatus(&xml)?;
        let mut entries = BTreeMap::new();
        for row in rows {
            let relative = self.relative_href(&row.href, &request_path)?;
            if row.status.is_some_and(|s| s == 404) {
                if relative == path {
                    return Ok(None);
                }
                bail!(
                    "server returned a missing child during directory enumeration; retry the scan"
                );
            }
            if row.status.is_some_and(|s| !(200..300).contains(&s)) {
                bail!("server returned a failed WebDAV resource status");
            }
            let prop = row
                .props
                .ok_or_else(|| anyhow!("server returned no successful WebDAV properties"))?;
            let is_dir = prop.is_dir.ok_or_else(|| {
                anyhow!("server omitted resource type; refusing an ambiguous scan")
            })?;
            let size = if is_dir {
                0
            } else {
                if prop.empty_size {
                    bail!("server returned an empty file size; refusing an ambiguous scan");
                }
                prop.size.ok_or_else(|| {
                    anyhow!("server omitted file size; refusing an ambiguous scan")
                })?
            };
            let modified = prop
                .modified
                .as_deref()
                .and_then(|text| chrono::DateTime::parse_from_rfc2822(text).ok())
                .map(|time| time.timestamp_millis());
            let entry = Entry {
                path: relative.clone(),
                is_dir,
                size,
                modified,
                etag: prop.etag,
            };
            if entries.insert(relative, entry).is_some() {
                bail!("server returned duplicate WebDAV paths");
            }
        }
        if !entries.contains_key(&path) {
            bail!("server omitted the requested WebDAV resource");
        }
        if depth == 0 {
            if entries.len() != 1 {
                bail!("server ignored Depth: 0");
            }
        } else {
            for key in entries.keys().filter(|key| *key != &path) {
                let parent = key.rsplit_once('/').map(|x| x.0).unwrap_or("");
                if parent != path {
                    bail!("server returned a resource outside the requested directory");
                }
            }
        }
        Ok(Some(entries.into_values().collect()))
    }
}

fn method(name: &str) -> Method {
    Method::from_bytes(name.as_bytes()).expect("static HTTP method")
}
fn status_error(operation: &str, status: StatusCode) -> anyhow::Error {
    match status {
        StatusCode::PRECONDITION_FAILED => anyhow!(
            "WebDAV {operation}: revision conflict (HTTP 412); requested revision no longer matches"
        ),
        StatusCode::LOCKED => anyhow!(
            "WebDAV {operation}: resource is locked (HTTP 423); provide the acquired lock token"
        ),
        s if s.is_redirection() => anyhow!(
            "WebDAV {operation}: redirect rejected (HTTP {s}); configure the final server URL"
        ),
        _ => anyhow!("WebDAV {operation} failed (HTTP {status})"),
    }
}
async fn send(request: RequestBuilder) -> Result<Response> {
    request.send().await.map_err(|error| {
        if error.is_timeout() {
            anyhow!("WebDAV request timed out; remote commit status may be unknown")
        } else {
            anyhow!("WebDAV request failed; check connection, certificate and server availability")
        }
    })
}
async fn bounded_body(response: Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_XML_BYTES as u64)
    {
        bail!("WebDAV metadata response exceeds 16 MiB; narrow the remote root");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow!("WebDAV metadata response was interrupted"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_XML_BYTES {
            bail!("WebDAV metadata response exceeds 16 MiB; narrow the remote root");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[async_trait]
impl Backend for WebDav {
    fn capabilities(&self) -> Capabilities {
        Capabilities { protocol: "webdav", conditional_write: true, server_copy: true, locking: true, notes: vec![
            "Basic authentication over HTTPS; Digest, OAuth and arbitrary redirects are not supported.".into(),
            "Guarded replacement requires a strong server ETag; servers without one are rejected.".into(),
            "Checked GET uses If-Match when a strong ETag exists; otherwise callers compare before/after metadata (not an atomic snapshot).".into(),
            "LOCK/COPY/MOVE depend on server support. Mutating a locked resource requires its token.".into(),
            "Guarded DELETE requires a strong ETag and a file; general-purpose conditional MOVE and byte-range PUT are not implemented.".into(),
            if self.nextcloud_upload_url.is_some() { "Resumable uploads use explicitly configured Nextcloud chunks; confirmed chunks require unchanged strong validators for reuse. Sessions expire after24h inactivity.".into() } else { "Generic WebDAV uploads are whole PUTs. Resumable uploads require an explicit Nextcloud upload endpoint; resumable GET requires byte ranges and a strong ETag.".into() },
        ] }
    }

    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        Ok(self
            .propfind(path, 0)
            .await?
            .and_then(|mut entries| entries.pop()))
    }

    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let path = normalize(path)?;
        let mut entries = self
            .propfind(&path, 1)
            .await?
            .ok_or_else(|| anyhow!("remote directory does not exist"))?;
        let own = entries
            .iter()
            .find(|entry| entry.path == path)
            .expect("propfind validates self");
        if !own.is_dir {
            bail!("remote path is not a directory");
        }
        entries.retain(|entry| entry.path != path);
        Ok(entries)
    }

    async fn download(&self, remote: &str, local: &Path) -> Result<()> {
        self.download_inner(remote, local, None).await
    }

    async fn download_checked(&self, remote: &str, local: &Path, expected: &Entry) -> Result<()> {
        self.download_inner(remote, local, Some(expected)).await
    }

    async fn download_resume(
        &self,
        remote: &str,
        local: &Path,
        expected: &Entry,
        offset: u64,
    ) -> Result<()> {
        self.resume_download_inner(remote, local, expected, offset)
            .await
    }

    async fn upload(&self, local: &Path, remote: &str, condition: WriteCondition) -> Result<Entry> {
        let remote = normalize(remote)?;
        if remote.is_empty() {
            bail!("cannot replace the configured root");
        }
        reject_symlink_components(local)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(local)
            .context("unable to open upload source")?;
        let before = file.metadata()?;
        if !before.is_file() {
            bail!("upload source must be a regular file");
        }
        let mut request = self.request(Method::PUT, &remote, true)?;
        match condition {
            WriteCondition::Any => (),
            WriteCondition::CreateNew => request = request.header(IF_NONE_MATCH, "*"),
            WriteCondition::Match(previous) => {
                if previous.path != remote || previous.is_dir {
                    bail!("upload revision belongs to a different file");
                }
                let etag = previous.etag.as_deref().filter(|etag| strong_etag(etag)).ok_or_else(|| anyhow!("safe replacement requires a strong server ETag; use explicit overwrite only if intended"))?;
                request = request.header(
                    IF_MATCH,
                    HeaderValue::from_str(etag).context("invalid server ETag")?,
                );
            }
        }
        let metadata_handle = file.try_clone()?;
        let transfer = self
            .transfer
            .start("upload", &remote, Some(before.len()), 0)?;
        let stream = crate::transfer::meter_stream(
            ReaderStream::with_capacity(
                tokio::fs::File::from_std(file),
                self.transfer.chunk_size(64 * 1024),
            ),
            transfer.clone(),
        );
        let response = send(
            request
                .header(CONTENT_LENGTH, before.len())
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(reqwest::Body::wrap_stream(stream)),
        )
        .await?;
        if !matches!(response.status().as_u16(), 200 | 201 | 204) {
            return Err(status_error("PUT", response.status()));
        }
        let after = metadata_handle.metadata()?;
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            bail!("upload source changed during transfer; remote commit may contain mixed content");
        }
        let response_etag = upload_response_etag(&response);
        // A successful PUT is a confirmed HTTP commit. A later stat can already
        // describe another writer's content, so bind it to the response ETag.
        // Missing commit metadata must not turn success into a repeatable PUT.
        if response_etag.is_some() {
            if let Ok(Some(entry)) = self.stat(&remote).await {
                if entry.etag == response_etag && entry.size == before.len() && !entry.is_dir {
                    transfer.finish();
                    return Ok(entry);
                }
            }
        }
        transfer.finish();
        Ok(Entry {
            path: remote,
            is_dir: false,
            size: before.len(),
            modified: None,
            etag: response_etag,
        })
    }

    async fn upload_resume(
        &self,
        local: &Path,
        remote: &str,
        condition: WriteCondition,
        checkpoint: &Path,
    ) -> Result<Entry> {
        self.nextcloud_upload(local, remote, condition, checkpoint)
            .await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        if normalize(path)?.is_empty() {
            bail!("configured root already exists");
        }
        let response = send(self.request(method("MKCOL"), path, true)?).await?;
        if !matches!(response.status().as_u16(), 200 | 201 | 204) {
            return Err(status_error("MKCOL", response.status()));
        }
        Ok(())
    }

    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        let path = normalize(path)?;
        if path.is_empty() {
            bail!("cannot delete the configured root");
        }
        let entry = self
            .stat(&path)
            .await?
            .ok_or_else(|| anyhow!("remote path does not exist"))?;
        // DAV DELETE always recursively removes collections; there is no atomic
        // empty-only directory delete. Even an empty preflight scan can race.
        if entry.is_dir && !recursive {
            bail!("WebDAV directory deletion requires --recursive (the server DELETE operation is recursive)");
        }
        if !recursive {
            return self.remove_checked(&path, &entry).await;
        }
        let response = send(self.request(Method::DELETE, &path, true)?).await?;
        if !matches!(response.status().as_u16(), 200 | 204) {
            return Err(status_error("DELETE", response.status()));
        }
        Ok(())
    }

    async fn remove_checked(&self, path: &str, expected: &Entry) -> Result<()> {
        let path = normalize(path)?;
        if path.is_empty() || expected.path != path || expected.is_dir {
            bail!(
                "safe WebDAV deletion requires a non-root file revision; directory was preserved"
            );
        }
        let etag = expected
            .etag
            .as_deref()
            .filter(|etag| strong_etag(etag))
            .ok_or_else(|| {
                anyhow!("safe deletion requires a strong server ETag; remote file was preserved")
            })?;
        let response = send(self.request(Method::DELETE, &path, true)?.header(
            IF_MATCH,
            HeaderValue::from_str(etag).context("invalid server ETag")?,
        ))
        .await?;
        if !matches!(response.status().as_u16(), 200 | 204) {
            return Err(status_error("DELETE", response.status()));
        }
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        self.copy_or_move("MOVE", from, to, overwrite).await
    }

    async fn rename_checked(&self,from:&str,to:&str,expected:&Entry)->Result<Entry>{
        let from=normalize(from)?;let to=normalize(to)?;
        if from.is_empty()||to.is_empty()||from==to||to.starts_with(&format!("{from}/"))||expected.path!=from{bail!("invalid guarded WebDAV move");}
        let etag=expected.etag.as_deref().filter(|tag|strong_etag(tag)).context("unsupported: guarded WebDAV move requires a strong source ETag")?;
        let source=self.url(&from)?;
        // RFC4918 tagged If applies to the SOURCE resource, while Overwrite:F
        // makes destination creation atomic. If-Match alone can be evaluated
        // against the wrong resource by MOVE implementations.
        let mut condition=format!("<{}> ([{}]",source,etag);
        {let locks=self.owned_locks.lock().map_err(|_|anyhow!("lock state unavailable"))?;
        if let Some(token)=locks.get(&from).or(self.lock_token.as_ref()){condition.push_str(&format!(" <{token}>"));}}
        condition.push(')');
        let response=send(self.request(method("MOVE"),&from,false)?.header("Destination",self.url(&to)?.as_str()).header("Overwrite","F").header("Depth","infinity").header("If",HeaderValue::from_str(&condition)?)).await?;
        if !matches!(response.status().as_u16(),200|201|204){return Err(status_error("guarded MOVE",response.status()));}
        let actual=self.stat(&to).await?.context("version_conflict: moved WebDAV target disappeared")?;
        if !actual.same_revision(expected){bail!("version_conflict: target revision changed during guarded WebDAV move");}
        Ok(actual)
    }
    async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        self.copy_or_move("COPY", from, to, overwrite).await
    }

    async fn lock(&self, path: &str, seconds: u32) -> Result<String> {
        let path = normalize(path)?;
        if seconds == 0 {
            bail!("lock duration must be positive");
        }
        let body = r#"<?xml version="1.0"?><d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype><d:owner>rdrive</d:owner></d:lockinfo>"#;
        let response = send(
            self.request(method("LOCK"), &path, false)?
                .header("Timeout", format!("Second-{seconds}"))
                .header("Depth", "0")
                .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(body),
        )
        .await?;
        if !matches!(response.status().as_u16(), 200 | 201) {
            return Err(status_error("LOCK", response.status()));
        }
        let token = response
            .headers()
            .get("Lock-Token")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                anyhow!("server accepted LOCK but omitted Lock-Token; inspect server locks")
            })?;
        let token = token
            .strip_prefix('<')
            .and_then(|token| token.strip_suffix('>'))
            .ok_or_else(|| anyhow!("server supplied malformed Lock-Token"))?
            .to_owned();
        validate_token(&token)?;
        self.owned_locks
            .lock()
            .map_err(|_| anyhow!("lock state unavailable"))?
            .insert(path, token.clone());
        Ok(token)
    }

    async fn unlock(&self, path: &str, token: &str) -> Result<()> {
        let path = normalize(path)?;
        validate_token(token)?;
        let response = send(
            self.request(method("UNLOCK"), &path, false)?
                .header("Lock-Token", format!("<{token}>")),
        )
        .await?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(status_error("UNLOCK", response.status()));
        }
        self.owned_locks
            .lock()
            .map_err(|_| anyhow!("lock state unavailable"))?
            .remove(&path);
        Ok(())
    }
}

impl WebDav {
    fn authenticated_url_request(&self, method: Method, url: Url) -> Result<RequestBuilder> {
        if url.origin() != self.root.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("upload endpoint escaped the configured origin");
        }
        let mut request = self.client.request(method, url);
        if let Some(username) = &self.username {
            request = request.basic_auth(username, Some(&self.password));
        }
        Ok(request)
    }

    async fn nextcloud_stat(&self, url: &Url, endpoint: &Url) -> Result<Option<UploadMetadata>> {
        let response = send(
            self.authenticated_url_request(method("PROPFIND"), url.clone())?
                .header("Depth", "0")
                .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(PROPERTIES),
        )
        .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::MULTI_STATUS {
            return Err(status_error("Nextcloud PROPFIND", response.status()));
        }
        let rows = parse_multistatus(&bounded_body(response).await?)?;
        if rows.len() != 1 {
            bail!("upload endpoint returned an ambiguous Depth:0 response");
        }
        let row = rows.into_iter().next().unwrap();
        validate_href_at(&row.href, url, endpoint)?;
        if row.status == Some(404) {
            return Ok(None);
        }
        if row
            .status
            .is_some_and(|status| !(200..300).contains(&status))
        {
            bail!("upload endpoint returned a failed resource status");
        }
        let props = row
            .props
            .context("upload endpoint omitted successful properties")?;
        let is_dir = props
            .is_dir
            .context("upload endpoint omitted resource type")?;
        let size = if is_dir {
            0
        } else {
            props.size.context("upload endpoint omitted chunk length")?
        };
        Ok(Some(UploadMetadata {
            is_dir,
            size,
            etag: props.etag,
        }))
    }

    async fn verify_uploaded_destination(
        &self,
        remote: &str,
        sha256: &str,
        size: u64,
        checkpoint: &Path,
    ) -> Result<Option<Entry>> {
        let Some(before) = self.stat(remote).await? else {
            return Ok(None);
        };
        if before.is_dir || before.size != size {
            return Ok(None);
        }
        if !before.etag.as_deref().is_some_and(strong_etag) {
            bail!("completed Nextcloud upload has no strong ETag for final checksum verification");
        }
        let parent = checkpoint
            .parent()
            .context("upload checkpoint needs a private parent directory")?
            .canonicalize()?;
        let temp = parent.join(format!(".rdrive-upload-verify-{}", uuid::Uuid::new_v4()));
        let result = async {
            self.download_checked(remote, &temp, &before).await?;
            let checksum = crate::state::fingerprint(&temp)?;
            let after = self
                .stat(remote)
                .await?
                .context("uploaded destination disappeared during verification")?;
            if !after.same_revision(&before) {
                bail!("uploaded destination changed during checksum verification");
            }
            Ok((checksum == sha256 && std::fs::metadata(&temp)?.len() == size).then_some(after))
        }
        .await;
        let _ = std::fs::remove_file(temp);
        result
    }

    async fn nextcloud_upload(
        &self,
        local: &Path,
        remote: &str,
        condition: WriteCondition,
        checkpoint: &Path,
    ) -> Result<Entry> {
        let endpoint = self.nextcloud_upload_url.as_ref().context("resumable WebDAV upload requires an explicit Nextcloud upload endpoint; generic PUT was not attempted")?;
        let remote = normalize(remote)?;
        if remote.is_empty() {
            bail!("cannot upload over the remote root");
        }
        let guard = UploadGuard::from_condition(condition, &remote)?;
        reject_symlink_components(local)?;
        reject_symlink_components(checkpoint)?;
        let source_path = local
            .canonicalize()?
            .to_str()
            .context("upload source path must be UTF-8")?
            .to_owned();
        let source_size = std::fs::metadata(local)?.len();
        let source_sha256 = crate::state::fingerprint(local)?;
        let checkpoint_bytes = match std::fs::read(checkpoint) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let fresh = checkpoint_bytes.is_none();
        let mut state = match checkpoint_bytes {
            Some(bytes) => serde_json::from_slice::<NextcloudUpload>(&bytes)
                .context("invalid Nextcloud upload checkpoint; no upload was resumed")?,
            None => {
                let chunk_size = source_size.div_ceil(10_000).max(MIN_NEXTCLOUD_CHUNK);
                if chunk_size > MAX_NEXTCLOUD_CHUNK {
                    bail!("file exceeds the supported Nextcloud chunk count/size limits");
                }
                NextcloudUpload {
                    version: 1,
                    session: format!("rdrive-{}", uuid::Uuid::new_v4()),
                    root: self.root.to_string(),
                    endpoint: endpoint.to_string(),
                    username: self.username.clone(),
                    remote: remote.clone(),
                    source_path: source_path.clone(),
                    source_size,
                    source_sha256: source_sha256.clone(),
                    guard: guard.clone(),
                    chunk_size,
                    phase: UploadPhase::Preparing,
                    chunks: BTreeMap::new(),
                    committed: None,
                }
            }
        };
        state.validate()?;
        if state.root != self.root.as_str()
            || state.endpoint != endpoint.as_str()
            || state.username != self.username
            || state.remote != remote
            || state.source_path != source_path
            || state.source_size != source_size
            || state.source_sha256 != source_sha256
            || state.guard != guard
        {
            bail!("upload checkpoint belongs to a different source, account, destination or write condition; staging was preserved");
        }
        if fresh {
            crate::state::atomic_json(checkpoint, &state)?;
        }
        if state.phase == UploadPhase::Complete {
            return state
                .committed
                .context("completed upload checkpoint omitted its receipt");
        }
        if state.phase == UploadPhase::Publishing {
            let entry = self.verify_uploaded_destination(&remote, &source_sha256, source_size, checkpoint).await?
                .context("previous MOVE outcome is ambiguous and destination checksum does not match; no MOVE or overwrite was repeated")?;
            state.phase = UploadPhase::Complete;
            state.committed = Some(entry.clone());
            crate::state::atomic_json(checkpoint, &state)?;
            return Ok(entry);
        }
        let destination = self.url(&remote)?;
        let mut session = endpoint.clone();
        session
            .path_segments_mut()
            .map_err(|_| anyhow!("invalid upload endpoint"))?
            .pop_if_empty()
            .push(&state.session)
            .push("");
        if state.phase == UploadPhase::Preparing {
            let existing = if fresh {
                None
            } else {
                self.nextcloud_stat(&session, endpoint).await?
            };
            if let Some(meta) = existing {
                if !meta.is_dir {
                    bail!("upload session path is not a directory; staging was preserved");
                }
            } else {
                let response = send(
                    self.authenticated_url_request(method("MKCOL"), session.clone())?
                        .header("Destination", destination.as_str())
                        .header("OC-Total-Length", source_size),
                )
                .await?;
                if !matches!(response.status().as_u16(), 200 | 201 | 204) {
                    return Err(status_error("Nextcloud MKCOL", response.status()));
                }
            }
            state.phase = UploadPhase::Uploading;
            crate::state::atomic_json(checkpoint, &state)?;
        } else {
            let existing = self.nextcloud_stat(&session, endpoint).await?.context("Nextcloud upload session expired or disappeared; saved chunks cannot be resumed (sessions expire after24h inactivity)")?;
            if !existing.is_dir {
                bail!("upload session changed type; staging was preserved");
            }
        }
        let chunk_count = source_size.div_ceil(state.chunk_size).max(1);
        for number in 1..=chunk_count {
            let offset = (number - 1) * state.chunk_size;
            let size = source_size.saturating_sub(offset).min(state.chunk_size);
            let sha256 = file_segment_sha256(local, offset, size)?;
            let chunk_url = session.join(&format!("{number:05}"))?;
            if let Some(receipt) = state.chunks.get(&(number as u32)) {
                if receipt.size != size || receipt.sha256 != sha256 {
                    bail!("source chunk differs from its saved upload receipt; no chunk was overwritten");
                }
                if let Some(etag) = receipt.etag.as_deref().filter(|etag| strong_etag(etag)) {
                    if let Some(actual) = self.nextcloud_stat(&chunk_url, endpoint).await? {
                        if !actual.is_dir
                            && actual.size == size
                            && actual.etag.as_deref() == Some(etag)
                        {
                            continue;
                        }
                    }
                }
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options.open(local)?;
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset))?;
            let transfer = self.transfer.start(
                "upload_chunk",
                &format!("{remote} [{number}/{chunk_count}]"),
                Some(size),
                0,
            )?;
            let stream = crate::transfer::meter_stream(
                ReaderStream::with_capacity(
                    tokio::fs::File::from_std(file).take(size),
                    self.transfer.chunk_size(64 * 1024),
                ),
                transfer.clone(),
            );
            let response = send(
                self.authenticated_url_request(Method::PUT, chunk_url)?
                    .header("Destination", destination.as_str())
                    .header("OC-Total-Length", source_size)
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .header(CONTENT_LENGTH, size)
                    .body(reqwest::Body::wrap_stream(stream)),
            )
            .await?;
            if !matches!(response.status().as_u16(), 200 | 201 | 204) {
                return Err(status_error("Nextcloud chunk PUT", response.status()));
            }
            if file_segment_sha256(local, offset, size)? != sha256 {
                bail!("upload source changed while sending a chunk; destination was not published");
            }
            transfer.finish();
            let etag = upload_response_etag(&response).filter(|etag| strong_etag(etag));
            state
                .chunks
                .insert(number as u32, ChunkReceipt { size, sha256, etag });
            crate::state::atomic_json(checkpoint, &state)?;
        }
        if crate::state::fingerprint(local)? != source_sha256
            || std::fs::metadata(local)?.len() != source_size
        {
            bail!("source changed during chunk transfer; destination was not published");
        }
        let current = self.stat(&remote).await?;
        if current.as_ref().is_some_and(|entry| entry.is_dir) {
            bail!("upload destination is a directory; it was preserved");
        }
        // Even explicit overwrite snapshots the current destination revision for
        // the final MOVE, so a new collection cannot be removed by a type race.
        let final_guard = match &guard {
            UploadGuard::CreateNew => {
                if current.is_some() {
                    bail!("upload destination already exists; no MOVE was attempted");
                }
                UploadGuard::CreateNew
            }
            UploadGuard::Match { etag } => {
                if current.as_ref().and_then(|entry| entry.etag.as_deref()) != Some(etag.as_str()) {
                    bail!("upload destination revision changed; no MOVE was attempted");
                }
                guard.clone()
            }
            UploadGuard::Any => match current {
                None => UploadGuard::CreateNew,
                Some(entry) => UploadGuard::Match {
                    etag: entry
                        .etag
                        .filter(|etag| strong_etag(etag))
                        .context("safe Nextcloud overwrite requires a strong destination ETag")?,
                },
            },
        };
        let lock_token = {
            let locks = self
                .owned_locks
                .lock()
                .map_err(|_| anyhow!("lock state unavailable"))?;
            locks.get(&remote).or(self.lock_token.as_ref()).cloned()
        };
        let mut request = self
            .authenticated_url_request(method("MOVE"), session.join(".file")?)?
            .header("Destination", destination.as_str())
            .header("OC-Total-Length", source_size)
            .header(
                "Overwrite",
                if final_guard == UploadGuard::CreateNew {
                    "F"
                } else {
                    "T"
                },
            );
        let mut conditions = Vec::new();
        if let UploadGuard::Match { etag } = &final_guard {
            conditions.push(format!("[{etag}]"));
        }
        if let Some(token) = &lock_token {
            validate_token(token)?;
            conditions.push(format!("<{token}>"));
        }
        if !conditions.is_empty() {
            request = request.header(
                "If",
                format!("<{}> ({})", destination, conditions.join(" ")),
            );
        }
        state.phase = UploadPhase::Publishing;
        crate::state::atomic_json(checkpoint, &state)?;
        let response = send(request).await?;
        if !matches!(response.status().as_u16(), 200 | 201 | 204) {
            if matches!(
                response.status().as_u16(),
                400 | 401 | 403 | 404 | 405 | 409 | 412 | 423
            ) {
                state.phase = UploadPhase::Uploading;
                crate::state::atomic_json(checkpoint, &state)?;
            }
            return Err(status_error("Nextcloud final MOVE", response.status()));
        }
        let entry = self.verify_uploaded_destination(&remote, &source_sha256, source_size, checkpoint).await?.context("server assembled content with a different checksum; destination was not reported as synchronized")?;
        state.phase = UploadPhase::Complete;
        state.committed = Some(entry.clone());
        crate::state::atomic_json(checkpoint, &state)?;
        Ok(entry)
    }

    /// Continue a private partial file without ever truncating or replacing its
    /// existing prefix. The checkpoint owner must verify its saved prefix hash.
    async fn resume_download_inner(
        &self,
        remote: &str,
        local: &Path,
        expected: &Entry,
        offset: u64,
    ) -> Result<()> {
        let remote = normalize(remote)?;
        if remote.is_empty() || expected.path != remote || expected.is_dir {
            bail!("resume revision belongs to a different file");
        }
        let etag = expected.etag.as_deref().filter(|etag| strong_etag(etag)).ok_or_else(|| anyhow!("resumable WebDAV downloads require a strong ETag; existing partial file was preserved"))?;
        if offset > expected.size {
            bail!("partial download is larger than the remote file");
        }
        reject_symlink_components(local)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let file = if offset == 0 {
            match options.clone().create_new(true).open(local) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    options.open(local)?
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            options
                .open(local)
                .context("partial download file is missing")?
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != offset {
            bail!("partial download length differs from its checkpoint; file was preserved");
        }
        let mut file = tokio::fs::File::from_std(file);
        if offset == expected.size && expected.size != 0 {
            let current = self.stat(&remote).await?.ok_or_else(|| {
                anyhow!("remote file disappeared; completed partial file was preserved")
            })?;
            if current.etag.as_deref() != Some(etag)
                || current.size != expected.size
                || current.is_dir
            {
                bail!("remote revision changed; completed partial file was preserved");
            }
            file.sync_all().await?;
            return Ok(());
        }
        let mut request = self
            .request(Method::GET, &remote, false)?
            .header(
                IF_MATCH,
                HeaderValue::from_str(etag).context("invalid server ETag")?,
            )
            .header(ACCEPT_ENCODING, "identity");
        if expected.size != 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = send(request).await?;
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let range = response.headers().get(CONTENT_RANGE).and_then(|value| value.to_str().ok()).ok_or_else(|| anyhow!("206 response omitted a valid Content-Range; partial file was preserved"))?;
                if response.headers().get_all(CONTENT_RANGE).iter().count() != 1 { bail!("ambiguous Content-Range; partial file was preserved"); }
                validate_suffix_range(range, offset, expected.size)?;
            }
            StatusCode::OK if offset == 0 => {
                if response.headers().contains_key(CONTENT_RANGE) { bail!("200 response contains unexpected Content-Range; partial file was preserved"); }
            }
            StatusCode::OK => bail!("server ignored the requested byte range; partial file was preserved (restart this download explicitly)"),
            status => return Err(status_error("resumable GET", status)),
        }
        if let Some(actual) = response.headers().get(ETAG) {
            if response.headers().get_all(ETAG).iter().count() != 1
                || actual.to_str().ok() != Some(etag)
            {
                bail!("remote ETag changed or server ignored the resume revision guard; partial file was preserved");
            }
        }
        if response
            .headers()
            .get_all(CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            bail!("encoded HTTP responses cannot be combined with a byte-range checkpoint; partial file was preserved");
        }
        if response.headers().get(CONTENT_TYPE).is_some_and(|value| {
            value
                .to_str()
                .ok()
                .is_some_and(|value| value.to_ascii_lowercase().starts_with("multipart/"))
        }) {
            bail!("multipart range response is not a single suffix; partial file was preserved");
        }
        let remaining = expected.size - offset;
        if response
            .content_length()
            .is_some_and(|length| length != remaining)
        {
            bail!("range response length differs from the requested suffix; partial file was preserved");
        }
        if file.metadata().await?.len() != offset {
            bail!("partial file changed while requesting the next range");
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let progress =
            self.transfer
                .start("download_resume", &remote, Some(expected.size), offset)?;
        let transfer: Result<()> = async {
            let mut received = 0u64;
            let mut stream = Box::pin(crate::transfer::meter_stream(response.bytes_stream(), progress.clone()));
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| anyhow!("resumable WebDAV download was interrupted; completed bytes were retained"))?;
                if chunk.len() as u64 > remaining - received { bail!("server sent more bytes than the requested range; completed bytes were retained"); }
                file.write_all(&chunk).await?;
                received += chunk.len() as u64;
            }
            if received != remaining { bail!("range response ended early; completed bytes were retained"); }
            Ok(())
        }.await;
        // On both success and interruption, flush completed writes before the
        // caller updates its durable checkpoint. Never delete a partial here.
        let synced = file.sync_all().await;
        transfer?;
        synced?;
        if file.metadata().await?.len() != expected.size {
            bail!("partial file length changed during transfer");
        }
        progress.finish();
        Ok(())
    }

    async fn download_inner(
        &self,
        remote: &str,
        local: &Path,
        expected: Option<&Entry>,
    ) -> Result<()> {
        let remote = normalize(remote)?;
        reject_symlink_components(local)?;
        let mut request = self.request(Method::GET, &remote, false)?;
        let mut expected_etag = None;
        if let Some(expected) = expected {
            if expected.is_dir || expected.path != remote {
                bail!("download revision belongs to a different file");
            }
            if let Some(etag) = expected.etag.as_deref().filter(|etag| strong_etag(etag)) {
                request = request.header(
                    IF_MATCH,
                    HeaderValue::from_str(etag).context("invalid server ETag")?,
                );
                expected_etag = Some(etag);
            }
        }
        let response = send(request).await?;
        if response.status() != StatusCode::OK {
            return Err(status_error("GET", response.status()));
        }
        if let (Some(expected), Some(actual)) = (expected_etag, response.headers().get(ETAG)) {
            if actual.to_str().ok() != Some(expected) {
                bail!("server ignored the GET revision guard; download was not committed");
            }
        }
        if let (Some(expected), Some(length)) = (expected, response.content_length()) {
            if length != expected.size {
                bail!("GET response length differs from the protected file size");
            }
        }
        let progress = self.transfer.start(
            "download",
            &remote,
            expected
                .map(|entry| entry.size)
                .or(response.content_length()),
            0,
        )?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(local)
            .await
            .context("download destination must be a new regular file")?;
        let transfer: Result<()> = async {
            let mut received = 0u64;
            let mut stream = Box::pin(crate::transfer::meter_stream(
                response.bytes_stream(),
                progress.clone(),
            ));
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| anyhow!("WebDAV download was interrupted"))?;
                let next = received
                    .checked_add(chunk.len() as u64)
                    .context("WebDAV download length overflow")?;
                if expected.is_some_and(|entry| next > entry.size) {
                    bail!("server sent more bytes than the protected file size");
                }
                file.write_all(&chunk).await?;
                received = next;
            }
            if expected.is_some_and(|entry| received != entry.size) {
                bail!("GET response ended before the protected file size");
            }
            file.sync_all().await?;
            Ok(())
        }
        .await;
        drop(file);
        if transfer.is_err() {
            let _ = tokio::fs::remove_file(local).await;
        } else {
            progress.finish();
        }
        transfer
    }

    async fn copy_or_move(
        &self,
        operation: &str,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<()> {
        let from = normalize(from)?;
        let to = normalize(to)?;
        if from.is_empty() || to.is_empty() || from == to {
            bail!("source and destination must be distinct non-root paths");
        }
        if to.starts_with(&format!("{from}/")) {
            bail!("destination cannot be inside source");
        }
        let response = send(
            self.request(method(operation), &from, true)?
                .header("Destination", self.url(&to)?.as_str())
                .header("Overwrite", if overwrite { "T" } else { "F" })
                .header("Depth", "infinity"),
        )
        .await?;
        if !matches!(response.status().as_u16(), 200 | 201 | 204) {
            return Err(status_error(operation, response.status()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum UploadGuard {
    Any,
    CreateNew,
    Match { etag: String },
}
impl UploadGuard {
    fn from_condition(condition: WriteCondition, remote: &str) -> Result<Self> {
        Ok(match condition {
            WriteCondition::Any => Self::Any,
            WriteCondition::CreateNew => Self::CreateNew,
            WriteCondition::Match(entry) => {
                if entry.path != remote || entry.is_dir {
                    bail!("upload revision belongs to a different file");
                }
                Self::Match {
                    etag: entry
                        .etag
                        .filter(|etag| strong_etag(etag))
                        .context("safe Nextcloud replacement requires a strong destination ETag")?,
                }
            }
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UploadPhase {
    Preparing,
    Uploading,
    Publishing,
    Complete,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkReceipt {
    size: u64,
    sha256: String,
    etag: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct NextcloudUpload {
    version: u32,
    session: String,
    root: String,
    endpoint: String,
    username: Option<String>,
    remote: String,
    source_path: String,
    source_size: u64,
    source_sha256: String,
    guard: UploadGuard,
    chunk_size: u64,
    phase: UploadPhase,
    chunks: BTreeMap<u32, ChunkReceipt>,
    committed: Option<Entry>,
}
impl NextcloudUpload {
    fn validate(&self) -> Result<()> {
        let valid_hash =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        if self.version != 1
            || !self
                .session
                .strip_prefix("rdrive-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            || !valid_hash(&self.source_sha256)
            || !(MIN_NEXTCLOUD_CHUNK..=MAX_NEXTCLOUD_CHUNK).contains(&self.chunk_size)
            || self.source_size.div_ceil(self.chunk_size).max(1) > 10_000
        {
            bail!("invalid Nextcloud upload checkpoint identity or chunk layout");
        }
        if let UploadGuard::Match { etag } = &self.guard {
            if !strong_etag(etag) {
                bail!("invalid destination revision in upload checkpoint");
            }
        }
        let count = self.source_size.div_ceil(self.chunk_size).max(1);
        for (number, receipt) in &self.chunks {
            if *number == 0
                || u64::from(*number) > count
                || !valid_hash(&receipt.sha256)
                || receipt.size
                    != self
                        .source_size
                        .saturating_sub((u64::from(*number) - 1) * self.chunk_size)
                        .min(self.chunk_size)
                || receipt
                    .etag
                    .as_deref()
                    .is_some_and(|etag| !strong_etag(etag))
            {
                bail!("invalid confirmed chunk receipt in Nextcloud upload checkpoint");
            }
        }
        match (&self.phase, &self.committed) {
            (UploadPhase::Complete, Some(entry))
                if entry.path == self.remote && !entry.is_dir && entry.size == self.source_size =>
            {
                ()
            }
            (UploadPhase::Complete, _) => bail!("invalid completed upload receipt"),
            (_, Some(_)) => bail!("premature completion receipt in upload checkpoint"),
            _ => (),
        }
        Ok(())
    }
}
struct UploadMetadata {
    is_dir: bool,
    size: u64,
    etag: Option<String>,
}

fn upload_response_etag(response: &Response) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .or_else(|| response.headers().get("OC-Etag"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
fn file_segment_sha256(path: &Path, offset: u64, length: u64) -> Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let before = file.metadata()?;
    if !before.is_file()
        || offset
            .checked_add(length)
            .is_none_or(|end| end > before.len())
    {
        bail!("upload source is shorter than the expected chunk");
    }
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    while remaining != 0 {
        let capacity = (remaining.min(buffer.len() as u64)) as usize;
        let count = std::io::Read::read(&mut file, &mut buffer[..capacity])?;
        if count == 0 {
            bail!("upload source ended within a chunk");
        }
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        bail!("upload source changed while hashing a chunk");
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn validate_href_at(href: &str, requested: &Url, endpoint: &Url) -> Result<()> {
    if href.contains(['?', '#']) {
        bail!("upload endpoint returned an href with a query or fragment");
    }
    let raw_path = if let Some((_, rest)) = href.split_once("://") {
        rest.find('/').map(|index| &rest[index..]).unwrap_or("/")
    } else {
        href
    };
    validate_raw_path(raw_path)?;
    let resolved = requested
        .join(href)
        .context("upload endpoint returned invalid href")?;
    let parts = decode_path(resolved.path())?;
    if resolved.origin() != endpoint.origin()
        || !resolved.username().is_empty()
        || resolved.password().is_some()
        || !parts.starts_with(&decode_path(endpoint.path())?)
        || parts != decode_path(requested.path())?
    {
        bail!("upload endpoint returned href outside the requested upload resource");
    }
    Ok(())
}

fn validate_suffix_range(value: &str, offset: u64, total: u64) -> Result<()> {
    let valid = (|| {
        let value = value.strip_prefix("bytes ")?;
        let (range, length) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        if ![start, end, length]
            .iter()
            .all(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return None;
        }
        let start: u64 = start.parse().ok()?;
        let end: u64 = end.parse().ok()?;
        let length: u64 = length.parse().ok()?;
        (start == offset && length == total && start <= end && end.checked_add(1) == Some(total))
            .then_some(())
    })();
    if valid.is_none() {
        bail!("server returned an invalid Content-Range for the requested suffix; partial file was preserved");
    }
    Ok(())
}

fn strong_etag(etag: &str) -> bool {
    let bytes = etag.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && !bytes[1..bytes.len() - 1]
            .iter()
            .any(|byte| *byte == b'"' || *byte < 0x21 || *byte == 0x7f)
}
fn validate_token(token: &str) -> Result<()> {
    if token.is_empty()
        || token.contains(['<', '>', '\r', '\n', ' ', '\t', '(', ')'])
        || Url::parse(token).is_err()
    {
        bail!("invalid WebDAV lock token");
    }
    HeaderValue::from_str(token).context("invalid WebDAV lock token")?;
    Ok(())
}
fn reject_symlink_components(path: &Path) -> Result<()> {
    let mut prefix = std::path::PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            bail!("local transfer path must not contain parent traversal");
        }
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("local transfer path contains a symbolic link")
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn decode_segment(segment: &str) -> Result<String> {
    let bytes = segment.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("malformed percent escape in WebDAV path");
            }
            let high = (bytes[index + 1] as char)
                .to_digit(16)
                .ok_or_else(|| anyhow!("malformed percent escape in WebDAV path"))?;
            let low = (bytes[index + 2] as char)
                .to_digit(16)
                .ok_or_else(|| anyhow!("malformed percent escape in WebDAV path"))?;
            output.push((high * 16 + low) as u8);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    let value = String::from_utf8(output).context("WebDAV paths must be UTF-8")?;
    if matches!(value.as_str(), "." | "..") || value.contains(['/', '\\', '\0']) {
        bail!("unsafe encoded path returned by WebDAV server");
    }
    Ok(value)
}
fn validate_raw_path(path: &str) -> Result<()> {
    for segment in path.split('/') {
        decode_segment(segment)?;
    }
    Ok(())
}
fn decode_path(path: &str) -> Result<Vec<String>> {
    if path.contains("//") {
        bail!("ambiguous repeated separator in WebDAV path");
    }
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(decode_segment)
        .collect()
}

#[derive(Default)]
struct Properties {
    is_dir: Option<bool>,
    size: Option<u64>,
    empty_size: bool,
    modified: Option<String>,
    etag: Option<String>,
}
#[derive(Default)]
struct Row {
    href: String,
    status: Option<u16>,
    props: Option<Properties>,
}
fn parse_status(text: &str) -> Result<u16> {
    let mut parts = text.split_whitespace();
    if !parts.next().is_some_and(|part| part.starts_with("HTTP/")) {
        bail!("malformed status in WebDAV XML");
    }
    parts
        .next()
        .ok_or_else(|| anyhow!("missing WebDAV resource status"))?
        .parse()
        .context("malformed WebDAV resource status")
}
fn is_path(stack: &[Vec<u8>], expected: &[&[u8]]) -> bool {
    stack.len() == expected.len()
        && stack
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_slice() == *expected)
}
fn dav_name(namespace: ResolveResult<'_>, local: &[u8]) -> Vec<u8> {
    if matches!(namespace, ResolveResult::Bound(value) if value.as_ref() == b"DAV:") {
        local.to_vec()
    } else {
        let mut name = vec![b'!'];
        name.extend_from_slice(local);
        name
    }
}
fn parse_multistatus(xml: &[u8]) -> Result<Vec<Row>> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut rows = Vec::new();
    let mut current = None::<Row>;
    let mut props = None::<Properties>;
    let mut prop_status = None;
    let mut text = String::new();
    let mut found_root = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event()
            .context("malformed WebDAV XML")?;
        match event {
            Event::Start(event) => {
                let name = dav_name(namespace, event.local_name().as_ref());
                if stack.is_empty() {
                    if name != b"multistatus" || found_root {
                        bail!("WebDAV response is not a single DAV multistatus XML document");
                    }
                    found_root = true;
                }
                match name.as_slice() {
                    b"response" if is_path(&stack, &[b"multistatus"]) => {
                        if current.is_some() {
                            bail!("nested WebDAV response");
                        }
                        current = Some(Row::default());
                    }
                    b"propstat" if is_path(&stack, &[b"multistatus", b"response"]) => {
                        props = Some(Properties::default());
                        prop_status = None;
                    }
                    b"resourcetype"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().is_dir = Some(false)
                    }
                    b"collection"
                        if is_path(
                            &stack,
                            &[
                                b"multistatus",
                                b"response",
                                b"propstat",
                                b"prop",
                                b"resourcetype",
                            ],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().is_dir = Some(true)
                    }
                    _ => (),
                }
                stack.push(name);
                text.clear();
                if stack.len() > 64 {
                    bail!("WebDAV XML nesting exceeds limit");
                }
            }
            Event::Empty(event) => {
                let name = dav_name(namespace, event.local_name().as_ref());
                if stack.is_empty() {
                    if name != b"multistatus" || found_root {
                        bail!("WebDAV response is not a single DAV multistatus XML document");
                    }
                    found_root = true;
                }
                match name.as_slice() {
                    b"getcontentlength"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().empty_size = true
                    }
                    b"resourcetype"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().is_dir = Some(false)
                    }
                    b"collection"
                        if is_path(
                            &stack,
                            &[
                                b"multistatus",
                                b"response",
                                b"propstat",
                                b"prop",
                                b"resourcetype",
                            ],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().is_dir = Some(true)
                    }
                    _ => (),
                }
                text.clear();
            }
            Event::Text(event) => {
                let decoded = event
                    .xml_content()
                    .context("invalid WebDAV text encoding")?;
                if stack.is_empty() && !decoded.trim().is_empty() {
                    bail!("text outside WebDAV XML root");
                }
                text.push_str(&decoded);
            }
            Event::CData(event) => text.push_str(
                &event
                    .xml_content()
                    .context("invalid WebDAV text encoding")?,
            ),
            Event::GeneralRef(event) => {
                let escaped = format!("&{};", event.decode()?);
                text.push_str(
                    &quick_xml::escape::unescape(&escaped)
                        .context("unknown entity in WebDAV XML")?,
                );
            }
            Event::End(event) => {
                let name = dav_name(namespace, event.local_name().as_ref());
                if stack.pop().as_deref() != Some(name.as_slice()) {
                    bail!("malformed WebDAV XML nesting");
                }
                let value = text.trim();
                match name.as_slice() {
                    b"href"
                        if is_path(&stack, &[b"multistatus", b"response"]) && current.is_some() =>
                    {
                        current.as_mut().unwrap().href = value.to_owned()
                    }
                    b"status"
                        if is_path(&stack, &[b"multistatus", b"response", b"propstat"])
                            && props.is_some() =>
                    {
                        prop_status = Some(parse_status(value)?)
                    }
                    b"status"
                        if is_path(&stack, &[b"multistatus", b"response"]) && current.is_some() =>
                    {
                        current.as_mut().unwrap().status = Some(parse_status(value)?)
                    }
                    b"getcontentlength"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        let props = props.as_mut().unwrap();
                        if value.is_empty() {
                            // Collections may return an empty length. Resource
                            // type can arrive later or in another propstat, so
                            // retain that fact until the full row is validated.
                            props.empty_size = true;
                        } else {
                            props.size =
                                Some(value.parse().context("invalid WebDAV content length")?);
                        }
                    }
                    b"getlastmodified"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().modified = Some(value.to_owned())
                    }
                    b"getetag"
                        if is_path(
                            &stack,
                            &[b"multistatus", b"response", b"propstat", b"prop"],
                        ) && props.is_some() =>
                    {
                        props.as_mut().unwrap().etag = (!value.is_empty()).then(|| value.to_owned())
                    }
                    b"propstat" if is_path(&stack, &[b"multistatus", b"response"]) => {
                        let parsed = props
                            .take()
                            .ok_or_else(|| anyhow!("malformed WebDAV propstat"))?;
                        let status = prop_status
                            .take()
                            .ok_or_else(|| anyhow!("WebDAV propstat omitted status"))?;
                        if (200..300).contains(&status) {
                            let row = current
                                .as_mut()
                                .ok_or_else(|| anyhow!("WebDAV properties outside response"))?;
                            let combined = row.props.get_or_insert_with(Properties::default);
                            if parsed.is_dir.is_some() {
                                combined.is_dir = parsed.is_dir;
                            }
                            if parsed.size.is_some() {
                                combined.size = parsed.size;
                            }
                            combined.empty_size |= parsed.empty_size;
                            if parsed.modified.is_some() {
                                combined.modified = parsed.modified;
                            }
                            if parsed.etag.is_some() {
                                combined.etag = parsed.etag;
                            }
                        }
                    }
                    b"response" if is_path(&stack, &[b"multistatus"]) => {
                        let row = current
                            .take()
                            .ok_or_else(|| anyhow!("invalid WebDAV response nesting"))?;
                        if row.href.is_empty() {
                            bail!("WebDAV response omitted href");
                        }
                        rows.push(row);
                    }
                    _ => (),
                }
                text.clear();
            }
            Event::DocType(_) => bail!("WebDAV metadata must not contain a document type"),
            Event::Eof => break,
            _ => (),
        }
    }
    if !found_root || !stack.is_empty() || current.is_some() {
        bail!("truncated WebDAV XML");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> Profile {
        Profile {
            protocol: crate::config::Protocol::Webdav,
            url: "http://localhost/dav/root/".into(),
            username: None,
            password_env: None,
            allow_http: true,
            timeout_secs: 5,
            smb_encryption: true,
            nextcloud_upload_url: None,
        }
    }

    #[test]
    fn href_containment_is_checked_before_url_normalization() {
        let client = WebDav::new(&profile(), "").unwrap();
        for href in [
            "/dav/root/../escape",
            "/dav/root/%2e%2e/escape",
            "/dav/root/a%2fb",
            "/dav/root/a%5cb",
            "https://evil.example/dav/root/file",
            "/dav/rooted/file",
            "/dav/root/a?secret=x",
        ] {
            assert!(client.relative_href(href, "").is_err(), "{href}");
        }
        assert_eq!(
            client
                .relative_href("/dav/root/%E4%B8%AD%E6%96%87%20a%26b.txt", "")
                .unwrap(),
            "中文 a&b.txt"
        );
        assert_eq!(client.url("a%2fb").unwrap().path(), "/dav/root/a%252fb");
    }

    #[test]
    fn parse_entities_and_successful_properties_only() {
        let xml = br#"<x:multistatus xmlns:x="DAV:"><x:response><x:href>/dav/root/a&amp;b</x:href><x:propstat><x:prop><x:resourcetype/><x:getcontentlength>3</x:getcontentlength><x:getetag>&quot;abc&quot;</x:getetag></x:prop><x:status>HTTP/1.1 200 OK</x:status></x:propstat><x:propstat><x:prop><x:getetag>bad</x:getetag></x:prop><x:status>HTTP/1.1 404 Not Found</x:status></x:propstat></x:response></x:multistatus>"#;
        let mut rows = parse_multistatus(xml).unwrap();
        let row = rows.pop().unwrap();
        assert_eq!(row.href, "/dav/root/a&b");
        let props = row.props.unwrap();
        assert_eq!(props.etag.as_deref(), Some("\"abc\""));
        assert_eq!(props.is_dir, Some(false));
        assert_eq!(props.size, Some(3));
        assert!(parse_multistatus(b"<multistatus><response>").is_err());
        assert!(parse_multistatus(b"<!DOCTYPE x><multistatus/>").is_err());
        assert!(parse_multistatus(br#"<multistatus xmlns="urn:unrelated"/>"#).is_err());
        let nested = br#"<multistatus xmlns="DAV:"><response><href>/dav/root/file</href><propstat><prop><resourcetype/><getcontentlength>1</getcontentlength><unknown><href>/dav/root/wrong</href></unknown></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        assert_eq!(parse_multistatus(nested).unwrap()[0].href, "/dav/root/file");
    }

    #[test]
    fn conditional_etags_cannot_weaken_the_write_guard() {
        assert!(strong_etag("\"abc\""));
        for value in ["*", "W/\"abc\"", "\"a\", \"b\"", "\"bad\r\nvalue\""] {
            assert!(!strong_etag(value));
        }
        assert!(validate_token("opaquelocktoken:a-b").is_ok());
        assert!(validate_token("x\r\nInjected: yes").is_err());
    }
}
