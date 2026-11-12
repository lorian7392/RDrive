//! Guarded ordinary-folder namespace mutations. Deleted directories are retained
//! as recoverable hidden tombstones; no recursive unguarded DELETE is issued.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Node {
    suffix: String,
    entry: Entry,
    metadata: Option<crate::metadata::MetadataRecord>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Plan {
    from: String,
    to: String,
    deleted: bool,
    expected: Entry,
    nodes: Vec<Node>,
    records: Vec<Record>,
    phase: String,
    #[serde(default)]
    confirmed: Option<Entry>,
    #[serde(default)]
    followup: Option<Request>,
}

fn key(d: &Domain, req: &Request) -> Result<String> {
    Ok(format!(
        "{}:{}",
        state::key(&d.id),
        req.request_id
            .as_deref()
            .context("namespace mutation requires request_id")?
    ))
}
pub(super) fn pending(j: &Journal, d: &Domain, req: &Request) -> bool {
    key(d, req)
        .ok()
        .and_then(|key| j.requests.get(&key))
        .is_some_and(|p| p.namespace.is_some())
}
fn check_filters(store: &Store, d: &Domain, plan: &Plan) -> Result<()> {
    for node in &plan.nodes {
        let source = if node.suffix.is_empty() {
            plan.from.clone()
        } else {
            backend::join(&plan.from, &node.suffix)?
        };
        require_mutation_path(store, d, &source, node.entry.is_dir)?;
        if !plan.deleted {
            require_mutation_path(
                store,
                d,
                &mapped(&source, &plan.from, &plan.to)?,
                node.entry.is_dir,
            )?;
        }
    }
    Ok(())
}
pub(super) fn check_pending_filters(
    store: &Store,
    d: &Domain,
    j: &Journal,
    req: &Request,
) -> Result<bool> {
    if let Some(plan) = key(d, req)
        .ok()
        .and_then(|key| j.requests.get(&key))
        .and_then(|p| p.namespace.as_ref())
    {
        check_filters(store, d, plan)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
pub(super) fn requested(j: &Journal, req: &Request) -> bool {
    let record = req.id.as_ref().and_then(|id| j.items.get(id));
    req.op == "modify"
        && record.is_some_and(|record| {
            child_path(
                j,
                req.parent_id.as_deref().unwrap_or(&record.item.parent_id),
                req.name.as_deref().unwrap_or(&record.item.name),
            )
            .is_ok_and(|target| target != record.path)
        })
        || req.op == "delete" && record.is_some_and(|r| r.item.kind == "directory")
}
fn under(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}/"))
}
fn mapped(path: &str, from: &str, to: &str) -> Result<String> {
    if path == from {
        Ok(to.into())
    } else {
        backend::join(
            to,
            path.strip_prefix(&format!("{from}/"))
                .context("invalid namespace descendant")?,
        )
    }
}
fn capabilities(directory: bool) -> Vec<String> {
    let mut result = vec!["read", "rename", "reparent", "delete"];
    result.push(if directory { "add" } else { "write" });
    result.into_iter().map(str::to_owned).collect()
}
async fn revision(b: &dyn Backend, path: &str, is_dir: bool) -> Result<Entry> {
    if is_dir {
        b.stat_metadata(path).await?
    } else {
        b.stat(path).await?
    }
    .context("version_conflict: namespace source disappeared")
}
async fn tree(b: &dyn Backend, root: &str, initial: &Entry) -> Result<Vec<(String, Entry)>> {
    let mut result = vec![(String::new(), initial.clone())];
    let mut scopes = if initial.is_dir {
        vec![root.to_owned()]
    } else {
        vec![]
    };
    while let Some(parent) = scopes.pop() {
        for entry in b.list_metadata(&parent).await? {
            if backend::normalize(&entry.path)? != entry.path
                || entry.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("") != parent
            {
                bail!("unsafe namespace manifest path");
            }
            let suffix = entry
                .path
                .strip_prefix(&format!("{root}/"))
                .context("namespace listing escaped source")?
                .to_owned();
            if entry.is_dir {
                scopes.push(entry.path.clone());
            }
            let strong = revision(b, &entry.path, entry.is_dir).await?;
            result.push((suffix, strong));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    crate::sync::validate_path_collisions(
        result
            .iter()
            .map(|(path, _)| path)
            .filter(|p| !p.is_empty()),
    )?;
    Ok(result)
}
async fn prepare(
    store: &Store,
    b: &dyn Backend,
    d: &Domain,
    j: &mut Journal,
    req: &Request,
) -> Result<Plan> {
    let id = req.id.as_deref().context("namespace item required")?;
    let original = j.items.get(id).context("no_such_item")?.clone();
    check_base(req, &original.item)?;
    let deleted = req.op == "delete";
    if original.item.kind == "directory"
        && (req.content_path.is_some()
            || req.tag_data_hex.is_some()
            || req.extended_attributes.is_some())
    {
        bail!("unsupported: directory contents and user attributes cannot accompany a move");
    }
    let target = if deleted {
        if !req.recursive {
            bail!("directory deletion requires recursive=true");
        }
        let parent = original.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        backend::join(parent, &format!(".rdrive-trash-{}", uuid::Uuid::new_v4()))?
    } else {
        child_path(
            j,
            req.parent_id.as_deref().unwrap_or(&original.item.parent_id),
            req.name.as_deref().unwrap_or(&original.item.name),
        )?
    };
    if target == original.path || target.starts_with(&format!("{}/", original.path)) {
        bail!("invalid namespace destination: same item or its descendant");
    }
    require_mutation_path(store, d, &original.path, original.item.kind == "directory")?;
    if !deleted {
        require_mutation_path(store, d, &target, original.item.kind == "directory")?;
    }
    if j.items.values().any(|r| r.path == target) {
        bail!("version_conflict: namespace destination already exists");
    }
    let source = remote(d, &original.path)?;
    let observed = b.stat_metadata(&source).await?.context("no_such_item")?;
    if !original
        .entry
        .as_ref()
        .is_some_and(|cached| cached.same_revision(&observed))
    {
        bail!("version_conflict: namespace source changed since enumeration");
    }
    let expected = revision(b, &source, original.item.kind == "directory").await?;
    if let Some(strong) = &original.strong {
        if !b.namespace_matches(strong, &expected) {
            bail!("version_conflict: namespace content changed since its verified snapshot");
        }
    }
    let manifest = tree(b, &source, &expected).await?;
    for (suffix, entry) in &manifest {
        let path = if suffix.is_empty() {
            original.path.clone()
        } else {
            backend::join(&original.path, suffix)?
        };
        require_mutation_path(store, d, &path, entry.is_dir)?;
        if !deleted {
            require_mutation_path(
                store,
                d,
                &mapped(&path, &original.path, &target)?,
                entry.is_dir,
            )?;
        }
    }
    if deleted {
        for (suffix, _) in &manifest {
            let path = if suffix.is_empty() {
                original.path.clone()
            } else {
                backend::join(&original.path, suffix)?
            };
            if !j.items.values().any(|r| r.path == path) {
                bail!("version_conflict: directory contains an unindexed new child; refresh its subtree before deletion");
            }
        }
    }
    let mut nodes = Vec::new();
    for (suffix, entry) in manifest {
        let metadata = if entry.is_dir {
            None
        } else {
            crate::metadata_remote::load(b, &entry.path, &entry).await?
        };
        nodes.push(Node {
            suffix,
            entry,
            metadata,
        });
    }
    let mut records = j
        .items
        .values()
        .filter(|r| under(&r.path, &original.path))
        .cloned()
        .collect::<Vec<_>>();
    // Move unopened descendants too, assigning IDs only where no identity was
    // previously exposed. Existing descendants keep their exact IDs and pins.
    let mut ids = records
        .iter()
        .map(|r| (r.path.clone(), r.item.id.clone()))
        .collect::<BTreeMap<_, _>>();
    for node in &nodes {
        let path = if node.suffix.is_empty() {
            original.path.clone()
        } else {
            backend::join(&original.path, &node.suffix)?
        };
        ids.entry(path)
            .or_insert_with(|| uuid::Uuid::new_v4().to_string());
    }
    for node in &nodes {
        let path = if node.suffix.is_empty() {
            original.path.clone()
        } else {
            backend::join(&original.path, &node.suffix)?
        };
        if records.iter().any(|r| r.path == path) {
            continue;
        }
        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let tags = node
            .metadata
            .as_ref()
            .and_then(|m| m.attributes.get(crate::metadata::FINDER_TAGS))
            .cloned();
        let attrs = node.metadata.as_ref().map(extended).unwrap_or_default();
        let item = Item {
            id: ids[&path].clone(),
            // New records are strict descendants: tree() lists each direct
            // parent before descending, and ids contains the whole manifest.
            parent_id: ids.get(parent).cloned().context("namespace parent missing")?,
            name: path.rsplit('/').next().unwrap().into(),
            kind: if node.entry.is_dir {
                "directory"
            } else {
                "file"
            }
            .into(),
            size: node.entry.size,
            modified: None,
            content_version: version(&node.entry)?,
            metadata_version: metadata_version(&path, &tags, &attrs, false),
            pinned: false,
            capabilities: capabilities(node.entry.is_dir),
            tag_data_hex: tags,
            extended_attributes: attrs,
            smart: d.smart,
            sync_state: None,
            sync_message: None,
            has_unsynced_changes: false,
            native_materialized: false,
        };
        records.push(Record {
            path,
            item,
            entry: Some(node.entry.clone()),
            strong: if node.entry.is_dir {
                None
            } else {
                Some(node.entry.clone())
            },
            client_id: None,
            metadata_known: node.metadata.is_some(),
            metadata_checked: 0,
        });
    }
    Ok(Plan {
        from: original.path,
        to: target,
        deleted,
        expected,
        nodes,
        records,
        phase: "prepared".into(),
        confirmed: None,
        followup: None,
    })
}
async fn verify(b: &dyn Backend, d: &Domain, plan: &Plan) -> Result<Vec<(String, Entry)>> {
    let root = remote(d, &plan.to)?;
    let actual = revision(b, &root, plan.expected.is_dir).await?;
    if !b.namespace_matches(plan.confirmed.as_ref().unwrap_or(&plan.expected), &actual) {
        bail!("version_conflict: moved source identity/content changed");
    }
    let actual = tree(b, &root, &actual).await?;
    if actual.len() != plan.nodes.len() {
        bail!("version_conflict: namespace subtree changed during publication; all contents were retained");
    }
    for ((suffix, entry), node) in actual.iter().zip(&plan.nodes) {
        if suffix != &node.suffix || !b.namespace_matches(&node.entry, entry) {
            bail!("version_conflict: namespace descendant changed during publication; all contents were retained");
        }
    }
    Ok(actual)
}
async fn migrate(
    store: &Store,
    b: &dyn Backend,
    d: &Domain,
    j: &mut Journal,
    plan: &Plan,
    actual: &[(String, Entry)],
    req: &Request,
) -> Result<()> {
    let old = j.items.clone();
    let mut next = old.clone();
    next.retain(|_, r| !under(&r.path, &plan.from) && !under(&r.path, &plan.to));
    if !plan.deleted {
        for original in &plan.records {
            let mut record = original.clone();
            record.path = mapped(&record.path, &plan.from, &plan.to)?;
            let suffix = record
                .path
                .strip_prefix(&plan.to)
                .unwrap()
                .trim_start_matches('/');
            let entry = actual
                .iter()
                .find(|(path, _)| path == suffix)
                .map(|(_, entry)| entry.clone())
                .context("namespace record missing from manifest")?;
            // SMB exposes metadata-only enumeration revisions separately from its
            // verified content/identity guard. Keep the same public version family
            // as enumeration so the next directory refresh cannot invalidate this
            // successful receipt or discard its strong edit guard.
            let observed = b
                .stat_metadata(&entry.path)
                .await?
                .context("version_conflict: moved catalog item disappeared")?;
            if observed.is_dir != entry.is_dir || observed.size != entry.size {
                bail!("version_conflict: moved catalog item changed during confirmation");
            }
            record.entry = Some(observed.clone());
            record.strong = if entry.is_dir {
                None
            } else {
                Some(entry.clone())
            };
            record.item.content_version = version(&observed)?;
            if original.path == plan.from {
                record.item.parent_id = req
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| record.item.parent_id.clone());
                record.item.name = plan.to.rsplit('/').next().unwrap().into();
            }
            record.item.capabilities = capabilities(entry.is_dir);
            record.item.metadata_version = metadata_version(
                &record.path,
                &record.item.tag_data_hex,
                &record.item.extended_attributes,
                record.item.pinned,
            );
            next.insert(record.item.id.clone(), record);
        }
    }
    emit_namespace(store, d, j, &old, next)
}

pub(super) async fn process(
    store: Store,
    b: &dyn Backend,
    d: &Domain,
    j: &mut Journal,
    staging: &Path,
    fd: &File,
    req: &Request,
    context: crate::transfer::TransferContext,
) -> Result<Value> {
    let key = key(d, req)?;
    let mut stable = req.clone();
    stable.content_path = None;
    stable.auth_token = None;
    let source_hash = req
        .content_path
        .as_ref()
        .map(|path| {
            staging_file(staging, fd, path).and_then(|file| {
                crate::metadata::capture_file(&file, &Default::default())
                    .and_then(|m| Ok((m.content_sha256.clone(), m.fingerprint()?)))
            })
        })
        .transpose()?;
    let fingerprint = state::key(&serde_json::to_string(&(&stable, &source_hash))?);
    if let Some(pending) = j.requests.get(&key) {
        if pending.fingerprint != fingerprint {
            bail!("version_conflict: namespace request identity was reused");
        }
        if let Some(response) = &pending.response {
            return Ok(response.clone());
        }
    }
    if j.requests
        .get(&key)
        .and_then(|p| p.namespace.as_ref())
        .is_none()
    {
        let plan = prepare(&store, b, d, j, req).await?;
        j.requests.insert(
            key.clone(),
            Pending {
                fingerprint,
                metadata: None,
                source_fingerprint: None,
                transaction: None,
                confirmed: None,
                response: None,
                deletion_path: None,
                deletion_expected: None,
                metadata_base_fingerprint: None,
                namespace: Some(plan),
                version: None,
            },
        );
        save_journal(&store, &d.id, j)?;
    }
    let mut plan = j.requests[&key].namespace.clone().unwrap();
    check_filters(&store, d, &plan)?;
    if plan.phase == "restored" {
        bail!("version_conflict: namespace publication was restored after a concurrent change; retained plan requires attention");
    }
    if plan.phase == "prepared" {
        let source = remote(d, &plan.from)?;
        let target = remote(d, &plan.to)?;
        let source_actual = if plan.expected.is_dir {
            b.stat_metadata(&source).await?
        } else {
            b.stat(&source).await?
        };
        let target_actual = if plan.expected.is_dir {
            b.stat_metadata(&target).await?
        } else {
            b.stat(&target).await?
        };
        let moved = if let Some(target) = target_actual {
            if source_actual
                .as_ref()
                .is_some_and(|entry| b.namespace_matches(&plan.expected, entry))
                || !b.namespace_matches(&plan.expected, &target)
            {
                bail!("version_conflict: namespace destination was occupied; both paths preserved");
            }
            target
        } else {
            if !source_actual
                .as_ref()
                .is_some_and(|entry| b.namespace_matches(&plan.expected, entry))
            {
                bail!("version_conflict: namespace source changed or disappeared");
            }
            if plan.deleted {crate::history::capture(&store,b,&d.profile,&source,req.id.as_deref(),"delete",&key,&plan.expected,false).await?;}
            b.rename_checked(&source, &target, &plan.expected).await?
        };
        plan.confirmed = Some(moved);
        plan.phase = "moved".into();
        j.requests.get_mut(&key).unwrap().namespace = Some(plan.clone());
        save_journal(&store, &d.id, j)?;
    }
    if plan.followup.is_none() {
        let actual = match verify(b, d, &plan).await {
            Ok(actual) => actual,
            Err(error) => {
                if let Ok(current) = revision(b, &remote(d, &plan.to)?, plan.expected.is_dir).await
                {
                    if b.rename_checked(&remote(d, &plan.to)?, &remote(d, &plan.from)?, &current)
                        .await
                        .is_ok()
                    {
                        plan.phase = "restored".into();
                        j.requests.get_mut(&key).unwrap().namespace = Some(plan.clone());
                        save_journal(&store, &d.id, j)?;
                    }
                }
                return Err(error.context(format!(
                    "recoverable namespace retained at {} or {}",
                    plan.from, plan.to
                )));
            }
        };
        if !plan.deleted {
            for node in &plan.nodes {
                if let Some(metadata) = &node.metadata {
                    let entry = actual
                        .iter()
                        .find(|(suffix, _)| suffix == &node.suffix)
                        .unwrap()
                        .1
                        .clone();
                    crate::metadata_remote::save_record_if_fingerprint(
                        b,
                        &entry.path,
                        &entry,
                        metadata,
                        None,
                    )
                    .await?;
                }
            }
        }
        migrate(&store, b, d, j, &plan, &actual, req).await?;
        if !plan.deleted
            && (req.content_path.is_some()
                || req.tag_data_hex.is_some()
                || req.extended_attributes.is_some())
        {
            let item = &j.items[req.id.as_deref().unwrap()].item;
            let mut followup = req.clone();
            followup.name = None;
            followup.parent_id = None;
            followup.content_path = None;
            followup.auth_token = None;
            followup.request_id = Some(state::key(&format!("{key}:namespace-followup")));
            followup.base_content_version = Some(item.content_version.clone());
            followup.base_metadata_version = Some(item.metadata_version.clone());
            plan.followup = Some(followup);
            j.requests.get_mut(&key).unwrap().namespace = Some(plan.clone());
            save_journal(&store, &d.id, j)?;
        }
    }
    if let Some(mut followup) = plan.followup.clone() {
        followup.content_path = req.content_path.clone();
        let home = store.home.clone();
        drop(store);
        let response = Box::pin(online_process(&home, staging, fd, followup, context)).await?;
        let store = Store::open(home.clone())?;
        *j = checked_journal(&home, &store.config, d)?;
        return complete(&store, d, j, &key, plan, response);
    }
    let response = if plan.deleted {
        json!({"recoverable":true,"recovery_path":plan.to,"space_reclaimed":false})
    } else {
        json!({"item":j.items.get(req.id.as_deref().unwrap()).context("moved item missing")?.item})
    };
    complete(&store, d, j, &key, plan, response)
}
fn complete(
    store: &Store,
    d: &Domain,
    j: &mut Journal,
    key: &str,
    mut plan: Plan,
    response: Value,
) -> Result<Value> {
    j.requests
        .get_mut(key)
        .context("namespace receipt missing")?
        .response = Some(response.clone());
    plan.phase = "complete".into();
    let from=remote(d,&plan.from)?;let to=remote(d,&plan.to)?;
    if plan.deleted {crate::history::confirm_request(store,&d.profile,key,Some(&to))?;}else{crate::history::renamed(store,&d.profile,&from,&to)?;}
    let _=crate::history::prune(store,&d.profile,true);
    j.requests.get_mut(key).unwrap().namespace = Some(plan);
    save_journal(store, &d.id, j)?;
    Ok(response)
}

pub(super) fn recovery(j: &Journal) -> Value {
    json!(j.requests.iter().filter_map(|(id,p)|p.namespace.as_ref().filter(|plan|plan.deleted||plan.phase!="complete").map(|plan|json!({"id":id,"source":plan.from,"recovery_path":plan.to,"phase":plan.phase,"recoverable":true,"space_reclaimed":false}))).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::coverage_tests::{setup,entry,Tree};
    use std::sync::{Mutex,atomic::{AtomicBool,Ordering}};
    fn fixture()->(tempfile::TempDir,Store,Domain,Journal,Tree,Request) {
        let (temp,store,domain)=setup();let mut journal=checked_journal(&store.home,&store.config,&domain).unwrap();let source=entry("folder",true);
        let mut item=root_item(&domain);item.id="folder".into();item.name="folder".into();item.content_version=version(&source).unwrap();
        journal.items.insert("folder".into(),Record{path:"folder".into(),item,entry:Some(source.clone()),strong:None,client_id:None,metadata_known:false,metadata_checked:0});
        let tree=Tree{entries:Mutex::new(vec![source,entry("folder/sub",true),entry("folder/sub/file",false)]),unsafe_listing:AtomicBool::new(false)};
        let request=serde_json::from_value(json!({"v":1,"op":"modify","domain":domain.id,"id":"folder","request_id":"rename","name":"renamed"})).unwrap();(temp,store,domain,journal,tree,request)
    }
    #[tokio::test]
    async fn unopened_descendants_get_stable_ids_and_survive_guarded_namespace_projection() {
        let (_temp,store,domain,mut journal,tree,request)=fixture();let plan=prepare(&store,&tree,&domain,&mut journal,&request).await.unwrap();
        assert_eq!(plan.records.len(),3);assert_eq!(plan.records[0].item.id,"folder");assert_eq!(plan.nodes.len(),3);
        for record in tree.entries.lock().unwrap().iter_mut(){record.path=record.path.replacen("folder","renamed",1);}
        let actual=verify(&tree,&domain,&plan).await.unwrap();migrate(&store,&tree,&domain,&mut journal,&plan,&actual,&request).await.unwrap();
        assert_eq!(journal.items.len(),3);assert_eq!(journal.items["folder"].path,"renamed");
        assert!(journal.items.values().any(|record|record.path=="renamed/sub/file"&&record.item.kind=="file"));
        tree.entries.lock().unwrap().pop();assert!(verify(&tree,&domain,&plan).await.unwrap_err().to_string().contains("subtree changed"));
        tree.entries.lock().unwrap().push(entry("renamed/sub/file",false));tree.entries.lock().unwrap().last_mut().unwrap().size=99;
        assert!(verify(&tree,&domain,&plan).await.is_err());
    }
    #[tokio::test]
    async fn namespace_preparation_rejects_invalid_moves_attributes_and_stale_observations() {
        let (_temp,store,domain,mut journal,tree,request)=fixture();
        let mut invalid=request.clone();invalid.content_path=Some("not-opened".into());assert!(prepare(&store,&tree,&domain,&mut journal,&invalid).await.unwrap_err().to_string().contains("directory contents"));
        let mut invalid=request.clone();invalid.op="delete".into();assert!(prepare(&store,&tree,&domain,&mut journal,&invalid).await.unwrap_err().to_string().contains("recursive"));
        let mut invalid=request.clone();invalid.name=Some("folder".into());assert!(prepare(&store,&tree,&domain,&mut journal,&invalid).await.unwrap_err().to_string().contains("same item"));
        tree.unsafe_listing.store(true,Ordering::SeqCst);assert!(prepare(&store,&tree,&domain,&mut journal,&request).await.unwrap_err().to_string().contains("unsafe namespace"));tree.unsafe_listing.store(false,Ordering::SeqCst);
        let mut stale=journal.items["folder"].entry.clone().unwrap();stale.etag=Some("changed".into());journal.items.get_mut("folder").unwrap().entry=Some(stale);
        assert!(prepare(&store,&tree,&domain,&mut journal,&request).await.unwrap_err().to_string().contains("source changed since enumeration"));
        assert!(revision(&tree,"missing",false).await.is_err());assert!(mapped("outside","folder","renamed").is_err());
    }
}
