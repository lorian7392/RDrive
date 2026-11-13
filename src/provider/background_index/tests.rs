use super::*;
use async_trait::async_trait;
use std::sync::{Mutex as StdMutex,atomic::{AtomicBool,AtomicUsize,Ordering}};
use tokio::sync::Notify;

struct Peer {entries:StdMutex<Vec<Entry>>,block_path:Option<String>,entered:Notify,release:Notify,blocked:AtomicBool,fail:AtomicBool}
impl Peer {
    fn new(entries:Vec<Entry>,block:Option<&str>)->Arc<Self>{Arc::new(Self{entries:StdMutex::new(entries),block_path:block.map(str::to_owned),entered:Notify::new(),release:Notify::new(),blocked:AtomicBool::new(false),fail:AtomicBool::new(false)})}
}
#[async_trait]
impl Backend for Peer {
    fn capabilities(&self)->backend::Capabilities {backend::Capabilities{protocol:"test",conditional_write:true,server_copy:false,locking:false,notes:vec![]}}
    async fn stat(&self,path:&str)->Result<Option<Entry>> {Ok(self.entries.lock().unwrap().iter().find(|entry|entry.path==path).cloned())}
    async fn list(&self,path:&str)->Result<Vec<Entry>> {
        if self.block_path.as_deref()==Some(path)&&!self.blocked.swap(true,Ordering::SeqCst){self.entered.notify_one();self.release.notified().await;}
        if self.fail.load(Ordering::SeqCst){bail!("synthetic read failure");}
        Ok(self.entries.lock().unwrap().iter().filter(|entry|entry.path.rsplit_once('/').map(|(p,_)|p).unwrap_or("")==path).cloned().collect())
    }
    async fn download(&self,_:&str,_:&Path)->Result<()> {bail!("synthetic peer has no file payloads")}
    async fn upload(&self,_:&Path,_:&str,_:WriteCondition)->Result<Entry>{bail!("read-only metadata must never upload")}
    async fn mkdir(&self,_:&str)->Result<()> {bail!("read-only metadata must never create")}
    async fn remove(&self,_:&str,_:bool)->Result<()> {bail!("read-only metadata must never delete")}
    async fn rename(&self,_:&str,_:&str,_:bool)->Result<()> {bail!("read-only metadata must never rename")}
    async fn copy(&self,_:&str,_:&str,_:bool)->Result<()> {bail!("read-only metadata must never copy")}
}
struct Connected {backend:Arc<dyn Backend>,calls:AtomicUsize,fail:bool}
#[async_trait]
impl Connector for Connected {
    async fn connect(&self,_:&config::Profile,_:&str,_:crate::transfer::TransferContext)->Result<Arc<dyn Backend>> {self.calls.fetch_add(1,Ordering::SeqCst);if self.fail{bail!("synthetic authentication failure");}Ok(self.backend.clone())}
}
fn connected(backend:Arc<dyn Backend>)->Connected{Connected{backend,calls:AtomicUsize::new(0),fail:false}}
fn setup()->(tempfile::TempDir,PathBuf,Domain){let(tmp,store,mut domain)=super::super::coverage_tests::setup();domain.smart=true;add_domain(&store,domain.clone()).unwrap();let home=store.home.clone();checked_journal(&home,&store.config,&domain).unwrap();drop(store);(tmp,home,domain)}
fn entry(path:&str,directory:bool)->Entry{super::super::coverage_tests::entry(path,directory)}
fn journal(home:&Path,domain:&Domain)->Journal{let store=Store::open(home.to_path_buf()).unwrap();checked_journal(home,&store.config,domain).unwrap()}
fn reset_scan(home:&Path,domain:&Domain){let store=Store::open(home.to_path_buf()).unwrap();let mut j=checked_journal(home,&store.config,domain).unwrap();j.directory_scans.clear();j.directory_attempts.clear();save_journal(&store,&domain.id,&j).unwrap();}

#[tokio::test]
async fn a_blocked_domain_does_not_hold_store_or_prevent_another_domain_from_publishing() {
    let(_tmp,home,mut first)=setup();first.remote="first".into();let mut second=first.clone();second.id="second".into();second.remote="second".into();
    state::atomic_json(&home.join("provider-domains.json"),&vec![first.clone(),second.clone()]).unwrap();
    // The existing empty first index has a different remote binding; use fresh IDs.
    first.id="first".into();state::atomic_json(&home.join("provider-domains.json"),&vec![first.clone(),second.clone()]).unwrap();
    let peer=Peer::new(vec![entry("first/slow.txt",false),entry("second/ready.txt",false)],Some("first"));
    let coordinator=Arc::new(Mutex::new(()));let h=home.clone();let d=first.clone();let c=coordinator.clone();let backend=peer.clone();
    let pending=tokio::spawn(async move{once(&h,&d,&c,None,&mut Worker::default(),&connected(backend)).await});
    tokio::time::timeout(std::time::Duration::from_secs(1),peer.entered.notified()).await.unwrap();
    drop(Store::open(home.clone()).expect("a network read must not retain Store's writer lock"));
    tokio::time::timeout(std::time::Duration::from_secs(1),once(&home,&second,&coordinator,None,&mut Worker::default(),&connected(peer.clone()))).await.unwrap().unwrap();
    assert!(journal(&home,&second).items.values().any(|record|record.path=="ready.txt"));assert!(!pending.is_finished());
    peer.release.notify_one();pending.await.unwrap().unwrap();assert!(journal(&home,&first).items.values().any(|record|record.path=="slow.txt"));
}

#[tokio::test]
async fn stale_publications_preserve_foreground_namespace_receipts_and_new_configuration() {
    for change in ["anchor","profile","filter","removed","paused"] {
        let(_tmp,home,domain)=setup();let mut worker=Worker::default();let plan=prepare(&home,&domain,&mut worker).unwrap().unwrap();
        let mut store=Store::open(home.clone()).unwrap();
        match change {
            "anchor"=>{let mut j=checked_journal(&home,&store.config,&domain).unwrap();push_event(&mut j,Some(root_item(&domain)),None);j.requests.insert("receipt".into(),Pending{fingerprint:"retained".into(),metadata:None,source_fingerprint:None,transaction:None,confirmed:None,response:Some(json!({"retained":true})),deletion_path:None,deletion_expected:None,metadata_base_fingerprint:None,namespace:None,version:None});save_journal(&store,&domain.id,&j).unwrap();},
            "profile"=>{store.config.profiles.get_mut(&domain.profile).unwrap().timeout_secs+=1;store.save_config().unwrap();},
            "filter"=>{store.config.filters.exclude.push("secret/".into());store.save_config().unwrap();},
            "removed"=>state::atomic_json(&home.join("provider-domains.json"),&Vec::<Domain>::new()).unwrap(),
            "paused"=>{crate::smart::set_paused(&store,true).unwrap();},_=>unreachable!(),
        }
        let binding=journal_binding(&store.config,&domain).unwrap();drop(store);
        let before=index::load_existing(&home,&domain.id,&binding).unwrap().unwrap();let serialized=serde_json::to_value(&before).unwrap();
        assert!(!publish(&home,&plan,None,None).unwrap(),"{change}");
        assert_eq!(serde_json::to_value(index::load_existing(&home,&domain.id,&binding).unwrap().unwrap()).unwrap(),serialized,"stale read changed {change}");
    }
}

#[tokio::test]
async fn directory_errors_backoff_connections_reuse_and_metadata_sidecars_publish() {
    let(_tmp,home,domain)=setup();let peer=Peer::new(vec![entry("document.txt",false)],None);let connector=connected(peer.clone());let coordinator=Mutex::new(());let mut worker=Worker::default();
    once(&home,&domain,&coordinator,None,&mut worker,&connector).await.unwrap();assert_eq!(connector.calls.load(Ordering::SeqCst),1);
    once(&home,&domain,&coordinator,None,&mut worker,&connector).await.unwrap();
    reset_scan(&home,&domain);once(&home,&domain,&coordinator,None,&mut worker,&connector).await.unwrap();assert_eq!(connector.calls.load(Ordering::SeqCst),1,"healthy backend must be reused");
    let store=Store::open(home.clone()).unwrap();let mut j=checked_journal(&home,&store.config,&domain).unwrap();let id=j.items.keys().next().unwrap().clone();j.items.get_mut(&id).unwrap().metadata_known=true;j.items.get_mut(&id).unwrap().metadata_checked=0;save_journal(&store,&domain.id,&j).unwrap();drop(store);
    worker.prefer_metadata=true;once(&home,&domain,&coordinator,None,&mut worker,&connector).await.unwrap();assert!(journal(&home,&domain).items[&id].metadata_checked>0);
    reset_scan(&home,&domain);peer.fail.store(true,Ordering::SeqCst);once(&home,&domain,&coordinator,None,&mut worker,&connector).await.unwrap();assert!(journal(&home,&domain).directory_attempts["root"].failures>0);assert!(worker.backends.is_empty());
    reset_scan(&home,&domain);let failing=Connected{backend:peer,calls:AtomicUsize::new(0),fail:true};once(&home,&domain,&coordinator,None,&mut worker,&failing).await.unwrap();assert!(journal(&home,&domain).directory_attempts["root"].failures>0);
}

#[tokio::test]
async fn managed_snapshots_are_read_without_store_and_publish_with_current_pin_policy() {
    let(_tmp,home,mut domain)=setup();domain.id="repository".into();domain.repository=true;
    let disk=super::super::test_backend::Disk::new(home.parent().unwrap().join("remote"));let store=Store::open(home.clone()).unwrap();add_domain(&store,domain.clone()).unwrap();repository::init(&store,disk.as_ref(),"").await.unwrap();drop(store);
    let coordinator=Mutex::new(());let mut worker=Worker::default();once(&home,&domain,&coordinator,None,&mut worker,&connected(disk)).await.unwrap();
    assert!(journal(&home,&domain).directory_scans.contains_key("root"));assert!(prepare(&home,&domain,&mut worker).unwrap().is_none());
}

#[tokio::test]
async fn paused_missing_and_busy_domains_do_not_start_metadata_network_work() {
    let(_tmp,home,domain)=setup();let store=Store::open(home.clone()).unwrap();crate::smart::set_paused(&store,true).unwrap();drop(store);
    let mut workers=Workers::default();workers.schedule(&home,Arc::new(Mutex::new(())),Arc::new(admin::Priority::default())).unwrap();tokio::task::yield_now().await;
    let current=workers.domains[&domain.id].clone();let held=current.lock_owned().await;
    workers.schedule(&home,Arc::new(Mutex::new(())),Arc::new(admin::Priority::default())).unwrap();drop(held);
    workers.reads.close();workers.schedule(&home,Arc::new(Mutex::new(())),Arc::new(admin::Priority::default())).unwrap();tokio::task::yield_now().await;
    state::atomic_json(&home.join("provider-domains.json"),&Vec::<Domain>::new()).unwrap();workers.schedule(&home,Arc::new(Mutex::new(())),Arc::new(admin::Priority::default())).unwrap();assert!(workers.domains.is_empty());
    assert!(prepare(&home,&domain,&mut Worker::default()).unwrap().is_none());
}

#[tokio::test]
async fn queued_publications_run_between_complete_serialized_replay_attempts() {
    let(_tmp,home,first)=setup();let mut second=first.clone();second.id="second".into();
    state::atomic_json(&home.join("provider-domains.json"),&vec![first.clone(),second.clone()]).unwrap();
    let coordinator=Arc::new(Mutex::new(()));let priority=Arc::new(admin::Priority::default());
    let entered=Arc::new(Notify::new());let release=Arc::new(Notify::new());let published=Arc::new(AtomicBool::new(false));
    let h=home.clone();let c=coordinator.clone();let p=priority.clone();let e=entered.clone();let r=release.clone();let published_in=published.clone();let first_id=first.id.clone();
    let task=tokio::spawn(async move {coordinated_replays(&h,&p,&c,|domain|{let e=e.clone();let r=r.clone();let published=published_in.clone();let first=domain.id==first_id;async move {if first{e.notify_one();r.notified().await;}else{assert!(published.load(Ordering::SeqCst),"queued publication must run before second remote attempt");}Ok(())}}).await});
    entered.notified().await;
    let mut publication=Box::pin(coordinator.lock());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10),&mut publication).await.is_err(),"active replay retains coordinator until its receipt is settled");
    release.notify_one();let guard=publication.await;published.store(true,Ordering::SeqCst);drop(guard);
    task.await.unwrap().unwrap();
    let staging=private(&home.join("staging")).unwrap();let fd=open_staging(&staging).unwrap();
    replay_once(&home,&staging,&fd,&priority,&coordinator).await.unwrap();
    let store=Store::open(home.clone()).unwrap();crate::smart::set_paused(&store,true).unwrap();drop(store);
    replay_once(&home,&staging,&fd,&priority,&coordinator).await.unwrap();
}

#[tokio::test]
async fn administration_can_remove_a_domain_between_replay_attempts_without_using_its_stale_snapshot() {
    let(_tmp,home,first)=setup();let mut second=first.clone();second.id="second".into();
    state::atomic_json(&home.join("provider-domains.json"),&vec![first.clone(),second]).unwrap();
    let coordinator=Arc::new(Mutex::new(()));let priority=Arc::new(admin::Priority::default());
    let entered=Arc::new(Notify::new());let release=Arc::new(Notify::new());let attempts=Arc::new(AtomicUsize::new(0));
    let h=home.clone();let c=coordinator.clone();let p=priority.clone();let e=entered.clone();let r=release.clone();let count=attempts.clone();
    let task=tokio::spawn(async move {coordinated_replays(&h,&p,&c,|_|{let e=e.clone();let r=r.clone();let count=count.clone();async move {assert_eq!(count.fetch_add(1,Ordering::SeqCst),0,"removed second domain must not be replayed");e.notify_one();r.notified().await;Ok(())}}).await});
    entered.notified().await;let mut administration=Box::pin(coordinator.lock());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10),&mut administration).await.is_err());
    release.notify_one();let guard=administration.await;
    state::atomic_json(&home.join("provider-domains.json"),&vec![first]).unwrap();drop(guard);
    task.await.unwrap().unwrap();assert_eq!(attempts.load(Ordering::SeqCst),1);
}

#[tokio::test]
async fn replay_attempts_keep_failed_pending_intents_and_refresh_the_overlay() {
    let(_tmp,home,domain)=setup();let store=Store::open(home.clone()).unwrap();
    let mut queue=crate::smart::Queue::open(&store,&domain.id,&journal_binding(&store.config,&domain).unwrap()).unwrap();
    let mutation=queue.enqueue(json!({"v":1,"op":"modify","domain":domain.id,"id":"missing","request_id":"retained","base_content_version":"old","base_metadata_version":"old","name":"missing"}),None).unwrap();drop(queue);drop(store);
    let staging=private(&home.join("staging")).unwrap();let fd=open_staging(&staging).unwrap();
    replay_once(&home,&staging,&fd,&admin::Priority::default(),&Mutex::new(())).await.unwrap();
    let store=Store::open(home.clone()).unwrap();let queue=crate::smart::Queue::open(&store,&domain.id,&journal_binding(&store.config,&domain).unwrap()).unwrap();
    assert!(queue.list().iter().any(|item|item.id==mutation.id&&item.state!=crate::smart::MutationState::Done));
}


struct PendingConnection;
#[async_trait]
impl Connector for PendingConnection {
    async fn connect(&self,_:&config::Profile,_:&str,_:crate::transfer::TransferContext)->Result<Arc<dyn Backend>> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn bounded_connect_and_metadata_deadlines_preserve_success_and_record_unknown_reachability() {
    let(_temporary,home,domain)=setup();
    let mut store=Store::open(home.clone()).unwrap();
    store.config.profiles.get_mut(&domain.profile).unwrap().timeout_secs=1;
    store.save_config().unwrap();let profile=store.config.profiles[&domain.profile].clone();drop(store);
    let coordinator=Mutex::new(());
    once(&home,&domain,&coordinator,None,&mut Worker::default(),&connected(Peer::new(vec![],None))).await.unwrap();
    let success=crate::connection_status::snapshot(&home,&domain.profile,&profile,crate::connection_status::now_ms(),300).unwrap()["last_success_at"].clone();
    assert!(success.is_string());
    for connect_phase in [true,false] {
        reset_scan(&home,&domain);
        let mut worker=Worker::default();
        if connect_phase {
            once(&home,&domain,&coordinator,None,&mut worker,&PendingConnection).await.unwrap();
        }else {
            once(&home,&domain,&coordinator,None,&mut worker,&connected(Peer::new(vec![],Some(&domain.remote)))).await.unwrap();
        }
        let observed=crate::connection_status::snapshot(&home,&domain.profile,&profile,crate::connection_status::now_ms(),300).unwrap();
        assert_eq!(observed["state"],"unknown");
        assert_eq!(observed["last_success_at"],success);
        assert_eq!(observed["last_error"]["detail"]["code"],"timeout");
        assert_eq!(journal(&home,&domain).directory_attempts["root"].failures,1);
        assert!(worker.backends.is_empty());
    }
}
