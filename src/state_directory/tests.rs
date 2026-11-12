use super::*;
use std::os::unix::fs::symlink;

#[derive(Default)]
struct Fake {
    installed: Option<ServiceOptions>, running: bool, fail: Option<String>, crash: Option<String>,
    calls: Vec<String>,
}
impl Operations for Fake {
    fn options(&mut self, _: &Path)->Result<Option<ServiceOptions>> { Ok(self.installed.clone()) }
    fn running(&mut self, _: &Path)->Result<bool> { Ok(self.running) }
    fn action(&mut self, _: &Path, action:ServiceAction)->Result<()> { self.calls.push(format!("{action:?}")); self.running=action==ServiceAction::Start; Ok(()) }
    fn install(&mut self, options:&ServiceOptions)->Result<()> { self.calls.push(format!("install:{}",options.home.display())); self.installed=Some(options.clone()); Ok(()) }
    fn checkpoint(&mut self, phase:&str)->Result<()> {
        if self.crash.as_deref()==Some(phase) { panic!("synthetic crash at {phase}"); }
        if self.fail.as_deref()==Some(phase) { self.fail=None; bail!("synthetic failure at {phase}"); }
        Ok(())
    }
    fn wait_ready(&mut self, _: &Path)->Result<()> { self.calls.push("ready".into()); Ok(()) }
}
struct Fixture { _temp:tempfile::TempDir, source:PathBuf, standard:PathBuf, destination:PathBuf }
fn private(path:&Path) { fs::DirBuilder::new().recursive(true).mode(0o700).create(path).unwrap(); fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap(); }
fn bytes(path:&Path,value:&[u8]) { private(path.parent().unwrap()); state::atomic_write(path,value).unwrap(); }
fn fixture()->Fixture {
    let temp=tempfile::tempdir().unwrap(); let root=temp.path().canonicalize().unwrap();
    let source=root.join("original state"); let standard=root.join("Application Support/rdrive"); let destination=standard.join("state");
    private(&source); private(&standard);
    let mut store=crate::state::Store::open(source.clone()).unwrap();
    store.config.profiles.insert("nas".into(),crate::config::Profile {protocol:crate::config::Protocol::Smb,url:"smb://example.invalid/homes".into(),username:Some("synthetic".into()),password_env:None,keychain_internet:true,allow_http:false,timeout_secs:12,smb_encryption:true,nextcloud_upload_url:None});
    store.save_config().unwrap();drop(store);
    bytes(&source.join("provider/index.sqlite3"),b"retained synthetic index bytes");
    bytes(&source.join("cache/object"),b"verified retained cache");
    bytes(&source.join("smart/domain/dirty/snapshot"),b"the only retained offline edit");
    bytes(&source.join("smart/domain/queue.json"),br#"{"mutations":{"stable-id":{"id":"stable-id","state":"pending","snapshot":"snapshot","guard":"remote-version"}}}"#);
    bytes(&source.join("smart/control.json"),br#"{"version":1,"paused":false}"#);
    bytes(&standard.join(LOCATOR),&locator(&standard,&source));
    Fixture{_temp:temp,source,standard,destination}
}
fn apply(f:&Fixture,ops:&mut Fake)->Result<Value> { migrate_with(&f.source,&f.destination,&f.standard,true,ops) }
fn selected(f:&Fixture)->PathBuf { let v:Value=read_json(&f.standard.join(LOCATOR)).unwrap();PathBuf::from(v["home"].as_str().unwrap()) }
fn retained_at(home:&Path) {
    assert_eq!(fs::read(home.join("provider/index.sqlite3")).unwrap(),b"retained synthetic index bytes");
    assert_eq!(fs::read(home.join("cache/object")).unwrap(),b"verified retained cache");
    assert_eq!(fs::read(home.join("smart/domain/dirty/snapshot")).unwrap(),b"the only retained offline edit");
    assert_eq!(fs::read(home.join("smart/domain/queue.json")).unwrap(),br#"{"mutations":{"stable-id":{"id":"stable-id","state":"pending","snapshot":"snapshot","guard":"remote-version"}}}"#);
}

#[test]
fn preview_is_read_only_and_complete_migration_preserves_originals_and_seals_legacy_home() {
    let f=fixture();let mut ops=Fake::default();let before=walk(&f.source).unwrap();
    let plan=migrate_with(&f.source,&f.destination,&f.standard,false,&mut ops).unwrap();assert_eq!(plan["apply"],false);
    assert!(!f.destination.exists());assert!(!f.standard.join(JOURNAL).exists());assert_eq!(walk(&f.source).unwrap(),before);
    assert_eq!(show(&f.source,&f.standard).unwrap()["home"],f.source.to_string_lossy().as_ref());
    let report=apply(&f,&mut ops).unwrap();assert_eq!(report["migrated"],true);assert!(!report["service_running"].as_bool().unwrap());
    assert_eq!(selected(&f),f.destination);retained_at(&f.destination);
    retained_at(Path::new(report["source_backup"].as_str().unwrap()));
    assert!(!f.source.join("config.json").exists());assert_eq!(fs::metadata(&f.source).unwrap().mode()&0o777,0o500);
    assert!(guard(&f.source).unwrap_err().to_string().contains("moved"));
    assert!(crate::state::Store::open(f.source.clone()).is_err());
    assert!(!paused(&f.destination).unwrap());
    assert_eq!(recover_with(&f.standard,&mut ops).unwrap()["committed"],true);
}

#[test]
fn service_home_is_reinstalled_with_original_pause_and_external_ipc() {
    let f=fixture();set_paused(&f.source,true).unwrap();
    let original=ServiceOptions{home:f.source.clone(),executable:f.standard.join("rdrive"),ipc_root:f.standard.join("ipc"),app:None,development_tcp:false,launch_agents_dir:None};
    let mut ops=Fake{installed:Some(original),running:true,..Default::default()};
    let report=apply(&f,&mut ops).unwrap();assert_eq!(report["service_running"],true);assert!(paused(&f.destination).unwrap());
    assert_eq!(ops.installed.unwrap().home,f.destination);
    assert_eq!(ops.calls.iter().filter(|call|call.as_str()=="Stop").count(),1);
    assert!(ops.calls.contains(&"Uninstall".into()));assert!(ops.calls.contains(&"Start".into()));assert!(ops.calls.contains(&"ready".into()));
}

#[test]
fn every_precommit_failure_restores_source_and_every_committed_failure_recovers_forward() {
    for phase in ["planned","stopped","copied","published","backed_up","committed","activated"] {
        let f=fixture();let mut ops=Fake{fail:Some(phase.into()),..Default::default()};
        let error=apply(&f,&mut ops).unwrap_err();assert!(error.to_string().contains(if ["committed","activated"].contains(&phase){"committed safely"}else{"rolled back"}),"{phase}: {error:#}");
        let recovered=recover_with(&f.standard,&mut ops).unwrap();
        if ["committed","activated"].contains(&phase){assert_eq!(selected(&f),f.destination);retained_at(&f.destination);assert!(recovered["migrated"]==true||recovered["committed"]==true);}else{assert_eq!(selected(&f),f.source);retained_at(&f.source);assert!(!paused(&f.source).unwrap());}
    }
}

#[test]
fn durable_journal_recovers_actual_unwound_crashes_at_each_phase() {
    for phase in ["planned","stopped","copied","published","backed_up","committed","activated"] {
        let f=fixture();let mut ops=Fake{crash:Some(phase.into()),..Default::default()};
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||apply(&f,&mut ops))).is_err());ops.crash=None;
        recover_with(&f.standard,&mut ops).unwrap();
        retained_at(&selected(&f));
        assert!(!paused(&selected(&f)).unwrap());
    }
}

#[test]
fn unsafe_targets_locators_profiles_and_writer_locks_fail_without_losing_state() {
    let f=fixture();let mut ops=Fake::default();
    for target in [f.source.clone(),f.source.join("nested"),f.source.parent().unwrap().to_path_buf(),f.standard.clone()] { assert!(migrate_with(&f.source,&target,&f.standard,true,&mut ops).is_err()); }
    private(&f.destination);assert!(apply(&f,&mut ops).is_err());fs::remove_dir(&f.destination).unwrap();
    let guard_lock=Lease::acquire(&f.source.join("client.lock"),Duration::ZERO).unwrap();assert!(apply(&f,&mut ops).is_err());drop(guard_lock);
    assert!(!f.standard.join(JOURNAL).exists());retained_at(&f.source);
    bytes(&f.standard.join(LOCATOR),br#"{"version":1,"home":"/another/state"}"#);assert!(apply(&f,&mut ops).is_err());
    bytes(&f.standard.join(LOCATOR),&locator(&f.standard,&f.source));
    bytes(&f.source.join("config.json"),b"damaged");assert!(apply(&f,&mut ops).is_err());
}

#[test]
fn migration_markers_metadata_permissions_and_links_fail_closed() {
    let f=fixture();assert!(guard(&f.source.join("missing")).is_ok());
    for value in [json!({"version":99,"id":"invalid","source":"relative","destination":"relative","phase":"moved"}),json!({"version":1})] {write_json(&f.source.join(MARKER),&value).unwrap();assert!(guard(&f.source).is_err());}
    bytes(&f.source.join(MARKER),b"{");assert!(guard(&f.source).is_err());fs::remove_file(f.source.join(MARKER)).unwrap();
    let reference=f.source.join("reference");symlink(f.source.join("config.json"),&reference).unwrap();assert!(read_optional(&reference,4096).is_err());assert!(walk(&f.source).is_err());fs::remove_file(&reference).unwrap();
    bytes(&reference,b"oversized");assert!(read_optional(&reference,2).is_err());
    fs::set_permissions(&reference,fs::Permissions::from_mode(0o644)).unwrap();assert!(read_optional(&reference,4096).is_err());
    fs::remove_file(reference).unwrap();fs::set_permissions(&f.source,fs::Permissions::from_mode(0o755)).unwrap();assert!(private_directory(&f.source).is_err());
}

#[test]
fn absent_and_invalid_recovery_journals_are_bounded_and_non_destructive() {
    let f=fixture();let mut ops=Fake::default();assert_eq!(recover_with(&f.standard,&mut ops).unwrap()["pending"],false);
    bytes(&f.standard.join(JOURNAL),b"bad");assert!(recover_with(&f.standard,&mut ops).is_err());retained_at(&f.source);
}

#[test]
fn internal_service_artifacts_are_refused_before_any_service_change() {
    let f=fixture();let mut ops=Fake{installed:Some(ServiceOptions{home:f.source.clone(),executable:f.source.join("rdrive"),ipc_root:f.standard.join("ipc"),app:None,development_tcp:false,launch_agents_dir:None}),..Default::default()};
    assert!(apply(&f,&mut ops).unwrap_err().to_string().contains("outside"));assert!(ops.calls.is_empty());
}

#[test]
fn empty_directories_times_and_selected_snapshot_metadata_survive_clone_and_fallback() {
    let f=fixture();let empty=f.source.join("tmp/empty/private");private(&empty);
    let source=f.source.join("smart/domain/dirty/snapshot");
    let mut tags=crate::metadata::capture(&source,&Default::default()).unwrap();
    tags.attributes.insert("user.rdrive-migration#S".into(),"746167676564".into());
    crate::metadata::apply(&source,&tags,&Default::default()).unwrap();
    fs::set_permissions(&source,fs::Permissions::from_mode(0o400)).unwrap();
    let stamp=std::time::UNIX_EPOCH+Duration::from_secs(1_600_000_001);
    File::open(&source).unwrap().set_times(fs::FileTimes::new().set_modified(stamp)).unwrap();
    let manifest=walk(&f.source).unwrap();let object=&manifest[Path::new("smart/domain/dirty/snapshot")];
    let fallback=f.standard.join("fallback-snapshot");copy_file(&source,&fallback,object,|_,_|Ok(false)).unwrap();
    assert_eq!(crate::metadata::capture(&fallback,&Default::default()).unwrap().fingerprint().unwrap(),tags.fingerprint().unwrap());
    assert_eq!(fs::metadata(&fallback).unwrap().mode()&0o777,0o400);
    assert_eq!(fs::metadata(&fallback).unwrap().modified().unwrap(),stamp);
    let mut ops=Fake::default();apply(&f,&mut ops).unwrap();
    assert!(f.destination.join("tmp/empty/private").is_dir());
    assert_eq!(fs::metadata(f.destination.join("smart/domain/dirty/snapshot")).unwrap().modified().unwrap(),stamp);
    assert_eq!(crate::metadata::capture(&f.destination.join("smart/domain/dirty/snapshot"),&Default::default()).unwrap().fingerprint().unwrap(),tags.fingerprint().unwrap());
    #[cfg(target_os="macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let invalid=Path::new(std::ffi::OsStr::from_bytes(b"bad\0path"));
        assert!(clone_file(invalid,&fallback).is_err());assert!(clone_file(&source,invalid).is_err());
    }
}

fn ready_options(home:&Path)->Result<Option<ServiceOptions>> {
    Ok(Some(ServiceOptions{home:home.into(),executable:std::env::current_exe()?,ipc_root:home.join("ipc"),app:None,development_tcp:false,launch_agents_dir:None}))
}
fn ready_status(_: &Path)->Result<Value> {
    Ok(json!({"launchd":{"loaded":true,"running":true},"supervisor_running":true,"native_watcher":null,"runtime":{"children":{"broker":{"pid":std::process::id()}}}}))
}
fn not_ready_status(_: &Path)->Result<Value> { Ok(json!({"launchd":{"loaded":false,"running":false},"supervisor_running":false})) }
fn service_action_stub(_: &Path,action:ServiceAction,dry:bool)->Result<Value> {assert!(!dry);assert_eq!(action,ServiceAction::Start);Ok(json!({"ok":true}))}
fn service_install_stub(options:&ServiceOptions,dry:bool)->Result<Value> {assert!(!dry);assert!(options.home.is_absolute());Ok(json!({"ok":true}))}
fn socket_fixture(home:&Path)->std::os::unix::net::UnixListener {
    let root=home.join("ipc");private(&root);let socket=root.join("bridge.sock");
    let listener=std::os::unix::net::UnixListener::bind(&socket).unwrap();fs::set_permissions(&socket,fs::Permissions::from_mode(0o600)).unwrap();
    write_json(&home.join("provider-admin.json"),&json!({"version":1,"home":home,"socket":socket,"instance_id":uuid::Uuid::new_v4()})).unwrap();listener
}
#[test]
fn platform_adapter_requires_live_child_identity_home_bound_descriptor_and_actual_domain_rpc() {
    let f=fixture();let listener=socket_fixture(&f.source);
    let peer=std::thread::spawn(move||{let(mut stream,_)=listener.accept().unwrap();use std::io::BufRead;let mut input=String::new();std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut input).unwrap();assert_eq!(serde_json::from_str::<Value>(&input).unwrap()["op"],"domains");stream.write_all(b"{\"ok\":true,\"domains\":[{\"id\":\"nas\"}]}\n").unwrap();});
    let mut system=System{options:ready_options,status:ready_status,action:service_action_stub,install:service_install_stub,timeout:Duration::ZERO,poll:Duration::ZERO};
    assert!(system.options(&f.source).unwrap().is_some());assert!(system.running(&f.source).unwrap());
    system.install(&ready_options(&f.source).unwrap().unwrap()).unwrap();system.action(&f.source,ServiceAction::Start).unwrap();
    system.checkpoint("normal adapter").unwrap();
    DELAYED_STATUS.store(0,std::sync::atomic::Ordering::SeqCst);system.status=delayed_status;system.timeout=Duration::from_secs(1);
    system.wait_ready(&f.source).unwrap();peer.join().unwrap();
    assert!(process_matches(std::process::id()as u64,&std::env::current_exe().unwrap()));
    assert!(!process_matches(0,Path::new("/unknown")));assert!(!process_matches(i32::MAX as u64+1,Path::new("/unknown")));assert!(!process_matches(std::process::id()as u64,Path::new("/unknown")));
    system.status=not_ready_status;system.timeout=Duration::ZERO;assert!(!system.running(&f.source).unwrap());assert!(system.wait_ready(&f.source).is_err());
    let options=ready_options(&f.source).unwrap().unwrap();let status=ready_status(&f.source).unwrap();
    let mut wrong=status.clone();wrong["runtime"]["children"]["broker"]["pid"]=json!(0);assert!(!connection_ready(&f.source,&options,&wrong).unwrap());
    let mut options_with_watcher=options.clone();options_with_watcher.app=Some(f.standard.join("absent.app"));assert!(!connection_ready(&f.source,&options_with_watcher,&status).unwrap());
    write_json(&f.source.join("provider-admin.json"),&json!({"version":2,"home":f.source,"socket":options.ipc_root.join("bridge.sock"),"instance_id":"invalid"})).unwrap();assert!(!connection_ready(&f.source,&options,&status).unwrap());
}

#[test]
fn active_queue_and_invalid_controls_are_never_forced_during_relocation() {
    let f=fixture();let queue=f.source.join("smart/domain/queue.json");
    bytes(&queue,br#"{"mutations":[{"state":"running"}]}"#);assert!(active_work(&f.source).unwrap());
    assert!(drain_with(&f.source,Duration::ZERO,Duration::ZERO).unwrap_err().to_string().contains("no active operation"));
    assert!(apply(&f,&mut Fake::default()).is_err());assert!(!f.standard.join(JOURNAL).exists());
    bytes(&queue,br#"{"mutations":null}"#);assert!(active_work(&f.source).is_err());
    bytes(&f.source.join("smart/control.json"),br#"{"version":2,"paused":false}"#);assert!(set_paused(&f.source,true).is_err());
    bytes(&f.source.join("smart/control.json"),br#"{"version":1,"paused":"bad"}"#);assert!(paused(&f.source).is_err());
    let fresh=f.standard.join("fresh");private(&fresh);assert!(!paused(&fresh).unwrap());assert!(!active_work(&fresh).unwrap());set_paused(&fresh,true).unwrap();assert!(paused(&fresh).unwrap());
}

#[test]
fn public_unmanaged_adapter_is_a_real_read_only_plan_and_missing_recovery_is_safe() {
    let f=fixture();assert_eq!(migrate(&f.source,&f.destination,&f.standard,false).unwrap()["apply"],false);assert_eq!(recover(&f.standard).unwrap()["pending"],false);
}

#[test]
fn partial_stub_preparation_and_atomic_stub_retirement_are_recoverable() {
    for retirement in [false,true] {
        let f=fixture();let mut ops=Fake{crash:Some("backed_up".into()),..Default::default()};
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||apply(&f,&mut ops))).is_err());ops.crash=None;
        let intent:Intent=read_json(&f.standard.join(JOURNAL)).unwrap();
        if retirement {seal_source(&intent).unwrap();remove_stub(&intent).unwrap();}else{create_private(&intent.stage.with_file_name(format!("state-stub-{}",intent.id))).unwrap();}
        recover_with(&f.standard,&mut ops).unwrap();retained_at(&f.source);assert_eq!(selected(&f),f.source);
    }
}

#[test]
fn committed_recovery_does_not_override_a_new_locator_selection() {
    let f=fixture();let mut ops=Fake{crash:Some("committed".into()),..Default::default()};
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||apply(&f,&mut ops))).is_err());ops.crash=None;ops.calls.clear();
    bytes(&f.standard.join(LOCATOR),br#"{"version":1,"home":"/new/selected/state"}"#);
    assert!(recover_with(&f.standard,&mut ops).unwrap_err().to_string().contains("locator changed"));
    assert!(ops.calls.is_empty());retained_at(&f.destination);assert_eq!(selected(&f),PathBuf::from("/new/selected/state"));
}

static DELAYED_STATUS: std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
fn delayed_status(home:&Path)->Result<Value> {
    if DELAYED_STATUS.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0 {not_ready_status(home)}else{ready_status(home)}
}
#[test]
fn stopped_writer_lock_is_waited_for_without_stealing_it() {
    let f=fixture();let path=f.source.join("client.lock");let held=Lease::acquire(&path,Duration::ZERO).unwrap();
    let release=std::thread::spawn(move||{std::thread::sleep(Duration::from_millis(35));drop(held);});
    let acquired=Lease::acquire(&path,Duration::from_secs(1)).unwrap();drop(acquired);release.join().unwrap();
}
#[test]
fn recovery_refuses_to_claim_success_when_original_and_backup_are_both_missing() {
    let f=fixture();let mut ops=Fake{crash:Some("planned".into()),..Default::default()};
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||apply(&f,&mut ops))).is_err());ops.crash=None;
    let rescued=f.standard.join("separately rescued original");fs::rename(&f.source,&rescued).unwrap();
    assert!(recover_with(&f.standard,&mut ops).unwrap_err().to_string().contains("unavailable"));retained_at(&rescued);
}

#[test]
fn recovery_never_follows_a_replaced_backup_symlink_into_other_state() {
    let f=fixture();let mut ops=Fake{crash:Some("backed_up".into()),..Default::default()};
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||apply(&f,&mut ops))).is_err());ops.crash=None;
    let intent:Intent=read_json(&f.standard.join(JOURNAL)).unwrap();let rescue=f.standard.join("independent rescued backup");
    fs::rename(&intent.backup,&rescue).unwrap();symlink(&rescue,&intent.backup).unwrap();
    assert!(recover_with(&f.standard,&mut ops).is_err());retained_at(&rescue);assert!(!f.source.exists());
}
