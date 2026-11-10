//! Durable, one-decision conflict plans. Namespace publication remains in the
//! ordinary provider path and retains its fresh version checks and receipts.
use super::*;
use crate::{
    policies::{self, ConflictStrategy},
    smart::{Mutation, MutationState, Queue},
};

fn original_location(j: &Journal, request: &Request) -> Result<(String, String, String)> {
    if request.op != "create" {
        if let Some(record) = request.id.as_ref().and_then(|id| j.items.get(id)) {
            return Ok((
                record.item.parent_id.clone(),
                record.item.name.clone(),
                record.path.clone(),
            ));
        }
    }
    let parent = request.parent_id.as_deref().unwrap_or("root");
    let name = request.name.as_deref().context(
        "original conflict name is no longer indexed; export the retained edit before resolving",
    )?;
    Ok((parent.into(), name.into(), child_path(j, parent, name)?))
}

async fn plan(
    store: &Store,
    d: &Domain,
    queue: &mut Queue,
    mutation: &Mutation,
    strategy: ConflictStrategy,
    explicit: bool,
) -> Result<Mutation> {
    plan_with(store,d,queue,mutation,strategy,explicit,&NetworkConnector).await
}
async fn plan_with(store:&Store,d:&Domain,queue:&mut Queue,mutation:&Mutation,strategy:ConflictStrategy,explicit:bool,connector:&dyn Connector) -> Result<Mutation> {
    if !matches!(
        mutation.state,
        MutationState::Conflict | MutationState::Failed
    ) {
        bail!("resolution requires a stopped conflict or failure");
    }
    if mutation.resolution.is_some() && !explicit {
        bail!("automatic resolution was already attempted; explicit attention is required");
    }
    if queue.snapshot(&mutation.id)?.is_none() {
        bail!("resolution requires retained file contents");
    }
    let original: Request = serde_json::from_value(mutation.request.clone())?;
    if !matches!(original.op.as_str(), "create" | "modify") {
        bail!("only file create/modify conflicts can be resolved this way");
    }
    let mut j = checked_journal(&store.home, &store.config, d)?;
    if strategy == ConflictStrategy::Replace && original.op == "modify" {
        if let Some(record) = original.id.as_ref().and_then(|id| j.items.get(id)) {
            let desired = child_path(
                &j,
                original
                    .parent_id
                    .as_deref()
                    .unwrap_or(&record.item.parent_id),
                original.name.as_deref().unwrap_or(&record.item.name),
            )?;
            if desired != record.path {
                bail!("replace does not discard a pending rename or move; keep-both can recover its file contents, or resolve the namespace conflict manually");
            }
        }
    }
    let (parent, name, path) = original_location(&j, &original)?;
    require_mutation_path(store, d, &path, false)?;
    let profile = store
        .config
        .profiles
        .get(&d.profile)
        .context("domain profile missing")?;
    let b = connector.connect(profile, &d.profile, crate::transfer::global()).await?;
    let snapshot = if d.repository {
        refresh(store, b.as_ref(), d, &mut j).await?
    } else {
        refresh_directory(store, b.as_ref(), d, &mut j, &parent).await?;
        None
    };
    let mut request = original;
    request.auth_token = None;
    request.content_path = None;
    request.recursive = false;
    let mut version = None;
    let mut repository_snapshot = None;
    let target = match strategy {
        ConflictStrategy::Stop => bail!("choose keep-both, replace or remote"),
        ConflictStrategy::KeepBoth => {
            let name = policies::conflict_name(&name, &mutation.id)?;
            let target = child_path(&j, &parent, &name)?;
            request.op = "create".into();
            request.id = None;
            request.parent_id = Some(parent);
            request.name = Some(name);
            request.kind = Some("file".into());
            request.base_content_version = None;
            request.base_metadata_version = None;
            request.smart_origin_id = None;
            request.client_item_id =
                Some(state::key(&format!("keep-both:{}:{}", d.id, mutation.id)));
            request.request_id = Some(state::key(&format!(
                "keep-both-request:{}:{}",
                d.id, mutation.id
            )));
            target
        }
        ConflictStrategy::Replace => {
            let record=j.items.values().find(|record|record.path==path&&record.item.kind=="file").cloned().context("replace target is absent or is not a file; keep-both can save the local edit instead")?;
            request.op = "modify".into();
            request.id = Some(record.item.id.clone());
            request.parent_id = None;
            request.name = None;
            request.kind = None;
            request.client_item_id = None;
            request.smart_origin_id = None;
            if d.repository {
                repository_snapshot = Some(
                    snapshot
                        .as_ref()
                        .context("managed prior snapshot missing")?
                        .checksum
                        .clone(),
                );
            } else {
                if !b.capabilities().conditional_write {
                    bail!("backend cannot guard conflict replacement");
                }
                let remote = remote(d, &path)?;
                let observed = b
                    .stat(&remote)
                    .await?
                    .context("replace target disappeared")?;
                let metadata = b
                    .stat_metadata(&remote)
                    .await?
                    .context("replace target disappeared")?;
                if !record
                    .entry
                    .as_ref()
                    .is_some_and(|entry| entry.same_revision(&metadata))
                {
                    bail!("version_conflict: replace target changed while observing its current version");
                }
                if b.capabilities().protocol == "webdav"
                    && observed
                        .etag
                        .as_ref()
                        .is_none_or(|tag| tag.starts_with("W/"))
                {
                    bail!("replace requires a strong guarded WebDAV revision");
                }
                j.items.get_mut(&record.item.id).unwrap().strong = Some(observed.clone());
                enrich_metadata(store, b.as_ref(), d, &mut j, &record.item.id).await?;
                let identity = state::key(&serde_json::to_string(&(
                    &mutation.id,
                    &observed,
                    &j.items[&record.item.id].item.metadata_version,
                ))?);
                version = policies::prepare_version(
                    store,
                    b.as_ref(),
                    &d.profile,
                    &remote,
                    &observed,
                    &identity,
                    true,
                )
                .await?;
            }
            let current = &j.items[&record.item.id].item;
            request.base_content_version = Some(current.content_version.clone());
            request.base_metadata_version = Some(current.metadata_version.clone());
            request.request_id = Some(state::key(&serde_json::to_string(&(
                "replace",
                &mutation.id,
                &request.base_content_version,
                &request.base_metadata_version,
                &version,
                &repository_snapshot,
            ))?));
            path
        }
    };
    require_mutation_path(store, d, &target, false)?;
    let result = queue.plan_resolution(
        &mutation.id,
        strategy,
        serde_json::to_value(request)?,
        target,
        version,
        repository_snapshot,
        explicit,
    )?;
    update_overlay(store, d)?;
    Ok(result)
}

/// Persist a local decision; the running service (or smart run) publishes it.
/// `remote` cancels the upload while retaining its original recovery snapshot.
pub async fn resolve_smart(
    home: &Path,
    domain_id: &str,
    mutation_id: &str,
    strategy: &str,
) -> Result<Value> {
    let store = Store::open(home.to_path_buf())?;
    let d = domains(home)?
        .into_iter()
        .find(|d| d.id == domain_id && d.smart)
        .context("Smart domain not found")?;
    let mut queue = Queue::open(&store, &d.id, &journal_binding(&store.config, &d)?)?;
    if strategy == "remote" {
        let mutation = queue.cancel(mutation_id)?;
        update_overlay(&store, &d)?;
        return Ok(
            json!({"resolution":"remote","mutation":mutation,"local_snapshot_retained":true,"remote_changed":false}),
        );
    }
    let strategy: ConflictStrategy = strategy.parse()?;
    let mutation = queue.get(mutation_id)?;
    let planned = plan(&store, &d, &mut queue, &mutation, strategy, true).await?;
    Ok(
        json!({"planned":true,"completed":false,"path":planned.resolution.as_ref().map(|p|&p.target_path),"mutation":planned,"requires_provider_service":true}),
    )
}

/// Automatic policy can plan once. An additional external version after that
/// decision leaves a conflict instead of repeatedly rebasing and overwriting.
pub(super) async fn auto_resolve(
    home: &Path,
    d: &Domain,
    queue: &mut Queue,
    mutation: &Mutation,
) -> Result<bool> {
    let current = queue.get(&mutation.id)?;
    if current.state != MutationState::Conflict || current.resolution.is_some() {
        return Ok(false);
    }
    let store = Store::open(home.to_path_buf())?;
    let strategy = store.config.policies.effective(&d.profile).conflicts;
    if strategy == ConflictStrategy::Stop {
        return Ok(false);
    }
    plan(&store, d, queue, &current, strategy, false).await?;
    Ok(true)
}

pub(super) fn response_with_resolution(mutation: &Mutation, mut response: Value) -> Value {
    if let Some(plan) = &mutation.resolution {
        if let Some(object) = response.as_object_mut() {
            object.insert("resolved_path".into(), json!(plan.target_path));
            object.insert("resolution".into(), json!(plan.strategy));
        }
    }
    response
}

/// Reuse the backup recorded in the authenticated, immutable local decision.
/// A public request cannot inject a version reference to bypass preparation.
pub(super) fn planned_version(
    store: &Store,
    d: &Domain,
    request: &Request,
) -> Result<Option<crate::operations::Version>> {
    let Some(identity) = request.request_id.as_deref() else {
        return Ok(None);
    };
    let mutations =
        crate::smart::read_only(&store.home, &d.id, &journal_binding(&store.config, d)?)?;
    Ok(mutations
        .into_iter()
        .filter_map(|mutation| mutation.resolution)
        .find(|plan| {
            plan.strategy == ConflictStrategy::Replace
                && plan.request["request_id"].as_str() == Some(identity)
        })
        .and_then(|plan| plan.version))
}

pub(super) async fn finish_resolution(home: &Path, d: &Domain, mutation: &Mutation) -> Result<()> {
    finish_resolution_with(home,d,mutation,&NetworkConnector).await
}
async fn finish_resolution_with(home:&Path,d:&Domain,mutation:&Mutation,connector:&dyn Connector)->Result<()> {
    if d.repository {
        return Ok(());
    }
    let Some(version) = mutation
        .resolution
        .as_ref()
        .and_then(|plan| plan.version.as_ref())
    else {
        return Ok(());
    };
    let store = Store::open(home.to_path_buf())?;
    let profile = store
        .config
        .profiles
        .get(&d.profile)
        .context("domain profile missing")?;
    let b = connector.connect(profile, &d.profile, crate::transfer::global()).await?;
    policies::finish_version(&store, b.as_ref(), &d.profile, &version.path, version).await?;
    Ok(())
}

#[cfg(test)]
pub(super) async fn plan_for_test(store:&Store,d:&Domain,queue:&mut Queue,mutation:&Mutation,strategy:ConflictStrategy,explicit:bool,connector:&dyn Connector)->Result<Mutation> {
    plan_with(store,d,queue,mutation,strategy,explicit,connector).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(value:Value)->Request {serde_json::from_value(value).unwrap()}
    fn decision()->crate::policies::ResolutionPlan {crate::policies::ResolutionPlan{strategy:ConflictStrategy::KeepBoth,request:json!({}),target_path:"saved-copy".into(),version:None,repository_snapshot:None,fingerprint:"test".into(),created:state::now()}}
    fn setup()->(tempfile::TempDir,Store,Domain,Queue,Mutation) {
        let temporary=tempfile::tempdir().unwrap();let mut store=Store::open(temporary.path().join("state")).unwrap();
        store.config.profiles.insert("p".into(),config::Profile{protocol:config::Protocol::Webdav,url:"https://example.invalid/".into(),username:None,password_env:None,allow_http:false,timeout_secs:1,smb_encryption:true,nextcloud_upload_url:None});store.save_config().unwrap();
        let domain=Domain{id:"resolution-test".into(),profile:"p".into(),remote:String::new(),display_name:"Test".into(),repository:false,local_root:None,smart:true};
        add_domain(&store,domain.clone()).unwrap();
        let mut queue=Queue::open(&store,&domain.id,&journal_binding(&store.config,&domain).unwrap()).unwrap();
        let source=temporary.path().join("local-edit");fs::write(&source,b"retained edit").unwrap();
        let mutation=queue.enqueue(json!({"v":1,"op":"modify","domain":domain.id,"id":"file","request_id":"change","base_content_version":"base","base_metadata_version":"metadata"}),Some(&File::open(source).unwrap())).unwrap();
        (temporary,store,domain,queue,mutation)
    }
    #[test]
    fn original_location_uses_indexed_identity_or_validates_explicit_parent_and_name() {
        let (_temporary,_store,domain,_queue,_mutation)=setup();let mut journal=Journal::default();
        let mut item=root_item(&domain);item.id="file".into();item.kind="file".into();item.name="indexed".into();
        journal.items.insert("file".into(),Record{path:"indexed".into(),item,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0});
        assert_eq!(original_location(&journal,&request(json!({"v":1,"op":"modify","id":"file"}))).unwrap(),("root".into(),"indexed".into(),"indexed".into()));
        assert_eq!(original_location(&journal,&request(json!({"v":1,"op":"create","name":"new"}))).unwrap().2,"new");
        assert!(original_location(&journal,&request(json!({"v":1,"op":"modify","id":"absent"}))).is_err());
        assert!(original_location(&journal,&request(json!({"v":1,"op":"create","parent_id":"missing","name":"new"}))).is_err());
    }
    #[tokio::test]
    async fn invalid_resolution_states_are_stopped_before_any_connection_or_credential_lookup() {
        let (_temporary,store,domain,mut queue,mutation)=setup();
        assert!(plan(&store,&domain,&mut queue,&mutation,ConflictStrategy::KeepBoth,true).await.unwrap_err().to_string().contains("stopped"));
        let failed=queue.fail(&mutation.id,crate::smart::FailureKind::Conflict,"synthetic conflict").unwrap();
        let mut resolved=failed.clone();resolved.resolution=Some(decision());
        assert!(plan(&store,&domain,&mut queue,&resolved,ConflictStrategy::KeepBoth,false).await.unwrap_err().to_string().contains("already attempted"));
        let mut deleted=failed.clone();deleted.request["op"]=json!("delete");
        assert!(plan(&store,&domain,&mut queue,&deleted,ConflictStrategy::KeepBoth,true).await.unwrap_err().to_string().contains("only file"));
        assert!(plan(&store,&domain,&mut queue,&failed,ConflictStrategy::KeepBoth,true).await.unwrap_err().to_string().contains("no longer indexed"));
        let no_contents=queue.enqueue(json!({"v":1,"op":"modify","domain":domain.id,"id":"another","request_id":"no-content","base_content_version":"base","base_metadata_version":"metadata"}),None).unwrap();
        let no_contents=queue.fail(&no_contents.id,crate::smart::FailureKind::Conflict,"metadata only").unwrap();
        assert!(plan(&store,&domain,&mut queue,&no_contents,ConflictStrategy::KeepBoth,true).await.unwrap_err().to_string().contains("retained file contents"));
        let mut journal=checked_journal(&store.home,&store.config,&domain).unwrap();let mut item=root_item(&domain);item.id="file".into();item.kind="file".into();item.name="original".into();
        journal.items.insert("file".into(),Record{path:"original".into(),item,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0});save_journal(&store,&domain.id,&journal).unwrap();
        let mut renamed=failed;renamed.request["name"]=json!("new-name");
        assert!(plan(&store,&domain,&mut queue,&renamed,ConflictStrategy::Replace,true).await.unwrap_err().to_string().contains("pending rename"));
    }
    #[tokio::test]
    async fn responses_only_add_resolution_receipts_and_no_version_needs_no_network() {
        let (_temporary,store,domain,_queue,mut mutation)=setup();
        let home=store.home.clone();assert!(planned_version(&store,&domain,&request(json!({"v":1,"op":"modify"}))).unwrap().is_none());
        assert_eq!(response_with_resolution(&mutation,json!({"item":"confirmed"})),json!({"item":"confirmed"}));
        mutation.resolution=Some(decision());
        assert_eq!(response_with_resolution(&mutation,json!({"item":"confirmed"}))["resolved_path"],"saved-copy");
        assert_eq!(response_with_resolution(&mutation,json!(null)),json!(null));
        drop(store);finish_resolution(&home,&domain,&mutation).await.unwrap();
        let mut managed=domain.clone();managed.repository=true;finish_resolution(&home,&managed,&mutation).await.unwrap();
    }
    async fn remote_fixture() -> (tempfile::TempDir,Store,Domain,Queue,Mutation,Arc<super::super::test_backend::Disk>) {
        let (temporary,store,domain,mut queue,mutation)=setup();let disk=super::super::test_backend::Disk::new(temporary.path().join("remote"));fs::write(disk.root.join("file"),b"remote original").unwrap();
        let entry=disk.stat("file").await.unwrap().unwrap();let mut journal=checked_journal(&store.home,&store.config,&domain).unwrap();let mut item=root_item(&domain);item.id="file".into();item.kind="file".into();item.name="file".into();item.size=entry.size;item.content_version=version(&entry).unwrap();
        journal.items.insert("file".into(),Record{path:"file".into(),item,entry:Some(entry.clone()),strong:Some(entry),client_id:None,metadata_known:false,metadata_checked:0});save_journal(&store,&domain.id,&journal).unwrap();
        let failed=queue.fail(&mutation.id,crate::smart::FailureKind::Conflict,"synthetic external edit").unwrap();(temporary,store,domain,queue,failed,disk)
    }
    #[tokio::test]
    async fn keep_both_plans_content_recovery_and_replace_archives_exact_remote_revision() {
        use super::super::test_backend::Connected;
        for strategy in [ConflictStrategy::KeepBoth,ConflictStrategy::Replace] {
            let (_temporary,store,domain,mut queue,failed,disk)=remote_fixture().await;let connector=Connected(disk.clone());
            let planned=plan_with(&store,&domain,&mut queue,&failed,strategy,true,&connector).await.unwrap();let decision=planned.resolution.as_ref().unwrap();assert_eq!(decision.strategy,strategy);
            assert_eq!(fs::read(disk.root.join("file")).unwrap(),b"remote original","a decision must not publish the local edit");
            if strategy==ConflictStrategy::KeepBoth {assert_eq!(decision.request["op"],"create");assert!(decision.target_path.contains("conflict-"));assert!(decision.version.is_none());}
            else {let version=decision.version.clone().unwrap();assert_eq!(version.path,"file");let req:Request=serde_json::from_value(decision.request.clone()).unwrap();assert_eq!(planned_version(&store,&domain,&req).unwrap().unwrap().id,version.id);}
            let home=store.home.clone();drop(store);finish_resolution_with(&home,&domain,&planned,&connector).await.unwrap();
            assert!(disk.connections.load(std::sync::atomic::Ordering::SeqCst)>=1);
        }
    }
    #[tokio::test]
    async fn stop_missing_target_and_unsupported_replacement_do_not_publish_any_bytes() {
        use super::super::test_backend::Connected;
        let (_temporary,store,domain,mut queue,failed,mut disk)=remote_fixture().await;Arc::get_mut(&mut disk).unwrap().conditional=false;let connector=Connected(disk.clone());
        assert!(plan_with(&store,&domain,&mut queue,&failed,ConflictStrategy::Stop,true,&connector).await.unwrap_err().to_string().contains("choose keep-both"));
        assert!(plan_with(&store,&domain,&mut queue,&failed,ConflictStrategy::Replace,true,&connector).await.unwrap_err().to_string().contains("cannot guard"));
        fs::remove_file(disk.root.join("file")).unwrap();assert!(plan_with(&store,&domain,&mut queue,&failed,ConflictStrategy::Replace,true,&connector).await.unwrap_err().to_string().contains("target is absent"));
        assert!(queue.get(&failed.id).unwrap().resolution.is_none());
    }

}
