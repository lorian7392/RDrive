//! Private, home-bound profile administration. The home lock is acquired before
//! a provider replaces its IPC socket, so a second IPC root cannot steal a home.
use super::{apply, Command};
use crate::state::Store;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::UnixStream, time::Instant};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTOR_BYTES: u64 = 4096;
const DESCRIPTOR: &str = "provider-admin.json";
const LOCK: &str = "provider-admin.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    home: PathBuf,
    socket: PathBuf,
    instance_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    v: u32,
    op: String,
    home: PathBuf,
    instance_id: String,
    command: Command,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReply {
    ok: bool,
    home: PathBuf,
    instance_id: String,
    request_id: String,
    result: Option<Value>,
    error: Option<String>,
}

fn uid() -> u32 { unsafe { libc::geteuid() } }

fn private_directory(path: &Path, owner: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o700 {
        bail!("profile administration directories must be real, owned and private (mode 0700)");
    }
    Ok(())
}

fn home_directory(path: &Path) -> Result<PathBuf> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    private_directory(path, uid())?;
    Ok(fs::canonicalize(path)?)
}

fn owned_file(metadata: &fs::Metadata, owner: u32) -> Result<()> {
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != owner || metadata.mode() & 0o777 != 0o600 {
        bail!("profile administration files must be owned regular files with one link and mode 0600");
    }
    Ok(())
}

fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> Result<()> {
    if before.dev() != after.dev() || before.ino() != after.ino() {
        bail!("profile administration file changed while being inspected");
    }
    Ok(())
}

fn read_private(path: &Path, owner: u32) -> Result<Option<Vec<u8>>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    owned_file(&before, owner)?;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    let opened = file.metadata()?;
    owned_file(&opened, owner)?;
    same_file(&before, &opened)?;
    let mut bytes = Vec::new();
    file.take(MAX_DESCRIPTOR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES { bail!("profile administration descriptor exceeds 4096 bytes"); }
    let after = fs::symlink_metadata(path)?;
    owned_file(&after, owner)?;
    same_file(&opened, &after)?;
    Ok(Some(bytes))
}

fn locked(file: &File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    lock_result(if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) })
}

fn lock_result(result: std::io::Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn try_registration(home: PathBuf) -> Result<Option<Registration>> {
    let path = home.join(LOCK);
    let file = OpenOptions::new().create(true).read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path)?;
    let metadata = file.metadata()?;
    owned_file(&metadata, uid())?;
    same_file(&metadata, &fs::symlink_metadata(path)?)?;
    Ok(locked(&file)?.then(|| Registration { home, instance_id: uuid::Uuid::new_v4().to_string(), _lock: file }))
}

/// Keep this guard alive until all accepted connections have finished.
pub struct Registration {
    home: PathBuf,
    instance_id: String,
    _lock: File,
}

impl Registration {
    pub fn acquire(home: &Path) -> Result<Self> {
        try_registration(home_directory(home)?)?.context("another provider is already administering this state directory")
    }

    /// Publish only after binding and securing the Unix socket, before workers start.
    pub fn publish(&self, socket: &Path) -> Result<()> {
        let socket = socket_location(socket, uid())?.context("profile administration socket is missing")?;
        let descriptor = Descriptor { version: 1, home: self.home.clone(), socket, instance_id: self.instance_id.clone() };
        publish_descriptor(&self.home, &descriptor)
    }

    pub fn home(&self) -> &Path { &self.home }
    pub fn instance_id(&self) -> &str { &self.instance_id }

    pub fn parse(&self, bytes: &[u8]) -> Result<Command> {
        if bytes.len() > MAX_REQUEST_BYTES { bail!("profile administration request exceeds 64 KiB"); }
        let request: WireRequest = serde_json::from_slice(bytes).context("invalid profile administration request")?;
        if request.v != 1 || request.op != "profile_admin" || request.home != self.home || request.instance_id != self.instance_id {
            bail!("profile administration request does not match this provider's home and instance");
        }
        request.command.validate()?;
        Ok(request.command)
    }

    pub fn reply(&self, request_id: &str, result: &Result<Value>) -> Value {
        let mut reply = json!({"ok":result.is_ok(),"home":self.home,"instance_id":self.instance_id,"request_id":request_id});
        match result {
            Ok(value) => { reply["result"] = value.clone(); }
            Err(error) => { reply["error"] = Value::String(format!("{error:#}")); }
        }
        reply
    }
}

fn publish_descriptor(home: &Path, descriptor: &Descriptor) -> Result<()> {
    read_private(&home.join(DESCRIPTOR), uid())?;
    let bytes = serde_json::to_vec(descriptor)?;
    if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES { bail!("profile administration descriptor exceeds 4096 bytes"); }
    crate::state::atomic_write(&home.join(DESCRIPTOR), &bytes)
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(Some(bytes)) = read_private(&self.home.join(DESCRIPTOR), uid()) {
            if let Ok(descriptor) = serde_json::from_slice::<Descriptor>(&bytes) {
                if descriptor.home == self.home && descriptor.instance_id == self.instance_id {
                    let _ = fs::remove_file(self.home.join(DESCRIPTOR));
                }
            }
        }
    }
}

fn socket_parts(socket: &Path) -> Result<(&Path, &std::ffi::OsStr)> {
    if !socket.is_absolute() { bail!("profile administration socket must be an absolute Unix path"); }
    Ok((socket.parent().context("profile administration socket needs a parent")?, socket.file_name().context("profile administration socket needs a filename")?))
}

fn socket_location(socket: &Path, owner: u32) -> Result<Option<PathBuf>> {
    let (parent, filename) = socket_parts(socket)?;
    private_directory(parent, owner)?;
    let canonical = parent.canonicalize()?.join(filename);
    if socket != canonical { bail!("profile administration socket must use its canonical parent"); }
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o600 {
        bail!("profile administration endpoint must be a private owned Unix socket (mode 0600)");
    }
    Ok(Some(canonical))
}

fn parse_descriptor(home: &Path) -> Result<Option<Descriptor>> {
    let Some(bytes) = read_private(&home.join(DESCRIPTOR), uid())? else { return Ok(None); };
    let descriptor: Descriptor = serde_json::from_slice(&bytes).context("invalid profile administration descriptor")?;
    if descriptor.version != 1 || descriptor.home != home || uuid::Uuid::parse_str(&descriptor.instance_id).is_err() {
        bail!("profile administration descriptor does not match this home or supported format");
    }
    if uuid::Uuid::parse_str(&descriptor.instance_id)?.to_string() != descriptor.instance_id {
        bail!("profile administration instance identifier is not canonical");
    }
    socket_parts(&descriptor.socket)?;
    Ok(Some(descriptor))
}

fn descriptor(home: &Path) -> Result<Option<Descriptor>> {
    let Some(descriptor) = parse_descriptor(home)? else { return Ok(None); };
    socket_location(&descriptor.socket, uid())?;
    Ok(Some(descriptor))
}

fn peer(peer: u32, owner: u32) -> Result<()> {
    if peer != owner { bail!("profile administration server belongs to another user"); }
    Ok(())
}

fn reply(bytes: &[u8], endpoint: &Descriptor, request_id: &str) -> Result<Value> {
    let response: WireReply = serde_json::from_slice(bytes).context("invalid profile administration reply")?;
    if response.home != endpoint.home || response.instance_id != endpoint.instance_id || response.request_id != request_id {
        bail!("profile administration reply does not match the requested home, instance and request ID");
    }
    match (response.ok, response.result, response.error) {
        (true, Some(value), None) => Ok(value),
        (false, None, Some(error)) => Err(anyhow::anyhow!(error)),
        _ => bail!("profile administration reply has an invalid result"),
    }
}

async fn exchange(endpoint: &Descriptor, command: &Command) -> Result<Option<Value>> {
    let stream = match UnixStream::connect(&endpoint.socket).await {
        Ok(stream) => stream,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    peer(stream.peer_cred()?.uid(), uid())?;
    let request = json!({"v":1,"op":"profile_admin","home":endpoint.home,"instance_id":endpoint.instance_id,"command":command});
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES { bail!("profile administration request exceeds 64 KiB"); }
    let (read, mut write) = stream.into_split();
    if write.write_all(&bytes).await.is_err() { return Ok(None); }
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(read).take(MAX_REQUEST_BYTES as u64 + 1);
    if reader.read_until(b'\n', &mut bytes).await.is_err() || bytes.is_empty() { return Ok(None); }
    if bytes.len() > MAX_REQUEST_BYTES || bytes.last() != Some(&b'\n') { bail!("invalid or oversized profile administration reply"); }
    Ok(Some(reply(&bytes, endpoint, &command.request_id)?))
}

/// A lost reply is retried with this exact command, allowing the durable receipt
/// to settle uncertainty even if the provider exits between attempts.
pub async fn dispatch(home: &Path, command: &Command, wait: Duration) -> Result<Value> {
    dispatch_inner(home, command, wait).await.with_context(|| format!("profile request {} failed; use --request-id {} when retrying this operation", command.request_id, command.request_id))
}

async fn dispatch_inner(home: &Path, command: &Command, wait: Duration) -> Result<Value> {
    command.validate()?;
    let home = home_directory(home)?;
    let deadline = Instant::now().checked_add(wait).context("profile administration timeout is too large")?;
    loop {
        if let Some(_registration) = try_registration(home.clone())? {
            // A crashed provider's endpoint may be gone, but a retained descriptor
            // must still have a valid identity and private file before local use.
            parse_descriptor(&home)?;
            loop {
                match Store::open(home.clone()) {
                    Ok(mut store) => return apply(&mut store, command),
                    Err(error) if error.to_string().contains("another rdrive process is using this state directory") => (),
                    Err(error) => return Err(error),
                }
                pause(deadline).await.context("state directory is busy with another local operation")?;
            }
        }
        if let Some(endpoint) = descriptor(&home)? {
            match tokio::time::timeout_at(deadline, exchange(&endpoint, command)).await {
                Ok(Ok(Some(value))) => return Ok(value),
                Ok(Err(error)) => return Err(error),
                _ => (),
            }
        }
        pause(deadline).await.context("profile administration is busy or its reply is unconfirmed")?;
    }
}

async fn pause(deadline: Instant) -> Result<()> {
    let remaining = deadline.checked_duration_since(Instant::now()).context("profile administration timed out")?;
    tokio::time::sleep(remaining.min(Duration::from_millis(20))).await;
    if Instant::now() >= deadline { bail!("profile administration timed out"); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::{Profile, Protocol}, profile_admin::Change};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tokio::net::UnixListener;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::Builder::new().prefix("rda-").tempdir_in("/private/tmp").unwrap();
        let home = home_directory(&temp.path().join("home")).unwrap();
        let ipc = home_directory(&temp.path().join("ipc")).unwrap();
        (temp, home, ipc.join("bridge.sock"))
    }
    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn command(name: &str) -> Command {
        Command::new(Change::Add { name: name.into(), profile: Profile { protocol: Protocol::Webdav, url: "https://example.invalid/dav".into(), username: Some("account".into()), password_env: Some("UNREAD_PROFILE_SECRET".into()), keychain_internet: false, allow_http: false, timeout_secs: 120, smb_encryption: true, nextcloud_upload_url: None } }, None).unwrap()
    }
    fn request(registration: &Registration, command: &Command) -> Value {
        json!({"v":1,"op":"profile_admin","home":registration.home(),"instance_id":registration.instance_id(),"command":command})
    }
    fn endpoint(registration: &Registration, socket: &Path) -> Descriptor {
        Descriptor { version: 1, home: registration.home.clone(), socket: socket.into(), instance_id: registration.instance_id.clone() }
    }
    async fn listen(home: &Path, socket: &Path) -> (Registration, UnixListener) {
        let registration = Registration::acquire(home).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).unwrap();
        registration.publish(socket).unwrap();
        (registration, listener)
    }
    async fn receive(listener: &UnixListener, registration: &Registration) -> (tokio::net::unix::OwnedWriteHalf, Command) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, write) = stream.into_split();
        let mut bytes = Vec::new();
        BufReader::new(read).read_until(b'\n', &mut bytes).await.unwrap();
        (write, registration.parse(&bytes).unwrap())
    }
    async fn respond(mut write: tokio::net::unix::OwnedWriteHalf, value: &Value) {
        write.write_all(format!("{value}\n").as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn registration_serializes_all_ipc_roots_and_only_removes_its_own_descriptor() {
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        assert_eq!(registration.home(), home);
        assert!(Registration::acquire(&home).err().unwrap().to_string().contains("already administering"));
        assert_eq!(descriptor(&home).unwrap().unwrap().socket, socket);
        let mut foreign = endpoint(&registration, &socket);
        foreign.instance_id = uuid::Uuid::new_v4().to_string();
        publish_descriptor(&home, &foreign).unwrap();
        drop(registration);
        assert!(home.join(DESCRIPTOR).exists());
        let registration = Registration::acquire(&home).unwrap();
        registration.publish(&socket).unwrap();
        drop(registration);
        assert!(!home.join(DESCRIPTOR).exists());
        drop(listener);
        let registration = Registration::acquire(&home).unwrap();
        write(&home.join(DESCRIPTOR), b"broken");
        drop(registration);
        assert_eq!(fs::read(home.join(DESCRIPTOR)).unwrap(), b"broken");
    }

    #[tokio::test]
    async fn wire_requests_and_replies_are_strictly_bound_and_bounded() {
        let (_temp, home, socket) = fixture();
        let (registration, _listener) = listen(&home, &socket).await;
        let command = command("bound");
        let wire = request(&registration, &command);
        assert_eq!(registration.parse(&serde_json::to_vec(&wire).unwrap()).unwrap().request_id, command.request_id);
        for (field, value) in [("v", json!(2)), ("op", json!("enumerate")), ("home", json!("/other")), ("instance_id", json!(uuid::Uuid::new_v4().to_string())), ("unexpected", json!(true))] {
            let mut invalid = wire.clone(); invalid[field] = value;
            assert!(registration.parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let mut invalid = wire.clone(); invalid["command"]["request_id"] = json!("bad");
        assert!(registration.parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        assert!(registration.parse(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let endpoint = endpoint(&registration, &socket);
        let good = registration.reply(&command.request_id, &Ok(json!({"saved":true})));
        assert_eq!(reply(&serde_json::to_vec(&good).unwrap(), &endpoint, &command.request_id).unwrap(), json!({"saved":true}));
        let error = registration.reply(&command.request_id, &Err(anyhow::anyhow!("busy operation")));
        assert!(reply(&serde_json::to_vec(&error).unwrap(), &endpoint, &command.request_id).unwrap_err().to_string().contains("busy operation"));
        for (field, value) in [("home", json!("/other")), ("instance_id", json!("other")), ("request_id", json!("other")), ("error", json!("contradiction")), ("unexpected", json!(true))] {
            let mut invalid = good.clone(); invalid[field] = value;
            assert!(reply(&serde_json::to_vec(&invalid).unwrap(), &endpoint, &command.request_id).is_err());
        }
        let mut missing = good; missing.as_object_mut().unwrap().remove("result");
        assert!(reply(&serde_json::to_vec(&missing).unwrap(), &endpoint, &command.request_id).is_err());
    }

    #[test]
    fn private_discovery_rejects_foreign_public_linked_nonregular_and_oversized_files() {
        let (temp, home, _socket) = fixture();
        let path = home.join(DESCRIPTOR);
        assert!(read_private(&path, uid()).unwrap().is_none());
        write(&path, b"{}");
        assert!(read_private(&path, uid().wrapping_add(1)).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path, uid()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = home.join("linked"); fs::hard_link(&path, &link).unwrap();
        assert!(read_private(&path, uid()).is_err()); fs::remove_file(link).unwrap();
        let other = home.join("other"); write(&other, b"{}");
        assert!(same_file(&fs::metadata(&path).unwrap(), &fs::metadata(&other).unwrap()).is_err());
        fs::remove_file(&path).unwrap(); symlink(&other, &path).unwrap();
        assert!(read_private(&path, uid()).is_err()); fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap(); assert!(read_private(&path, uid()).is_err()); fs::remove_dir(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_private(&path, uid()).is_err()); fs::remove_file(&path).unwrap();
        write(&path, &vec![b' '; MAX_DESCRIPTOR_BYTES as usize + 1]);
        assert!(read_private(&path, uid()).unwrap_err().to_string().contains("4096"));
        assert!(read_private(&other.join("child"), uid()).is_err());
        assert!(private_directory(&home, uid().wrapping_add(1)).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Registration::acquire(&home).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = temp.path().join("alias"); symlink(&home, &alias).unwrap();
        assert!(Registration::acquire(&alias).is_err());
        assert!(Registration::acquire(&other).is_err());
        fs::remove_file(home.join(LOCK)).ok(); symlink(&other, home.join(LOCK)).unwrap();
        assert!(Registration::acquire(&home).is_err());
        fs::remove_file(home.join(LOCK)).unwrap(); write(&home.join(LOCK), b"");
        fs::set_permissions(home.join(LOCK), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Registration::acquire(&home).is_err());
        assert!(!lock_result(Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))).unwrap());
        assert!(lock_result(Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))).is_err());
        assert!(peer(uid().wrapping_add(1), uid()).is_err());
    }

    #[tokio::test]
    async fn corrupt_and_stale_descriptors_never_select_another_home_or_tcp() {
        let (_temp, home, socket) = fixture();
        let (registration, _listener) = listen(&home, &socket).await;
        let valid = serde_json::to_value(endpoint(&registration, &socket)).unwrap();
        for (field, value) in [("version", json!(2)), ("home", json!("/other")), ("instance_id", json!("bad")), ("instance_id", json!(registration.instance_id().to_uppercase())), ("socket", json!("tcp://127.0.0.1:80")), ("extra", json!(1))] {
            let mut invalid = valid.clone(); invalid[field] = value;
            write(&home.join(DESCRIPTOR), &serde_json::to_vec(&invalid).unwrap());
            assert!(descriptor(&home).is_err());
        }
        write(&home.join(DESCRIPTOR), b"{"); assert!(descriptor(&home).is_err());
        let mut huge = endpoint(&registration, &socket); huge.socket = PathBuf::from("x".repeat(4096));
        assert!(publish_descriptor(&home, &huge).is_err());
        registration.publish(&socket).unwrap(); fs::remove_file(&socket).unwrap();
        assert!(descriptor(&home).unwrap().is_some());
        assert!(registration.publish(&socket).is_err());
    }

    #[tokio::test]
    async fn socket_discovery_requires_private_owned_unix_paths() {
        let (temp, home, socket) = fixture();
        let registration = Registration::acquire(&home).unwrap();
        assert!(socket_location(Path::new("relative"), uid()).is_err());
        assert!(socket_location(Path::new("/"), uid()).is_err());
        write(&socket, b"not socket"); assert!(registration.publish(&socket).is_err()); fs::remove_file(&socket).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(socket_location(&socket, uid().wrapping_add(1)).is_err());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(registration.publish(&socket).is_err());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = temp.path().join("alias"); symlink(socket.parent().unwrap(), &alias).unwrap();
        assert!(socket_location(&alias.join("bridge.sock"), uid()).is_err());
        assert!(socket_location(&socket.parent().unwrap().join("./bridge.sock"), uid()).is_ok());
        let target = socket.parent().unwrap().join("bad"); write(&target, b"file");
        assert!(socket_location(&target.join("child"), uid()).is_err());
        assert!(socket_location(&socket.parent().unwrap().join("n".repeat(300)), uid()).is_err());
        drop(listener);
        fs::remove_file(&socket).unwrap(); symlink(&target, &socket).unwrap();
        assert!(registration.publish(&socket).is_err());
    }

    #[tokio::test]
    async fn absent_broker_and_duplicate_local_clients_commit_one_receipt_without_credentials() {
        let (_temp, home, _socket) = fixture();
        let command = command("local");
        let (first, second) = tokio::join!(dispatch(&home, &command, Duration::from_secs(1)), dispatch(&home, &command, Duration::from_secs(1)));
        assert_eq!(first.unwrap(), second.unwrap());
        let store = Store::open(home.clone()).unwrap();
        assert_eq!(store.config.profiles.len(), 1);
        assert_eq!(store.config.profile_admin_receipts.len(), 1);
        assert_eq!(store.config.profiles["local"].password_env.as_deref(), Some("UNREAD_PROFILE_SECRET"));
    }

    #[tokio::test]
    async fn local_fallback_rejects_invalid_descriptors_before_opening_the_store() {
        let (_temp, home, socket) = fixture();
        let valid = json!({"version":1,"home":home,"socket":socket,"instance_id":"01234567-89ab-4cde-8012-3456789abcde"});
        let command = command("rejected_local");
        for (field, value) in [("home", json!("/foreign")), ("version", json!(2)), ("instance_id", json!("invalid")), ("instance_id", json!("01234567-89AB-4CDE-8012-3456789ABCDE")), ("socket", json!("tcp://127.0.0.1:80")), ("socket", json!("/")), ("extra", json!(true))] {
            let mut invalid = valid.clone(); invalid[field] = value;
            let bytes = serde_json::to_vec(&invalid).unwrap();
            write(&home.join(DESCRIPTOR), &bytes);
            assert!(dispatch(&home, &command, Duration::from_secs(1)).await.is_err(), "offline dispatch accepted invalid {field}");
            assert_eq!(fs::read(home.join(DESCRIPTOR)).unwrap(), bytes);
            assert!(!home.join("config.json").exists());
            assert!(!home.join("state.json").exists());
            assert!(!home.join("transactions").exists());
        }
        for bytes in [b"{".to_vec(), vec![b' '; MAX_DESCRIPTOR_BYTES as usize + 1]] {
            write(&home.join(DESCRIPTOR), &bytes);
            assert!(dispatch(&home, &command, Duration::from_secs(1)).await.is_err());
            assert_eq!(fs::read(home.join(DESCRIPTOR)).unwrap(), bytes);
        }
        let bytes = serde_json::to_vec(&valid).unwrap();
        write(&home.join(DESCRIPTOR), &bytes);
        fs::set_permissions(home.join(DESCRIPTOR), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(dispatch(&home, &command, Duration::from_secs(1)).await.is_err());
        fs::set_permissions(home.join(DESCRIPTOR), fs::Permissions::from_mode(0o600)).unwrap();
        let hard = home.join("descriptor-hard-link"); fs::hard_link(home.join(DESCRIPTOR), &hard).unwrap();
        assert!(dispatch(&home, &command, Duration::from_secs(1)).await.is_err());
        fs::remove_file(&hard).unwrap(); fs::remove_file(home.join(DESCRIPTOR)).unwrap();
        write(&hard, &bytes); symlink(&hard, home.join(DESCRIPTOR)).unwrap();
        assert!(dispatch(&home, &command, Duration::from_secs(1)).await.is_err());
        assert_eq!(fs::read(&hard).unwrap(), bytes);
        assert!(!home.join("config.json").exists());
        assert!(!home.join("transactions").exists());
    }

    #[tokio::test]
    async fn local_fallback_accepts_a_valid_stale_descriptor_after_its_ipc_directory_is_gone() {
        let (_temp, home, socket) = fixture();
        let descriptor = Descriptor { version: 1, home: home.clone(), socket: socket.clone(), instance_id: uuid::Uuid::new_v4().to_string() };
        publish_descriptor(&home, &descriptor).unwrap();
        fs::remove_dir(socket.parent().unwrap()).unwrap();
        assert_eq!(dispatch(&home, &command("crashed_broker"), Duration::from_secs(1)).await.unwrap()["profile"], "crashed_broker");
        assert_eq!(Store::open(home).unwrap().config.profile_admin_receipts.len(), 1);
        assert!(!socket.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn external_store_lock_is_bounded_and_distinct_from_a_running_broker() {
        let (_temp, home, _socket) = fixture();
        let store = Store::open(home.clone()).unwrap();
        let command = command("busy");
        let error = format!("{:#}", dispatch(&home, &command, Duration::from_millis(2)).await.unwrap_err());
        assert!(error.contains("another local operation")); assert!(error.contains(&command.request_id));
        drop(store);
        let registration = Registration::acquire(&home).unwrap();
        assert!(dispatch(&home, &command, Duration::from_millis(2)).await.unwrap_err().root_cause().to_string().contains("timed out"));
        drop(registration);
        write(&home.join("config.json"), b"broken");
        assert!(format!("{:#}", dispatch(&home, &command, Duration::ZERO).await.unwrap_err()).contains("invalid JSON"));
        let mut invalid = command.clone(); invalid.request_id = "invalid".into();
        assert!(dispatch(&home, &invalid, Duration::ZERO).await.is_err());
        assert!(dispatch(&home, &command, Duration::MAX).await.is_err());
        assert!(pause(Instant::now() - Duration::from_secs(1)).await.is_err());
        pause(Instant::now() + Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn lost_reply_retries_the_identical_command_and_recovers_the_receipt() {
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        let server = tokio::spawn(async move {
            let (write, first) = receive(&listener, &registration).await;
            let result = apply(&mut Store::open(registration.home.clone()).unwrap(), &first).unwrap();
            drop(write);
            let (write, second) = receive(&listener, &registration).await;
            assert_eq!(first.request_id, second.request_id);
            let replay = apply(&mut Store::open(registration.home.clone()).unwrap(), &second).unwrap();
            assert_eq!(result, replay);
            respond(write, &registration.reply(&second.request_id, &Ok(replay))).await;
        });
        let result = dispatch(&home, &command("lost"), Duration::from_secs(2)).await.unwrap();
        assert_eq!(result["profile"], "lost"); server.await.unwrap();
        assert_eq!(Store::open(home).unwrap().config.profile_admin_receipts.len(), 1);
    }

    #[tokio::test]
    async fn provider_exit_after_commit_can_fall_back_only_after_the_home_lock_releases() {
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        let server = tokio::spawn(async move {
            let (write, command) = receive(&listener, &registration).await;
            apply(&mut Store::open(registration.home.clone()).unwrap(), &command).unwrap();
            drop(listener); drop(registration); drop(write);
        });
        assert_eq!(dispatch(&home, &command("exit"), Duration::from_secs(2)).await.unwrap()["profile"], "exit");
        server.await.unwrap();
        assert_eq!(Store::open(home).unwrap().config.profile_admin_receipts.len(), 1);
    }

    #[tokio::test]
    async fn stale_socket_waits_for_guard_and_hung_reply_reports_recoverable_request_id() {
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        drop(listener);
        let command = command("stale");
        let error = format!("{:#}", dispatch(&home, &command, Duration::from_millis(2)).await.unwrap_err());
        assert!(error.contains(&command.request_id)); assert!(!home.join("config.json").exists());
        drop(registration);
        assert_eq!(dispatch(&home, &command, Duration::from_secs(1)).await.unwrap()["profile"], "stale");
        fs::remove_file(socket).unwrap();
    }

    #[tokio::test]
    async fn malformed_oversized_and_error_replies_fail_with_bound_identity() {
        for bytes in [b"{\n".to_vec(), b"truncated".to_vec(), vec![b' '; MAX_REQUEST_BYTES + 1]] {
            let (_temp, home, socket) = fixture();
            let (registration, listener) = listen(&home, &socket).await;
            let server = tokio::spawn(async move {
                let (mut write, _) = receive(&listener, &registration).await;
                let _ = write.write_all(&bytes).await;
            });
            assert!(dispatch(&home, &command("malformed"), Duration::from_secs(1)).await.is_err());
            server.await.unwrap();
        }
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        let server = tokio::spawn(async move {
            let (write, command) = receive(&listener, &registration).await;
            respond(write, &registration.reply(&command.request_id, &Err(anyhow::anyhow!("profile is still referenced")))).await;
        });
        assert!(format!("{:#}", dispatch(&home, &command("rejected"), Duration::from_secs(1)).await.unwrap_err()).contains("still referenced"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_endpoint_and_oversized_outbound_command_cannot_bypass_the_broker() {
        let (_temp, home, socket) = fixture();
        let (registration, listener) = listen(&home, &socket).await;
        let mut endpoint = endpoint(&registration, &socket);
        endpoint.socket = socket.with_file_name("missing");
        assert!(exchange(&endpoint, &command("missing")).await.unwrap().is_none());
        endpoint.socket = PathBuf::from("/".to_owned() + &"x".repeat(300));
        assert!(exchange(&endpoint, &command("invalid_socket")).await.is_err());
        let long = command(&"n".repeat(MAX_REQUEST_BYTES));
        assert!(exchange(&super::tests::endpoint(&registration, &socket), &long).await.unwrap_err().to_string().contains("64 KiB"));
        drop(listener);
    }
}

/// Use the same private, home/instance-bound transport for explicit history
/// decisions. Lost replies never fall back to a second remote mutation.
pub async fn dispatch_history(home:&Path,command:&crate::history_control::Command,wait:Duration)->Result<Value>{
    if let Some(value)=crate::history_control::read(home,command)? {return Ok(value);}
    let home=home_directory(home)?;let deadline=Instant::now().checked_add(wait).context("history timeout is too large")?;
    loop {
        if let Some(_registration)=try_registration(home.clone())? {
            parse_descriptor(&home)?;
            loop {match crate::history_control::execute(&home,command).await {
                Err(e) if e.to_string().contains("another rdrive process is using this state directory")=>pause(deadline).await?,
                result=>return result,
            }}
        }
        if let Some(endpoint)=descriptor(&home)? {
            let result=tokio::time::timeout_at(deadline,async {
                let stream=UnixStream::connect(&endpoint.socket).await?;peer(stream.peer_cred()?.uid(),uid())?;
                let request_id=uuid::Uuid::new_v4().to_string();
                let request=json!({"v":1,"op":"history_cli","home":endpoint.home,"instance_id":endpoint.instance_id,"request_id":request_id,"command":command});
                let mut request=serde_json::to_vec(&request)?;request.push(b'\n');if request.len()>MAX_REQUEST_BYTES{bail!("history request exceeds 64 KiB");}
                let (read,mut write)=stream.into_split();write.write_all(&request).await?;
                let mut bytes=Vec::new();let mut reader=BufReader::new(read).take(8*1024*1024+1);reader.read_until(b'\n',&mut bytes).await?;
                if bytes.len()>8*1024*1024||bytes.last()!=Some(&b'\n'){bail!("invalid or oversized history reply; inspect the saved decision before retrying");}
                reply(&bytes,&endpoint,&request_id)
            }).await.context("history request timed out; its saved decision is retained")?;
            return result;
        }
        pause(deadline).await.context("history administration is waiting for the background service")?;
    }
}
impl Registration {
    pub fn parse_history(&self,bytes:&[u8])->Result<(String,crate::history_control::Command)>{
        #[derive(Deserialize)] #[serde(deny_unknown_fields)]
        struct Request {v:u32,op:String,home:PathBuf,instance_id:String,request_id:String,command:crate::history_control::Command}
        if bytes.len()>MAX_REQUEST_BYTES {bail!("history request exceeds 64 KiB");}
        let req:Request=serde_json::from_slice(bytes)?;
        if req.v!=1||req.op!="history_cli"||req.home!=self.home||req.instance_id!=self.instance_id||uuid::Uuid::parse_str(&req.request_id)?.to_string()!=req.request_id{bail!("history request does not match this home and service instance");}
        Ok((req.request_id,req.command))
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::history_control::Command as History;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    fn fixture()->(tempfile::TempDir,PathBuf,PathBuf){let temp=tempfile::Builder::new().prefix("rdh-").tempdir_in("/private/tmp").unwrap();let home=home_directory(&temp.path().join("home")).unwrap();let ipc=home_directory(&temp.path().join("ipc")).unwrap();(temp,home,ipc.join("bridge.sock"))}
    fn command()->History{History::Clear{profile:"missing".into(),apply:false}}
    #[test]
    fn history_envelope_binds_home_instance_id_and_request_identity(){
        let (_temp,home,_socket)=fixture();let r=Registration::acquire(&home).unwrap();let value=json!({"v":1,"op":"history_cli","home":r.home(),"instance_id":r.instance_id(),"request_id":uuid::Uuid::new_v4().to_string(),"command":command()});assert!(r.parse_history(&serde_json::to_vec(&value).unwrap()).is_ok());
        for (field,wrong) in [("v",json!(2)),("op",json!("other")),("home",json!("/tmp/other")),("instance_id",json!("other")),("request_id",json!("invalid"))]{let mut bad=value.clone();bad[field]=wrong;assert!(r.parse_history(&serde_json::to_vec(&bad).unwrap()).is_err());}
        assert!(r.parse_history(&vec![b' ';MAX_REQUEST_BYTES+1]).is_err());
    }
    #[tokio::test]
    async fn history_dispatch_uses_validated_live_endpoint_and_never_replays_lost_replies(){
        let (_temp,home,socket)=fixture();let store=Store::open(home.clone()).unwrap();store.save_config().unwrap();drop(store);
        let registration=Registration::acquire(&home).unwrap();let listener=UnixListener::bind(&socket).unwrap();fs::set_permissions(&socket,fs::Permissions::from_mode(0o600)).unwrap();registration.publish(&socket).unwrap();
        let server=tokio::spawn(async move{for mode in 0..3 {let (mut stream,_)=listener.accept().await.unwrap();let mut bytes=Vec::new();{let mut reader=BufReader::new(&mut stream);reader.read_until(b'\n',&mut bytes).await.unwrap();}let (id,_)=registration.parse_history(&bytes).unwrap();match mode{0=>{let value=registration.reply(&id,&Ok(json!({"served":true})));stream.write_all(format!("{value}\n").as_bytes()).await.unwrap();},1=>{stream.write_all(b"invalid\n").await.unwrap();},_=>{stream.write_all(b"unfinished").await.unwrap();}}}});
        assert_eq!(dispatch_history(&home,&command(),Duration::from_secs(1)).await.unwrap()["served"],true);
        assert!(dispatch_history(&home,&command(),Duration::from_secs(1)).await.is_err());
        assert!(dispatch_history(&home,&command(),Duration::from_secs(1)).await.unwrap_err().to_string().contains("reply"));server.await.unwrap();
        // A local failure reports the actual command, not a phantom remote success.
        assert!(dispatch_history(&home,&command(),Duration::from_secs(1)).await.unwrap_err().to_string().contains("profile"));
        // An endpoint being republished is bounded by the original wait budget.
        let _registration=Registration::acquire(&home).unwrap();assert!(dispatch_history(&home,&command(),Duration::from_millis(5)).await.is_err());
    }
    #[tokio::test]
    async fn read_queries_do_not_need_an_administration_or_store_lease(){
        let (_temp,home,_socket)=fixture();let mut store=Store::open(home.clone()).unwrap();store.config.profiles.insert("p".into(),crate::config::Profile{protocol:crate::config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:None,keychain_internet:false,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();let _registration=Registration::acquire(&home).unwrap();let value=dispatch_history(&home,&History::Status{profile:"p".into()},Duration::from_millis(1)).await.unwrap();assert_eq!(value["entries"],0);
    }
    fn profile(home:&Path){let mut store=Store::open(home.to_path_buf()).unwrap();store.config.profiles.insert("p".into(),crate::config::Profile{protocol:crate::config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:Some("UNREAD_HISTORY_TRANSPORT_TEST".into()),keychain_internet:false,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();}
    #[tokio::test]
    async fn local_history_dispatch_waits_for_store_and_never_turns_timeout_into_success(){
        let (_temp,home,_socket)=fixture();profile(&home);let store=Store::open(home.clone()).unwrap();let waiting=home.clone();let task=tokio::spawn(async move{dispatch_history(&waiting,&History::Clear{profile:"p".into(),apply:false},Duration::from_secs(2)).await});tokio::time::sleep(Duration::from_millis(45)).await;assert!(!task.is_finished());drop(store);assert_eq!(task.await.unwrap().unwrap()["apply"],false);
        let _store=Store::open(home.clone()).unwrap();assert!(dispatch_history(&home,&History::Clear{profile:"p".into(),apply:false},Duration::from_millis(5)).await.unwrap_err().to_string().contains("timed out"));assert!(dispatch_history(&home,&command(),Duration::MAX).await.unwrap_err().to_string().contains("too large"));
    }
    #[tokio::test]
    async fn live_history_outbound_size_and_reply_deadlines_remain_bounded(){
        let (_temp,home,socket)=fixture();profile(&home);let registration=Registration::acquire(&home).unwrap();let listener=UnixListener::bind(&socket).unwrap();fs::set_permissions(&socket,fs::Permissions::from_mode(0o600)).unwrap();registration.publish(&socket).unwrap();
        let huge=History::Clear{profile:"x".repeat(MAX_REQUEST_BYTES),apply:false};assert!(dispatch_history(&home,&huge,Duration::from_secs(1)).await.unwrap_err().to_string().contains("64 KiB"));let (first,_)=listener.accept().await.unwrap();drop(first);
        let server=tokio::spawn(async move{let (mut stream,_)=listener.accept().await.unwrap();let mut bytes=vec![];BufReader::new(&mut stream).read_until(b'\n',&mut bytes).await.unwrap();tokio::time::sleep(Duration::from_millis(100)).await;drop(stream);drop(registration);});assert!(dispatch_history(&home,&command(),Duration::from_millis(20)).await.unwrap_err().to_string().contains("saved decision is retained"));server.await.unwrap();
    }
    #[tokio::test]
    async fn oversized_history_reply_and_mismatched_receipt_are_not_replayed_locally(){
        let (temp,home,socket)=fixture();profile(&home);let store=Store::open(home.clone()).unwrap();let source=temp.path().join("retained");fs::write(&source,b"preserved history").unwrap();crate::history::capture_import(&store,"p","file",&source,"retained").unwrap();drop(store);let registration=Registration::acquire(&home).unwrap();let listener=UnixListener::bind(&socket).unwrap();fs::set_permissions(&socket,fs::Permissions::from_mode(0o600)).unwrap();registration.publish(&socket).unwrap();
        let server=tokio::spawn(async move{for mode in 0..2{let (mut stream,_)=listener.accept().await.unwrap();let mut bytes=vec![];BufReader::new(&mut stream).read_until(b'\n',&mut bytes).await.unwrap();let (id,_)=registration.parse_history(&bytes).unwrap();if mode==0{let _=stream.write_all(&vec![b'x';8*1024*1024+1]).await;}else{let mut value=registration.reply(&id,&Ok(json!({"wrong":true})));value["request_id"]=json!(uuid::Uuid::new_v4().to_string());stream.write_all(format!("{value}\n").as_bytes()).await.unwrap();}}});
        assert!(dispatch_history(&home,&History::Clear{profile:"p".into(),apply:true},Duration::from_secs(2)).await.unwrap_err().to_string().contains("oversized"));assert!(dispatch_history(&home,&command(),Duration::from_secs(1)).await.unwrap_err().to_string().contains("does not match"));server.await.unwrap();let config=crate::client_read::config(&home).unwrap();assert_eq!(crate::history::list(&home,&config,"p",None,None).unwrap().len(),1);
    }
    #[test]
    fn history_wire_rejects_unknown_fields_noncanonical_uuid_and_bad_json(){
        let (_temp,home,_socket)=fixture();let r=Registration::acquire(&home).unwrap();let mut value=json!({"v":1,"op":"history_cli","home":r.home(),"instance_id":r.instance_id(),"request_id":"154108C8-1CBB-4FAE-81F4-C6C53161F2AE","command":command()});assert!(r.parse_history(&serde_json::to_vec(&value).unwrap()).unwrap_err().to_string().contains("does not match"));value["request_id"]=json!(uuid::Uuid::new_v4().to_string());value["extra"]=json!(true);assert!(r.parse_history(&serde_json::to_vec(&value).unwrap()).is_err());assert!(r.parse_history(b"not json").is_err());
    }

}
