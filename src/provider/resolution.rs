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
    let b = crate::factory::connect(profile, &d.profile, crate::transfer::global()).await?;
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
    let b = crate::factory::connect(profile, &d.profile, crate::transfer::global()).await?;
    policies::finish_version(&store, b.as_ref(), &d.profile, &version.path, version).await?;
    Ok(())
}
