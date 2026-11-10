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

pub fn capture(path: &Path, policy: &MetadataPolicy) -> Result<MetadataRecord> {
    capture_file(&open_regular(path)?, policy)
}
pub fn capture_file(file: &File, policy: &MetadataPolicy) -> Result<MetadataRecord> {
    policy.validate()?;
    let before = stamp(file)?;
    let mut attributes = BTreeMap::new();
    let mut excluded = Vec::new();
    let mut total = 0usize;
    for name in list(file)? {
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
            get(file, &name)?.context("metadata changed while capturing; retry the snapshot")?;
        total = total
            .checked_add(value.len())
            .context("metadata size overflow")?;
        if total > MAX_TOTAL {
            bail!("selected metadata exceeds4MiB; no partial snapshot was returned");
        }
        attributes.insert(name, hex(&value));
    }
    let content_sha256 = hash_file(file)?;
    if before != stamp(file)? {
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
    record.validate()?;
    policy.validate()?;
    private_journal_parent(journal)?;
    if journal.symlink_metadata().is_ok() {
        bail!("metadata recovery journal already exists; recover it before a new update");
    }
    let before_stamp = stamp(file)?;
    if hash_file(file)? != record.content_sha256 {
        bail!("metadata content hash does not match target; attributes were preserved");
    }
    let mut desired = BTreeMap::<String, Option<Vec<u8>>>::new();
    let mut report = ApplyReport {
        skipped: record.excluded.clone(),
        ..ApplyReport::default()
    };
    for name in list(file)? {
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
        let value = get(file, name)?;
        total = total
            .checked_add(value.as_ref().map_or(0, Vec::len))
            .context("metadata backup size overflow")?;
        if total > MAX_TOTAL {
            bail!("existing selected metadata exceeds4MiB; attributes were preserved");
        }
        previous.insert(name.clone(), value);
    }
    if before_stamp != stamp(file)? {
        bail!("file or metadata changed during preparation; attributes were preserved");
    }
    let recovery = Recovery {
        version: 1,
        identity: identity(file)?,
        content_sha256: record.content_sha256.clone(),
        phase: "prepared".into(),
        previous: previous
            .iter()
            .map(|(name, value)| (name.clone(), value.as_ref().map(|value| hex(value))))
            .collect(),
    };
    crate::state::atomic_json(journal, &recovery)?;
    let applied = update_with_rollback(&previous, &desired, |name, value| {
        set_or_remove(file, name, value)
    });
    if let Err(failure) = applied {
        if failure.rollback_failures.is_empty() {
            file.sync_all().with_context(|| {
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
    if hash_file(file)? != record.content_sha256 {
        let failures = restore(file, &previous);
        if failures.is_empty() {
            file.sync_all().with_context(|| {
                format!(
                    "metadata rollback could not be flushed; recovery journal retained at {}",
                    journal.display()
                )
            })?;
            remove_journal(journal)?;
        }
        bail!("file content changed during metadata update; prior attributes restored where possible; recovery journal: {}",journal.display());
    }
    if let Err(error) = file.sync_all() {
        let failures = restore(file, &previous);
        if failures.is_empty() && file.sync_all().is_ok() {
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
        || recovery.identity != identity(file)?
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
    if hash_file(file)? != recovery.content_sha256 {
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
    file.sync_all()?;
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
fn restore(file: &File, previous: &BTreeMap<String, Option<Vec<u8>>>) -> Vec<String> {
    previous
        .iter()
        .filter_map(|(name, value)| {
            set_or_remove(file, name, value.as_deref())
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
    {
        #[cfg(target_os = "macos")]
        let length = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
        #[cfg(target_os = "linux")]
        let length = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
        if length < 0 {
            return Err(std::io::Error::last_os_error()).context("cannot enumerate file xattrs");
        }
        if length as usize > 256 * 1024 {
            bail!("xattr name list exceeds256KiB");
        }
        let mut bytes = vec![0u8; length as usize];
        #[cfg(target_os = "macos")]
        let actual = unsafe {
            libc::flistxattr(file.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0)
        };
        #[cfg(target_os = "linux")]
        let actual =
            unsafe { libc::flistxattr(file.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if actual < 0 {
            return Err(std::io::Error::last_os_error()).context("file xattr list changed");
        }
        if actual as usize > bytes.len() {
            bail!("file xattr list grew while reading; no snapshot was returned");
        }
        bytes.truncate(actual as usize);
        let mut names = BTreeSet::new();
        for name in bytes
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            names.insert(
                std::str::from_utf8(name)
                    .context("non-UTF8 xattr name cannot be represented portably")?
                    .to_owned(),
            );
        }
        if names.len() > MAX_NAMES {
            bail!("file has more than1024 xattrs");
        }
        return Ok(names.into_iter().collect());
    }
    #[allow(unreachable_code)]
    {
        let _ = file;
        bail!("xattrs are unsupported on this platform")
    }
}
fn get(file: &File, name: &str) -> Result<Option<Vec<u8>>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let name = std::ffi::CString::new(name)?;
        #[cfg(target_os = "macos")]
        let length = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let length =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if length < 0 {
            let error = std::io::Error::last_os_error();
            if missing_attr(&error) {
                return Ok(None);
            }
            return Err(error).context("cannot read selected xattr");
        }
        if length as usize > MAX_VALUE {
            bail!("selected xattr exceeds1MiB; no value was discarded");
        }
        let mut value = vec![0u8; length as usize];
        #[cfg(target_os = "macos")]
        let actual = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let actual = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if actual < 0 {
            return Err(std::io::Error::last_os_error()).context("xattr changed while reading");
        }
        if actual as usize > value.len() {
            bail!("xattr grew while reading; no partial value was returned");
        }
        value.truncate(actual as usize);
        return Ok(Some(value));
    }
    #[allow(unreachable_code)]
    {
        let _ = (file, name);
        bail!("xattrs are unsupported on this platform")
    }
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
    #[allow(unreachable_code)]
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
    #[allow(unreachable_code)]
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
}
