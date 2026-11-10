//! macOS native network volumes. These mounts do not provide offline synchronization.
//!
//! Passwords are never put in process arguments. SMB uses Apple's credential flow;
//! WebDAV can additionally receive credentials through an unlinked, seekable fd.
use crate::config::{validate_profile, Profile, Protocol};
use anyhow::{bail, Context, Result};
use serde::Serialize;
#[cfg(unix)]
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawFd,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Default, Serialize)]
pub struct MountOptions {
    pub read_only: bool,
    pub no_browse: bool,
    pub no_prompt: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountPlan {
    pub program: String,
    pub args: Vec<String>,
    pub mountpoint: PathBuf,
    pub source: String,
    pub filesystem: String,
    pub credential_mode: String,
    pub read_only: bool,
    pub encryption_required: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MountInfo {
    pub mountpoint: PathBuf,
    /// Sanitized kernel mount-from name; any password is removed.
    pub source: String,
    pub filesystem: String,
    pub read_only: bool,
    pub no_browse: bool,
    pub owner: u32,
    pub fsid: [i32; 2],
}

#[derive(Clone)]
struct KernelMount {
    info: MountInfo,
    source: String,
}

trait System {
    fn mounts(&self) -> Result<Vec<KernelMount>>;
    fn run(
        &self,
        plan: &MountPlan,
        credentials: Option<&CredentialFile>,
        remove_env: Option<&str>,
    ) -> Result<bool>;
    fn detach(&self, path: &Path, force: bool) -> Result<bool>;
}

struct Native {
    command_status: fn(&mut Command) -> Result<bool>,
}
impl Default for Native {
    fn default() -> Self {
        Self {
            command_status: helper_status,
        }
    }
}
fn helper_status(command: &mut Command) -> Result<bool> {
    Ok(command.status()?.success())
}

/// Inspect the actual macOS mount table, including volumes mounted outside rdrive.
pub fn list() -> Result<Vec<MountInfo>> {
    Ok(network_mounts(Native::default().mounts()?))
}
fn network_mounts(mounts: Vec<KernelMount>) -> Vec<MountInfo> {
    mounts
        .into_iter()
        .filter(|m| is_network(&m.info))
        .map(|m| m.info)
        .collect()
}

/// A dry run validates the existing mountpoint and builds only credential-free arguments.
pub fn plan(profile: &Profile, mountpoint: &Path, options: &MountOptions) -> Result<MountPlan> {
    ensure_macos()?;
    plan_with(&Native::default(), profile, mountpoint, options)
}

/// Mount a volume and confirm its owner, filesystem, root, and requested readonly flag.
///
/// `password` is supported for WebDAV only. SMB must use native credentials or its
/// terminal password prompt. This function never creates a Keychain item.
pub fn mount(
    profile: &Profile,
    mountpoint: &Path,
    options: &MountOptions,
    password: Option<&str>,
) -> Result<MountInfo> {
    ensure_macos()?;
    if matches!(profile.protocol, Protocol::Smb) && password.is_some() {
        bail!("native SMB mount cannot accept an rdrive password; use the native terminal prompt or existing macOS credentials");
    }
    if password.is_none() && !options.no_prompt && !terminal_available() {
        bail!("native mount requires a controlling terminal for its credential prompt; run it in a terminal or use --no-prompt with existing macOS credentials");
    }
    mount_with(&Native::default(), profile, mountpoint, options, password)
}

/// Unmount only an exact, user-owned SMB/WebDAV mountpoint. No force by default.
/// Returns the volume that has been confirmed absent from the kernel mount table.
pub fn unmount(mountpoint: &Path, force: bool) -> Result<MountInfo> {
    ensure_macos()?;
    unmount_with(&Native::default(), mountpoint, force)
}

fn is_network(info: &MountInfo) -> bool {
    matches!(info.filesystem.as_str(), "smbfs" | "webdav")
}

fn ensure_macos() -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        bail!("native network mounts are currently supported on macOS only");
    }
    #[cfg(target_os = "macos")]
    Ok(())
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

#[cfg(unix)]
fn terminal_available() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
}
#[cfg(not(unix))]
fn terminal_available() -> bool {
    false
}

fn plan_with(
    system: &dyn System,
    profile: &Profile,
    mountpoint: &Path,
    options: &MountOptions,
) -> Result<MountPlan> {
    let source = source_for(profile)?;
    let before = system.mounts()?;
    reject_overlapping(&absolute_path(mountpoint)?, &before)?;
    let point = validate_mountpoint(mountpoint)?;
    build_plan(profile, &point.path, source, options)
}

fn build_plan(
    profile: &Profile,
    mountpoint: &Path,
    source: String,
    options: &MountOptions,
) -> Result<MountPlan> {
    let target = mountpoint
        .to_str()
        .context("mountpoint must be UTF-8 for macOS native helpers")?;
    let mut native_options = vec!["nodev", "nosuid"];
    if options.read_only {
        native_options.push("rdonly");
    }
    if options.no_browse {
        native_options.push("nobrowse");
    }
    let (filesystem, credential_mode, mut notes) = match profile.protocol {
        Protocol::Smb => {
            // Apple's helper decodes percent escapes in its mountpoint argument.
            if target.contains('%') {
                bail!("native SMB mountpoints must not contain '%' because Apple's helper decodes percent escapes");
            }
            native_options.push("forcenewsession");
            if profile.smb_encryption {
                native_options.extend(["sessionencrypt", "shareencrypt"]);
            }
            if options.no_prompt {
                native_options.push("-N");
            }
            ("smbfs", if options.no_prompt { "native_credentials_no_prompt" } else { "native_terminal_prompt" }, vec![
                "SMB uses macOS credentials or Apple's terminal prompt; rdrive password-env/Keychain passwords are not passed to SMB.".to_owned(),
                "Existing macOS SMB preferences may affect the native client; the mounted root is verified before success is reported.".to_owned(),
            ])
        }
        Protocol::Webdav => {
            if source.starts_with("https:") {
                native_options.push("-s");
            }
            native_options.push(if options.no_prompt { "-S" } else { "-i" });
            ("webdav", if options.no_prompt { "native_credentials_no_prompt" } else { "native_terminal_prompt" }, vec![
                "WebDAV may become readonly automatically when the server does not implement DAV LOCK.".to_owned(),
                "A supplied WebDAV password replaces the prompt with an anonymous seekable credential descriptor; no Keychain item is created.".to_owned(),
                "The native WebDAV helper does not support nofollow; private paths and the mountpoint inode are checked before execution.".to_owned(),
            ])
        }
    };
    notes.push("Native mounts provide online network volumes, not placeholders, offline pinning, or rdrive sync.".to_owned());
    let mut args = vec!["-t".to_owned(), filesystem.to_owned()];
    if filesystem == "smbfs" {
        args.push("-k".to_owned());
    }
    args.extend([
        "-o".to_owned(),
        native_options.join(","),
        source.clone(),
        target.to_owned(),
    ]);
    Ok(MountPlan {
        program: "/sbin/mount".to_owned(),
        args,
        mountpoint: mountpoint.to_owned(),
        source,
        filesystem: filesystem.to_owned(),
        credential_mode: credential_mode.to_owned(),
        read_only: options.read_only,
        encryption_required: matches!(profile.protocol, Protocol::Smb) && profile.smb_encryption,
        notes,
    })
}

fn mount_with(
    system: &dyn System,
    profile: &Profile,
    mountpoint: &Path,
    options: &MountOptions,
    password: Option<&str>,
) -> Result<MountInfo> {
    let source = source_for(profile)?;
    let before = system.mounts()?;
    reject_overlapping(&absolute_path(mountpoint)?, &before)?;
    let point = validate_mountpoint(mountpoint)?;
    let mut plan = build_plan(profile, &point.path, source, options)?;
    let credentials = match password {
        Some(password) if matches!(profile.protocol, Protocol::Webdav) => {
            let credential_file =
                CredentialFile::new(profile.username.as_deref().unwrap_or(""), password)?;
            let opt = plan
                .args
                .iter()
                .position(|a| a == "-o")
                .context("missing native mount options")?
                + 1;
            let mut flags: Vec<_> = plan.args[opt]
                .split(',')
                .filter(|x| *x != "-i" && *x != "-S")
                .map(str::to_owned)
                .collect();
            flags.extend(["-a=0".to_owned(), "-S".to_owned()]);
            plan.args[opt] = flags.join(",");
            plan.credential_mode = "anonymous_credential_descriptor".to_owned();
            Some(credential_file)
        }
        Some(_) => bail!("native SMB accepts only its own credential flow"),
        None => None,
    };
    point.recheck()?;
    let result = system.run(&plan, credentials.as_ref(), profile.password_env.as_deref());
    // Always inspect the kernel, even when the helper failed after mounting.
    let after = system
        .mounts()
        .context("unable to verify the mount result; inspect the system mount table")?;
    let candidate = after.iter().find(|m| m.info.mountpoint == point.path);
    let successful = result.as_ref().is_ok_and(|success| *success);
    let Some(candidate) = candidate else {
        result.context("native mount helper failed")?;
        if successful {
            bail!(
                "native helper exited successfully but no volume is mounted at {}",
                point.path.display()
            );
        }
        bail!(
            "native mount helper failed; no volume was mounted at {}",
            point.path.display()
        );
    };
    if candidate.info.filesystem != plan.filesystem || candidate.info.owner != uid() {
        bail!(
            "a different volume appeared at {}; it was not unmounted automatically",
            point.path.display()
        );
    }
    let root_matches = source_matches(&plan.source, &candidate.source, &plan.filesystem);
    if !root_matches || (options.read_only && !candidate.info.read_only) || !successful {
        // No mount was present before; this is a new native volume owned by us.
        // Refuse a native client's broader submount fallback or ignored readonly.
        let reason = if !root_matches {
            "native client mounted a different root"
        } else if options.read_only && !candidate.info.read_only {
            "native client ignored readonly"
        } else {
            "native helper failed despite leaving a mounted volume"
        };
        let cleanup = system.detach(&point.path, false);
        let remaining = system
            .mounts()?
            .iter()
            .any(|m| m.info.mountpoint == point.path);
        if remaining || !cleanup.unwrap_or(false) {
            bail!(
                "{reason}; cleanup did not complete; inspect and unmount {}",
                point.path.display()
            );
        }
        bail!("{reason}; the new volume was unmounted");
    }
    Ok(candidate.info.clone())
}

fn unmount_with(system: &dyn System, mountpoint: &Path, force: bool) -> Result<MountInfo> {
    let target = absolute_path(mountpoint)?;
    // Do not canonicalize through the mounted server or accept an inner folder.
    let current = system.mounts()?;
    let mounted = current
        .iter()
        .find(|m| m.info.mountpoint == target)
        .context("path is not an exact mounted volume")?;
    if !is_network(&mounted.info) || mounted.info.owner != uid() {
        bail!("only SMB/WebDAV volumes mounted by the current user can be unmounted");
    }
    if current
        .iter()
        .any(|m| m.info.mountpoint != target && m.info.mountpoint.starts_with(&target))
    {
        bail!("unmount nested volumes first");
    }
    let result = system.detach(&target, force);
    let after = system.mounts()?;
    if after.iter().any(|m| m.info.mountpoint == target) {
        bail!(
            "volume remains mounted at {}; close files and retry{}",
            target.display(),
            if force {
                ""
            } else {
                " (force is an explicit option)"
            }
        );
    }
    // If the exact volume disappeared, the desired result is independently confirmed.
    let _ = result;
    Ok(mounted.info.clone())
}

fn reject_overlapping(path: &Path, mounts: &[KernelMount]) -> Result<()> {
    if mounts
        .iter()
        .any(|m| m.info.mountpoint == path || m.info.mountpoint.starts_with(path))
    {
        bail!("mountpoint is already mounted or contains another mounted volume");
    }
    if mounts
        .iter()
        .any(|m| is_network(&m.info) && path.starts_with(&m.info.mountpoint))
    {
        bail!("mountpoint must be on a local filesystem, outside other network volumes");
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        bail!("mountpoint must be an absolute path without '.' or '..'");
    }
    if path.as_os_str().is_empty() || path == Path::new("/") {
        bail!("root is not an allowed mountpoint");
    }
    Ok(path.components().collect())
}

struct MountPoint {
    path: PathBuf,
    file: File,
}

#[cfg(unix)]
fn validate_ancestor_permissions(
    path: &Path,
    owner: u32,
    mode: u32,
    current_uid: u32,
) -> Result<()> {
    if owner != 0 && owner != current_uid {
        bail!(
            "mountpoint ancestor is owned by a different user: {}",
            path.display()
        );
    }
    // Sticky root temp directories protect entries from deletion by other users.
    let trusted_sticky = owner == 0 && mode & 0o1000 != 0;
    if mode & 0o022 != 0 && !trusted_sticky {
        bail!(
            "mountpoint ancestor is writable by other users: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn validate_mountpoint(path: &Path) -> Result<MountPoint> {
    let path = absolute_path(path)?;
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part.as_os_str());
        let meta = fs::symlink_metadata(&prefix).with_context(|| {
            format!(
                "mountpoint must already exist as a private empty directory: {}",
                prefix.display()
            )
        })?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!(
                "mountpoint components must be real directories: {}",
                prefix.display()
            );
        }
        validate_ancestor_permissions(&prefix, meta.uid(), meta.mode(), uid())?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != uid() || metadata.mode() & 0o077 != 0 || metadata.mode() & 0o700 != 0o700 {
        bail!(
            "mountpoint must belong to the current user with owner-only permissions (mkdir -m 700)"
        );
    }
    if fs::read_dir(&path)?.next().transpose()?.is_some() {
        bail!("mountpoint must be empty; existing data will not be hidden");
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("mountpoint is already in use by another rdrive operation");
    }
    let point = MountPoint { path, file };
    point.recheck()?;
    Ok(point)
}

#[cfg(not(unix))]
fn validate_mountpoint(_path: &Path) -> Result<MountPoint> {
    bail!("native mounting requires macOS");
}

impl MountPoint {
    #[cfg(unix)]
    fn recheck(&self) -> Result<()> {
        let before = self.file.metadata()?;
        let now = fs::symlink_metadata(&self.path)?;
        if !now.is_dir()
            || now.file_type().is_symlink()
            || before.dev() != now.dev()
            || before.ino() != now.ino()
            || now.mode() & 0o077 != 0
            || now.uid() != uid()
        {
            bail!("mountpoint changed while preparing mount");
        }
        if fs::read_dir(&self.path)?.next().transpose()?.is_some() {
            bail!("mountpoint is no longer empty");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fn recheck(&self) -> Result<()> {
        bail!("native mounting requires macOS");
    }
}

fn source_for(profile: &Profile) -> Result<String> {
    validate_profile(profile)?;
    // Url normalizes dot segments. Reject them in the original URL before parsing.
    let raw = profile
        .url
        .split_once("://")
        .context("invalid native mount URL")?
        .1;
    let raw_path = raw.find('/').map(|i| &raw[i..]).unwrap_or("/");
    checked_segments(raw_path)?;
    let mut url = url::Url::parse(&profile.url)?;
    if let Some(username) = &profile.username {
        if username.is_empty() || username.chars().any(|c| c.is_control() || c == ':') {
            bail!("native mount username must be nonempty and must not contain ':' or control characters");
        }
    }
    if matches!(profile.protocol, Protocol::Smb) {
        if let Some(username) = &profile.username {
            url.set_username(&percent_encode(username))
                .map_err(|_| anyhow::anyhow!("invalid SMB username"))?;
        }
        // set_username accepts already-escaped characters unchanged.
        Ok(url
            .as_str()
            .trim_start_matches("smb:")
            .trim_end_matches('/')
            .to_owned())
    } else {
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url.to_string())
    }
}

fn percent_encode(value: &str) -> String {
    let mut result = String::new();
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut result, "%{byte:02X}").unwrap();
        }
    }
    result
}

fn decode(value: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut rest = value.as_bytes();
    while !rest.is_empty() {
        if rest[0] == b'%' {
            if rest.len() < 3 {
                bail!("invalid URL escape");
            }
            let hi = (rest[1] as char)
                .to_digit(16)
                .context("invalid URL escape")?;
            let lo = (rest[2] as char)
                .to_digit(16)
                .context("invalid URL escape")?;
            bytes.push((hi * 16 + lo) as u8);
            rest = &rest[3..];
        } else {
            bytes.push(rest[0]);
            rest = &rest[1..];
        }
    }
    Ok(String::from_utf8(bytes).context("native mount URL must use valid UTF-8")?)
}

fn checked_segments(path: &str) -> Result<Vec<String>> {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let decoded = decode(part)?;
            if decoded == "."
                || decoded == ".."
                || decoded
                    .chars()
                    .any(|c| c.is_control() || c == '/' || c == '\\')
            {
                bail!("unsafe native mount URL path component");
            }
            Ok(decoded)
        })
        .collect()
}

fn source_matches(expected: &str, actual: &str, filesystem: &str) -> bool {
    let parse = |source: &str| -> Result<url::Url> {
        Ok(url::Url::parse(&if filesystem == "smbfs" {
            format!("smb:{source}")
        } else {
            source.to_owned()
        })?)
    };
    let (Ok(expected), Ok(actual)) = (parse(expected), parse(actual)) else {
        return false;
    };
    if expected.scheme() != actual.scheme()
        || expected.host_str().map(str::to_ascii_lowercase)
            != actual.host_str().map(str::to_ascii_lowercase)
        || expected.port_or_known_default() != actual.port_or_known_default()
        || actual.password().is_some()
    {
        return false;
    }
    let (Ok(mut ep), Ok(mut ap)) = (
        checked_segments(expected.path()),
        checked_segments(actual.path()),
    ) else {
        return false;
    };
    if filesystem == "smbfs" {
        if !expected.username().is_empty()
            && decode(expected.username()).ok() != decode(actual.username()).ok()
        {
            return false;
        }
        // SMB share names are case-insensitive. Subdirectories keep their case.
        if let Some(share) = ep.first_mut() {
            *share = share.to_lowercase();
        }
        if let Some(share) = ap.first_mut() {
            *share = share.to_lowercase();
        }
    }
    ep == ap
}

fn sanitized_source(source: &str) -> String {
    let smb = source.starts_with("//");
    let input = if smb {
        format!("smb:{source}")
    } else {
        source.to_owned()
    };
    if let Ok(mut url) = url::Url::parse(&input) {
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        return if smb {
            url.as_str().trim_start_matches("smb:").to_owned()
        } else {
            url.to_string()
        };
    }
    // Do not echo an unparseable mount source which could contain old credentials.
    "[unrecognized mount source]".to_owned()
}

/// Apple readCredentialsFromFile uses lseek, so a pipe is unsuitable. Unlink a
/// mode0600 file before writing; only the inherited stdin descriptor names it.
struct CredentialFile {
    file: File,
    length: usize,
}

impl CredentialFile {
    #[cfg(unix)]
    fn new(username: &str, password: &str) -> Result<Self> {
        if username.len() >= 256
            || password.len() >= 256
            || username.contains('\0')
            || password.contains('\0')
        {
            bail!("native WebDAV credentials must be NUL-free and shorter than 256 UTF-8 bytes");
        }
        let name =
            std::env::temp_dir().join(format!(".rdrive-native-auth-{}", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&name)?;
        // If unlink fails, do not write any credential bytes.
        fs::remove_file(&name).context("could not unlink native credential descriptor")?;
        let length = username.len() + password.len() + 5 * 4;
        let mut result = Self { file, length };
        // Big endian lengths: username, password, proxy user, proxy pass, cert plist.
        for value in [username, password, "", "", ""] {
            result.file.write_all(&(value.len() as u32).to_be_bytes())?;
            result.file.write_all(value.as_bytes())?;
        }
        result.file.seek(SeekFrom::Start(0))?;
        Ok(result)
    }
    #[cfg(not(unix))]
    fn new(_username: &str, _password: &str) -> Result<Self> {
        bail!("native mounting requires macOS");
    }
}

impl Drop for CredentialFile {
    fn drop(&mut self) {
        let _ = self.file.seek(SeekFrom::Start(0));
        let zeros = [0u8; 1024];
        let _ = self.file.write_all(&zeros[..self.length.min(zeros.len())]);
        let _ = self.file.set_len(0);
    }
}

impl System for Native {
    fn mounts(&self) -> Result<Vec<KernelMount>> {
        kernel_mounts()
    }
    fn run(
        &self,
        plan: &MountPlan,
        credentials: Option<&CredentialFile>,
        remove_env: Option<&str>,
    ) -> Result<bool> {
        let mut command = Command::new(&plan.program);
        command.args(&plan.args);
        if let Some(key) = remove_env {
            command.env_remove(key);
        }
        if let Some(credentials) = credentials {
            command.stdin(Stdio::from(credentials.file.try_clone()?));
        }
        (self.command_status)(&mut command).context("could not start macOS native mount helper")
    }
    fn detach(&self, path: &Path, force: bool) -> Result<bool> {
        let mut command = Command::new("/sbin/umount");
        if force {
            command.arg("-f");
        }
        command.arg(path);
        (self.command_status)(&mut command).context("could not start macOS unmount helper")
    }
}

#[cfg(target_os = "macos")]
fn kernel_mounts() -> Result<Vec<KernelMount>> {
    kernel_mounts_with(|records, bytes, flags| unsafe { libc::getfsstat(records, bytes, flags) })
}

#[cfg(target_os = "macos")]
fn kernel_mounts_with(
    mut query: impl FnMut(*mut libc::statfs, i32, i32) -> i32,
) -> Result<Vec<KernelMount>> {
    // getfsstat gives us an owned buffer, avoiding getmntinfo's process-global data.
    for _ in 0..4 {
        let count = query(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let capacity = count as usize + 16;
        let mut records: Vec<libc::statfs> = (0..capacity)
            .map(|_| unsafe { std::mem::zeroed() })
            .collect();
        let bytes = i32::try_from(capacity * std::mem::size_of::<libc::statfs>())?;
        let got = query(records.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
        if got < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if got as usize >= capacity {
            continue;
        }
        return records
            .into_iter()
            .take(got as usize)
            .map(|record| {
                let string = |value: &[libc::c_char]| -> Result<String> {
                    let end = value
                        .iter()
                        .position(|b| *b == 0)
                        .context("unterminated kernel mount field")?;
                    let bytes: Vec<u8> = value[..end].iter().map(|b| *b as u8).collect();
                    Ok(String::from_utf8(bytes).context("kernel mount paths must be UTF-8")?)
                };
                let source = string(&record.f_mntfromname)?;
                let filesystem = string(&record.f_fstypename)?;
                // fsid_t is two int32_t values in the Darwin ABI (libc fields are private).
                let fsid: [i32; 2] = unsafe { std::mem::transmute(record.f_fsid) };
                Ok(KernelMount {
                    info: MountInfo {
                        mountpoint: PathBuf::from(string(&record.f_mntonname)?),
                        source: sanitized_source(&source),
                        filesystem,
                        read_only: record.f_flags & libc::MNT_RDONLY as u32 != 0,
                        no_browse: record.f_flags & libc::MNT_DONTBROWSE as u32 != 0,
                        owner: record.f_owner,
                        fsid,
                    },
                    source,
                })
            })
            .collect();
    }
    bail!("mount table kept changing; retry the operation")
}

#[cfg(not(target_os = "macos"))]
fn kernel_mounts() -> Result<Vec<KernelMount>> {
    bail!("native mount listing is currently supported on macOS only");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        io::Read,
        os::unix::fs::PermissionsExt,
    };

    fn profile(protocol: Protocol) -> Profile {
        let url = if matches!(protocol, Protocol::Smb) {
            "smb://server/test/subdir"
        } else {
            "https://example.test/dav/root/"
        };
        Profile {
            protocol,
            url: url.to_owned(),
            username: Some("test".to_owned()),
            password_env: Some("RDRIVE_TEST_SECRET".to_owned()),
            allow_http: false,
            timeout_secs: 15,
            smb_encryption: true,
            nextcloud_upload_url: None,
        }
    }

    fn target() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().canonicalize().unwrap().join("mount");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, target)
    }

    fn record(path: &Path, source: &str, filesystem: &str) -> KernelMount {
        KernelMount {
            info: MountInfo {
                mountpoint: path.to_owned(),
                source: sanitized_source(source),
                filesystem: filesystem.to_owned(),
                read_only: false,
                no_browse: false,
                owner: uid(),
                fsid: [81, 19],
            },
            source: source.to_owned(),
        }
    }

    struct MockSystem {
        active: RefCell<Vec<KernelMount>>,
        mount_result: Option<KernelMount>,
        helper_success: bool,
        detach_success: bool,
        runs: Cell<usize>,
        plans: RefCell<Vec<MountPlan>>,
        credential_bytes: RefCell<Vec<u8>>,
        removed_env: RefCell<Option<String>>,
        forced: RefCell<Vec<bool>>,
    }

    impl MockSystem {
        fn new(mount_result: Option<KernelMount>) -> Self {
            Self {
                active: RefCell::new(vec![]),
                mount_result,
                helper_success: true,
                detach_success: true,
                runs: Cell::new(0),
                plans: RefCell::new(vec![]),
                credential_bytes: RefCell::new(vec![]),
                removed_env: RefCell::new(None),
                forced: RefCell::new(vec![]),
            }
        }
    }

    impl System for MockSystem {
        fn mounts(&self) -> Result<Vec<KernelMount>> {
            Ok(self.active.borrow().clone())
        }
        fn run(
            &self,
            plan: &MountPlan,
            credentials: Option<&CredentialFile>,
            remove_env: Option<&str>,
        ) -> Result<bool> {
            self.runs.set(self.runs.get() + 1);
            self.plans.borrow_mut().push(plan.clone());
            *self.removed_env.borrow_mut() = remove_env.map(str::to_owned);
            if let Some(credentials) = credentials {
                let mut file = credentials.file.try_clone()?;
                file.read_to_end(&mut self.credential_bytes.borrow_mut())?;
                file.seek(SeekFrom::Start(0))?;
            }
            if let Some(result) = &self.mount_result {
                self.active.borrow_mut().push(result.clone());
            }
            Ok(self.helper_success)
        }
        fn detach(&self, path: &Path, force: bool) -> Result<bool> {
            self.forced.borrow_mut().push(force);
            if self.detach_success {
                self.active
                    .borrow_mut()
                    .retain(|m| m.info.mountpoint != path);
            }
            Ok(self.detach_success)
        }
    }

    #[test]
    fn mount_source_scope_and_username_encoding() {
        let mut smb = profile(Protocol::Smb);
        smb.username = Some("a@b;\\用户".to_owned());
        let source = source_for(&smb).unwrap();
        assert_eq!(source, "//a%40b%3B%5C%E7%94%A8%E6%88%B7@server/test/subdir");
        assert!(source_matches(&source, &source, "smbfs"));
        assert!(!source_matches(&source, "//a@server/test", "smbfs"));
        assert!(!source_matches(
            "//test@server/test/subdir",
            "//test@server/test",
            "smbfs"
        ));
        assert!(!source_matches(
            "https://server/root",
            "https://server/",
            "webdav"
        ));
        assert!(source_matches(
            "https://server/root%20dir/",
            "https://SERVER/root%20dir",
            "webdav"
        ));
    }

    #[test]
    fn rejects_embedded_credentials_and_unsafe_roots() {
        let mut p = profile(Protocol::Webdav);
        for url in [
            "https://user:secret@server/root",
            "https://server/a/../root",
            "https://server/a/%2e%2e/root",
            "https://server/root/%2fescape",
            "https://server/root/%5cescape",
            "https://server/root/%00",
        ] {
            p.url = url.to_owned();
            assert!(source_for(&p).is_err(), "{url}");
        }
        p.url = "https://server/root".to_owned();
        p.username = Some("user:secret".to_owned());
        assert!(source_for(&p).is_err());
        assert_eq!(sanitized_source("//u:secret@host/share"), "//u@host/share");
        assert!(!sanitized_source("broken secret!").contains("secret"));
    }

    #[test]
    fn plan_has_native_encryption_readonly_and_no_password() {
        let (_root, path) = target();
        let system = MockSystem::new(None);
        let options = MountOptions {
            read_only: true,
            no_browse: true,
            no_prompt: true,
        };
        let p = plan_with(&system, &profile(Protocol::Smb), &path, &options).unwrap();
        assert_eq!(p.program, "/sbin/mount");
        assert!(p.args.iter().any(|a| a == "-k"));
        assert!(p
            .args
            .iter()
            .any(|a| a.contains("sessionencrypt,shareencrypt")));
        assert!(p.args.iter().any(|a| a.contains("rdonly,nobrowse")));
        assert!(p.args.iter().any(|a| a.contains("-N")));
        assert!(p.encryption_required);
        assert_eq!(system.runs.get(), 0);
        let w = plan_with(&system, &profile(Protocol::Webdav), &path, &options).unwrap();
        assert!(!w.args.iter().any(|a| a == "-k" || a.contains("nofollow")));
    }

    #[test]
    fn ancestor_permissions_reject_foreign_owners_and_unprotected_writable_directories() {
        let path = Path::new("/synthetic/ancestor");
        assert!(validate_ancestor_permissions(path, 502, 0o700, 501)
            .unwrap_err()
            .to_string()
            .contains("different user"));
        assert!(validate_ancestor_permissions(path, 501, 0o777, 501)
            .unwrap_err()
            .to_string()
            .contains("writable by other users"));
        assert!(validate_ancestor_permissions(path, 0, 0o777, 501).is_err());
        assert!(validate_ancestor_permissions(path, 501, 0o1777, 501).is_err());
        assert!(validate_ancestor_permissions(path, 0, 0o1777, 501).is_ok());
        assert!(validate_ancestor_permissions(path, 501, 0o700, 501).is_ok());
    }

    #[test]
    fn public_mount_inspection_and_planning_never_mount_an_empty_fixture() {
        let (_root, path) = target();
        assert!(list().is_ok());
        assert!(plan(&profile(Protocol::Webdav), &path, &MountOptions::default()).is_ok());
        assert!(unmount(&path, false)
            .unwrap_err()
            .to_string()
            .contains("not an exact mounted volume"));
        let network = record(&path, "//fixture@host/share", "smbfs");
        let local = record(&path, "disk1s1", "apfs");
        let filtered = network_mounts(vec![local, network]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].filesystem, "smbfs");
    }

    #[test]
    fn rejects_nonempty_symlink_and_public_mountpoints() {
        let (_root, path) = target();
        fs::write(path.join("data"), "do not hide me").unwrap();
        assert!(validate_mountpoint(&path).is_err());
        fs::remove_file(path.join("data")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_mountpoint(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = path.with_file_name("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(validate_mountpoint(&alias).is_err());
        assert!(validate_mountpoint(Path::new("relative")).is_err());
        assert!(validate_mountpoint(&path.join("..").join("mount")).is_err());
    }

    #[test]
    fn detects_mountpoint_replacement_and_busy_inode() {
        let (_root, path) = target();
        let held = validate_mountpoint(&path).unwrap();
        assert!(validate_mountpoint(&path).is_err());
        fs::rename(&path, path.with_file_name("old")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(held.recheck().is_err());
    }

    #[test]
    fn prevents_overlay_and_network_mount_nesting() {
        let (_root, path) = target();
        let mount = record(&path, "//test@server/test", "smbfs");
        assert!(reject_overlapping(&path, &[mount.clone()]).is_err());
        assert!(reject_overlapping(path.parent().unwrap(), &[mount.clone()]).is_err());
        assert!(reject_overlapping(&path.join("inner"), &[mount]).is_err());
    }

    #[test]
    fn credentials_use_unlinked_fd_and_are_erased_on_drop() {
        let secret = CredentialFile::new("user", "password").unwrap();
        assert_eq!(secret.file.metadata().unwrap().nlink(), 0);
        assert_eq!(secret.file.metadata().unwrap().mode() & 0o777, 0o600);
        let mut held = secret.file.try_clone().unwrap();
        let mut bytes = vec![];
        held.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            b"\0\0\0\x04user\0\0\0\x08password\0\0\0\0\0\0\0\0\0\0\0\0"
        );
        drop(secret);
        assert_eq!(held.metadata().unwrap().len(), 0);
        assert!(CredentialFile::new("u", &"x".repeat(256)).is_err());
        assert!(CredentialFile::new("u", "bad\0value").is_err());
    }

    #[test]
    fn credential_handoff_changes_flags_and_does_not_expose_secret() {
        let (_root, path) = target();
        let profile = profile(Protocol::Webdav);
        let system = MockSystem::new(Some(record(&path, &profile.url, "webdav")));
        let mounted = mount_with(
            &system,
            &profile,
            &path,
            &MountOptions::default(),
            Some("needle-password"),
        )
        .unwrap();
        assert_eq!(mounted.filesystem, "webdav");
        let plans = system.plans.borrow();
        assert!(plans[0].args.iter().any(|a| a.contains("-a=0,-S")));
        assert!(!plans[0]
            .args
            .iter()
            .any(|a| a.contains("-i") || a.contains("needle-password")));
        assert!(system
            .credential_bytes
            .borrow()
            .windows(15)
            .any(|b| b == b"needle-password"));
        assert_eq!(
            system.removed_env.borrow().as_deref(),
            Some("RDRIVE_TEST_SECRET")
        );
    }

    #[test]
    fn exit_success_without_kernel_mount_is_failure() {
        let (_root, path) = target();
        let system = MockSystem::new(None);
        let error = mount_with(
            &system,
            &profile(Protocol::Webdav),
            &path,
            &MountOptions::default(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no volume is mounted"));
    }

    #[test]
    fn broader_root_readonly_and_failed_helper_are_cleaned_up() {
        for scenario in 0..3 {
            let (_root, path) = target();
            let profile = profile(Protocol::Smb);
            let source = if scenario == 0 {
                "//test@server/test"
            } else {
                "//test@server/test/subdir"
            };
            let mut system = MockSystem::new(Some(record(&path, source, "smbfs")));
            system.helper_success = scenario != 2;
            let options = MountOptions {
                read_only: scenario == 1,
                ..Default::default()
            };
            assert!(mount_with(&system, &profile, &path, &options, None).is_err());
            assert!(system.active.borrow().is_empty());
            assert_eq!(*system.forced.borrow(), vec![false]);
        }
    }

    #[test]
    fn reports_failed_cleanup_and_does_not_detach_wrong_filesystem() {
        let (_root, path) = target();
        let mut system = MockSystem::new(Some(record(&path, "//test@server/test", "smbfs")));
        system.detach_success = false;
        let error = mount_with(
            &system,
            &profile(Protocol::Smb),
            &path,
            &MountOptions::default(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cleanup did not complete"));
        let (_root2, path2) = target();
        let system2 = MockSystem::new(Some(record(&path2, "//test@server/test/subdir", "apfs")));
        assert!(mount_with(
            &system2,
            &profile(Protocol::Smb),
            &path2,
            &MountOptions::default(),
            None
        )
        .is_err());
        assert!(system2.forced.borrow().is_empty());
    }

    #[test]
    fn unmount_requires_exact_owned_network_volume_and_verifies_removal() {
        let (_root, path) = target();
        let mut system = MockSystem::new(None);
        system
            .active
            .borrow_mut()
            .push(record(&path, "//test@server/test", "smbfs"));
        assert!(unmount_with(&system, &path.join("inner"), false).is_err());
        system.detach_success = false;
        assert!(unmount_with(&system, &path, false).is_err());
        system.detach_success = true;
        assert!(unmount_with(&system, &path, true).is_ok());
        assert_eq!(*system.forced.borrow(), vec![false, true]);
        let mut wrong_owner = record(&path, "//test@server/test", "smbfs");
        wrong_owner.info.owner = uid().wrapping_add(1);
        system.active.borrow_mut().push(wrong_owner);
        assert!(unmount_with(&system, &path, false).is_err());
    }

    #[test]
    fn command_adapters_are_observable_without_running_mount_or_umount() {
        fn inspected(command: &mut Command) -> Result<bool> {
            let program = command.get_program().to_string_lossy();
            let arguments = command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if program == "/sbin/umount" {
                assert!(arguments == ["/synthetic"] || arguments == ["-f", "/synthetic"]);
            } else {
                assert_eq!(program, "/sbin/mount");
                assert!(command
                    .get_envs()
                    .any(|(name, value)| name == "RDRIVE_TEST_SECRET" && value.is_none()));
            }
            Ok(true)
        }
        let system = Native {
            command_status: inspected,
        };
        let (_root, path) = target();
        let plan = build_plan(
            &profile(Protocol::Webdav),
            &path,
            "https://example.invalid/".into(),
            &MountOptions::default(),
        )
        .unwrap();
        assert!(system.run(&plan, None, Some("RDRIVE_TEST_SECRET")).unwrap());
        let credentials = CredentialFile::new("synthetic", "private-fixture-only").unwrap();
        assert!(system
            .run(&plan, Some(&credentials), Some("RDRIVE_TEST_SECRET"))
            .unwrap());
        assert!(system.detach(Path::new("/synthetic"), false).unwrap());
        assert!(system.detach(Path::new("/synthetic"), true).unwrap());
        assert!(helper_status(&mut Command::new("/usr/bin/true")).unwrap());
        assert!(!helper_status(&mut Command::new("/usr/bin/false")).unwrap());
        assert!(helper_status(&mut Command::new("/rdrive-synthetic-missing-program")).is_err());
    }

    #[test]
    fn additional_mountpoint_and_source_guards_preserve_existing_files() {
        let (_root, path) = target();
        assert!(absolute_path(Path::new("/")).is_err());
        let percent = path.with_file_name("percent%name");
        assert!(build_plan(
            &profile(Protocol::Smb),
            &percent,
            "//host/share".into(),
            &MountOptions::default()
        )
        .is_err());
        let mut dav = profile(Protocol::Webdav);
        dav.url = "https://example.test/no-trailing-slash".into();
        assert!(source_for(&dav).unwrap().ends_with('/'));
        let mut anonymous = profile(Protocol::Smb);
        anonymous.username = None;
        assert_eq!(source_for(&anonymous).unwrap(), "//server/test/subdir");
        for input in ["%", "%0", "%xx", "%ff"] {
            assert!(decode(input).is_err());
        }
        for (expected, actual) in [
            ("invalid", "https://host/root"),
            ("https://host/root", "invalid"),
            ("https://host/%2f", "https://host/root"),
        ] {
            assert!(!source_matches(expected, actual, "webdav"));
        }
        assert!(!source_matches(
            "//alice@host/share",
            "//bob@host/share",
            "smbfs"
        ));
        assert!(!source_matches(
            "https://host/root",
            "https://user:secret@host/root",
            "webdav"
        ));
        let point = validate_mountpoint(&path).unwrap();
        fs::write(path.join("appeared"), b"preserve").unwrap();
        assert!(point.recheck().is_err());
        assert!(mount(
            &profile(Protocol::Smb),
            &path,
            &MountOptions::default(),
            Some("synthetic")
        )
        .is_err());
        assert_eq!(fs::read(path.join("appeared")).unwrap(), b"preserve");
        let missing = path.join("missing");
        assert!(mount(
            &profile(Protocol::Webdav),
            &missing,
            &MountOptions {
                no_prompt: true,
                ..Default::default()
            },
            Some("synthetic")
        )
        .is_err());
        assert!(mount(
            &profile(Protocol::Webdav),
            &missing,
            &MountOptions::default(),
            None
        )
        .is_err());
    }

    #[test]
    fn failed_helper_and_nested_unmount_never_hide_unconfirmed_state() {
        let (_root, path) = target();
        let mut system = MockSystem::new(None);
        system.helper_success = false;
        assert!(mount_with(
            &system,
            &profile(Protocol::Webdav),
            &path,
            &MountOptions::default(),
            None
        )
        .unwrap_err()
        .to_string()
        .contains("no volume"));
        assert!(mount_with(
            &system,
            &profile(Protocol::Smb),
            &path,
            &MountOptions::default(),
            Some("synthetic")
        )
        .is_err());
        system
            .active
            .borrow_mut()
            .push(record(&path, "//test@server/test", "smbfs"));
        system.active.borrow_mut().push(record(
            &path.join("nested"),
            "//test@server/other",
            "smbfs",
        ));
        assert!(unmount_with(&system, &path, false)
            .unwrap_err()
            .to_string()
            .contains("nested"));
        assert!(system.forced.borrow().is_empty());
        system
            .active
            .borrow_mut()
            .retain(|entry| entry.info.mountpoint == path);
        system.detach_success = false;
        assert!(unmount_with(&system, &path, true)
            .unwrap_err()
            .to_string()
            .contains("remains"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_snapshot_failures_and_churn_are_tested_with_synthetic_records() {
        assert!(kernel_mounts_with(|_, _, _| -1).is_err());
        assert!(
            kernel_mounts_with(|records, _, _| if records.is_null() { 0 } else { -1 }).is_err()
        );
        assert!(
            kernel_mounts_with(|records, bytes, _| if records.is_null() {
                0
            } else {
                bytes / (std::mem::size_of::<libc::statfs>() as i32)
            })
            .is_err()
        );
        let malformed = |kind: u8| {
            kernel_mounts_with(|records, _, _| {
                if records.is_null() {
                    return 1;
                }
                let mut record: libc::statfs = unsafe { std::mem::zeroed() };
                for (field, text) in [
                    (&mut record.f_fstypename[..], b"smbfs".as_slice()),
                    (&mut record.f_mntfromname[..], b"//host/share".as_slice()),
                    (&mut record.f_mntonname[..], b"/synthetic".as_slice()),
                ] {
                    for (out, value) in field.iter_mut().zip(text) {
                        *out = *value as libc::c_char;
                    }
                }
                if kind == 1 {
                    record.f_fstypename.fill(1);
                } else if kind == 2 {
                    record.f_mntfromname[0] = 255_u8 as libc::c_char;
                }
                unsafe { *records = record };
                1
            })
        };
        assert!(malformed(1).is_err());
        assert!(malformed(2).is_err());
        let valid = malformed(0).unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].info.mountpoint, Path::new("/synthetic"));
    }
}
