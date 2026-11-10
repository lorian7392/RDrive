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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub cache: CachePolicy,
    #[serde(default)]
    pub index: IndexPolicy,
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
        Self { enabled: true, prefetch_depth: None, refresh_secs: 300, active_refresh_secs: 30 }
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
    if let Some(key) = &profile.password_env {
        return std::env::var(key)
            .with_context(|| format!("credential environment variable {key} is missing"));
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                &format!("rdrive:{name}"),
                "-a",
                profile.username.as_deref().unwrap_or(""),
                "-w",
            ])
            .output()
            .context("unable to read macOS Keychain")?;
        if output.status.success() {
            return Ok(String::from_utf8(output.stdout)?
                .trim_end_matches('\n')
                .to_owned());
        }
    }
    let _ = name;
    if profile.username.is_none() {
        return Ok(String::new());
    }
    bail!("no credential found; set password-env or add a Keychain generic password with service rdrive:{name}")
}
