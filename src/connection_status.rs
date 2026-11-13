//! Account-bound observations of authenticated remote operations. A configured
//! profile or a live local service alone is never connectivity evidence.
use crate::{config::Profile, state};
use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};

const VERSION: u32 = 1;
const MAX_RECORD: u64 = 16 * 1024;
const MAX_OUTPUT: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Code {
    Timeout,
    CredentialsUnavailable,
    AuthenticationFailed,
    AccessDenied,
    Dns,
    Tls,
    Network,
    Server,
    RootUnavailable,
    ConfigurationChanged,
    ProbeProcess,
    InvalidResponse,
    Busy,
    PermissionDenied,
    NotFound,
    InvalidData,
    UnsafeState,
    Operation,
    Conflict,
    Quota,
    Filtered,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Effect {
    Online,
    Offline,
    Unknown,
    None,
}
impl Code {
    fn effect(self) -> Effect {
        match self {
            Self::AuthenticationFailed
            | Self::AccessDenied
            | Self::Dns
            | Self::Tls
            | Self::Network
            | Self::Server
            | Self::RootUnavailable => Effect::Offline,
            Self::Timeout
            | Self::CredentialsUnavailable
            | Self::ConfigurationChanged
            | Self::ProbeProcess
            | Self::InvalidResponse => Effect::Unknown,
            _ => Effect::None,
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::Timeout=>"The connection check timed out before it could establish current reachability.",
            Self::CredentialsUnavailable=>"Configured credentials are unavailable in this process; check the environment reference or Keychain access.",
            Self::AuthenticationFailed=>"The server rejected authentication for this connection.",
            Self::AccessDenied=>"The server denied access to the configured remote location.",
            Self::Dns=>"The server name could not be resolved.",
            Self::Tls=>"TLS certificate or secure connection verification failed.",
            Self::Network=>"The network transport could not connect or was interrupted.",
            Self::Server=>"The server returned a temporary or internal service error.",
            Self::RootUnavailable=>"The configured remote root is missing or is not an accessible directory.",
            Self::ConfigurationChanged=>"Connection settings changed during the check; no stale result was adopted.",
            Self::ProbeProcess=>"The isolated connection-check process failed to complete normally.",
            Self::InvalidResponse=>"The connection-check response did not match the expected profile and request.",
            Self::Busy=>"This local component is busy; its last known status may still be available.",
            Self::PermissionDenied=>"This process lacks permission to read the requested local component.",
            Self::NotFound=>"The requested item or local status record is unavailable.",
            Self::InvalidData=>"The local status data is invalid or incomplete.",
            Self::UnsafeState=>"The local status path failed ownership, privacy or file-type validation.",
            Self::Operation=>"The operation failed; no safe detailed cause is available.",
            Self::Conflict=>"The remote revision changed; retained work requires conflict review.",
            Self::Quota=>"The requested operation exceeds an available storage or history budget.",
            Self::Filtered=>"Synchronization rules exclude this operation; retained contents remain local.",
        }
    }
    fn value(self) -> Value {
        json!({"code":self,"message":self.message(),"connection_effect":self.effect()})
    }
}
fn classify(error: &anyhow::Error) -> Code {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("version_conflict") || text.contains("revision changed") {
        return Code::Conflict;
    }
    if text.contains("insufficient_quota") || text.contains("no space left") {
        return Code::Quota;
    }
    if text.contains("excluded_by_filter") || text.contains("synchronization filters exclude") {
        return Code::Filtered;
    }
    if text.contains("credential environment variable")
        || text.contains("keychain")
        || text.contains("no credential found")
    {
        return Code::CredentialsUnavailable;
    }
    if error.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
    }) || error.chain().any(|e| {
        e.downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
    }) || text.contains("timed out")
    {
        return Code::Timeout;
    }
    if text.contains("certificate") || text.contains("tls") || text.contains("ssl") {
        return Code::Tls;
    }
    if text.contains("dns")
        || text.contains("resolve")
        || text.contains("name or service not known")
        || text.contains("nodename nor servname")
    {
        return Code::Dns;
    }
    if [
        "http 401",
        "401 unauthorized",
        "status_logon_failure",
        "logon failure",
        "logonfailure",
        "wrong password",
        "invalid credentials",
    ]
    .iter()
    .any(|p| text.contains(p))
        || (text.contains("authentication failed")
            && !text.contains("connection or authentication failed"))
    {
        return Code::AuthenticationFailed;
    }
    if [
        "http 403",
        "403 forbidden",
        "status_access_denied",
        "access denied",
    ]
    .iter()
    .any(|p| text.contains(p))
    {
        return Code::AccessDenied;
    }
    if text.contains("http 5")
        || text.contains("503 service")
        || text.contains("502 bad gateway")
        || text.contains("500 internal")
    {
        return Code::Server;
    }
    if error.chain().any(|e| {
        e.downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect)
    }) || [
        "not connected",
        "connection refused",
        "connection reset",
        "network unreachable",
        "network is unreachable",
        "broken pipe",
        "transport error",
        "server_unreachable",
    ]
    .iter()
    .any(|p| text.contains(p))
    {
        return Code::Network;
    }
    if error
        .chain()
        .any(|e| e.downcast_ref::<serde_json::Error>().is_some())
        || text.contains("invalid read-only configuration")
    {
        return Code::InvalidData;
    }
    if text.contains("private")
        || text.contains("owned")
        || text.contains("symlink")
        || text.contains("unsafe")
    {
        return Code::UnsafeState;
    }
    if let Some(io) = error
        .chain()
        .find_map(|e| e.downcast_ref::<std::io::Error>())
    {
        return match io.kind() {
            std::io::ErrorKind::WouldBlock => Code::Busy,
            std::io::ErrorKind::PermissionDenied => Code::PermissionDenied,
            std::io::ErrorKind::NotFound => Code::NotFound,
            _ => Code::Operation,
        };
    }
    Code::Operation
}
/// Deliberately returns fixed messages, never raw server bodies, URLs, headers
/// or an error string that could contain a credential or environment value.
pub fn safe_error(error: &anyhow::Error) -> Value {
    classify(error).value()
}
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn binding(profile: &Profile) -> Result<String> {
    crate::config::validate_profile(profile)?;
    Ok(state::key(&serde_json::to_string(profile)?))
}
fn valid_time(value: i64) -> Result<()> {
    if value < 0 || chrono::DateTime::from_timestamp_millis(value).is_none() {
        bail!("invalid connection observation timestamp");
    }
    Ok(())
}
fn operation(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        bail!("invalid connection observation operation");
    }
    Ok(())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn owned_metadata(m: &fs::Metadata, directory: bool) -> Result<()> {
    if m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.file_type().is_symlink()
        || if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        bail!("connection observation paths must be private owned regular paths without links");
    }
    Ok(())
}
fn private(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    owned_metadata(&metadata, directory)?;
    Ok(metadata)
}
fn same_identity(before: &fs::Metadata, after: &fs::Metadata) -> Result<()> {
    owned_metadata(after, false)?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        bail!("connection observation path changed while opening");
    }
    Ok(())
}
fn directory(home: &Path, create: bool) -> Result<Option<PathBuf>> {
    directory_with(home, create, |path| fs::symlink_metadata(path))
}
fn directory_with(
    home: &Path,
    create: bool,
    inspect: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
) -> Result<Option<PathBuf>> {
    match private(home, true) {
        Ok(_) => (),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && !create =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    }
    crate::state_directory::guard(home)?;
    let path = home.join("connection-health");
    match inspect(&path) {
        Ok(_) => {
            private(&path, true)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            File::open(home)?.sync_all()?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(Some(path))
}
fn filename(name: &str, scope: &str) -> String {
    state::key(&format!("{name}\0{scope}"))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    started_at_ms: i64,
    finished_at_ms: i64,
    operation: String,
    error: Option<Code>,
}
impl Event {
    fn validate(&self) -> Result<()> {
        valid_time(self.started_at_ms)?;
        valid_time(self.finished_at_ms)?;
        operation(&self.operation)?;
        if self.finished_at_ms < self.started_at_ms {
            bail!("connection observation clock moved backwards");
        }
        Ok(())
    }
    fn order(&self) -> (i64, i64) {
        (self.started_at_ms, self.finished_at_ms)
    }
    fn value(&self) -> Value {
        json!({"started_at":stamp(self.started_at_ms),"finished_at":stamp(self.finished_at_ms),"started_at_ms":self.started_at_ms,"finished_at_ms":self.finished_at_ms,"operation":self.operation,"error":self.error.map(Code::value)})
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    scope: String,
    name_hash: String,
    last_attempt: Event,
    last_success: Option<Event>,
    last_failure: Option<Event>,
    evidence: Option<Event>,
}
fn read(path: &Path, scope: &str, name: &str) -> Result<Option<Record>> {
    let before = match private(path, false) {
        Ok(v) => v,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let after = file.metadata()?;
    same_identity(&before, &after)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD {
        bail!("connection observation record exceeds its size bound");
    }
    let value: Record = serde_json::from_slice(&bytes)?;
    if value.version != VERSION || value.scope != scope || value.name_hash != state::key(name) {
        bail!("connection observation identity differs from this profile");
    }
    if value
        .last_success
        .as_ref()
        .is_some_and(|e| e.error.is_some())
        || value
            .last_failure
            .as_ref()
            .is_some_and(|e| e.error.is_none())
        || value
            .evidence
            .as_ref()
            .is_some_and(|e| e.error.is_some_and(|code| code.effect() == Effect::None))
    {
        bail!("connection observation event roles are inconsistent");
    }
    for e in [&value.last_attempt]
        .into_iter()
        .chain(value.last_success.iter())
        .chain(value.last_failure.iter())
        .chain(value.evidence.iter())
    {
        e.validate()?;
        if e.order() > value.last_attempt.order() {
            bail!("connection observation events are out of order");
        }
    }
    Ok(Some(value))
}
/// Persist successful remote I/O or a classified failure. Callers should ignore
/// an observation-write error rather than changing a completed user operation.
pub fn observe(
    home: &Path,
    name: &str,
    profile: &Profile,
    label: &str,
    started_at_ms: i64,
    result: Result<(), &anyhow::Error>,
) -> Result<()> {
    observe_at(
        home,
        name,
        profile,
        label,
        started_at_ms,
        now_ms(),
        result.err().map(classify),
    )
}
fn observe_at(
    home: &Path,
    name: &str,
    profile: &Profile,
    label: &str,
    started: i64,
    finished: i64,
    error: Option<Code>,
) -> Result<()> {
    let scope = binding(profile)?;
    let event = Event {
        started_at_ms: started,
        finished_at_ms: finished,
        operation: label.into(),
        error,
    };
    event.validate()?;
    let dir = directory(home, true)?.context("connection observation directory missing")?;
    let key = filename(name, &scope);
    let lock = dir.join(format!("{key}.lock"));
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&lock)?;
    let identity = private(&lock, false)?;
    let opened = lease.metadata()?;
    same_identity(&identity, &opened)?;
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let path = dir.join(format!("{key}.json"));
    let old = read(&path, &scope, name)?;
    if old
        .as_ref()
        .is_some_and(|r| r.last_attempt.order() > event.order())
    {
        return Ok(());
    }
    let value = merge_record(old, scope, name, event);
    state::atomic_json(&path, &value)?;
    Ok(())
}
fn merge_record(old: Option<Record>, scope: String, name: &str, event: Event) -> Record {
    let mut value = old.unwrap_or(Record {
        version: VERSION,
        scope,
        name_hash: state::key(name),
        last_attempt: event.clone(),
        last_success: None,
        last_failure: None,
        evidence: None,
    });
    value.last_attempt = event.clone();
    match event.error {
        None => {
            value.last_success = Some(event.clone());
            value.evidence = Some(event);
        }
        Some(code) => {
            value.last_failure = Some(event.clone());
            if code.effect() != Effect::None {
                value.evidence = Some(event);
            }
        }
    }
    value
}
fn stamp(value: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(value)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
/// Read locally without credentials, network access, writer recovery or creating
/// an absent home/status directory. Unknown, stale and clock-skewed are explicit.
pub fn snapshot(
    home: &Path,
    name: &str,
    profile: &Profile,
    now: i64,
    stale_after_secs: u64,
) -> Result<Value> {
    valid_time(now)?;
    if stale_after_secs == 0 || stale_after_secs > 31_536_000 {
        bail!("connection observation freshness must be between one second and one year");
    }
    let scope = binding(profile)?;
    let record = if let Some(dir) = directory(home, false)? {
        read(
            &dir.join(format!("{}.json", filename(name, &scope))),
            &scope,
            name,
        )?
    } else {
        None
    };
    Ok(snapshot_record(
        name,
        record.as_ref(),
        now,
        stale_after_secs,
    ))
}
fn snapshot_record(name: &str, record: Option<&Record>, now: i64, stale: u64) -> Value {
    let evidence = record.and_then(|r| r.evidence.as_ref());
    let age_ms = evidence
        .and_then(|e| now.checked_sub(e.finished_at_ms))
        .filter(|v| *v >= 0)
        .map(|v| v as u64);
    let age = age_ms.map(|v| v / 1000);
    let state = match (evidence, age) {
        (Some(_), Some(_)) if age_ms.is_some_and(|age| age > stale * 1000) => "stale",
        (Some(e), Some(_)) => match e.error.map(Code::effect).unwrap_or(Effect::Online) {
            Effect::Online => "online",
            Effect::Offline => "offline",
            _ => "unknown",
        },
        _ => "unknown",
    };
    json!({"profile":name,"state":state,"checked_at":evidence.and_then(|e|stamp(e.finished_at_ms)),"checked_at_ms":evidence.map(|e|e.finished_at_ms),"age_seconds":age,"freshness_seconds":stale,"source":if record.is_some(){"persisted_network_observation"}else{"no_current_observation"},"last_success_at":record.and_then(|r|r.last_success.as_ref()).and_then(|e|stamp(e.finished_at_ms)),"last_success_at_ms":record.and_then(|r|r.last_success.as_ref()).map(|e|e.finished_at_ms),"last_attempt":record.map(|r|r.last_attempt.value()),"last_error":record.and_then(|r|r.last_attempt.error.map(|code|json!({"at":stamp(r.last_attempt.finished_at_ms),"at_ms":r.last_attempt.finished_at_ms,"operation":r.last_attempt.operation,"detail":code.value()}))),"last_failure":record.and_then(|r|r.last_failure.as_ref()).map(Event::value),"persistent_transport_observed":false,"clock_skew":evidence.is_some_and(|e|e.finished_at_ms>now),"meaning":"Most recent authenticated remote operation or explicit check; service state and cached indexes do not establish reachability."})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u32,
    binding: String,
    profile_hash: String,
    started_at_ms: i64,
    finished_at_ms: i64,
    error: Option<Code>,
}
/// Hidden child entry point. It never opens Store or writes local observations.
/// Parent process ownership/timeouts also bound synchronous Keychain requests.
pub async fn worker(home: &Path, name: &str, expected_binding: &str) -> Result<Value> {
    worker_with(home, name, expected_binding, &NetworkConnector).await
}
#[async_trait::async_trait]
trait Connector: Sync {
    async fn connect(
        &self,
        profile: &Profile,
        name: &str,
    ) -> Result<std::sync::Arc<dyn crate::backend::Backend>>;
}
struct NetworkConnector;
#[async_trait::async_trait]
impl Connector for NetworkConnector {
    async fn connect(
        &self,
        profile: &Profile,
        name: &str,
    ) -> Result<std::sync::Arc<dyn crate::backend::Backend>> {
        crate::factory::connect(profile, name, crate::transfer::global()).await
    }
}
async fn worker_with(
    home: &Path,
    name: &str,
    expected_binding: &str,
    connector: &dyn Connector,
) -> Result<Value> {
    if !digest(expected_binding) {
        bail!("invalid connection-check binding");
    }
    let started = now_ms();
    let config = crate::client_read::config(home)?;
    let profile = config
        .profiles
        .get(name)
        .context("connection-check profile missing")?;
    let actual = binding(profile)?;
    let error = if actual != expected_binding {
        Some(Code::ConfigurationChanged)
    } else {
        match connector.connect(profile, name).await {
            Err(error) => Some(classify(&error)),
            Ok(backend) => match backend.stat_metadata("").await {
                Ok(Some(entry)) if entry.is_dir && entry.path.is_empty() => None,
                Ok(_) => Some(Code::RootUnavailable),
                Err(error) => Some(classify(&error)),
            },
        }
    };
    Ok(serde_json::to_value(Reply {
        version: VERSION,
        binding: actual,
        profile_hash: state::key(name),
        started_at_ms: started,
        finished_at_ms: now_ms(),
        error,
    })?)
}
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}
async fn bounded_read(mut input: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut input)
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut out)
        .await?;
    if out.len() as u64 > MAX_OUTPUT {
        bail!("connection-check output exceeds its size bound");
    }
    Ok(out)
}
async fn child(
    executable: &Path,
    home: &Path,
    name: &str,
    scope: &str,
    timeout: Duration,
) -> Result<Reply, Code> {
    let parent_started = now_ms();
    let mut command = tokio::process::Command::new(executable);
    command.args(["--home"]).arg(home).args([
        "--json",
        "--progress",
        "off",
        "__status-probe",
        name,
        scope,
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut process = command.spawn().map_err(|_| Code::ProbeProcess)?;
    let mut group = ProcessGroup(process.id());
    let out = process.stdout.take().ok_or(Code::ProbeProcess)?;
    let err = process.stderr.take().ok_or(Code::ProbeProcess)?;
    let result = tokio::time::timeout(timeout, async {
        let (out, err) = tokio::try_join!(bounded_read(out), bounded_read(err))?;
        let status = process.wait().await?;
        Ok::<_, anyhow::Error>((status, out, err))
    })
    .await;
    let raw = match result {
        Ok(Ok(value)) => {
            group.0 = None;
            value
        }
        other => {
            drop(group);
            let _ = process.wait().await;
            return Err(if other.is_err() {
                Code::Timeout
            } else {
                Code::InvalidResponse
            });
        }
    };
    if !raw.0.success() {
        return Err(Code::ProbeProcess);
    }
    let value: Reply = serde_json::from_slice(&raw.1).map_err(|_| Code::InvalidResponse)?;
    if value.version != VERSION
        || value.binding != scope
        || value.profile_hash != state::key(name)
        || value.started_at_ms > value.finished_at_ms
        || value.started_at_ms < 0
        || value.started_at_ms.saturating_add(1000) < parent_started
        || value.finished_at_ms > now_ms().saturating_add(1000)
    {
        return Err(Code::InvalidResponse);
    }
    Ok(value)
}
/// Explicit network checks only. At most four owned child process groups run;
/// a timed-out or cancelled group cannot leave a Keychain helper running.
pub async fn probe_profiles(
    home: &Path,
    selected: Option<&str>,
    timeout_secs: u64,
    max_parallel: usize,
) -> Result<Value> {
    let executable = std::env::current_exe()?;
    probe_with(&executable, home, selected, timeout_secs, max_parallel).await
}
async fn probe_with(
    executable: &Path,
    home: &Path,
    selected: Option<&str>,
    timeout_secs: u64,
    max_parallel: usize,
) -> Result<Value> {
    if !(1..=120).contains(&timeout_secs) || !(1..=4).contains(&max_parallel) {
        bail!("connection checks require timeout 1..120 seconds and parallelism 1..4");
    }
    let config = crate::client_read::config(home)?;
    if selected.is_some_and(|name| !config.profiles.contains_key(name)) {
        bail!("unknown connection-check profile");
    }
    let profiles: Vec<_> = config
        .profiles
        .into_iter()
        .filter(|(name, _)| selected.is_none_or(|wanted| name == wanted))
        .collect();
    let mut tasks=stream::iter(profiles.into_iter().map(|(name,profile)|async move {
        match probe_one(executable,home,&name,&profile,timeout_secs).await {
            Ok(value)=>value,
            Err(error)=>json!({"profile":name,"state":"unknown","checked_now":true,"probe_error":safe_error(&error)}),
        }
    })).buffer_unordered(max_parallel);
    let mut values = Vec::new();
    while let Some(value) = tasks.next().await {
        values.push(value);
    }

    values.sort_by(|a, b| a["profile"].as_str().cmp(&b["profile"].as_str()));
    Ok(json!({"checked":true,"profiles":values}))
}

async fn probe_one(
    executable: &Path,
    home: &Path,
    name: &str,
    profile: &Profile,
    timeout_secs: u64,
) -> Result<Value> {
    let scope = binding(profile)?;
    let start = now_ms();
    let result = child(
        executable,
        home,
        name,
        &scope,
        Duration::from_secs(timeout_secs),
    )
    .await;
    let fresh = crate::client_read::config(home)?;
    let Some(current) = fresh.profiles.get(name) else {
        return Ok(
            json!({"profile":name,"state":"unknown","checked_now":true,"probe_error":Code::ConfigurationChanged.value()}),
        );
    };
    if binding(current)? != scope {
        let mut value=snapshot(home,name,current,now_ms(),300).unwrap_or_else(|error|json!({"profile":name,"state":"unknown","observation_error":safe_error(&error)}));
        value["checked_now"] = json!(true);
        value["probe_error"] = Code::ConfigurationChanged.value();
        return Ok(value);
    }
    let (started, finished, error) = match result {
        Ok(reply) => (reply.started_at_ms, reply.finished_at_ms, reply.error),
        Err(code) => (start, now_ms(), Some(code)),
    };
    let event = Event {
        started_at_ms: started,
        finished_at_ms: finished,
        operation: "probe.root_metadata".into(),
        error,
    };
    event.validate()?;
    let saved = observe_at(
        home,
        name,
        current,
        &event.operation,
        started,
        finished,
        error,
    );
    let mut value = match snapshot(home, name, current, now_ms(), 300) {
        Ok(value) if saved.is_ok() => value,
        other => {
            let memory = merge_record(None, scope, name, event);
            let mut value = snapshot_record(name, Some(&memory), now_ms(), 300);
            value["source"] = json!("current_probe_not_persisted");
            if let Err(error) = other {
                value["observation_error"] = safe_error(&error);
            }
            value
        }
    };
    value["checked_now"] = json!(true);
    value["probe_error"] = error.map(Code::value).unwrap_or(Value::Null);
    if let Err(error) = saved {
        value["observation_error"] = safe_error(&error);
    }
    Ok(value)
}

#[cfg(test)]
#[path = "connection_status/tests.rs"]
mod tests;
