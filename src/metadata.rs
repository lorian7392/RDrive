//! Portable snapshots of selected user xattrs, including opaque Finder tag plists.
//! A private recovery journal supplies rollback for multi-attribute updates;
//! macOS itself does not provide an atomic multi-xattr transaction.
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::{
    fd::AsRawFd,
    unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

pub const FINDER_TAGS: &str = "com.apple.metadata:_kMDItemUserTags";
pub const FINDER_INFO: &str = "com.apple.FinderInfo";
const MAX_VALUE: usize = 1024 * 1024;
const MAX_TOTAL: usize = 4 * 1024 * 1024;
const MAX_NAMES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataPolicy {
    pub include: Vec<String>,
}
impl Default for MetadataPolicy {
    fn default() -> Self {
        Self {
            include: vec![
                FINDER_INFO.into(),
                "com.apple.metadata:*".into(),
                "user.*".into(),
            ],
        }
    }
}
impl MetadataPolicy {
    pub fn validate(&self) -> Result<()> {
        validate_selection(&self.include)
    }
    pub fn allows(&self, name: &str) -> bool {
        valid_name(name) && !protected(name) && selected(name, &self.include)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExcludedAttribute {
    pub name: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataRecord {
    pub version: u32,
    pub content_sha256: String,
    /// Values are lower-case hexadecimal; binary plists are preserved verbatim.
    pub attributes: BTreeMap<String, String>,
    /// Complete source selection. Absent attributes in this selection are cleared
    /// on apply, within the receiver's allowed policy only.
    pub selection: Vec<String>,
    pub excluded: Vec<ExcludedAttribute>,
}
impl MetadataRecord {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !valid_hash(&self.content_sha256)
            || self.attributes.len() > MAX_NAMES
            || self.excluded.len() > MAX_NAMES
            || self
                .excluded
                .iter()
                .any(|item| item.name.len() > 1024 || item.reason.len() > 512)
        {
            bail!("invalid metadata snapshot");
        }
        validate_selection(&self.selection)?;
        let mut total = 0usize;
        for (name, value) in &self.attributes {
            if !valid_name(name) || protected(name) || !selected(name, &self.selection) {
                bail!("metadata snapshot contains a protected or unselected attribute");
            }
            if value.len() > MAX_VALUE * 2
                || value.len() % 2 != 0
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("invalid or oversized hexadecimal metadata value");
            }
            total = total
                .checked_add(value.len() / 2)
                .context("metadata size overflow")?;
            if total > MAX_TOTAL {
                bail!("metadata snapshot exceeds4MiB");
            }
        }
        Ok(())
    }
    /// Omits excluded attributes and content hash, so unrelated quarantine or
    /// security metadata does not create false metadata synchronization changes.
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        let mut selection = self.selection.clone();
        selection.sort();
        selection.dedup();
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(selection, &self.attributes))?)
        ))
    }
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<ExcludedAttribute>,
    pub rolled_back: bool,
    pub recovery_journal: Option<PathBuf>,
}

// File effects stay behind a narrow boundary so recovery can be tested for
// syscall failures and concurrent changes without corrupting real files.
type FileStamp = (u64, i64, i64, i64, i64);
trait MetadataIo {
    fn snapshot_stamp(&self) -> Result<FileStamp>;
    fn snapshot_hash(&self) -> Result<String>;
    fn names(&self) -> Result<Vec<String>>;
    fn read_attribute(&self, name: &str) -> Result<Option<Vec<u8>>>;
    fn write_attribute(&self, name: &str, value: Option<&[u8]>) -> Result<()>;
    fn file_identity(&self) -> Result<String>;
    fn flush(&self) -> std::io::Result<()>;
}
impl MetadataIo for File {
    fn snapshot_stamp(&self) -> Result<FileStamp> { stamp(self) }
    fn snapshot_hash(&self) -> Result<String> { hash_file(self) }
    fn names(&self) -> Result<Vec<String>> { list(self) }
    fn read_attribute(&self, name: &str) -> Result<Option<Vec<u8>>> { get(self, name) }
    fn write_attribute(&self, name: &str, value: Option<&[u8]>) -> Result<()> { set_or_remove(self, name, value) }
    fn file_identity(&self) -> Result<String> { identity(self) }
    fn flush(&self) -> std::io::Result<()> { self.sync_all() }
}

pub fn capture(path: &Path, policy: &MetadataPolicy) -> Result<MetadataRecord> {
    capture_file(&open_regular(path)?, policy)
}
pub fn capture_file(file: &File, policy: &MetadataPolicy) -> Result<MetadataRecord> {
    capture_with_io(file, policy)
}
fn capture_with_io(file: &impl MetadataIo, policy: &MetadataPolicy) -> Result<MetadataRecord> {
    policy.validate()?;
    let before = file.snapshot_stamp()?;
    let mut attributes = BTreeMap::new();
    let mut excluded = Vec::new();
    let mut total = 0usize;
    for name in file.names()? {
        if !policy.allows(&name) {
            excluded.push(ExcludedAttribute {
                name: name.clone(),
                reason: if protected(&name) {
                    "security, resource, or system metadata is never copied"
                } else {
                    "not selected by metadata policy"
                }
                .into(),
            });
            continue;
        }
        let value =
            file.read_attribute(&name)?.context("metadata changed while capturing; retry the snapshot")?;
        total = total
            .checked_add(value.len())
            .context("metadata size overflow")?;
        if total > MAX_TOTAL {
            bail!("selected metadata exceeds4MiB; no partial snapshot was returned");
        }
        attributes.insert(name, hex(&value));
    }
    let content_sha256 = file.snapshot_hash()?;
    if before != file.snapshot_stamp()? {
        bail!("file or metadata changed while capturing; no partial snapshot was returned");
    }
    let mut selection = policy.include.clone();
    selection.sort();
    selection.dedup();
    let record = MetadataRecord {
        version: 1,
        content_sha256,
        attributes,
        selection,
        excluded,
    };
    record.validate()?;
    Ok(record)
}

/// For application integration prefer `apply_with_recovery` with a journal in
/// persistent private application state. This convenience API uses a private
/// temporary directory and retains it if automatic rollback cannot finish.
pub fn apply(path: &Path, record: &MetadataRecord, policy: &MetadataPolicy) -> Result<ApplyReport> {
    apply_file(&open_regular(path)?, record, policy)
}
pub fn apply_file(
    file: &File,
    record: &MetadataRecord,
    policy: &MetadataPolicy,
) -> Result<ApplyReport> {
    let directory = std::env::temp_dir()
        .canonicalize()?
        .join(format!("rdrive-metadata-recovery-{}", uuid::Uuid::new_v4()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&directory)?;
    let journal = directory.join("attributes.json");
    let result = apply_file_with_recovery(file, record, policy, &journal);
    if !journal.exists() {
        let _ = fs::remove_dir(directory);
    }
    result
}
pub fn apply_with_recovery(
    path: &Path,
    record: &MetadataRecord,
    policy: &MetadataPolicy,
    journal: &Path,
) -> Result<ApplyReport> {
    apply_file_with_recovery(&open_regular(path)?, record, policy, journal)
}
pub fn apply_file_with_recovery(
    file: &File,
    record: &MetadataRecord,
    policy: &MetadataPolicy,
    journal: &Path,
) -> Result<ApplyReport> {
    apply_with_io(file, record, policy, journal)
}
fn apply_with_io(file: &impl MetadataIo, record: &MetadataRecord, policy: &MetadataPolicy, journal: &Path) -> Result<ApplyReport> {
    record.validate()?;
    policy.validate()?;
    private_journal_parent(journal)?;
    if journal.symlink_metadata().is_ok() {
        bail!("metadata recovery journal already exists; recover it before a new update");
    }
    let before_stamp = file.snapshot_stamp()?;
    if file.snapshot_hash()? != record.content_sha256 {
        bail!("metadata content hash does not match target; attributes were preserved");
    }
    let mut desired = BTreeMap::<String, Option<Vec<u8>>>::new();
    let mut report = ApplyReport {
        skipped: record.excluded.clone(),
        ..ApplyReport::default()
    };
    for name in file.names()? {
        if policy.allows(&name) && selected(&name, &record.selection) {
            desired.insert(name, None);
        }
    }
    for (name, value) in &record.attributes {
        if policy.allows(name) {
            desired.insert(name.clone(), Some(unhex(value)?));
        } else {
            report.skipped.push(ExcludedAttribute {
                name: name.clone(),
                reason: "not permitted by receiving metadata policy".into(),
            });
        }
    }
    let mut previous = BTreeMap::new();
    let mut total = 0usize;
    for name in desired.keys() {
        let value = file.read_attribute(name)?;
        total = total
            .checked_add(value.as_ref().map_or(0, Vec::len))
            .context("metadata backup size overflow")?;
        if total > MAX_TOTAL {
            bail!("existing selected metadata exceeds4MiB; attributes were preserved");
        }
        previous.insert(name.clone(), value);
    }
    if before_stamp != file.snapshot_stamp()? {
        bail!("file or metadata changed during preparation; attributes were preserved");
    }
    let recovery = Recovery {
        version: 1,
        identity: file.file_identity()?,
        content_sha256: record.content_sha256.clone(),
        phase: "prepared".into(),
        previous: previous
            .iter()
            .map(|(name, value)| (name.clone(), value.as_ref().map(|value| hex(value))))
            .collect(),
    };
    crate::state::atomic_json(journal, &recovery)?;
    let applied = update_with_rollback(&previous, &desired, |name, value| {
        file.write_attribute(name, value)
    });
    if let Err(failure) = applied {
        if failure.rollback_failures.is_empty() {
            file.flush().with_context(|| {
                format!(
                    "metadata rollback could not be flushed; recovery journal retained at {}",
                    journal.display()
                )
            })?;
            remove_journal(journal)?;
        }
        return Err(anyhow!(
            "metadata update failed: {}; rollback {}; recovery journal: {}",
            failure.cause,
            if failure.rollback_failures.is_empty() {
                "completed".into()
            } else {
                format!("incomplete for {:?}", failure.rollback_failures)
            },
            journal.display()
        ));
    }
    if file.snapshot_hash()? != record.content_sha256 {
        let failures = restore(file, &previous);
        if failures.is_empty() {
            file.flush().with_context(|| {
                format!(
                    "metadata rollback could not be flushed; recovery journal retained at {}",
                    journal.display()
                )
            })?;
            remove_journal(journal)?;
        }
        bail!("file content changed during metadata update; prior attributes restored where possible; recovery journal: {}",journal.display());
    }
    if let Err(error) = file.flush() {
        let failures = restore(file, &previous);
        if failures.is_empty() && file.flush().is_ok() {
            remove_journal(journal)?;
        }
        return Err(error).context(format!(
            "metadata flush failed; recovery journal: {}",
            journal.display()
        ));
    }
    let mut committed = recovery;
    committed.phase = "complete".into();
    crate::state::atomic_json(journal, &committed)?;
    for (name, value) in &desired {
        if previous.get(name) == Some(value) {
            continue;
        }
        if value.is_some() {
            report.applied.push(name.clone());
        } else {
            report.removed.push(name.clone());
        }
    }
    remove_journal(journal)?;
    Ok(report)
}

#[derive(Debug, Serialize, Deserialize)]
struct Recovery {
    version: u32,
    identity: String,
    content_sha256: String,
    phase: String,
    previous: BTreeMap<String, Option<String>>,
}
pub fn recover(path: &Path, journal: &Path) -> Result<ApplyReport> {
    recover_file(&open_regular(path)?, journal)
}
pub fn recover_file(file: &File, journal: &Path) -> Result<ApplyReport> {
    recover_with_io(file, journal)
}
fn recover_with_io(file: &impl MetadataIo, journal: &Path) -> Result<ApplyReport> {
    private_journal_parent(journal)?;
    if journal.symlink_metadata()?.file_type().is_symlink() {
        bail!("recovery journal cannot be a symlink");
    }
    let bytes = fs::read(journal)?;
    if bytes.len() > MAX_TOTAL * 2 + 1024 * 1024 {
        bail!("oversized metadata recovery journal");
    }
    let recovery: Recovery = serde_json::from_slice(&bytes)?;
    if recovery.version != 1
        || recovery.identity != file.file_identity()?
        || !valid_hash(&recovery.content_sha256)
        || !["prepared", "complete"].contains(&recovery.phase.as_str())
        || recovery.previous.len() > MAX_NAMES * 2
    {
        bail!("metadata journal belongs to a different file or unsupported format");
    }
    if recovery.phase == "complete" {
        remove_journal(journal)?;
        return Ok(ApplyReport::default());
    }
    if file.snapshot_hash()? != recovery.content_sha256 {
        bail!("recovery target content changed; journal and attributes were preserved");
    }
    let mut previous = BTreeMap::new();
    let mut total = 0;
    for (name, value) in &recovery.previous {
        if !valid_name(name) || protected(name) {
            bail!("recovery journal contains protected metadata");
        }
        let value = value.as_deref().map(unhex).transpose()?;
        total += value.as_ref().map_or(0, Vec::len);
        if total > MAX_TOTAL {
            bail!("metadata recovery exceeds4MiB");
        }
        previous.insert(name.clone(), value);
    }
    let failures = restore(file, &previous);
    if !failures.is_empty() {
        bail!(
            "metadata rollback incomplete for {:?}; journal retained at {}",
            failures,
            journal.display()
        );
    }
    file.flush()?;
    remove_journal(journal)?;
    Ok(ApplyReport {
        rolled_back: true,
        ..ApplyReport::default()
    })
}
struct UpdateFailure {
    cause: anyhow::Error,
    rollback_failures: Vec<String>,
}
fn update_with_rollback(
    previous: &BTreeMap<String, Option<Vec<u8>>>,
    desired: &BTreeMap<String, Option<Vec<u8>>>,
    mut setter: impl FnMut(&str, Option<&[u8]>) -> Result<()>,
) -> std::result::Result<(), UpdateFailure> {
    for (name, value) in desired {
        if previous.get(name) == Some(value) {
            continue;
        }
        if let Err(cause) = setter(name, value.as_deref()) {
            let rollback_failures = previous
                .iter()
                .filter_map(|(name, value)| {
                    setter(name, value.as_deref()).err().map(|_| name.clone())
                })
                .collect();
            return Err(UpdateFailure {
                cause,
                rollback_failures,
            });
        }
    }
    Ok(())
}
fn restore(file: &impl MetadataIo, previous: &BTreeMap<String, Option<Vec<u8>>>) -> Vec<String> {
    previous
        .iter()
        .filter_map(|(name, value)| {
            file.write_attribute(name, value.as_deref())
                .err()
                .map(|_| name.clone())
        })
        .collect()
}
fn remove_journal(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    File::open(path.parent().context("journal needs parent")?)?.sync_all()?;
    Ok(())
}
fn private_journal_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("journal requires a private parent directory")?;
    let meta = fs::symlink_metadata(parent)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("metadata journal parent must be a real directory");
    }
    #[cfg(unix)]
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        bail!("metadata recovery directory must be private and owned by current user");
    }
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("metadata target must be a regular file");
    }
    Ok(file)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains('\0')
        && !name.chars().any(char::is_control)
}
fn protected(name: &str) -> bool {
    matches!(
        name,
        "com.apple.quarantine"
            | "com.apple.ResourceFork"
            | "com.apple.decmpfs"
            | "com.apple.provenance"
            | "com.apple.macl"
            | "com.apple.rootless"
    ) || [
        "security.",
        "system.",
        "trusted.",
        "com.apple.system.",
        "com.apple.cs.",
        "com.apple.fileprovider.",
        "com.apple.icloud.",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}
fn selected(name: &str, selection: &[String]) -> bool {
    selection.iter().any(|pattern| {
        pattern
            .strip_suffix('*')
            .map_or(name == pattern, |prefix| name.starts_with(prefix))
    })
}
fn validate_selection(selection: &[String]) -> Result<()> {
    if selection.len() > 128
        || selection.iter().any(|pattern| {
            !valid_name(pattern)
                || pattern[..pattern.len() - usize::from(pattern.ends_with('*'))].contains('*')
        })
    {
        bail!("invalid metadata selection; only a final wildcard is supported");
    }
    Ok(())
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn hex(value: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}
fn unhex(value: &str) -> Result<Vec<u8>> {
    if value.len() > MAX_VALUE * 2 || value.len() % 2 != 0 {
        bail!("invalid metadata hexadecimal length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let a = (pair[0] as char)
                .to_digit(16)
                .context("invalid metadata hexadecimal value")?;
            let b = (pair[1] as char)
                .to_digit(16)
                .context("invalid metadata hexadecimal value")?;
            Ok((a * 16 + b) as u8)
        })
        .collect()
}
#[cfg(unix)]
fn identity(file: &File) -> Result<String> {
    let m = file.metadata()?;
    Ok(format!("{}:{}", m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn identity(_: &File) -> Result<String> {
    bail!("metadata requires a supported Unix filesystem")
}
#[cfg(unix)]
fn stamp(file: &File) -> Result<(u64, i64, i64, i64, i64)> {
    let m = file.metadata()?;
    if !m.is_file() {
        bail!("metadata target is not a regular file");
    }
    Ok((
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
#[cfg(not(unix))]
fn stamp(_: &File) -> Result<(u64, i64, i64, i64, i64)> {
    bail!("metadata requires a supported Unix filesystem")
}
#[cfg(unix)]
fn hash_file(file: &File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read_at(&mut buffer, offset)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}
#[cfg(not(unix))]
fn hash_file(_: &File) -> Result<String> {
    bail!("metadata requires a supported Unix filesystem")
}

fn list(file: &File) -> Result<Vec<String>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    { list_with(|buffer| native_list(file, buffer)) }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { let _ = file; bail!("xattrs are unsupported on this platform") }
}

// The two reads can fail independently or disagree if another process changes
// attributes. Keep length validation in the shared path exercised by mocks.
fn list_with(mut read: impl FnMut(Option<&mut [u8]>) -> std::io::Result<usize>) -> Result<Vec<String>> {
    let length = read(None).context("cannot enumerate file xattrs")?;
    if length > 256 * 1024 { bail!("xattr name list exceeds256KiB"); }
    let mut bytes = vec![0; length];
    let actual = read(Some(&mut bytes)).context("file xattr list changed")?;
    if actual > bytes.len() { bail!("file xattr list grew while reading; no snapshot was returned"); }
    bytes.truncate(actual);
    let mut names = BTreeSet::new();
    for name in bytes.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        names.insert(std::str::from_utf8(name).context("non-UTF8 xattr name cannot be represented portably")?.to_owned());
    }
    if names.len() > MAX_NAMES { bail!("file has more than1024 xattrs"); }
    Ok(names.into_iter().collect())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_list(file: &File, buffer: Option<&mut [u8]>) -> std::io::Result<usize> {
    let (pointer,length) = buffer.map_or((std::ptr::null_mut(),0), |bytes|(bytes.as_mut_ptr(),bytes.len()));
    #[cfg(target_os = "macos")]
    let result = unsafe {libc::flistxattr(file.as_raw_fd(),pointer.cast(),length,0)};
    #[cfg(target_os = "linux")]
    let result = unsafe {libc::flistxattr(file.as_raw_fd(),pointer.cast(),length)};
    if result < 0 {Err(std::io::Error::last_os_error())} else {Ok(result as usize)}
}

fn get(file: &File, name: &str) -> Result<Option<Vec<u8>>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = std::ffi::CString::new(name)?;
        get_with(|buffer| native_get(file, &name, buffer))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { let _ = (file,name); bail!("xattrs are unsupported on this platform") }
}
fn get_with(mut read: impl FnMut(Option<&mut [u8]>) -> std::io::Result<usize>) -> Result<Option<Vec<u8>>> {
    let length = match read(None) {
        Ok(length) => length,
        Err(error) if missing_attr(&error) => return Ok(None),
        Err(error) => return Err(error).context("cannot read selected xattr"),
    };
    if length > MAX_VALUE { bail!("selected xattr exceeds1MiB; no value was discarded"); }
    let mut value = vec![0;length];
    let actual = read(Some(&mut value)).context("xattr changed while reading")?;
    if actual > value.len() { bail!("xattr grew while reading; no partial value was returned"); }
    value.truncate(actual);
    Ok(Some(value))
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_get(file: &File, name: &std::ffi::CStr, buffer: Option<&mut [u8]>) -> std::io::Result<usize> {
    let (pointer,length) = buffer.map_or((std::ptr::null_mut(),0), |bytes|(bytes.as_mut_ptr(),bytes.len()));
    #[cfg(target_os = "macos")]
    let result = unsafe {libc::fgetxattr(file.as_raw_fd(),name.as_ptr(),pointer.cast(),length,0,0)};
    #[cfg(target_os = "linux")]
    let result = unsafe {libc::fgetxattr(file.as_raw_fd(),name.as_ptr(),pointer.cast(),length)};
    if result < 0 {Err(std::io::Error::last_os_error())} else {Ok(result as usize)}
}
fn set_or_remove(file: &File, name: &str, value: Option<&[u8]>) -> Result<()> {
    if protected(name) || !valid_name(name) {
        bail!("refusing protected or invalid xattr");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = std::ffi::CString::new(name)?;
        let result = match value {
            #[cfg(target_os = "macos")]
            Some(value) => unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            },
            #[cfg(target_os = "linux")]
            Some(value) => unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                )
            },
            #[cfg(target_os = "macos")]
            None => unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr(), 0) },
            #[cfg(target_os = "linux")]
            None => unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) },
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if value.is_none() && missing_attr(&error) {
                return Ok(());
            }
            return Err(error).context("cannot update selected xattr");
        }
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (file, name, value);
        bail!("xattrs are unsupported on this platform")
    }
}
fn missing_attr(error: &std::io::Error) -> bool {
    #[cfg(target_os = "macos")]
    return error.raw_os_error() == Some(libc::ENOATTR);
    #[cfg(target_os = "linux")]
    return error.raw_os_error() == Some(libc::ENODATA);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn security_and_resource_metadata_cannot_be_opted_in() {
        let p = MetadataPolicy {
            include: vec!["*".into()],
        };
        for name in [
            "com.apple.quarantine",
            "com.apple.ResourceFork",
            "com.apple.macl",
            "com.apple.cs.CodeSignature",
            "system.posix_acl_access",
        ] {
            assert!(!p.allows(name));
        }
        assert!(p.allows(FINDER_TAGS));
    }
    #[test]
    fn partial_update_failure_restores_all_previous_values() {
        let previous = BTreeMap::from([
            ("a".into(), Some(b"old-a".to_vec())),
            ("b".into(), Some(b"old-b".to_vec())),
        ]);
        let desired = BTreeMap::from([("a".into(), Some(b"new-a".to_vec())), ("b".into(), None)]);
        let mut values = previous.clone();
        let mut fail = true;
        let result = update_with_rollback(&previous, &desired, |name, value| {
            if name == "b" && fail {
                fail = false;
                bail!("simulated unsupported update");
            }
            values.insert(name.to_owned(), value.map(ToOwned::to_owned));
            Ok(())
        });
        assert!(result.is_err());
        assert!(result.err().unwrap().rollback_failures.is_empty());
        assert_eq!(values, previous);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn real_finder_tags_and_user_xattrs_roundtrip_and_clear_absent_selected_values() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let src = dir.join("source");
        let dst = dir.join("target");
        fs::write(&src, b"identical contents").unwrap();
        fs::write(&dst, b"identical contents").unwrap();
        let source = open_regular(&src).unwrap();
        let target = open_regular(&dst).unwrap();
        let tags=r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><array><string>Research&#10;4</string><string>待处理&#10;0</string></array></plist>"#.as_bytes();
        set_or_remove(&source, FINDER_TAGS, Some(tags)).unwrap();
        set_or_remove(&source, "user.rdrive.example", Some(&[0, 1, 2, 255])).unwrap();
        set_or_remove(&target, "user.rdrive.stale", Some(b"remove me")).unwrap();
        let record = capture(&src, &MetadataPolicy::default()).unwrap();
        let report = apply_with_recovery(
            &dst,
            &record,
            &MetadataPolicy::default(),
            &dir.join("journal.json"),
        )
        .unwrap();
        assert!(report.removed.contains(&"user.rdrive.stale".into()));
        assert_eq!(get(&target, FINDER_TAGS).unwrap().as_deref(), Some(tags));
        assert_eq!(
            get(&target, "user.rdrive.example").unwrap().unwrap(),
            vec![0, 1, 2, 255]
        );
        assert_eq!(
            capture(&dst, &MetadataPolicy::default())
                .unwrap()
                .fingerprint()
                .unwrap(),
            record.fingerprint().unwrap()
        );
        fs::write(&dst, b"different contents").unwrap();
        assert!(apply(&dst, &record, &MetadataPolicy::default()).is_err());
        assert_eq!(get(&target, FINDER_TAGS).unwrap().as_deref(), Some(tags));
    }

    #[cfg(unix)]
    #[test]
    fn durable_recovery_restores_prior_attrs_and_refuses_a_replaced_inode() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("file");
        fs::write(&path, b"content").unwrap();
        let file = open_regular(&path).unwrap();
        set_or_remove(&file, "user.rdrive.original", Some(b"old")).unwrap();
        let recovery = Recovery {
            version: 1,
            identity: identity(&file).unwrap(),
            content_sha256: hash_file(&file).unwrap(),
            phase: "prepared".into(),
            previous: BTreeMap::from([
                ("user.rdrive.original".into(), Some(hex(b"old"))),
                ("user.rdrive.added".into(), None),
            ]),
        };
        let journal = root.join("recovery.json");
        crate::state::atomic_json(&journal, &recovery).unwrap();
        set_or_remove(&file, "user.rdrive.original", Some(b"new")).unwrap();
        set_or_remove(&file, "user.rdrive.added", Some(b"new-only")).unwrap();
        assert!(recover_file(&file, &journal).unwrap().rolled_back);
        assert!(!journal.exists());
        assert_eq!(
            get(&file, "user.rdrive.original").unwrap().as_deref(),
            Some(b"old".as_slice())
        );
        assert!(get(&file, "user.rdrive.added").unwrap().is_none());
        crate::state::atomic_json(&journal, &recovery).unwrap();
        fs::rename(&path, root.join("previous")).unwrap();
        fs::write(&path, b"content").unwrap();
        assert!(recover(&path, &journal).is_err());
        assert!(journal.exists());
    }

    #[test]
    fn malformed_snapshots_and_selections_are_rejected_before_filesystem_changes() {
        let base = MetadataRecord { version:1, content_sha256:"a".repeat(64), attributes:BTreeMap::new(), selection:vec!["user.*".into()], excluded:vec![] };
        assert!(base.validate().is_ok());
        let mut bad = base.clone(); bad.version=2; assert!(bad.validate().is_err());
        let mut bad = base.clone(); bad.content_sha256="A".repeat(64); assert!(bad.validate().is_err());
        let mut bad = base.clone(); bad.excluded.push(ExcludedAttribute {name:"x".repeat(1025),reason:String::new()}); assert!(bad.validate().is_err());
        let mut bad = base.clone(); bad.excluded.push(ExcludedAttribute {name:"x".into(),reason:"x".repeat(513)}); assert!(bad.validate().is_err());
        let mut bad = base.clone(); bad.attributes = (0..MAX_NAMES+1).map(|i|(format!("user.{i}"),String::new())).collect(); assert!(bad.validate().is_err());
        for (name,value) in [("security.bad","00"),("unselected","00"),("user.x","0"),("user.x","gg"),("user.x","AA")] {
            let mut bad=base.clone(); bad.attributes.insert(name.into(),value.into()); assert!(bad.validate().is_err(),"{name}={value}");
        }
        let mut bad=base.clone(); bad.attributes.insert("user.large".into(),"00".repeat(MAX_VALUE+1)); assert!(bad.validate().is_err());
        let mut bad=base.clone(); for i in 0..5 { bad.attributes.insert(format!("user.{i}"),"00".repeat(MAX_VALUE)); } assert!(bad.validate().is_err());
        for selection in [vec![String::new()], vec!["user.*.x".into()], vec!["user.x".into();129],vec!["user.\0".into()]] { assert!(validate_selection(&selection).is_err()); }
        assert_eq!(unhex("00ff").unwrap(),vec![0,255]);
        for value in ["0","0g","g0"] { assert!(unhex(value).is_err()); }
        assert!(unhex(&"00".repeat(MAX_VALUE+1)).is_err());
        assert!(!valid_name(&"x".repeat(256)));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_validates_journal_security_format_content_and_total_budget() {
        use std::os::unix::fs::{symlink,PermissionsExt};
        let temporary = tempfile::tempdir().unwrap(); let root=temporary.path().canonicalize().unwrap();
        fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();
        let path=root.join("file"); fs::write(&path,b"content").unwrap(); let file=open_regular(&path).unwrap();
        let journal=root.join("recovery.json");
        let base=Recovery {version:1,identity:identity(&file).unwrap(),content_sha256:hash_file(&file).unwrap(),phase:"prepared".into(),previous:BTreeMap::new()};
        symlink(&path,&journal).unwrap(); assert!(recover_file(&file,&journal).is_err()); fs::remove_file(&journal).unwrap();
        fs::write(&journal,vec![b' ';MAX_TOTAL*2+1024*1024+1]).unwrap(); assert!(recover_file(&file,&journal).is_err()); fs::remove_file(&journal).unwrap();
        for (field,value) in [("version",serde_json::json!(2)),("identity",serde_json::json!("wrong-file")),("phase",serde_json::json!("unknown")),("content_sha256",serde_json::json!("bad"))] {
            let mut invalid=serde_json::to_value(&base).unwrap(); invalid[field]=value;
            crate::state::atomic_json(&journal,&invalid).unwrap(); assert!(recover_file(&file,&journal).is_err()); assert!(journal.exists());
        }
        let mut invalid=serde_json::to_value(&base).unwrap(); invalid["content_sha256"]=serde_json::json!("0".repeat(64));
        crate::state::atomic_json(&journal,&invalid).unwrap(); assert!(recover_file(&file,&journal).is_err());
        let mut invalid=serde_json::to_value(&base).unwrap(); invalid["previous"]=serde_json::json!({"com.apple.quarantine":"00"});
        crate::state::atomic_json(&journal,&invalid).unwrap(); assert!(recover_file(&file,&journal).is_err());
        let mut invalid=serde_json::to_value(&base).unwrap(); invalid["previous"]=serde_json::json!({"user.x":"0g"});
        crate::state::atomic_json(&journal,&invalid).unwrap(); assert!(recover_file(&file,&journal).is_err());
        let mut invalid=serde_json::to_value(&base).unwrap(); invalid["previous"]=serde_json::to_value((0..5).map(|i|(format!("user.{i}"),"00".repeat(if i==4 {1}else{MAX_VALUE}))).collect::<BTreeMap<_,_>>()).unwrap();
        crate::state::atomic_json(&journal,&invalid).unwrap(); assert!(recover_file(&file,&journal).is_err());
        let mut complete=serde_json::to_value(&base).unwrap(); complete["phase"]=serde_json::json!("complete");
        crate::state::atomic_json(&journal,&complete).unwrap(); assert!(!recover_file(&file,&journal).unwrap().rolled_back); assert!(!journal.exists());
        assert!(open_regular(&root).is_err()); assert!(stamp(&File::open(&root).unwrap()).is_err());
        assert!(set_or_remove(&file,"com.apple.quarantine",Some(b"test")).is_err());
        assert!(set_or_remove(&file,"bad\0name",None).is_err());
        assert!(get(&file,"bad\0name").is_err());
        assert_eq!(restore(&file,&BTreeMap::from([("com.apple.quarantine".into(),None)])),vec!["com.apple.quarantine"]);
        let public=root.join("public"); fs::create_dir(&public).unwrap(); fs::set_permissions(&public,fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_journal_parent(&public.join("file")).is_err());
        let alias=root.join("alias"); symlink(&root,&alias).unwrap(); assert!(private_journal_parent(&alias.join("journal")).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn receiver_selection_skips_attrs_and_existing_journal_blocks_new_update() {
        use std::os::unix::fs::PermissionsExt;
        let temporary=tempfile::tempdir().unwrap(); let root=temporary.path().canonicalize().unwrap();
        fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();
        let source=root.join("source"); let target=root.join("target"); fs::write(&source,b"same").unwrap();fs::write(&target,b"same").unwrap();
        let input=open_regular(&source).unwrap(); set_or_remove(&input,"user.hidden",Some(b"private")).unwrap();
        let narrow=MetadataPolicy {include:vec!["user.other".into()]};
        let captured=capture(&source,&narrow).unwrap(); assert!(captured.excluded.iter().any(|item|item.name=="user.hidden"&&item.reason.contains("not selected")));
        let captured=capture(&source,&MetadataPolicy::default()).unwrap();
        let applied=apply(&target,&captured,&narrow).unwrap(); assert!(applied.skipped.iter().any(|item|item.name=="user.hidden")); assert!(applied.applied.is_empty());
        let journal=root.join("existing.json"); fs::write(&journal,b"preserved").unwrap();
        assert!(apply_with_recovery(&target,&captured,&MetadataPolicy::default(),&journal).unwrap_err().to_string().contains("already exists"));
        assert_eq!(fs::read(journal).unwrap(),b"preserved");
        let null=File::open("/dev/null").unwrap();
        assert!(list(&null).is_err()); assert!(get(&null,"user.x").is_err()); assert!(set_or_remove(&null,"user.x",Some(b"value")).is_err());
        let target_file=open_regular(&target).unwrap();assert!(get(&target_file,"user.rdrive_test_missing").unwrap().is_none());
        set_or_remove(&target_file,"user.rdrive_test_missing",None).unwrap();
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    use std::{cell::{Cell,RefCell},collections::VecDeque};
    #[derive(Default)]
    struct FakeFile {
        attributes:RefCell<BTreeMap<String,Vec<u8>>>,hashes:RefCell<VecDeque<String>>,
        stamp_calls:Cell<usize>,change_stamp:Option<usize>,writes:Cell<usize>,failed_writes:BTreeSet<usize>,
        flushes:Cell<usize>,failed_flushes:BTreeSet<usize>,
    }
    impl MetadataIo for FakeFile {
        fn snapshot_stamp(&self)->Result<FileStamp> {let call=self.stamp_calls.get();self.stamp_calls.set(call+1);Ok((0,i64::from(self.change_stamp.is_some_and(|after|call>=after)),0,0,0))}
        fn snapshot_hash(&self)->Result<String> {let mut hashes=self.hashes.borrow_mut();if hashes.len()>1 {Ok(hashes.pop_front().unwrap())} else {Ok(hashes.front().cloned().unwrap_or_else(||"a".repeat(64)))}}
        fn names(&self)->Result<Vec<String>> {Ok(self.attributes.borrow().keys().cloned().collect())}
        fn read_attribute(&self,name:&str)->Result<Option<Vec<u8>>> {Ok(self.attributes.borrow().get(name).cloned())}
        fn write_attribute(&self,name:&str,value:Option<&[u8]>)->Result<()> {let call=self.writes.get()+1;self.writes.set(call);if self.failed_writes.contains(&call) {bail!("synthetic xattr failure");}if let Some(value)=value {self.attributes.borrow_mut().insert(name.into(),value.into());}else{self.attributes.borrow_mut().remove(name);}Ok(())}
        fn file_identity(&self)->Result<String> {Ok("synthetic:1".into())}
        fn flush(&self)->std::io::Result<()> {let call=self.flushes.get()+1;self.flushes.set(call);if self.failed_flushes.contains(&call){Err(std::io::Error::other("synthetic flush failure"))}else{Ok(())}}
    }
    fn record()->MetadataRecord {MetadataRecord{version:1,content_sha256:"a".repeat(64),attributes:BTreeMap::from([("user.a".into(),"01".into())]),selection:vec!["user.*".into()],excluded:vec![]}}
    fn file()->FakeFile {FakeFile{attributes:RefCell::new(BTreeMap::from([("user.a".into(),vec![0])])),..Default::default()}}
    fn directory()->tempfile::TempDir {use std::os::unix::fs::PermissionsExt;let temp=tempfile::tempdir().unwrap();fs::set_permissions(temp.path(),fs::Permissions::from_mode(0o700)).unwrap();temp}
    #[test]
    fn xattr_reads_reject_failed_or_inconsistent_lengths_without_partial_snapshots() {
        assert!(list_with(|_|Err(std::io::Error::other("first syscall"))).unwrap_err().to_string().contains("enumerate"));
        assert!(list_with(|buffer|if buffer.is_none(){Ok(1)}else{Err(std::io::Error::other("second syscall"))}).unwrap_err().to_string().contains("list changed"));
        assert!(list_with(|_|Ok(256*1024+1)).unwrap_err().to_string().contains("exceeds256KiB"));
        assert!(list_with(|buffer|Ok(if buffer.is_none(){1}else{2})).unwrap_err().to_string().contains("grew"));
        assert!(list_with(|buffer|{if let Some(buffer)=buffer {buffer.copy_from_slice(&[255,0]);}Ok(2)}).unwrap_err().to_string().contains("non-UTF8"));
        let names=(0..MAX_NAMES+1).map(|index|format!("user.{index}\0")).collect::<String>().into_bytes();
        assert!(list_with(|buffer|{if let Some(buffer)=buffer{buffer.copy_from_slice(&names);}Ok(names.len())}).unwrap_err().to_string().contains("1024"));
        assert_eq!(list_with(|buffer|{if let Some(buffer)=buffer{buffer.copy_from_slice(b"user.a\0user.a\0");}Ok(14)}).unwrap(),vec!["user.a"]);
        assert!(get_with(|_|Err(std::io::Error::other("first syscall"))).unwrap_err().to_string().contains("cannot read"));
        #[cfg(target_os="macos")] let missing=libc::ENOATTR;
        #[cfg(target_os="linux")] let missing=libc::ENODATA;
        #[cfg(any(target_os="macos",target_os="linux"))] assert!(get_with(|_|Err(std::io::Error::from_raw_os_error(missing))).unwrap().is_none());
        assert!(get_with(|_|Ok(MAX_VALUE+1)).unwrap_err().to_string().contains("exceeds1MiB"));
        assert!(get_with(|buffer|if buffer.is_none(){Ok(1)}else{Err(std::io::Error::other("second syscall"))}).unwrap_err().to_string().contains("changed while reading"));
        assert!(get_with(|buffer|Ok(if buffer.is_none(){1}else{2})).unwrap_err().to_string().contains("grew"));
        assert_eq!(get_with(|buffer|{if let Some(buffer)=buffer{buffer.copy_from_slice(&[0,255]);}Ok(2)}).unwrap(),Some(vec![0,255]));
    }
    #[test]
    fn capture_and_preparation_detect_concurrent_changes_and_enforce_backup_budgets() {
        let mut changed=file();changed.change_stamp=Some(1);assert!(capture_with_io(&changed,&MetadataPolicy::default()).unwrap_err().to_string().contains("changed while capturing"));
        let temp=directory();let journal=temp.path().join("journal");let mut changed=file();changed.change_stamp=Some(1);
        assert!(apply_with_io(&changed,&record(),&MetadataPolicy::default(),&journal).unwrap_err().to_string().contains("changed during preparation"));assert!(!journal.exists());
        let large=FakeFile::default();for i in 0..5 {large.attributes.borrow_mut().insert(format!("user.{i}"),vec![0;MAX_VALUE]);}
        assert!(capture_with_io(&large,&MetadataPolicy::default()).unwrap_err().to_string().contains("exceeds4MiB"));
        let mut desired=record();desired.attributes=(0..5).map(|i|(format!("user.{i}"),String::new())).collect();
        assert!(apply_with_io(&large,&desired,&MetadataPolicy::default(),&journal).unwrap_err().to_string().contains("existing selected metadata"));assert!(!journal.exists());
    }
    #[test]
    fn failed_attribute_updates_remove_only_completed_rollback_journals() {
        for (writes,flushes,retained,expected) in [(vec![1],vec![],false,"rollback completed"),(vec![1,2],vec![],true,"rollback incomplete"),(vec![1],vec![1],true,"rollback could not be flushed")] {
            let temp=directory();let journal=temp.path().join("journal");let mut fake=file();fake.failed_writes=writes.into_iter().collect();fake.failed_flushes=flushes.into_iter().collect();
            let error=apply_with_io(&fake,&record(),&MetadataPolicy::default(),&journal).unwrap_err();assert!(error.to_string().contains(expected),"{error:#}");assert_eq!(journal.exists(),retained);
            assert_eq!(fake.attributes.borrow()["user.a"],vec![0]);
        }
    }
    #[test]
    fn concurrent_content_changes_roll_back_attributes_and_keep_unflushed_recovery() {
        for (writes,flushes,retained,expected) in [(vec![],vec![],false,"content changed"),(vec![2],vec![],true,"content changed"),(vec![],vec![1],true,"rollback could not be flushed")] {
            let temp=directory();let journal=temp.path().join("journal");let mut fake=file();fake.hashes=RefCell::new(VecDeque::from(["a".repeat(64),"b".repeat(64)]));fake.failed_writes=writes.into_iter().collect();fake.failed_flushes=flushes.into_iter().collect();
            let error=apply_with_io(&fake,&record(),&MetadataPolicy::default(),&journal).unwrap_err();assert!(error.to_string().contains(expected),"{error:#}");assert_eq!(journal.exists(),retained);
        }
    }
    #[test]
    fn final_flush_failure_and_recovery_failure_preserve_the_only_recovery_record() {
        for (flushes,retained) in [(vec![1],false),(vec![1,2],true)] {
            let temp=directory();let journal=temp.path().join("journal");let mut fake=file();fake.failed_flushes=flushes.into_iter().collect();
            assert!(apply_with_io(&fake,&record(),&MetadataPolicy::default(),&journal).unwrap_err().to_string().contains("metadata flush failed"));assert_eq!(journal.exists(),retained);assert_eq!(fake.attributes.borrow()["user.a"],vec![0]);
        }
        let temp=directory();let journal=temp.path().join("journal");let mut fake=file();fake.failed_writes=BTreeSet::from([1]);
        let recovery=Recovery{version:1,identity:"synthetic:1".into(),content_sha256:"a".repeat(64),phase:"prepared".into(),previous:BTreeMap::from([("user.a".into(),Some("00".into()))])};crate::state::atomic_json(&journal,&recovery).unwrap();
        assert!(recover_with_io(&fake,&journal).unwrap_err().to_string().contains("rollback incomplete"));assert!(journal.exists());
        let previous=BTreeMap::from([("user.a".into(),Some(vec![0]))]);
        assert!(update_with_rollback(&previous,&previous,|_,_|panic!("unchanged attributes must not be written")).is_ok());
        let temp=directory();let journal=temp.path().join("journal");let fake=file();let mut unchanged=record();unchanged.attributes.insert("user.a".into(),"00".into());
        let report=apply_with_io(&fake,&unchanged,&MetadataPolicy::default(),&journal).unwrap();assert!(report.applied.is_empty());assert!(report.removed.is_empty());assert_eq!(fake.writes.get(),0);assert!(!journal.exists());
    }
}
