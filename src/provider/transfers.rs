//! Ephemeral Finder payload observations. Receipts remain authoritative in the
//! existing provider journal; no byte counter is written to disk or item versions.
use super::*;
use crate::transfer::{JobProgress, TransferContext};
use std::{collections::VecDeque, sync::{Mutex, MutexGuard, OnceLock}, time::{Duration, Instant}};

const LIMIT: usize = 256;
const RETAIN: Duration = Duration::from_secs(30);

#[derive(Default, Clone)]
pub(super) struct Origin { pub mutation_id: Option<String>, pub background: bool }

#[derive(Default)]
struct Registry { entries: VecDeque<Arc<Entry>> }

struct Details { path: Option<String>, total: Option<u64>, terminal: Option<(&'static str, Instant)> }
struct Entry {
    home: PathBuf, domain: String, id: String, transfer_id: String,
    operation: &'static str, origin: Origin, context: TransferContext, details: Mutex<Details>,
}
pub(super) struct Activity(Arc<Entry>);
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> { value.lock().unwrap_or_else(|error| error.into_inner()) }
fn registry() -> &'static Mutex<Registry> { REGISTRY.get_or_init(Default::default) }

impl Registry {
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|entry| lock(&entry.details).terminal.is_none_or(|(_, at)| now.saturating_duration_since(at) < RETAIN));
    }
    fn insert(&mut self, entry: Arc<Entry>) -> Option<Activity> {
        self.prune(Instant::now());
        if self.entries.len() >= LIMIT {
            if let Some(index) = self.entries.iter().position(|row| lock(&row.details).terminal.is_some()) { self.entries.remove(index); }
            else { return None; }
        }
        self.entries.push_back(entry.clone());
        Some(Activity(entry))
    }
}

/// A context without a bridge request label receives its own attempt label.
pub(super) fn context(context: TransferContext) -> TransferContext {
    if context.scope_label().is_some() { context } else { context.scoped(uuid::Uuid::new_v4().to_string()) }
}

pub(super) fn begin(home: &Path, request: &Request, context: &TransferContext, origin: Origin) -> Option<Activity> {
    let operation = match request.op.as_str() {
        "fetch" => "download",
        "create" if request.kind.as_deref() != Some("directory") => "upload",
        "modify" if request.content_path.is_some() => "upload",
        _ => return None,
    };
    // Smart's client receipt ID is content-derived. Its retained origin is the
    // provisional item identifier that the system originally supplied.
    let id = request.id.as_ref().or(request.smart_origin_id.as_ref()).or(request.client_item_id.as_ref())?;
    if id.is_empty() || request.domain.is_empty() { return None; }
    lock(registry()).insert(Arc::new(Entry {
        home: home.into(), domain: request.domain.clone(), id: id.clone(),
        transfer_id: uuid::Uuid::new_v4().to_string(), operation, origin, context: context.clone(),
        details: Mutex::new(Details { path: None, total: None, terminal: None }),
    }))
}

impl Activity {
    /// Bind only an authoritative content path at the actual payload call site.
    pub(super) fn bind(&self, path: String, total: u64) {
        let mut details = lock(&self.0.details); details.path = Some(path); details.total = Some(total);
    }
    pub(super) fn finish(&self, result: &Result<Value>) {
        lock(&self.0.details).terminal = Some((if result.is_ok() { "completed" } else { "failed" }, Instant::now()));
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut details = lock(&self.0.details);
        if details.terminal.is_none() { details.terminal = Some(("cancelled", Instant::now())); }
    }
}

fn primary(job: &JobProgress, direction: &str, path: &str) -> bool {
    if direction == "download" { return matches!(job.operation.as_str(), "download" | "download_resume") && job.path == path; }
    match job.operation.as_str() {
        "upload" => job.path == path,
        "upload_chunk" => job.path.starts_with(&format!("{path} [")),
        "smb.write" => {
            let (parent, name) = job.path.rsplit_once('/').unwrap_or(("", &job.path));
            let expected_parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            job.path == path || parent == expected_parent && name.strip_prefix(".rdrive-upload-").is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
        }
        _ => false,
    }
}

impl Entry {
    fn snapshot(&self) -> Value {
        let details = lock(&self.details);
        let jobs = self.context.scoped_jobs().into_iter().filter(|job| details.path.as_ref().is_some_and(|path| primary(job, self.operation, path))).collect::<Vec<_>>();
        let bytes = jobs.iter().fold(0u64, |sum, job| sum.saturating_add(job.bytes));
        let total = details.total.or_else(|| (!jobs.is_empty() && jobs.iter().all(|job| job.total.is_some())).then(|| jobs.iter().fold(0u64, |sum, job| sum.saturating_add(job.total.unwrap()))));
        let completed = total.map_or(bytes, |total| bytes.min(total));
        let state = details.terminal.map(|(state, _)| state).unwrap_or_else(|| {
            if !jobs.is_empty() && (jobs.iter().all(|job| job.state == "complete") || total.is_some_and(|total| completed >= total)) { "committing" } else { "transferring" }
        });
        let speed = if details.terminal.is_some() { 0.0 } else { jobs.iter().map(|job| job.bytes_per_second).sum::<f64>() };
        let speed = if speed.is_finite() && speed >= 0.0 { speed } else { 0.0 };
        let eta = total.and_then(|total| (speed > 0.0).then(|| total.saturating_sub(completed) as f64 / speed));
        json!({"transfer_id":self.transfer_id,"id":self.id,"mutation_id":self.origin.mutation_id,
            "operation":self.operation,"completed":if state=="completed" {total.unwrap_or(completed)} else {completed},
            "total":total,"bytes_per_second":speed,"eta_seconds":eta,"state":state,"background":self.origin.background})
    }
}

/// Existing bridge authorization applies before this local, writer-independent query.
pub(super) fn status(home: &Path, domain: &str) -> Result<Value> {
    if domain.is_empty() { bail!("transfer status requires a domain"); }
    let entries = { let mut registry = lock(registry()); registry.prune(Instant::now()); registry.entries.iter().filter(|entry| entry.home == home && entry.domain == domain).cloned().collect::<Vec<_>>() };
    Ok(json!({"transfers":entries.iter().map(|entry| entry.snapshot()).collect::<Vec<_>>()}))
}

pub(super) fn timed_out(context: &TransferContext) {
    if let Some(entry)=lock(registry()).entries.iter().rev().find(|entry| entry.context.scope_label()==context.scope_label()).cloned() {
        lock(&entry.details).terminal=Some(("failed",Instant::now()));
    }
}

pub(super) fn frame(label: &str) -> Option<Value> {
    let entry = lock(registry()).entries.iter().rev().find(|entry| !entry.origin.background && entry.context.scope_label() == Some(label)).cloned()?;
    // Use the operation's payload binding rather than unrelated metadata jobs.
    let mut snapshot = entry.snapshot();
    if snapshot["state"] != "completed" {
        if let (Some(total), Some(completed)) = (snapshot["total"].as_u64(), snapshot["completed"].as_u64()) {
            snapshot["completed"] = json!(completed.min(total.saturating_sub(1)));
        }
    }
    Some(json!({"progress":snapshot}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(op: &str) -> Request { serde_json::from_value(json!({"v":1,"op":op,"domain":"progress-domain","id":"document","kind":"file","content_path":"staged"})).unwrap() }
    fn setup() -> (tempfile::TempDir, TransferContext, Activity) {
        let root=tempfile::tempdir().unwrap();let context=context(TransferContext::default());
        let activity=begin(root.path(),&req("modify"),&context,Origin{mutation_id:Some("mutation".into()),background:false}).unwrap();
        (root,context,activity)
    }
    #[test]
    fn payload_progress_excludes_auxiliary_work_and_waits_for_receipt() {
        let (root,context,activity)=setup();activity.bind("folder/document".into(),10);
        let archive=context.start("upload","folder/.rdrive-versions/backup",Some(100),0).unwrap();archive.record(100).unwrap();archive.finish();
        let metadata=context.start("upload","folder/.rdrive-meta/sidecar",Some(50),0).unwrap();metadata.record(50).unwrap();metadata.finish();
        let verifying=context.start("download","folder/document",Some(10),0).unwrap();verifying.record(10).unwrap();verifying.finish();
        let payload=context.start("upload","folder/document",Some(10),0).unwrap();payload.record(4).unwrap();
        let state=activity.0.snapshot();assert_eq!(state["completed"],4);assert_eq!(state["total"],10);assert_eq!(state["state"],"transferring");assert!(state["bytes_per_second"].as_f64().unwrap()>0.0);
        assert_eq!(state["id"],"document");assert_eq!(state["mutation_id"],"mutation");assert_eq!(state["background"],false);
        assert_eq!(status(root.path(),"different").unwrap()["transfers"],json!([]));assert_eq!(status(&root.path().join("other-home"),"progress-domain").unwrap()["transfers"],json!([]));
        assert_eq!(status(root.path(),"progress-domain").unwrap()["transfers"][0]["completed"],4);
        payload.record(6).unwrap();payload.finish();
        let state=activity.0.snapshot();assert_eq!(state["state"],"committing");
        let progress_frame=frame(context.scope_label().unwrap()).unwrap();assert_eq!(progress_frame["progress"]["completed"],9);assert_eq!(progress_frame["progress"]["total"],10);
        activity.finish(&Ok(json!({"durable_receipt":true})));
        let state=activity.0.snapshot();assert_eq!(state["state"],"completed");assert_eq!(state["completed"],10);assert_eq!(state["bytes_per_second"],0.0);assert!(state["eta_seconds"].is_null());
        assert_eq!(frame(context.scope_label().unwrap()).unwrap()["progress"]["completed"],10);
        assert!(frame("missing").is_none());assert!(status(root.path(),"").is_err());
    }
    #[test]
    fn unknown_downloads_failures_cancellation_and_background_are_honest() {
        let root=tempfile::tempdir().unwrap();let context=context(TransferContext::default());let mut request=req("fetch");
        let activity=begin(root.path(),&request,&context,Origin{background:true,..Default::default()}).unwrap();
        assert!(activity.0.snapshot()["total"].is_null());assert!(activity.0.snapshot()["eta_seconds"].is_null());assert!(frame(context.scope_label().unwrap()).is_none());
        // A received prefix and verification pass are not the requested file payload.
        activity.bind("document".into(),10);
        let verify=context.start("smb.read-and-verify","document",None,0).unwrap();verify.record(100).unwrap();verify.finish();
        let payload=context.start("download","document",Some(10),4).unwrap();payload.record(2).unwrap();
        assert_eq!(activity.0.snapshot()["completed"],6);
        activity.finish(&Err(anyhow::anyhow!("connection lost")));assert_eq!(activity.0.snapshot()["state"],"failed");assert_eq!(activity.0.snapshot()["completed"],6);
        request.id=None;request.client_item_id=Some("local-client-item".into());
        let cancelled=begin(root.path(),&request,&context,Default::default()).unwrap();let entry=cancelled.0.clone();drop(cancelled);assert_eq!(entry.snapshot()["state"],"cancelled");
        timed_out(&context);assert_eq!(entry.snapshot()["state"],"failed");timed_out(&TransferContext::default().scoped("missing-timeout"));
        assert_eq!(entry.snapshot()["id"],"local-client-item");
        request.smart_origin_id=Some("system-item".into());
        let originated=begin(root.path(),&request,&context,Default::default()).unwrap();assert_eq!(originated.0.snapshot()["id"],"system-item");drop(originated);request.smart_origin_id=None;
        request.client_item_id=None;assert!(begin(root.path(),&request,&context,Default::default()).is_none());
        request.id=Some(String::new());assert!(begin(root.path(),&request,&context,Default::default()).is_none());
        request.id=Some("document".into());request.domain.clear();assert!(begin(root.path(),&request,&context,Default::default()).is_none());
        assert!(begin(root.path(),&req("delete"),&context,Default::default()).is_none());
        let mut directory=req("create");directory.kind=Some("directory".into());assert!(begin(root.path(),&directory,&context,Default::default()).is_none());
        let mut metadata=req("modify");metadata.content_path=None;assert!(begin(root.path(),&metadata,&context,Default::default()).is_none());
        let foreground=context.clone();assert_eq!(super::context(foreground).scope_label(),context.scope_label());
    }
    #[test]
    fn protocol_paths_select_only_primary_payload_jobs() {
        fn job(operation:&str,path:&str)->JobProgress {JobProgress{id:1,label:None,operation:operation.into(),path:path.into(),total:Some(1),resumed:0,transferred:1,bytes:1,bytes_per_second:0.0,eta_seconds:None,state:"complete",error:None}}
        assert!(primary(&job("download_resume","folder/doc"),"download","folder/doc"));
        assert!(!primary(&job("smb.read-and-verify","folder/doc"),"download","folder/doc"));
        assert!(primary(&job("upload_chunk","folder/doc [1/2]"),"upload","folder/doc"));
        assert!(!primary(&job("upload_chunk","other [1/2]"),"upload","folder/doc"));
        assert!(primary(&job("smb.write","folder/doc"),"upload","folder/doc"));
        assert!(primary(&job("smb.write",&format!("folder/.rdrive-upload-{}",uuid::Uuid::new_v4())),"upload","folder/doc"));
        assert!(primary(&job("smb.write",&format!(".rdrive-upload-{}",uuid::Uuid::new_v4())),"upload","doc"));
        assert!(!primary(&job("smb.write","folder/.rdrive-upload-not-a-uuid"),"upload","folder/doc"));
        assert!(!primary(&job("smb.write",&format!("other/.rdrive-upload-{}",uuid::Uuid::new_v4())),"upload","folder/doc"));
        assert!(!primary(&job("metadata","folder/doc"),"upload","folder/doc"));
    }
    #[test]
    fn bounded_registry_preserves_active_work_and_expires_terminal_observations() {
        let (_root,_context,activity)=setup();let mut registry=Registry::default();
        for _ in 0..LIMIT {registry.entries.push_back(activity.0.clone());}
        assert!(registry.insert(activity.0.clone()).is_none());
        let replacement=Arc::new(Entry{home:"other".into(),domain:"domain".into(),id:"item".into(),transfer_id:"replacement".into(),operation:"upload",origin:Default::default(),context:TransferContext::default(),details:Mutex::new(Details{path:None,total:None,terminal:Some(("failed",Instant::now()))})});
        registry.entries[0]=replacement;assert!(registry.insert(activity.0.clone()).is_some());assert_eq!(registry.entries.len(),LIMIT);
        activity.finish(&Err(anyhow::anyhow!("stopped")));registry.prune(Instant::now()+RETAIN);assert!(registry.entries.is_empty());
        let mutex=Mutex::new(0);let _=std::panic::catch_unwind(||{let _guard=mutex.lock().unwrap();panic!("synthetic observer panic");});*lock(&mutex)=1;assert_eq!(*lock(&mutex),1);
    }
}
