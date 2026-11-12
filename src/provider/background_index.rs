//! Read-only metadata requests run independently for each connection. Only
//! planning and publication take the home coordinator; remote writes keep it.
use super::*;
use std::collections::BTreeSet;
use tokio::sync::{Mutex,Semaphore};

#[derive(Default)]
struct Worker {backends:IndexBackends,prefer_metadata:bool}
pub(super) struct Workers {domains:BTreeMap<String,Arc<Mutex<Worker>>>,reads:Arc<Semaphore>}
impl Default for Workers {fn default()->Self {Self{domains:BTreeMap::new(),reads:Arc::new(Semaphore::new(4))}}}
impl Workers {
    pub(super) fn schedule(&mut self,home:&Path,coordinator:Arc<Mutex<()>>,priority:Arc<admin::Priority>)->Result<()> {
        let configured=domains(home)?;let smart:BTreeSet<_>=configured.iter().filter(|d|d.smart).map(|d|d.id.clone()).collect();
        self.domains.retain(|id,_|smart.contains(id));
        for domain in configured.into_iter().filter(|d|d.smart) {
            let worker=self.domains.entry(domain.id.clone()).or_default().clone();
            let Ok(mut worker)=worker.try_lock_owned() else {continue;};
            let home=home.to_path_buf();let coordinator=coordinator.clone();let priority=priority.clone();let reads=self.reads.clone();
            tokio::spawn(async move {let Ok(_permit)=reads.acquire().await else{return;};let _=once(&home,&domain,&coordinator,Some(priority.as_ref()),&mut worker,&NetworkConnector).await;});
        }
        Ok(())
    }
}

/// Remote mutations remain serialized. Release coordination only after each
/// complete domain attempt so queued publications and administration can run.
pub(super) async fn replay_once(home:&Path,staging:&Path,fd:&File,priority:&admin::Priority,coordinator:&Mutex<()>)->Result<()> {
    coordinated_replays(home,priority,coordinator,|domain| replay_domain(home,staging,fd,domain)).await
}
async fn coordinated_replays<F,Fut>(home:&Path,priority:&admin::Priority,coordinator:&Mutex<()>,mut attempt:F)->Result<()>
where F:FnMut(Domain)->Fut,Fut:std::future::Future<Output=Result<()>> {
    for domain in domains(home)?.into_iter().filter(|d|d.smart) {
        let _guard=coordinator.lock().await;
        if priority.requested() {return Ok(());}
        let Some(domain)=domains(home)?.into_iter().find(|current|current.id==domain.id&&current.smart) else {continue;};
        attempt(domain).await?;
    }
    Ok(())
}
async fn replay_domain(home:&Path,staging:&Path,fd:&File,domain:Domain)->Result<()> {
    use crate::smart::MutationState;
    let store=Store::open(home.to_path_buf())?;
    if crate::smart::control(&store)?.paused {return Ok(());}
    let mut queue=crate::smart::Queue::open(&store,&domain.id,&journal_binding(&store.config,&domain)?)?;
    let pending=queue.list().into_iter().find(|m|matches!(m.state,MutationState::Pending|MutationState::Interrupted));
    if let Some(mutation)=pending {
        update_overlay(&store,&domain)?;drop(store);
        let _=replay(home,staging,fd,&domain,&mut queue,&mutation,crate::transfer::global().scoped(format!("smart:{}:{}",mutation.id,uuid::Uuid::new_v4())),true).await;
        drop(queue);let store=Store::open(home.to_path_buf())?;update_overlay(&store,&domain)?;
    }
    Ok(())
}

#[derive(Clone)]
enum Scope {Directory{id:String,path:String},Metadata{id:String,record:Record},Repository}
struct Plan {domain:Domain,profile:config::Profile,binding:String,signature:String,anchor:u64,scope:Scope,demands:Vec<CatalogDemand>}
fn signature(store:&Store,domain:&Domain)->Result<String> {
    let profile=store.config.profiles.get(&domain.profile).context("domain profile missing")?;
    Ok(state::key(&serde_json::to_string(&(domain,profile,&store.config.filters,&store.config.index))?))
}
fn prepare(home:&Path,domain:&Domain,worker:&mut Worker)->Result<Option<Plan>> {
    let configured=domains(home)?.into_iter().find(|candidate|candidate.id==domain.id&&candidate.smart);
    let Some(domain)=configured else {return Ok(None);};
    let store=Store::open(home.to_path_buf())?;update_overlay(&store,&domain)?;
    if crate::smart::control(&store)?.paused {return Ok(None);}
    let profile=store.config.profiles.get(&domain.profile).context("domain profile missing")?.clone();
    let binding=journal_binding(&store.config,&domain)?;let mut journal=checked_journal(home,&store.config,&domain)?;
    let demands=catalog_demands(&store,&domain,&journal)?;let now=chrono::Utc::now().timestamp_millis();
    let scope=if domain.repository {
        let last=journal.directory_scans.get("root").copied().unwrap_or(0);
        let requests=demands.iter().filter(|d|!d.metadata&&d.id=="root").collect::<Vec<_>>();
        let force=requests.iter().any(|d|d.force&&d.requested>last);
        let ttl=if requests.iter().any(|d|!d.force){store.config.index.active_refresh_secs}else{store.config.index.refresh_secs};
        if (!force&&last>0&&now.saturating_sub(last)<(ttl*1000)as i64)||journal.directory_attempts.get("root").is_some_and(|a|a.next_try>now) {return Ok(None);}
        Scope::Repository
    } else {
        let directory=next_directory(&store,&domain,&journal,&demands)?;let metadata=next_metadata(&store,&domain,&journal,&demands)?;
        let selected=if worker.prefer_metadata||directory.is_none(){metadata.as_ref().map(|id|Scope::Metadata{id:id.clone(),record:journal.items[id].clone()})}else{None};
        worker.prefer_metadata=!worker.prefer_metadata;
        if let Some(scope)=selected {scope}
        else if let Some(id)=directory {Scope::Directory{path:scope_parent(&journal,&id)?,id}}
        else {return Ok(None);}
    };
    match &scope {
        Scope::Directory{id,..}=>{begin_scan(&mut journal.directory_attempts,id);journal.directory_turn=journal.directory_turn.saturating_add(1);},
        Scope::Repository=>begin_scan(&mut journal.directory_attempts,"root"),
        Scope::Metadata{id,..}=>begin_scan(&mut journal.metadata_attempts,id),
    }
    save_journal(&store,&domain.id,&journal)?;
    Ok(Some(Plan{signature:signature(&store,&domain)?,domain,profile,binding,anchor:journal.anchor,scope,demands}))
}

enum Observed {Directory(Vec<Entry>),Metadata(Option<crate::metadata::MetadataRecord>),Repository(repository::Snapshot)}
async fn read(home:&Path,backend:&dyn Backend,plan:&Plan)->Result<Observed> {
    match &plan.scope {
        Scope::Directory{path,..}=>{
            let parent=remote(&plan.domain,path)?;let entries=backend.list_metadata(&parent).await?;let mut paths=BTreeSet::new();
            for entry in &entries {if backend::normalize(&entry.path)?!=entry.path||entry.path.rsplit_once('/').map(|(p,_)|p).unwrap_or("")!=parent||!paths.insert(&entry.path){bail!("unsafe or duplicate provider child path");}}
            Ok(Observed::Directory(entries))
        },
        Scope::Repository=>Ok(Observed::Repository(repository::read_at(home,backend,&plan.domain.remote).await?)),
        Scope::Metadata{record,..}=>{
            let strong=record.strong.as_ref().context("metadata target has no verified content revision")?;
            let path=remote(&plan.domain,&record.path)?;
            let value=crate::metadata_remote::load(backend,&path,strong).await?;
            let observed=backend.stat_metadata(&path).await?.context("version_conflict: metadata source disappeared")?;
            if !record.entry.as_ref().is_some_and(|entry|entry.same_revision(&observed)){bail!("version_conflict: metadata source changed after namespace listing");}
            Ok(Observed::Metadata(value))
        }
    }
}
fn publish(home:&Path,plan:&Plan,backend:Option<&dyn Backend>,observed:Option<Observed>)->Result<bool> {
    let Some(domain)=domains(home)?.into_iter().find(|d|d.id==plan.domain.id&&d.smart) else{return Ok(false);};
    let store=Store::open(home.to_path_buf())?;
    if signature(&store,&domain)?!=plan.signature||crate::smart::control(&store)?.paused {return Ok(false);}
    let mut journal=checked_journal(home,&store.config,&domain)?;
    // A foreground edit may have completed while this network request was in
    // flight. Never replace its new namespace, receipt, or local projection.
    if journal.anchor!=plan.anchor {return Ok(false);}
    if let Scope::Directory{id,path}=&plan.scope {if scope_parent(&journal,id)?!=*path {return Ok(false);}}
    let success=observed.is_some();
    if let Some(observed)=observed {
        let next=match observed {
            Observed::Directory(entries)=>{let Scope::Directory{id,..}=&plan.scope else{bail!("invalid directory publication scope");};directory_entries(backend.context("metadata backend missing")?,&domain,&journal,id,entries)?},
            Observed::Repository(snapshot)=>repository_entries(&domain,&journal,&snapshot)?,
            Observed::Metadata(metadata)=>{let Scope::Metadata{id,..}=&plan.scope else{bail!("invalid metadata publication scope");};let mut next=journal.items.clone();project_metadata(next.get_mut(id).context("metadata item disappeared")?,metadata.as_ref());next},
        };
        let old=journal.items.clone();
        match &plan.scope {Scope::Directory{id,..}=>{journal.directory_scans.insert(id.clone(),chrono::Utc::now().timestamp_millis());},Scope::Repository=>{journal.directory_scans.insert("root".into(),chrono::Utc::now().timestamp_millis());},Scope::Metadata{..}=>{}}
        emit_namespace(&store,&domain,&mut journal,&old,next)?;
    }
    match &plan.scope {Scope::Directory{id,..}=>finish_scan(&mut journal.directory_attempts,id,success),Scope::Repository=>finish_scan(&mut journal.directory_attempts,"root",success),Scope::Metadata{id,..}=>finish_scan(&mut journal.metadata_attempts,id,success)}
    if success {journal.completed_demands.extend(index_scheduler::completed_manual_demands(&journal,&plan.demands,domain.repository));}
    save_journal(&store,&domain.id,&journal)?;Ok(true)
}
async fn connect(worker:&mut Worker,plan:&Plan,connector:&dyn Connector)->Result<Arc<dyn Backend>> {
    let key=state::key(&serde_json::to_string(&(&plan.binding,&plan.profile))?);
    if let Some((saved,backend))=worker.backends.get(&plan.domain.id){if *saved==key{return Ok(backend.clone());}}
    let backend=connector.connect(&plan.profile,&plan.domain.profile,crate::transfer::global()).await?;
    worker.backends.insert(plan.domain.id.clone(),(key,backend.clone()));Ok(backend)
}
async fn once(home:&Path,domain:&Domain,coordinator:&Mutex<()>,priority:Option<&admin::Priority>,worker:&mut Worker,connector:&dyn Connector)->Result<()> {
    let plan={let _guard=coordinator.lock().await;if priority.is_some_and(admin::Priority::requested){return Ok(());}prepare(home,domain,worker)?};
    let Some(plan)=plan else{return Ok(());};let timeout=std::time::Duration::from_secs(plan.profile.timeout_secs.max(5));
    let connected=admin::read_phase(priority,timeout,connect(worker,&plan,connector)).await;
    let backend=match connected {admin::ReadPhase::Finished(Ok(backend))=>Some(backend),admin::ReadPhase::Deferred=>{worker.backends.clear();return Ok(());},_=>None};
    let observed=if let Some(backend)=&backend {match admin::read_phase(priority,timeout,read(home,backend.as_ref(),&plan)).await {admin::ReadPhase::Finished(Ok(value))=>Some(value),admin::ReadPhase::Deferred=>{worker.backends.clear();return Ok(());},_=>None}}else{None};
    if observed.is_none(){worker.backends.clear();}
    let _guard=coordinator.lock().await;publish(home,&plan,backend.as_ref().map(AsRef::as_ref),observed)?;Ok(())
}

#[cfg(test)]
mod tests;
