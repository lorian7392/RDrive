use super::*;
use async_trait::async_trait;
use std::sync::{Mutex,atomic::{AtomicUsize,Ordering}};

#[derive(Default)]
struct ScopedBackend { rows:Mutex<BTreeMap<String,Vec<Entry>>>, lists:Mutex<Vec<String>>, stats:AtomicUsize, fail_metadata:std::sync::atomic::AtomicBool, sidecar:Mutex<Option<(String,Vec<u8>)>> }
impl ScopedBackend {
    fn set(&self,parent:&str,entries:Vec<Entry>){self.rows.lock().unwrap().insert(parent.into(),entries);}
    fn entry(path:&str,directory:bool)->Entry{Entry{path:path.into(),is_dir:directory,size:if directory{0}else{4},modified:Some(1),etag:Some(format!("\"{path}-v1\""))}}
}
#[async_trait]
impl Backend for ScopedBackend {
    fn capabilities(&self)->crate::backend::Capabilities{crate::backend::Capabilities{protocol:"webdav",conditional_write:true,server_copy:false,locking:false,notes:vec![]}}
    async fn stat(&self,path:&str)->Result<Option<Entry>>{
        self.stats.fetch_add(1,Ordering::Relaxed);
        if path.starts_with(".rdrive-metadata/")&&self.fail_metadata.load(Ordering::Relaxed){bail!("synthetic metadata service unavailable");}
        if let Some((name,body))=&*self.sidecar.lock().unwrap(){if name==path{return Ok(Some(Entry{path:path.into(),is_dir:false,size:body.len() as u64,modified:Some(1),etag:Some("\"sidecar-v1\"".into())}));}}
        Ok(self.rows.lock().unwrap().values().flatten().find(|entry|entry.path==path).cloned())
    }
    async fn list(&self,path:&str)->Result<Vec<Entry>>{self.lists.lock().unwrap().push(path.into());self.rows.lock().unwrap().get(path).cloned().context("synthetic unavailable parent")}
    async fn download(&self,remote:&str,local:&Path)->Result<()>{if let Some((name,body))=&*self.sidecar.lock().unwrap(){if name==remote{fs::write(local,body)?;return Ok(());}}bail!("namespace tests must not download contents")}
    async fn upload(&self,_:&Path,_:&str,_:WriteCondition)->Result<Entry>{bail!("namespace tests must not upload")}
    async fn mkdir(&self,_:&str)->Result<()>{bail!("read only test")}
    async fn remove(&self,_:&str,_:bool)->Result<()>{bail!("read only test")}
    async fn rename(&self,_:&str,_:&str,_:bool)->Result<()>{bail!("read only test")}
    async fn copy(&self,_:&str,_:&str,_:bool)->Result<()>{bail!("read only test")}
}
fn setup()->(tempfile::TempDir,Store,Domain,Journal){
    let temp=tempfile::tempdir().unwrap();let mut store=Store::open(temp.path().join("state")).unwrap();
    store.config.profiles.insert("p".into(),config::Profile{protocol:config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:None,allow_http:false,timeout_secs:5,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();
    let domain=Domain{id:"lazy".into(),profile:"p".into(),remote:String::new(),display_name:"Lazy".into(),repository:false,smart:true,local_root:None};add_domain(&store,domain.clone()).unwrap();let journal=checked_journal(&store.home,&store.config,&domain).unwrap();(temp,store,domain,journal)
}
fn id(j:&Journal,path:&str)->String{j.items.values().find(|r|r.path==path).unwrap().item.id.clone()}

#[tokio::test]
async fn parent_namespace_is_published_without_descendant_or_sidecar_requests(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();
    b.set("",vec![ScopedBackend::entry("a",true),ScopedBackend::entry("doc",false)]);
    b.set("a",vec![ScopedBackend::entry("a/secret",false)]);
    refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();
    assert_eq!(*b.lists.lock().unwrap(),vec![String::new()]);assert_eq!(b.stats.load(Ordering::Relaxed),0,"cold namespace must not probe sidecars");
    assert_eq!(j.items.len(),2);assert!(!j.items.values().any(|r|r.path=="a/secret"));
    assert!(j.directory_scans.contains_key("root"));assert!(!j.directory_scans.contains_key(&id(&j,"a")));
}

#[tokio::test]
async fn partial_refresh_preserves_unvisited_subtrees_and_removes_only_conclusively_deleted_children(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();
    b.set("",vec![ScopedBackend::entry("a",true),ScopedBackend::entry("b",true)]);b.set("a",vec![ScopedBackend::entry("a/deep",true)]);b.set("a/deep",vec![ScopedBackend::entry("a/deep/retained",false)]);
    refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();let a=id(&j,"a");refresh_directory(&store,&b,&d,&mut j,&a).await.unwrap();let deep=id(&j,"a/deep");refresh_directory(&store,&b,&d,&mut j,&deep).await.unwrap();let retained=id(&j,"a/deep/retained");
    b.set("",vec![ScopedBackend::entry("a",true),ScopedBackend::entry("b",true),ScopedBackend::entry("new",false)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();assert!(j.items.contains_key(&retained));
    // Failed parent reads are not empty listings and cannot erase a cached tree.
    b.rows.lock().unwrap().remove("a");let before=serde_json::to_value(&j).unwrap();assert!(refresh_directory(&store,&b,&d,&mut j,&a).await.is_err());assert_eq!(serde_json::to_value(&j).unwrap(),before);
    // A complete root listing replacing directory a with a file does invalidate its known descendants.
    b.set("",vec![ScopedBackend::entry("a",false),ScopedBackend::entry("b",true)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();assert!(!j.items.contains_key(&retained));assert!(!j.items.contains_key(&deep));assert_eq!(j.items[&a].item.kind,"file");
}

#[tokio::test]
async fn failed_collision_merge_never_commits_a_success_scan_or_mutates_existing_namespace(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();b.set("",vec![ScopedBackend::entry("keep",false)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();
    j.directory_scans.insert("root".into(),1);let before=serde_json::to_value(&j).unwrap();b.set("",vec![ScopedBackend::entry("Case",false),ScopedBackend::entry("case",false)]);assert!(refresh_directory(&store,&b,&d,&mut j,"root").await.is_err());assert_eq!(serde_json::to_value(&j).unwrap(),before);
    b.set("",vec![ScopedBackend::entry("same",false),ScopedBackend::entry("same",false)]);assert!(refresh_directory(&store,&b,&d,&mut j,"root").await.is_err());assert_eq!(serde_json::to_value(&j).unwrap(),before);
}

#[tokio::test]
async fn cached_catalog_requests_do_not_take_the_store_network_lock_or_expand_working_set(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();b.set("",vec![ScopedBackend::entry("a",true)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();
    let stage=private(&store.home.join("staging")).unwrap();let fd=open_staging(&stage).unwrap();
    let working:Request=serde_json::from_value(json!({"v":1,"op":"list","domain":d.id,"parent_id":"working_set"})).unwrap();
    let answer=process(&store.home,&stage,&fd,working,Default::default()).await.unwrap();assert_eq!(answer["items"].as_array().unwrap().len(),1);assert!(catalog_demands(&store,&d,&j).unwrap().is_empty());
    let child=id(&j,"a");let browse:Request=serde_json::from_value(json!({"v":1,"op":"list","domain":d.id,"parent_id":child})).unwrap();let answer=process(&store.home,&stage,&fd,browse,Default::default()).await.unwrap();assert!(answer["items"].as_array().unwrap().is_empty());assert_eq!(catalog_demands(&store,&d,&j).unwrap()[0].id,child);
    let hint=demand_directory(&store.home,&d.id).join(format!("directory-{}.json",state::key(&child)));
    let original=fs::read(&hint).unwrap();tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let repeat:Request=serde_json::from_value(json!({"v":1,"op":"list","domain":d.id,"parent_id":child})).unwrap();
    process(&store.home,&stage,&fd,repeat,Default::default()).await.unwrap();
    assert_eq!(fs::read(&hint).unwrap(),original,"repeated indexed reads coalesce pending browse hints instead of rewriting them");
    fs::write(demand_directory(&store.home,&d.id).join(".rdrive-interrupted.tmp"),b"{").unwrap();assert_eq!(catalog_demands(&store,&d,&j).unwrap().len(),1,"atomic temporary files cannot block namespace work");
}

#[tokio::test]
async fn failed_scopes_back_off_without_blocking_other_requested_directories(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();b.set("",vec![ScopedBackend::entry("a",true)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();let child=id(&j,"a");
    request_demand(&store.home,&store.config,&d,&j,&child,false).unwrap();begin_scan(&mut j.directory_attempts,"root");finish_scan(&mut j.directory_attempts,"root",false);
    let demands=catalog_demands(&store,&d,&j).unwrap();assert_eq!(next_directory(&store,&d,&j,&demands).unwrap(),Some(child));assert_eq!(j.directory_attempts["root"].failures,1);assert!(j.directory_scans["root"]>0);
}

#[tokio::test]
async fn absent_or_failed_metadata_preserves_cached_intent_but_explicit_empty_clears_it(){
    let (_temp,store,d,mut j)=setup();let b=ScopedBackend::default();b.set("",vec![ScopedBackend::entry("doc",false)]);refresh_directory(&store,&b,&d,&mut j,"root").await.unwrap();let doc=id(&j,"doc");j.items.get_mut(&doc).unwrap().item.tag_data_hex=Some("00".into());
    b.fail_metadata.store(true,Ordering::Relaxed);assert!(enrich_metadata(&store,&b,&d,&mut j,&doc).await.is_err());assert_eq!(j.items[&doc].item.tag_data_hex.as_deref(),Some("00"));
    b.fail_metadata.store(false,Ordering::Relaxed);enrich_metadata(&store,&b,&d,&mut j,&doc).await.unwrap();assert_eq!(j.items[&doc].item.tag_data_hex.as_deref(),Some("00"));assert!(j.items[&doc].metadata_known);
    let entry=j.items[&doc].strong.clone().unwrap();let path=crate::metadata_remote::sidecar_path("doc",&entry).unwrap();
    let record=crate::metadata::MetadataRecord{version:1,content_sha256:"0".repeat(64),attributes:BTreeMap::new(),selection:crate::metadata::MetadataPolicy::default().include,excluded:vec![]};
    let body=serde_json::to_vec(&json!({"version":1,"path":"doc","entry":entry,"record":record})).unwrap();*b.sidecar.lock().unwrap()=Some((path,body));
    enrich_metadata(&store,&b,&d,&mut j,&doc).await.unwrap();assert!(j.items[&doc].item.tag_data_hex.is_none(),"only explicit empty metadata clears the prior label");
}

#[test]
fn unknown_sidecar_allows_guarded_create_but_existing_metadata_keeps_fresh_version_check(){
    let (_temp,_store,d,_j)=setup();let mut item=root_item(&d);item.id="doc".into();item.name="doc".into();item.kind="file".into();item.content_version="current-file".into();item.tag_data_hex=Some("aa".into());item.metadata_version=metadata_version("doc",&item.tag_data_hex,&item.extended_attributes,false).unwrap();
    let request:Request=serde_json::from_value(json!({"v":1,"op":"modify","id":"doc","domain":d.id,"base_content_version":item.content_version,"base_metadata_version":item.metadata_version})).unwrap();
    assert!(check_base(&request,&item).is_ok(),"the initial displayed snapshot must still match");assert!(check_fresh_metadata_base(&request,"doc",false,None).is_ok(),"unknown metadata uses expected=None publication, not an invented empty metadata version");
    let mut record=crate::metadata::MetadataRecord{version:1,content_sha256:"0".repeat(64),attributes:BTreeMap::from([("com.apple.metadata:_kMDItemUserTags".into(),"bb".into())]),selection:crate::metadata::MetadataPolicy::default().include,excluded:vec![]};
    assert!(check_fresh_metadata_base(&request,"doc",false,Some(&record)).is_err(),"an external present sidecar must never be adopted as the cached base");record.attributes.clear();assert!(check_fresh_metadata_base(&request,"doc",false,Some(&record)).is_err(),"an explicitly empty existing record remains a real metadata change");
    record.attributes.insert("com.apple.metadata:_kMDItemUserTags".into(),"aa".into());assert!(check_fresh_metadata_base(&request,"doc",false,Some(&record)).is_ok());
}
