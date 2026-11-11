//! A paths-only reference to an existing Finder state directory. The profile
//! and credential references continue to live in that directory's config.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const LOCATOR: &str = "shared-home.json";
const MAX_LOCATOR_BYTES: u64 = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedHome {
    version: u32,
    home: PathBuf,
}

/// Explicit selections remain authoritative. Only an otherwise default CLI
/// consults the private reference; resolving a home never initializes state.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    let environment = std::env::var_os("RDRIVE_HOME");
    let standard = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Application Support/rdrive");
    resolve_with(explicit, environment.as_deref(), &standard, unsafe { libc::geteuid() })
}

fn private_directory(path: &Path, owner: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("shared connection directory is unavailable: {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        bail!("shared connection directories must be real, private and owned by the current user: {}", path.display());
    }
    Ok(())
}

fn private_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1
}

fn owned_file(metadata: &fs::Metadata, owner: u32) -> Result<()> {
    if !private_file(metadata) || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        bail!("shared connection files must be private owned regular files without links");
    }
    Ok(())
}

fn read_locator(path: &Path, owner: u32) -> Result<SharedHome> {
    owned_file(&fs::symlink_metadata(path)?, owner)?;
    let file: File = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path)?;
    owned_file(&file.metadata()?, owner)?;
    let mut bytes = Vec::new();
    file.take(MAX_LOCATOR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOCATOR_BYTES { bail!("shared connection locator exceeds 4096 bytes"); }
    serde_json::from_slice(&bytes).context("invalid shared connection locator; expected version and home only")
}

fn resolve_with(explicit: Option<&Path>, environment: Option<&OsStr>, standard: &Path, owner: u32) -> Result<PathBuf> {
    if let Some(path) = explicit { return Ok(path.to_path_buf()); }
    if let Some(path) = environment { return Ok(PathBuf::from(path)); }
    let locator = standard.join(LOCATOR);
    match fs::symlink_metadata(&locator) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(standard.to_path_buf()),
        Err(error) => return Err(error).context("cannot inspect shared connection locator"),
        Ok(_) => (),
    }
    private_directory(standard, owner)?;
    let selected = read_locator(&locator, owner)?;
    if selected.version != 1 { bail!("unsupported shared connection locator version; expected version 1"); }
    if !selected.home.is_absolute() { bail!("shared connection home must be an absolute path"); }
    private_directory(&selected.home, owner)?;
    let config = selected.home.join("config.json");
    let metadata = fs::symlink_metadata(&config)
        .context("shared Finder configuration is missing; preserve the shared locator and restore its original state directory")?;
    owned_file(&metadata, owner)?;
    fs::canonicalize(&selected.home).context("cannot resolve shared Finder state directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn owner() -> u32 { unsafe { libc::geteuid() } }
    fn private(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let standard = temp.path().join("default");
        let finder = temp.path().join("existing Finder state");
        private(&standard); private(&finder);
        write(&finder.join("config.json"), br#"{"profiles":{"pan123":{}}}"#);
        write(&standard.join(LOCATOR), &serde_json::to_vec(&serde_json::json!({"version":1,"home":finder})).unwrap());
        (temp, standard, finder)
    }

    #[test]
    fn explicit_and_environment_selections_never_consult_a_broken_locator() {
        let (_temp, standard, _) = fixture();
        write(&standard.join(LOCATOR), b"invalid JSON");
        assert_eq!(resolve_with(Some(Path::new("explicit")), Some(OsStr::new("env")), &standard, owner()).unwrap(), PathBuf::from("explicit"));
        assert_eq!(resolve_with(None, Some(OsStr::new("env")), &standard, owner()).unwrap(), PathBuf::from("env"));
        assert_eq!(resolve(Some(Path::new("explicit"))).unwrap(), PathBuf::from("explicit"));
    }

    #[test]
    fn missing_locator_preserves_default_without_creating_or_writing_state() {
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("missing/default");
        assert_eq!(resolve_with(None, None, &absent, owner()).unwrap(), absent);
        assert!(!absent.exists());
        let existing = temp.path().join("existing"); private(&existing);
        let before = fs::read_dir(&existing).unwrap().count();
        assert_eq!(resolve_with(None, None, &existing, owner()).unwrap(), existing);
        assert_eq!(fs::read_dir(existing).unwrap().count(), before);
    }

    #[test]
    fn valid_reference_returns_the_one_existing_canonical_state_without_copying_it() {
        let (_temp, standard, finder) = fixture();
        let config = fs::read(finder.join("config.json")).unwrap();
        assert_eq!(resolve_with(None, None, &standard, owner()).unwrap(), finder.canonicalize().unwrap());
        assert_eq!(fs::read(finder.join("config.json")).unwrap(), config);
        assert_eq!(fs::read_dir(&standard).unwrap().count(), 1);
    }

    #[test]
    fn invalid_documents_versions_and_stale_targets_are_explicit_errors() {
        let (_temp, standard, finder) = fixture();
        for value in [serde_json::json!({"version":2,"home":finder}), serde_json::json!({"version":1,"home":"relative"}),
            serde_json::json!({"version":1,"home":finder,"profiles":{}}), serde_json::json!({"home":finder})] {
            write(&standard.join(LOCATOR), &serde_json::to_vec(&value).unwrap());
            assert!(resolve_with(None, None, &standard, owner()).is_err());
        }
        write(&standard.join(LOCATOR), &[b' ';4097]);
        assert!(resolve_with(None, None, &standard, owner()).unwrap_err().to_string().contains("4096"));
        write(&standard.join(LOCATOR), b"{");
        assert!(resolve_with(None, None, &standard, owner()).unwrap_err().to_string().contains("invalid shared"));
        write(&standard.join(LOCATOR), &serde_json::to_vec(&serde_json::json!({"version":1,"home":finder.join("missing")})).unwrap());
        assert!(resolve_with(None, None, &standard, owner()).is_err());
        write(&standard.join(LOCATOR), &serde_json::to_vec(&serde_json::json!({"version":1,"home":finder})).unwrap());
        fs::remove_file(finder.join("config.json")).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).unwrap_err().to_string().contains("configuration is missing"));
    }

    #[test]
    fn public_foreign_linked_and_nonregular_files_are_rejected() {
        let (_temp, standard, finder) = fixture();
        let locator = standard.join(LOCATOR);
        let original = fs::read(&locator).unwrap();
        fs::set_permissions(&standard, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err());
        fs::set_permissions(&standard, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&locator, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err());
        fs::set_permissions(&locator, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(resolve_with(None, None, &standard, owner().wrapping_add(1)).is_err());
        assert!(owned_file(&fs::metadata(&locator).unwrap(), owner().wrapping_add(1)).is_err());
        let linked = standard.join("locator-link"); fs::hard_link(&locator, &linked).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err()); fs::remove_file(linked).unwrap();
        fs::remove_file(&locator).unwrap(); symlink(finder.join("config.json"), &locator).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err()); fs::remove_file(&locator).unwrap();
        fs::create_dir(&locator).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err()); fs::remove_dir(&locator).unwrap();
        write(&locator, &original);
        fs::set_permissions(&finder, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err()); fs::set_permissions(&finder, fs::Permissions::from_mode(0o700)).unwrap();
        let config = finder.join("config.json"); fs::remove_file(&config).unwrap(); symlink(&locator, &config).unwrap();
        assert!(resolve_with(None, None, &standard, owner()).is_err());
    }

    #[test]
    fn linked_state_and_locator_parent_and_unreadable_ancestors_are_rejected() {
        let (temp, standard, finder) = fixture();
        let alias = temp.path().join("alias"); symlink(&finder, &alias).unwrap();
        write(&standard.join(LOCATOR), &serde_json::to_vec(&serde_json::json!({"version":1,"home":alias})).unwrap());
        assert!(resolve_with(None, None, &standard, owner()).is_err());
        let parent_alias = temp.path().join("parent-alias"); symlink(&standard, &parent_alias).unwrap();
        assert!(resolve_with(None, None, &parent_alias, owner()).is_err());
        let file = temp.path().join("regular"); write(&file,b"regular file");
        assert!(resolve_with(None, None, &file, owner()).unwrap_err().to_string().contains("cannot inspect"));
    }
}
