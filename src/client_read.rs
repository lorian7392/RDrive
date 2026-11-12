//! Configuration snapshots for remote read commands. These callers never open
//! upload/cache databases or run writer recovery while Finder owns the store.
use crate::config::{self, Config};
use anyhow::{bail, Context, Result};
use std::{fs, io::Read, path::Path};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

fn private_metadata(metadata: &fs::Metadata, directory: bool, owner: u32) -> Result<()> {
    if metadata.file_type().is_symlink()
        || if directory { !metadata.is_dir() } else { !metadata.is_file() }
    {
        bail!("read-only configuration requires a real state directory and regular config file");
    }
    #[cfg(unix)]
    if metadata.uid() != owner || metadata.mode() & 0o077 != 0 || (!directory && metadata.nlink() != 1) {
        bail!("read-only configuration must be private and owned by the current user");
    }
    Ok(())
}

/// Read one atomically published configuration without taking a Store lock.
/// Missing configuration is an empty profile list, not a new state directory.
pub fn config(home: &Path) -> Result<Config> {
    let owner = unsafe { libc::geteuid() };
    let metadata = match fs::symlink_metadata(home) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).context("unable to inspect state directory"),
    };
    private_metadata(&metadata, true, owner)?;
    let path = home.join("config.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => private_metadata(&metadata, false, owner)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::state_directory::guard(home)?;
            return Ok(Config::default());
        }
        Err(error) => return Err(error).context("unable to inspect read-only configuration"),
    }
    crate::state_directory::guard(home)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path).context("unable to open read-only configuration")?;
    private_metadata(&file.metadata()?, false, owner)?;
    read_config(file)
}

fn read_config(mut file: impl Read) -> Result<Config> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let config: Config = serde_json::from_slice(&bytes).context("invalid read-only configuration JSON")?;
    config.index.validate()?;
    config.filters.validate()?;
    config.policies.validate()?;
    config.history.validate()?;
    config.checkpoints.validate()?;
    for profile in config.profiles.values() { config::validate_profile(profile)?; }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Store;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn snapshot_does_not_lock_initialize_or_recover_unrelated_state() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        assert!(config(&home).unwrap().profiles.is_empty());
        assert!(!home.exists());
        let store = Store::open(home.clone()).unwrap();
        assert!(config(&home).unwrap().profiles.is_empty());
        store.save_config().unwrap();
        fs::write(home.join("state.json"), b"invalid database must not be touched").unwrap();
        let before_config = fs::read(home.join("config.json")).unwrap();
        let before_state = fs::read(home.join("state.json")).unwrap();
        let before_names = fs::read_dir(&home).unwrap().map(|row| row.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        assert!(config(&home).unwrap().profiles.is_empty());
        assert_eq!(fs::read(home.join("config.json")).unwrap(), before_config);
        assert_eq!(fs::read(home.join("state.json")).unwrap(), before_state);
        assert_eq!(fs::read_dir(&home).unwrap().map(|row| row.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>(), before_names);
        assert!(Store::open(home).err().unwrap().to_string().contains("another rdrive process"));
        drop(store);
    }

    #[test]
    fn state_and_config_boundaries_reject_links_permissions_types_and_invalid_input() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        let store = Store::open(home.clone()).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&home, &alias).unwrap();
        assert!(config(&alias).is_err());
        let file_home = temporary.path().join("file");
        fs::write(&file_home, b"not a state directory").unwrap();
        assert!(config(&file_home).is_err());
        assert!(config(&file_home.join("child")).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(config(&home).unwrap_err().to_string().contains("private"));
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(private_metadata(&fs::metadata(&home).unwrap(), true, unsafe { libc::geteuid() }.wrapping_add(1)).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o600)).unwrap();
        let inaccessible = config(&home).unwrap_err().to_string();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(inaccessible.contains("unable to inspect read-only configuration"));
        let path = home.join("config.json");
        fs::create_dir(&path).unwrap();
        assert!(config(&home).is_err());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config(&home).unwrap_err().to_string().contains("private"));
        fs::remove_file(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(config(&home).is_err());
        fs::remove_file(&path).unwrap();
        let linked = temporary.path().join("linked-config");
        fs::write(&linked, b"{}").unwrap();
        fs::set_permissions(&linked, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&linked, &path).unwrap();
        assert!(config(&home).unwrap_err().to_string().contains("private"));
        fs::remove_file(&path).unwrap();
        symlink(&file_home, &path).unwrap();
        assert!(config(&home).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"{").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(config(&home).unwrap_err().to_string().contains("invalid read-only"));
        let mut invalid = Config::default();
        invalid.index.refresh_secs = 0;
        crate::state::atomic_json(&path, &invalid).unwrap();
        assert!(config(&home).is_err());
        invalid.index.refresh_secs = 300;
        invalid.filters.exclude.push("../outside".into());
        crate::state::atomic_json(&path, &invalid).unwrap();
        assert!(config(&home).is_err());
        invalid.filters.exclude.clear();
        invalid.policies.versions.keep = 0;
        crate::state::atomic_json(&path, &invalid).unwrap();
        assert!(config(&home).is_err());
        crate::state::atomic_json(&path, &serde_json::json!({"profiles":{"bad":{"protocol":"webdav","url":"http://untrusted.invalid/","username":null,"password_env":null}}})).unwrap();
        assert!(config(&home).is_err());
        drop(store);
    }

    #[test]
    fn interrupted_snapshot_read_returns_the_input_error() {
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "snapshot read failed"))
            }
        }
        assert!(read_config(Interrupted).unwrap_err().to_string().contains("snapshot read failed"));
    }
}
