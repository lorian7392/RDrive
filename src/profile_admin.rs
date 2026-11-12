//! Serialized profile administration, with receipts committed beside profiles.
use crate::{config::{self, Profile}, state::{self, Store}};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs::{self, File, OpenOptions}, io::Read, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::Path};

mod transport;
pub use transport::{dispatch, dispatch_history, Registration, MAX_REQUEST_BYTES};

const RETENTION_SECONDS: i64 = 24 * 60 * 60;
const MAX_FUTURE_SECONDS: i64 = 30;
const MAX_RECEIPTS: usize = 1024;
const MAX_REFERENCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Add { name: String, profile: Profile },
    Remove { name: String },
    HistoryPolicy { profile: Option<String>, policy: crate::history::HistoryPolicy },
    CheckpointPolicy { policy: crate::checkpoints::Policy },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub request_id: String,
    pub change: Change,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub fingerprint: String,
    pub result: Value,
    pub expires_at: i64,
}

impl Command {
    pub fn new(change: Change, request_id: Option<String>) -> Result<Self> {
        let now = chrono::Utc::now().timestamp();
        let command = Self { request_id: request_id.unwrap_or_else(|| format!("{now}:{}", uuid::Uuid::new_v4())), change };
        command.validate_at(now)?;
        Ok(command)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_at(chrono::Utc::now().timestamp()).map(|_| ())
    }

    fn validate_at(&self, now: i64) -> Result<i64> {
        let (created, id) = self.request_id.split_once(':').context("profile request ID must be <unix_seconds>:<UUID>")?;
        let created: i64 = created.parse().context("invalid profile request timestamp")?;
        let uuid = uuid::Uuid::parse_str(id).context("invalid profile request UUID")?;
        if created < 0 || format!("{created}:{uuid}") != self.request_id || created > now.saturating_add(MAX_FUTURE_SECONDS) {
            bail!("profile request ID must be canonical and cannot be more than 30 seconds in the future");
        }
        let expires_at = created.checked_add(RETENTION_SECONDS).context("invalid profile request expiration")?;
        if expires_at <= now { bail!("profile request expired; inspect the saved profiles before issuing a new request"); }
        let name = match &self.change {
            Change::Add { name, .. } | Change::Remove { name } => Some(name),
            Change::HistoryPolicy { profile, .. } => profile.as_ref(),
            Change::CheckpointPolicy { .. } => None,
        };
        if let Some(name) = name {
            if name.is_empty() || name.starts_with('-') || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                bail!("profile name must contain letters, numbers, hyphen or underscore");
            }
        }
        if let Change::HistoryPolicy { policy, .. } = &self.change { policy.validate()?; }
        if let Change::CheckpointPolicy { policy } = &self.change { policy.validate()?; }
        if let Change::Add { profile, .. } = &self.change {
            if profile.password_env.as_ref().is_some_and(|name| name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
                bail!("invalid credential environment variable name");
            }
            config::validate_profile(profile)?;
        }
        Ok(expires_at)
    }
}

pub fn apply(store: &mut Store, command: &Command) -> Result<Value> {
    apply_at(store, command, chrono::Utc::now().timestamp())
}

fn apply_at(store: &mut Store, command: &Command, now: i64) -> Result<Value> {
    apply_with_save(store, command, now, Store::save_config)
}

fn apply_with_save(store: &mut Store, command: &Command, now: i64, save: impl FnOnce(&Store) -> Result<()>) -> Result<Value> {
    // Expired requests never regain authority after their receipt is pruned.
    let expires_at = command.validate_at(now)?;
    let fingerprint = state::key(&serde_json::to_string(&command.change)?);
    if let Some(receipt) = store.config.profile_admin_receipts.get(&command.request_id) {
        if receipt.fingerprint != fingerprint { bail!("profile request ID was already used with different arguments"); }
        return Ok(receipt.result.clone());
    }
    let mut candidate: config::Config = serde_json::from_value(serde_json::to_value(&store.config)?)?;
    candidate.profile_admin_receipts.retain(|_, receipt| receipt.expires_at > now);
    if candidate.profile_admin_receipts.len() >= MAX_RECEIPTS { bail!("profile request receipt capacity reached; retry after existing receipts expire"); }
    let result = match &command.change {
        Change::Add { name, profile } => {
            if candidate.profiles.contains_key(name) { bail!("profile already exists; remove it before replacing its destination"); }
            candidate.profiles.insert(name.clone(), profile.clone());
            json!({"profile": name, "saved": true, "request_id": command.request_id})
        }
        Change::Remove { name } => {
            if !candidate.profiles.contains_key(name) { bail!("unknown profile"); }
            require_removable(store, name)?;
            candidate.profiles.remove(name);
            candidate.history.profiles.remove(name);
            json!({"removed": name, "request_id": command.request_id})
        }
        Change::HistoryPolicy { profile, policy } => {
            if let Some(name) = profile {
                if !candidate.profiles.contains_key(name) { bail!("unknown profile"); }
                candidate.history.profiles.insert(name.clone(), policy.clone());
            } else {
                candidate.history.default = policy.clone();
            }
            candidate.history.validate()?;
            json!({"saved":true,"profile":profile,"policy":policy,"request_id":command.request_id})
        }
        Change::CheckpointPolicy { policy } => {
            candidate.checkpoints = policy.clone();
            json!({"saved":true,"policy":policy,"request_id":command.request_id})
        }
    };
    candidate.profile_admin_receipts.insert(command.request_id.clone(), Receipt { fingerprint, result: result.clone(), expires_at });
    let previous = std::mem::replace(&mut store.config, candidate);
    if let Err(error) = save(store) {
        // A directory fsync can fail after rename: reload the actual committed
        // receipt rather than claiming success or leaving a false memory view.
        store.config = read_reference(&store.home.join("config.json")).ok().flatten().unwrap_or(previous);
        return Err(error.context(format!("profile change outcome uncertain; retry the same request ID {}", command.request_id)));
    }
    Ok(result)
}

fn read_reference<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file: File = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read retained profile reference"),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
        bail!("retained profile references must be private owned regular files without links");
    }
    let mut bytes = Vec::new();
    file.take(MAX_REFERENCE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REFERENCE_BYTES { bail!("retained profile reference exceeds 64 MiB"); }
    Ok(Some(serde_json::from_slice(&bytes).context("invalid retained profile reference")?))
}

#[derive(Deserialize)]
struct Reference { profile: String, phase: String }

fn require_removable(store: &Store, name: &str) -> Result<()> {
    if !crate::history::list(&store.home,&store.config,name,None,None)?.is_empty() {
        bail!("profile has retained file history; export or explicitly clear it before removing the connection");
    }
    if store.db.jobs.iter().any(|job| job.profile == name && !["done", "cancelled"].contains(&job.status.as_str())) {
        bail!("profile has unfinished uploads; finish or cancel them first");
    }
    if store.db.cache.values().any(|item| item.profile == name) { bail!("profile has cached files; unpin and prune them first"); }
    let domains: Vec<crate::provider::Domain> = read_reference(&store.home.join("provider-domains.json"))?.unwrap_or_default();
    if domains.iter().any(|domain| domain.profile == name) { bail!("profile is used by a configured Finder domain; unregister and remove the domain first"); }
    for (directory, downloads) in [("transfers", true), ("transactions", false)] {
        for entry in fs::read_dir(store.home.join(directory))? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") { continue; }
            let reference: Reference = read_reference(&path)?.context("retained profile reference disappeared")?;
            if !downloads && !["open", "prepared", "committed", "aborted"].contains(&reference.phase.as_str()) {
                bail!("unknown retained repository transaction phase; preserve and recover it before removing profiles");
            }
            if reference.profile == name && (downloads || ["open", "prepared"].contains(&reference.phase.as_str())) {
                bail!("profile has retained transfers or unfinished repository transactions; recover or discard them first");
            }
        }
    }
    let smart = store.home.join("smart");
    if let Ok(metadata) = fs::symlink_metadata(&smart) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!("retained Smart state must be a private owned real directory");
        }
    }
    let entries = match fs::read_dir(&smart) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("cannot inspect retained Smart state"),
    };
    let known: BTreeMap<_, _> = domains.iter().filter(|domain| domain.smart).map(|domain| (state::key(&domain.id), domain)).collect();
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == "control.json" { continue; }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() { bail!("unknown retained Smart state; preserve and recover it before removing profiles"); }
        let id = entry.file_name();
        let domain = id.to_str().and_then(|id| known.get(id)).context("unknown retained Smart queue; preserve and recover it before removing profiles")?;
        crate::smart::read_only(&store.home, &domain.id, &crate::provider::journal_binding(&store.config, domain)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::Entry, config::Protocol, state::{CacheItem, Job}};
    use std::os::unix::fs::{symlink, PermissionsExt};

    const NOW: i64 = 1_800_000_000;
    fn profile() -> Profile {
        Profile { protocol: Protocol::Webdav, url: "https://example.invalid/".into(), username: Some("account".into()), password_env: Some("RDRIVE_PASSWORD".into()), keychain_internet: false, allow_http: false, timeout_secs: 120, smb_encryption: true, nextcloud_upload_url: None }
    }
    fn add(name: &str) -> Command { command(Change::Add { name: name.into(), profile: profile() }) }
    fn remove(name: &str) -> Command { command(Change::Remove { name: name.into() }) }
    fn command(change: Change) -> Command { Command { request_id: format!("{NOW}:{}", uuid::Uuid::new_v4()), change } }
    fn store() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("state")).unwrap();
        (temp, store)
    }
    fn write(path: &Path, value: &impl Serialize) { state::atomic_json(path, value).unwrap(); }
    fn entry() -> Entry { Entry { path: "doc".into(), is_dir: false, size: 3, modified: None, etag: Some("v1".into()) } }
    fn domain(id: &str, name: &str, smart: bool) -> crate::provider::Domain {
        crate::provider::Domain { id: id.into(), profile: name.into(), remote: String::new(), display_name: id.into(), repository: false, smart, local_root: None }
    }

    #[test]
    fn canonical_requests_validate_before_use_and_never_contain_secret_values() {
        let command = Command::new(Change::Add { name: "nas".into(), profile: profile() }, None).unwrap();
        command.validate().unwrap();
        Command::new(command.change.clone(), Some(command.request_id.clone())).unwrap();
        let serialized = serde_json::to_value(&command).unwrap();
        assert_eq!(serialized["change"]["profile"]["password_env"], "RDRIVE_PASSWORD");
        assert!(serialized["change"]["profile"].get("password").is_none());
        for id in ["missing-colon".into(), "bad:not-a-uuid".into(), format!("{NOW}:bad"), format!("0{NOW}:{}", uuid::Uuid::new_v4()), format!("{}:{}", NOW + 31, uuid::Uuid::new_v4()), format!("-1:{}", uuid::Uuid::new_v4()), format!("{}:{}", NOW - RETENTION_SECONDS, uuid::Uuid::new_v4())] {
            let mut invalid = add("nas"); invalid.request_id = id;
            assert!(invalid.validate_at(NOW).is_err());
        }
        for name in ["", "-nas", "bad/path", "名称"] { assert!(add(name).validate_at(NOW).is_err()); }
        for variable in ["", "bad-name", "PASSWORD=value"] {
            let mut p = profile(); p.password_env = Some(variable.into());
            assert!(command_with_profile(p).validate_at(NOW).is_err());
        }
        let mut p = profile(); p.url = "https://user:secret@example.invalid/".into();
        assert!(command_with_profile(p).validate_at(NOW).is_err());
        let mut unknown = serialized.clone(); unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<Command>(unknown).is_err());
        let mut unknown = serialized["change"].clone(); unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<Change>(unknown).is_err());
        assert!(serde_json::from_value::<Receipt>(json!({"fingerprint":"hash","result":{},"expires_at":NOW,"extra":true})).is_err());
    }

    fn command_with_profile(profile: Profile) -> Command { command(Change::Add { name: "nas".into(), profile }) }

    #[test]
    fn durable_receipts_survive_restart_preserve_configuration_and_prevent_aba() {
        let (_temp, mut store) = store();
        store.config.cache.max_bytes = Some(1234);
        store.config.index.refresh_secs = 77;
        store.config.filters.include = vec!["*.txt".into()];
        let add = add("nas");
        let saved = apply_at(&mut store, &add, NOW).unwrap();
        assert_eq!(saved["profile"], "nas");
        assert_eq!(saved["request_id"], add.request_id);
        let before = fs::read(store.home.join("config.json")).unwrap();
        assert_eq!(apply_at(&mut store, &add, NOW + 1).unwrap(), saved);
        assert_eq!(fs::read(store.home.join("config.json")).unwrap(), before);
        let home = store.home.clone(); drop(store);
        let mut store = Store::open(home).unwrap();
        assert_eq!(apply_at(&mut store, &add, NOW + 2).unwrap(), saved);
        assert_eq!(store.config.cache.max_bytes, Some(1234));
        assert_eq!(store.config.index.refresh_secs, 77);
        assert_eq!(store.config.filters.include, ["*.txt"]);
        let mut changed = add.clone(); changed.change = Change::Remove { name: "nas".into() };
        assert!(apply_at(&mut store, &changed, NOW + 3).unwrap_err().to_string().contains("different arguments"));
        assert!(apply_at(&mut store, &self::add("nas"), NOW + 3).unwrap_err().to_string().contains("already exists"));
        let removal = remove("nas"); let receipt = apply_at(&mut store, &removal, NOW + 4).unwrap();
        assert_eq!(receipt["removed"], "nas");
        apply_at(&mut store, &self::add("nas"), NOW + 5).unwrap();
        assert_eq!(apply_at(&mut store, &removal, NOW + 6).unwrap(), receipt);
        assert!(store.config.profiles.contains_key("nas"));
        assert!(apply_at(&mut store, &removal, NOW + RETENTION_SECONDS).unwrap_err().to_string().contains("expired"));
        assert!(store.config.profiles.contains_key("nas"));
        assert!(apply_at(&mut store, &remove("unknown"), NOW).unwrap_err().to_string().contains("unknown profile"));
    }

    #[test]
    fn receipt_capacity_prunes_only_expired_requests() {
        let (_temp, mut store) = store();
        for i in 0..MAX_RECEIPTS { store.config.profile_admin_receipts.insert(format!("existing-{i}"), Receipt { fingerprint: "hash".into(), result: json!({}), expires_at: NOW + 1 }); }
        let command = add("new");
        assert!(apply_at(&mut store, &command, NOW).unwrap_err().to_string().contains("capacity"));
        assert!(store.config.profiles.is_empty());
        assert_eq!(store.config.profile_admin_receipts.len(), MAX_RECEIPTS);
        apply_at(&mut store, &command, NOW + 1).unwrap();
        assert_eq!(store.config.profile_admin_receipts.len(), 1);
    }

    #[test]
    fn failed_save_reloads_disk_and_lost_commit_acknowledgement_is_retryable() {
        let (_temp, mut store) = store(); store.save_config().unwrap();
        let command = add("nas");
        let error = apply_with_save(&mut store, &command, NOW, |_| bail!("before publication")).unwrap_err();
        assert!(error.to_string().contains("outcome uncertain"));
        assert!(!store.config.profiles.contains_key("nas"));
        assert!(store.config.profile_admin_receipts.is_empty());
        let error = apply_with_save(&mut store, &command, NOW, |store| { store.save_config()?; bail!("directory fsync acknowledgement lost") }).unwrap_err();
        assert!(error.to_string().contains(&command.request_id));
        assert!(store.config.profiles.contains_key("nas"));
        let committed = fs::read(store.home.join("config.json")).unwrap();
        let result = apply_at(&mut store, &command, NOW + 1).unwrap();
        assert_eq!(result["saved"], true);
        assert_eq!(fs::read(store.home.join("config.json")).unwrap(), committed);
        let missing = add("missing");
        assert!(apply_with_save(&mut store, &missing, NOW, |store| { fs::remove_file(store.home.join("config.json"))?; bail!("disk unavailable") }).is_err());
        assert!(!store.config.profiles.contains_key("missing"));
        assert!(store.config.profiles.contains_key("nas"));
    }

    #[test]
    fn removal_protects_uploads_cache_and_explicit_domains_but_allows_implicit_defaults() {
        let (_temp, mut store) = store(); apply_at(&mut store, &add("nas"), NOW).unwrap();
        let command = remove("nas");
        store.db.jobs.push(Job { id: "job".into(), profile: "nas".into(), remote: "doc".into(), snapshot: "job".into(), sha256: "a".repeat(64), size: 3, created: "now".into(), status: "failed".into(), attempts: 1, error: None, expected: None, overwrite: false, version: false, resume: false, version_prepared: false });
        assert!(apply_at(&mut store, &command, NOW).unwrap_err().to_string().contains("unfinished uploads"));
        store.db.jobs[0].status = "done".into();
        store.db.cache.insert("cache".into(), CacheItem { profile: "nas".into(), remote: "doc".into(), object: "object".into(), entry: entry(), sha256: "b".repeat(64), pinned: false, accessed: "now".into(), scope: None });
        assert!(apply_at(&mut store, &command, NOW).unwrap_err().to_string().contains("cached files"));
        store.db.cache.clear();
        let configured = vec![domain("finder", "nas", true)]; write(&store.home.join("provider-domains.json"), &configured);
        assert!(apply_at(&mut store, &command, NOW).unwrap_err().to_string().contains("configured Finder domain"));
        fs::remove_file(store.home.join("provider-domains.json")).unwrap();
        assert!(crate::provider::domains(&store.home).unwrap().iter().any(|domain| domain.profile == "nas"));
        apply_at(&mut store, &command, NOW).unwrap();
        assert!(!store.config.profiles.contains_key("nas"));
    }

    #[test]
    fn removal_preserves_retained_download_and_repository_snapshots() {
        let (_temp, mut store) = store(); apply_at(&mut store, &add("nas"), NOW).unwrap();
        let command = remove("nas");
        let checkpoint = store.home.join("transfers/download.json");
        write(&checkpoint, &json!({"profile":"nas","phase":"complete"}));
        let bytes = fs::read(&checkpoint).unwrap();
        assert!(apply_at(&mut store, &command, NOW).is_err()); assert_eq!(fs::read(&checkpoint).unwrap(), bytes);
        fs::remove_file(checkpoint).unwrap();
        let transaction = store.home.join("transactions/tx.json");
        for phase in ["open", "prepared"] {
            write(&transaction, &json!({"profile":"nas","phase":phase}));
            assert!(apply_at(&mut store, &command, NOW).unwrap_err().to_string().contains("repository transactions"));
        }
        write(&transaction, &json!({"profile":"nas","phase":"aborted"}));
        write(&store.home.join("transfers/other.json"), &json!({"profile":"other","phase":"downloading"}));
        fs::write(store.home.join("transactions/ignored.txt"), b"retained snapshot marker").unwrap();
        apply_at(&mut store, &command, NOW).unwrap();
        assert!(transaction.is_file());
        assert_eq!(fs::read(store.home.join("transactions/ignored.txt")).unwrap(), b"retained snapshot marker");
    }

    #[test]
    fn only_known_finished_transaction_phases_allow_profile_removal() {
        for phase in ["committed", "aborted", "damaged"] {
            let (_temp, mut store) = store();
            apply_at(&mut store, &add("nas"), NOW).unwrap();
            let transaction = store.home.join("transactions/retained.json");
            write(&transaction, &json!({"profile":"nas","phase":phase,"snapshots":{"doc":"retained-local-data"}}));
            let journal = fs::read(&transaction).unwrap();
            let config = fs::read(store.home.join("config.json")).unwrap();
            let result = apply_at(&mut store, &remove("nas"), NOW);
            if phase == "damaged" {
                assert!(result.unwrap_err().to_string().contains("unknown retained repository transaction phase"));
                assert_eq!(fs::read(store.home.join("config.json")).unwrap(), config);
                assert!(store.config.profiles.contains_key("nas"));
                assert_eq!(store.config.profile_admin_receipts.len(), 1);
            } else {
                assert_eq!(result.unwrap()["removed"], "nas");
                assert!(!store.config.profiles.contains_key("nas"));
                assert_eq!(store.config.profile_admin_receipts.len(), 2);
            }
            assert_eq!(fs::read(transaction).unwrap(), journal);
        }
    }

    #[test]
    fn orphan_smart_state_blocks_removal_while_other_bound_queues_are_preserved() {
        let (_temp, mut store) = store(); apply_at(&mut store, &add("nas"), NOW).unwrap(); apply_at(&mut store, &add("other"), NOW).unwrap();
        let domain = domain("other-finder", "other", true); write(&store.home.join("provider-domains.json"), &vec![domain.clone()]);
        let binding = crate::provider::journal_binding(&store.config, &domain).unwrap();
        drop(crate::smart::Queue::open(&store, &domain.id, &binding).unwrap());
        crate::smart::set_paused(&store, true).unwrap();
        require_removable(&store, "nas").unwrap();
        let unknown = store.home.join("smart/orphan"); fs::create_dir(&unknown).unwrap();
        assert!(require_removable(&store, "nas").unwrap_err().to_string().contains("unknown retained Smart queue"));
        fs::remove_dir(&unknown).unwrap(); fs::write(&unknown, b"preserved").unwrap();
        assert!(require_removable(&store, "nas").unwrap_err().to_string().contains("unknown retained Smart state"));
        assert_eq!(fs::read(&unknown).unwrap(), b"preserved"); fs::remove_file(unknown).unwrap();
        let queue = store.home.join("smart").join(state::key(&domain.id)).join("queue.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&queue).unwrap()).unwrap(); value["binding"] = json!("a".repeat(64)); write(&queue, &value);
        assert!(require_removable(&store, "nas").is_err());
    }

    #[test]
    fn reference_reads_reject_links_public_files_nonfiles_and_oversize_documents() {
        let (temp, store) = store(); let path = store.home.join("reference.json");
        assert!(read_reference::<Value>(&path).unwrap().is_none());
        write(&path, &json!({"profile":"nas","phase":"open"}));
        assert!(read_reference::<Reference>(&path).unwrap().is_some());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(); assert!(read_reference::<Value>(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let hard = temp.path().join("hard"); fs::hard_link(&path, &hard).unwrap(); assert!(read_reference::<Value>(&path).is_err()); fs::remove_file(hard).unwrap();
        fs::remove_file(&path).unwrap(); symlink("config.json", &path).unwrap(); assert!(read_reference::<Value>(&path).is_err()); fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap(); assert!(read_reference::<Value>(&path).is_err()); fs::remove_dir(&path).unwrap();
        write(&path, &json!({})); OpenOptions::new().write(true).open(&path).unwrap().set_len(MAX_REFERENCE_BYTES + 1).unwrap(); assert!(read_reference::<Value>(&path).unwrap_err().to_string().contains("64 MiB"));
        fs::write(&path, b"not json").unwrap(); assert!(read_reference::<Value>(&path).is_err());
    }

    #[test]
    fn inaccessible_and_unsafe_smart_roots_preserve_profiles() {
        let (temp, mut store) = store(); apply_at(&mut store, &add("nas"), NOW).unwrap();
        let root = store.home.join("smart");
        symlink(temp.path(), &root).unwrap(); assert!(require_removable(&store, "nas").is_err()); fs::remove_file(&root).unwrap();
        fs::create_dir(&root).unwrap(); fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap(); assert!(require_removable(&store, "nas").is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o300)).unwrap(); assert!(require_removable(&store, "nas").is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(&root).unwrap(); fs::write(&root, b"not a directory").unwrap(); assert!(require_removable(&store, "nas").is_err());
        fs::remove_file(&root).unwrap();
        fs::remove_dir(store.home.join("transactions")).unwrap(); assert!(require_removable(&store, "nas").is_err());
        assert!(store.config.profiles.contains_key("nas"));
    }

    #[test]
    fn public_entrypoints_accept_current_ids_and_preserve_legacy_configs() {
        let (_temp, mut store) = store();
        let config: config::Config = serde_json::from_value(json!({"profiles":{}})).unwrap();
        assert!(config.profile_admin_receipts.is_empty());
        assert!(serde_json::to_value(&config).unwrap().get("profile_admin_receipts").is_none());
        let command = Command::new(Change::Add { name: "current".into(), profile: profile() }, None).unwrap();
        assert_eq!(apply(&mut store, &command).unwrap()["saved"], true);
    }
    #[test]
    fn retained_history_requires_explicit_cleanup_and_prepared_versions_remain_protected() {
        let (temp,mut store)=store();apply_at(&mut store,&add("nas"),NOW).unwrap();
        let source=temp.path().join("before-image");fs::write(&source,b"retained bytes").unwrap();
        let version=crate::history::capture_import(&store,"nas","file",&source,"history-admin-removal").unwrap();
        assert!(require_removable(&store,"nas").unwrap_err().to_string().contains("retained file history"));
        let db=rusqlite::Connection::open(store.home.join("history/history.sqlite3")).unwrap();
        let manifest:String=db.query_row("SELECT archive FROM entries WHERE id=?1",[&version.id],|row|row.get(0)).unwrap();let mut manifest:Value=serde_json::from_str(&manifest).unwrap();manifest["public"]["state"]=json!("prepared");
        db.execute("UPDATE entries SET state='prepared',archive=?1 WHERE id=?2",rusqlite::params![serde_json::to_string(&manifest).unwrap(),version.id]).unwrap();
        assert_eq!(crate::history::clear(&store,"nas",true).unwrap()["protected_prepared"],1);
        assert!(apply_at(&mut store,&remove("nas"),NOW+1).unwrap_err().to_string().contains("retained file history"));
        crate::history::confirm(&store,"nas",&version.id,None).unwrap();assert_eq!(crate::history::clear(&store,"nas",true).unwrap()["removed"],1);drop(db);
        assert_eq!(apply_at(&mut store,&remove("nas"),NOW+2).unwrap()["removed"],"nas");
    }

}
