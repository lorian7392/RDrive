//! Durable local mutations for Smart Synchronization. This module never contacts
//! a server and never acknowledges a remote write on the strength of local I/O.
//! The provider journal owns offline metadata/anchors; cache.rs owns clean data.
use crate::{
    backend,
    metadata::{self, MetadataRecord},
    state::{self, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::{
    fd::AsRawFd,
    unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_INDEX: u64 = 64 * 1024 * 1024;
const MAX_REQUEST: usize = 8 * 1024 * 1024;
const BUFFER: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Control {
    pub version: u32,
    pub paused: bool,
    pub updated: Option<String>,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            version: 1,
            paused: false,
            updated: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MutationState {
    Pending,
    Running,
    Interrupted,
    Conflict,
    Failed,
    Done,
    Cancelled,
    Superseded,
}
impl MutationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }
    pub fn active(self) -> bool {
        !matches!(self, Self::Done | Self::Cancelled | Self::Superseded)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FailureKind {
    Offline,
    Conflict,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// A contained filename, never the application's original path.
    pub file: String,
    pub sha256: String,
    pub size: u64,
    pub metadata: MetadataRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutation {
    pub id: String,
    pub sequence: u64,
    pub request: Value,
    pub fingerprint: String,
    pub snapshot: Option<SnapshotInfo>,
    pub payload_discarded: bool,
    pub state: MutationState,
    pub attempts: u32,
    pub created: String,
    pub updated: String,
    pub error: Option<String>,
    /// Filled only by complete(), after the caller confirms remote publication.
    pub response: Option<Value>,
    pub superseded_by: Option<String>,
    /// Immutable replay decision after a conflict. A retry reuses this exact
    /// request and guard; automatic policy never rebases it again.
    #[serde(default)]
    pub resolution: Option<crate::policies::ResolutionPlan>,
}
const STALE_DIRECTORY_CANCELLED: &str = "cancelled stale directory request before remote publication";

impl Mutation {
    pub(crate) fn stale_directory_cancelled(&self) -> bool {
        self.state == MutationState::Cancelled
            && self.error.as_deref() == Some(STALE_DIRECTORY_CANCELLED)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Index {
    version: u32,
    domain: String,
    binding: String,
    next_sequence: u64,
    mutations: BTreeMap<String, Mutation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueStatus {
    pub domain: String,
    pub paused: bool,
    pub counts: BTreeMap<String, usize>,
    /// Retained payloads not confirmed remotely, including explicit cancellations.
    pub dirty_bytes: u64,
    /// All retained snapshots, including completed recovery copies.
    pub retained_bytes: u64,
    pub completed_snapshot_bytes: u64,
    pub superseded_snapshot_bytes: u64,
    /// Files left before journal publication or after an interrupted explicit discard.
    pub unmanaged_snapshot_bytes: u64,
    pub missing_snapshots: usize,
}

pub struct SnapshotLease {
    file: File,
    path: PathBuf,
    pub info: SnapshotInfo,
}
impl SnapshotLease {
    pub fn file(&self) -> &File {
        &self.file
    }
    pub fn try_clone_file(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }
    /// Advisory pathname; prefer the held descriptor for copying.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Holds a domain-local exclusive lock. Callers may release Store's global lock
/// while replaying a request, but must retain this Queue to exclude another worker.
pub struct Queue {
    home: PathBuf,
    root: PathBuf,
    index: Index,
    _lock: File,
}

pub fn control(store: &Store) -> Result<Control> {
    read_control(&store.home)
}
pub fn control_at(home: &Path) -> Result<Control> {
    read_control(home)
}
/// Atomic index reads stay available to the UI while a network worker holds the
/// queue lock. Running is intentionally not converted to Interrupted here.
pub fn read_only(home: &Path, domain: &str, binding: &str) -> Result<Vec<Mutation>> {
    if domain.is_empty()
        || domain.len() > 120
        || domain.chars().any(char::is_control)
        || !valid_hash(binding)
    {
        bail!("invalid Smart domain identity or binding");
    }
    let root = home.join("smart");
    if !existing_private_directory(&root)? {
        return Ok(Vec::new());
    }
    let root = root.join(state::key(domain));
    if !existing_private_directory(&root)? {
        return Ok(Vec::new());
    }
    let Some(bytes) = read_optional(&root.join("queue.json"))? else {
        return Ok(Vec::new());
    };
    let index: Index = serde_json::from_slice(&bytes).context("invalid Smart mutation journal")?;
    validate_index(&index, domain, binding)?;
    let mut mutations: Vec<_> = index.mutations.into_values().collect();
    mutations.sort_by_key(|mutation| mutation.sequence);
    Ok(mutations)
}
pub fn set_paused(store: &Store, paused: bool) -> Result<Control> {
    let root = smart_root(&store.home)?;
    let result = Control {
        version: 1,
        paused,
        updated: Some(state::now()),
    };
    state::atomic_json(&root.join("control.json"), &result)?;
    Ok(result)
}

impl Queue {
    pub fn open(store: &Store, domain: &str, binding: &str) -> Result<Self> {
        if domain.is_empty()
            || domain.len() > 120
            || domain.chars().any(char::is_control)
            || !valid_hash(binding)
        {
            bail!("invalid Smart domain identity or backend binding");
        }
        let root = smart_root(&store.home)?.join(state::key(domain));
        private_directory(&root)?;
        private_directory(&root.join("dirty"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("queue.lock"))?;
        private_file(&lock)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Smart domain already has an active local worker");
        }
        let path = root.join("queue.json");
        let mut index: Index = match read_optional(&path)? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).context("invalid Smart mutation journal")?
            }
            None => Index {
                version: 1,
                domain: domain.into(),
                binding: binding.into(),
                next_sequence: 1,
                mutations: BTreeMap::new(),
            },
        };
        validate_index(&index, domain, binding)?;
        let mut interrupted = false;
        for mutation in index.mutations.values_mut() {
            if mutation.state == MutationState::Running {
                mutation.state = MutationState::Interrupted;
                mutation.updated = state::now();
                mutation.error = Some("previous worker ended without confirming remote completion; immutable local edit retained".into());
                interrupted = true;
            }
        }
        if interrupted || !path.exists() {
            save_index(&path, &index)?;
        }
        Ok(Self {
            home: store.home.clone(),
            root,
            index,
            _lock: lock,
        })
    }

    pub fn domain(&self) -> &str {
        &self.index.domain
    }
    pub fn binding(&self) -> &str {
        &self.index.binding
    }
    pub fn list(&self) -> Vec<Mutation> {
        let mut list: Vec<_> = self.index.mutations.values().cloned().collect();
        list.sort_by_key(|mutation| mutation.sequence);
        list
    }
    pub fn get(&self, id: &str) -> Result<Mutation> {
        Ok(self
            .index
            .mutations
            .get(id)
            .context("Smart mutation not found")?
            .clone())
    }
    pub fn status(&self) -> Result<QueueStatus> {
        let mut status = QueueStatus {
            domain: self.index.domain.clone(),
            paused: read_control(&self.home)?.paused,
            counts: BTreeMap::new(),
            dirty_bytes: 0,
            retained_bytes: 0,
            completed_snapshot_bytes: 0,
            superseded_snapshot_bytes: 0,
            unmanaged_snapshot_bytes: 0,
            missing_snapshots: 0,
        };
        for mutation in self.index.mutations.values() {
            *status
                .counts
                .entry(mutation.state.as_str().into())
                .or_default() += 1;
            if let Some(snapshot) = &mutation.snapshot {
                if !mutation.payload_discarded {
                    if fs::symlink_metadata(self.root.join("dirty").join(&snapshot.file)).is_err() {
                        status.missing_snapshots += 1;
                    }
                    status.retained_bytes = status
                        .retained_bytes
                        .checked_add(snapshot.size)
                        .context("Smart snapshot size overflow")?;
                    if mutation.state == MutationState::Done {
                        status.completed_snapshot_bytes = status
                            .completed_snapshot_bytes
                            .checked_add(snapshot.size)
                            .context("Smart snapshot size overflow")?;
                    } else if mutation.state == MutationState::Superseded {
                        status.superseded_snapshot_bytes = status
                            .superseded_snapshot_bytes
                            .checked_add(snapshot.size)
                            .context("Smart snapshot size overflow")?;
                    } else {
                        status.dirty_bytes = status
                            .dirty_bytes
                            .checked_add(snapshot.size)
                            .context("Smart snapshot size overflow")?;
                    }
                }
            }
        }
        let retained: std::collections::BTreeSet<_> = self
            .index
            .mutations
            .values()
            .filter(|mutation| !mutation.payload_discarded)
            .filter_map(|mutation| mutation.snapshot.as_ref().map(|info| info.file.as_str()))
            .collect();
        for item in fs::read_dir(self.root.join("dirty"))? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_str().context("invalid Smart snapshot filename")?;
            if !retained.contains(name) {
                let info = fs::symlink_metadata(item.path())?;
                if !info.is_file() || info.file_type().is_symlink() {
                    bail!("unsafe unexpected entry in Smart snapshot directory");
                }
                status.unmanaged_snapshot_bytes = status
                    .unmanaged_snapshot_bytes
                    .checked_add(info.len())
                    .context("Smart snapshot size overflow")?;
            }
        }
        Ok(status)
    }

    /// Snapshots and fsyncs the edit before advertising Pending. Only the known
    /// provider mutation fields are persisted: auth tokens and source paths never are.
    pub fn enqueue(&mut self, request: Value, source: Option<&File>) -> Result<Mutation> {
        let mut request = sanitize_request(request, &self.index.domain)?;
        if request["op"] == "delete" && source.is_some() {
            bail!("delete cannot carry file contents");
        }
        // A system-created file may be saved again before creation is confirmed.
        // Different desired contents need separate provider receipt keys and
        // separate immutable snapshots, while sharing their original local ID.
        let mut snapshot = source
            .map(|file| snapshot_info(&"0".repeat(64), file))
            .transpose()?;
        normalize_create(&mut request, snapshot.as_ref())?;
        let id = mutation_id(&self.index.binding, &request)?;
        if let Some(info) = snapshot.as_mut() {
            info.file = format!("{id}.data");
        }
        let fingerprint = mutation_fingerprint(&request, snapshot.as_ref())?;
        if let Some(previous) = self.index.mutations.get(&id) {
            if previous.fingerprint != fingerprint {
                bail!(
                    "version_conflict: Smart request identity was reused for a different local edit"
                );
            }
            return Ok(previous.clone());
        }
        if let (Some(file), Some(info)) = (source, snapshot.as_ref()) {
            capture_snapshot(&self.root, file, info)?;
        }
        let now = state::now();
        let mutation = Mutation {
            id: id.clone(),
            sequence: self.index.next_sequence,
            request,
            fingerprint,
            snapshot,
            payload_discarded: false,
            state: MutationState::Pending,
            attempts: 0,
            created: now.clone(),
            updated: now,
            error: None,
            response: None,
            superseded_by: None,
            resolution: None,
        };
        let mut next = self.index.clone();
        next.next_sequence = next
            .next_sequence
            .checked_add(1)
            .context("Smart sequence overflow")?;
        for previous in next.mutations.values_mut() {
            if safely_supersedes(&mutation, previous) {
                previous.state = MutationState::Superseded;
                previous.superseded_by = Some(id.clone());
                previous.updated = mutation.created.clone();
                previous.error = Some("a later durable local edit superseded this never-started mutation; snapshot retained".into());
            }
        }
        next.mutations.insert(id, mutation.clone());
        self.commit(next)?;
        Ok(mutation)
    }

    pub fn find_pending_for_item(&self, item: &str) -> Option<Mutation> {
        self.index
            .mutations
            .values()
            .filter(|mutation| {
                mutation.state.active()
                    && (mutation.request.get("id").and_then(Value::as_str) == Some(item)
                        || mutation
                            .request
                            .get("client_item_id")
                            .and_then(Value::as_str)
                            == Some(item)
                        || mutation
                            .request
                            .get("smart_origin_id")
                            .and_then(Value::as_str)
                            == Some(item))
            })
            .max_by_key(|mutation| mutation.sequence)
            .cloned()
    }

    pub fn snapshot(&self, id: &str) -> Result<Option<SnapshotLease>> {
        let mutation = self
            .index
            .mutations
            .get(id)
            .context("Smart mutation not found")?;
        let Some(info) = &mutation.snapshot else {
            return Ok(None);
        };
        if mutation.payload_discarded {
            bail!("Smart snapshot was explicitly discarded");
        }
        let path = self.root.join("dirty").join(&info.file);
        let file = open_read(&path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            bail!("Smart snapshot is busy");
        }
        verify_snapshot(&file, info)?;
        Ok(Some(SnapshotLease {
            file,
            path,
            info: info.clone(),
        }))
    }

    pub fn start(&mut self, id: &str) -> Result<Mutation> {
        if read_control(&self.home)?.paused {
            bail!("Smart synchronization is paused; local edits retained");
        }
        let mutation = self.get(id)?;
        if !matches!(
            mutation.state,
            MutationState::Pending | MutationState::Interrupted
        ) {
            bail!("Smart mutation requires explicit retry or is already complete");
        }
        let _verified = self.snapshot(id)?;
        self.update(id, |mutation| {
            mutation.state = MutationState::Running;
            mutation.attempts = mutation.attempts.saturating_add(1);
            mutation.error = None;
        })
    }

    pub fn fail(&mut self, id: &str, kind: FailureKind, message: &str) -> Result<Mutation> {
        if self.get(id)?.state == MutationState::Done {
            bail!("confirmed Smart receipt cannot be replaced by a failure");
        }
        self.update(id, |mutation| {
            mutation.state = match kind {
                FailureKind::Offline => MutationState::Interrupted,
                FailureKind::Conflict => MutationState::Conflict,
                FailureKind::Other => MutationState::Failed,
            };
            mutation.error = Some(message.chars().take(4096).collect());
            mutation.response = None;
        })
    }

    /// Preserve a failed automatic preparation diagnostic without changing its
    /// stopped state or scheduling an unconditional network retry.
    pub fn set_error(&mut self, id: &str, message: &str) -> Result<Mutation> {
        if !matches!(
            self.get(id)?.state,
            MutationState::Conflict | MutationState::Failed
        ) {
            bail!("attention detail requires stopped Smart work");
        }
        self.update(id, |mutation| {
            mutation.error = Some(message.chars().take(4096).collect())
        })
    }

    /// Call only with the successful response of a remotely confirmed replay.
    /// Local enqueue, cached reads and snapshot creation must never call this.
    pub fn complete(&mut self, id: &str, response: Value) -> Result<Mutation> {
        let current = self.get(id)?;
        if current.state == MutationState::Done {
            if current.response.as_ref() != Some(&response) {
                bail!("confirmed Smart response changed");
            }
            return Ok(current);
        }
        if !matches!(
            current.state,
            MutationState::Running | MutationState::Interrupted
        ) {
            bail!("Smart completion requires a replay attempt");
        }
        self.update(id, |mutation| {
            mutation.state = MutationState::Done;
            mutation.response = Some(response);
            mutation.error = None;
        })
    }

    /// Explicit retry preserves the original base versions; it never forces an overwrite.
    pub fn retry(&mut self, id: &str) -> Result<Mutation> {
        let current = self.get(id)?;
        if current.stale_directory_cancelled() {
            bail!("stale directory request was cancelled; use the refreshed Finder item");
        }
        if matches!(
            current.state,
            MutationState::Done | MutationState::Running | MutationState::Superseded
        ) {
            bail!("Smart mutation is complete, superseded or currently running");
        }
        let _verified = self.snapshot(id)?;
        self.update(id, |mutation| {
            mutation.state = MutationState::Pending;
            mutation.error = None;
            mutation.response = None;
        })
    }

    pub fn plan_resolution(
        &mut self,
        id: &str,
        strategy: crate::policies::ConflictStrategy,
        request: Value,
        target_path: String,
        version: Option<crate::operations::Version>,
        repository_snapshot: Option<String>,
        explicit: bool,
    ) -> Result<Mutation> {
        use crate::policies::{ConflictStrategy, ResolutionPlan};
        let current = self.get(id)?;
        if !matches!(
            current.state,
            MutationState::Conflict | MutationState::Failed
        ) {
            bail!("resolution requires a stopped conflict or failure; retry an interrupted receipt first");
        }
        if current.resolution.is_some() && !explicit {
            bail!("automatic conflict resolution was already attempted; new external changes require an explicit decision");
        }
        if strategy == ConflictStrategy::Stop {
            bail!("choose keep-both, replace or remote for an explicit resolution");
        }
        if !matches!(current.request["op"].as_str(), Some("create" | "modify"))
            || self.snapshot(id)?.is_none()
        {
            bail!("conflict resolution requires retained file contents");
        }
        let request = sanitize_request(request, &self.index.domain)?;
        if (strategy == ConflictStrategy::KeepBoth && request["op"] != "create")
            || (strategy == ConflictStrategy::Replace && request["op"] != "modify")
            || target_path.is_empty()
            || backend::normalize(&target_path)? != target_path
        {
            bail!("invalid conflict resolution target");
        }
        if strategy == ConflictStrategy::Replace
            && version.is_none()
            && repository_snapshot.is_none()
        {
            bail!("replace requires a confirmed prior-version snapshot");
        }
        if repository_snapshot
            .as_deref()
            .is_some_and(|id| !valid_hash(id))
        {
            bail!("invalid prior repository snapshot");
        }
        let fingerprint = resolution_fingerprint(
            id,
            &current.fingerprint,
            strategy,
            &request,
            &target_path,
            &version,
            &repository_snapshot,
        );
        let plan = ResolutionPlan {
            strategy,
            request,
            target_path,
            version,
            repository_snapshot,
            fingerprint,
            created: state::now(),
        };
        self.update(id, |mutation| {
            mutation.resolution = Some(plan);
            mutation.state = MutationState::Pending;
            mutation.error = None;
            mutation.response = None;
        })
    }
    pub fn cancel(&mut self, id: &str) -> Result<Mutation> {
        let current = self.get(id)?;
        if current.stale_directory_cancelled() {
            return Ok(current);
        }
        if matches!(
            current.state,
            MutationState::Done | MutationState::Running | MutationState::Superseded
        ) {
            bail!("cannot cancel confirmed, superseded or running Smart work");
        }
        self.update(id, |mutation| {
            mutation.state = MutationState::Cancelled;
            mutation.error = Some("cancelled explicitly; local snapshot retained".into());
        })
    }
    pub(crate) fn cancel_stale_directory(&mut self, mutation: &Mutation) -> Result<Mutation> {
        self.update(&mutation.id, |mutation| {
            mutation.state = MutationState::Cancelled;
            mutation.error = Some(STALE_DIRECTORY_CANCELLED.into());
        })
    }
    pub fn discard_snapshot(&mut self, id: &str) -> Result<()> {
        let mutation = self.get(id)?;
        if !matches!(
            mutation.state,
            MutationState::Done | MutationState::Cancelled | MutationState::Superseded
        ) {
            bail!("pending/conflicting Smart edits cannot be evicted");
        }
        if mutation.payload_discarded {
            return Ok(());
        }
        let Some(info) = mutation.snapshot else {
            return Ok(());
        };
        let path = self.root.join("dirty").join(info.file);
        let file = open_read(&path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Smart snapshot is in use");
        }
        // Persist the explicit discard before unlinking. A crash can leave an
        // unreferenced recovery payload, never an advertised missing dirty edit.
        self.update(id, |mutation| mutation.payload_discarded = true)?;
        fs::remove_file(path)?;
        File::open(self.root.join("dirty"))?.sync_all()?;
        Ok(())
    }

    fn update(&mut self, id: &str, change: impl FnOnce(&mut Mutation)) -> Result<Mutation> {
        let mut next = self.index.clone();
        let mutation = next
            .mutations
            .get_mut(id)
            .context("Smart mutation not found")?;
        change(mutation);
        mutation.updated = state::now();
        let result = mutation.clone();
        self.commit(next)?;
        Ok(result)
    }
    fn commit(&mut self, next: Index) -> Result<()> {
        validate_index(&next, &self.index.domain, &self.index.binding)?;
        save_index(&self.root.join("queue.json"), &next)?;
        self.index = next;
        Ok(())
    }
}

fn sanitize_request(request: Value, domain: &str) -> Result<Value> {
    let object = request
        .as_object()
        .context("Smart request must be an object")?;
    let allowed = [
        "v",
        "op",
        "domain",
        "id",
        "request_id",
        "parent_id",
        "name",
        "kind",
        "tag_data_hex",
        "extended_attributes",
        "base_content_version",
        "base_metadata_version",
        "client_item_id",
        "smart_origin_id",
        "recursive",
    ];
    let clean: serde_json::Map<_, _> = allowed
        .into_iter()
        .filter_map(|name| object.get(name).map(|value| (name.into(), value.clone())))
        .collect();
    let clean = Value::Object(clean);
    if clean["v"] != 1
        || clean["domain"].as_str() != Some(domain)
        || !matches!(clean["op"].as_str(), Some("create" | "modify" | "delete"))
    {
        bail!("invalid or cross-domain Smart mutation");
    }
    let required = if clean["op"] == "create" {
        vec!["client_item_id"]
    } else {
        vec![
            "request_id",
            "id",
            "base_content_version",
            "base_metadata_version",
        ]
    };
    for field in required {
        let value = clean[field]
            .as_str()
            .context("Smart mutation is missing its identity or base version")?;
        if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
            bail!("invalid Smart mutation identity/version");
        }
    }
    if let Some(origin) = clean
        .get("smart_origin_id")
        .filter(|value| !value.is_null())
    {
        let origin = origin.as_str().context("invalid Smart creation origin")?;
        if origin.is_empty() || origin.len() > 4096 || origin.chars().any(char::is_control) {
            bail!("invalid Smart creation origin");
        }
    }
    if let Some(name) = clean.get("name").filter(|value| !value.is_null()) {
        let name = name.as_str().context("invalid Smart name")?;
        if name.is_empty()
            || backend::normalize(name)? != name
            || name.contains('/')
            || name.chars().any(char::is_control)
        {
            bail!("unsafe Smart item name");
        }
    }
    if serde_json::to_vec(&clean)?.len() > MAX_REQUEST {
        bail!("Smart mutation request is too large");
    }
    Ok(clean)
}
fn mutation_id(binding: &str, request: &Value) -> Result<String> {
    let identity = if request["op"] == "create" {
        format!(
            "create:{}",
            request["client_item_id"]
                .as_str()
                .context("missing create identity")?
        )
    } else {
        format!(
            "request:{}",
            request["request_id"]
                .as_str()
                .context("missing request identity")?
        )
    };
    Ok(state::key(&format!("{binding}\0{identity}")))
}
fn normalize_create(request: &mut Value, source: Option<&SnapshotInfo>) -> Result<()> {
    if request["op"] != "create" {
        return Ok(());
    }
    let origin = request
        .get("smart_origin_id")
        .and_then(Value::as_str)
        .or_else(|| request["client_item_id"].as_str())
        .context("missing Smart creation origin")?
        .to_owned();
    let mut desired = request.clone();
    for field in ["request_id", "client_item_id", "smart_origin_id"] {
        desired.as_object_mut().unwrap().remove(field);
    }
    let content = source
        .map(|info| {
            Ok::<_, anyhow::Error>((info.sha256.clone(), info.size, info.metadata.fingerprint()?))
        })
        .transpose()?;
    let client = state::key(&serde_json::to_string(&(&origin, desired, content))?);
    request["smart_origin_id"] = Value::String(origin);
    request["client_item_id"] = Value::String(client);
    Ok(())
}
fn mutation_fingerprint(request: &Value, source: Option<&SnapshotInfo>) -> Result<String> {
    let mut identity = request.clone();
    if identity["op"] == "create" {
        identity.as_object_mut().unwrap().remove("request_id");
    }
    let snapshot = source
        .map(|info| {
            Ok::<_, anyhow::Error>((info.sha256.clone(), info.size, info.metadata.fingerprint()?))
        })
        .transpose()?;
    Ok(state::key(&serde_json::to_string(&(identity, snapshot))?))
}
fn safely_supersedes(new: &Mutation, old: &Mutation) -> bool {
    if old.resolution.is_some() {
        return false;
    }
    if new.request["op"] == "create" && old.request["op"] == "create" {
        let origin = |request: &Value| {
            request
                .get("smart_origin_id")
                .filter(|value| !value.is_null())
                .unwrap_or(&request["client_item_id"])
                .clone()
        };
        return old.state == MutationState::Pending
            && old.attempts == 0
            && origin(&new.request) == origin(&old.request)
            && ["name", "parent_id", "kind"]
                .iter()
                .all(|field| new.request[*field] == old.request[*field])
            && (old.snapshot.is_none() || new.snapshot.is_some())
            && ["tag_data_hex", "extended_attributes"].iter().all(|field| {
                old.request.get(*field).is_none_or(Value::is_null)
                    || new
                        .request
                        .get(*field)
                        .is_some_and(|value| !value.is_null())
            });
    }
    if new.request["op"] != "modify"
        || old.request["op"] != "modify"
        || old.state != MutationState::Pending
        || old.attempts != 0
        || new.request["id"] != old.request["id"]
        || new.request["base_content_version"] != old.request["base_content_version"]
        || new.request["base_metadata_version"] != old.request["base_metadata_version"]
        || old.snapshot.is_some() && new.snapshot.is_none()
    {
        return false;
    }
    // A later content-only callback does not implicitly include an earlier
    // rename/tag/reparent edit. Retain those as separate intent.
    ["name", "parent_id", "tag_data_hex", "extended_attributes"]
        .iter()
        .all(|field| {
            old.request.get(*field).is_none_or(Value::is_null)
                || new
                    .request
                    .get(*field)
                    .is_some_and(|value| !value.is_null())
        })
}
fn validate_index(index: &Index, domain: &str, binding: &str) -> Result<()> {
    if index.version != 1
        || index.domain != domain
        || index.binding != binding
        || index.next_sequence == 0
    {
        bail!("Smart domain/backend scope changed; local edits preserved");
    }
    let mut sequences = std::collections::BTreeSet::new();
    for (id, mutation) in &index.mutations {
        let clean = sanitize_request(mutation.request.clone(), domain)?;
        if clean["op"] == "create"
            && clean
                .get("smart_origin_id")
                .is_some_and(|value| !value.is_null())
        {
            let mut normalized = clean.clone();
            normalize_create(&mut normalized, mutation.snapshot.as_ref())?;
            if normalized != clean {
                bail!("invalid Smart content-bound creation identity");
            }
        }
        if &clean != &mutation.request
            || !valid_hash(id)
            || &mutation.id != id
            || mutation_id(binding, &clean)? != *id
            || mutation.sequence == 0
            || mutation.sequence >= index.next_sequence
            || !sequences.insert(mutation.sequence)
        {
            bail!("invalid Smart mutation journal identity");
        }
        if let Some(info) = &mutation.snapshot {
            info.metadata.validate()?;
            if info.file != format!("{id}.data")
                || !valid_hash(&info.sha256)
                || info.metadata.content_sha256 != info.sha256
            {
                bail!("unsafe or corrupt Smart snapshot reference");
            }
        } else if mutation.payload_discarded {
            bail!("invalid Smart discarded payload record");
        }
        if mutation.fingerprint != mutation_fingerprint(&clean, mutation.snapshot.as_ref())?
            || (mutation.state == MutationState::Done) != mutation.response.is_some()
            || mutation.payload_discarded
                && !matches!(
                    mutation.state,
                    MutationState::Done | MutationState::Cancelled | MutationState::Superseded
                )
            || (mutation.state == MutationState::Superseded) != mutation.superseded_by.is_some()
        {
            bail!("invalid Smart mutation state or fingerprint");
        }
        if let Some(plan) = &mutation.resolution {
            let request = sanitize_request(plan.request.clone(), domain)?;
            if request != plan.request
                || plan.strategy == crate::policies::ConflictStrategy::Stop
                || !matches!(clean["op"].as_str(), Some("create" | "modify"))
                || mutation.snapshot.is_none()
                || plan.target_path.is_empty()
                || backend::normalize(&plan.target_path)? != plan.target_path
                || plan.fingerprint
                    != resolution_fingerprint(
                        id,
                        &mutation.fingerprint,
                        plan.strategy,
                        &plan.request,
                        &plan.target_path,
                        &plan.version,
                        &plan.repository_snapshot,
                    )
            {
                bail!("invalid persisted Smart resolution plan");
            }
            if plan.strategy == crate::policies::ConflictStrategy::Replace
                && plan.version.is_none()
                && plan.repository_snapshot.is_none()
            {
                bail!("replace plan lost its prior-version snapshot");
            }
            if plan
                .repository_snapshot
                .as_deref()
                .is_some_and(|id| !valid_hash(id))
            {
                bail!("invalid persisted prior repository snapshot");
            }
        }
        if let Some(replacement) = &mutation.superseded_by {
            if replacement == id
                || index
                    .mutations
                    .get(replacement)
                    .is_none_or(|next| next.sequence <= mutation.sequence)
            {
                bail!("invalid Smart supersession history");
            }
        }
    }
    Ok(())
}
fn resolution_fingerprint(
    id: &str,
    original: &str,
    strategy: crate::policies::ConflictStrategy,
    request: &Value,
    target: &str,
    version: &Option<crate::operations::Version>,
    repository_snapshot: &Option<String>,
) -> String {
    // This closed schema contains only strings, integers, booleans, JSON
    // values and derived enums/structs. It has no custom serializer or invalid
    // map key, and an owned String writer cannot fail. Keep the original
    // tuple/struct field order: persisted resolution fingerprints depend on it.
    let encoded = serde_json::to_string(&(
        id,
        original,
        strategy,
        request,
        target,
        version,
        repository_snapshot,
    ))
    .expect("the closed resolution fingerprint schema is JSON-serializable");
    state::key(&encoded)
}
fn save_index(path: &Path, index: &Index) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(index)?;
    if bytes.len() as u64 > MAX_INDEX {
        bail!("Smart mutation journal exceeds 64 MiB; edits were not acknowledged");
    }
    state::atomic_write(path, &bytes)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn private_file(file: &File) -> Result<()> {
    let info = file.metadata()?;
    if !info.is_file()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o077 != 0
        || info.nlink() != 1
    {
        bail!("Smart state must be an owned private regular file without hard links");
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    existing_private_directory(path)?
        .then_some(())
        .context("Smart directory disappeared")
}
fn existing_private_directory(path: &Path) -> Result<bool> {
    let info = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !info.is_dir()
        || info.file_type().is_symlink()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o077 != 0
    {
        bail!("Smart directories must be owned private real directories");
    }
    Ok(true)
}
fn smart_root(home: &Path) -> Result<PathBuf> {
    let root = home.join("smart");
    private_directory(&root)?;
    Ok(root)
}
fn open_read(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    private_file(&file)?;
    Ok(file)
}
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    read_optional_with(path, |file, bytes| Read::by_ref(file).take(MAX_INDEX + 1).read_to_end(bytes))
}
fn read_optional_with(path: &Path, read: impl FnOnce(&mut File,&mut Vec<u8>)->std::io::Result<usize>) -> Result<Option<Vec<u8>>> {
    let mut file = match open_read(path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_INDEX {
        bail!("Smart state exceeds 64 MiB");
    }
    let mut bytes = Vec::new();
    read(&mut file,&mut bytes)?;
    if bytes.len() as u64 > MAX_INDEX {
        bail!("Smart state grew beyond allowed size");
    }
    Ok(Some(bytes))
}
fn read_control(home: &Path) -> Result<Control> {
    let root = home.join("smart");
    if !existing_private_directory(&root)? {
        return Ok(Control::default());
    }
    let control: Control = match read_optional(&root.join("control.json"))? {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => Control::default(),
    };
    if control.version != 1 {
        bail!("invalid Smart pause/resume state");
    }
    Ok(control)
}
fn verify_snapshot(file: &File, info: &SnapshotInfo) -> Result<()> {
    if file.metadata()?.len() != info.size || hash_file(file)? != info.sha256 {
        bail!("Smart immutable local snapshot changed; upload refused");
    }
    let captured = metadata::capture_file(file, &Default::default())?;
    if captured.fingerprint()? != info.metadata.fingerprint()? {
        bail!("Smart snapshot metadata changed; upload refused");
    }
    Ok(())
}
fn hash_file(file: &File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; BUFFER];
    let mut position = 0;
    loop {
        let count = file.read_at(&mut buffer, position)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        position += count as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn snapshot_info(id: &str, source: &File) -> Result<SnapshotInfo> {
    if !source.metadata()?.is_file() {
        bail!("Smart edited contents must be a regular file descriptor");
    }
    let metadata = metadata::capture_file(source, &Default::default())?;
    Ok(SnapshotInfo {
        file: format!("{id}.data"),
        sha256: metadata.content_sha256.clone(),
        size: source.metadata()?.len(),
        metadata,
    })
}
fn capture_snapshot(root: &Path, source: &File, info: &SnapshotInfo) -> Result<()> {
    let directory = root.join("dirty");
    let path = directory.join(&info.file);
    match open_read(&path) {
        Ok(file) => {
            verify_snapshot(&file, info)?;
            return Ok(());
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            ()
        }
        Err(error) => return Err(error),
    }
    let temporary = directory.join(format!(".snapshot-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        let mut buffer = vec![0; BUFFER];
        let mut position = 0;
        loop {
            let count = source.read_at(&mut buffer, position)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            position += count as u64;
        }
        if position != info.size
            || hash_file(&output)? != info.sha256
            || hash_file(source)? != info.sha256
        {
            bail!("local contents changed while retaining Smart edit; retry after saving");
        }
        metadata::apply_file(&output, &info.metadata, &Default::default())?;
        output.set_permissions(fs::Permissions::from_mode(0o400))?;
        output.sync_all()?;
        fs::hard_link(&temporary, &path)
            .context("Smart snapshot destination already exists; no local edit was overwritten")?;
        fs::remove_file(&temporary)?;
        File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use serde_json::json;
    fn setup()->(tempfile::TempDir,Store,Queue,Mutation) {
        let temp=tempfile::tempdir().unwrap();let store=Store::open(temp.path().join("state")).unwrap();let mut queue=Queue::open(&store,"domain",&"a".repeat(64)).unwrap();
        let source=temp.path().join("source");fs::write(&source,b"retained content").unwrap();
        let mutation=queue.enqueue(json!({"v":1,"op":"modify","domain":"domain","id":"file","request_id":"first","base_content_version":"c","base_metadata_version":"m"}),Some(&File::open(source).unwrap())).unwrap();
        (temp,store,queue,mutation)
    }
    #[test]
    fn every_state_has_a_stable_external_name_and_requests_reject_unsafe_fields() {
        for (state,name,active) in [(MutationState::Pending,"pending",true),(MutationState::Running,"running",true),(MutationState::Interrupted,"interrupted",true),(MutationState::Conflict,"conflict",true),(MutationState::Failed,"failed",true),(MutationState::Done,"done",false),(MutationState::Cancelled,"cancelled",false),(MutationState::Superseded,"superseded",false)] {assert_eq!(state.as_str(),name);assert_eq!(state.active(),active);}
        let request=json!({"v":1,"op":"modify","domain":"domain","id":"file","request_id":"first","base_content_version":"c","base_metadata_version":"m"});
        for (field,value) in [("domain",json!("wrong")),("v",json!(2)),("op",json!("list")),("id",json!("")),("request_id",json!("bad\n")),("smart_origin_id",json!(4)),("smart_origin_id",json!("")),("name",json!(4)),("name",json!("parent/child"))] {
            let mut invalid=request.clone();invalid[field]=value;assert!(sanitize_request(invalid,"domain").is_err());
        }
        assert!(sanitize_request(json!(null),"domain").is_err());
        let mut huge=request;huge["extended_attributes"]=json!({"user.large#S":"0".repeat(MAX_REQUEST)});assert!(sanitize_request(huge,"domain").is_err());
    }
    #[test]
    fn journal_corruption_cannot_invent_receipts_snapshots_resolution_or_supersession() {
        let (_temp,_store,queue,mutation)=setup();let base=queue.index.clone();
        let check=|index:&Index|assert!(validate_index(index,"domain",&"a".repeat(64)).is_err());
        let mut bad=base.clone();bad.version=2;check(&bad);
        let mut bad=base.clone();bad.mutations.get_mut(&mutation.id).unwrap().id="wrong".into();check(&bad);
        let mut bad=base.clone();bad.mutations.get_mut(&mutation.id).unwrap().snapshot.as_mut().unwrap().file="../escape".into();check(&bad);
        let mut bad=base.clone();let item=bad.mutations.get_mut(&mutation.id).unwrap();item.snapshot=None;item.payload_discarded=true;check(&bad);
        let mut bad=base.clone();bad.mutations.get_mut(&mutation.id).unwrap().fingerprint="wrong".into();check(&bad);
        let mut bad=base.clone();bad.mutations.get_mut(&mutation.id).unwrap().state=MutationState::Done;check(&bad);
        let mut bad=base.clone();let item=bad.mutations.get_mut(&mutation.id).unwrap();item.state=MutationState::Superseded;item.superseded_by=Some(item.id.clone());check(&bad);
        for (strategy,repository) in [(crate::policies::ConflictStrategy::Stop,None),(crate::policies::ConflictStrategy::Replace,None),(crate::policies::ConflictStrategy::Replace,Some("invalid".to_owned()))] {
            let mut bad=base.clone();let item=bad.mutations.get_mut(&mutation.id).unwrap();
            let mut plan=crate::policies::ResolutionPlan{strategy,request:item.request.clone(),target_path:"file".into(),version:None,repository_snapshot:repository,fingerprint:String::new(),created:state::now()};
            plan.fingerprint=resolution_fingerprint(&item.id,&item.fingerprint,plan.strategy,&plan.request,&plan.target_path,&plan.version,&plan.repository_snapshot);item.resolution=Some(plan);check(&bad);
        }
        let mut bad=base;bad.mutations.get_mut(&mutation.id).unwrap().request["auth_token"]=json!("must-not-persist");check(&bad);
    }
    #[test]
    fn invalid_native_files_and_changed_snapshot_metadata_never_become_upload_sources() {
        use std::os::unix::fs::symlink;
        let (temp,store,queue,mutation)=setup();let info=mutation.snapshot.unwrap();let snapshot=queue.root.join("dirty").join(&info.file);
        let mut wrong=info.clone();wrong.metadata.attributes.insert("user.missing".into(),"00".into());
        assert!(verify_snapshot(&File::open(&snapshot).unwrap(),&wrong).unwrap_err().to_string().contains("metadata changed"));
        assert!(snapshot_info("test",&File::open(temp.path()).unwrap()).is_err());
        let source=temp.path().join("new-source");fs::write(&source,b"different data").unwrap();
        let mut wrong=info.clone();wrong.file="new.data".into();
        assert!(capture_snapshot(&queue.root,&File::open(&source).unwrap(),&wrong).is_err());assert!(!queue.root.join("dirty/new.data").exists());
        let alias=queue.root.join("dirty/alias.data");symlink(&source,&alias).unwrap();wrong.file="alias.data".into();assert!(capture_snapshot(&queue.root,&File::open(&source).unwrap(),&wrong).is_err());
        assert!(read_optional(&alias).is_err());assert!(private_file(&File::open(temp.path()).unwrap()).is_err());
        let oversized=temp.path().join("oversized");let file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&oversized).unwrap();file.set_len(MAX_INDEX+1).unwrap();assert!(read_optional(&oversized).is_err());
        state::atomic_json(&queue.root.parent().unwrap().join("control.json"),&json!({"version":2,"paused":false,"updated":null})).unwrap();assert!(control(&store).is_err());
    }
    #[test]
    fn queue_guards_keep_confirmed_receipts_and_busy_or_pending_snapshots() {
        let (_temp,_store,mut queue,mutation)=setup();assert_eq!(queue.domain(),"domain");assert_eq!(queue.binding(),"a".repeat(64));
        assert!(queue.set_error(&mutation.id,"invalid pending attention").is_err());
        assert!(queue.complete(&mutation.id,json!({})).is_err());assert!(queue.discard_snapshot(&mutation.id).is_err());
        queue.start(&mutation.id).unwrap();assert!(queue.cancel(&mutation.id).is_err());
        queue.complete(&mutation.id,json!({"receipt":1})).unwrap();
        assert!(queue.fail(&mutation.id,FailureKind::Other,"should-not-replace-success").is_err());
        assert!(queue.complete(&mutation.id,json!({"receipt":2})).is_err());assert!(queue.cancel(&mutation.id).is_err());
        let lease=queue.snapshot(&mutation.id).unwrap().unwrap();assert!(queue.discard_snapshot(&mutation.id).is_err());drop(lease);
        queue.discard_snapshot(&mutation.id).unwrap();queue.discard_snapshot(&mutation.id).unwrap();assert!(queue.snapshot(&mutation.id).is_err());
    }
    #[test]
    fn absent_and_invalid_domains_stay_read_only_and_stopped_work_needs_explicit_retry() {
        let (_temp,store,mut queue,mutation)=setup();
        assert!(read_only(&store.home,"",&"a".repeat(64)).is_err());assert!(read_only(&store.home,"missing-domain",&"a".repeat(64)).unwrap().is_empty());
        assert!(Queue::open(&store,"",&"a".repeat(64)).is_err());
        let untouched=tempfile::tempdir().unwrap();assert!(read_only(untouched.path(),"domain",&"a".repeat(64)).unwrap().is_empty());assert!(!control_at(untouched.path()).unwrap().paused);
        set_paused(&store,true).unwrap();assert!(queue.start(&mutation.id).unwrap_err().to_string().contains("paused"));set_paused(&store,false).unwrap();
        queue.start(&mutation.id).unwrap();assert!(queue.start(&mutation.id).is_err());assert!(queue.retry(&mutation.id).is_err());
        queue.complete(&mutation.id,json!({"receipt":1})).unwrap();assert_eq!(queue.complete(&mutation.id,json!({"receipt":1})).unwrap().response,Some(json!({"receipt":1})));
        let path=queue.root.join("dirty").join(&mutation.snapshot.as_ref().unwrap().file);let file=open_read(&path).unwrap();assert_eq!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);assert!(queue.snapshot(&mutation.id).err().unwrap().to_string().contains("busy"));drop(file);
        capture_snapshot(&queue.root,&File::open(&path).unwrap(),mutation.snapshot.as_ref().unwrap()).unwrap();
    }
    #[test]
    fn snapshot_inventory_counts_unmanaged_and_missing_data_without_deleting_user_bytes() {
        let (_temp,_store,queue,mutation)=setup();let dirty=queue.root.join("dirty");let foreign=dirty.join("foreign-file");fs::write(&foreign,b"keep me").unwrap();
        assert_eq!(queue.status().unwrap().unmanaged_snapshot_bytes,7);
        fs::remove_file(dirty.join(mutation.snapshot.unwrap().file)).unwrap();assert_eq!(queue.status().unwrap().missing_snapshots,1);assert_eq!(fs::read(&foreign).unwrap(),b"keep me");
        fs::create_dir(dirty.join("foreign-directory")).unwrap();assert!(queue.status().unwrap_err().to_string().contains("unsafe unexpected"));
    }
    #[test]
    fn resolution_plans_reject_bad_state_missing_snapshots_and_unguarded_targets() {
        use crate::policies::ConflictStrategy;
        let (_temp,_store,mut queue,mutation)=setup();let original=mutation.request.clone();let create=json!({"v":1,"op":"create","domain":"domain","client_item_id":"copy","name":"copy","kind":"file"});
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::KeepBoth,create.clone(),"copy".into(),None,None,true).is_err());
        queue.fail(&mutation.id,FailureKind::Conflict,"synthetic conflict").unwrap();
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::Stop,create.clone(),"copy".into(),None,None,true).is_err());
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::KeepBoth,original.clone(),"file".into(),None,None,true).is_err());
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::Replace,original.clone(),"file".into(),None,None,true).is_err());
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::Replace,original.clone(),"file".into(),None,Some("bad".into()),true).is_err());
        let metadata=queue.enqueue(json!({"v":1,"op":"modify","domain":"domain","id":"metadata","request_id":"metadata","base_content_version":"c","base_metadata_version":"m"}),None).unwrap();queue.fail(&metadata.id,FailureKind::Conflict,"metadata conflict").unwrap();
        assert!(queue.plan_resolution(&metadata.id,ConflictStrategy::KeepBoth,create.clone(),"copy".into(),None,None,true).is_err());
        queue.plan_resolution(&mutation.id,ConflictStrategy::KeepBoth,create.clone(),"copy".into(),None,None,true).unwrap();queue.fail(&mutation.id,FailureKind::Conflict,"new external edit").unwrap();
        assert!(queue.plan_resolution(&mutation.id,ConflictStrategy::KeepBoth,create,"copy".into(),None,None,false).is_err());
    }

    #[test]
    fn sparse_domains_missing_snapshots_and_corrupt_creations_cannot_change_persisted_intent() {
        let (_temp,_store,mut queue,mutation)=setup();let queue_path=queue.root.join("queue.json");let bytes=fs::read(&queue_path).unwrap();fs::remove_file(&queue_path).unwrap();assert!(read_only(&queue.home,"domain",&"a".repeat(64)).unwrap().is_empty());fs::write(&queue_path,&bytes).unwrap();fs::set_permissions(&queue_path,fs::Permissions::from_mode(0o600)).unwrap();
        let snapshot=queue.root.join("dirty").join(&mutation.snapshot.as_ref().unwrap().file);let source=File::open(&snapshot).unwrap();
        assert!(queue.enqueue(json!({"v":1,"op":"delete","domain":"domain","id":"file","request_id":"delete","base_content_version":"c","base_metadata_version":"m"}),Some(&source)).is_err());
        let mut wrong=mutation.snapshot.clone().unwrap();wrong.sha256="0".repeat(64);assert!(verify_snapshot(&source,&wrong).is_err());
        let metadata=queue.enqueue(json!({"v":1,"op":"modify","domain":"domain","id":"meta","request_id":"metadata","base_content_version":"c","base_metadata_version":"m"}),None).unwrap();assert!(queue.status().unwrap().counts["pending"]>=2);queue.cancel(&metadata.id).unwrap();queue.discard_snapshot(&metadata.id).unwrap();
        let mut bad=queue.index.clone();bad.mutations.get_mut(&mutation.id).unwrap().payload_discarded=true;assert!(validate_index(&bad,"domain",&"a".repeat(64)).is_err());
        let created=queue.enqueue(json!({"v":1,"op":"create","domain":"domain","client_item_id":"origin","name":"created","kind":"file"}),None).unwrap();
        let mut bad=queue.index.clone();bad.mutations.get_mut(&created.id).unwrap().request["client_item_id"]=json!("forged");assert!(validate_index(&bad,"domain",&"a".repeat(64)).unwrap_err().to_string().contains("content-bound creation"));
        let file=queue.root.join("foreign-file");fs::write(&file,b"bytes").unwrap();assert!(existing_private_directory(&file).is_err());assert!(existing_private_directory(&file.join("child")).is_err());
    }
    #[test]
    fn growing_state_reads_and_oversized_journals_leave_the_original_index_untouched() {
        let (_temp,_store,queue,mutation)=setup();let path=queue.root.join("queue.json");let original=fs::read(&path).unwrap();
        assert!(read_optional_with(&path,|_,bytes|{bytes.resize(MAX_INDEX as usize+1,0);Ok(bytes.len())}).unwrap_err().to_string().contains("grew beyond"));
        let mut large=queue.index.clone();large.mutations.get_mut(&mutation.id).unwrap().error=Some("x".repeat(MAX_INDEX as usize+1));assert!(save_index(&path,&large).unwrap_err().to_string().contains("exceeds 64 MiB"));assert_eq!(fs::read(path).unwrap(),original);
    }

}

#[cfg(test)]
mod resolution_tests {
    use super::*;
    use crate::policies::ConflictStrategy;
    use serde_json::json;
    #[test]
    fn resolution_fingerprint_keeps_persisted_version_encoding_compatible() {
        let version = crate::operations::Version {
            id: "v1".into(), path: "folder/file".into(), saved: "saved".into(),
            original: crate::backend::Entry {
                path: "folder/file".into(), is_dir: false, size: u64::MAX,
                modified: Some(i64::MIN), etag: Some("revision".into()),
            }, sha256: "hash".into(),
        };
        // Golden from the pre-refactor tuple/derived-struct JSON encoding,
        // including the largest unsigned and smallest signed supported values.
        assert_eq!(resolution_fingerprint("mutation", "original", ConflictStrategy::Replace,
            &json!({"id":"item","op":"modify"}), "folder/file", &Some(version), &Some("repository".into())),
            "db8e47e42c475c63b5057a10255943f52b759c2a06df7589d53868aeb163e307");
    }
    #[test]
    fn resolution_is_durable_and_does_not_change_original_intent_or_retry_guards() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("state")).unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"local conflicting edit").unwrap();
        let mut queue = Queue::open(&store, "domain", &"a".repeat(64)).unwrap();
        let original = json!({"v":1,"op":"modify","domain":"domain","id":"item","request_id":"original","base_content_version":"c0","base_metadata_version":"m0"});
        let mutation = queue
            .enqueue(original.clone(), Some(&File::open(source).unwrap()))
            .unwrap();
        queue.start(&mutation.id).unwrap();
        queue
            .fail(&mutation.id, FailureKind::Conflict, "external edit")
            .unwrap();
        let request = json!({"v":1,"op":"create","domain":"domain","client_item_id":"conflict-copy","name":"copy.txt","parent_id":"root","kind":"file"});
        queue
            .plan_resolution(
                &mutation.id,
                ConflictStrategy::KeepBoth,
                request.clone(),
                "copy.txt".into(),
                None,
                None,
                false,
            )
            .unwrap();
        drop(queue);
        let mut queue = Queue::open(&store, "domain", &"a".repeat(64)).unwrap();
        let planned = queue.get(&mutation.id).unwrap();
        assert_eq!(planned.request, original);
        assert_eq!(planned.resolution.as_ref().unwrap().request, request);
        assert_eq!(
            fs::read(queue.snapshot(&mutation.id).unwrap().unwrap().path()).unwrap(),
            b"local conflicting edit"
        );
        queue.start(&mutation.id).unwrap();
        queue
            .fail(&mutation.id, FailureKind::Conflict, "another external edit")
            .unwrap();
        assert!(queue
            .plan_resolution(
                &mutation.id,
                ConflictStrategy::KeepBoth,
                request.clone(),
                "copy.txt".into(),
                None,
                None,
                false
            )
            .is_err());
        queue.retry(&mutation.id).unwrap();
        assert_eq!(
            queue.get(&mutation.id).unwrap().resolution.unwrap().request,
            request
        );
    }
    #[test]
    fn replace_requires_backup_and_cancel_retains_original_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("state")).unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"recover me").unwrap();
        let mut queue = Queue::open(&store, "domain", &"b".repeat(64)).unwrap();
        let request = json!({"v":1,"op":"modify","domain":"domain","id":"item","request_id":"original","base_content_version":"c0","base_metadata_version":"m0"});
        let m = queue
            .enqueue(request.clone(), Some(&File::open(source).unwrap()))
            .unwrap();
        queue.start(&m.id).unwrap();
        queue
            .fail(&m.id, FailureKind::Conflict, "conflict")
            .unwrap();
        assert!(queue
            .plan_resolution(
                &m.id,
                ConflictStrategy::Replace,
                request,
                "item.txt".into(),
                None,
                None,
                true
            )
            .is_err());
        assert_eq!(queue.cancel(&m.id).unwrap().state, MutationState::Cancelled);
        assert!(queue.snapshot(&m.id).unwrap().is_some());
    }
}
