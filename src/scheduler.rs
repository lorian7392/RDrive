//! Concurrent network workers with one durable journal coordinator.
use crate::{state::{self,Job,Store},factory,operations,transfer::TransferContext};
use anyhow::{bail,Context,Result};
use serde::Serialize;
use std::{collections::BTreeMap,path::Path,sync::Arc};
#[derive(Debug,Serialize)]pub struct JobResult {pub id:String,pub status:String,pub error:Option<String>}
#[derive(Debug,Serialize)]pub struct ScheduleReport {pub results:Vec<JobResult>,pub failed:usize,pub concurrency:usize}
enum Event {Start{job:Job,ack:tokio::sync::oneshot::Sender<std::result::Result<(),String>>},Finish{id:String,error:Option<String>}}
pub async fn run(store:&mut Store,profile:Option<&str>,jobs:usize,retry_failed:bool,retry_overwrite:bool,context:TransferContext)->Result<ScheduleReport>{
    run_with_connector(store,profile,jobs,retry_failed,retry_overwrite,context,|p,name,context|async move{factory::connect(&p,&name,context).await}).await
}
pub(crate) async fn run_with_connector<F,C>(store:&mut Store,profile:Option<&str>,jobs:usize,retry_failed:bool,retry_overwrite:bool,context:TransferContext,connect:F)->Result<ScheduleReport>
where F:Fn(crate::config::Profile,String,TransferContext)->C+Clone+Send+Sync+'static,C:std::future::Future<Output=Result<Arc<dyn crate::backend::Backend>>>+Send {
    if jobs==0||jobs>64{bail!("jobs must be between 1 and 64");}
    let mut targets=BTreeMap::<String,Vec<(Job,crate::config::Profile)>>::new();let mut results=Vec::new();
    for job in &store.db.jobs {
        if profile.is_some_and(|p|p!=job.profile){continue;}
        if job.status!="pending"&&job.status!="interrupted"&&!(retry_failed&&job.status=="failed"){continue;}
        if job.status!="pending"&&job.overwrite&&!retry_overwrite{results.push(JobResult{id:job.id.clone(),status:"skipped".into(),error:Some("requires --retry-ambiguous-overwrite".into())});continue;}
        let p=store.config.profiles.get(&job.profile).cloned().context("unknown queued profile")?;
        targets.entry(format!("{}\0{}",job.profile,job.remote)).or_default().push((job.clone(),p));
    }
    let semaphore=Arc::new(tokio::sync::Semaphore::new(jobs));let mut workers=tokio::task::JoinSet::new();
    let (send,mut recv)=tokio::sync::mpsc::channel::<Event>(128);
    for (_target,group) in targets {
        let semaphore=semaphore.clone();let send=send.clone();let home=store.home.clone();let context=context.clone();let connect=connect.clone();
        workers.spawn(async move {
            let _permit=semaphore.acquire_owned().await.context("scheduler cancelled")?;
            for (mut job,p) in group {
                let result=async {
                    let b=connect(p,job.profile.clone(),context.scoped(&job.id)).await?;
                    let source=home.join("spool").join(&job.snapshot);
                    if state::fingerprint(&source)?!=job.sha256{bail!("upload snapshot checksum mismatch");}
                    if job.version&&!job.version_prepared {
                        if let Some(v)=operations::archive_at(&home,b.as_ref(),&job.remote).await?{if job.overwrite{job.expected=Some(v.original);}}
                        job.version_prepared=true;
                    }
                    let (ack,ready)=tokio::sync::oneshot::channel();
                    send.send(Event::Start{job:job.clone(),ack}).await.context("journal coordinator gone")?;
                    ready.await.context("journal acknowledgment lost")?.map_err(anyhow::Error::msg)?;
                    upload(&home,b.as_ref(),&job).await
                }.await;
                let error=result.err().map(|e|format!("{e:#}"));
                send.send(Event::Finish{id:job.id.clone(),error}).await.context("journal coordinator gone")?;
            }
            Result::<()>::Ok(())
        });
    }
    drop(send);
    while let Some(event)=recv.recv().await {
        match event {
            Event::Start{mut job,ack}=>{
                let index=store.db.jobs.iter().position(|j|j.id==job.id).context("scheduler job disappeared")?;
                job.status="running".into();job.attempts=store.db.jobs[index].attempts+1;job.error=None;
                store.db.jobs[index]=job;
                match store.save(){Ok(())=>{let _=ack.send(Ok(()));},Err(e)=>{let _=ack.send(Err(e.to_string()));return Err(e);}}
            },
            Event::Finish{id,error}=>{
                let index=store.db.jobs.iter().position(|j|j.id==id).context("scheduler job disappeared")?;
                let status=if error.is_some(){"failed"}else{"done"};store.db.jobs[index].status=status.into();store.db.jobs[index].error=error.clone();store.save()?;
                if error.is_none(){let _=std::fs::remove_file(store.home.join("spool").join(&store.db.jobs[index].snapshot));}
                results.push(JobResult{id,status:status.into(),error});
            }
        }
    }
    while let Some(join)=workers.join_next().await{join??;}
    let failed=results.iter().filter(|r|r.status=="failed").count();Ok(ScheduleReport{results,failed,concurrency:jobs})
}
async fn upload(home:&Path,b:&dyn crate::backend::Backend,job:&Job)->Result<()>{
    let source=home.join("spool").join(&job.snapshot);
    if job.resume {b.upload_resume(&source,&job.remote,job.condition(),&home.join("spool").join(format!("{}.upload.json",job.id))).await?;}
    else{b.upload(&source,&job.remote,job.condition()).await?;}Ok(())
}
