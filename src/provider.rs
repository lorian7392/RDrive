//! Same-user Unix bridge for sandboxed Finder/File Provider extensions. Private
//! credentials remain in the host process; only staged content crosses the socket.
use crate::{backend::{self,Backend,Entry,WriteCondition},config,repository,state::{self,Store}};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::BTreeMap,fs::{self,File},io::{Seek,SeekFrom},path::{Path,PathBuf},sync::Arc};
use tokio::{io::{AsyncBufReadExt,AsyncReadExt,AsyncWriteExt,BufReader},net::{UnixListener,TcpListener}};
mod index;
mod index_scheduler;

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Domain {pub id:String,pub profile:String,pub remote:String,pub display_name:String,#[serde(default)]pub repository:bool,#[serde(default)]pub local_root:Option<PathBuf>,#[serde(default)]pub smart:bool}
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
pub struct Item {pub id:String,pub parent_id:String,pub name:String,pub kind:String,pub size:u64,pub modified:Option<String>,pub content_version:String,pub metadata_version:String,pub pinned:bool,pub capabilities:Vec<String>,#[serde(skip_serializing_if="Option::is_none")]pub tag_data_hex:Option<String>,#[serde(default)]pub extended_attributes:BTreeMap<String,String>,#[serde(default)]pub smart:bool,#[serde(default)]pub sync_state:Option<String>,#[serde(default)]pub sync_message:Option<String>,#[serde(default)]pub has_unsynced_changes:bool,#[serde(default)]pub native_materialized:bool}
#[derive(Debug,Clone,Serialize,Deserialize)]
struct Record {path:String,item:Item,entry:Option<Entry>,#[serde(default)]strong:Option<Entry>,#[serde(default)]client_id:Option<String>,#[serde(default)]metadata_known:bool,#[serde(default)]metadata_checked:i64}
#[derive(Debug,Clone,Serialize,Deserialize)]
struct Pending {fingerprint:String,#[serde(default)]metadata:Option<crate::metadata::MetadataRecord>,#[serde(default)]source_fingerprint:Option<String>,#[serde(default)]transaction:Option<String>,#[serde(default)]confirmed:Option<Entry>,#[serde(default)]response:Option<Value>,#[serde(default)]deletion_path:Option<String>,#[serde(default)]deletion_expected:Option<Entry>,#[serde(default)]metadata_base_fingerprint:Option<String>}
#[derive(Debug,Clone,Serialize,Deserialize)]
struct NativeAttention {version:String,message:String}
#[derive(Debug,Default,Serialize,Deserialize)]
struct NativeCached {binding:String,versions:BTreeMap<String,String>}
#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct ScanAttempt {last:i64,failures:u32,next_try:i64}
#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct Journal {#[serde(default)]binding:Option<String>,items:BTreeMap<String,Record>,anchor:u64,events:Vec<Event>,requests:BTreeMap<String,Pending>,#[serde(default)]pins:std::collections::BTreeSet<String>,#[serde(default)]pins_migrated:bool,#[serde(default)]native_errors:BTreeMap<String,NativeAttention>,#[serde(default)]smart_replays:BTreeMap<String,Request>,#[serde(default)]directory_scans:BTreeMap<String,i64>,#[serde(default)]directory_attempts:BTreeMap<String,ScanAttempt>,#[serde(default)]metadata_attempts:BTreeMap<String,ScanAttempt>,#[serde(default)]directory_turn:u64,#[serde(default)]completed_demands:BTreeMap<String,i64>}
#[derive(Debug,Clone,Serialize,Deserialize)]struct Event {anchor:u64,item:Option<Item>,deleted:Option<String>}
#[derive(Debug,Clone,Deserialize,Serialize)]
pub struct Request {
    pub v:u32,pub op:String,#[serde(default)]pub domain:String,#[serde(default)]pub id:Option<String>,
    #[serde(default)]pub request_id:Option<String>,#[serde(default)]pub parent_id:Option<String>,#[serde(default)]pub name:Option<String>,
    #[serde(default)]pub kind:Option<String>,#[serde(default)]pub content_path:Option<PathBuf>,#[serde(default)]pub tag_data_hex:Option<String>,
    #[serde(default)]pub extended_attributes:Option<BTreeMap<String,String>>,
    #[serde(default)]pub base_content_version:Option<String>,#[serde(default)]pub base_metadata_version:Option<String>,
    #[serde(default)]pub anchor:Option<String>,#[serde(default)]pub client_item_id:Option<String>,#[serde(default)]pub pinned:Option<bool>,
    #[serde(default)]pub recursive:bool,#[serde(default)]pub file_url:Option<String>,
    #[serde(default)]pub smart_origin_id:Option<String>,
    #[serde(default)]pub native_message:Option<String>,
    #[serde(default)]pub native_confirmed_unregistered:bool,
    #[serde(default)]pub native_cached_versions:Option<BTreeMap<String,String>>,
    #[serde(default)]pub native_cached_versions_reset:Option<bool>,
    #[serde(default)]pub native_materialization:bool,
    #[serde(default)]pub native_accounting_id:Option<String>,
    #[serde(default)]pub native_used_bytes:Option<u64>,
    #[serde(default)]pub native_reserved_bytes:Option<u64>,
    #[serde(default)]pub native_pinned_bytes:Option<u64>,
    #[serde(default)]pub native_dirty_bytes:Option<u64>,
    #[serde(default)]pub auth_token:Option<String>,
}
#[cfg(test)]
fn journal_path(store:&Store,domain:&str)->PathBuf{store.home.join("provider").join(format!("{}.json",state::key(domain)))}
fn save_journal(store:&Store,domain:&str,journal:&Journal)->Result<()> {
    let binding=journal.binding.as_deref().context("provider journal has no account binding")?;
    index::save(&store.home,domain,binding,journal)
}
pub fn journal_binding(config:&config::Config,domain:&Domain)->Result<String>{
    let profile=config.profiles.get(&domain.profile).context("not_authenticated: domain profile missing")?;
    let original=serde_json::to_string(&(&domain.id,&domain.profile,&profile.protocol,&profile.url,&profile.username,&domain.remote,domain.repository))?;
    Ok(state::key(&if domain.smart {format!("{original}:smart-v1")}else{original}))
}
fn checked_journal(home:&Path,config:&config::Config,domain:&Domain)->Result<Journal>{
    let path=home.join("provider").join(format!("{}.json",state::key(&domain.id)));
    let expected=journal_binding(config,domain)?;
    let mut journal=index::load(home,&domain.id,&expected,&path)?.context("provider index initialization failed")?;
    match &journal.binding {
        Some(binding)if binding==&expected=>(),
        None if journal.items.is_empty()&&journal.requests.is_empty()&&journal.events.is_empty()&&journal.anchor==0=>journal.binding=Some(expected),
        _=>bail!("version_conflict: provider domain/backend scope changed or journal is unbound; existing state was preserved, use a new domain ID or explicitly repair the journal"),
    }
    if !journal.pins_migrated { journal.pins=journal.items.iter().filter(|(_,r)|r.item.pinned).map(|(id,_)|id.clone()).collect();journal.pins_migrated=true; }
    for record in journal.items.values_mut(){record.metadata_known|=record.item.tag_data_hex.is_some()||!record.item.extended_attributes.is_empty();}
    Ok(journal)
}
pub fn domains(home:&Path)->Result<Vec<Domain>> {
    let path=home.join("provider-domains.json");
    match fs::read(path){Ok(data)=>return Ok(serde_json::from_slice(&data)?),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>(),Err(e)=>return Err(e.into())};
    let config:config::Config=state::read_json(&home.join("config.json"))?;
    Ok(config.profiles.keys().map(|name|Domain{id:name.clone(),profile:name.clone(),remote:String::new(),display_name:name.clone(),repository:false,smart:false,local_root:None}).collect())
}
pub fn add_domain(store:&Store,domain:Domain)->Result<()> {
    if domain.id.is_empty()||domain.id.len()>120||domain.id.chars().any(|c|c.is_control())||domain.display_name.is_empty(){bail!("invalid provider domain identity");}
    if !store.config.profiles.contains_key(&domain.profile)||backend::normalize(&domain.remote)?!=domain.remote{bail!("unknown domain profile or unsafe root");}
    if let Some(root)=&domain.local_root{if !root.is_absolute()||fs::symlink_metadata(root)?.file_type().is_symlink(){bail!("Finder root must be an absolute real directory");}}
    let mut all=domains(&store.home)?;if all.iter().any(|d|d.id==domain.id){bail!("provider domain already exists");}all.push(domain);state::atomic_json(&store.home.join("provider-domains.json"),&all)
}
pub fn remove_domain(store:&Store,id:&str)->Result<()> {let mut all=domains(&store.home)?;let n=all.len();all.retain(|d|d.id!=id);if all.len()==n{bail!("domain does not exist");}state::atomic_json(&store.home.join("provider-domains.json"),&all)}
fn root_item(d:&Domain)->Item{Item{id:"root".into(),parent_id:"root".into(),name:d.display_name.clone(),kind:"directory".into(),size:0,modified:None,content_version:"root".into(),metadata_version:"root".into(),pinned:false,capabilities:vec!["read".into(),"add".into()],tag_data_hex:None,extended_attributes:BTreeMap::new(),smart:d.smart,sync_state:Some("online".into()),sync_message:None,has_unsynced_changes:false,native_materialized:false}}
fn relative(root:&str,path:&str)->Result<String>{if root.is_empty(){backend::normalize(path)}else{path.strip_prefix(&format!("{root}/")).map(str::to_owned).context("backend returned item outside provider root")}}
fn remote(d:&Domain,path:&str)->Result<String>{backend::join(&d.remote,path)}
fn version(e:&Entry)->Result<String>{Ok(state::key(&serde_json::to_string(&(e.is_dir,e.size,&e.etag,if e.etag.is_none(){e.modified}else{None}))?))}
fn keep_internal(path:&str)->bool{!path.split('/').any(|p|p.starts_with(".rdrive-")||p==".rdrive-repository")}
fn metadata_version(path:&str,tags:&Option<String>,attributes:&BTreeMap<String,String>,pinned:bool)->Result<String>{Ok(state::key(&serde_json::to_string(&(path,tags,attributes,pinned))?))}
fn extended(record:&crate::metadata::MetadataRecord)->BTreeMap<String,String>{record.attributes.iter().filter(|(name,_)|name.as_str()!="com.apple.metadata:_kMDItemUserTags").map(|(k,v)|(k.clone(),v.clone())).collect()}
fn apply_xattrs(mut record:crate::metadata::MetadataRecord,attributes:Option<&BTreeMap<String,String>>)->Result<crate::metadata::MetadataRecord>{
    if let Some(values)=attributes{let policy=crate::metadata::MetadataPolicy::default();record.attributes.retain(|name,_|!name.ends_with("#S")||!policy.allows(name));for(name,value)in values{if !policy.allows(name)||!name.ends_with("#S")||name=="com.apple.metadata:_kMDItemUserTags"{bail!("unsupported extended attribute; Finder tags use tag_data_hex");}record.attributes.insert(name.clone(),value.clone());}}
    record.validate()?;Ok(record)
}
fn push_event(j:&mut Journal,item:Option<Item>,deleted:Option<String>){j.anchor+=1;j.events.push(Event{anchor:j.anchor,item,deleted});if j.events.len()>10000{j.events.drain(..j.events.len()-10000);}}
#[derive(Debug,Clone,Serialize,Deserialize)]
struct CatalogDemand {binding:String,id:String,path:String,metadata:bool,requested:i64,#[serde(default)]recursive:bool,#[serde(default)]force:bool}
fn demand_directory(home:&Path,domain:&str)->PathBuf{home.join("provider").join(format!("demands-{}",state::key(domain)))}
#[cfg(test)]
fn request_demand(home:&Path,config:&config::Config,d:&Domain,j:&Journal,id:&str,metadata:bool)->Result<()> {
    let path=if id=="root"&&!metadata{String::new()}else{let record=j.items.get(id).context("no_such_item: requested catalog item missing")?;
        if metadata==(record.item.kind=="directory"){bail!("unsupported catalog demand kind");}record.path.clone()};
    let now=chrono::Utc::now().timestamp_millis();
    if metadata&&j.items.get(id).is_some_and(|r|r.metadata_known&&now.saturating_sub(r.metadata_checked)<30_000){return Ok(());}
    if !metadata&&j.directory_scans.get(id).is_some_and(|last|now.saturating_sub(*last)<3_000){return Ok(());}
    let directory=private(&demand_directory(home,&d.id))?;
    let kind=if metadata{"item"}else{"directory"};
    state::atomic_json(&directory.join(format!("{kind}-{}.json",state::key(id))),&CatalogDemand{binding:journal_binding(config,d)?,id:id.into(),path,metadata,requested:now,recursive:false,force:false})
}
/// SQLite's directory and item indexes keep Finder reads independent of the
/// number of entries elsewhere in this account and of the network worker lock.
fn request_index_demand(home:&Path,config:&config::Config,d:&Domain,id:&str,metadata:bool)->Result<()> {
    let binding=journal_binding(config,d)?;
    let now=chrono::Utc::now().timestamp_millis();
    let path=if id=="root"&&!metadata {String::new()} else {
        let record=index::lookup(home,&d.id,&binding,id)?.context("no_such_item: requested catalog item missing")?;
        if metadata==(record.item.kind=="directory") {bail!("unsupported catalog demand kind");}
        if metadata&&record.metadata_known&&now.saturating_sub(record.metadata_checked)<30_000 {return Ok(());}
        record.path
    };
    if !metadata&&index::directory_scan(home,&d.id,&binding,id)?.is_some_and(|last|now.saturating_sub(last)<3_000) {return Ok(());}
    let directory=private(&demand_directory(home,&d.id))?;
    let kind=if metadata {"item"}else{"directory"};
    let file=directory.join(format!("{kind}-{}.json",state::key(id)));
    // Coalesce repeated reads while a scope is offline or awaiting refresh.
    // Index queries should not fsync an identical browse hint on every open.
    if let Ok(meta)=fs::symlink_metadata(&file) {
        if meta.is_file()&&!meta.file_type().is_symlink()&&meta.len()<=16*1024 {
            if let Ok(previous)=state::read_json::<Option<CatalogDemand>>(&file) {
                if previous.is_some_and(|p|p.binding==binding&&p.id==id&&p.path==path&&p.metadata==metadata&&now>=p.requested&&now-p.requested<3_000) {return Ok(());}
            }
        }
    }
    state::atomic_json(&file,&CatalogDemand{binding,id:id.into(),path,metadata,requested:now,recursive:false,force:false})
}
fn catalog_demands(store:&Store,d:&Domain,j:&Journal)->Result<Vec<CatalogDemand>> {
    let directory=demand_directory(&store.home,&d.id);if !directory.exists(){return Ok(vec![]);}
    let directory=private(&directory)?;let binding=journal_binding(&store.config,d)?;let mut demands=vec![];
    for entry in fs::read_dir(directory)? {
        let entry=entry?;let name=entry.file_name();let Some(name)=name.to_str() else{continue;};
        let hash=name.strip_prefix("directory-").or_else(||name.strip_prefix("item-")).and_then(|value|value.strip_suffix(".json"));
        if !hash.is_some_and(|value|value.len()==64&&value.bytes().all(|byte|byte.is_ascii_hexdigit())){continue;}
        let file=fs::symlink_metadata(entry.path())?;if !file.is_file()||file.file_type().is_symlink()||file.len()>16*1024{bail!("invalid private catalog demand");}
        let demand:CatalogDemand=serde_json::from_slice(&fs::read(entry.path())?)?;
        if demand.binding!=binding{continue;}
        if demand.id=="root"&&!demand.metadata&&demand.path.is_empty(){demands.push(demand);}
        else if j.items.get(&demand.id).is_some_and(|r|r.path==demand.path&&demand.metadata!=(r.item.kind=="directory")){demands.push(demand);}
    }Ok(demands)
}
fn next_directory(store:&Store,_d:&Domain,j:&Journal,demands:&[CatalogDemand])->Result<Option<String>> {
    Ok(index_scheduler::next_directory(j,demands,&store.config.index,chrono::Utc::now().timestamp_millis()))
}
fn scope_parent(j:&Journal,id:&str)->Result<String>{if id=="root"{return Ok(String::new());}let record=j.items.get(id).context("no_such_item: directory is no longer known")?;if record.item.kind!="directory"{bail!("no_such_item: catalog scope is not a directory");}Ok(record.path.clone())}
fn emit_namespace(store:&Store,d:&Domain,j:&mut Journal,old:&BTreeMap<String,Record>,mut next:BTreeMap<String,Record>)->Result<()> {
    crate::sync::validate_path_collisions(next.values().map(|r|&r.path))?;
    apply_policies(store,d,j,&mut next)?;
    for (id,record) in &next{if old.get(id).is_none_or(|prior|prior.item!=record.item){push_event(j,Some(record.item.clone()),None);}}
    for id in old.keys(){if !next.contains_key(id){push_event(j,None,Some(id.clone()));j.directory_scans.remove(id);}}
    j.items=next;save_journal(store,&d.id,j)
}
/// A successful Depth:1 listing is evidence only about this parent's direct
/// children. Unvisited sibling directories are never interpreted as deletion.
async fn refresh_directory(store:&Store,b:&dyn Backend,d:&Domain,j:&mut Journal,parent_id:&str)->Result<()> {
    let parent=scope_parent(j,parent_id)?;let backend_parent=remote(d,&parent)?;
    let incoming=b.list_metadata(&backend_parent).await?;
    let old=j.items.clone();let mut next=old.clone();let mut entries=BTreeMap::new();
    for entry in incoming {
        if backend::normalize(&entry.path)?!=entry.path||entry.path.rsplit_once('/').map(|(p,_)|p).unwrap_or("")!=backend_parent{bail!("unsafe provider child path");}
        let path=relative(&d.remote,&entry.path)?;if !keep_internal(&path){continue;}
        if entries.insert(path,entry).is_some(){bail!("duplicate provider child path");}
    }
    let direct:Vec<_>=old.iter().filter(|(_,r)|r.item.parent_id==parent_id).map(|(id,r)|(id.clone(),r.clone())).collect();
    for (id,record) in &direct {
        let replacement=entries.get(&record.path);
        let gone=replacement.is_none();let directory_replaced=record.item.kind=="directory"&&replacement.is_some_and(|e|!e.is_dir);
        if gone{next.remove(id);}
        if gone||directory_replaced{let prefix=format!("{}/",record.path);next.retain(|_,child|!child.path.starts_with(&prefix));}
    }
    for (path,entry) in entries {
        let previous=direct.iter().find(|(_,r)|r.path==path).map(|(_,r)|r);
        let id=previous.map(|r|r.item.id.clone()).unwrap_or_else(||uuid::Uuid::new_v4().to_string());
        let same=previous.is_some_and(|r|r.entry.as_ref().is_some_and(|old|old.same_revision(&entry)));
        let strong=if entry.etag.as_ref().is_some_and(|etag|!etag.starts_with("W/")){Some(entry.clone())}else{previous.filter(|_|same).and_then(|r|r.strong.clone())};
        let tags=previous.and_then(|r|r.item.tag_data_hex.clone());
        let attributes=previous.map(|r|r.item.extended_attributes.clone()).unwrap_or_default();
        let mut capabilities=vec!["read".into()];if entry.is_dir{capabilities.push("add".into());}else if b.capabilities().conditional_write{capabilities.push("write".into());}if b.capabilities().protocol=="webdav"&&!entry.is_dir{capabilities.push("delete".into());}
        let item=Item{id:id.clone(),parent_id:parent_id.into(),name:path.rsplit('/').next().unwrap().into(),kind:if entry.is_dir{"directory"}else{"file"}.into(),size:entry.size,modified:entry.modified.and_then(chrono::DateTime::from_timestamp_millis).map(|value|value.to_rfc3339()),content_version:version(&entry)?,metadata_version:metadata_version(&path,&tags,&attributes,false)?,pinned:false,capabilities,tag_data_hex:tags,extended_attributes:attributes,smart:d.smart,sync_state:None,sync_message:None,has_unsynced_changes:false,native_materialized:false};
        next.insert(id,Record{path,item,entry:Some(entry),strong,client_id:previous.and_then(|r|r.client_id.clone()),metadata_known:previous.is_some_and(|r|r.metadata_known),metadata_checked:previous.filter(|_|same).map_or(0,|r|r.metadata_checked)});
    }
    let mut candidate=j.clone();candidate.directory_scans.insert(parent_id.into(),chrono::Utc::now().timestamp_millis());
    emit_namespace(store,d,&mut candidate,&old,next)?;*j=candidate;Ok(())
}
fn check_fresh_metadata_base(req:&Request,path:&str,pinned:bool,prior:Option<&crate::metadata::MetadataRecord>)->Result<()> {
    // No sidecar means there is no remote metadata to overwrite. Publication
    // still uses expected=None/CreateNew. A present record always requires the
    // caller's metadata version to match the freshly observed record.
    if let Some(prior)=prior {
        let tags=prior.attributes.get("com.apple.metadata:_kMDItemUserTags").cloned();let attributes=extended(prior);
        let observed=metadata_version(path,&tags,&attributes,pinned)?;
        if req.base_metadata_version.as_ref().is_some_and(|version|metadata_base(version)!=metadata_base(&observed)){bail!("version_conflict: Finder tags changed after enumeration");}
    }Ok(())
}
fn project_metadata(record:&mut Record,metadata:Option<&crate::metadata::MetadataRecord>){
    // None is unknown/no explicit deletion intent. Only a version-bound record,
    // including an explicitly empty record, replaces cached display/local intent.
    if let Some(metadata)=metadata{record.item.tag_data_hex=metadata.attributes.get("com.apple.metadata:_kMDItemUserTags").cloned();record.item.extended_attributes=extended(metadata);}
    record.metadata_known=true;record.metadata_checked=chrono::Utc::now().timestamp_millis();
}
async fn enrich_metadata(store:&Store,b:&dyn Backend,d:&Domain,j:&mut Journal,id:&str)->Result<()> {
    let Some(previous)=j.items.get(id).cloned() else{return Ok(());};if previous.item.kind!="file"{return Ok(());}
    let Some(strong)=&previous.strong else{return Ok(());};
    let metadata=crate::metadata_remote::load(b,&remote(d,&previous.path)?,strong).await?;
    let observed=b.stat_metadata(&remote(d,&previous.path)?).await?.context("version_conflict: metadata source disappeared")?;
    if !previous.entry.as_ref().is_some_and(|entry|entry.same_revision(&observed)){bail!("version_conflict: metadata source changed after namespace listing");}
    let mut next=j.items.clone();let record=next.get_mut(id).unwrap();
    project_metadata(record,metadata.as_ref());
    let old=j.items.clone();emit_namespace(store,d,j,&old,next)
}
fn next_metadata(j:&Journal,demands:&[CatalogDemand])->Option<String>{
    let now=chrono::Utc::now().timestamp_millis();
    j.items.iter().filter(|(id,record)|record.item.kind=="file"&&record.strong.is_some()&&j.metadata_attempts.get(*id).is_none_or(|attempt|attempt.next_try<=now)).filter_map(|(id,record)|{
        let request=demands.iter().find(|d|d.metadata&&d.id==*id).map(|d|d.requested).unwrap_or(0);
        let priority=if request>record.metadata_checked{0}else if record.metadata_known&&now.saturating_sub(record.metadata_checked)>=30_000{1}else{return None};
        Some(((priority,j.metadata_attempts.get(id).map_or(0,|attempt|attempt.last),record.metadata_checked),id.clone()))
    }).min_by_key(|(priority,_)|*priority).map(|(_,id)|id)
}
fn request_parent(j:&Journal,d:&Domain,req:&Request)->Result<String>{
    if req.op=="create"{return Ok(req.parent_id.clone().unwrap_or_else(||"root".into()));}
    if let Some(record)=req.id.as_ref().and_then(|id|j.items.get(id)){return Ok(record.item.parent_id.clone());}
    if req.op=="list"{return Ok(req.parent_id.clone().filter(|p|p!="working_set").unwrap_or_else(||"root".into()));}
    if req.op=="delete"{if let Some(key)=req.request_id.as_ref(){let key=format!("{}:{key}",state::key(&d.id));if let Some(path)=j.requests.get(&key).and_then(|p|p.deletion_path.as_ref()){
        let parent=path.rsplit_once('/').map(|(parent,_)|parent).unwrap_or("");if parent.is_empty(){return Ok("root".into());}if let Some(record)=j.items.values().find(|r|r.path==parent&&r.item.kind=="directory"){return Ok(record.item.id.clone());}
    }}}
    Ok("root".into())
}
async fn refresh_request(store:&Store,b:&dyn Backend,d:&Domain,j:&mut Journal,req:&Request,post_target:Option<&str>)->Result<Option<repository::Snapshot>> {
    if !d.smart||d.repository{return refresh(store,b,d,j).await;}
    let parent=request_parent(j,d,req)?;refresh_directory(store,b,d,j,&parent).await?;
    let id=if let Some(path)=post_target{j.items.iter().find(|(_,r)|r.path==path).map(|(id,_)|id.clone())}else{req.id.clone()};
    if let Some(id)=id{enrich_metadata(store,b,d,j,&id).await?;}Ok(None)
}

async fn provider_scan(b:&dyn Backend,root:&str)->Result<Vec<Entry>>{
    let mut pending=vec![root.to_owned()];let mut all=vec![];
    while let Some(parent)=pending.pop(){for item in b.list_metadata(&parent).await?{
        if backend::normalize(&item.path)?!=item.path||item.path.rsplit_once('/').map(|p|p.0).unwrap_or("")!=parent{bail!("unsafe provider child path");}
        let path=relative(root,&item.path)?;if !keep_internal(&path){continue;}if item.is_dir{pending.push(item.path.clone());}all.push(item);
    }}Ok(all)
}
async fn refresh(store:&Store,b:&dyn Backend,d:&Domain,j:&mut Journal)->Result<Option<repository::Snapshot>> {
    let old=j.items.clone();let mut next=BTreeMap::new();let mut pinned_snapshot=None;
    if d.repository {
        let snapshot=repository::read(store,b,&d.remote).await?;
        for (path,entry) in &snapshot.commit.entries {
            let parent=path.rsplit_once('/').map(|p|p.0).unwrap_or("");
            let parent_id=if parent.is_empty(){"root".into()}else{snapshot.commit.entries.get(parent).context("missing managed parent")?.id.clone()};
            let tags=entry.metadata.as_ref().and_then(|v|v.get("attributes")).and_then(|a|a.get("com.apple.metadata:_kMDItemUserTags")).and_then(|v|v.as_str()).map(str::to_owned);
            let attributes: BTreeMap<String,String>=entry.metadata.as_ref().and_then(|v|v.get("attributes")).and_then(|v|v.as_object()).map(|attrs|attrs.iter().filter(|(name,_)|name.as_str()!="com.apple.metadata:_kMDItemUserTags").filter_map(|(k,v)|v.as_str().map(|v|(k.clone(),v.to_owned()))).collect()).unwrap_or_default();
            let pinned=old.get(&entry.id).is_some_and(|r|r.item.pinned);
            let mv=metadata_version(path,&tags,&attributes,pinned)?;
            let item=Item{id:entry.id.clone(),parent_id,name:path.rsplit('/').next().unwrap().into(),kind:entry.kind.clone(),size:entry.size,modified:Some(entry.modified.clone()),content_version:entry.blob.clone().unwrap_or_else(||"directory".into()),metadata_version:mv,pinned,capabilities:vec!["read","write","add","rename","reparent","delete"].into_iter().map(str::to_owned).collect(),tag_data_hex:tags,extended_attributes:attributes,smart:d.smart,sync_state:None,sync_message:None,has_unsynced_changes:false,native_materialized:false};
            next.insert(entry.id.clone(),Record{path:path.clone(),item,entry:None,strong:None,client_id:old.get(&entry.id).and_then(|r|r.client_id.clone()),metadata_known:entry.metadata.is_some(),metadata_checked:0});
        }
        pinned_snapshot=Some(snapshot);
    }else {
        let mut entries=provider_scan(b,&d.remote).await?;entries.retain(|e|relative(&d.remote,&e.path).is_ok_and(|p|keep_internal(&p)));
        let mut paths=BTreeMap::<String,String>::new();
        for e in &entries {let path=relative(&d.remote,&e.path)?;let existing=old.iter().find(|(_,r)|r.path==path).map(|(id,_)|id.clone());paths.insert(path,existing.unwrap_or_else(||uuid::Uuid::new_v4().to_string()));}
        crate::sync::validate_path_collisions(paths.keys())?;
        for e in entries {
            let path=relative(&d.remote,&e.path)?;let id=paths.get(&path).unwrap().clone();let parent=path.rsplit_once('/').map(|p|p.0).unwrap_or("");let parent_id=if parent.is_empty(){"root".into()}else{paths.get(parent).context("missing parent in provider listing")?.clone()};
            let strong=if e.etag.as_ref().is_some_and(|tag|!tag.starts_with("W/")){Some(e.clone())}else{old.get(&id).filter(|previous|previous.entry.as_ref().is_some_and(|prior|prior.same_revision(&e))).and_then(|previous|previous.strong.clone())};
            let mut tags=old.get(&id).and_then(|r|r.item.tag_data_hex.clone());
            let mut attributes=old.get(&id).map(|r|r.item.extended_attributes.clone()).unwrap_or_default();
            if !e.is_dir{if let Some(verified)=&strong{if let Some(meta)=crate::metadata_remote::load(b,&e.path,verified).await?{tags=meta.attributes.get("com.apple.metadata:_kMDItemUserTags").cloned();attributes=extended(&meta);}}}
            let mut capabilities=vec!["read".into()];if e.is_dir{capabilities.push("add".into());}else if b.capabilities().conditional_write{capabilities.push("write".into());}if b.capabilities().protocol=="webdav"&&!e.is_dir{capabilities.push("delete".into());}
            let item=Item{id:id.clone(),parent_id,name:path.rsplit('/').next().unwrap().into(),kind:if e.is_dir{"directory"}else{"file"}.into(),size:e.size,modified:e.modified.and_then(chrono::DateTime::from_timestamp_millis).map(|t|t.to_rfc3339()),content_version:version(&e)?,metadata_version:metadata_version(&path,&tags,&attributes,old.get(&id).is_some_and(|r|r.item.pinned))?,pinned:old.get(&id).is_some_and(|r|r.item.pinned),capabilities,tag_data_hex:tags,extended_attributes:attributes,smart:d.smart,sync_state:None,sync_message:None,has_unsynced_changes:false,native_materialized:false};
            next.insert(id.clone(),Record{path,item,entry:Some(e),strong,client_id:old.get(&id).and_then(|r|r.client_id.clone()),metadata_known:old.get(&id).is_some_and(|r|r.metadata_known),metadata_checked:old.get(&id).map_or(0,|r|r.metadata_checked)});
        }
    }
    apply_policies(store,d,j,&mut next)?;
    for (id,record) in &next {if old.get(id).is_none_or(|r|r.item!=record.item){push_event(j,Some(record.item.clone()),None);}}
    for id in old.keys(){if !next.contains_key(id){push_event(j,None,Some(id.clone()));}}
    j.items=next;save_journal(store,&d.id,j)?;Ok(pinned_snapshot)
}
fn staging_file(staging:&Path,directory:&File,source:&Path)->Result<File>{
    use std::{ffi::CString,os::fd::{AsRawFd,FromRawFd}};
    if source.parent().context("staging source needs parent")?.canonicalize()?!=staging{bail!("content must be a direct child of the private staging directory");}
    let name=source.file_name().and_then(|n|n.to_str()).context("invalid staging filename")?;
    if name.is_empty()||name=="."||name==".."{bail!("invalid staging filename");}
    let name=CString::new(name)?;let fd=unsafe{libc::openat(directory.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC)};
    if fd<0{return Err(std::io::Error::last_os_error().into());}let file=unsafe{File::from_raw_fd(fd)};
    if !file.metadata()?.is_file(){bail!("staging source must be a regular file");}Ok(file)
}
fn staging_output(staging:&Path,directory:&File,source:&Path)->Result<PathBuf>{
    use std::os::unix::fs::OpenOptionsExt;
    let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(source)?;
    staging_output_file(staging,directory,&file)
}
fn staging_output_file(staging:&Path,directory:&File,source:&File)->Result<PathBuf>{
    use std::{ffi::CString,os::fd::{AsRawFd,FromRawFd},os::unix::fs::MetadataExt};
    let name=uuid::Uuid::new_v4().to_string();let c=CString::new(name.clone())?;
    let fd=unsafe{libc::openat(directory.as_raw_fd(),c.as_ptr(),libc::O_RDWR|libc::O_CREAT|libc::O_EXCL|libc::O_NOFOLLOW|libc::O_CLOEXEC,0o600)};
    if fd<0{return Err(std::io::Error::last_os_error().into());}let mut output=unsafe{File::from_raw_fd(fd)};
    use std::{io::Write,os::unix::fs::FileExt};
    if !source.metadata()?.is_file(){bail!("staging source must be a regular descriptor");}
    let attributes=crate::metadata::capture_file(source,&Default::default())?;
    let mut buffer=[0u8;128*1024];let mut offset=0;
    loop{let count=source.read_at(&mut buffer,offset)?;if count==0{break;}output.write_all(&buffer[..count])?;offset+=count as u64;}
    let actual=crate::metadata::capture_file(&output,&Default::default())?;
    if actual.content_sha256!=attributes.content_sha256{bail!("staging source changed during descriptor copy");}
    crate::metadata::apply_file(&output,&attributes,&Default::default())?;
    output.sync_all()?;directory.sync_all()?;
    let actual=fs::symlink_metadata(staging)?;let pinned=directory.metadata()?;
    if !actual.is_dir()||actual.dev()!=pinned.dev()||actual.ino()!=pinned.ino(){bail!("staging directory changed; no external file was opened");}
    Ok(staging.join(name))
}
fn open_staging(staging:&Path)->Result<File>{use std::os::unix::fs::OpenOptionsExt;Ok(fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(staging)?)}

fn check_fetch_base(req:&Request,item:&Item)->Result<()> {if req.base_content_version.as_ref().is_some_and(|version|version!=&item.content_version){bail!("version_conflict: file content changed since the system snapshot");}Ok(())}
fn check_base(req:&Request,item:&Item)->Result<()> {if req.base_content_version.as_ref().is_some_and(|v|v!=&item.content_version)||req.base_metadata_version.as_ref().is_some_and(|v|metadata_base(v)!=metadata_base(&item.metadata_version)){bail!("version_conflict: item changed since the system snapshot");}Ok(())}
fn child_path(j:&Journal,parent:&str,name:&str)->Result<String>{if name.is_empty()||name.contains('/')||backend::normalize(name)?!=name||name.chars().any(|c|c.is_control()){bail!("invalid item name");}let p=if parent=="root"{""}else{let r=j.items.get(parent).context("no_such_item: parent missing")?;if r.item.kind!="directory"{bail!("parent is not a directory");}&r.path};backend::join(p,name)}
fn apply_tags(mut record:crate::metadata::MetadataRecord,tags:Option<&str>)->Result<crate::metadata::MetadataRecord>{if let Some(tags)=tags{if tags.len()>128*1024||tags.len()%2!=0||!tags.bytes().all(|b|b.is_ascii_hexdigit()){bail!("invalid Finder tag data");}if tags.is_empty(){record.attributes.remove("com.apple.metadata:_kMDItemUserTags");}else{record.attributes.insert("com.apple.metadata:_kMDItemUserTags".into(),tags.into());}}record.validate()?;Ok(record)}
fn native_cached_path(home:&Path,id:&str)->PathBuf{home.join("provider").join(format!("native-cached-{}.json",state::key(id)))}
fn read_native_cached(home:&Path,config:&config::Config,d:&Domain)->Result<BTreeMap<String,String>>{
    let cached:NativeCached=state::read_json(&native_cached_path(home,&d.id))?;
    if cached.binding==journal_binding(config,d)?{Ok(cached.versions)}else{Ok(BTreeMap::new())}
}
fn metadata_base(value:&str)->&str {value.split(':').next().unwrap_or(value)}
fn explicit_pin(j:&Journal,id:&str,records:&BTreeMap<String,Record>)->bool {
    let mut current=id;let mut seen=std::collections::BTreeSet::new();
    loop {if j.pins.contains(current){return true;}if current=="root"||!seen.insert(current){return false;}current=match records.get(current){Some(record)=>&record.item.parent_id,None=>return false};}
}
fn apply_policies(store:&Store,d:&Domain,j:&Journal,records:&mut BTreeMap<String,Record>)->Result<()> {
    let pins:BTreeMap<_,_>=records.keys().map(|id|(id.clone(),explicit_pin(j,id,records))).collect();
    let mutations=if d.smart {crate::smart::read_only(&store.home,&d.id,&journal_binding(&store.config,d)?)?}else{vec![]};
    let native=read_native_cached(&store.home,&store.config,d)?;
    for (id,record) in records {
        record.item.pinned=pins.get(id).copied().unwrap_or(false);record.item.smart=d.smart;record.item.native_materialized=native.get(id)==Some(&record.item.content_version);
        let pending=mutations.iter().rev().find(|m|m.state.active()&&(m.request["id"].as_str()==Some(id)||j.smart_replays.get(&m.id).and_then(|r|r.id.as_ref())==Some(id)));
        record.item.has_unsynced_changes=pending.is_some();
        let (state,message)=if let Some(m)=pending {use crate::smart::MutationState::*;let state=match m.state {Conflict=>"conflict",Failed=>"error",Running=>"syncing",_=>"queued"};(state,m.error.clone())}
            else if let Some(error)=j.native_errors.get(id).filter(|e|e.version==record.item.content_version){("error",Some(error.message.clone()))}else if record.item.pinned {("pinned",None)}else if native.get(id)==Some(&record.item.content_version){("cached",None)}else {("online",None)};
        record.item.sync_state=Some(state.into());record.item.sync_message=message;
        let base=metadata_version(&record.path,&record.item.tag_data_hex,&record.item.extended_attributes,record.item.pinned)?;
        record.item.metadata_version=format!("{base}:{}",&state::key(&serde_json::to_string(&(&record.item.sync_state,&record.item.sync_message,record.item.has_unsynced_changes,record.item.native_materialized))?)[..32]);
    }Ok(())
}
fn catalog(d:&Domain,j:&Journal,req:&Request)->Result<Value> {
    let id=req.id.as_deref().unwrap_or("root");
    match req.op.as_str(){
        "item"=>{let item=if id=="root"{let mut root=root_item(d);root.pinned=j.pins.contains("root");root.metadata_version=format!("root:{}",root.pinned);root.sync_state=Some(if root.pinned{"pinned"}else{"online"}.into());root}else{j.items.get(id).context("no_such_item")?.item.clone()};Ok(json!({"item":item}))},
        "list"=>{let parent=req.parent_id.as_deref().unwrap_or("root");let items=j.items.values().filter(|r|parent=="working_set"||r.item.parent_id==parent).map(|r|r.item.clone()).collect::<Vec<_>>();Ok(json!({"items":items,"anchor":j.anchor.to_string(),"smart":d.smart}))},
        "changes"=>{if req.anchor.as_deref()==Some("current"){return Ok(json!({"items":[],"deleted":[],"anchor":j.anchor.to_string(),"more":false}));}let since=req.anchor.as_deref().unwrap_or("0").parse::<u64>()?;if j.events.first().is_some_and(|e|since>0&&since+1<e.anchor)||since>j.anchor{bail!("sync_anchor_expired");}let mut items=BTreeMap::new();let mut deleted=std::collections::BTreeSet::new();for e in j.events.iter().filter(|e|e.anchor>since){if let Some(i)=&e.item{items.insert(i.id.clone(),i.clone());deleted.remove(&i.id);}if let Some(id)=&e.deleted{items.remove(id);deleted.insert(id.clone());}}Ok(json!({"items":items.values().collect::<Vec<_>>(),"deleted":deleted,"anchor":j.anchor.to_string(),"more":false}))},
        _=>bail!("unsupported catalog operation"),
    }
}
fn is_smart_catalog(home:&Path,req:&Request)->bool {
    ["item","list","changes"].contains(&req.op.as_str())&&domains(home).is_ok_and(|all|all.iter().any(|d|d.id==req.domain&&d.smart))
}
fn update_overlay(store:&Store,d:&Domain)->Result<()> {
    let mut j=checked_journal(&store.home,&store.config,d)?;let old=j.items.clone();let mut next=std::mem::take(&mut j.items);apply_policies(store,d,&j,&mut next)?;
    for (id,r) in &next {if old.get(id).is_none_or(|old|old.item!=r.item){push_event(&mut j,Some(r.item.clone()),None);}}
    j.items=next;save_journal(store,&d.id,&j)
}
pub fn smart_summary(store:&Store)->Result<Value> {
    let mut result=vec![];for d in domains(&store.home)?.into_iter().filter(|d|d.smart){let queue=crate::smart::Queue::open(store,&d.id,&journal_binding(&store.config,&d)?)?;result.push(json!({"domain":d,"status":queue.status()?,"mutations":queue.list()}));}
    Ok(json!({"control":crate::smart::control(store)?,"domains":result,"cache":crate::cache::status(store)?}))
}

/// Local-only progress queries bypass the worker's Store/network lock.
pub fn index_status(home:&Path,domain_id:Option<&str>)->Result<Value> {
    let config:config::Config=state::read_json(&home.join("config.json"))?;
    config.index.validate()?;
    let all=domains(home)?;
    if let Some(id)=domain_id {if !all.iter().any(|d|d.id==id&&d.smart){bail!("Smart domain not found");}}
    let mut reports=Vec::new();
    for d in all.into_iter().filter(|d|d.smart&&domain_id.is_none_or(|id|id==d.id)) {
        let mut report=index::status(home,&d.id,&journal_binding(&config,&d)?)?;
        report["domain"]=json!(d.id);report["display_name"]=json!(d.display_name);report["repository"]=json!(d.repository);
        reports.push(report);
    }
    Ok(json!({"policy":config.index,"domains":reports,"refresh_requires_provider_service":true}))
}

/// Persist a one-shot demand separately from browser hints, so opening a folder
/// cannot overwrite a user's recursive refresh before the worker consumes it.
pub fn index_refresh(home:&Path,domain_id:&str,relative_path:&str,recursive:bool)->Result<Value> {
    let config:config::Config=state::read_json(&home.join("config.json"))?;
    let d=domains(home)?.into_iter().find(|d|d.id==domain_id&&d.smart).context("Smart domain not found")?;
    let path=backend::normalize(relative_path)?;
    if d.repository&&!path.is_empty(){bail!("managed repository indexing refreshes its complete root snapshot");}
    let binding=journal_binding(&config,&d)?;
    let id=if path.is_empty(){"root".to_owned()}else{
        let record=index::lookup_path(home,&d.id,&binding,&path)?.context("directory is not indexed yet; refresh its nearest indexed parent")?;
        if record.item.kind!="directory"{bail!("index refresh requires a directory");}record.item.id
    };
    let directory=private(&demand_directory(home,&d.id))?;
    let demand=CatalogDemand{binding,id:id.clone(),path:path.clone(),metadata:false,requested:chrono::Utc::now().timestamp_millis(),recursive,force:true};
    state::atomic_json(&directory.join(format!("directory-{}.json",state::key(&format!("manual:{id}")))),&demand)?;
    Ok(json!({"queued":true,"domain":d.id,"remote":path,"recursive":recursive,"completed":false,"requires_provider_service":true}))
}
fn stable_mutation(req:&Request,source:Option<&File>)->Result<Request> {
    let mut request=req.clone();request.auth_token=None;request.content_path=None;request.request_id=None;request.base_metadata_version=request.base_metadata_version.map(|v|metadata_base(&v).to_owned());
    let content=source.map(|file|crate::metadata::capture_file(file,&Default::default()).and_then(|m|Ok((m.content_sha256.clone(),m.fingerprint()?)))).transpose()?;
    request.request_id=Some(state::key(&serde_json::to_string(&(&request,&content))?));Ok(request)
}
// Chaining is permitted only through our own confirmed write receipt. The
// original mutation/base stays immutable; the next remote operation still runs
// the usual fresh scan and conditional write using that exact confirmed version.
fn chained_request(store:&Store,d:&Domain,queue:&crate::smart::Queue,mutation:&crate::smart::Mutation)->Result<(Request,Option<Value>)>{
    let mut j=checked_journal(&store.home,&store.config,d)?;
    let mut req:Request=if let Some(saved)=j.smart_replays.get(&mutation.id){saved.clone()}else{serde_json::from_value(mutation.request.clone())?};
    let key=if req.op=="create"{format!("create:{}",req.client_item_id.as_deref().context("create identity missing")?)}else{req.request_id.clone().context("request identity missing")?};
    let key=format!("{}:{key}",state::key(&d.id));
    if let Some(response)=j.requests.get(&key).and_then(|p|p.response.clone()){return Ok((req,Some(response)));}
    if j.smart_replays.contains_key(&mutation.id){return Ok((req,None));}
    let previous=queue.list().into_iter().filter(|m|m.sequence<mutation.sequence&&m.state==crate::smart::MutationState::Done).collect::<Vec<_>>();
    if req.op=="modify" {
        let id=req.id.as_deref().context("modify item missing")?;
        let mut reachable=std::collections::BTreeSet::new();
        if let (Some(content),Some(metadata))=(&req.base_content_version,&req.base_metadata_version){reachable.insert((content.clone(),metadata_base(metadata).to_owned()));}
        for prior in &previous {
            let plan=j.smart_replays.get(&prior.id).cloned().unwrap_or(serde_json::from_value(prior.request.clone())?);
            if plan.op!="modify"||plan.id.as_deref()!=Some(id){continue;}
            let Some(item)=prior.response.as_ref().and_then(|response|response.get("item")) else{continue;};let item:Item=serde_json::from_value(item.clone())?;
            if item.id!=id{continue;}
            if let (Some(content),Some(metadata))=(&plan.base_content_version,&plan.base_metadata_version){if reachable.contains(&(content.clone(),metadata_base(metadata).to_owned())){reachable.insert((item.content_version.clone(),metadata_base(&item.metadata_version).to_owned()));}}
        }
        if let Some(current)=j.items.get(id){
            let current_base=(current.item.content_version.clone(),metadata_base(&current.item.metadata_version).to_owned());
            if reachable.contains(&current_base){req.base_content_version=Some(current_base.0);req.base_metadata_version=Some(current.item.metadata_version.clone());}
            // Otherwise preserve the original base: the normal fresh scan/CAS
            // reports the conflict. Never adopt an unrelated current revision.
        }
    }else if req.op=="create"&&req.smart_origin_id.is_some() {
        let prior=previous.iter().rev().find(|m|m.request["op"]=="create"&&m.request["smart_origin_id"]==mutation.request["smart_origin_id"]&&m.request["name"]==mutation.request["name"]&&m.request["parent_id"]==mutation.request["parent_id"]&&m.request["kind"]==mutation.request["kind"]);
        if let Some(prior)=prior {
            let item:Item=serde_json::from_value(prior.response.as_ref().and_then(|r|r.get("item")).cloned().context("version_conflict: earlier creation receipt has no item")?)?;
            if !j.items.get(&item.id).is_some_and(|r|r.item.content_version==item.content_version&&metadata_base(&r.item.metadata_version)==metadata_base(&item.metadata_version)){bail!("version_conflict: remote item changed after an earlier creation; newer edit retained");}
            req.base_content_version=Some(item.content_version);req.base_metadata_version=Some(item.metadata_version);req.op="modify".into();req.id=Some(item.id);req.client_item_id=None;
        }
    }
    // Persist the exact replay before any remote side effect. A crash cannot
    // later derive a different base or turn an own-create update into create.
    j.smart_replays.insert(mutation.id.clone(),req.clone());save_journal(store,&d.id,&j)?;Ok((req,None))
}
// Confirmation receipts keep their hashes and metadata, while completed
// transfer payloads can be reclaimed. Unconfirmed/conflicting/superseded edits
// remain explicit recovery data and are never silently discarded here.
fn release_confirmed_payload(home:&Path,d:&Domain,queue:&mut crate::smart::Queue,mutation:&crate::smart::Mutation){
    if queue.get(&mutation.id).is_ok_and(|m|m.state==crate::smart::MutationState::Done){let _=queue.discard_snapshot(&mutation.id);}else{return;}
    let Ok(store)=Store::open(home.to_path_buf()) else{return;};let Ok(j)=checked_journal(home,&store.config,d) else{return;};
    let Some(req)=j.smart_replays.get(&mutation.id) else{return;};
    let key=if req.op=="create"{req.client_item_id.as_ref().map(|id|format!("create:{id}"))}else{req.request_id.clone()};
    if let Some(key)=key{let key=format!("{}:{key}",state::key(&d.id));if j.requests.get(&key).is_some_and(|p|p.response.is_some()){
        for prefix in ["content","empty"]{let _=fs::remove_file(home.join("provider").join(format!("{prefix}-{}",state::key(&key))));}let _=File::open(home.join("provider")).and_then(|directory|directory.sync_all());
    }}
}
fn smart_failure(error:&anyhow::Error)->crate::smart::FailureKind {
    let text=format!("{error:#}");if text.contains("version_conflict")||text.contains("revision")||text.contains("conflict"){crate::smart::FailureKind::Conflict}
    else if text.contains("unsupported")||text.contains("invalid")||text.contains("checksum")||text.contains("snapshot")||text.contains("No space"){crate::smart::FailureKind::Other}else{crate::smart::FailureKind::Offline}
}
async fn replay(home:&Path,staging:&Path,fd:&File,d:&Domain,queue:&mut crate::smart::Queue,mutation:&crate::smart::Mutation)->Result<Value> {
    use crate::smart::MutationState;
    if mutation.state==MutationState::Done{release_confirmed_payload(home,d,queue,mutation);return mutation.response.clone().context("Smart receipt missing");}
    if matches!(mutation.state,MutationState::Conflict|MutationState::Failed){bail!("version_conflict: retained Smart edit requires attention: {}",mutation.error.as_deref().unwrap_or("replay stopped"));}
    let prepared=(||->Result<_>{
        queue.start(&mutation.id)?;let lease=queue.snapshot(&mutation.id)?;let store=Store::open(home.to_path_buf())?;
        let (req,receipt)=chained_request(&store,d,queue,mutation)?;update_overlay(&store,d)?;drop(store);
        let content=if receipt.is_none(){lease.as_ref().map(|lease|staging_output_file(staging,fd,lease.file())).transpose()?}else{None};Ok((req,receipt,content))
    })();
    let (mut req,receipt,content)=match prepared{Ok(result)=>result,Err(error)=>{queue.fail(&mutation.id,smart_failure(&error),&format!("{error:#}"))?;return Err(error);}};
    if let Some(receipt)=receipt{queue.complete(&mutation.id,receipt.clone())?;release_confirmed_payload(home,d,queue,mutation);return Ok(receipt);}req.content_path=content.clone();
    let result=tokio::time::timeout(std::time::Duration::from_secs(30),online_process(home,staging,fd,req,crate::transfer::global())).await;
    if let Some(content)=content {let _=fs::remove_file(content);}
    match result {
        Ok(Ok(response))=>{queue.complete(&mutation.id,response.clone())?;release_confirmed_payload(home,d,queue,mutation);Ok(response)},
        other=>{let error=match other {Ok(Err(e))=>e,Err(_)=>anyhow::anyhow!("server_unreachable: Smart upload timed out; local edit retained"),_=>unreachable!()};queue.fail(&mutation.id,smart_failure(&error),&format!("{error:#}"))?;Err(error)},
    }
}
pub async fn process(home:&Path,staging:&Path,fd:&File,req:Request,context:crate::transfer::TransferContext)->Result<Value> {
    if req.v!=1{bail!("unsupported bridge protocol version");}
    if ["domains","status","resolve","cache_budget"].contains(&req.op.as_str()){return quick(home,&req);}
    if req.op=="pin"{return local_pin(home,&req);}
    if req.op=="native_error"{return local_native_error(home,&req);}
    let all=domains(home)?;let d=all.into_iter().find(|d|d.id==req.domain).context("no_such_item: domain missing")?;
    if !d.smart {return online_process(home,staging,fd,req,context).await;}
    if is_smart_catalog(home,&req){
        let config=state::read_json(&home.join("config.json"))?;
        let binding=journal_binding(&config,&d)?;
        let payload=match index::query(home,&d,&binding,&req)? {
            Some(payload)=>payload,
            None=>{checked_journal(home,&config,&d)?;index::query(home,&d,&binding,&req)?.context("provider index was not initialized")?},
        };
        if !d.repository {
            if req.op=="list" {let parent=req.parent_id.as_deref().unwrap_or("root");if parent!="working_set"{request_index_demand(home,&config,&d,parent,false)?;}}
            else if req.op=="item" {if let Some(id)=req.id.as_deref().filter(|id|*id!="root"){if index::lookup(home,&d.id,&binding,id)?.is_some_and(|r|r.item.kind=="file"){request_index_demand(home,&config,&d,id,true)?;}}}
        }
        return Ok(payload);
    }
    if ["create","modify","delete"].contains(&req.op.as_str()) {
        let source=req.content_path.as_ref().map(|p|staging_file(staging,fd,p)).transpose()?;let normalized=stable_mutation(&req,source.as_ref())?;
        let store=Store::open(home.to_path_buf())?;let mut queue=crate::smart::Queue::open(&store,&d.id,&journal_binding(&store.config,&d)?)?;
        let mutation=queue.enqueue(serde_json::to_value(normalized)?,source.as_ref())?;
        update_overlay(&store,&d)?;
        let predecessor=queue.list().iter().any(|m|m.sequence<mutation.sequence&&m.state.active()&&((m.request["op"]!="create"&&m.request["id"]==mutation.request["id"])||(m.request["op"]=="create"&&mutation.request["op"]=="create"&&!mutation.request["smart_origin_id"].is_null()&&m.request["smart_origin_id"]==mutation.request["smart_origin_id"])));drop(store);
        let result=if predecessor {Err(anyhow::anyhow!("server_unreachable: an earlier local edit is pending; newer edit retained in order"))}else if crate::smart::control_at(home)?.paused {Err(anyhow::anyhow!("server_unreachable: Smart synchronization paused; local edit retained"))}else{replay(home,staging,fd,&d,&mut queue,&mutation).await};
        drop(queue);let store=Store::open(home.to_path_buf())?;update_overlay(&store,&d)?;return result;
    }
    // The operating system opens materialized files offline itself. For explicit
    // CLI-cached copies, only return a verified, account- and revision-bound FD.
    if req.op=="fetch" {
        let config:config::Config=state::read_json(&home.join("config.json"))?;
        if crate::smart::read_only(home,&d.id,&journal_binding(&config,&d)?)?.iter().any(|m|m.state.active()&&m.request["id"].as_str()==req.id.as_deref()){
            bail!("server_unreachable: a retained local edit is awaiting synchronization; the system local copy is protected");
        }
    }
    if req.op=="fetch"&&!d.repository {
        let mut store=Store::open(home.to_path_buf())?;let j=checked_journal(home,&store.config,&d)?;
        if let Some(record)=req.id.as_ref().and_then(|id|j.items.get(id)) {check_fetch_base(&req,&record.item)?;let path=remote(&d,&record.path)?;
            if let Ok(lease)=crate::cache::acquire(&mut store,&d.profile,&path) {if record.strong.as_ref().is_some_and(|revision|revision.same_revision(&lease.item.entry)) {let delivered=staging_output_file(staging,fd,lease.file())?;return Ok(json!({"item":record.item,"content_path":delivered,"cached":true}));}}
        }
    }
    online_process(home,staging,fd,req,context).await
}
/// Foreground reconnect pass for CLI users; the long-running provider service
/// invokes the same worker automatically. Staging remains private and local.
pub async fn run_smart_once(home:&Path,staging:&Path)->Result<Value> {
    let home=private(home)?;let staging=private(staging)?;let fd=open_staging(&staging)?;
    let mut backends=BTreeMap::new();
    background_once(&home,&staging,&fd,&mut backends).await?;let store=Store::open(home)?;smart_summary(&store)
}
fn begin_scan(attempts:&mut BTreeMap<String,ScanAttempt>,id:&str){let now=chrono::Utc::now().timestamp_millis();let attempt=attempts.entry(id.into()).or_default();attempt.last=now;attempt.next_try=now+3_000;}
fn finish_scan(attempts:&mut BTreeMap<String,ScanAttempt>,id:&str,success:bool){
    let now=chrono::Utc::now().timestamp_millis();let attempt=attempts.entry(id.into()).or_default();
    if success{attempt.failures=0;attempt.next_try=now+3_000;}else{attempt.failures=attempt.failures.saturating_add(1);let delay=(3_000i64<<attempt.failures.saturating_sub(1).min(4)).min(60_000);attempt.next_try=now+delay;}
}
type IndexBackends=BTreeMap<String,(String,Arc<dyn Backend>)>;
async fn background_once(home:&Path,staging:&Path,fd:&File,backends:&mut IndexBackends)->Result<()> {
    use crate::smart::MutationState;
    let configured=domains(home)?;
    backends.retain(|id,_|configured.iter().any(|d|d.smart&&d.id==*id));
    for d in configured.into_iter().filter(|d|d.smart) {
        let store=Store::open(home.to_path_buf())?;update_overlay(&store,&d)?;if crate::smart::control(&store)?.paused {continue;}
        let mut queue=crate::smart::Queue::open(&store,&d.id,&journal_binding(&store.config,&d)?)?;
        let pending=queue.list().into_iter().find(|m|matches!(m.state,MutationState::Pending|MutationState::Interrupted));drop(store);
        if let Some(mutation)=pending {let _=replay(home,staging,fd,&d,&mut queue,&mutation).await;}drop(queue);
        let store=Store::open(home.to_path_buf())?;update_overlay(&store,&d)?;
        let profile=store.config.profiles.get(&d.profile).context("domain profile missing")?.clone();let mut j=checked_journal(home,&store.config,&d)?;
        let timeout=std::time::Duration::from_secs(profile.timeout_secs.max(5));
        let binding=journal_binding(&store.config,&d)?;
        if d.repository {
            let now=chrono::Utc::now().timestamp_millis();
            let demands=catalog_demands(&store,&d,&j)?;
            let last=j.directory_scans.get("root").copied().unwrap_or(0);
            let root_demands=demands.iter().filter(|d|!d.metadata&&d.id=="root").collect::<Vec<_>>();
            let forced=root_demands.iter().any(|d|d.force&&d.requested>last);
            let active=root_demands.iter().any(|d|!d.force);
            let ttl=if active{store.config.index.active_refresh_secs}else{store.config.index.refresh_secs};
            if !forced&&last>0&&now.saturating_sub(last)<(ttl*1000) as i64{continue;}
            if j.directory_attempts.get("root").is_some_and(|a|a.next_try>now){continue;}
            begin_scan(&mut j.directory_attempts,"root");save_journal(&store,&d.id,&j)?;
            let result=tokio::time::timeout(timeout,async {
                let b=background_backend(backends,&d,&profile,&binding).await?;
                refresh(&store,b.as_ref(),&d,&mut j).await?;
                j.directory_scans.insert("root".into(),chrono::Utc::now().timestamp_millis());
                Ok::<_,anyhow::Error>(())
            }).await;
            let success=matches!(result,Ok(Ok(())));
            if !success{backends.remove(&d.id);}
            finish_scan(&mut j.directory_attempts,"root",success);
            if success {let completed=index_scheduler::completed_manual_demands(&j,&demands,true);j.completed_demands.extend(completed);}
            save_journal(&store,&d.id,&j)?;
            continue;
        }
        let demands=catalog_demands(&store,&d,&j)?;let directory=next_directory(&store,&d,&j,&demands)?;
        let metadata=next_metadata(&j,&demands);
        if directory.is_none()&&metadata.is_none(){continue;}
        if let Some(id)=&directory{begin_scan(&mut j.directory_attempts,id);j.directory_turn=j.directory_turn.saturating_add(1);save_journal(&store,&d.id,&j)?;}
        let connected=tokio::time::timeout(timeout,background_backend(backends,&d,&profile,&binding)).await;
        let b=match connected {Ok(Ok(b))=>b,_=>{if let Some(id)=&directory{finish_scan(&mut j.directory_attempts,id,false);save_journal(&store,&d.id,&j)?;}continue;}};
        if let Some(id)=&directory {
            let success=matches!(tokio::time::timeout(timeout,refresh_directory(&store,b.as_ref(),&d,&mut j,id)).await,Ok(Ok(())));
            if !success{backends.remove(&d.id);}
            finish_scan(&mut j.directory_attempts,id,success);
            if success {let completed=index_scheduler::completed_manual_demands(&j,&demands,false);j.completed_demands.extend(completed);}
            save_journal(&store,&d.id,&j)?;
        }
        // Namespace is already durably published before optional sidecar work.
        // At most one requested/known binding is enriched per pass.
        if let Some(id)=next_metadata(&j,&demands){
            begin_scan(&mut j.metadata_attempts,&id);save_journal(&store,&d.id,&j)?;
            let success=matches!(tokio::time::timeout(timeout,enrich_metadata(&store,b.as_ref(),&d,&mut j,&id)).await,Ok(Ok(())));
            finish_scan(&mut j.metadata_attempts,&id,success);save_journal(&store,&d.id,&j)?;
        }
    }Ok(())
}
async fn background_backend(backends:&mut IndexBackends,d:&Domain,profile:&config::Profile,binding:&str)->Result<Arc<dyn Backend>> {
    let connection_key=state::key(&serde_json::to_string(&(binding,profile))?);
    if let Some((saved,backend))=backends.get(&d.id){if saved==&connection_key{return Ok(backend.clone());}}
    let backend=crate::factory::connect(profile,&d.profile,crate::transfer::global()).await?;
    backends.insert(d.id.clone(),(connection_key,backend.clone()));Ok(backend)
}

fn quick(home:&Path,req:&Request)->Result<Value>{
    if req.op=="cache_budget" {
        if let Some(used)=req.native_used_bytes{let id=req.native_accounting_id.as_deref().context("native accounting identity required")?;
            let configured=domains(home)?.iter().any(|d|d.id==req.domain);
            let cleared_unregistered=!configured&&req.native_confirmed_unregistered&&used==0&&req.native_reserved_bytes.unwrap_or(0)==0&&req.native_pinned_bytes.unwrap_or(0)==0&&req.native_dirty_bytes.unwrap_or(0)==0&&req.native_cached_versions.is_none();
            if id!=format!("fileprovider:{}",req.domain)||(!configured&&!cleared_unregistered){bail!("invalid native domain accounting identity");}
            crate::cache::report_native_usage(home,id,used,req.native_reserved_bytes.unwrap_or(0),req.native_pinned_bytes.unwrap_or(0))?;
        }
        if let Some(versions)=&req.native_cached_versions {
            let config:config::Config=state::read_json(&home.join("config.json"))?;let d=domains(home)?.into_iter().find(|d|d.id==req.domain).context("invalid native cached domain")?;
            if req.native_accounting_id.as_deref()!=Some(&format!("fileprovider:{}",d.id)){bail!("invalid native cached accounting identity");}
            if versions.len()>512||versions.iter().any(|(id,version)|id.len()>4096||version.len()>128||id.chars().any(char::is_control)||version.chars().any(char::is_control)){bail!("invalid native cached version chunk");}
            let mut combined=if req.native_cached_versions_reset.unwrap_or(true){BTreeMap::new()}else{read_native_cached(home,&config,&d)?};combined.extend(versions.clone());
            state::atomic_json(&native_cached_path(home,&d.id),&NativeCached{binding:journal_binding(&config,&d)?,versions:combined})?;
        }
        let status=crate::cache::budget(home)?;let mut payload=serde_json::to_value(&status)?;
        payload["rust_used_bytes"]=json!(status.used_bytes);payload["rust_reserved_bytes"]=json!(status.reserved_bytes);
        payload["used_bytes"]=json!(status.used_bytes.saturating_add(status.native_used_bytes));
        payload["reserved_bytes"]=json!(status.reserved_bytes.saturating_add(status.native_reserved_bytes));
        payload["pinned_bytes"]=json!(status.pinned_bytes.saturating_add(status.native_pinned_bytes));return Ok(payload);
    }
    let all=domains(home)?;
    if req.op=="domains"{return Ok(json!({"domains":all.iter().map(|d|json!({"id":d.id,"display_name":d.display_name,"smart":d.smart,"local_root":d.local_root})).collect::<Vec<_>>()}));}
    let config:config::Config=state::read_json(&home.join("config.json"))?;
    if req.op=="resolve" {
        let url=url::Url::parse(req.file_url.as_deref().context("file URL required")?)?;let path=url.to_file_path().map_err(|_|anyhow::anyhow!("invalid local file URL"))?;
        for d in all {if let Some(root)=&d.local_root{if let Ok(relative)=path.strip_prefix(root){
            if !index::path(home,&d.id).try_exists()?{checked_journal(home,&config,&d)?;}
            let binding=journal_binding(&config,&d)?;
            let rel=relative.to_str().context("non-UTF8 local path")?.replace('\\',"/");
            if rel.is_empty(){let root_request:Request=serde_json::from_value(json!({"v":1,"op":"item","id":"root"}))?;index::query(home,&d,&binding,&root_request)?;return Ok(json!({"domain":d.id,"id":"root"}));}
            if let Some(record)=index::lookup_path(home,&d.id,&binding,&rel)?{return Ok(json!({"domain":d.id,"id":record.item.id}));}
        }}}
        bail!("no_such_item: file is outside configured Finder roots");
    }
    let d=all.into_iter().find(|d|d.id==req.domain).context("no_such_item: domain missing")?;
    if !index::path(home,&d.id).try_exists()?{checked_journal(home,&config,&d)?;}
    let binding=journal_binding(&config,&d)?;
    let id=req.id.as_deref().unwrap_or("root");if id=="root"{
        let mut request=req.clone();request.op="item".into();
        let mut payload=index::query(home,&d,&binding,&request)?.context("provider index was not initialized")?;
        payload["state"]=json!(if payload["item"]["pinned"]==true{"pinned"}else{"online"});payload["smart"]=json!(d.smart);payload["message"]=json!("状态来自本地索引");return Ok(payload);
    }
    let record=index::lookup(home,&d.id,&binding,id)?.context("no_such_item: item not enumerated")?;
    let db:crate::state::Database=state::read_json(&home.join("state.json"))?;
    let path=remote(&d,&record.path)?;let cached=db.cache.get(&Store::cache_key(&d.profile,&path));
    let state=if record.item.sync_state.as_deref().is_some_and(|s|["queued","conflict","error","syncing"].contains(&s)){record.item.sync_state.as_deref().unwrap()}else if db.jobs.iter().any(|x|x.profile==d.profile&&x.remote==path&&["pending","running","interrupted"].contains(&x.status.as_str())){"syncing"}else if record.item.pinned{"pinned"}else if read_native_cached(home,&config,&d)?.get(id)==Some(&record.item.content_version)||cached.is_some_and(|c|home.join("cache").join(&c.object).is_file()){"cached"}else{"online"};
    Ok(json!({"state":state,"item":record.item,"smart":d.smart,"message":record.item.sync_message.as_deref().unwrap_or("状态来自本地索引，未探测网络")}))
}
fn local_native_error(home:&Path,req:&Request)->Result<Value>{
    let store=Store::open(home.to_path_buf())?;let d=domains(home)?.into_iter().find(|d|d.id==req.domain).context("no_such_item: domain missing")?;let mut j=checked_journal(home,&store.config,&d)?;let id=req.id.as_deref().context("item required")?;
    let record=j.items.get(id).context("no_such_item")?;if req.base_content_version.as_ref().is_some_and(|v|v!=&record.item.content_version){bail!("version_conflict: native report refers to an older version");}
    if let Some(message)=&req.native_message {if message.len()>1024||message.chars().any(char::is_control){bail!("invalid native attention message");}j.native_errors.insert(id.into(),NativeAttention{version:record.item.content_version.clone(),message:message.clone()});}else{j.native_errors.remove(id);}
    save_journal(&store,&d.id,&j)?;update_overlay(&store,&d)?;let j=checked_journal(home,&store.config,&d)?;Ok(json!({"item":j.items.get(id).unwrap().item}))
}
fn local_pin(home:&Path,req:&Request)->Result<Value>{
    let mut store=Store::open(home.to_path_buf())?;let domain=domains(home)?.into_iter().find(|d|d.id==req.domain).context("no_such_item: domain missing")?;
    let mut j=checked_journal(home,&store.config,&domain)?;let id=req.id.as_deref().context("item required")?;
    if id!="root"&&!j.items.contains_key(id){bail!("no_such_item");}
    if req.pinned.unwrap_or(true){j.pins.insert(id.to_owned());}else{j.pins.remove(id);}
    let old=j.items.clone();let mut next=std::mem::take(&mut j.items);apply_policies(&store,&domain,&j,&mut next)?;
    for (id,r) in &next {if old.get(id).is_none_or(|old|old.item!=r.item){push_event(&mut j,Some(r.item.clone()),None);}
        if !domain.repository{let path=remote(&domain,&r.path)?;if store.db.cache.contains_key(&Store::cache_key(&domain.profile,&path)){crate::cache::set_pinned(&mut store,&domain.profile,&path,r.item.pinned,false)?;}}
    }
    j.items=next;
    let item=if id=="root"{let mut item=root_item(&domain);item.pinned=j.pins.contains("root");item.metadata_version=format!("root:{}",item.pinned);item.sync_state=Some(if item.pinned{"pinned"}else{"online"}.into());push_event(&mut j,Some(item.clone()),None);item}else{j.items.get(id).unwrap().item.clone()};
    save_journal(&store,&domain.id,&j)?;Ok(json!({"item":item}))
}
async fn online_process(home:&Path,staging:&Path,staging_fd:&File,req:Request,context:crate::transfer::TransferContext)->Result<Value>{
    if req.v!=1{bail!("unsupported bridge protocol version");}
    if ["domains","status","resolve","cache_budget"].contains(&req.op.as_str()){return quick(home,&req);}
    if req.op=="pin"{return local_pin(home,&req);}
    if req.op=="native_error"{return local_native_error(home,&req);}
    let store=Store::open(home.to_path_buf())?;let d=domains(home)?.into_iter().find(|d|d.id==req.domain).context("no_such_item: domain missing")?;
    let profile=store.config.profiles.get(&d.profile).context("not_authenticated: domain profile missing")?.clone();
    let mut j=checked_journal(home,&store.config,&d)?;let b=crate::factory::connect(&profile,&d.profile,context).await?;
    let pinned_snapshot=refresh_request(&store,b.as_ref(),&d,&mut j,&req,None).await?;
    let id=req.id.as_deref().unwrap_or("root");
    match req.op.as_str(){
        "item"|"list"|"changes"=>return catalog(&d,&j,&req),
        "fetch"=>{
            let record=j.items.get(id).context("no_such_item")?.clone();check_fetch_base(&req,&record.item)?;
            if record.item.kind!="file"{bail!("unsupported: cannot fetch a directory");}
            let dest=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());
            if d.repository{repository::get_from(&store,b.as_ref(),&d.remote,pinned_snapshot.as_ref().context("managed snapshot missing")?,&record.path,&dest,false).await?;}
            else{
                let path=remote(&d,&record.path)?;let strong=b.stat(&path).await?.context("no_such_item")?;
                let downloaded=crate::operations::verified_download(b.as_ref(),&path,&dest).await?;
                if !strong.same_revision(&downloaded){let _=fs::remove_file(&dest);bail!("version_conflict: file changed during content snapshot");}
                let observed=b.stat_metadata(&path).await?.context("no_such_item")?;
                if !record.entry.as_ref().is_some_and(|old|old.same_revision(&observed)){let _=fs::remove_file(&dest);bail!("version_conflict: file changed while fetching");}
                if strong.etag.as_ref().is_some_and(|tag|!tag.starts_with("W/")){
                    let metadata=crate::metadata_remote::load(b.as_ref(),&path,&strong).await?;
                    if let Some(metadata)=&metadata{crate::metadata::apply(&dest,metadata,&Default::default())?;}
                    let previous=j.items.clone();let mut next=j.items.clone();let item=next.get_mut(id).unwrap();item.strong=Some(strong);project_metadata(item,metadata.as_ref());emit_namespace(&store,&d,&mut j,&previous,next)?;
                }
            }
            let delivered=staging_output(staging,staging_fd,&dest)?;let _=fs::remove_file(dest);
            return Ok(json!({"item":j.items.get(id).map(|r|&r.item).unwrap_or(&record.item),"content_path":delivered}));
        },
        "create"|"modify"|"delete"=>(),_=>bail!("unsupported provider operation"),
    }
    let key=if req.op=="create"{format!("create:{}",req.client_item_id.as_deref().context("create requires client_item_id")?)}else{req.request_id.clone().context("mutation requires request_id")?};
    let key=format!("{}:{key}",state::key(&req.domain));
    let mut source=None;let mut metadata=None;
    if let Some(path)=&req.content_path {
        let mut input=staging_file(staging,staging_fd,path)?;let record=apply_xattrs(apply_tags(crate::metadata::capture_file(&input,&crate::metadata::MetadataPolicy::default())?,req.tag_data_hex.as_deref())?,req.extended_attributes.as_ref())?;
        let snapshot=store.home.join("provider").join(format!("content-{}",state::key(&key)));
        if !snapshot.exists(){use std::os::unix::fs::OpenOptionsExt;input.seek(SeekFrom::Start(0))?;let mut out=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&snapshot)?;std::io::copy(&mut input,&mut out)?;out.sync_all()?;}
        if state::fingerprint(&snapshot)?!=record.content_sha256{bail!("version_conflict: replayed source changed");}
        source=Some(snapshot);metadata=Some(record);
    }
    if req.op=="create"&&req.kind.as_deref()!=Some("directory")&&source.is_none(){let empty=store.home.join("provider").join(format!("empty-{}",state::key(&key)));if !empty.exists(){state::atomic_write(&empty,b"")?;}metadata=Some(apply_xattrs(apply_tags(crate::metadata::capture(&empty,&Default::default())?,req.tag_data_hex.as_deref())?,req.extended_attributes.as_ref())?);source=Some(empty);}
    let source_fingerprint=metadata.as_ref().map(|m|m.fingerprint()).transpose()?;
    if let Some(captured)=metadata.as_mut(){
        if let Some(saved)=j.requests.get(&key).and_then(|pending|pending.metadata.clone()){
            if captured.content_sha256!=saved.content_sha256||req.op=="create"&&j.requests.get(&key).and_then(|p|p.source_fingerprint.as_ref())!=source_fingerprint.as_ref(){bail!("version_conflict: replayed source or metadata changed");}
            *captured=saved;
        }else if req.op=="modify"{
            let previous=j.items.get(id).context("no_such_item")?;
            let old=if d.repository{pinned_snapshot.as_ref().context("managed snapshot missing")?.commit.entries.get(&previous.path).and_then(|i|i.metadata.clone()).map(serde_json::from_value::<crate::metadata::MetadataRecord>).transpose()?}
                else if let Some(strong)=&previous.strong{crate::metadata_remote::load(b.as_ref(),&remote(&d,&previous.path)?,strong).await?}else{None};
            captured.attributes=if let Some(old)=old{old.attributes}else{let mut attrs=previous.item.extended_attributes.clone();if let Some(tags)=&previous.item.tag_data_hex{attrs.insert("com.apple.metadata:_kMDItemUserTags".into(),tags.clone());}attrs};
            *captured=apply_xattrs(apply_tags(captured.clone(),req.tag_data_hex.as_deref())?,req.extended_attributes.as_ref())?;
        }
    }
    let metadata_fingerprint=metadata.as_ref().map(|m|m.fingerprint()).transpose()?;
    let fingerprint=state::key(&serde_json::to_string(&(&req.op,&req.domain,&req.id,&req.parent_id,&req.name,&req.kind,&req.base_content_version,&req.base_metadata_version,&req.tag_data_hex,&req.extended_attributes,&metadata_fingerprint,req.recursive))?);
    if let Some(p)=j.requests.get(&key){if p.fingerprint!=fingerprint{bail!("version_conflict: replayed request differs");}if let Some(response)=&p.response{return Ok(response.clone());}}
    // A successful delete removes the very item needed by normal dispatch.
    // Reconcile its saved immutable transaction before looking up that item;
    // never issue another delete against a replacement at the old pathname.
    if d.repository&&req.op=="delete" {
        if let Some(tx_id)=j.requests.get(&key).and_then(|pending|pending.transaction.clone()) {
            let transaction=repository::load(&store,&tx_id)?;
            if transaction.profile!=d.profile||transaction.root!=d.remote{bail!("version_conflict: replayed transaction belongs to another provider domain");}
            repository::commit(&store,b.as_ref(),&tx_id).await?;
            refresh(&store,b.as_ref(),&d,&mut j).await?;
            let response=json!({});j.requests.get_mut(&key).unwrap().response=Some(response.clone());
            save_journal(&store,&d.id,&j)?;return Ok(response);
        }
    }
    if !d.repository&&req.op=="delete"&&!j.items.contains_key(id) {
        if let Some(pending)=j.requests.get(&key) {
            if let (Some(path),Some(_expected))=(&pending.deletion_path,&pending.deletion_expected) {
                if j.items.values().any(|r|r.path==*path)||b.stat_metadata(&remote(&d,path)?).await?.is_some(){bail!("version_conflict: a replacement occupies the deleted item's path; it was preserved");}
                let response=json!({});j.requests.get_mut(&key).unwrap().response=Some(response.clone());save_journal(&store,&d.id,&j)?;return Ok(response);
            }
        }
    }
    let existing=if req.op=="create"{None}else{Some(j.items.get(id).context("no_such_item")?.clone())};
    if let Some(record)=&existing {if j.requests.get(&key).is_none_or(|p|p.confirmed.is_none()&&p.transaction.is_none()){check_base(&req,&record.item)?;}}
    let target=if req.op=="create"{child_path(&j,req.parent_id.as_deref().unwrap_or("root"),req.name.as_deref().context("name required")?)?}else{let record=existing.as_ref().unwrap();if req.name.is_some()||req.parent_id.is_some(){child_path(&j,req.parent_id.as_deref().unwrap_or(&record.item.parent_id),req.name.as_deref().unwrap_or(&record.item.name))?}else{record.path.clone()}};
    if !j.requests.contains_key(&key){let deletion=existing.as_ref().filter(|_|!d.repository&&req.op=="delete");j.requests.insert(key.clone(),Pending{fingerprint,metadata:metadata.clone(),source_fingerprint,transaction:None,confirmed:None,response:None,deletion_path:deletion.map(|r|r.path.clone()),deletion_expected:deletion.and_then(|r|r.entry.clone()),metadata_base_fingerprint:None});save_journal(&store,&d.id,&j)?;}
    let mut response_id=existing.as_ref().map(|r|r.item.id.clone());
    let mut committed_target:Option<Value>=None;
    if d.repository {
        let tx_id=if let Some(id)=j.requests.get(&key).and_then(|p|p.transaction.clone()){id}else{
            let snapshot=pinned_snapshot.clone().context("managed snapshot missing")?;
            if req.op=="create"&&snapshot.commit.entries.contains_key(&target){bail!("version_conflict: item name already exists; create does not replace existing content");}
            let tx=repository::begin_from(&store,&d.profile,&d.remote,snapshot)?;
            let tx_id=tx.id.clone();
            match req.op.as_str(){
                "create"=>{if req.kind.as_deref()==Some("directory"){repository::mkdir(&store,&tx_id,&target)?;}else{let path=if let Some(path)=&source{path.clone()}else{let path=store.home.join("provider").join(format!("empty-{}",state::key(&key)));if !path.exists(){state::atomic_write(&path,b"")?;}path};repository::put(&store,&tx_id,&path,&target)?;}},
                "modify"=>{let from=&existing.as_ref().unwrap().path;if let Some(source)=&source{repository::put(&store,&tx_id,source,from)?;}if &target!=from{repository::rename(&store,&tx_id,from,&target)?;}},
                "delete"=>{repository::remove(&store,&tx_id,&existing.as_ref().unwrap().path,req.recursive)?;},_=>unreachable!(),
            }
            if req.op!="delete" {
                if let Some(record)=metadata.clone(){repository::set_metadata(&store,&tx_id,&target,serde_json::to_value(record)?)?;}
                else if req.tag_data_hex.is_some()||req.extended_attributes.is_some(){let tx=repository::load(&store,&tx_id)?;let old=tx.entries.get(&target).context("item missing")?;if old.kind!="file"{bail!("unsupported directory tags");}let mut record:crate::metadata::MetadataRecord=if let Some(value)=&old.metadata{serde_json::from_value(value.clone())?}else{crate::metadata::MetadataRecord{version:1,content_sha256:old.blob.clone().unwrap(),attributes:BTreeMap::new(),selection:crate::metadata::MetadataPolicy::default().include,excluded:vec![]}};record=apply_xattrs(apply_tags(record,req.tag_data_hex.as_deref())?,req.extended_attributes.as_ref())?;repository::set_metadata(&store,&tx_id,&target,serde_json::to_value(record)?)?;}
                response_id=Some(repository::load(&store,&tx_id)?.entries.get(&target).context("item missing")?.id.clone());
            }
            j.requests.get_mut(&key).unwrap().transaction=Some(tx_id.clone());save_journal(&store,&d.id,&j)?;tx_id
        };
        let committed=repository::commit(&store,b.as_ref(),&tx_id).await?;if req.op!="delete"{let entry=committed.commit.entries.get(&target).context("committed item missing")?;response_id=Some(entry.id.clone());committed_target=Some(serde_json::to_value(entry)?);}
    }else {
        if req.op=="delete" {let record=existing.as_ref().unwrap();let e=record.entry.as_ref().context("item revision missing")?;if record.item.kind=="directory"{bail!("unsupported: directory deletion requires managed repository mode");}b.remove_checked(&remote(&d,&record.path)?,e).await?;}
        else if req.op=="create"&&req.kind.as_deref()==Some("directory"){if j.items.values().any(|r|r.path==target){bail!("version_conflict: directory already exists");}b.mkdir(&remote(&d,&target)?).await?;}
        else {
            let dest=remote(&d,&target)?;
            if let Some(old)=&existing {if target!=old.path{bail!("unsupported: use managed repository mode for version-guarded rename/reparent");}}
            let confirmed=j.requests.get(&key).and_then(|p|p.confirmed.clone());
            let entry=if let Some(confirmed)=confirmed {let observed=b.stat(&dest).await?.context("version_conflict: confirmed content disappeared")?;if !observed.same_revision(&confirmed){bail!("version_conflict: target changed after confirmed upload");}observed}
            else if let Some(source)=&source {
                let condition=if let Some(record)=&existing{WriteCondition::Match(record.strong.clone().context("unsupported: fetch this file before editing to establish its content guard")?)}else{WriteCondition::CreateNew};
                b.upload(source,&dest,condition).await?
            }else if let Some(old)=&existing {old.strong.clone().context("unsupported: fetch this file before editing metadata")?}
            else{let empty=store.home.join("provider").join(format!("empty-{}",state::key(&key)));state::atomic_write(&empty,b"")?;b.upload(&empty,&dest,WriteCondition::CreateNew).await?};
            j.requests.get_mut(&key).unwrap().confirmed=Some(entry.clone());save_journal(&store,&d.id,&j)?;
            if metadata.is_none()&&source.is_none()&&(req.tag_data_hex.is_some()||req.extended_attributes.is_some()) {
                if let Some(saved)=j.requests.get(&key).and_then(|pending|pending.metadata.clone()) {
                    let expected=j.requests.get(&key).and_then(|pending|pending.metadata_base_fingerprint.as_deref());
                    crate::metadata_remote::save_record_if_fingerprint(b.as_ref(),&dest,&entry,&saved,expected).await?;metadata=Some(saved);
                }
            }
            if let Some(record)=metadata.as_ref(){crate::metadata_remote::save_record_if_fingerprint(b.as_ref(),&dest,&entry,record,None).await?;}
            else if req.tag_data_hex.is_some()||req.extended_attributes.is_some(){let prior=crate::metadata_remote::load(b.as_ref(),&dest,&entry).await?;
                check_fresh_metadata_base(&req,&target,existing.as_ref().is_some_and(|r|r.item.pinned),prior.as_ref())?;
                let expected=prior.as_ref().map(|m|m.fingerprint()).transpose()?;
                let record=if let Some(record)=prior{record}else{let local=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());crate::operations::verified_download(b.as_ref(),&dest,&local).await?;let mut record=crate::metadata::capture(&local,&Default::default())?;let _=fs::remove_file(local);
                    if let Some(previous)=&existing{record.attributes=previous.item.extended_attributes.clone();if let Some(tags)=&previous.item.tag_data_hex{record.attributes.insert("com.apple.metadata:_kMDItemUserTags".into(),tags.clone());}}
                    record};
                let record=apply_xattrs(apply_tags(record,req.tag_data_hex.as_deref())?,req.extended_attributes.as_ref())?;
                let pending=j.requests.get_mut(&key).unwrap();pending.metadata=Some(record.clone());pending.metadata_base_fingerprint=expected.clone();save_journal(&store,&d.id,&j)?;
                crate::metadata_remote::save_record_if_fingerprint(b.as_ref(),&dest,&entry,&record,expected.as_deref()).await?;metadata=Some(record);}
        }
    }
    let final_snapshot=refresh_request(&store,b.as_ref(),&d,&mut j,&req,Some(&target)).await?;
    if let Some(expected)=committed_target {let actual=final_snapshot.as_ref().and_then(|snapshot|snapshot.commit.entries.get(&target)).map(serde_json::to_value).transpose()?;
        if actual.as_ref()!=Some(&expected){bail!("version_conflict: managed item changed after our commit; another writer's version was not recorded as our receipt");}
    }
    if req.op=="create"&&response_id.is_none(){response_id=j.items.iter().find(|(_,r)|r.path==target).map(|(id,_)|id.clone());}
    if !d.repository && req.op!="delete" {
        let response_id=response_id.as_deref().context("created item not enumerated")?;
        let record=j.items.get(response_id).context("updated item missing")?;
        if record.item.kind=="file" {
            let confirmed=j.requests.get(&key).and_then(|pending|pending.confirmed.as_ref()).context("version_conflict: successful file write has no confirmed revision")?;
            let path=remote(&d,&record.path)?;
            let current=b.stat(&path).await?.context("version_conflict: confirmed content disappeared")?;
            if !current.same_revision(confirmed){bail!("version_conflict: target changed after the confirmed write; no success receipt was recorded");}
            let observed=b.stat_metadata(&path).await?.context("version_conflict: confirmed metadata disappeared")?;
            if !record.entry.as_ref().is_some_and(|listed|listed.same_revision(&observed)){bail!("version_conflict: target changed after the final listing; no success receipt was recorded");}
            let verified_metadata=crate::metadata_remote::load(b.as_ref(),&path,&current).await?;
            if let Some(expected)=&metadata {
                let observed=verified_metadata.as_ref().context("version_conflict: confirmed file metadata disappeared; no success receipt was recorded")?;
                if observed.content_sha256!=expected.content_sha256 || observed.attributes!=expected.attributes {
                    bail!("version_conflict: file metadata changed after the confirmed write; no success receipt was recorded");
                }
            }
            let previous=record.item.clone();
            let record=j.items.get_mut(response_id).unwrap();
            record.strong=Some(current);project_metadata(record,verified_metadata.as_ref());
            let mut corrected=std::mem::take(&mut j.items);
            apply_policies(&store,&d,&j,&mut corrected)?;
            j.items=corrected;
            let corrected=j.items.get(response_id).unwrap().item.clone();
            if corrected!=previous {push_event(&mut j,Some(corrected),None);}
        }
    }
    let response=if req.op=="delete"{json!({})}else{let record=j.items.get_mut(response_id.as_deref().context("created item not enumerated")?).context("updated item missing")?;if req.op=="create"{record.client_id=req.client_item_id.clone();}let mut item=record.item.clone();if d.smart{item.has_unsynced_changes=false;item.native_materialized=false;item.sync_state=Some(if item.pinned{"pinned"}else{"online"}.into());item.sync_message=None;let base=metadata_version(&record.path,&item.tag_data_hex,&item.extended_attributes,item.pinned)?;item.metadata_version=format!("{base}:{}",&state::key(&serde_json::to_string(&(&item.sync_state,&item.sync_message,false,false))?)[..32]);}json!({"item":item})};
    j.requests.get_mut(&key).unwrap().response=Some(response.clone());save_journal(&store,&d.id,&j)?;Ok(response)
}
fn error(error:anyhow::Error)->Value {let text=format!("{error:#}");let code=if text.contains("cache capacity")||text.contains("cache budget")||text.contains("insufficient_quota"){"insufficient_quota"}else if text.contains("version_conflict")||text.contains("revision")||text.contains("conflict"){"version_conflict"}else if text.contains("sync_anchor_expired"){"sync_anchor_expired"}else if text.contains("no_such_item")||text.contains("missing"){"no_such_item"}else if text.contains("credential")||text.contains("authentication")||text.contains("password"){"not_authenticated"}else if text.contains("unsupported"){"unsupported"}else{"server_unreachable"};json!({"ok":false,"error":{"code":code,"message":text}})}
fn success(payload:Value)->Value{let mut value=payload.as_object().cloned().unwrap_or_default();value.insert("ok".into(),Value::Bool(true));Value::Object(value)}
fn private(path:&Path)->Result<PathBuf>{use std::os::unix::fs::{DirBuilderExt,MetadataExt};let mut builder=fs::DirBuilder::new();builder.recursive(true).mode(0o700);builder.create(path)?;let meta=fs::symlink_metadata(path)?;if meta.file_type().is_symlink()||!meta.is_dir()||meta.uid()!=unsafe{libc::geteuid()}||meta.mode()&0o077!=0{bail!("bridge directories must be real, private, owned directories");}Ok(path.canonicalize()?)}
pub async fn serve(home:PathBuf,socket:PathBuf,staging:PathBuf,development_tcp:bool)->Result<()> {
    use std::os::unix::{fs::{FileTypeExt,OpenOptionsExt,PermissionsExt},io::AsRawFd};
    let home=private(&home)?;let staging=private(&staging)?;let parent=private(socket.parent().context("socket needs a private parent")?)?;
    let socket=parent.join(socket.file_name().context("socket requires filename")?);
    let lock=fs::OpenOptions::new().create(true).read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(parent.join("bridge.lock"))?;
    if unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{bail!("provider bridge already running");}
    if let Ok(meta)=fs::symlink_metadata(&socket){if !meta.file_type().is_socket(){bail!("refusing to replace a non-socket bridge path");}fs::remove_file(&socket)?;}
    let staging_fd=Arc::new(open_staging(&staging)?);let listener=UnixListener::bind(&socket)?;fs::set_permissions(&socket,fs::Permissions::from_mode(0o600))?;
    let endpoint=parent.join("bridge-endpoint.json");
    let tcp=if development_tcp{Some(TcpListener::bind(("127.0.0.1",0)).await?)}else{None};
    let token=format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple());
    if let Some(tcp)=&tcp{state::atomic_json(&endpoint,&json!({"transport":"tcp","host":"127.0.0.1","port":tcp.local_addr()?.port(),"token":token}))?;}
    let coordinator=Arc::new(tokio::sync::Mutex::new(()));
    let backends=Arc::new(tokio::sync::Mutex::new(IndexBackends::new()));
    let mut smart_tick=tokio::time::interval(std::time::Duration::from_millis(500));
    loop{tokio::select!{
        _=tokio::signal::ctrl_c()=>break,
        _=smart_tick.tick()=>{let home=home.clone();let staging=staging.clone();let fd=staging_fd.clone();let coordinator=coordinator.clone();let backends=backends.clone();tokio::spawn(async move{if let Ok(_guard)=coordinator.try_lock(){let mut backends=backends.lock().await;let _=background_once(&home,&staging,fd.as_ref(),&mut backends).await;}});},
        accepted=listener.accept()=>{let (stream,_)=accepted?;if stream.peer_cred()?.uid()!=unsafe{libc::geteuid()}{continue;}let home=home.clone();let staging=staging.clone();let staging_fd=staging_fd.clone();let coordinator=coordinator.clone();tokio::spawn(async move{let _=connection(stream,&home,&staging,staging_fd.as_ref(),None,coordinator).await;});}
        accepted=async{if let Some(listener)=&tcp{listener.accept().await}else{std::future::pending().await}}=>{let (stream,address)=accepted?;if !address.ip().is_loopback(){continue;}let home=home.clone();let staging=staging.clone();let staging_fd=staging_fd.clone();let token=token.clone();let coordinator=coordinator.clone();tokio::spawn(async move{let _=connection(stream,&home,&staging,staging_fd.as_ref(),Some(&token),coordinator).await;});}
    }}
    if development_tcp{let _=fs::remove_file(endpoint);}
    fs::remove_file(socket)?;drop(lock);Ok(())
}
async fn connection<S:tokio::io::AsyncRead+tokio::io::AsyncWrite+Unpin>(stream:S,home:&Path,staging:&Path,staging_fd:&File,token:Option<&str>,coordinator:Arc<tokio::sync::Mutex<()>>)->Result<()> {
    let (read,mut write)=tokio::io::split(stream);let mut reader=BufReader::new(read);let mut bytes=Vec::new();
    let mut limited=(&mut reader).take(1024*1024);
    let count=tokio::time::timeout(std::time::Duration::from_secs(30),limited.read_until(b'\n',&mut bytes)).await??;drop(limited);
    if count==0||bytes.last()!=Some(&b'\n'){bail!("invalid or oversized provider request");}
    let req:Request=match serde_json::from_slice(&bytes){Ok(req)=>req,Err(e)=>{let msg=error(e.into());write.write_all(format!("{msg}\n").as_bytes()).await?;return Ok(());}};
    if let Some(expected)=token {
        let supplied=req.auth_token.as_deref().unwrap_or("");
        let difference=supplied.bytes().zip(expected.bytes()).fold(0u8,|acc,(a,b)|acc|(a^b));
        if supplied.len()!=expected.len()||difference!=0{write.write_all(b"{\"ok\":false,\"error\":{\"code\":\"not_authenticated\",\"message\":\"invalid local bridge authorization\"}}\n").await?;return Ok(());}
    }
    let label=req.request_id.clone().unwrap_or_else(||uuid::Uuid::new_v4().to_string());let ctx=crate::transfer::global().scoped(label.clone());
    // File Provider issues concurrent callbacks. Queue private-state operations
    // asynchronously instead of making our own requests fail Store's file lock.
    let operation=async{let _guard=if ["domains","status","resolve","cache_budget"].contains(&req.op.as_str())||is_smart_catalog(home,&req){None}else{Some(coordinator.lock().await)};process(home,staging,staging_fd,req,ctx.clone()).await};tokio::pin!(operation);let mut tick=tokio::time::interval(std::time::Duration::from_millis(250));let mut closed=[0u8;1];
    let result=loop{tokio::select!{
        result=&mut operation=>break result,
        read=reader.read(&mut closed)=>{let _=read;return Ok(());},
        _=tick.tick()=>{let snapshot=ctx.snapshot();let jobs=snapshot.jobs.iter().filter(|j|j.label.as_deref()==Some(&label)).collect::<Vec<_>>();let total=jobs.iter().filter_map(|j|j.total).sum::<u64>();let completed=jobs.iter().map(|j|j.bytes).sum::<u64>();if total>0{write.write_all(format!("{}\n",json!({"progress":{"completed":completed,"total":total}})).as_bytes()).await?;}}
    }};
    let reply=match result{Ok(value)=>success(value),Err(e)=>error(e)};write.write_all(format!("{reply}\n").as_bytes()).await?;write.shutdown().await?;Ok(())
}

#[cfg(test)]mod tests {
 use super::*;use std::os::unix::fs::{symlink,PermissionsExt};
 #[test]
 fn own_write_chaining_follows_intervening_metadata_receipts_and_rejects_external_versions(){
    let temp=tempfile::tempdir().unwrap();let home=temp.path().join("state");let mut store=Store::open(home.clone()).unwrap();
    store.config.profiles.insert("p".into(),config::Profile{protocol:config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:None,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();
    let d=Domain{id:"smart".into(),profile:"p".into(),remote:String::new(),display_name:"Smart".into(),repository:false,smart:true,local_root:None};add_domain(&store,d.clone()).unwrap();let mut j=checked_journal(&home,&store.config,&d).unwrap();
    let mut item=root_item(&d);item.id="doc".into();item.name="doc".into();item.kind="file".into();item.content_version="c0".into();item.metadata_version="m2:current-status".into();j.items.insert("doc".into(),Record{path:"doc".into(),item:item.clone(),entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0});save_journal(&store,&d.id,&j).unwrap();
    let mut q=crate::smart::Queue::open(&store,&d.id,&journal_binding(&store.config,&d).unwrap()).unwrap();
    for (id,base,result) in [("a","m0","m1"),("b","m1","m2")] {
        let mutation=q.enqueue(json!({"v":1,"op":"modify","domain":"smart","id":"doc","request_id":id,"base_content_version":"c0","base_metadata_version":base}),None).unwrap();q.start(&mutation.id).unwrap();let mut response=item.clone();response.metadata_version=format!("{result}:confirmed");q.complete(&mutation.id,json!({"item":response})).unwrap();
    }
    let next=q.enqueue(json!({"v":1,"op":"modify","domain":"smart","id":"doc","request_id":"next","base_content_version":"c0","base_metadata_version":"m0"}),None).unwrap();
    let (planned,_)=chained_request(&store,&d,&q,&next).unwrap();assert_eq!(planned.base_metadata_version.as_deref(),Some("m2:current-status"));
    j=checked_journal(&home,&store.config,&d).unwrap();j.items.get_mut("doc").unwrap().item.metadata_version="external:status".into();save_journal(&store,&d.id,&j).unwrap();
    let other=q.enqueue(json!({"v":1,"op":"modify","domain":"smart","id":"doc","request_id":"later","base_content_version":"c0","base_metadata_version":"m0"}),None).unwrap();let (planned,_)=chained_request(&store,&d,&q,&other).unwrap();assert_eq!(planned.base_metadata_version.as_deref(),Some("m0"));assert!(check_base(&planned,&j.items["doc"].item).is_err());
 }
 #[test]
 fn staged_lease_copy_reads_the_verified_fd_after_its_path_is_replaced(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let stage=root.join("stage");fs::create_dir(&stage).unwrap();
    let source=root.join("source");let other=root.join("other");fs::write(&source,b"verified original").unwrap();fs::write(&other,b"different private bytes").unwrap();let file=File::open(&source).unwrap();fs::remove_file(&source).unwrap();symlink(&other,&source).unwrap();
    let delivered=staging_output_file(&stage,&open_staging(&stage).unwrap(),&file).unwrap();assert_eq!(fs::read(delivered).unwrap(),b"verified original");
 }
 #[test]
 fn smart_request_identity_ignores_transport_and_status_but_binds_content(){
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("source");fs::write(&path,b"first").unwrap();
    let req:Request=serde_json::from_value(json!({"v":1,"op":"modify","domain":"smart","id":"file","request_id":"transport-a","base_content_version":"revision","base_metadata_version":"protected:online","content_path":path,"auth_token":"private"})).unwrap();
    let first=stable_mutation(&req,Some(&File::open(&path).unwrap())).unwrap();
    let mut second=req.clone();second.request_id=Some("transport-b".into());second.base_metadata_version=Some("protected:queued".into());
    let retry=stable_mutation(&second,Some(&File::open(&path).unwrap())).unwrap();assert_eq!(first.request_id,retry.request_id);assert!(retry.content_path.is_none()&&retry.auth_token.is_none());
    fs::write(&path,b"later").unwrap();let changed=stable_mutation(&second,Some(&File::open(&path).unwrap())).unwrap();assert_ne!(first.request_id,changed.request_id);
 }
 #[test]
 fn recursive_pin_keeps_explicit_descendant_choices(){
    let d=Domain{id:"x".into(),profile:"x".into(),remote:String::new(),display_name:"x".into(),repository:false,smart:true,local_root:None};
    let mut parent=root_item(&d);parent.id="folder".into();let mut child=parent.clone();child.id="child".into();child.parent_id="folder".into();
    let records=BTreeMap::from([("folder".into(),Record{path:"folder".into(),item:parent,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0}),("child".into(),Record{path:"folder/child".into(),item:child,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0})]);
    let mut j=Journal::default();j.pins.insert("folder".into());assert!(explicit_pin(&j,"child",&records));j.pins.insert("child".into());j.pins.remove("folder");assert!(explicit_pin(&j,"child",&records));assert!(!explicit_pin(&j,"folder",&records));j.pins.insert("root".into());assert!(explicit_pin(&j,"folder",&records));
 }
 #[test]
 fn transient_sync_badges_do_not_invalidate_remote_metadata_guards(){
    let d=Domain{id:"x".into(),profile:"x".into(),remote:String::new(),display_name:"x".into(),repository:false,smart:true,local_root:None};let mut item=root_item(&d);item.metadata_version="protected:queued".into();
    let mut req:Request=serde_json::from_value(json!({"v":1,"op":"modify","base_content_version":"root","base_metadata_version":"protected:online"})).unwrap();assert!(check_base(&req,&item).is_ok());req.base_metadata_version=Some("other:online".into());assert!(check_base(&req,&item).is_err());
 }
 #[tokio::test]
 async fn rebound_domain_cannot_reuse_item_versions_or_successful_replay_receipts(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let home=root.join("state");
    let mut store=Store::open(home.clone()).unwrap();store.config.profiles.insert("profile".into(),config::Profile{protocol:config::Protocol::Webdav,url:"https://old.example.invalid/".into(),username:Some("test".into()),password_env:None,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();
    let visible=root.join("visible");fs::create_dir(&visible).unwrap();
    let domain=Domain{id:"bound".into(),profile:"profile".into(),remote:"original".into(),display_name:"Bound".into(),repository:false,smart:false,local_root:Some(visible.clone())};add_domain(&store,domain.clone()).unwrap();
    let mut journal=checked_journal(&home,&store.config,&domain).unwrap();let mut item=root_item(&domain);item.id="stable-item".into();item.name="document".into();item.kind="file".into();
    journal.items.insert(item.id.clone(),Record{path:"document".into(),item,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0});
    journal.requests.insert("old-success".into(),Pending{fingerprint:"request".into(),metadata:None,source_fingerprint:None,transaction:None,confirmed:None,response:Some(json!({"item":"old-backend-success"})),deletion_path:None,deletion_expected:None,metadata_base_fingerprint:None});save_journal(&store,&domain.id,&journal).unwrap();
    let original_binding=journal.binding.clone().unwrap();
    let before=serde_json::to_value(&journal).unwrap();
    let status:Request=serde_json::from_value(json!({"v":1,"op":"status","domain":"bound","id":"stable-item"})).unwrap();assert!(quick(&home,&status).is_ok());
    let mut moved=domain.clone();moved.remote="different-root".into();assert!(checked_journal(&home,&store.config,&moved).is_err());
    store.config.profiles.get_mut("profile").unwrap().url="https://new.example.invalid/".into();store.save_config().unwrap();drop(store);
    assert!(quick(&home,&status).unwrap_err().to_string().contains("scope changed"));
    let resolve:Request=serde_json::from_value(json!({"v":1,"op":"resolve","file_url":url::Url::from_file_path(visible.join("document")).unwrap().to_string()})).unwrap();assert!(quick(&home,&resolve).is_err());
    let pin:Request=serde_json::from_value(json!({"v":1,"op":"pin","domain":"bound","id":"stable-item","pinned":true})).unwrap();assert!(local_pin(&home,&pin).is_err());
    let stage=root.join("stage");fs::create_dir(&stage).unwrap();let fd=open_staging(&stage).unwrap();
    let request:Request=serde_json::from_value(json!({"v":1,"op":"list","domain":"bound"})).unwrap();let error=process(&home,&stage,&fd,request,Default::default()).await.unwrap_err();assert!(error.to_string().contains("scope changed"),"scope must fail before connecting to the newly configured backend: {error:#}");
    let unchanged=index::load(&home,"bound",&original_binding,&home.join("provider").join(format!("{}.json",state::key("bound")))).unwrap().unwrap();
    assert_eq!(serde_json::to_value(unchanged).unwrap(),before);
 }
 #[test]
 fn populated_legacy_unbound_journal_is_preserved_and_rejected(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let home=root.join("state");let mut store=Store::open(home.clone()).unwrap();
    store.config.profiles.insert("profile".into(),config::Profile{protocol:config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:None,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});
    let domain=Domain{id:"legacy".into(),profile:"profile".into(),remote:String::new(),display_name:"Legacy".into(),repository:false,smart:false,local_root:None};let path=journal_path(&store,&domain.id);
    state::atomic_json(&path,&json!({"items":{},"anchor":1,"events":[],"requests":{}})).unwrap();let before=fs::read(&path).unwrap();assert!(checked_journal(&home,&store.config,&domain).is_err());assert_eq!(fs::read(path).unwrap(),before);
 }
 #[test]fn source_fd_never_follows_swapped_directory_or_leaf_symlink(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let stage=root.join("stage");fs::create_dir(&stage).unwrap();fs::write(stage.join("file"),b"owned stage").unwrap();let fd=open_staging(&stage).unwrap();
    let outside=root.join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("file"),b"private secret").unwrap();symlink(outside.join("file"),stage.join("link")).unwrap();assert!(staging_file(&stage,&fd,&stage.join("link")).is_err());
    fs::rename(&stage,root.join("old")).unwrap();fs::create_dir(&stage).unwrap();fs::write(stage.join("file"),b"replacement folder").unwrap();
    let mut file=staging_file(&stage,&fd,&stage.join("file")).unwrap();let mut bytes=Vec::new();std::io::Read::read_to_end(&mut file,&mut bytes).unwrap();assert_eq!(bytes,b"owned stage");
    assert!(staging_output(&stage,&fd,&outside.join("file")).is_err());assert_eq!(fs::read(stage.join("file")).unwrap(),b"replacement folder");
 }
 #[test]fn apple_tmp_alias_resolves_to_the_pinned_parent(){let temp=tempfile::Builder::new().prefix("rdrive-bridge-").tempdir_in("/private/tmp").unwrap();let root=temp.path().canonicalize().unwrap();let stage=root.join("Transfers");fs::create_dir(&stage).unwrap();fs::write(stage.join("x"),b"data").unwrap();let fd=open_staging(&stage).unwrap();let alias=PathBuf::from(stage.join("x").to_str().unwrap().replacen("/private/tmp/","/tmp/",1));assert!(staging_file(&stage,&fd,&alias).is_ok());}
 #[test]fn tag_clear_removes_attribute_and_staging_copy_keeps_user_xattrs(){
    use std::os::fd::AsRawFd;let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let stage=root.join("stage");fs::create_dir(&stage).unwrap();fs::set_permissions(&stage,fs::Permissions::from_mode(0o700)).unwrap();let source=root.join("source");fs::write(&source,b"data").unwrap();
    let file=File::open(&source).unwrap();let name=std::ffi::CString::new("user.rdrive-test").unwrap();let value=b"attribute";let result=unsafe{libc::fsetxattr(file.as_raw_fd(),name.as_ptr(),value.as_ptr().cast(),value.len(),0,0)};assert_eq!(result,0);
    let mut metadata=crate::metadata::capture(&source,&Default::default()).unwrap();metadata.attributes.insert("com.apple.metadata:_kMDItemUserTags".into(),"00".into());let cleared=apply_tags(metadata,Some("")).unwrap();assert!(!cleared.attributes.contains_key("com.apple.metadata:_kMDItemUserTags"));
    let delivered=staging_output(&stage,&open_staging(&stage).unwrap(),&source).unwrap();let copied=crate::metadata::capture(&delivered,&Default::default()).unwrap();assert_eq!(copied.attributes.get("user.rdrive-test"),Some(&"617474726962757465".to_owned()));
 }
}

#[cfg(test)]
#[path = "provider_scopes_tests.rs"]
mod scope_tests;
