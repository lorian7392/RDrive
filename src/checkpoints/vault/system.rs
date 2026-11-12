//! A fixed-size, private APFS image. This adapter never creates or deletes
//! native snapshots, physical volumes, privileged services, or system backups.
use super::{Host, Mounted, Space};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MIN_BYTES: u64 = 256 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    home_binding: String,
    instance_id: String,
    capacity_bytes: u64,
    image: Option<Identity>,
    volume_uuid: Option<String>,
    ready: bool,
}
#[derive(Clone)]
struct Paths {
    home: PathBuf,
    vault: PathBuf,
    image: PathBuf,
    descriptor: PathBuf,
    mount: PathBuf,
}
#[derive(Debug, Clone)]
struct Request {
    program: &'static str,
    args: Vec<String>,
    input: Option<Vec<u8>>,
}
#[derive(Debug)]
struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
type Executor = fn(Request) -> Result<Output>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Diskutil,
    Hdiutil,
}
#[derive(Clone, Copy, Debug)]
struct Statistics {
    total: u64,
    used: u64,
    available: u64,
    readonly: bool,
}
pub(super) struct System {
    execute: Executor,
    statistics: fn(&Path) -> Result<Statistics>,
    supported: fn() -> bool,
    family: Option<Family>,
}
impl Default for System {
    fn default() -> Self {
        Self {
            execute,
            statistics,
            supported: || cfg!(target_os = "macos"),
            family: None,
        }
    }
}
fn capacity_value(bytes: u64) -> Result<()> {
    if bytes < MIN_BYTES || bytes > i64::MAX as u64 || bytes % 512 != 0 {
        bail!("APFS vault capacity must be at least 256 MiB, sector aligned, and at most i64::MAX bytes");
    }
    Ok(())
}
fn private(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.file_type().is_symlink()
        || (if directory {
            !meta.is_dir()
        } else {
            !meta.is_file() || meta.nlink() != 1
        })
    {
        bail!("APFS vault paths must be real, private, singly linked and owned by this user");
    }
    Ok(meta)
}
fn directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    let created = match builder.create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    private(path, true)?;
    if created {
        File::open(path)?.sync_all()?;
        File::open(path.parent().context("vault directory needs a parent")?)?.sync_all()?;
    }
    Ok(())
}
fn unmounted_directory(path: &Path, parent: &Path) -> Result<()> {
    if fs::metadata(path)?.dev() != fs::metadata(parent)?.dev() {
        bail!("APFS vault mount point already contains an unrecognized mounted filesystem");
    }
    Ok(())
}
fn paths(home: &Path, initialize: bool) -> Result<Option<Paths>> {
    private(home, true)?;
    if home.canonicalize()? != home {
        bail!("APFS vault requires the canonical state directory");
    }
    let checkpoints = home.join("checkpoints");
    let vault = checkpoints.join("vault");
    if initialize {
        directory(&checkpoints)?;
        directory(&vault)?;
    } else {
        for path in [&checkpoints, &vault] {
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    private(path, true)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(Some(Paths {
        home: home.into(),
        image: vault.join("vault.dmg"),
        descriptor: vault.join("descriptor.json"),
        mount: vault.join("mount"),
        vault,
    }))
}
fn home_binding(home: &Path) -> Result<String> {
    Ok(crate::state::key(
        home.to_str()
            .context("APFS vault state path must be UTF-8")?,
    ))
}
fn regular_identity(path: &Path, normalize_mode: bool) -> Result<Identity> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.nlink() != 1 {
        bail!("APFS vault image must be an owned regular file without hard links");
    }
    if normalize_mode {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    private(path, false)?;
    Ok(Identity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}
fn descriptor(paths: &Paths) -> Result<Option<Descriptor>> {
    match fs::symlink_metadata(&paths.descriptor) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::read_dir(&paths.vault)?.next().is_some() {
                bail!("unrecognized contents in APFS vault; initialization refused");
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    private(&paths.descriptor, false)?;
    if fs::metadata(&paths.descriptor)?.len() > 16 * 1024 {
        bail!("APFS vault descriptor is oversized");
    }
    let value: Descriptor = serde_json::from_slice(&fs::read(&paths.descriptor)?)?;
    capacity_value(value.capacity_bytes)?;
    if value.version != 1
        || value.home_binding != home_binding(&paths.home)?
        || uuid::Uuid::parse_str(&value.instance_id)?.to_string() != value.instance_id
        || value.ready && (value.image.is_none() || value.volume_uuid.is_none())
    {
        bail!("APFS vault descriptor does not identify this state directory");
    }
    if let Some(id) = &value.volume_uuid {
        uuid::Uuid::parse_str(id)?;
    }
    if let Some(expected) = &value.image {
        if regular_identity(&paths.image, false)? != *expected
            || fs::metadata(&paths.image)?.len() != value.capacity_bytes
        {
            bail!("APFS vault image identity or fixed capacity changed");
        }
    }
    Ok(Some(value))
}
fn save(paths: &Paths, value: &Descriptor) -> Result<()> {
    crate::state::atomic_json(&paths.descriptor, value)
}
fn text(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("APFS vault paths must be UTF-8")?
        .into())
}
fn device(value: &str) -> Result<()> {
    let suffix = value
        .strip_prefix("/dev/disk")
        .context("invalid APFS image device")?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        bail!("invalid APFS image device");
    }
    Ok(())
}
impl System {
    pub(super) fn capacity(home: &Path) -> Result<Option<u64>> {
        let Some(paths) = paths(home, false)? else {
            return Ok(None);
        };
        Ok(descriptor(&paths)?.map(|d| d.capacity_bytes))
    }
    pub(super) fn validate_capacity(home: &Path, max_bytes: u64) -> Result<()> {
        capacity_value(max_bytes)?;
        if Self::capacity(home)?.is_some_and(|actual| actual != max_bytes) {
            bail!("APFS vault has a fixed capacity; changing its configured size requires an explicit vault migration");
        }
        Ok(())
    }
    fn run(
        &self,
        program: &'static str,
        args: Vec<String>,
        input: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let result = (self.execute)(Request {
            program,
            args,
            input,
        })?;
        if result.code != Some(0) {
            bail!(
                "APFS image command failed: {}",
                String::from_utf8_lossy(&result.stderr)
                    .chars()
                    .take(1000)
                    .collect::<String>()
            );
        }
        Ok(result.stdout)
    }
    fn json(&self, program: &'static str, args: Vec<String>) -> Result<Value> {
        let raw = self.run(program, args, None)?;
        let json = self.run(
            "/usr/bin/plutil",
            vec![
                "-convert".into(),
                "json".into(),
                "-o".into(),
                "-".into(),
                "-".into(),
            ],
            Some(raw),
        )?;
        Ok(serde_json::from_slice(&json)?)
    }
    fn family(&mut self) -> Result<Family> {
        if let Some(value) = self.family {
            return Ok(value);
        }
        let version = self.run("/usr/bin/sw_vers", vec!["-productVersion".into()], None)?;
        let major: u32 = std::str::from_utf8(&version)?
            .trim()
            .split('.')
            .next()
            .context("macOS version missing")?
            .parse()?;
        if major < 13 {
            bail!("APFS checkpoint vault requires macOS 13 or later");
        }
        let family = if major >= 27 {
            let help = self.run(
                "/usr/sbin/diskutil",
                vec!["image".into(), "attach".into(), "--help".into()],
                None,
            )?;
            if !["--mountPoint", "--readOnly", "--mountOptions"]
                .iter()
                .all(|flag| String::from_utf8_lossy(&help).contains(flag))
            {
                bail!("macOS disk image API does not advertise the required private attachment options");
            }
            Family::Diskutil
        } else {
            Family::Hdiutil
        };
        self.family = Some(family);
        Ok(family)
    }
    fn attached(&self, paths: &Paths) -> Result<Option<(String, String)>> {
        let value = self.json("/usr/bin/hdiutil", vec!["info".into(), "-plist".into()])?;
        let images = value["images"]
            .as_array()
            .context("disk image attachment list missing")?;
        let found = images
            .iter()
            .filter(|row| row["image-path"].as_str() == paths.image.to_str())
            .collect::<Vec<_>>();
        if found.len() > 1 {
            bail!("APFS vault has ambiguous image attachments");
        }
        let Some(image) = found.first() else {
            return Ok(None);
        };
        let entities = image["system-entities"]
            .as_array()
            .context("disk image devices missing")?;
        let root = entities
            .iter()
            .filter_map(|row| row["dev-entry"].as_str())
            .find(|name| {
                name.strip_prefix("/dev/disk")
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            })
            .context("whole disk image device missing")?;
        device(root)?;
        let mounted = entities
            .iter()
            .filter(|row| row["mount-point"].as_str().is_some_and(|s| !s.is_empty()))
            .collect::<Vec<_>>();
        if mounted.is_empty() {
            return Ok(Some((root.into(), String::new())));
        }
        if mounted.len() != 1 || mounted[0]["mount-point"].as_str() != paths.mount.to_str() {
            bail!("APFS vault image is mounted at an unexpected or ambiguous location");
        }
        let volume = mounted[0]["dev-entry"]
            .as_str()
            .context("mounted APFS image device missing")?;
        device(volume)?;
        Ok(Some((root.into(), volume.into())))
    }
    fn attach(&mut self, paths: &Paths, readonly: bool) -> Result<()> {
        let (program, mut args) = match self.family()? {
            Family::Diskutil => (
                "/usr/sbin/diskutil",
                vec![
                    "image".into(),
                    "attach".into(),
                    "--plist".into(),
                    "--nobrowse".into(),
                    "--mountOptions".into(),
                    "owners".into(),
                    "--mountPoint".into(),
                    text(&paths.mount)?,
                ],
            ),
            Family::Hdiutil => (
                "/usr/bin/hdiutil",
                vec![
                    "attach".into(),
                    "-plist".into(),
                    "-nobrowse".into(),
                    "-noautoopen".into(),
                    "-owners".into(),
                    "on".into(),
                    "-mountpoint".into(),
                    text(&paths.mount)?,
                ],
            ),
        };
        if readonly {
            args.push(
                if program == "/usr/sbin/diskutil" {
                    "--readOnly"
                } else {
                    "-readonly"
                }
                .into(),
            );
        }
        args.push(text(&paths.image)?);
        self.run(program, args, None)?;
        Ok(())
    }
    fn create_image(&mut self, paths: &Paths, value: &Descriptor) -> Result<()> {
        let label = format!("RDriveVault-{}", value.instance_id);
        // The modern creator has no UID/GID options. Explicit ownership is
        // required before trusting a writable volume, including on macOS 27.
        let args = vec![
            "create".into(),
            "-sectors".into(),
            (value.capacity_bytes / 512).to_string(),
            "-type".into(),
            "UDIF".into(),
            "-fs".into(),
            "APFS".into(),
            "-volname".into(),
            label,
            "-uid".into(),
            unsafe { libc::geteuid() }.to_string(),
            "-gid".into(),
            unsafe { libc::getegid() }.to_string(),
            "-mode".into(),
            "0700".into(),
            "-nospotlight".into(),
            text(&paths.image)?,
        ];
        self.run("/usr/bin/hdiutil", args, None)?;
        Ok(())
    }
    fn verify(
        &self,
        paths: &Paths,
        value: &Descriptor,
        root_device: &str,
        volume_device: &str,
        initializing: bool,
    ) -> Result<Mounted> {
        let info = self.json(
            "/usr/sbin/diskutil",
            vec!["info".into(), "-plist".into(), volume_device.into()],
        )?;
        let uuid = info["VolumeUUID"]
            .as_str()
            .context("APFS vault volume identity missing")?;
        uuid::Uuid::parse_str(uuid)?;
        if info["FilesystemType"] != "apfs"
            || info["APFSSnapshot"] == true
            || info["MountPoint"].as_str() != paths.mount.to_str()
            || info["DeviceIdentifier"].as_str() != volume_device.strip_prefix("/dev/")
            || info["VolumeName"] != format!("RDriveVault-{}", value.instance_id)
            || value
                .volume_uuid
                .as_ref()
                .is_some_and(|expected| !expected.eq_ignore_ascii_case(uuid))
        {
            bail!("mounted image is not the recorded private APFS vault volume");
        }
        let stats = (self.statistics)(&paths.mount)?;
        if stats.total > value.capacity_bytes || stats.total == 0 {
            bail!("mounted APFS image capacity exceeds its bound");
        }
        let meta = fs::symlink_metadata(&paths.mount)?;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != unsafe { libc::geteuid() }
        {
            bail!(
                "APFS vault mount root is not owned by this user (observed UID {})",
                meta.uid()
            );
        }
        if initializing && !stats.readonly {
            File::open(&paths.mount)?.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        private(&paths.mount, true)?;
        let expected = Descriptor {
            volume_uuid: Some(uuid.into()),
            ready: true,
            ..value.clone()
        };
        let marker = paths.mount.join(".rdrive-vault.json");
        if initializing
            && fs::symlink_metadata(&marker)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            crate::state::atomic_json(&marker, &expected)?;
        }
        private(&marker, false)?;
        if fs::metadata(&marker)?.len() > 16 * 1024
            || serde_json::from_slice::<Descriptor>(&fs::read(&marker)?)? != expected
        {
            bail!("APFS vault interior identity does not match its descriptor");
        }
        if initializing {
            File::open(&marker)?.sync_all()?;
            File::open(&paths.mount)?.sync_all()?;
        }
        Ok(Mounted {
            root: paths.mount.clone(),
            instance_id: value.instance_id.clone(),
            volume_uuid: uuid.into(),
            capacity_bytes: value.capacity_bytes,
            home_binding: value.home_binding.clone(),
            device: root_device.into(),
            readonly: stats.readonly,
        })
    }
    fn detach_exact(&self, paths: &Paths, expected: &str) -> Result<()> {
        if let Some((device, _)) = self.attached(paths)? {
            if device != expected {
                bail!("APFS vault device changed before detach");
            }
            self.run("/usr/bin/hdiutil", vec!["detach".into(), device], None)?;
        }
        if self.attached(paths)?.is_some() {
            bail!("APFS vault image remained attached after detach");
        }
        // Detach flushes the inner filesystem; persist its host-file blocks
        // and registry directory before the caller confirms a checkpoint.
        File::open(&paths.image)?.sync_all()?;
        File::open(&paths.vault)?.sync_all()?;
        Ok(())
    }
    fn existing(&self, mount: &Mounted) -> Result<(Paths, Descriptor, String)> {
        let home = mount
            .root
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("invalid APFS vault mount path")?;
        let paths = paths(home, false)?.context("APFS vault descriptor disappeared")?;
        if paths.mount != mount.root {
            bail!("APFS vault mount path changed");
        }
        let value = descriptor(&paths)?.context("APFS vault descriptor missing")?;
        if !value.ready
            || value.instance_id != mount.instance_id
            || value.capacity_bytes != mount.capacity_bytes
            || value.home_binding != mount.home_binding
            || value.volume_uuid.as_deref() != Some(&mount.volume_uuid)
        {
            bail!("APFS vault handle identity changed");
        }
        let (device, volume) = self
            .attached(&paths)?
            .context("APFS vault image is no longer attached")?;
        if device != mount.device {
            bail!("APFS vault handle device changed");
        }
        Ok((paths, value, volume))
    }
}
impl Host for System {
    fn mount(
        &mut self,
        home: &Path,
        max_bytes: u64,
        initialize: bool,
        readonly: bool,
    ) -> Result<Option<Mounted>> {
        if !(self.supported)() {
            bail!("APFS checkpoint vault requires macOS");
        }
        capacity_value(max_bytes)?;
        if initialize && readonly {
            bail!("cannot initialize a read-only APFS vault");
        }
        let Some(paths) = paths(home, initialize)? else {
            return Ok(None);
        };
        let mut value = match descriptor(&paths)? {
            Some(value) => value,
            None if !initialize => return Ok(None),
            None => {
                let value = Descriptor {
                    version: 1,
                    home_binding: home_binding(home)?,
                    instance_id: uuid::Uuid::new_v4().to_string(),
                    capacity_bytes: max_bytes,
                    image: None,
                    volume_uuid: None,
                    ready: false,
                };
                save(&paths, &value)?;
                value
            }
        };
        if value.capacity_bytes != max_bytes {
            bail!("APFS vault fixed capacity differs from configuration; an explicit migration is required");
        }
        if !value.ready && !initialize {
            bail!(
                "APFS vault initialization is incomplete; retry checkpoint creation to recover it"
            );
        }
        if value.image.is_none() {
            if fs::symlink_metadata(&paths.image)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                self.create_image(&paths, &value)?;
            }
            value.image = Some(regular_identity(&paths.image, true)?);
            if fs::metadata(&paths.image)?.len() != value.capacity_bytes {
                bail!("new APFS vault image does not have the exact configured size");
            }
            File::open(&paths.image)?.sync_all()?;
            save(&paths, &value)?;
        }
        let mut before = self.attached(&paths)?;
        if let Some((root, volume)) = &before {
            if volume.is_empty() {
                self.detach_exact(&paths, root)?;
                before = None;
            }
        }
        let mut touched = false;
        let operation = (|| -> Result<Mounted> {
            if before.is_none() {
                directory(&paths.mount)?;
                unmounted_directory(&paths.mount, &paths.vault)?;
                touched = true;
                self.attach(&paths, readonly)?;
            }
            let (root, volume) = self
                .attached(&paths)?
                .context("new APFS image attachment was not observed")?;
            let mut mount = self.verify(&paths, &value, &root, &volume, !value.ready)?;
            if mount.readonly != readonly {
                self.detach_exact(&paths, &root)?;
                directory(&paths.mount)?;
                unmounted_directory(&paths.mount, &paths.vault)?;
                touched = true;
                self.attach(&paths, readonly)?;
                let (root, volume) = self
                    .attached(&paths)?
                    .context("APFS vault remount was not observed")?;
                mount = self.verify(&paths, &value, &root, &volume, !value.ready)?;
            }
            if mount.readonly != readonly {
                bail!("APFS vault mount did not honor the requested access mode");
            }
            if !value.ready {
                value.volume_uuid = Some(mount.volume_uuid.clone());
                value.ready = true;
                save(&paths, &value)?;
            }
            Ok(mount)
        })();
        match operation {
            Ok(mount) => Ok(Some(mount)),
            Err(error) => {
                if touched {
                    let cleanup = (|| -> Result<()> {
                        if let Some((root, _)) = self.attached(&paths)? {
                            self.detach_exact(&paths, &root)?;
                        }
                        Ok(())
                    })();
                    if let Err(cleanup) = cleanup {
                        return Err(
                            error.context(format!("owned image cleanup also failed: {cleanup:#}"))
                        );
                    }
                }
                Err(error)
            }
        }
    }
    fn space(&mut self, mount: &Mounted) -> Result<Space> {
        let (paths, value, volume) = self.existing(mount)?;
        let verified = self.verify(&paths, &value, &mount.device, &volume, false)?;
        if verified.readonly != mount.readonly {
            bail!("APFS vault mount access mode changed");
        }
        let stats = (self.statistics)(&mount.root)?;
        Ok(Space {
            used_bytes: stats.used,
            available_bytes: stats.available,
        })
    }
    fn unmount(&mut self, mount: &Mounted) -> Result<()> {
        let (paths, value, volume) = self.existing(mount)?;
        self.verify(&paths, &value, &mount.device, &volume, false)?;
        self.detach_exact(&paths, &mount.device)
    }
}

fn statistics(path: &Path) -> Result<Statistics> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), value.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let value = unsafe { value.assume_init() };
    block_statistics(
        value.f_frsize as u64,
        value.f_blocks as u64,
        value.f_bfree as u64,
        value.f_bavail as u64,
        value.f_flag & libc::ST_RDONLY as u64 != 0,
    )
}
fn block_statistics(
    size: u64,
    blocks: u64,
    free: u64,
    available: u64,
    readonly: bool,
) -> Result<Statistics> {
    if size == 0 || free > blocks || available > free {
        bail!("invalid APFS vault filesystem space statistics");
    }
    Ok(Statistics {
        total: blocks.checked_mul(size).context("APFS capacity overflow")?,
        used: (blocks - free)
            .checked_mul(size)
            .context("APFS usage overflow")?,
        available: available
            .checked_mul(size)
            .context("APFS available capacity overflow")?,
        readonly,
    })
}
fn nonblocking(fd: i32) -> Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn drain<R: Read>(source: &mut R, buffer: &mut Vec<u8>) -> Result<bool> {
    let mut bytes = [0u8; 8192];
    match source.read(&mut bytes) {
        Ok(0) => Ok(true),
        Ok(count) => {
            if buffer.len().saturating_add(count) > MAX_OUTPUT {
                bail!("APFS tool output exceeded its bound");
            }
            buffer.extend_from_slice(&bytes[..count]);
            Ok(false)
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}
fn execute(request: Request) -> Result<Output> {
    execute_with(request, Duration::from_secs(120))
}
fn feed<W: Write>(stream: &mut W, bytes: &[u8]) -> Result<usize> {
    match stream.write(bytes) {
        Ok(0) => bail!("APFS converter input pipe closed"),
        Ok(count) => Ok(count),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::Interrupted =>
        {
            Ok(0)
        }
        Err(error) => Err(error.into()),
    }
}
fn execute_with(request: Request, timeout: Duration) -> Result<Output> {
    let mut command = Command::new(request.program);
    command
        .args(&request.args)
        .process_group(0)
        .env("LC_ALL", "C")
        .stdin(if request.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("cannot execute the fixed APFS image tool")?;
    let result = (|| -> Result<Output> {
        let mut stdout = child
            .stdout
            .take()
            .context("APFS tool output pipe missing")?;
        let mut stderr = child
            .stderr
            .take()
            .context("APFS tool error pipe missing")?;
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let mut stdin = child.stdin.take();
        if let Some(stream) = &stdin {
            nonblocking(stream.as_raw_fd())?;
        }
        let input = request.input.unwrap_or_default();
        if input.len() > MAX_OUTPUT {
            bail!("APFS converter input exceeded its bound");
        }
        let (mut out, mut err, mut sent, mut out_end, mut err_end) =
            (vec![], vec![], 0, false, false);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(stream) = stdin.as_mut() {
                if sent < input.len() {
                    sent += feed(stream, &input[sent..])?;
                }
                if sent == input.len() {
                    stdin.take();
                }
            }
            if !out_end {
                out_end = drain(&mut stdout, &mut out)?;
            }
            if !err_end {
                err_end = drain(&mut stderr, &mut err)?;
            }
            if let Some(status) = child.try_wait()? {
                if out_end && err_end {
                    return Ok(Output {
                        code: status.code(),
                        stdout: out,
                        stderr: err,
                    });
                }
            }
            if Instant::now() >= deadline {
                bail!("APFS image command timed out; no successful operation was assumed");
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    // Kill the private process group even after the direct child exits: a
    // descendant may still hold a pipe open. Always reap the child before return.
    let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
