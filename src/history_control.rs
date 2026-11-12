//! Explicit history decisions shared by the CLI and the authenticated broker.
use crate::{
    backend::{self, Backend, Entry},
    config::Config,
    history,
    state::{self, Store},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Files {
        profile: String,
        remote: Option<String>,
        item_id: Option<String>,
        offset: usize,
        page_size: usize,
    },
    Status {
        profile: String,
    },
    Export {
        profile: String,
        version: String,
        local: PathBuf,
    },
    Restore {
        profile: String,
        version: String,
        target: Option<String>,
        replace: bool,
        plan: Option<String>,
        apply: bool,
    },
    Prune {
        profile: String,
        apply: bool,
    },
    Clear {
        profile: String,
        apply: bool,
    },
    Import {
        profile: String,
        remote: String,
        local: PathBuf,
        request_id: String,
    },
    CheckpointStatus,
    CheckpointList,
    CheckpointCreate {
        authorize: bool,
    },
    CheckpointInspect {
        id: String,
    },
    CheckpointExport {
        id: String,
        payload: PathBuf,
        local: PathBuf,
        authorize: bool,
    },
    CheckpointReconcile {
        id: String,
        authorize: bool,
    },
    CheckpointForget {
        id: String,
        apply: bool,
    },
    CheckpointPrune { apply: bool },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    id: String,
    profile: String,
    binding: String,
    entry_id: String,
    scope: String,
    target: String,
    replace: bool,
    expected: Option<Entry>,
    expected_metadata: Option<String>,
    created: i64,
}
fn binding(config: &Config, profile: &str) -> Result<String> {
    let p = config
        .profiles
        .get(profile)
        .context("history profile not found")?;
    Ok(state::key(&serde_json::to_string(&(
        profile,
        &p.protocol,
        &p.url,
        &p.username,
    ))?))
}
fn plan_path(home: &Path, id: &str) -> Result<PathBuf> {
    if uuid::Uuid::parse_str(id)?.to_string() != id {
        bail!("invalid history restore plan identity");
    }
    Ok(home.join("history/plans").join(format!("{id}.json")))
}
fn plan_file(path: &Path) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .context("history plan needs a parent directory")?;
    for directory in [
        parent,
        parent
            .parent()
            .context("history plan directory needs a history root")?,
    ] {
        let meta = fs::symlink_metadata(directory)?;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
        {
            bail!("history plan directory must be private and owned");
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.len() > 1024 * 1024
    {
        bail!("history plan must be a private owned regular file within its size limit");
    }
    Ok(file)
}
pub fn files(
    home: &Path,
    config: &Config,
    profile: &str,
    remote: Option<&str>,
    item_id: Option<&str>,
    offset: usize,
    page_size: usize,
) -> Result<Value> {
    if !(1..=1000).contains(&page_size) {
        bail!("history page size must be 1..1000");
    }
    let rows = history::list(home, config, profile, remote, item_id)?;
    let total = rows.len();
    Ok(
        json!({"entries":rows.into_iter().skip(offset).take(page_size).collect::<Vec<_>>(),"total":total,"offset":offset,"has_more":offset.saturating_add(page_size)<total,"policy":config.history.effective(profile)}),
    )
}
pub async fn prepare(
    store: &Store,
    b: &dyn Backend,
    profile: &str,
    version: &str,
    target: Option<&str>,
    replace: bool,
    scope: &str,
) -> Result<Value> {
    let entry = history::lookup(&store.home, &store.config, profile, version)?;
    let id = uuid::Uuid::new_v4().to_string();
    let target = match target {
        Some(path) => backend::normalize(path)?,
        None if replace => entry.path.clone(),
        None => {
            let (parent, name) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
            let name =
                crate::policies::conflict_name(name, &id)?.replace(" (conflict-", " (recovered-");
            // The archive parent is normalized and conflict_name validates the leaf.
            if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            }
        }
    };
    if target.is_empty() {
        bail!("cannot restore over the remote root");
    }
    if replace {
        crate::provider::history_replacement_guard(&store.home, &store.config, profile, &target)?;
    }
    let current = b.stat(&target).await?;
    if replace && (entry.kind != "file" || !current.as_ref().is_some_and(|e| !e.is_dir)) {
        bail!("replacement requires an existing file; restore directories as a new copy");
    }
    if !replace && current.is_some() {
        bail!("restore copy destination already exists");
    }
    let expected_metadata = if let Some(e) = current.as_ref() {
        crate::metadata_remote::load(b, &target, e)
            .await?
            .as_ref()
            .map(crate::metadata::MetadataRecord::fingerprint)
            .transpose()?
    } else {
        None
    };
    let plan = Plan {
        version: 1,
        id: id.clone(),
        profile: profile.into(),
        binding: binding(&store.config, profile)?,
        entry_id: version.into(),
        scope: scope.into(),
        target: target.clone(),
        replace,
        expected: current,
        expected_metadata,
        created: chrono::Utc::now().timestamp(),
    };
    let directory = store.home.join("history/plans");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let meta = fs::symlink_metadata(&directory)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        bail!("history plan directory must be private and owned");
    }
    state::atomic_json(&plan_path(&store.home, &id)?, &plan)?;
    Ok(
        json!({"can_restore":true,"plan_id":id,"mode":if replace{"replace"}else{"copy"},"target":target,"message":if replace{"The current file will first be preserved in history. Replacement stops if its contents or metadata change."}else{"A new copy will be created without replacing existing files."}}),
    )
}
pub async fn apply(
    store: &Store,
    b: &dyn Backend,
    profile: &str,
    version: &str,
    id: &str,
    scope: &str,
) -> Result<Value> {
    let mut bytes = Vec::new();
    plan_file(&plan_path(&store.home, id)?)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let plan: Plan = serde_json::from_slice(&bytes)?;
    let age = chrono::Utc::now().timestamp().saturating_sub(plan.created);
    if plan.version != 1
        || plan.id != id
        || plan.profile != profile
        || plan.entry_id != version
        || plan.binding != binding(&store.config, profile)?
        || plan.scope != scope
        || !(0..=86400).contains(&age)
    {
        bail!("history plan expired or does not match this connection, item and version; preview again");
    }
    if plan.replace {
        crate::provider::history_replacement_guard(
            &store.home,
            &store.config,
            profile,
            &plan.target,
        )?;
    }
    history::restore(
        store,
        b,
        profile,
        version,
        Some(&plan.target),
        plan.expected.as_ref(),
        plan.replace.then_some(plan.expected_metadata),
        &format!("history-plan:{id}"),
    )
    .await
}
/// Read-only queries avoid the Store writer lease, including while Finder runs.
pub fn read(home: &Path, command: &Command) -> Result<Option<Value>> {
    let value = match command {
        Command::Files {
            profile,
            remote,
            item_id,
            offset,
            page_size,
        } => files(
            home,
            &crate::client_read::config(home)?,
            profile,
            remote.as_deref(),
            item_id.as_deref(),
            *offset,
            *page_size,
        )?,
        Command::Status { profile } => {
            history::status(home, &crate::client_read::config(home)?, profile)?
        }
        Command::CheckpointStatus => {
            crate::checkpoints::status(home, &crate::client_read::config(home)?.checkpoints)?
        }
        Command::CheckpointList => crate::checkpoints::list(home)?,
        Command::CheckpointInspect { id } => crate::checkpoints::inspect(home, id)?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}
pub async fn execute(home: &Path, command: &Command) -> Result<Value> {
    let open = || Store::open(home.to_path_buf());
    match command {
        Command::Files { .. }
        | Command::Status { .. }
        | Command::CheckpointStatus
        | Command::CheckpointList
        | Command::CheckpointInspect { .. } => {
            read(home, command)?.context("read-only history command returned no result")
        }
        Command::Export {
            profile,
            version,
            local,
        } => history::export(&open()?, profile, version, local),
        Command::Prune { profile, apply } => history::prune(&open()?, profile, *apply),
        Command::Clear { profile, apply } => history::clear(&open()?, profile, *apply),
        Command::Import {
            profile,
            remote,
            local,
            request_id,
        } => Ok(serde_json::to_value(history::capture_import(
            &open()?,
            profile,
            remote,
            local,
            request_id,
        )?)?),
        Command::Restore {
            profile,
            version,
            target,
            replace,
            plan,
            apply: commit,
        } => {
            let store = open()?;
            if *commit && plan.is_none() {
                bail!("history restore --apply requires a saved --plan from a preview");
            }
            if !*commit && plan.is_some() {
                bail!("a saved restore plan requires --apply");
            }
            if plan.is_some() && (target.is_some() || *replace) {
                bail!("a saved plan fixes its target and mode; omit --to and --replace when applying it");
            }
            let p = store
                .config
                .profiles
                .get(profile)
                .context("history profile not found")?;
            let b = crate::factory::connect(p, profile, crate::transfer::global()).await?;
            if let Some(id) = plan {
                apply(&store, b.as_ref(), profile, version, id, "cli").await
            } else {
                prepare(
                    &store,
                    b.as_ref(),
                    profile,
                    version,
                    target.as_deref(),
                    *replace,
                    "cli",
                )
                .await
            }
        }
        Command::CheckpointCreate { authorize } => {
            let store = open()?;
            crate::checkpoints::create(&store, &store.config.checkpoints, *authorize)
        }
        Command::CheckpointExport {
            id,
            payload,
            local,
            authorize,
        } => {
            let _store = open()?;
            crate::checkpoints::export_file(home, id, payload, local, *authorize)
        }
        Command::CheckpointReconcile { id, authorize } => {
            crate::checkpoints::reconcile(&open()?, id, *authorize)
        }
        Command::CheckpointForget { id, apply } => {
            let store = open()?;
            if *apply {
                crate::checkpoints::forget(&store, id)
            } else {
                Ok(
                    json!({"applied":false,"checkpoint":crate::checkpoints::inspect(home,id)?,"os_snapshot_deleted":false}),
                )
            }
        }
        Command::CheckpointPrune { apply } => crate::checkpoints::prune(&open()?, *apply),
    }
}

#[cfg(test)]
mod tests;
