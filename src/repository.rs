//! Snapshot transactions: immutable blobs and append-only, atomically published
//! commit slots. A concurrent writer can win a slot, never replace its contents.
use crate::{backend::{self,Backend,WriteCondition},state::{self,Store}};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use std::{collections::{BTreeMap,BTreeSet},fs,path::{Path,PathBuf}};
const CONTROL:&str=".rdrive-repository";
const MAX_MANIFEST:u64=64*1024*1024;

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
pub struct RepoItem {
    pub id:String,pub kind:String,pub size:u64,pub blob:Option<String>,pub modified:String,
    #[serde(default)]pub metadata:Option<serde_json::Value>,
}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Commit {pub version:u32,pub sequence:u64,pub parent:Option<String>,pub transaction:String,pub created:String,pub entries:BTreeMap<String,RepoItem>}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Snapshot {pub checksum:String,pub commit:Commit}
impl Snapshot {
    pub fn empty()->Self{Self{checksum:String::new(),commit:Commit{version:1,sequence:0,parent:None,transaction:String::new(),created:String::new(),entries:BTreeMap::new()}}}
    pub fn validate(&self)->Result<()> {
        if self.commit.version!=1||self.commit.sequence==0||!hash_valid(&self.checksum)||self.checksum!=state::key(&serde_json::to_string(&self.commit)?) {bail!("invalid repository commit checksum/format");}
        uuid::Uuid::parse_str(&self.commit.transaction)?;
        if self.commit.sequence==1&&self.commit.parent.is_some()||self.commit.sequence>1&&self.commit.parent.as_ref().is_none_or(|v|!hash_valid(v)){bail!("invalid repository commit parent");}
        validate_tree(&self.commit.entries)
    }
}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Transaction {
    pub version:u32,pub id:String,pub profile:String,pub scope:String,pub root:String,
    pub base_sequence:u64,pub base_checksum:String,pub entries:BTreeMap<String,RepoItem>,
    pub snapshots:BTreeMap<String,String>,pub phase:String,pub commit:Option<Snapshot>,
}
fn hash_valid(s:&str)->bool{s.len()==64&&s.bytes().all(|v|v.is_ascii_hexdigit())}
fn logical(path:&str)->Result<String>{let p=backend::normalize(path)?;if p.is_empty()||p!=path||p.chars().any(|c|c.is_control()){bail!("repository paths must be nonempty canonical relative paths");}Ok(p)}
fn slot(root:&str,sequence:u64)->Result<String>{backend::join(root,&format!("{CONTROL}/commits/{sequence:020}.json"))}
pub fn object_path(root:&str,hash:&str)->Result<String>{if !hash_valid(hash){bail!("invalid repository blob hash");}backend::join(root,&format!("{CONTROL}/objects/{}/{hash}",&hash[..2]))}
fn tx_path(store:&Store,id:&str)->Result<PathBuf>{uuid::Uuid::parse_str(id)?;Ok(store.home.join("transactions").join(format!("{id}.json")))}
fn scope(store:&Store,profile:&str)->Result<String>{let p=store.config.profiles.get(profile).context("unknown profile")?;Ok(state::key(&format!("{profile}\0{}\0{}",p.url,p.username.as_deref().unwrap_or(""))))}
fn validate_tree(tree:&BTreeMap<String,RepoItem>)->Result<()> {
    let mut ids=BTreeSet::new();let mut folded=BTreeMap::new();
    for (path,item) in tree {
        logical(path)?;uuid::Uuid::parse_str(&item.id)?;
        if !ids.insert(&item.id){bail!("duplicate stable repository item identity");}
        let key=path.to_lowercase();if let Some(other)=folded.insert(key,path){if other!=path{bail!("case collision in repository");}}
        match item.kind.as_str(){"file"=>if item.blob.as_ref().is_none_or(|h|!hash_valid(h)){bail!("file lacks a content hash");},"directory"=>if item.blob.is_some()||item.size!=0{bail!("invalid directory entry");},_=>bail!("invalid repository item kind")}
        if let Some(value)=&item.metadata{let record:crate::metadata::MetadataRecord=serde_json::from_value(value.clone())?;record.validate()?;if Some(&record.content_sha256)!=item.blob.as_ref(){bail!("repository metadata is not bound to its content");}}
        if let Some((parent,_))=path.rsplit_once('/') {if tree.get(parent).is_none_or(|p|p.kind!="directory"){bail!("repository parent is missing or not a directory: {parent}");}}
    }crate::sync::validate_path_collisions(tree.keys())?;Ok(())
}
async fn json_file<T:serde::de::DeserializeOwned>(store:&Store,b:&dyn Backend,remote:&str)->Result<T>{
    let e=b.stat(remote).await?.context("repository record missing")?;if e.is_dir||e.size>MAX_MANIFEST{bail!("repository record exceeds allowed size");}
    let temp=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());
    let result=async{crate::operations::verified_download(b,remote,&temp).await?;Ok(serde_json::from_slice::<T>(&fs::read(&temp)?)?)}.await;
    let _=fs::remove_file(temp);result
}
pub async fn init(store:&Store,b:&dyn Backend,root:&str)->Result<Snapshot>{
    let root=backend::normalize(root)?;
    if b.stat(&root).await?.is_none_or(|e|!e.is_dir){bail!("repository root must already be a remote directory");}
    for dir in [format!("{CONTROL}/commits"),format!("{CONTROL}/objects"),format!("{CONTROL}/staging")] {backend::mkdir_all(b,&backend::join(&root,&dir)?).await?;}
    let format_path=backend::join(&root,&format!("{CONTROL}/FORMAT.json"))?;
    let format=serde_json::json!({"format":"rdrive-snapshot-repository","version":1,"publication":"append-only-atomic-rename"});
    if b.stat(&format_path).await?.is_none(){
        let temp=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());state::atomic_json(&temp,&format)?;
        let stage=backend::join(&root,&format!("{CONTROL}/staging/{}",uuid::Uuid::new_v4()))?;
        let result=async{b.upload(&temp,&stage,WriteCondition::CreateNew).await?;let checked:serde_json::Value=json_file(store,b,&stage).await?;if checked!=format{bail!("repository format staging mismatch");}b.rename(&stage,&format_path,false).await}.await;let _=fs::remove_file(temp);
        if result.is_err()&&b.stat(&format_path).await?.is_none(){result?;}
    }
    let observed:serde_json::Value=json_file(store,b,&format_path).await?;if observed!=format{bail!("incompatible repository format");}
    read(store,b,&root).await
}
pub async fn read(store:&Store,b:&dyn Backend,root:&str)->Result<Snapshot>{
    let observed:serde_json::Value=json_file(store,b,&backend::join(root,&format!("{CONTROL}/FORMAT.json"))?).await?;
    if observed!=serde_json::json!({"format":"rdrive-snapshot-repository","version":1,"publication":"append-only-atomic-rename"}){bail!("invalid repository format");}
    let dir=backend::join(&backend::normalize(root)?,&format!("{CONTROL}/commits"))?;
    let mut files=b.list(&dir).await?;files.sort_by(|a,b|a.path.cmp(&b.path));
    let mut current=Snapshot::empty();
    for e in files {
        let name=e.path.rsplit('/').next().context("invalid commit name")?;
        if e.is_dir||!name.ends_with(".json")||name.len()!=25||!name.is_ascii()||!name[..20].bytes().all(|c|c.is_ascii_digit()){bail!("unexpected entry in immutable commit directory");}
        let sequence=name[..20].parse::<u64>()?;if sequence!=current.commit.sequence+1{bail!("repository commit chain contains a missing/duplicate slot");}
        let next:Snapshot=json_file(store,b,&e.path).await?;next.validate()?;
        let expected_parent=if sequence==1{None}else{Some(current.checksum.clone())};
        if next.commit.sequence!=sequence||next.commit.parent!=expected_parent{bail!("repository history is not one valid parent chain");}
        current=next;
    }Ok(current)
}
pub async fn begin(store:&Store,b:&dyn Backend,profile:&str,root:&str)->Result<Transaction>{
    let root=backend::normalize(root)?;let current=read(store,b,&root).await?;
    begin_from(store,profile,&root,current)
}
/// Bind edits to the exact snapshot that supplied the caller's version guards.
pub fn begin_from(store:&Store,profile:&str,root:&str,current:Snapshot)->Result<Transaction>{
    let root=backend::normalize(root)?;
    if current.commit.sequence>0{current.validate()?;}else if current.checksum!=""||!current.commit.entries.is_empty(){bail!("invalid empty repository snapshot");}
    let tx=Transaction{version:1,id:uuid::Uuid::new_v4().to_string(),profile:profile.into(),scope:scope(store,profile)?,root,base_sequence:current.commit.sequence,base_checksum:current.checksum,entries:current.commit.entries,snapshots:BTreeMap::new(),phase:"open".into(),commit:None};
    state::atomic_json(&tx_path(store,&tx.id)?,&tx)?;Ok(tx)
}
pub fn load(store:&Store,id:&str)->Result<Transaction>{
    let tx:Transaction=serde_json::from_slice(&fs::read(tx_path(store,id)?)?)?;
    if tx.version!=1||tx.id!=id||scope(store,&tx.profile)?!=tx.scope||backend::normalize(&tx.root)?!=tx.root||!["open","prepared","committed","aborted"].contains(&tx.phase.as_str()){bail!("invalid or re-scoped transaction journal");}
    validate_tree(&tx.entries)?;
    for (hash,name) in &tx.snapshots {if !hash_valid(hash)||name!=&format!("txn-{}-{hash}",tx.id){bail!("invalid transaction source snapshot");}}
    if let Some(commit)=&tx.commit{commit.validate()?;if commit.commit.transaction!=tx.id||commit.commit.entries!=tx.entries{bail!("transaction manifest changed after preparation");}}
    Ok(tx)
}
fn mutable(tx:&Transaction)->Result<()>{if tx.phase!="open"{bail!("only an open transaction may be edited");}Ok(())}
pub fn put(store:&Store,id:&str,local:&Path,remote:&str)->Result<Transaction>{
    let mut tx=load(store,id)?;mutable(&tx)?;let remote=logical(remote)?;
    if tx.entries.get(&remote).is_some_and(|i|i.kind!="file"){bail!("cannot replace a directory with a file");}
    let hash=state::fingerprint(local)?;let name=format!("txn-{}-{hash}",tx.id);let dest=store.home.join("spool").join(&name);
    if !dest.exists(){let (observed,_)=state::snapshot(local,&dest)?;if observed!=hash{bail!("source changed during transaction snapshot");}}
    if state::fingerprint(&dest)?!=hash{bail!("transaction source snapshot is corrupt");}
    let metadata=crate::metadata::capture(local,&crate::metadata::MetadataPolicy::default())?;
    let item=RepoItem{id:tx.entries.get(&remote).map(|i|i.id.clone()).unwrap_or_else(||uuid::Uuid::new_v4().to_string()),kind:"file".into(),size:fs::metadata(&dest)?.len(),blob:Some(hash.clone()),modified:state::now(),metadata:Some(serde_json::to_value(metadata)?)};
    tx.entries.insert(remote,item);tx.snapshots.insert(hash,name);validate_tree(&tx.entries)?;state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)
}
pub fn mkdir(store:&Store,id:&str,remote:&str)->Result<Transaction>{let mut tx=load(store,id)?;mutable(&tx)?;let path=logical(remote)?;if tx.entries.contains_key(&path){bail!("repository path exists");}tx.entries.insert(path,RepoItem{id:uuid::Uuid::new_v4().to_string(),kind:"directory".into(),size:0,blob:None,modified:state::now(),metadata:None});validate_tree(&tx.entries)?;state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)}
pub fn remove(store:&Store,id:&str,remote:&str,recursive:bool)->Result<Transaction>{let mut tx=load(store,id)?;mutable(&tx)?;let path=logical(remote)?;if !tx.entries.contains_key(&path){bail!("repository path does not exist");}let prefix=format!("{path}/");if !recursive&&tx.entries.keys().any(|p|p.starts_with(&prefix)){bail!("directory has children; recursive removal required");}tx.entries.retain(|p,_|p!=&path&&!p.starts_with(&prefix));state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)}
pub fn rename(store:&Store,id:&str,from:&str,to:&str)->Result<Transaction>{let mut tx=load(store,id)?;mutable(&tx)?;let from=logical(from)?;let to=logical(to)?;if to.starts_with(&format!("{from}/"))||tx.entries.contains_key(&to)||!tx.entries.contains_key(&from){bail!("invalid repository move");}let moving=tx.entries.iter().filter(|(p,_)|*p==&from||p.starts_with(&format!("{from}/"))).map(|(p,i)|(p.clone(),i.clone())).collect::<Vec<_>>();for(p,_)in&moving{tx.entries.remove(p);}for(p,i)in moving{tx.entries.insert(format!("{to}{}",&p[from.len()..]),i);}validate_tree(&tx.entries)?;state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)}
pub fn set_metadata(store:&Store,id:&str,path:&str,metadata:serde_json::Value)->Result<Transaction>{let mut tx=load(store,id)?;mutable(&tx)?;let item=tx.entries.get_mut(&logical(path)?).context("item missing")?;let record:crate::metadata::MetadataRecord=serde_json::from_value(metadata.clone())?;record.validate()?;if Some(&record.content_sha256)!=item.blob.as_ref(){bail!("metadata content binding mismatch");}item.metadata=Some(metadata);state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)}
async fn verify_blob(store:&Store,b:&dyn Backend,path:&str,hash:&str,size:u64)->Result<()> {
    let temp=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());
    let result=async{let e=crate::operations::verified_download(b,path,&temp).await?;if e.size!=size||state::fingerprint(&temp)?!=hash{bail!("repository object is corrupt or does not match its content address");}Ok(())}.await;let _=fs::remove_file(temp);result
}
pub async fn commit(store:&Store,b:&dyn Backend,id:&str)->Result<Snapshot>{
    let mut tx=load(store,id)?;if tx.phase=="aborted"{bail!("transaction was aborted");}
    let current=read(store,b,&tx.root).await?;
    if let Some(saved)=&tx.commit {
        if let Some(observed)=find_transaction(store,b,&tx.root,&tx.id).await? {if observed.checksum!=saved.checksum{bail!("transaction identity collision");}tx.phase="committed".into();state::atomic_json(&tx_path(store,id)?,&tx)?;return Ok(observed);}
    }
    if tx.phase=="committed"{bail!("committed transaction slot was removed from server");}
    if current.commit.sequence!=tx.base_sequence||current.checksum!=tx.base_checksum{bail!("repository commit conflict: another writer committed first; prepared data retained");}
    if tx.commit.is_none(){let body=Commit{version:1,sequence:tx.base_sequence.checked_add(1).context("commit sequence overflow")?,parent:(!tx.base_checksum.is_empty()).then_some(tx.base_checksum.clone()),transaction:tx.id.clone(),created:state::now(),entries:tx.entries.clone()};let snap=Snapshot{checksum:state::key(&serde_json::to_string(&body)?),commit:body};snap.validate()?;tx.commit=Some(snap);tx.phase="prepared".into();state::atomic_json(&tx_path(store,id)?,&tx)?;}
    for item in tx.entries.values().filter(|e|e.kind=="file") {
        let hash=item.blob.as_deref().context("file lacks blob")?;let target=object_path(&tx.root,hash)?;
        if b.stat(&target).await?.is_some(){verify_blob(store,b,&target,hash,item.size).await?;continue;}
        let name=tx.snapshots.get(hash).context("required immutable object missing and no source snapshot available")?;let source=store.home.join("spool").join(name);
        if state::fingerprint(&source)?!=hash{bail!("local immutable transaction snapshot changed");}
        backend::mkdir_all(b,target.rsplit_once('/').context("invalid blob path")?.0).await?;
        let stage=backend::join(&tx.root,&format!("{CONTROL}/staging/{}",uuid::Uuid::new_v4()))?;
        b.upload(&source,&stage,WriteCondition::CreateNew).await?;verify_blob(store,b,&stage,hash,item.size).await?;
        if let Err(error)=b.rename(&stage,&target,false).await {if b.stat(&target).await?.is_none(){return Err(error);}verify_blob(store,b,&target,hash,item.size).await?;}
    }
    let prepared=tx.commit.as_ref().context("prepared manifest missing")?;
    let temp=store.home.join("tmp").join(uuid::Uuid::new_v4().to_string());state::atomic_json(&temp,prepared)?;
    let stage=backend::join(&tx.root,&format!("{CONTROL}/staging/{}",uuid::Uuid::new_v4()))?;
    let staged=b.upload(&temp,&stage,WriteCondition::CreateNew).await;let _=fs::remove_file(temp);staged?;
    let check:Snapshot=json_file(store,b,&stage).await?;check.validate()?;if check.checksum!=prepared.checksum{bail!("staged commit checksum mismatch");}
    let publish=b.rename(&stage,&slot(&tx.root,prepared.commit.sequence)?,false).await;
    // Even an acknowledged-lost rename is reconciled by its immutable slot.
    let observed:Option<Snapshot>=match b.stat(&slot(&tx.root,prepared.commit.sequence)?).await?{None=>None,Some(_)=>Some(json_file(store,b,&slot(&tx.root,prepared.commit.sequence)?).await?)};
    if let Some(observed)=&observed{observed.validate().context("published repository commit failed checksum/format validation")?;}
    if observed.as_ref().is_none_or(|s|s.checksum!=prepared.checksum){if let Err(error)=publish{return Err(error.context("atomic commit not confirmed; transaction journal retained"));}bail!("another transaction won the immutable commit slot");}
    let result=prepared.clone();tx.phase="committed".into();state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(result)
}
async fn find_transaction(store:&Store,b:&dyn Backend,root:&str,id:&str)->Result<Option<Snapshot>>{let current=read(store,b,root).await?;if current.commit.transaction==id{return Ok(Some(current));}for n in 1..current.commit.sequence{let c:Snapshot=json_file(store,b,&slot(root,n)?).await?;c.validate()?;if c.commit.transaction==id{return Ok(Some(c));}}Ok(None)}
pub async fn get(store:&Store,b:&dyn Backend,root:&str,logical_path:&str,dest:&Path,overwrite:bool)->Result<RepoItem>{let snapshot=read(store,b,root).await?;get_from(store,b,root,&snapshot,logical_path,dest,overwrite).await}
pub async fn get_from(store:&Store,b:&dyn Backend,root:&str,snapshot:&Snapshot,logical_path:&str,dest:&Path,overwrite:bool)->Result<RepoItem>{
    if snapshot.commit.sequence>0{snapshot.validate()?;}let item=snapshot.commit.entries.get(&logical(logical_path)?).context("repository file missing")?.clone();if item.kind!="file"{bail!("expected a repository file");}
    let temp=crate::operations::destination_temp(dest)?;let result=async{crate::operations::verified_download(b,&object_path(root,item.blob.as_deref().context("missing blob")?)?,&temp).await?;if state::fingerprint(&temp)?!=item.blob.as_deref().unwrap(){bail!("repository object checksum mismatch");}if let Some(value)=&item.metadata{let record:crate::metadata::MetadataRecord=serde_json::from_value(value.clone())?;crate::metadata::apply(&temp,&record,&crate::metadata::MetadataPolicy::default())?;}state::commit_download(&temp,dest,overwrite)?;Ok(item)}.await;let _=fs::remove_file(temp);let _=store;result
}
pub fn abort(store:&Store,id:&str)->Result<Transaction>{let mut tx=load(store,id)?;if tx.phase!="open"{bail!("only an open transaction can be aborted; a prepared commit may already be visible, so reconcile it with repo commit");}tx.phase="aborted".into();state::atomic_json(&tx_path(store,id)?,&tx)?;Ok(tx)}
pub fn transactions(store:&Store)->Result<Vec<Transaction>>{let mut result=Vec::new();for entry in fs::read_dir(store.home.join("transactions"))?{let e=entry?;if e.path().extension().is_some_and(|x|x=="json"){let id=e.path().file_stem().and_then(|x|x.to_str()).context("invalid transaction journal filename")?.to_owned();result.push(load(store,&id)?);}}Ok(result)}

#[cfg(test)]
mod tests {
 use super::*;use crate::{backend::{Capabilities,Entry},config::{Profile,Protocol}};use async_trait::async_trait;use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
 struct Disk {root:PathBuf,fail:AtomicBool,lost:AtomicBool,corrupt:AtomicBool,pause:AtomicBool,fail_published:AtomicBool,published:tokio::sync::Notify,barrier:std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>}
 impl Disk {fn p(&self,p:&str)->Result<PathBuf>{Ok(self.root.join(backend::normalize(p)?))}fn e(&self,p:&str)->Result<Option<Entry>>{let path=self.p(p)?;match fs::metadata(&path){Ok(m)=>Ok(Some(Entry{path:p.into(),is_dir:m.is_dir(),size:if m.is_dir(){0}else{m.len()},modified:Some(1),etag:if m.is_file(){Some(format!("\"{}\"",state::fingerprint(&path)?))}else{None}})),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(e)=>Err(e.into())}}}
 #[async_trait]impl Backend for Disk {
 fn capabilities(&self)->Capabilities{Capabilities{protocol:"test",conditional_write:true,server_copy:false,locking:false,notes:vec![]}}
 async fn stat(&self,p:&str)->Result<Option<Entry>>{self.e(p)}
 async fn list(&self,p:&str)->Result<Vec<Entry>>{let mut out=vec![];for e in fs::read_dir(self.p(p)?)?{let e=e?;let name=e.file_name().to_str().unwrap().to_owned();let path=backend::join(p,&name)?;out.push(self.e(&path)?.unwrap());}Ok(out)}
 async fn download(&self,p:&str,d:&Path)->Result<()>{let mut src=fs::File::open(self.p(p)?)?;let mut dst=fs::OpenOptions::new().write(true).create_new(true).open(d)?;std::io::copy(&mut src,&mut dst)?;Ok(())}
 async fn upload(&self,s:&Path,p:&str,c:WriteCondition)->Result<Entry>{if p.ends_with("/published.json")&&self.fail_published.load(Ordering::SeqCst){bail!("synthetic publication marker upload failure");}let mut opts=fs::OpenOptions::new();opts.write(true);match c{WriteCondition::CreateNew=>{opts.create_new(true);},WriteCondition::Any=>{opts.create(true).truncate(true);},WriteCondition::Match(ref e)=>{if self.e(p)?.is_none_or(|a|!a.same_revision(e)){bail!("conflict");}opts.truncate(true);}}let mut dst=opts.open(self.p(p)?)?;std::io::copy(&mut fs::File::open(s)?,&mut dst)?;dst.sync_all()?;Ok(self.e(p)?.unwrap())}
 async fn mkdir(&self,p:&str)->Result<()>{fs::create_dir(self.p(p)?)?;Ok(())}
 async fn remove(&self,p:&str,recursive:bool)->Result<()>{let path=self.p(p)?;if path.is_dir(){if recursive{fs::remove_dir_all(path)?;}else{fs::remove_dir(path)?;}}else{fs::remove_file(path)?;}Ok(())}
 async fn rename(&self,f:&str,t:&str,overwrite:bool)->Result<()>{let publication=t.contains("/commits/");if publication{let barrier=self.barrier.lock().unwrap().clone();if let Some(barrier)=barrier{barrier.wait().await;}if self.fail.load(Ordering::SeqCst){bail!("injected crash before publication");}}if overwrite{fs::rename(self.p(f)?,self.p(t)?)?;}else{fs::hard_link(self.p(f)?,self.p(t)?)?;fs::remove_file(self.p(f)?)?;}if publication{if self.corrupt.load(Ordering::SeqCst){let mut record:Snapshot=serde_json::from_slice(&fs::read(self.p(t)?)?)?;record.commit.created.push_str(" changed-after-publish");state::atomic_json(&self.p(t)?,&record)?;}self.published.notify_one();if self.pause.load(Ordering::SeqCst){std::future::pending::<()>().await;}if self.lost.load(Ordering::SeqCst){bail!("injected lost commit acknowledgment");}}Ok(())}
 async fn copy(&self,_:&str,_:&str,_:bool)->Result<()>{bail!("unused")}
 }
 fn setup(root:&Path,name:&str)->Store{let mut s=Store::open(root.join(name)).unwrap();s.config.profiles.insert("test".into(),Profile{protocol:Protocol::Webdav,url:"https://example.test/".into(),username:None,password_env:None,allow_http:false,timeout_secs:10,smb_encryption:true,keychain_internet:false,nextcloud_upload_url:None});s.save_config().unwrap();s}
 fn disk(root:&Path)->Arc<Disk>{fs::create_dir(root.join("remote")).unwrap();Arc::new(Disk{root:root.join("remote"),fail:AtomicBool::new(false),lost:AtomicBool::new(false),corrupt:AtomicBool::new(false),pause:AtomicBool::new(false),fail_published:AtomicBool::new(false),published:tokio::sync::Notify::new(),barrier:std::sync::Mutex::new(None)})}
 #[tokio::test]async fn crash_before_publish_leaves_old_snapshot_and_resume_commits_whole_tree(){let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let b=disk(&root);let s=setup(&root,"state");init(&s,b.as_ref(),"").await.unwrap();let tx=begin(&s,b.as_ref(),"test","").await.unwrap();for p in ["a","b"]{fs::write(root.join(p),p).unwrap();put(&s,&tx.id,&root.join(p),p).unwrap();}b.fail.store(true,Ordering::SeqCst);assert!(commit(&s,b.as_ref(),&tx.id).await.is_err());assert!(read(&s,b.as_ref(),"").await.unwrap().commit.entries.is_empty());b.fail.store(false,Ordering::SeqCst);let c=commit(&s,b.as_ref(),&tx.id).await.unwrap();assert_eq!(c.commit.entries.len(),2);assert_eq!(read(&s,b.as_ref(),"").await.unwrap().checksum,c.checksum);}
 #[tokio::test]async fn lost_commit_response_is_reconciled_without_second_commit(){let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let b=disk(&root);let s=setup(&root,"state");init(&s,b.as_ref(),"").await.unwrap();let tx=begin(&s,b.as_ref(),"test","").await.unwrap();mkdir(&s,&tx.id,"folder").unwrap();b.lost.store(true,Ordering::SeqCst);let c=commit(&s,b.as_ref(),&tx.id).await.unwrap();let again=commit(&s,b.as_ref(),&tx.id).await.unwrap();assert_eq!(c.checksum,again.checksum);assert_eq!(read(&s,b.as_ref(),"").await.unwrap().commit.sequence,1);}
 #[tokio::test]async fn competing_writers_cannot_replace_one_anothers_commit_slot(){let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let b=disk(&root);let a=setup(&root,"a");let c=setup(&root,"c");init(&a,b.as_ref(),"").await.unwrap();let ta=begin(&a,b.as_ref(),"test","").await.unwrap();let tc=begin(&c,b.as_ref(),"test","").await.unwrap();mkdir(&a,&ta.id,"a").unwrap();mkdir(&c,&tc.id,"c").unwrap();*b.barrier.lock().unwrap()=Some(Arc::new(tokio::sync::Barrier::new(2)));let(x,y)=tokio::join!(commit(&a,b.as_ref(),&ta.id),commit(&c,b.as_ref(),&tc.id));assert_ne!(x.is_ok(),y.is_ok());assert_eq!(read(&a,b.as_ref(),"").await.unwrap().commit.sequence,1);}
 #[tokio::test]async fn actual_task_death_after_atomic_publish_recovers_prepared_journal(){let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let b=disk(&root);let s=setup(&root,"state");init(&s,b.as_ref(),"").await.unwrap();let tx=begin(&s,b.as_ref(),"test","").await.unwrap();mkdir(&s,&tx.id,"complete").unwrap();b.pause.store(true,Ordering::SeqCst);let task_b=b.clone();let id=tx.id.clone();let task=tokio::spawn(async move{commit(&s,task_b.as_ref(),&id).await});b.published.notified().await;let reader=setup(&root,"reader");assert!(read(&reader,b.as_ref(),"").await.unwrap().commit.entries.contains_key("complete"));task.abort();let _=task.await;b.pause.store(false,Ordering::SeqCst);let reopened=Store::open(root.join("state")).unwrap();let done=commit(&reopened,b.as_ref(),&tx.id).await.unwrap();assert_eq!(done.commit.sequence,1);}
 #[tokio::test]
 async fn changed_published_commit_body_cannot_be_confirmed_by_checksum_field_alone() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();
    let backend=disk(&root);let store=setup(&root,"state");init(&store,backend.as_ref(),"").await.unwrap();
    let tx=begin(&store,backend.as_ref(),"test","").await.unwrap();mkdir(&store,&tx.id,"item").unwrap();
    backend.corrupt.store(true,Ordering::SeqCst);
    let error=commit(&store,backend.as_ref(),&tx.id).await.unwrap_err();
    assert!(error.to_string().contains("published repository commit failed"));
    assert_eq!(load(&store,&tx.id).unwrap().phase,"prepared");
    assert!(read(&store,backend.as_ref(),"").await.is_err());
 }
 #[tokio::test]
 async fn pinned_transaction_cannot_overwrite_a_commit_after_enumeration() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();
    let backend=disk(&root);let reader=setup(&root,"reader");let writer=setup(&root,"writer");
    init(&reader,backend.as_ref(),"").await.unwrap();
    let input=root.join("input");fs::write(&input,b"enumerated revision A").unwrap();
    let initial=begin(&reader,backend.as_ref(),"test","").await.unwrap();
    put(&reader,&initial.id,&input,"document").unwrap();commit(&reader,backend.as_ref(),&initial.id).await.unwrap();
    let enumerated=read(&reader,backend.as_ref(),"").await.unwrap();
    let intervening=begin(&writer,backend.as_ref(),"test","").await.unwrap();
    fs::write(&input,b"concurrent revision B").unwrap();put(&writer,&intervening.id,&input,"document").unwrap();
    let concurrent=commit(&writer,backend.as_ref(),&intervening.id).await.unwrap();
    let stale=begin_from(&reader,"test","",enumerated).unwrap();
    fs::write(&input,b"stale editor revision C").unwrap();put(&reader,&stale.id,&input,"document").unwrap();
    let failure=commit(&reader,backend.as_ref(),&stale.id).await.unwrap_err();
    assert!(failure.to_string().contains("conflict"));
    assert_eq!(read(&reader,backend.as_ref(),"").await.unwrap().checksum,concurrent.checksum);
    let output=root.join("final");get(&reader,backend.as_ref(),"","document",&output,false).await.unwrap();
    assert_eq!(fs::read(output).unwrap(),b"concurrent revision B");
 }
 #[tokio::test]
 async fn pinned_fetch_keeps_enumerated_bytes_after_a_new_commit() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();
    let backend=disk(&root);let store=setup(&root,"reader");let writer=setup(&root,"writer");
    init(&store,backend.as_ref(),"").await.unwrap();let input=root.join("input");
    fs::write(&input,b"snapshot A bytes").unwrap();let first=begin(&store,backend.as_ref(),"test","").await.unwrap();
    put(&store,&first.id,&input,"document").unwrap();let snapshot=commit(&store,backend.as_ref(),&first.id).await.unwrap();
    let next=begin(&writer,backend.as_ref(),"test","").await.unwrap();fs::write(&input,b"snapshot B bytes").unwrap();
    put(&writer,&next.id,&input,"document").unwrap();commit(&writer,backend.as_ref(),&next.id).await.unwrap();
    let old_output=root.join("old");let old_item=get_from(&store,backend.as_ref(),"",&snapshot,"document",&old_output,false).await.unwrap();
    assert_eq!(fs::read(old_output).unwrap(),b"snapshot A bytes");
    assert_eq!(old_item,snapshot.commit.entries["document"]);
    let current_output=root.join("current");get(&store,backend.as_ref(),"","document",&current_output,false).await.unwrap();
    assert_eq!(fs::read(current_output).unwrap(),b"snapshot B bytes");
 }
 #[tokio::test]
 async fn metadata_rejection_preserves_transaction_bytes_and_abort_recovery() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let backend = disk(&root);
    let store = setup(&root, "state");
    init(&store, backend.as_ref(), "").await.unwrap();
    let transaction = begin(&store, backend.as_ref(), "test", "").await.unwrap();
    let source = root.join("source");
    fs::write(&source, b"metadata-bound contents").unwrap();
    put(&store, &transaction.id, &source, "document").unwrap();
    let record = crate::metadata::capture(&source, &Default::default()).unwrap();
    let journal = tx_path(&store, &transaction.id).unwrap();
    let original = fs::read(&journal).unwrap();
    let mut invalid_version = record.clone();
    invalid_version.version = 2;
    let mut invalid_attribute = record.clone();
    invalid_attribute.attributes.insert("user.contract".into(), "not-hex".into());
    let mut different_content = record.clone();
    different_content.content_sha256 = "a".repeat(64);
    for invalid in [
        serde_json::to_value(invalid_version).unwrap(),
        serde_json::to_value(invalid_attribute).unwrap(),
        serde_json::to_value(different_content).unwrap(),
        serde_json::json!({"version":1}),
    ] {
        assert!(set_metadata(&store, &transaction.id, "document", invalid).is_err());
        assert_eq!(fs::read(&journal).unwrap(), original, "failed metadata must never poison the durable transaction");
        assert_eq!(load(&store, &transaction.id).unwrap().phase, "open");
    }
    let mut accepted = record;
    accepted.selection = vec!["user.contract".into()];
    accepted.attributes.clear();
    accepted.attributes.insert("user.contract".into(), "76616c7565".into());
    let accepted = serde_json::to_value(accepted).unwrap();
    let updated = set_metadata(&store, &transaction.id, "document", accepted.clone()).unwrap();
    assert_eq!(updated.entries["document"].metadata, Some(accepted.clone()));
    assert_eq!(load(&store, &transaction.id).unwrap().entries["document"].metadata, Some(accepted));
    assert_eq!(abort(&store, &transaction.id).unwrap().phase, "aborted");
    assert_eq!(fs::read(source).unwrap(), b"metadata-bound contents");
 }
 #[tokio::test]
 async fn local_namespace_edits_preserve_stable_ids_and_list_aborted_journals() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let backend = disk(&root);
    let store = setup(&root, "state");
    init(&store, backend.as_ref(), "").await.unwrap();
    init(&store, backend.as_ref(), "").await.unwrap();
    let transaction = begin(&store, backend.as_ref(), "test", "").await.unwrap();
    mkdir(&store, &transaction.id, "folder").unwrap();
    let source = root.join("source");
    fs::write(&source, b"namespace fixture").unwrap();
    let staged = put(&store, &transaction.id, &source, "folder/document").unwrap();
    let stable = staged.entries["folder/document"].id.clone();
    let renamed = rename(&store, &transaction.id, "folder", "renamed").unwrap();
    assert_eq!(renamed.entries["renamed/document"].id, stable);
    assert!(remove(&store, &transaction.id, "renamed", false).is_err());
    let before = fs::read(tx_path(&store, &transaction.id).unwrap()).unwrap();
    assert!(rename(&store, &transaction.id, "renamed", "renamed/child").is_err());
    assert_eq!(fs::read(tx_path(&store, &transaction.id).unwrap()).unwrap(), before);
    remove(&store, &transaction.id, "renamed/document", false).unwrap();
    mkdir(&store, &transaction.id, "renamed/child").unwrap();
    assert!(remove(&store, &transaction.id, "missing", true).is_err());
    assert!(remove(&store, &transaction.id, "renamed", true).unwrap().entries.is_empty());
    abort(&store, &transaction.id).unwrap();
    fs::write(store.home.join("transactions/ignored.txt"), b"not a journal").unwrap();
    let listed = transactions(&store).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, transaction.id);
    assert_eq!(listed[0].phase, "aborted");
    assert!(set_metadata(&store, &transaction.id, "document", serde_json::json!({})).is_err());
 }
 #[tokio::test]
 async fn version_publication_guards_preserve_history_until_confirmation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let backend = disk(&root);
    let store = setup(&root, "state");
    fs::write(backend.root.join("document"), b"prior revision").unwrap();
    let expected = backend.stat("document").await.unwrap().unwrap();
    let version = crate::policies::prepare_version(&store, backend.as_ref(), "test", "document", &expected, "publication-contract", true).await.unwrap().unwrap();
    let mut unconfirmed = version.clone();
    unconfirmed.sha256 = "a".repeat(64);
    assert!(crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &unconfirmed).await.unwrap_err().to_string().contains("not confirmed"));
    let prefix = format!("{}/{}", crate::operations::version_root("document"), version.id);
    let marker = backend.root.join(format!("{prefix}/published.json"));
    let invalid = serde_json::json!({"format":1,"version":version.id,"path":"another-file","sha256":version.sha256});
    state::atomic_json(&marker, &invalid).unwrap();
    let original_marker = fs::read(&marker).unwrap();
    assert!(crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &version).await.unwrap_err().to_string().contains("marker is invalid"));
    assert_eq!(fs::read(&marker).unwrap(), original_marker);
    fs::remove_file(&marker).unwrap();
    backend.fail_published.store(true, Ordering::SeqCst);
    assert!(crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &version).await.unwrap_err().to_string().contains("synthetic publication marker"));
    assert!(!marker.exists());
    assert_eq!(fs::read(backend.root.join(format!("{prefix}/content"))).unwrap(), b"prior revision");
    backend.fail_published.store(false, Ordering::SeqCst);
    assert!(crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &version).await.unwrap().is_empty());
    let confirmed = fs::read(&marker).unwrap();
    backend.fail_published.store(true, Ordering::SeqCst);
    assert!(crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &version).await.unwrap().is_empty(), "confirmed replay must not upload another marker");
    assert_eq!(fs::read(marker).unwrap(), confirmed);
    assert_eq!(fs::read(backend.root.join("document")).unwrap(), b"prior revision");
    assert!(fs::read_dir(store.home.join("tmp")).unwrap().next().is_none());
 }
 #[tokio::test]
 async fn scheduler_persists_archived_revision_guards_before_versioned_upload() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let backend = disk(&root);
    let mut store = setup(&root, "state");
    fs::write(backend.root.join("document"), b"queued prior revision").unwrap();
    let original = backend.stat("document").await.unwrap().unwrap();
    let source = root.join("source");
    fs::write(&source, b"queued replacement").unwrap();
    let overwrite = crate::operations::enqueue(&mut store, "test", &source, "document", true, None, true).unwrap();
    let new_file = crate::operations::enqueue(&mut store, "test", &source, "new-file", true, None, true).unwrap();
    let ordinary = crate::operations::enqueue(&mut store, "test", &source, "ordinary-file", false, None, false).unwrap();
    let connector = backend.clone();
    let context = crate::transfer::TransferContext::new(None, crate::transfer::ProgressOutput::Off).unwrap();
    let report = crate::scheduler::run_with_connector(&mut store, None, 2, false, false, context.clone(), move |_, _, _| {
        let connector = connector.clone();
        async move { Ok(connector as Arc<dyn Backend>) }
    }).await.unwrap();
    assert_eq!(report.failed, 0);
    assert_eq!(report.results.len(), 3);
    let completed = store.db.jobs.iter().find(|job| job.id == overwrite.id).unwrap();
    assert_eq!(completed.status, "done");
    assert!(completed.version_prepared);
    assert!(completed.expected.as_ref().unwrap().same_revision(&original));
    assert!(!store.home.join("spool").join(&completed.snapshot).exists());
    let created = store.db.jobs.iter().find(|job| job.id == new_file.id).unwrap();
    assert!(created.version_prepared);
    assert!(created.expected.is_none());
    assert!(matches!(created.condition(), WriteCondition::CreateNew));
    let versions = crate::operations::versions(&store, backend.as_ref(), "document").await.unwrap();
    assert_eq!(versions.len(), 1);
    assert!(versions[0].original.same_revision(&original));
    assert_eq!(fs::read(backend.root.join(format!("{}/{}/content", crate::operations::version_root("document"), versions[0].id))).unwrap(), b"queued prior revision");
    assert!(!store.db.jobs.iter().find(|job| job.id == ordinary.id).unwrap().version_prepared);
    for path in ["document", "new-file", "ordinary-file"] { assert_eq!(fs::read(backend.root.join(path)).unwrap(), b"queued replacement"); }
    let saved: serde_json::Value = state::read_json(&store.home.join("state.json")).unwrap();
    assert!(saved["jobs"].as_array().unwrap().iter().all(|job| job["status"] == "done" && (job["version"] == false || job["version_prepared"] == true)));
    assert!(crate::scheduler::run(&mut store, None, 1, false, false, context).await.unwrap().results.is_empty());
 }
 #[tokio::test]
 async fn confirmed_retention_prunes_only_old_published_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let backend = disk(&root);
    let mut store = setup(&root, "state");
    fs::write(backend.root.join("document"), b"guarded history").unwrap();
    let expected = backend.stat("document").await.unwrap().unwrap();
    assert!(!crate::policies::VersionPolicy::default().matches("document").unwrap());
    let mut versions = Vec::new();
    for request in ["old-published", "recent-published", "unpublished", "current-published"] {
        let version = crate::policies::prepare_version(&store, backend.as_ref(), "test", "document", &expected, request, true).await.unwrap().unwrap();
        if versions.len() < 2 {
            crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &version).await.unwrap();
        }
        versions.push(version);
    }
    store.config.policies.profiles.insert("test".into(), crate::policies::ProfilePolicy {
        versions: Some(crate::policies::VersionPolicy { enabled: true, keep: 2, exclude: vec!["*.tmp".into()], ..Default::default() }),
        conflicts: None,
    });
    store.config.policies.profiles.insert("inherited".into(), crate::policies::ProfilePolicy::default());
    store.config.policies.validate().unwrap();
    assert!(crate::policies::prepare_version(&store, backend.as_ref(), "test", "excluded.tmp", &expected, "ignored", false).await.unwrap().is_none());
    let removed = crate::policies::finish_version(&store, backend.as_ref(), "test", "document", &versions[3]).await.unwrap();
    assert_eq!(removed, vec![versions[0].id.clone()]);
    let remaining = crate::operations::versions(&store, backend.as_ref(), "document").await.unwrap();
    assert_eq!(remaining.len(), 3, "unpublished recovery copies do not consume confirmed retention quota");
    assert!(remaining.iter().any(|version| version.id == versions[2].id));
    for version in remaining {
        assert_eq!(fs::read(backend.root.join(format!("{}/{}/content", crate::operations::version_root("document"), version.id))).unwrap(), b"guarded history");
    }
 }
 #[tokio::test]async fn malformed_history_and_unicode_case_collisions_are_rejected(){let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let b=disk(&root);let s=setup(&root,"state");init(&s,b.as_ref(),"").await.unwrap();let tx=begin(&s,b.as_ref(),"test","").await.unwrap();mkdir(&s,&tx.id,"Café").unwrap();assert!(mkdir(&s,&tx.id,"CAFE\u{301}").is_err());fs::write(b.root.join(format!("{CONTROL}/commits/00000000000000000002.json")),b"corrupt").unwrap();assert!(read(&s,b.as_ref(),"").await.is_err());}
}
