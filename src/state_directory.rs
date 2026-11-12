//! Relocate the complete local state while retaining a recoverable original.
//! The locator is the commit point; recovery never replays remote operations.
use crate::{service::{self, ServiceAction, ServiceOptions}, state};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs::{self, File, OpenOptions}, io::{Read, Write}, os::{fd::AsRawFd, unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt}}, path::{Path, PathBuf}, time::{Duration, Instant}};

pub mod paths;

pub const MARKER: &str = "state-directory.json";
const TARGET: &str = "state-directory-target.json";
const JOURNAL: &str = "state-directory-migration.json";
const LOCATOR: &str = "shared-home.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker { version: u32, id: String, source: PathBuf, destination: PathBuf, phase: String }

/// Refuse both an in-progress writer and a stale selection of the old home.
/// Missing state is deliberately left to the normal command's existing policy.
pub fn guard(home: &Path) -> Result<()> {
    let Some(bytes) = read_optional(&home.join(MARKER), 4096)? else { return Ok(()); };
    let marker: Marker = serde_json::from_slice(&bytes).context("invalid state-directory migration marker")?;
    if marker.version != 1 || !["migrating", "moved"].contains(&marker.phase.as_str()) || !marker.source.is_absolute() || !marker.destination.is_absolute() || uuid::Uuid::parse_str(&marker.id).is_err() {
        bail!("invalid state-directory migration marker; preserve this directory and inspect migration recovery");
    }
    bail!("state directory is {} to {}; run rdrive config recover to finish an interrupted migration", marker.phase, marker.destination.display())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Object { bytes: u64, sha256: String, mode: u32, directory: bool, modified_secs: i64, modified_nanos: i64, metadata: Option<String> }
type Manifest = BTreeMap<PathBuf, Object>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase { Preparing, Copied, Published, BackedUp, Committed, Complete, RolledBack }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32, id: String, phase: Phase, source: PathBuf, destination: PathBuf,
    stage: PathBuf, backup: PathBuf, failed_copy: PathBuf,
    locator_before: Option<Vec<u8>>, paused: bool,
    service: Option<ServiceOptions>, running: bool, manifest: Manifest,
    paths: Option<paths::RebaseReport>,
}

trait Operations {
    fn options(&mut self, home: &Path) -> Result<Option<ServiceOptions>>;
    fn running(&mut self, home: &Path) -> Result<bool>;
    fn action(&mut self, home: &Path, action: ServiceAction) -> Result<()>;
    fn install(&mut self, options: &ServiceOptions) -> Result<()>;
    fn checkpoint(&mut self, _phase: &str) -> Result<()> { Ok(()) }
    fn wait_ready(&mut self, home: &Path) -> Result<()>;
}
struct System {
    options: fn(&Path)->Result<Option<ServiceOptions>>, status: fn(&Path)->Result<Value>,
    action: fn(&Path,ServiceAction,bool)->Result<Value>, install: fn(&ServiceOptions,bool)->Result<Value>,
    timeout: Duration, poll: Duration,
}
impl Default for System { fn default()->Self { Self { options:service::options,status:service::status,action:service::action,install:service::install,timeout:Duration::from_secs(90),poll:Duration::from_millis(200) } } }
impl Operations for System {
    fn options(&mut self, home: &Path) -> Result<Option<ServiceOptions>> { (self.options)(home) }
    fn running(&mut self, home: &Path) -> Result<bool> {
        let status = (self.status)(home)?;
        Ok(status["launchd"]["loaded"] == true || status["supervisor_running"] == true)
    }
    fn action(&mut self, home: &Path, action: ServiceAction) -> Result<()> { (self.action)(home, action, false).map(|_| ()) }
    fn install(&mut self, options: &ServiceOptions) -> Result<()> { (self.install)(options, false).map(|_| ()) }
    fn wait_ready(&mut self, home: &Path) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let value = (self.status)(home)?;
            let children = &value["runtime"]["children"];
            if value["launchd"]["running"] == true && value["supervisor_running"] == true
                && children["broker"]["pid"].as_u64().is_some_and(|pid| pid > 1)
                && (value["native_watcher"].is_null() || children["native_watcher"]["pid"].as_u64().is_some_and(|pid| pid > 1)) {
                if let Some(options)=(self.options)(home)? { if connection_ready(home,&options,&value).unwrap_or(false) { return Ok(()); } }
            }
            if Instant::now() >= deadline { bail!("the relocated background service has not become ready; run rdrive config recover"); }
            std::thread::sleep(self.poll);
        }
    }
}

fn process_matches(pid:u64,expected:&Path)->bool {
    if pid<=1 || pid>i32::MAX as u64 || unsafe {libc::kill(pid as i32,0)}!=0 {return false;}
    #[cfg(target_os="macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes=vec![0u8;4096];
        let length=unsafe {libc::proc_pidpath(pid as i32,bytes.as_mut_ptr().cast(),bytes.len() as u32)};
        length>0 && Path::new(std::ffi::OsStr::from_bytes(&bytes[..bytes.iter().position(|byte|*byte==0).unwrap_or(bytes.len())]))==expected
    }
    #[cfg(not(target_os="macos"))]
    {fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|path|path==expected)}
}
fn connection_ready(home:&Path,options:&ServiceOptions,status:&Value)->Result<bool> {
    use std::io::BufRead;
    use std::os::unix::{fs::FileTypeExt,net::UnixStream};
    let broker=status["runtime"]["children"]["broker"]["pid"].as_u64().unwrap_or(0);
    if !process_matches(broker,&options.executable) {return Ok(false);}
    if let Some(app)=&options.app {
        let watcher=status["runtime"]["children"]["native_watcher"]["pid"].as_u64().unwrap_or(0);
        if !process_matches(watcher,&app.join("Contents/MacOS/RDriveCompanion")) {return Ok(false);}
    }
    let descriptor:Value=read_json(&home.join("provider-admin.json"))?;
    let socket=options.ipc_root.join("bridge.sock");
    if descriptor["version"]!=1 || descriptor["home"]!=home.to_string_lossy().as_ref() || descriptor["socket"]!=socket.to_string_lossy().as_ref()
        || descriptor["instance_id"].as_str().is_none_or(|id|uuid::Uuid::parse_str(id).is_err()) {return Ok(false);}
    private_directory(&options.ipc_root)?;let metadata=fs::symlink_metadata(&socket)?;
    if !metadata.file_type().is_socket() || metadata.uid()!=unsafe{libc::geteuid()} || metadata.mode()&0o777!=0o600 {return Ok(false);}
    let mut stream=UnixStream::connect(socket)?;stream.set_read_timeout(Some(Duration::from_millis(500)))?;stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(b"{\"v\":1,\"op\":\"domains\"}\n")?;
    let mut bytes=Vec::new();std::io::BufReader::new(stream).take(65537).read_until(b'\n',&mut bytes)?;
    if bytes.len()>65536 || bytes.last()!=Some(&b'\n') {return Ok(false);}
    let reply:Value=serde_json::from_slice(&bytes)?;
    let expected:std::collections::BTreeSet<_>=crate::provider::domains(home)?.into_iter().map(|domain|domain.id).collect();
    let actual:std::collections::BTreeSet<_>=reply["domains"].as_array().context("invalid broker domain readiness reply")?.iter().map(|domain|domain["id"].as_str().map(str::to_owned).context("domain ID missing from broker readiness reply")).collect::<Result<_>>()?;
    Ok(reply["ok"]==true && actual==expected)
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        bail!("state migration directories must be real, private and owned by the current user: {}", path.display());
    }
    Ok(())
}
fn private_file(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
        bail!("state migration files must be private owned regular files without links");
    }
    Ok(())
}
fn read_optional(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let before = match fs::symlink_metadata(path) { Ok(value) => value, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.into()) };
    private_file(&before)?;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    let opened = file.metadata()?; private_file(&opened)?;
    if (before.dev(), before.ino()) != (opened.dev(), opened.ino()) { bail!("state migration file changed while being inspected"); }
    let mut bytes = Vec::new(); file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit { bail!("state migration metadata exceeds its size limit"); }
    let after = fs::symlink_metadata(path)?;
    if (opened.dev(), opened.ino(), opened.len(), opened.mtime(), opened.mtime_nsec()) != (after.dev(), after.ino(), after.len(), after.mtime(), after.mtime_nsec()) { bail!("state migration file changed while being read"); }
    Ok(Some(bytes))
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_optional(path, 256 * 1024 * 1024)?.context("migration metadata is missing")?).context("invalid state migration metadata")
}
fn create_private(path: &Path) -> Result<()> { fs::DirBuilder::new().mode(0o700).create(path)?; File::open(path.parent().context("directory needs a parent")?)?.sync_all()?; Ok(()) }
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> { state::atomic_json(path, value) }

struct Lease(File);
impl Lease {
    fn acquire(path:&Path,timeout:Duration)->Result<Self> { Self::open(path,timeout,true)?.context("migration lock is missing") }
    fn open(path: &Path, timeout: Duration, create:bool) -> Result<Option<Self>> {
        let file = match OpenOptions::new().read(true).write(true).create(create).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {Ok(file)=>file,Err(error) if !create&&error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(error.into())};
        private_file(&file.metadata()?)?;
        let metadata = fs::symlink_metadata(path)?;
        if (metadata.dev(), metadata.ino()) != (file.metadata()?.dev(), file.metadata()?.ino()) { bail!("migration lock was replaced"); }
        let deadline = Instant::now() + timeout;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 { return Ok(Some(Self(file))); }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock { return Err(error.into()); }
            if Instant::now() >= deadline { bail!("another process is using this state directory; migration left it in place"); }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Lease { fn drop(&mut self) { let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) }; } }

fn paused(home: &Path) -> Result<bool> {
    let Some(raw) = read_optional(&home.join("smart/control.json"), 4096)? else { return Ok(false); };
    let value: Value = serde_json::from_slice(&raw)?;
    value["paused"].as_bool().context("invalid Smart pause control")
}
fn set_paused(home: &Path, wanted: bool) -> Result<()> {
    let root = home.join("smart");
    if !root.exists() { create_private(&root)?; }
    let path = root.join("control.json");
    let before = read_optional(&path, 4096)?;
    let mut value: Value = before.as_ref().map(|raw| serde_json::from_slice(raw)).transpose()?.unwrap_or(json!({"version":1,"paused":false}));
    if !value["paused"].is_boolean() || value["version"] != 1 { bail!("invalid Smart pause control"); }
    if value["paused"] != wanted {
        value["paused"] = json!(wanted); value["updated"] = json!(state::now());
        if read_optional(&path, 4096)? != before { bail!("Smart pause control changed concurrently"); }
        write_json(&path, &value)?;
    }
    Ok(())
}
fn queue_roots(home: &Path) -> Result<Vec<PathBuf>> {
    let directory = home.join("smart");
    if !directory.exists() { return Ok(Vec::new()); }
    private_directory(&directory)?;
    let mut roots = Vec::new();
    for entry in fs::read_dir(directory)? { let path = entry?.path(); if path.file_name().is_some_and(|name| name == "control.json") { continue; } private_directory(&path)?; if path.join("queue.json").exists() { roots.push(path); } }
    Ok(roots)
}
fn active_work(home: &Path) -> Result<bool> {
    for path in queue_roots(home)? {
        let value: Value = read_json(&path.join("queue.json"))?;
        let mutations = &value["mutations"];
        let active = if let Some(items) = mutations.as_object() { items.values().any(|item| item["state"] == "running") } else if let Some(items) = mutations.as_array() { items.iter().any(|item| item["state"] == "running") } else { bail!("invalid retained Smart mutation journal"); };
        if active { return Ok(true); }
    }
    Ok(false)
}
fn drain(home:&Path)->Result<()> {drain_with(home,Duration::from_secs(180),Duration::from_millis(100))}
fn drain_with(home: &Path, timeout:Duration,poll:Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while active_work(home)? { if Instant::now() >= deadline { bail!("active synchronization did not become idle; no active operation was terminated"); } std::thread::sleep(poll); }
    Ok(())
}
fn probe_locks(home:&Path)->Result<()> {
    let mut guards=Vec::new();
    let mut paths=vec![home.join("provider-admin.lock"),home.join("client.lock")];paths.extend(queue_roots(home)?.into_iter().map(|root|root.join("queue.lock")));
    for path in paths {if let Some(lease)=Lease::open(&path,Duration::ZERO,false)? {guards.push(lease);}}
    Ok(())
}
fn locks(home: &Path, timeout: Duration) -> Result<Vec<Lease>> {
    let mut result = vec![Lease::acquire(&home.join("provider-admin.lock"), timeout)?, Lease::acquire(&home.join("client.lock"), timeout)?];
    for root in queue_roots(home)? { result.push(Lease::acquire(&root.join("queue.lock"), Duration::ZERO)?); }
    Ok(result)
}
fn excluded(relative: &Path) -> bool {
    let top = relative.components().next().map(|part| part.as_os_str());
    top.is_some_and(|name| ["service", "client.lock", "provider-admin.lock", "provider-admin.json", MARKER, TARGET].iter().any(|expected| name == *expected)) || relative.file_name().is_some_and(|name| name == "queue.lock")
}
fn walk(home: &Path) -> Result<Manifest> {
    fn visit(home: &Path, directory: &Path, output: &mut Manifest) -> Result<()> {
        private_directory(directory)?;
        for entry in fs::read_dir(directory)? {
            let path = entry?.path(); let relative = path.strip_prefix(home)?.to_path_buf();
            if excluded(&relative) { continue; }
            let metadata = fs::symlink_metadata(&path)?;
            let directory = metadata.is_dir() && !metadata.file_type().is_symlink();
            if directory { output.insert(relative, Object {bytes:0,sha256:String::new(),mode:metadata.mode()&0o777,directory:true,modified_secs:metadata.mtime(),modified_nanos:metadata.mtime_nsec(),metadata:None}); visit(home, &path, output)?; }
            else { private_file(&metadata)?; let retained_metadata=if relative.components().any(|part|part.as_os_str()=="dirty") || relative.starts_with("spool") {Some(crate::metadata::capture(&path,&Default::default())?.fingerprint()?)} else {None}; output.insert(relative, Object { bytes: metadata.len(), sha256: state::fingerprint(&path)?, mode: metadata.mode() & 0o777, directory:false,modified_secs:metadata.mtime(),modified_nanos:metadata.mtime_nsec(),metadata:retained_metadata }); }
        }
        Ok(())
    }
    let mut output = Manifest::new(); visit(home, home, &mut output)?; Ok(output)
}
fn clone_file(source: &Path, destination: &Path) -> Result<bool> {
    #[cfg(target_os="macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source=std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let destination=std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        Ok(unsafe { libc::clonefile(source.as_ptr(),destination.as_ptr(),0) }==0)
    }
    #[cfg(not(target_os="macos"))]
    { let _=(source,destination); Ok(false) }
}
fn copy_file(source:&Path,destination:&Path,object:&Object,clone:fn(&Path,&Path)->Result<bool>)->Result<()> {
    let mut input=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(source)?;
    private_file(&input.metadata()?)?;
    if !clone(source,destination)? {
        let mut output=OpenOptions::new().create_new(true).read(true).write(true).mode(object.mode|0o200).open(destination)?;
        let metadata=input.metadata()?;
        std::io::copy(&mut input,&mut output)?; output.flush()?;
        let attributes=crate::metadata::capture_file(&input,&Default::default())?;
        crate::metadata::apply_file(&output,&attributes,&Default::default())?;
        fs::set_permissions(destination,fs::Permissions::from_mode(object.mode))?;
        output.set_times(fs::FileTimes::new().set_accessed(metadata.accessed()?).set_modified(metadata.modified()?))?;
        output.sync_all()?;
    } else { File::open(destination)?.sync_all()?; }
    Ok(())
}
fn copy_tree(source: &Path, destination: &Path, manifest: &Manifest) -> Result<()> {
    private_directory(destination)?;
    for (relative, object) in manifest {
        let from=source.join(relative);let to=destination.join(relative);
        if object.directory { create_private(&to)?; } else { copy_file(&from,&to,object,clone_file)?; }
    }
    // Directory entries, empty directories and directory times are durable too.
    for (relative,object) in manifest.iter().rev().filter(|(_,object)|object.directory) {
        let from=File::open(source.join(relative))?.metadata()?;let to=File::open(destination.join(relative))?;
        to.set_times(fs::FileTimes::new().set_accessed(from.accessed()?).set_modified(from.modified()?))?;
        fs::set_permissions(destination.join(relative),fs::Permissions::from_mode(object.mode))?;to.sync_all()?;
    }
    if walk(destination)? != *manifest { bail!("copied state differs from its verified source; source data was retained"); }
    File::open(destination)?.sync_all()?;Ok(())
}

fn save(standard: &Path, intent: &Intent) -> Result<()> { write_json(&standard.join(JOURNAL), intent) }
fn marker(intent: &Intent, phase: &str) -> Marker { Marker { version:1, id:intent.id.clone(), source:intent.source.clone(), destination:intent.destination.clone(), phase:phase.into() } }
fn target_owned(intent: &Intent, path: &Path) -> Result<bool> {
    if !path.exists() { return Ok(false); }
    private_directory(path)?;
    if read_optional(&path.join(TARGET), 4096)?.is_none() { return Ok(false); }
    let value: Marker = read_json(&path.join(TARGET))?;
    if value.id != intent.id || value.source != intent.source || value.destination != intent.destination || value.version != 1 { bail!("migration target ownership changed; no directory was removed"); }
    Ok(true)
}
fn locator(standard: &Path, destination: &Path) -> Vec<u8> { let _ = standard; serde_json::to_vec_pretty(&json!({"version":1,"home":destination})).expect("serializable path reference") }
fn committed(standard: &Path, intent: &Intent) -> Result<bool> {
    Ok(matches!(intent.phase, Phase::Committed | Phase::Complete) || read_optional(&standard.join(LOCATOR), 4096)?.as_ref().is_some_and(|raw| serde_json::from_slice::<Value>(raw).ok().is_some_and(|v| v["version"] == 1 && v["home"] == intent.destination.to_string_lossy().as_ref())))
}
fn seal_source(intent: &Intent) -> Result<()> {
    if intent.source.exists() { bail!("old state path unexpectedly reappeared; original backup was retained"); }
    let stub=intent.stage.with_file_name(format!("state-stub-{}",intent.id));
    create_private(&stub)?;
    write_json(&stub.join(MARKER),&marker(intent,"moved"))?;
    let lock=OpenOptions::new().create_new(true).write(true).mode(0o400).open(stub.join("client.lock"))?;lock.sync_all()?;
    File::open(&stub)?.sync_all()?;
    fs::rename(&stub,&intent.source)?;
    fs::set_permissions(&intent.source,fs::Permissions::from_mode(0o500))?;
    File::open(intent.source.parent().context("source needs a parent")?)?.sync_all()?;
    Ok(())
}
fn remove_stub(intent: &Intent) -> Result<()> {
    if !intent.source.exists() { return Ok(()); }
    private_directory(&intent.source)?;
    let value: Marker = read_json(&intent.source.join(MARKER))?;
    let names: Vec<_> = fs::read_dir(&intent.source)?.map(|entry| entry.map(|e|e.file_name())).collect::<std::io::Result<_>>()?;
    if value.id != intent.id || names.iter().any(|name| name != MARKER && name != "client.lock") { bail!("old state stub changed; all original data was preserved"); }
    let retired=intent.stage.with_file_name(format!("state-retired-stub-{}",intent.id));
    if retired.exists() {bail!("retired migration stub already exists; original backup was preserved");}
    // Keep the complete marker and read-only client lock intact throughout.
    fs::set_permissions(&intent.source,fs::Permissions::from_mode(0o700))?;
    fs::rename(&intent.source,&retired)?;
    File::open(intent.source.parent().context("source needs a parent")?)?.sync_all()?;
    File::open(retired.parent().context("retired stub needs a parent")?)?.sync_all()?;
    Ok(())
}

fn validate_intent(standard: &Path, intent: &Intent) -> Result<()> {
    let id = uuid::Uuid::parse_str(&intent.id)?;
    if intent.version != 1 || id.to_string() != intent.id || intent.source == intent.destination || intent.source.starts_with(&intent.destination) || intent.destination.starts_with(&intent.source)
        || intent.stage != standard.join(format!("state-stage-{id}")) || intent.backup != standard.join(format!("state-backup-{id}")) || intent.failed_copy != standard.join(format!("state-incomplete-{id}"))
        || !intent.source.is_absolute() || !intent.destination.is_absolute()
        || [&intent.source,&intent.destination,&intent.stage,&intent.backup,&intent.failed_copy].iter().any(|path|path.components().any(|part|matches!(part,std::path::Component::ParentDir|std::path::Component::CurDir)))
        || intent.service.as_ref().is_some_and(|options|options.home!=intent.source)
        || [&intent.stage,&intent.backup,&intent.failed_copy].contains(&&intent.destination)
        || intent.manifest.keys().any(|path|path.is_absolute()||path.components().any(|part|matches!(part,std::path::Component::ParentDir|std::path::Component::CurDir))) { bail!("invalid state relocation journal; all directories were preserved"); }
    if let Some(report)=&intent.paths {paths::validate_report(report,&intent.source,&intent.destination)?;}
    Ok(())
}
fn finish(standard: &Path, intent: &mut Intent, operations: &mut dyn Operations) -> Result<Value> {
    let selected=read_optional(&standard.join(LOCATOR),4096)?.context("committed shared locator is missing; preserve all state copies")?;
    let selected:Value=serde_json::from_slice(&selected)?;
    if selected["version"]!=1 || selected["home"]!=intent.destination.to_string_lossy().as_ref() {bail!("shared locator changed after migration commit; activation did not alter another selected home");}
    if !target_owned(intent, &intent.destination)? { bail!("committed state is missing; restore its retained copy before recovery"); }
    if let Some(report) = &intent.paths { paths::apply_external_markers(report, true)?; }
    set_paused(&intent.destination, intent.paused)?;
    if let Some(previous) = &intent.service {
        let mut next = previous.clone(); next.home = intent.destination.clone();
        operations.install(&next)?;
        if intent.running { operations.action(&intent.destination, ServiceAction::Start)?; operations.wait_ready(&intent.destination)?; }
    }
    operations.checkpoint("activated")?;
    intent.phase = Phase::Complete; save(standard, intent)?;
    let bytes: u64 = intent.manifest.values().map(|value| value.bytes).sum();
    Ok(json!({"migrated":true,"home":intent.destination,"destination":intent.destination,"source":intent.source,"source_backup":intent.backup,"service_running":intent.running,"original_sync_paused":intent.paused,"verified_files":intent.manifest.values().filter(|object|!object.directory).count(),"verified_bytes":bytes,"changed_path_files":intent.paths.as_ref().map(|report|report.changed_files.len()).unwrap_or(0)}))
}
fn rollback(standard: &Path, intent: &mut Intent, operations: &mut dyn Operations) -> Result<()> {
    if committed(standard, intent)? { bail!("migration already committed; use recovery to finish the destination service"); }
    if let Some(report) = &intent.paths { paths::apply_external_markers(report, false)?; }
    if intent.backup.exists() {
        private_directory(&intent.backup)?;
        remove_stub(intent)?; fs::rename(&intent.backup, &intent.source)?;
        File::open(intent.source.parent().context("restored source needs a parent")?)?.sync_all()?;
        File::open(standard)?.sync_all()?;
    }
    if !intent.source.exists() {bail!("original state and its retained backup are unavailable; no recovery result was assumed");}
    private_directory(&intent.source)?;
    if let Some(raw) = read_optional(&intent.source.join(MARKER),4096)? {
            let value: Marker = serde_json::from_slice(&raw)?;
            if value.id != intent.id { bail!("source migration marker changed; original state was preserved"); }
            fs::remove_file(intent.source.join(MARKER))?;
            File::open(&intent.source)?.sync_all()?;
    }
    set_paused(&intent.source, intent.paused)?;
    for path in [&intent.destination, &intent.stage] {
        if target_owned(intent, path)? {
            if intent.failed_copy.exists() { bail!("an incomplete copy already exists; all copies were preserved"); }
            fs::rename(path, &intent.failed_copy)?;
        }
    }
    let current = read_optional(&standard.join(LOCATOR), 4096)?;
    if current != intent.locator_before { bail!("shared locator changed concurrently; recovery did not overwrite it"); }
    if let Some(options) = &intent.service { operations.install(options)?; if intent.running { operations.action(&intent.source, ServiceAction::Start)?; operations.wait_ready(&intent.source)?; } }
    intent.phase = Phase::RolledBack; save(standard, intent)
}

pub fn show(home: &Path, standard: &Path) -> Result<Value> {
    guard(home)?;
    Ok(json!({"home":home,"configuration":home.join("config.json"),"shared_locator":standard.join(LOCATOR),"migration_journal":standard.join(JOURNAL),"standard_home":standard.join("state")}))
}
fn require_relocatable_checkpoints(home: &Path) -> Result<()> {
    let checkpoints = home.join("checkpoints");
    match fs::symlink_metadata(&checkpoints) {
        Ok(_) => private_directory(&checkpoints)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let vault = checkpoints.join("vault");
    match fs::symlink_metadata(&vault) {
        Ok(_) => private_directory(&vault)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if fs::read_dir(&vault)?.next().transpose()?.is_some() {
        bail!("state relocation with a retained APFS checkpoint vault is not supported; keep the current state location and export checkpoint payloads for recovery");
    }
    Ok(())
}
pub fn migrate(home: &Path, destination: &Path, standard: &Path, apply: bool) -> Result<Value> { migrate_with(home, destination, standard, apply, &mut System::default()) }
fn migrate_with(home: &Path, destination: &Path, standard: &Path, apply: bool, operations: &mut dyn Operations) -> Result<Value> {
    guard(home)?; private_directory(home)?;
    require_relocatable_checkpoints(home)?;
    let source = home.canonicalize()?; let destination = state::resolve_location(destination)?; let standard = state::resolve_location(standard)?;
    if source == destination || destination.starts_with(&source) || source.starts_with(&destination) || source == standard || standard.starts_with(&source) { bail!("state source, destination and migration-control directory must not overlap"); }
    if fs::symlink_metadata(&destination).is_ok() { bail!("migration destination must not already exist; existing data was preserved"); }
    let parent = destination.parent().context("destination needs a parent")?.to_path_buf(); private_directory(&parent)?; private_directory(&standard)?;
    if fs::metadata(&source)?.dev()!=fs::metadata(&parent)?.dev() || fs::metadata(&source)?.dev()!=fs::metadata(&standard)?.dev() {bail!("state migration currently requires source, destination and control directory on the same filesystem");}
    crate::client_read::config(&source)?;
    let locator_before = read_optional(&standard.join(LOCATOR), 4096)?;
    if let Some(bytes) = &locator_before { let value:Value = serde_json::from_slice(bytes)?; if value["version"] != 1 || value["home"] != source.to_string_lossy().as_ref() { bail!("shared locator belongs to another state directory; migration did not change it"); } }
    let service = operations.options(&source)?;
    if service.as_ref().is_some_and(|options| options.ipc_root.starts_with(&source) || options.executable.starts_with(&source) || options.app.as_ref().is_some_and(|app|app.starts_with(&source)) || options.launch_agents_dir.as_ref().is_some_and(|path|path.starts_with(&source))) { bail!("service artifacts and IPC must be outside the state directory before relocation"); }
    let running = service.as_ref().is_some() && operations.running(&source)?;
    if !running {probe_locks(&source)?;if active_work(&source)? {bail!("unmanaged interrupted work must be recovered before relocating its state");}}
    let pending = read_optional(&standard.join(JOURNAL), 256 * 1024 * 1024)?;
    if let Some(raw) = &pending { let previous:Intent = serde_json::from_slice(raw)?; validate_intent(&standard,&previous)?; if !matches!(previous.phase, Phase::Complete | Phase::RolledBack) { bail!("an interrupted state migration exists; run rdrive config recover first"); } }
    if !apply { return Ok(json!({"apply":false,"source":source,"destination":destination,"service_installed":service.is_some(),"service_running":running,"shared_locator":standard.join(LOCATOR),"source_retained_as_backup":true})); }
    let _global = Lease::acquire(&standard.join("state-directory-migration.lock"), Duration::ZERO)?;
    if read_optional(&standard.join(JOURNAL),256 * 1024 * 1024)? != pending { bail!("migration control changed after preflight"); }
    let id = uuid::Uuid::new_v4().to_string();
    let mut intent = Intent { version:1,id:id.clone(),phase:Phase::Preparing,source:source.clone(),destination,stage:standard.join(format!("state-stage-{id}")),backup:standard.join(format!("state-backup-{id}")),failed_copy:standard.join(format!("state-incomplete-{id}")),locator_before,paused:paused(&source)?,service,running,manifest:Manifest::new(),paths:None };
    save(&standard,&intent)?;
    let result = (|| {
        operations.checkpoint("planned")?;
        set_paused(&source,true)?; drain(&source)?;
        if intent.running { operations.action(&source,ServiceAction::Stop)?; }
        let leases = locks(&source,Duration::from_secs(15))?;
        if active_work(&source)? { bail!("active work remains after stopping the service; original data was retained"); }
        require_relocatable_checkpoints(&source)?;
        write_json(&source.join(MARKER),&marker(&intent,"migrating"))?;
        operations.checkpoint("stopped")?;
        intent.manifest=walk(&source)?;
        create_private(&intent.stage)?;
        write_json(&intent.stage.join(TARGET),&marker(&intent,"target"))?;
        copy_tree(&source,&intent.stage,&intent.manifest)?;
        intent.paths=Some(paths::rebase_tree(&intent.stage,&source,&intent.destination)?);
        intent.phase=Phase::Copied;save(&standard,&intent)?;operations.checkpoint("copied")?;
        if walk(&source)? != intent.manifest { bail!("source changed during migration; every copy was retained"); }
        if intent.destination.exists() { bail!("destination appeared during migration; it was not replaced"); }
        fs::rename(&intent.stage,&intent.destination)?;File::open(&parent)?.sync_all()?;
        intent.phase=Phase::Published;save(&standard,&intent)?;operations.checkpoint("published")?;
        if intent.service.is_some() { operations.action(&source,ServiceAction::Uninstall)?; }
        fs::rename(&source,&intent.backup)?;File::open(source.parent().context("source needs a parent")?)?.sync_all()?;
        intent.phase=Phase::BackedUp;save(&standard,&intent)?;operations.checkpoint("backed_up")?;
        seal_source(&intent)?;
        if let Some(report)=&intent.paths {paths::apply_external_markers(report,true)?;}
        if read_optional(&standard.join(LOCATOR),4096)? != intent.locator_before {bail!("shared locator changed during migration; original data was preserved");}
        state::atomic_write(&standard.join(LOCATOR),&locator(&standard,&intent.destination))?;
        intent.phase=Phase::Committed;save(&standard,&intent)?;operations.checkpoint("committed")?;
        drop(leases);
        finish(&standard,&mut intent,operations)
    })();
    match result {
        Ok(value)=>Ok(value),
        Err(error)=>{
            if committed(&standard,&intent)? {return Err(error.context("state migration committed safely; run rdrive config recover to finish activation"));}
            if let Err(recovery)=rollback(&standard,&mut intent,operations){return Err(error.context(format!("automatic rollback could not finish ({recovery:#}); all copies were retained; run rdrive config recover")));}
            Err(error.context("state migration rolled back; original state and retained data are available"))
        }
    }
}
pub fn recover(standard: &Path) -> Result<Value> { recover_with(standard,&mut System::default()) }
fn recover_with(standard: &Path, operations: &mut dyn Operations) -> Result<Value> {
    private_directory(standard)?;
    let _global=Lease::acquire(&standard.join("state-directory-migration.lock"),Duration::ZERO)?;
    let Some(raw)=read_optional(&standard.join(JOURNAL),256*1024*1024)? else{return Ok(json!({"recovered":false,"pending":false}));};
    let mut intent:Intent=serde_json::from_slice(&raw)?;validate_intent(standard,&intent)?;
    if intent.phase==Phase::Complete {return Ok(json!({"recovered":true,"committed":true,"home":intent.destination,"source_backup":intent.backup}));}
    if intent.phase==Phase::RolledBack {return Ok(json!({"recovered":true,"committed":false,"home":intent.source}));}
    if committed(standard,&intent)? {finish(standard,&mut intent,operations)}else{rollback(standard,&mut intent,operations)?;Ok(json!({"recovered":true,"committed":false,"home":intent.source}))}
}

#[cfg(test)]
mod tests;
