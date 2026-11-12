//! Durable scheduling for checkpoint backends. This module never captures or
//! deletes payloads; a backend must prove the outcome of the exact planned ID.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

const FORMAT: u32 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const MAX_TIMESTAMP: i64 = 253_402_300_799;
const BUSY_RETRY: i64 = 30;
const MAX_RETRY: i64 = 3600;
pub const MAX_INTERVAL_SECS: u64 = 365 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct Options {
    pub enabled: bool,
    pub interval_secs: Option<u64>,
    /// Stable storage identity, rather than the current retention limits.
    pub backend_key: String,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        if self
            .interval_secs
            .is_some_and(|s| !(60..=MAX_INTERVAL_SECS).contains(&s))
        {
            bail!("checkpoint interval must be between 60 and 31536000 seconds");
        }
        if !valid_key(&self.backend_key) {
            bail!("invalid checkpoint scheduler backend identity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Scheduled,
    Manual,
}

pub trait Clock {
    fn now(&self) -> i64;
}
impl<F: Fn() -> i64> Clock for F {
    fn now(&self) -> i64 {
        self()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub id: String,
    pub backend_key: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    Completed {
        checkpoint_id: String,
    },
    /// No published checkpoint or running host operation exists for this ID.
    /// Only this proof authorizes executing the same planned ID again.
    RetrySameAttempt,
    Deferred {
        reason: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    Completed { checkpoint_id: String },
    Deferred { reason: String },
}
pub trait Backend {
    fn reconcile(&mut self, attempt: &Attempt) -> Result<Recovery>;
    fn create(&mut self, attempt: &Attempt) -> Result<AttemptOutcome>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Success {
    checkpoint_id: String,
    backend_key: String,
    completed_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    version: u32,
    inflight: Option<Attempt>,
    last_success: Option<Success>,
    failure_count: u32,
    retry_at: Option<i64>,
    last_error: Option<String>,
    last_deferred: Option<String>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            version: FORMAT,
            inflight: None,
            last_success: None,
            failure_count: 0,
            retry_at: None,
            last_error: None,
            last_deferred: None,
        }
    }
}
impl Runtime {
    fn validate(&self) -> Result<()> {
        if self.version != FORMAT {
            bail!("unsupported checkpoint scheduler state version");
        }
        if let Some(a) = &self.inflight {
            identity(&a.id)?;
            timestamp(a.started_at)?;
            if !valid_key(&a.backend_key) {
                bail!("invalid retained checkpoint backend identity");
            }
        }
        if let Some(s) = &self.last_success {
            identity(&s.checkpoint_id)?;
            timestamp(s.completed_at)?;
            if !valid_key(&s.backend_key) {
                bail!("invalid completed checkpoint backend identity");
            }
        }
        if let Some(t) = self.retry_at {
            timestamp(t)?;
        }
        for text in [&self.last_error, &self.last_deferred]
            .into_iter()
            .flatten()
        {
            if text.is_empty() || text.len() > 1024 || text.chars().any(char::is_control) {
                bail!("invalid checkpoint scheduler diagnostic");
            }
        }
        let diagnostic = self.last_error.is_some() || self.last_deferred.is_some();
        if (self.retry_at.is_some() != diagnostic)
            || (self.inflight.is_none() && (diagnostic || self.failure_count > 0))
            || (self.failure_count > 0) != self.last_error.is_some()
        {
            bail!("inconsistent checkpoint retry state");
        }
        if self
            .inflight
            .as_ref()
            .zip(self.last_success.as_ref())
            .is_some_and(|(a, s)| a.id == s.checkpoint_id)
        {
            bail!("a completed checkpoint cannot also be awaiting publication");
        }
        Ok(())
    }
}
fn identity(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value)?.to_string() != value {
        bail!("invalid checkpoint attempt identity");
    }
    Ok(())
}
fn valid_key(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn timestamp(value: i64) -> Result<i64> {
    if !(0..=MAX_TIMESTAMP).contains(&value) {
        bail!("checkpoint scheduler clock is outside its supported range");
    }
    Ok(value)
}
fn after(now: i64, delay: i64) -> Result<i64> {
    timestamp(
        now.checked_add(delay)
            .context("checkpoint schedule time overflow")?,
    )
}
fn diagnostic(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        if result.len() + ch.len_utf8() > 1024 {
            break;
        }
        result.push(ch);
    }
    if result.is_empty() {
        result.push_str("Checkpoint operation did not finish");
    }
    result
}
fn private(meta: &fs::Metadata, directory: bool) -> Result<()> {
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.file_type().is_symlink()
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file() || meta.nlink() != 1
        }
    {
        bail!("checkpoint scheduler paths must be real, private and owned");
    }
    Ok(())
}
struct Directory {
    path: PathBuf,
}
impl Directory {
    fn open(home: &Path, create: bool) -> Result<Option<Self>> {
        private(&fs::symlink_metadata(home)?, true)?;
        let path = home.join("checkpoint-scheduler");
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) => private(&meta, true)?,
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        Ok(Some(Self { path }))
    }
    fn read(&self) -> Result<Runtime> {
        let source = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path.join("state.json"))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Runtime::default()),
            Err(e) => return Err(e).context("cannot read checkpoint scheduler state"),
        };
        let meta = source.metadata()?;
        private(&meta, false)?;
        if meta.len() > MAX_STATE_BYTES {
            bail!("checkpoint scheduler state exceeds its size bound");
        }
        let mut raw = Vec::new();
        source.take(MAX_STATE_BYTES + 1).read_to_end(&mut raw)?;
        decode(&raw)
    }
    fn lock(&self) -> Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path.join("lock"))?;
        let meta = file.metadata()?;
        private(&meta, false)?;
        if meta.len() != 0 {
            bail!("invalid checkpoint scheduler lock");
        }
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_result(result, std::io::Error::last_os_error())? {
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }
    fn save(&self, state: &Runtime) -> Result<()> {
        self.save_with_parent_sync(state, |parent| Ok(File::open(parent)?.sync_all()?))
    }
    fn save_with_parent_sync(
        &self,
        state: &Runtime,
        mut sync_parent: impl FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        let raw = serde_json::to_vec_pretty(state)?;
        if raw.len() as u64 > MAX_STATE_BYTES {
            bail!("checkpoint scheduler state exceeds its size bound");
        }
        state.validate()?;
        self.read()?;
        // Another caller can observe a newly created directory before its
        // creator syncs the parent. Anchor that entry before persisting intent.
        sync_parent(
            self.path
                .parent()
                .context("checkpoint scheduler needs a parent")?,
        )?;
        let path = self
            .path
            .join(format!(".schedule-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let _temporary = Temporary(path.clone());
        file.write_all(&raw)?;
        file.sync_all()?;
        fs::rename(path, self.path.join("state.json"))?;
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}
fn decode(raw: &[u8]) -> Result<Runtime> {
    if raw.len() as u64 > MAX_STATE_BYTES {
        bail!("checkpoint scheduler state exceeds its size bound");
    }
    let state: Runtime =
        serde_json::from_slice(raw).context("invalid checkpoint scheduler JSON")?;
    state.validate()?;
    Ok(state)
}
fn lock_result(result: i32, error: std::io::Error) -> Result<bool> {
    if result == 0 {
        return Ok(true);
    }
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error).context("cannot lock checkpoint scheduler")
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn due(state: &Runtime, options: &Options, now: i64) -> Result<Option<i64>> {
    let Some(interval) = options.interval_secs.filter(|_| options.enabled) else {
        return Ok(None);
    };
    match state
        .last_success
        .as_ref()
        .filter(|s| s.backend_key == options.backend_key)
    {
        Some(s) => Ok(Some(after(s.completed_at, interval as i64)?)),
        None => Ok(Some(now)),
    }
}
fn report(state: &Runtime, options: &Options, now: i64, operation: &str) -> Result<Value> {
    Ok(
        json!({"operation":operation,"created":operation=="created","reconciled":operation=="reconciled",
        "checkpoint_id":state.last_success.as_ref().filter(|_|matches!(operation,"created"|"reconciled")).map(|s|&s.checkpoint_id),
        "enabled":options.enabled,"interval_secs":options.interval_secs,"backend_key":options.backend_key,
        "next_due_at":due(state,options,now)?,"retry_at":state.retry_at,"failure_count":state.failure_count,
        "inflight":state.inflight,"last_success":state.last_success,"last_error":state.last_error,"last_deferred":state.last_deferred}),
    )
}

/// Reads existing state without creating files, attempting a checkpoint or
/// mounting a volume. Invalid retained state is reported, never reset.
pub fn status(home: &Path, options: &Options, now: i64) -> Result<Value> {
    options.validate()?;
    timestamp(now)?;
    let state = Directory::open(home, false)?
        .map(|d| d.read())
        .transpose()?
        .unwrap_or_default();
    let operation = if !options.enabled {
        "disabled"
    } else if options.interval_secs.is_none() {
        "manual_only"
    } else if state.retry_at.is_some_and(|t| t > now) {
        "backoff"
    } else if state.inflight.is_some() {
        "recovery_due"
    } else if due(&state, options, now)?.is_some_and(|t| t > now) {
        "not_due"
    } else {
        "due"
    };
    report(&state, options, now, operation)
}

/// A separate lease serializes scheduled and explicit calls. A durable attempt
/// precedes every backend mutation and survives uncertain outcomes unchanged.
pub fn tick(
    home: &Path,
    options: &Options,
    trigger: Trigger,
    clock: &dyn Clock,
    backend: &mut dyn Backend,
) -> Result<Value> {
    tick_with_save(
        home,
        options,
        trigger,
        clock,
        backend,
        |directory, state| directory.save(state),
    )
}
fn tick_with_save(
    home: &Path,
    options: &Options,
    trigger: Trigger,
    clock: &dyn Clock,
    backend: &mut dyn Backend,
    mut save: impl FnMut(&Directory, &Runtime) -> Result<()>,
) -> Result<Value> {
    options.validate()?;
    let now = timestamp(clock.now())?;
    if !options.enabled && trigger == Trigger::Manual {
        bail!("APFS checkpoints are disabled; enable them through checkpoint configuration first");
    }
    if !options.enabled || (trigger == Trigger::Scheduled && options.interval_secs.is_none()) {
        return status(home, options, now);
    }
    let directory =
        Directory::open(home, true)?.context("checkpoint scheduler directory missing")?;
    let _lease = directory.lock()?;
    let mut state = directory.read()?;
    if _lease.is_none() {
        return report(&state, options, now, "busy");
    }
    if trigger == Trigger::Scheduled
        && (state.retry_at.is_some_and(|t| t > now)
            || (state.inflight.is_none() && due(&state, options, now)?.is_some_and(|t| t > now)))
    {
        return report(&state, options, now, "not_due");
    }
    if let Some(attempt) = state.inflight.clone() {
        match backend.reconcile(&attempt) {
            Ok(Recovery::Completed { checkpoint_id }) => {
                return finish(
                    &directory,
                    &mut state,
                    options,
                    clock,
                    &attempt,
                    checkpoint_id,
                    "reconciled",
                    &mut save,
                )
            }
            Ok(Recovery::Deferred { reason }) => {
                return defer(&directory, &mut state, options, clock, &reason, &mut save)
            }
            Err(error) => return fail(&directory, &mut state, clock, error, &mut save),
            Ok(Recovery::RetrySameAttempt) if attempt.backend_key != options.backend_key => {
                state.inflight = None;
                state.failure_count = 0;
                state.retry_at = None;
                state.last_error = None;
                state.last_deferred = None;
                save(&directory, &state)?;
            }
            Ok(Recovery::RetrySameAttempt) => (),
        }
    }
    if state.inflight.is_none() {
        state.inflight = Some(Attempt {
            id: uuid::Uuid::new_v4().to_string(),
            backend_key: options.backend_key.clone(),
            started_at: now,
        });
        save(&directory, &state)?;
    }
    let attempt = state
        .inflight
        .clone()
        .context("checkpoint attempt was not planned")?;
    match backend.create(&attempt) {
        Ok(AttemptOutcome::Completed { checkpoint_id }) => finish(
            &directory,
            &mut state,
            options,
            clock,
            &attempt,
            checkpoint_id,
            "created",
            &mut save,
        ),
        Ok(AttemptOutcome::Deferred { reason }) => {
            defer(&directory, &mut state, options, clock, &reason, &mut save)
        }
        Err(error) => fail(&directory, &mut state, clock, error, &mut save),
    }
}
fn finish(
    directory: &Directory,
    state: &mut Runtime,
    options: &Options,
    clock: &dyn Clock,
    attempt: &Attempt,
    checkpoint_id: String,
    operation: &str,
    save: &mut impl FnMut(&Directory, &Runtime) -> Result<()>,
) -> Result<Value> {
    if checkpoint_id != attempt.id {
        return fail(
            directory,
            state,
            clock,
            anyhow::anyhow!("checkpoint backend returned a different planned identity"),
            save,
        );
    }
    let completed_at = timestamp(clock.now())?;
    let mut next = state.clone();
    next.inflight = None;
    next.failure_count = 0;
    next.retry_at = None;
    next.last_error = None;
    next.last_deferred = None;
    next.last_success = Some(Success {
        checkpoint_id,
        backend_key: attempt.backend_key.clone(),
        completed_at,
    });
    let value = report(&next, options, completed_at, operation)?;
    save(directory, &next)?;
    *state = next;
    Ok(value)
}
fn defer(
    directory: &Directory,
    state: &mut Runtime,
    options: &Options,
    clock: &dyn Clock,
    reason: &str,
    save: &mut impl FnMut(&Directory, &Runtime) -> Result<()>,
) -> Result<Value> {
    let now = timestamp(clock.now())?;
    state.retry_at = Some(after(now, BUSY_RETRY)?);
    state.last_deferred = Some(diagnostic(reason));
    save(directory, state)?;
    report(state, options, now, "deferred")
}
fn fail(
    directory: &Directory,
    state: &mut Runtime,
    clock: &dyn Clock,
    error: anyhow::Error,
    save: &mut impl FnMut(&Directory, &Runtime) -> Result<()>,
) -> Result<Value> {
    let now = timestamp(clock.now())?;
    state.failure_count = state.failure_count.saturating_add(1);
    state.retry_at = Some(after(
        now,
        (BUSY_RETRY * (1i64 << state.failure_count.saturating_sub(1).min(7))).min(MAX_RETRY),
    )?);
    state.last_error = Some(diagnostic(&format!("{error:#}")));
    state.last_deferred = None;
    save(directory, state)?;
    Err(error.context("checkpoint attempt retained for reconciliation"))
}

#[cfg(test)]
#[path = "checkpoint_schedule/tests.rs"]
mod tests;
