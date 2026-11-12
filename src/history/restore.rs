use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Action {
    version: u32,
    id: String,
    account: String,
    profile: String,
    entry_id: String,
    target: String,
    expected: Option<Entry>,
    expected_metadata: Option<Option<String>>,
    phase: String,
    confirmed: Option<Entry>,
    prior: Option<String>,
    staging: Option<String>,
    nodes: BTreeMap<String, Entry>,
    result: Option<Value>,
}
fn action_key(account: &str, request_id: &str) -> Result<String> {
    if request_id.is_empty() || request_id.len() > 4096 {
        bail!("history publication requires a stable request identity");
    }
    Ok(state::key(&serde_json::to_string(&(account, request_id))?))
}
fn save(connection: &Connection, action: &Action, fingerprint: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO actions(id,binding,fingerprint,value,result) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(id) DO UPDATE SET value=excluded.value,result=excluded.result",
        params![
            action.id,
            action.account,
            fingerprint,
            serde_json::to_string(action)?,
            action
                .result
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
        ],
    )?;
    Ok(())
}
fn action(
    connection: &Connection,
    id: &str,
    account: &str,
    fingerprint: &str,
) -> Result<Option<Action>> {
    let row = connection
        .query_row(
            "SELECT binding,fingerprint,value FROM actions WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((saved_account, saved_fingerprint, value)) = row else {
        return Ok(None);
    };
    if saved_account != account || saved_fingerprint != fingerprint {
        bail!("version_conflict: history publication request was reused");
    }
    let saved: Action = serde_json::from_str(&value)?;
    if saved.version != 1 || saved.id != id || saved.account != account {
        bail!("history publication journal is corrupt");
    }
    Ok(Some(saved))
}
fn recovered_name(path: &str, request_id: &str) -> Result<String> {
    let parent = path.rsplit_once('/').map_or("", |parts| parts.0);
    let name = path
        .rsplit('/')
        .next()
        .context("history filename missing")?;
    let candidate =
        crate::policies::conflict_name(name, request_id)?.replace(" (conflict-", " (recovered-");
    backend::join(parent, &candidate)
}
async fn same(backend: &dyn Backend, path: &str, expected: &Entry) -> Result<Entry> {
    let actual = backend
        .stat(path)
        .await?
        .context("version_conflict: history publication target disappeared")?;
    if !expected.same_revision(&actual) {
        bail!("version_conflict: history publication target changed");
    }
    Ok(actual)
}
async fn check_metadata(
    backend: &dyn Backend,
    path: &str,
    entry: &Entry,
    expected: &Option<String>,
) -> Result<()> {
    let current = crate::metadata_remote::load(backend, path, entry).await?;
    if current
        .as_ref()
        .map(MetadataRecord::fingerprint)
        .transpose()?
        != *expected
    {
        bail!("version_conflict: restore destination metadata changed since its preview");
    }
    Ok(())
}
async fn metadata(
    backend: &dyn Backend,
    path: &str,
    entry: &Entry,
    node: &Node,
    prior: Option<&Node>,
) -> Result<()> {
    let Some(desired) = &node.metadata else {
        return Ok(());
    };
    let current = crate::metadata_remote::load(backend, path, entry).await?;
    let observed = current
        .as_ref()
        .map(MetadataRecord::fingerprint)
        .transpose()?;
    if observed.as_deref() == Some(desired.fingerprint()?.as_str()) {
        return Ok(());
    }
    let expected = prior
        .and_then(|node| node.metadata.as_ref())
        .map(MetadataRecord::fingerprint)
        .transpose()?;
    if observed != expected {
        bail!("version_conflict: restored file metadata changed; current contents and all recovery versions were retained");
    }
    crate::metadata_remote::save_record_if_fingerprint(
        backend,
        path,
        entry,
        desired,
        expected.as_deref(),
    )
    .await?;
    Ok(())
}

/// Restore a new sibling by default. A replacement requires the exact revision
/// from the caller's durable preview and an independent before-image of it.
pub async fn restore(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    id: &str,
    target: Option<&str>,
    expected: Option<&Entry>,
    expected_metadata: Option<Option<String>>,
    request_id: &str,
) -> Result<Value> {
    let account = binding(&store.config, profile)?;
    let (root, connection) = open(&store.home, true)?.unwrap();
    let archive = load(&connection, id, &account)?;
    let target = target
        .map(str::to_owned)
        .unwrap_or(recovered_name(&archive.public.path, request_id)?);
    remote_path(&target)?;
    if expected.is_some_and(|entry| entry.path != target || entry.is_dir)
        || expected.is_some() && (archive.public.kind == "directory" || expected_metadata.is_none())
    {
        bail!("history replacement requires a guarded file; restore directories as a new copy");
    }
    for node in archive.nodes.iter().filter(|node| !node.entry.is_dir) {
        verify_blob(&root, node)?;
    }
    let key = action_key(&account, request_id)?;
    let fingerprint = state::key(&serde_json::to_string(&(
        "restore",
        id,
        &target,
        expected,
        &expected_metadata,
    ))?);
    let mut publication = if let Some(saved) = action(&connection, &key, &account, &fingerprint)? {
        saved
    } else {
        let current = backend.stat(&target).await?;
        match (expected,current) {
            (None,None)=>(),
            (Some(expected),Some(current)) if expected.same_revision(&current)=>(),
            _=>bail!("version_conflict: restore destination differs from its preview; no remote operation was started"),
        }
        let publication = Action {
            version: 1,
            id: key.clone(),
            account: account.clone(),
            profile: profile.into(),
            entry_id: id.into(),
            target: target.clone(),
            expected: expected.cloned(),
            expected_metadata: expected_metadata.clone(),
            phase: "prepared".into(),
            confirmed: None,
            prior: None,
            staging: None,
            nodes: BTreeMap::new(),
            result: None,
        };
        save(&connection, &publication, &fingerprint)?;
        publication
    };
    if let Some(result) = publication.result {
        return Ok(result);
    }
    if archive.public.kind == "directory" {
        return restore_directory(
            store,
            backend,
            &root,
            connection,
            &archive,
            &mut publication,
            &fingerprint,
        )
        .await;
    }
    if publication.phase == "publishing" && publication.confirmed.is_none() {
        bail!("history restore publication is uncertain; inspect the destination before creating a new recovery decision");
    }
    if publication.phase == "prepared" {
        if let Some(expected) = expected {
            same(backend, &target, expected).await?;
            check_metadata(
                backend,
                &target,
                expected,
                expected_metadata.as_ref().unwrap(),
            )
            .await?;
            let saved = capture(
                store,
                backend,
                profile,
                &target,
                None,
                "restore",
                &format!("{request_id}:prior"),
                expected,
                true,
            )
            .await?
            .context("history before-image missing")?;
            publication.prior = Some(saved.id);
            check_metadata(
                backend,
                &target,
                expected,
                expected_metadata.as_ref().unwrap(),
            )
            .await?;
        }
        publication.phase = "publishing".into();
        save(&connection, &publication, &fingerprint)?;
        let content = verify_blob(&root, &archive.nodes[0])?;
        let confirmed = backend
            .upload(
                &content,
                &target,
                expected
                    .cloned()
                    .map_or(WriteCondition::CreateNew, WriteCondition::Match),
            )
            .await?;
        publication.confirmed = Some(confirmed);
        publication.phase = "content-confirmed".into();
        save(&connection, &publication, &fingerprint)?;
    }
    let confirmed = publication
        .confirmed
        .as_ref()
        .context("history restore receipt is incomplete")?;
    let actual = same(backend, &target, confirmed).await?;
    let prior = publication
        .prior
        .as_ref()
        .map(|id| load(&connection, id, &account))
        .transpose()?;
    let old = prior
        .as_ref()
        .filter(|prior| prior.original.same_revision(&actual))
        .map(|prior| &prior.nodes[0]);
    metadata(backend, &target, &actual, &archive.nodes[0], old).await?;
    if let Some(id) = &publication.prior {
        confirm(store, profile, id, None)?;
    }
    let result = json!({"restored":true,"path":target,"mode":if expected.is_some(){"replace"}else{"copy"},"entry_id":id,"current_version_preserved":publication.prior,"remote_entry":actual,"metadata_restored":archive.nodes[0].metadata.is_some()});
    publication.phase = "done".into();
    publication.result = Some(result.clone());
    save(&connection, &publication, &fingerprint)?;
    let _ = prune(store, profile, true);
    Ok(result)
}

async fn verify_directory(
    backend: &dyn Backend,
    path: &str,
    archive: &Archive,
    confirmed: &BTreeMap<String, Entry>,
) -> Result<Entry> {
    let root = backend
        .stat(path)
        .await?
        .context("version_conflict: restored directory disappeared")?;
    if !root.is_dir {
        bail!("version_conflict: history directory destination is a file");
    }
    let actual = roots(&root, backend::walk(backend, path).await?)?;
    if actual.len() != archive.nodes.len() {
        bail!("version_conflict: restored directory changed before publication");
    }
    for ((suffix, entry), node) in actual.iter().zip(&archive.nodes) {
        if suffix != &node.path
            || entry.is_dir != node.entry.is_dir
            || !entry.is_dir
                && !confirmed
                    .get(suffix)
                    .is_some_and(|old| old.same_revision(entry))
        {
            bail!("version_conflict: restored directory contents changed before publication");
        }
    }
    Ok(root)
}
async fn restore_directory(
    store: &Store,
    backend: &dyn Backend,
    root: &Path,
    connection: Connection,
    archive: &Archive,
    publication: &mut Action,
    fingerprint: &str,
) -> Result<Value> {
    if publication.phase == "creating-stage" || publication.phase.starts_with("uploading:") {
        bail!("history directory staging is uncertain; retained data was preserved for review");
    }
    if publication.phase == "prepared" {
        let parent = publication
            .target
            .rsplit_once('/')
            .map_or("", |parts| parts.0);
        let staging = backend::join(
            parent,
            &format!(".rdrive-history-restore-{}", publication.id),
        )?;
        if backend.stat(&staging).await?.is_some() {
            bail!("version_conflict: history restore staging name is occupied");
        }
        publication.staging = Some(staging.clone());
        publication.phase = "creating-stage".into();
        save(&connection, publication, fingerprint)?;
        backend.mkdir(&staging).await?;
        publication.phase = "staging".into();
        save(&connection, publication, fingerprint)?;
    }
    let staging = publication
        .staging
        .clone()
        .context("history directory staging receipt is missing")?;
    if publication.phase == "staging" {
        for node in archive.nodes.iter().skip(1) {
            let target = backend::join(&staging, &node.path)?;
            if let Some(expected) = publication.nodes.get(&node.path) {
                let actual = backend
                    .stat(&target)
                    .await?
                    .context("history staged item disappeared")?;
                if !backend.namespace_matches(expected, &actual) {
                    bail!("version_conflict: staged history item changed");
                }
                continue;
            }
            if backend.stat(&target).await?.is_some() {
                bail!("version_conflict: history staging descendant is unexpectedly occupied");
            }
            publication.phase = format!("uploading:{}", node.path);
            save(&connection, publication, fingerprint)?;
            if node.entry.is_dir {
                backend.mkdir(&target).await?;
                let current = backend
                    .stat(&target)
                    .await?
                    .context("history staging directory missing")?;
                publication.nodes.insert(node.path.clone(), current);
            } else {
                let current = backend
                    .upload(
                        &verify_blob(root, node)?,
                        &target,
                        WriteCondition::CreateNew,
                    )
                    .await?;
                metadata(backend, &target, &current, node, None).await?;
                publication.nodes.insert(node.path.clone(), current);
            }
            publication.phase = "staging".into();
            save(&connection, publication, fingerprint)?;
        }
        let expected = verify_directory(backend, &staging, archive, &publication.nodes).await?;
        publication.confirmed = Some(expected);
        publication.phase = "moving".into();
        save(&connection, publication, fingerprint)?;
    }
    let expected = publication
        .confirmed
        .as_ref()
        .context("history directory publication guard is missing")?
        .clone();
    let source = backend.stat(&staging).await?;
    let target = backend.stat(&publication.target).await?;
    let moved=match (source,target) {
        (Some(source),None) if backend.namespace_matches(&expected,&source)=>{verify_directory(backend,&staging,archive,&publication.nodes).await?;backend.rename_checked(&staging,&publication.target,&expected).await?},
        (None,Some(target)) if backend.namespace_matches(&expected,&target)=>target,
        _=>bail!("version_conflict: history directory publication could not be reconciled; no replacement was attempted"),
    };
    if !backend.namespace_matches(&expected, &moved) {
        bail!("version_conflict: history directory move returned another identity");
    }
    verify_directory(backend, &publication.target, archive, &publication.nodes).await?;
    for node in archive.nodes.iter().filter(|node| !node.entry.is_dir) {
        let target = backend::join(&publication.target, &node.path)?;
        let actual = backend
            .stat(&target)
            .await?
            .context("restored child disappeared")?;
        metadata(backend, &target, &actual, node, None).await?;
    }
    let result = json!({"restored":true,"path":publication.target,"mode":"copy","entry_id":archive.public.id,"files":archive.public.files,"directory":true});
    publication.phase = "done".into();
    publication.result = Some(result.clone());
    save(&connection, publication, fingerprint)?;
    let _ = prune(store, &publication.profile, true);
    Ok(result)
}

/// Retain both an independent local tree and the guarded remote tombstone.
/// Directory deletion never falls back to an unconditional recursive DELETE.
pub async fn recycle(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    path: &str,
    recursive: bool,
    request_id: &str,
) -> Result<Value> {
    recycle_checked(store, backend, profile, path, recursive, request_id, None).await
}
pub async fn recycle_checked(
    store: &Store,
    backend: &dyn Backend,
    profile: &str,
    path: &str,
    recursive: bool,
    request_id: &str,
    expected_source: Option<&Entry>,
) -> Result<Value> {
    remote_path(path)?;
    if !store.config.history.effective(profile).enabled {
        bail!("history recycle policy is disabled for this connection");
    }
    let account = binding(&store.config, profile)?;
    let key = action_key(&account, request_id)?;
    let fingerprint = state::key(&serde_json::to_string(&("delete", path, recursive))?);
    let (root, connection) = open(&store.home, true)?.unwrap();
    let mut publication = if let Some(saved) = action(&connection, &key, &account, &fingerprint)? {
        saved
    } else {
        let expected = backend
            .stat(path)
            .await?
            .context("remote item does not exist")?;
        if expected_source
            .is_some_and(|guard| guard.path != path || !guard.same_revision(&expected))
        {
            bail!("version_conflict: recycle source differs from its checked snapshot");
        }
        if expected.is_dir && !recursive {
            bail!("directory recycling requires recursive=true");
        }
        let archived = capture(
            store, backend, profile, path, None, "delete", request_id, &expected, false,
        )
        .await?
        .context("history recycle before-image missing")?;
        let parent = path.rsplit_once('/').map_or("", |parts| parts.0);
        let target = backend::join(parent, &format!(".rdrive-trash-{}", key))?;
        let publication = Action {
            version: 1,
            id: key.clone(),
            account: account.clone(),
            profile: profile.into(),
            entry_id: archived.id,
            target,
            expected: Some(expected),
            expected_metadata: None,
            phase: "prepared".into(),
            confirmed: None,
            prior: None,
            staging: Some(path.into()),
            nodes: BTreeMap::new(),
            result: None,
        };
        save(&connection, &publication, &fingerprint)?;
        publication
    };
    if let Some(result) = publication.result {
        return Ok(result);
    }
    let expected = publication
        .expected
        .as_ref()
        .context("history recycle guard missing")?
        .clone();
    if expected_source.is_some_and(|guard| guard.path != path || !guard.same_revision(&expected)) {
        bail!("version_conflict: recycle source guard differs from its saved intent");
    }
    let archive = load(&connection, &publication.entry_id, &account)?;
    let source = backend.stat(path).await?;
    let target = backend.stat(&publication.target).await?;
    let actual = match (source, target) {
        (Some(source), None) if backend.namespace_matches(&expected, &source) => {
            let observed = observed_tree(backend, path, &expected).await?;
            if observed.len() != archive.nodes.len()
                || observed
                    .iter()
                    .zip(&archive.nodes)
                    .any(|((suffix, entry), node)| {
                        suffix != &node.path || !entry.same_revision(&node.entry)
                    })
            {
                bail!("version_conflict: recycle contents changed since capture");
            }
            publication.phase = "moving".into();
            save(&connection, &publication, &fingerprint)?;
            backend
                .rename_checked(path, &publication.target, &expected)
                .await?
        }
        (None, Some(target)) if backend.namespace_matches(&expected, &target) => target,
        _ => bail!(
            "version_conflict: recycled item publication is uncertain; all copies were retained"
        ),
    };
    if !backend.namespace_matches(&expected, &actual) {
        bail!("version_conflict: recycle move returned another item");
    }
    for node in archive.nodes.iter().filter(|node| !node.entry.is_dir) {
        verify_blob(&root, node)?;
    }
    confirm(
        store,
        profile,
        &publication.entry_id,
        Some(&publication.target),
    )?;
    let result = json!({"deleted":path,"recycled":true,"history_id":publication.entry_id,"remote_backup":publication.target,"local_contents_retained":true});
    publication.phase = "done".into();
    publication.result = Some(result.clone());
    save(&connection, &publication, &fingerprint)?;
    let _ = prune(store, profile, true);
    Ok(result)
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
