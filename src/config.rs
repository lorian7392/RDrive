use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Webdav,
    Smb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub protocol: Protocol,
    pub url: String,
    pub username: Option<String>,
    pub password_env: Option<String>,
    /// Read the existing macOS Internet Keychain item for this URL host/account.
    #[serde(default, skip_serializing_if = "is_false")]
    pub keychain_internet: bool,
    #[serde(default)]
    pub allow_http: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// SMB requires encryption unless explicitly disabled by user.
    #[serde(default = "default_true")]
    pub smb_encryption: bool,
    /// Explicit opt-in to Nextcloud chunked WebDAV uploads. Same origin only.
    #[serde(default)]
    pub nextcloud_upload_url: Option<String>,
}
fn default_timeout() -> u64 {
    120
}
fn default_true() -> bool {
    true
}
fn is_false(value: &bool) -> bool { !*value }

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub cache: CachePolicy,
    #[serde(default)]
    pub index: IndexPolicy,
    #[serde(default)]
    pub filters: crate::filtering::FilterConfig,
    #[serde(default)]
    pub policies: crate::policies::PolicyConfig,
}

/// Background namespace metadata only; this does not pin or download content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexPolicy {
    pub enabled: bool,
    /// Root is depth zero; None walks the complete tree progressively.
    pub prefetch_depth: Option<u32>,
    pub refresh_secs: u64,
    pub active_refresh_secs: u64,
}
impl Default for IndexPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            prefetch_depth: None,
            refresh_secs: 300,
            active_refresh_secs: 30,
        }
    }
}
impl IndexPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.refresh_secs == 0 || self.active_refresh_secs == 0 {
            bail!("index refresh intervals must be positive");
        }
        if self.refresh_secs > 31_536_000 || self.active_refresh_secs > 31_536_000 {
            bail!("index refresh intervals cannot exceed one year");
        }
        Ok(())
    }
}

/// Clean cached file payloads only. Upload snapshots and pending offline edits
/// are durable user data and are never included in this eviction budget.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicy {
    /// None preserves the previous unlimited policy until explicitly configured.
    pub max_bytes: Option<u64>,
    /// Clean Rust payloads only. None keeps the legacy state-home/cache path.
    #[serde(default)]
    pub directory: Option<PathBuf>,
}

pub fn default_home() -> PathBuf {
    std::env::var_os("RDRIVE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Application Support/rdrive")
        })
}

pub fn validate_profile(profile: &Profile) -> Result<()> {
    if profile.keychain_internet && profile.password_env.is_some() {
        bail!("keychain-internet and password-env are mutually exclusive credential sources");
    }
    let url = url::Url::parse(&profile.url).context("invalid profile URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("put credentials in username/password-env, never in URL");
    }
    if url.query().is_some() || url.fragment().is_some() || url.host_str().is_none() {
        bail!("URL requires a host and must not contain a query or fragment");
    }
    match profile.protocol {
        Protocol::Webdav => {
            if url.scheme() != "https" && !(url.scheme() == "http" && profile.allow_http) {
                bail!("WebDAV requires HTTPS; opt in to HTTP explicitly for trusted test servers");
            }
        }
        Protocol::Smb => {
            if url.scheme() != "smb" || url.path().trim_matches('/').is_empty() {
                bail!("SMB URL must be smb://server/share[/folder]");
            }
        }
    }
    if let Some(endpoint) = &profile.nextcloud_upload_url {
        let uploads = url::Url::parse(endpoint).context("invalid Nextcloud upload URL")?;
        if !matches!(profile.protocol, Protocol::Webdav)
            || uploads.origin() != url.origin()
            || !uploads.username().is_empty()
            || uploads.password().is_some()
            || uploads.query().is_some()
            || uploads.fragment().is_some()
        {
            bail!("Nextcloud upload URL must be credential-free and on the same WebDAV origin");
        }
    }
    if profile.timeout_secs == 0 {
        bail!("timeout must be positive");
    }
    Ok(())
}

pub fn password(profile: &Profile, name: &str) -> Result<String> {
    password_with(profile, name, process_output)
}

fn process_output(command: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    command.output()
}

// Keep credential lookup injectable so tests can cover Keychain responses
// without reading credentials, presenting a prompt or replacing system tools.
fn password_with(
    profile: &Profile,
    name: &str,
    _run: impl FnOnce(&mut std::process::Command) -> std::io::Result<std::process::Output>,
) -> Result<String> {
    if profile.keychain_internet && profile.password_env.is_some() {
        bail!("keychain-internet and password-env are mutually exclusive credential sources");
    }
    if let Some(key) = &profile.password_env {
        return std::env::var(key)
            .with_context(|| format!("credential environment variable {key} is missing"));
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("/usr/bin/security");
        if profile.keychain_internet {
            let url = url::Url::parse(&profile.url).context("invalid Internet Keychain profile URL")?;
            let server = url.host_str().context("Internet Keychain profile URL requires a host")?;
            command.args(["find-internet-password", "-s", server, "-a", profile.username.as_deref().unwrap_or(""), "-w"]);
        } else {
            command.args([
                "find-generic-password",
                "-s",
                &format!("rdrive:{name}"),
                "-a",
                profile.username.as_deref().unwrap_or(""),
                "-w",
            ]);
        }
        let output = _run(&mut command)
            .context("unable to read macOS Keychain")?;
        if output.status.success() {
            return Ok(String::from_utf8(output.stdout)?
                .trim_end_matches('\n')
                .to_owned());
        }
        if profile.keychain_internet {
            bail!("the configured Internet Keychain credential is unavailable; grant access to the existing item for this connection");
        }
    }
    #[cfg(not(target_os = "macos"))]
    if profile.keychain_internet {
        bail!("Internet Keychain credentials are available only on macOS");
    }
    let _ = name;
    if profile.username.is_none() {
        return Ok(String::new());
    }
    bail!("no credential found; set password-env or add a Keychain generic password with service rdrive:{name}")
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, process::{Command, ExitStatus, Output}};

    fn profile(username: Option<&str>) -> Profile {
        Profile { protocol: Protocol::Webdav, url: "https://example.invalid/".into(), username: username.map(str::to_owned), password_env: None, keychain_internet: false, allow_http: false, timeout_secs: 120, smb_encryption: true, nextcloud_upload_url: None }
    }

    #[test]
    fn command_runner_reports_success_and_spawn_failure_without_keychain() {
        assert!(process_output(&mut Command::new("/usr/bin/true")).unwrap().status.success());
        assert!(!process_output(&mut Command::new("/usr/bin/false")).unwrap().status.success());
        assert!(process_output(&mut Command::new("/rdrive-test-no-such-executable")).is_err());
    }

    #[test]
    fn internet_keychain_is_explicit_and_preserves_existing_account_bindings() {
        let mut connection = profile(Some("account"));
        let legacy = serde_json::to_value(&connection).unwrap();
        assert!(legacy.get("keychain_internet").is_none());
        assert!(!serde_json::from_value::<Profile>(legacy).unwrap().keychain_internet);
        connection.password_env = Some("RDRIVE_KEYCHAIN_SOURCE_TEST".into());
        let mut config = Config::default();
        config.profiles.insert("existing".into(), connection.clone());
        let domain = crate::provider::Domain { id: "existing-smart".into(), profile: "existing".into(),
            remote: String::new(), display_name: "Existing".into(), repository: false, smart: true, local_root: None };
        let original = crate::provider::journal_binding(&config, &domain).unwrap();
        connection.password_env = None;
        connection.keychain_internet = true;
        validate_profile(&connection).unwrap();
        assert_eq!(serde_json::to_value(&connection).unwrap()["keychain_internet"], true);
        config.profiles.insert("existing".into(), connection.clone());
        assert_eq!(crate::provider::journal_binding(&config, &domain).unwrap(), original);
        connection.username = Some("another-account".into());
        config.profiles.insert("existing".into(), connection.clone());
        assert_ne!(crate::provider::journal_binding(&config, &domain).unwrap(), original);
        connection.password_env = Some("RDRIVE_KEYCHAIN_SOURCE_TEST".into());
        assert!(validate_profile(&connection).unwrap_err().to_string().contains("mutually exclusive"));
        assert!(password_with(&connection, "existing", |_| panic!("ambiguous credentials must not access Keychain"))
            .unwrap_err().to_string().contains("mutually exclusive"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn internet_keychain_reuses_the_host_account_item_without_generic_or_environment_fallback() {
        let mut connection = profile(Some("test user"));
        connection.keychain_internet = true;
        connection.url = "https://storage.example.invalid:8443/webdav/".into();
        let secret = password_with(&connection, "existing", |command| {
            assert_eq!(command.get_program(), "/usr/bin/security");
            assert_eq!(command.get_args().collect::<Vec<_>>(),
                ["find-internet-password", "-s", "storage.example.invalid", "-a", "test user", "-w"]);
            Ok(Output { status: ExitStatus::from_raw(0), stdout: b"mock existing item\n".to_vec(), stderr: vec![] })
        }).unwrap();
        assert_eq!(secret, "mock existing item");
        assert!(password_with(&connection, "existing", |_| Ok(Output { status: ExitStatus::from_raw(0), stdout: vec![255], stderr: vec![] })).is_err());
        for username in [Some("test user".into()), None] {
            connection.username = username;
            let error = password_with(&connection, "existing", |command| {
                assert_eq!(command.get_args().nth(4).unwrap(), connection.username.as_deref().unwrap_or(""));
                Ok(Output { status: ExitStatus::from_raw(256), stdout: vec![], stderr: b"mock denial".to_vec() })
            }).unwrap_err();
            assert!(error.to_string().contains("configured Internet Keychain credential is unavailable"));
        }
        let failed = password_with(&connection, "existing", |_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock unavailable security"))).unwrap_err();
        assert!(failed.to_string().contains("unable to read macOS Keychain"));
        connection.url = "not a URL".into();
        assert!(password_with(&connection, "existing", |_| panic!("invalid URL must not query Keychain"))
            .unwrap_err().to_string().contains("invalid Internet Keychain profile URL"));
        connection.url = "file:///tmp/owned-fixture".into();
        assert!(password_with(&connection, "existing", |_| panic!("missing host must not query Keychain"))
            .unwrap_err().to_string().contains("requires a host"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn explicit_internet_keychain_never_becomes_anonymous_on_other_platforms() {
        let mut connection = profile(None);
        connection.keychain_internet = true;
        let error = password_with(&connection, "existing", |_| panic!("unsupported Keychain must not spawn a process"))
            .unwrap_err();
        assert!(error.to_string().contains("available only on macOS"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_arguments_success_invalid_text_failure_and_anonymous_fallback() {
        let result = password_with(&profile(Some("test user")), "test profile", |command| {
            assert_eq!(command.get_program(), "/usr/bin/security");
            assert_eq!(command.get_args().collect::<Vec<_>>(), ["find-generic-password", "-s", "rdrive:test profile", "-a", "test user", "-w"]);
            Ok(Output { status: ExitStatus::from_raw(0), stdout: b"fake password\n\n".to_vec(), stderr: vec![] })
        }).unwrap();
        assert_eq!(result, "fake password");
        assert!(password_with(&profile(Some("u")), "p", |_| Ok(Output { status: ExitStatus::from_raw(0), stdout: vec![255], stderr: vec![] })).is_err());
        let denied = password_with(&profile(Some("u")), "p", |_| Ok(Output { status: ExitStatus::from_raw(256), stdout: vec![], stderr: b"test-only denial".to_vec() })).unwrap_err();
        assert!(denied.to_string().contains("no credential found"));
        assert_eq!(password_with(&profile(None), "p", |command| {
            assert_eq!(command.get_args().nth(4).unwrap(), "");
            Ok(Output { status: ExitStatus::from_raw(256), stdout: vec![], stderr: vec![] })
        }).unwrap(), "");
        let failed = password_with(&profile(None), "p", |_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "synthetic missing security"))).unwrap_err();
        assert!(failed.to_string().contains("unable to read macOS Keychain"));
    }
}
