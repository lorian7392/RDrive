use super::*;
use crate::checkpoint_schedule::{Attempt, AttemptOutcome, Backend, Recovery, Trigger};
use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

fn policy() -> Policy {
    Policy {
        enabled: true,
        max_records: 48,
        storage: Storage::ApfsVault,
        interval_secs: Some(3600),
        max_bytes: Some(512 * 1024 * 1024),
        auto_prune: true,
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    policy: Policy,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        let policy = policy();
        store.config.checkpoints = policy.clone();
        store.save_config().unwrap();
        drop(store);
        Self {
            _temp: temp,
            home,
            policy,
        }
    }
    fn attempt(&self) -> Attempt {
        Attempt {
            id: uuid::Uuid::new_v4().to_string(),
            backend_key: key(self.policy.max_bytes.unwrap()),
            started_at: 100,
        }
    }
    fn mode(&self, value: &str) {
        crate::state::atomic_write(&self.home.join("automation-test-mode"), value.as_bytes())
            .unwrap();
    }
    fn calls(&self) -> Vec<String> {
        crate::state::read_json(&self.home.join("automation-test-calls.json")).unwrap()
    }
    fn runtime(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.home.join("checkpoint-scheduler/state.json")).unwrap(),
        )
        .unwrap()
    }
}
fn record(home: &Path, step: String) {
    let path = home.join("automation-test-calls.json");
    let mut values: Vec<String> = crate::state::read_json(&path).unwrap();
    values.push(step);
    crate::state::atomic_json(&path, &values).unwrap();
}
fn mode(home: &Path) -> String {
    fs::read_to_string(home.join("automation-test-mode")).unwrap_or_default()
}
fn published(home: &Path) -> Vec<String> {
    crate::state::read_json(&home.join("automation-test-published.json")).unwrap()
}
fn confirmed(id: &str) -> Value {
    json!({"id":id,"kind":"apfs_file_vault","confirmed":true,"state":"confirmed"})
}
fn missing(id: &str) -> Value {
    json!({"id":id,"kind":"apfs_file_vault","state":"missing","missing":true,"safe_to_retry_same_id":true})
}
fn fake_create(store: &Store, id: &str, _limits: &vault::Limits) -> Result<Value> {
    record(&store.home, format!("create:{id}"));
    match mode(&store.home).as_str() {
        "create-busy" => bail!("checkpoint payload is busy; retry after it finishes"),
        "create-error" => bail!("insufficient_quota: current checkpoint remains protected"),
        _ => (),
    }
    let mut values = published(&store.home);
    values.push(id.into());
    crate::state::atomic_json(&store.home.join("automation-test-published.json"), &values)?;
    if mode(&store.home) == "lost-reply" {
        bail!("checkpoint publication reply lost");
    }
    let mut value = confirmed(id);
    value["created"] = json!(true);
    Ok(value)
}
fn fake_reconcile(home: &Path, id: &str, _limits: &vault::Limits) -> Result<Value> {
    record(home, format!("reconcile:{id}"));
    match mode(home).as_str() {
        "reconcile-busy" => bail!("another checkpoint operation is active"),
        "reconcile-error" => bail!("unrecognized file in owned checkpoint namespace"),
        _ => (),
    }
    Ok(if published(home).iter().any(|value| value == id) {
        confirmed(id)
    } else {
        missing(id)
    })
}
fn fake_open(home: PathBuf) -> Result<Store> {
    record(&home, "open".into());
    Store::open(home)
}
fn never_open(_home: PathBuf) -> Result<Store> {
    panic!("manual checkpoint reopened the caller's Store")
}
fn disable_while_opening(home: PathBuf) -> Result<Store> {
    let mut store = Store::open(home)?;
    store.config.checkpoints.enabled = false;
    store.save_config()?;
    Ok(store)
}
fn bad_open(_home: PathBuf) -> Result<Store> {
    bail!("source state was not validated")
}
fn other_open(home: PathBuf) -> Result<Store> {
    Store::open(home.parent().unwrap().join("other-state"))
}
fn calls() -> Calls {
    Calls {
        create: fake_create,
        reconcile: fake_reconcile,
        open: fake_open,
    }
}

#[test]
fn limits_and_mode_keys_validate_without_creating_a_vault() {
    let f = Fixture::new();
    let l = limits(&f.policy).unwrap();
    assert_eq!(l.max_bytes, 512 * 1024 * 1024);
    assert_eq!(l.max_records, 48);
    assert!(l.auto_prune);
    let mut invalid = f.policy.clone();
    invalid.max_bytes = None;
    assert!(limits(&invalid).is_err());
    assert!(limits(&Policy::default()).is_err());
    let mut a = f.attempt();
    let l = retained_limits(&f.policy, &a).unwrap();
    assert_eq!(l.max_bytes, 512 * 1024 * 1024);
    assert!(!l.auto_prune);
    for key in [
        "unknown:1",
        "apfs_vault:not-a-number",
        "apfs_vault:000536870912",
        "apfs_vault:0",
    ] {
        a.backend_key = key.into();
        assert!(retained_limits(&f.policy, &a).is_err());
    }
    a = f.attempt();
    a.id = "bad".into();
    assert!(retained_limits(&f.policy, &a).is_err());
    assert!(!f.home.join("checkpoints/vault").exists());
}

#[test]
fn only_matching_verified_publications_and_explicit_missing_proofs_are_accepted() {
    let f = Fixture::new();
    let a = f.attempt();
    let mut created = confirmed(&a.id);
    created["created"] = json!(true);
    assert_eq!(
        creation(Ok(created.clone()), &a).unwrap(),
        AttemptOutcome::Completed {
            checkpoint_id: a.id.clone()
        }
    );
    created.as_object_mut().unwrap().remove("state");
    assert!(creation(Ok(created.clone()), &a).is_ok());
    assert_eq!(
        recovery(Ok(confirmed(&a.id)), &a).unwrap(),
        Recovery::Completed {
            checkpoint_id: a.id.clone()
        }
    );
    assert_eq!(
        recovery(Ok(missing(&a.id)), &a).unwrap(),
        Recovery::RetrySameAttempt
    );
    let mut invalid = vec![
        json!(null),
        json!({}),
        json!({"id":42,"kind":"apfs_file_vault"}),
        confirmed(&uuid::Uuid::new_v4().to_string()),
    ];
    let mut value = confirmed(&a.id);
    value["kind"] = json!("system_snapshot");
    invalid.push(value);
    let mut value = confirmed(&a.id);
    value["missing"] = json!(true);
    invalid.push(value);
    let mut value = confirmed(&a.id);
    value["safe_to_retry_same_id"] = json!(true);
    invalid.push(value);
    for value in invalid {
        assert!(creation(Ok(value.clone()), &a).is_err());
        assert!(recovery(Ok(value), &a).is_err());
    }
    for value in [
        json!({"id":a.id,"kind":"apfs_file_vault","created":true}),
        json!({"id":a.id,"kind":"apfs_file_vault","created":true,"confirmed":"true"}),
        json!({"id":a.id,"kind":"apfs_file_vault","created":true,"confirmed":true,"state":"unconfirmed"}),
    ] {
        assert!(creation(Ok(value), &a).is_err());
    }
    for value in [
        json!({"id":a.id,"kind":"apfs_file_vault","confirmed":true}),
        json!({"id":a.id,"kind":"apfs_file_vault","state":"missing","missing":true}),
        json!({"id":a.id,"kind":"apfs_file_vault","state":"unknown","missing":true,"safe_to_retry_same_id":true}),
        json!({"id":a.id,"kind":"apfs_file_vault","state":"unconfirmed"}),
    ] {
        assert!(recovery(Ok(value), &a).is_err());
    }
}

#[test]
fn only_known_busy_failures_defer_without_hiding_corruption_or_quota_errors() {
    let f = Fixture::new();
    let a = f.attempt();
    for message in [
        "another rdrive process is using this state directory",
        "another checkpoint operation is active",
        "a Smart request is active; retry checkpoint after it finishes",
        "a Smart request is still running; checkpoint was not created",
        "an upload is running; retry checkpoint after it finishes",
        "checkpoint payload is busy; retry after it finishes",
    ] {
        let make = || anyhow::anyhow!(message).context("capture preflight");
        assert!(matches!(
            creation(Err(make()), &a).unwrap(),
            AttemptOutcome::Deferred { .. }
        ));
        assert!(matches!(
            recovery(Err(make()), &a).unwrap(),
            Recovery::Deferred { .. }
        ));
    }
    for message in [
        "insufficient_quota: not enough vault space",
        "invalid manifest",
        "unexpected: another checkpoint operation is active",
    ] {
        assert!(creation(Err(anyhow::anyhow!(message)), &a).is_err());
        assert!(recovery(Err(anyhow::anyhow!(message)), &a).is_err());
    }
}

#[test]
fn manual_capture_uses_the_existing_store_and_reports_the_planned_cli_identity() {
    let f = Fixture::new();
    let store = Store::open(f.home.clone()).unwrap();
    let clock = || 100;
    let value = run(
        &f.home,
        &f.policy,
        Trigger::Manual,
        Some(&store),
        Calls {
            open: never_open,
            ..calls()
        },
        &clock,
    )
    .unwrap();
    assert_eq!(value["created"], true);
    assert_eq!(value["confirmed"], true);
    assert_eq!(value["id"], value["checkpoint_id"]);
    assert_eq!(value["remote_changed"], false);
    assert_eq!(
        f.calls(),
        vec![format!("create:{}", value["id"].as_str().unwrap())]
    );
    assert!(!f.home.join("checkpoints/vault").exists());
    let mut disabled = f.policy.clone();
    disabled.enabled = false;
    assert!(manual(&store, &disabled).is_err());
    assert!(manual(&store, &Policy::default()).is_err());
}

#[test]
fn scheduled_capture_opens_only_when_due_and_adopts_a_lost_reply_exactly_once() {
    let f = Fixture::new();
    let clock = Cell::new(100);
    f.mode("lost-reply");
    assert!(run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        calls(),
        &|| clock.get()
    )
    .is_err());
    let id = f.runtime()["inflight"]["id"].as_str().unwrap().to_string();
    assert_eq!(f.runtime()["failure_count"], 1);
    assert_eq!(f.calls(), vec!["open".into(), format!("create:{id}")]);
    assert_eq!(
        run(
            &f.home,
            &f.policy,
            Trigger::Scheduled,
            None,
            calls(),
            &|| clock.get()
        )
        .unwrap()["operation"],
        "not_due"
    );
    clock.set(130);
    f.mode("");
    let value = run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        calls(),
        &|| clock.get(),
    )
    .unwrap();
    assert_eq!(value["reconciled"], true);
    assert_eq!(value["created"], false);
    assert_eq!(value["id"], id);
    assert_eq!(
        f.calls(),
        vec![
            "open".into(),
            format!("create:{id}"),
            format!("reconcile:{id}")
        ]
    );
}

#[test]
fn scheduled_store_contention_and_raced_configuration_changes_do_not_count_as_failures() {
    let f = Fixture::new();
    let owner = Store::open(f.home.clone()).unwrap();
    let value = scheduled(&f.home).unwrap();
    assert_eq!(value["operation"], "deferred");
    assert_eq!(value["failure_count"], 0);
    assert!(f.calls().is_empty());
    assert!(!due(&f.home).unwrap());
    assert!(!f.home.join("checkpoints/vault").exists());
    drop(owner);
    let f = Fixture::new();
    let value = run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        Calls {
            open: disable_while_opening,
            ..calls()
        },
        &|| 100,
    )
    .unwrap();
    assert_eq!(value["operation"], "deferred");
    assert_eq!(value["failure_count"], 0);
    assert!(f.calls().is_empty());
    assert!(
        !crate::client_read::config(&f.home)
            .unwrap()
            .checkpoints
            .enabled
    );
    assert_eq!(scheduled(&f.home).unwrap()["operation"], "disabled");
}

#[test]
fn fresh_store_failures_and_cross_home_handles_do_not_publish_any_checkpoint() {
    let f = Fixture::new();
    assert!(run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        Calls {
            open: bad_open,
            ..calls()
        },
        &|| 100
    )
    .is_err());
    assert_eq!(f.runtime()["failure_count"], 1);
    assert!(f.calls().is_empty());
    let f = Fixture::new();
    assert!(run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        Calls {
            open: other_open,
            ..calls()
        },
        &|| 100
    )
    .is_err());
    assert!(f.calls().is_empty());
    let f = Fixture::new();
    let other = Store::open(f.home.parent().unwrap().join("another")).unwrap();
    let mut backend = Managed {
        home: &f.home,
        policy: &f.policy,
        borrowed: Some(&other),
        calls: calls(),
    };
    assert!(backend
        .create(&f.attempt())
        .unwrap_err()
        .to_string()
        .contains("another state directory"));
    let mut a = f.attempt();
    a.backend_key = "apfs_vault:1073741824".into();
    assert!(backend.create(&a).is_err());
    a.id = "bad-id".into();
    assert!(backend.create(&a).is_err());
    assert!(f.calls().is_empty());
}

#[test]
fn busy_create_and_reconcile_keep_the_same_id_but_real_failures_back_off() {
    let f = Fixture::new();
    f.mode("create-busy");
    let clock = Cell::new(100);
    assert_eq!(
        run(
            &f.home,
            &f.policy,
            Trigger::Scheduled,
            None,
            calls(),
            &|| clock.get()
        )
        .unwrap()["operation"],
        "deferred"
    );
    let id = f.runtime()["inflight"]["id"].as_str().unwrap().to_string();
    assert_eq!(f.runtime()["failure_count"], 0);
    clock.set(130);
    f.mode("reconcile-busy");
    assert_eq!(
        run(
            &f.home,
            &f.policy,
            Trigger::Scheduled,
            None,
            calls(),
            &|| clock.get()
        )
        .unwrap()["operation"],
        "deferred"
    );
    assert_eq!(f.runtime()["failure_count"], 0);
    assert_eq!(f.runtime()["inflight"]["id"], id);
    clock.set(160);
    f.mode("reconcile-error");
    assert!(run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        calls(),
        &|| clock.get()
    )
    .is_err());
    assert_eq!(f.runtime()["failure_count"], 1);
    clock.set(190);
    f.mode("create-error");
    assert!(run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        calls(),
        &|| clock.get()
    )
    .is_err());
    assert_eq!(f.runtime()["inflight"]["id"], id);
}

#[test]
fn status_and_due_probes_never_open_store_or_mount_a_vault() {
    for change in ["native", "disabled", "manual"] {
        let f = Fixture::new();
        let mut store = Store::open(f.home.clone()).unwrap();
        match change {
            "native" => store.config.checkpoints = Policy::default(),
            "disabled" => store.config.checkpoints.enabled = false,
            "manual" => store.config.checkpoints.interval_secs = None,
            _ => unreachable!(),
        };
        store.save_config().unwrap();
        assert!(!due(&f.home).unwrap());
        assert_eq!(scheduled(&f.home).unwrap()["created"], false);
        assert!(!f.home.join("checkpoint-scheduler").exists());
        assert!(!f.home.join("checkpoints/vault").exists());
    }
    let f = Fixture::new();
    let store = Store::open(f.home.clone()).unwrap();
    assert_eq!(
        schedule_status(&f.home, &f.policy).unwrap()["operation"],
        "due"
    );
    assert!(due(&f.home).unwrap());
    assert!(!pending(&f.home, &uuid::Uuid::new_v4().to_string()).unwrap());
    assert!(pending(&f.home, "bad").is_err());
    run(
        &f.home,
        &f.policy,
        Trigger::Manual,
        Some(&store),
        Calls {
            open: never_open,
            ..calls()
        },
        &now,
    )
    .unwrap();
    assert!(!due(&f.home).unwrap());
    let prior_calls = f.calls();
    assert_eq!(scheduled(&f.home).unwrap()["operation"], "not_due");
    assert_eq!(f.calls(), prior_calls);
    drop(store);
    let f = Fixture::new();
    f.mode("create-busy");
    let clock = || 100;
    run(
        &f.home,
        &f.policy,
        Trigger::Scheduled,
        None,
        calls(),
        &clock,
    )
    .unwrap();
    let id = f.runtime()["inflight"]["id"].as_str().unwrap().to_string();
    assert!(pending(&f.home, &id).unwrap());
    assert!(!pending(&f.home, &uuid::Uuid::new_v4().to_string()).unwrap());
    assert!(!due_with(&f.home, &f.policy, 129).unwrap());
    assert!(due_with(&f.home, &f.policy, 130).unwrap());
    let mut invalid = f.policy.clone();
    invalid.interval_secs = Some(0);
    assert!(due_with(&f.home, &invalid, 100).is_err());
}

#[test]
fn malformed_shared_configuration_or_scheduler_state_is_reported_without_repairing_it() {
    let f = Fixture::new();
    crate::state::atomic_write(&f.home.join("config.json"), b"invalid").unwrap();
    assert!(due(&f.home).is_err());
    assert!(scheduled(&f.home).is_err());
    let f = Fixture::new();
    fs::create_dir(f.home.join("checkpoint-scheduler")).unwrap();
    fs::set_permissions(
        f.home.join("checkpoint-scheduler"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    crate::state::atomic_write(&f.home.join("checkpoint-scheduler/state.json"), b"invalid")
        .unwrap();
    assert!(schedule_status(&f.home, &f.policy).is_err());
    assert!(pending(&f.home, &uuid::Uuid::new_v4().to_string()).is_err());
    assert_eq!(
        fs::read(f.home.join("checkpoint-scheduler/state.json")).unwrap(),
        b"invalid"
    );
    assert!(!f.home.join("checkpoints/vault").exists());
}
