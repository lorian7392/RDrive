//! Adapter between durable scheduling and the owned APFS file checkpoint vault.
use super::{vault, Policy, Storage};
use crate::{checkpoint_schedule as schedule, state::Store};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(super) fn limits(policy: &Policy) -> Result<vault::Limits> {
    policy.validate()?;
    if policy.storage != Storage::ApfsVault {
        bail!("this operation requires apfs_vault checkpoint storage");
    }
    Ok(vault::Limits {
        max_bytes: policy
            .max_bytes
            .context("APFS checkpoint vault capacity is missing")?,
        max_records: policy.max_records,
        auto_prune: policy.auto_prune,
    })
}
fn key(bytes: u64) -> String {
    format!("apfs_vault:{bytes}")
}
fn options(policy: &Policy) -> Result<schedule::Options> {
    policy.validate()?;
    let managed = policy.storage == Storage::ApfsVault;
    Ok(schedule::Options {
        enabled: policy.enabled && managed,
        interval_secs: policy.interval_secs.filter(|_| managed),
        backend_key: if managed {
            key(limits(policy)?.max_bytes)
        } else {
            "system_snapshot".into()
        },
    })
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
pub(super) fn schedule_status(home: &Path, policy: &Policy) -> Result<Value> {
    schedule::status(home, &options(policy)?, now())
}
pub(super) fn pending(home: &Path, checkpoint: &str) -> Result<bool> {
    super::id(checkpoint)?;
    let value = schedule::status(
        home,
        &schedule::Options {
            enabled: false,
            interval_secs: None,
            backend_key: "inspection".into(),
        },
        now(),
    )?;
    Ok(value["inflight"]["id"].as_str() == Some(checkpoint))
}

/// A cheap probe does not acquire Store, mount an image, or reserve the shared
/// mutation coordinator. Only an actual due attempt asks metadata to yield.
pub(crate) fn due(home: &Path) -> Result<bool> {
    let config = crate::client_read::config(home)?;
    due_with(home, &config.checkpoints, now())
}
fn due_with(home: &Path, policy: &Policy, at: i64) -> Result<bool> {
    policy.validate()?;
    if !policy.enabled || policy.storage != Storage::ApfsVault || policy.interval_secs.is_none() {
        return Ok(false);
    }
    let value = schedule::status(home, &options(policy)?, at)?;
    if value["retry_at"].as_i64().is_some_and(|retry| retry > at) {
        return Ok(false);
    }
    if !value["inflight"].is_null() {
        return Ok(true);
    }
    Ok(value["next_due_at"]
        .as_i64()
        .context("checkpoint scheduler did not report its next due time")?
        <= at)
}

pub(super) fn manual(store: &Store, policy: &Policy) -> Result<Value> {
    limits(policy)?;
    run(
        &store.home,
        policy,
        schedule::Trigger::Manual,
        Some(store),
        Calls::default(),
        &now,
    )
}
pub(crate) fn scheduled(home: &Path) -> Result<Value> {
    let config = crate::client_read::config(home)?;
    run(
        home,
        &config.checkpoints,
        schedule::Trigger::Scheduled,
        None,
        Calls::default(),
        &now,
    )
}

#[derive(Clone, Copy)]
struct Calls {
    create: fn(&Store, &str, &vault::Limits) -> Result<Value>,
    reconcile: fn(&Path, &str, &vault::Limits) -> Result<Value>,
    open: fn(PathBuf) -> Result<Store>,
}
impl Default for Calls {
    fn default() -> Self {
        Self {
            create: vault::create_planned,
            reconcile: vault::reconcile,
            open: Store::open,
        }
    }
}
struct Managed<'a> {
    home: &'a Path,
    policy: &'a Policy,
    borrowed: Option<&'a Store>,
    calls: Calls,
}
impl schedule::Backend for Managed<'_> {
    fn reconcile(&mut self, attempt: &schedule::Attempt) -> Result<schedule::Recovery> {
        let limits = retained_limits(self.policy, attempt)?;
        recovery(
            (self.calls.reconcile)(self.home, &attempt.id, &limits),
            attempt,
        )
    }
    fn create(&mut self, attempt: &schedule::Attempt) -> Result<schedule::AttemptOutcome> {
        super::id(&attempt.id)?;
        let limits = limits(self.policy)?;
        if attempt.backend_key != key(limits.max_bytes) {
            bail!("checkpoint attempt belongs to another configured vault");
        }
        let result = if let Some(store) = self.borrowed {
            in_scope(store, self.home)?;
            (self.calls.create)(store, &attempt.id, &limits)
        } else {
            match (self.calls.open)(self.home.to_path_buf()) {
                Ok(store) => {
                    in_scope(&store, self.home)?;
                    if serde_json::to_value(&store.config.checkpoints)?
                        != serde_json::to_value(self.policy)?
                    {
                        return Ok(schedule::AttemptOutcome::Deferred{reason:"Checkpoint settings changed before capture; the next attempt will use the current configuration.".into()});
                    }
                    (self.calls.create)(&store, &attempt.id, &limits)
                }
                Err(error) => return creation(Err(error), attempt),
            }
        };
        creation(result, attempt)
    }
}
fn in_scope(store: &Store, home: &Path) -> Result<()> {
    if store.home != home {
        bail!("checkpoint capture Store belongs to another state directory");
    }
    Ok(())
}
fn retained_limits(policy: &Policy, attempt: &schedule::Attempt) -> Result<vault::Limits> {
    super::id(&attempt.id)?;
    let text = attempt
        .backend_key
        .strip_prefix("apfs_vault:")
        .context("unsupported retained checkpoint backend")?;
    let bytes = text
        .parse::<u64>()
        .context("invalid retained checkpoint vault capacity")?;
    if key(bytes) != attempt.backend_key {
        bail!("retained checkpoint vault capacity is not canonical");
    }
    let mut previous = policy.clone();
    previous.storage = Storage::ApfsVault;
    previous.max_bytes = Some(bytes);
    previous.interval_secs = None;
    previous.auto_prune = false;
    limits(&previous)
}
fn busy(error: &anyhow::Error) -> bool {
    const MESSAGES: &[&str] = &[
        "another rdrive process is using this state directory",
        "another checkpoint operation is active",
        "a Smart request is active; retry checkpoint after it finishes",
        "a Smart request is still running; checkpoint was not created",
        "an upload is running; retry checkpoint after it finishes",
        "checkpoint payload is busy; retry after it finishes",
    ];
    error
        .chain()
        .any(|cause| MESSAGES.contains(&cause.to_string().as_str()))
}
fn reply(value: &Value, attempt: &schedule::Attempt) -> Result<()> {
    if value["id"].as_str() != Some(attempt.id.as_str()) || value["kind"] != "apfs_file_vault" {
        bail!("vault reply does not identify the planned checkpoint");
    }
    if value["confirmed"] == true
        && (value["missing"] == true || value["safe_to_retry_same_id"] == true)
    {
        bail!("vault reply has contradictory publication state");
    }
    Ok(())
}
fn creation(
    result: Result<Value>,
    attempt: &schedule::Attempt,
) -> Result<schedule::AttemptOutcome> {
    match result {
        Ok(value) => {
            reply(&value, attempt)?;
            if value["created"] != true
                || value["confirmed"] != true
                || value.get("state").is_some_and(|state| state != "confirmed")
            {
                bail!("vault creation was not confirmed");
            }
            Ok(schedule::AttemptOutcome::Completed {
                checkpoint_id: attempt.id.clone(),
            })
        }
        Err(error) if busy(&error) => Ok(schedule::AttemptOutcome::Deferred {
            reason: format!("{error:#}"),
        }),
        Err(error) => Err(error),
    }
}
fn recovery(result: Result<Value>, attempt: &schedule::Attempt) -> Result<schedule::Recovery> {
    match result {
        Ok(value) => {
            reply(&value, attempt)?;
            if value["confirmed"] == true && value["state"] == "confirmed" {
                return Ok(schedule::Recovery::Completed {
                    checkpoint_id: attempt.id.clone(),
                });
            }
            if value["missing"] == true
                && value["safe_to_retry_same_id"] == true
                && value["state"] == "missing"
            {
                return Ok(schedule::Recovery::RetrySameAttempt);
            }
            bail!(
                "the retained checkpoint outcome is not established; no new capture was authorized"
            );
        }
        Err(error) if busy(&error) => Ok(schedule::Recovery::Deferred {
            reason: format!("{error:#}"),
        }),
        Err(error) => Err(error),
    }
}
fn run(
    home: &Path,
    policy: &Policy,
    trigger: schedule::Trigger,
    borrowed: Option<&Store>,
    calls: Calls,
    clock: &dyn schedule::Clock,
) -> Result<Value> {
    let mut backend = Managed {
        home,
        policy,
        borrowed,
        calls,
    };
    let mut value = schedule::tick(home, &options(policy)?, trigger, clock, &mut backend)?;
    if value["created"] == true || value["reconciled"] == true {
        value["id"] = value["checkpoint_id"].clone();
        value["confirmed"] = json!(true);
        value["kind"] = json!("apfs_file_vault");
        value["remote_changed"] = json!(false);
    }
    Ok(value)
}

#[cfg(test)]
#[path = "automation/tests.rs"]
mod tests;
