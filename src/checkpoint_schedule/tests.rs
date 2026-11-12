use super::*;
use std::{
    cell::Cell,
    collections::{BTreeSet, VecDeque},
    os::unix::fs::{symlink, PermissionsExt},
    rc::Rc,
};

#[derive(Clone)]
enum Create {
    Success,
    LostReply,
    Error(String),
    Deferred(String),
    WrongId,
}
enum Resume {
    Completed,
    Retry,
    Error,
    Deferred,
}
struct Fake {
    home: PathBuf,
    clock: Rc<Cell<i64>>,
    advance: i64,
    creates: Vec<Attempt>,
    reconciles: Vec<Attempt>,
    published: BTreeSet<String>,
    create: VecDeque<Create>,
    resume: VecDeque<Resume>,
}
impl Backend for Fake {
    fn create(&mut self, a: &Attempt) -> Result<AttemptOutcome> {
        let saved = Directory::open(&self.home, false)?.unwrap().read()?;
        assert_eq!(
            saved.inflight.as_ref(),
            Some(a),
            "The attempt must be durable before any publication"
        );
        self.creates.push(a.clone());
        self.clock.set(self.clock.get() + self.advance);
        match self.create.pop_front().unwrap_or(Create::Success) {
            Create::Success => {
                self.published.insert(a.id.clone());
                Ok(AttemptOutcome::Completed {
                    checkpoint_id: a.id.clone(),
                })
            }
            Create::LostReply => {
                self.published.insert(a.id.clone());
                bail!("publication reply lost")
            }
            Create::Error(text) => bail!(text),
            Create::Deferred(reason) => Ok(AttemptOutcome::Deferred { reason }),
            Create::WrongId => Ok(AttemptOutcome::Completed {
                checkpoint_id: uuid::Uuid::new_v4().to_string(),
            }),
        }
    }
    fn reconcile(&mut self, a: &Attempt) -> Result<Recovery> {
        self.reconciles.push(a.clone());
        match self.resume.pop_front() {
            Some(Resume::Completed) => Ok(Recovery::Completed {
                checkpoint_id: a.id.clone(),
            }),
            Some(Resume::Retry) => Ok(Recovery::RetrySameAttempt),
            Some(Resume::Error) => bail!("cannot establish publication outcome"),
            Some(Resume::Deferred) => Ok(Recovery::Deferred {
                reason: "owned publication still running".into(),
            }),
            None if self.published.contains(&a.id) => Ok(Recovery::Completed {
                checkpoint_id: a.id.clone(),
            }),
            None => Ok(Recovery::RetrySameAttempt),
        }
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    options: Options,
    clock: Rc<Cell<i64>>,
    backend: Fake,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("state");
        fs::create_dir(&home).unwrap();
        mode(&home, 0o700);
        let clock = Rc::new(Cell::new(100));
        let backend = Fake {
            home: home.clone(),
            clock: clock.clone(),
            advance: 0,
            creates: vec![],
            reconciles: vec![],
            published: BTreeSet::new(),
            create: VecDeque::new(),
            resume: VecDeque::new(),
        };
        Self {
            _temp: temp,
            home,
            options: Options {
                enabled: true,
                interval_secs: Some(60),
                backend_key: "vault:one".into(),
            },
            clock,
            backend,
        }
    }
    fn tick(&mut self, trigger: Trigger) -> Result<Value> {
        let clock = self.clock.clone();
        tick(
            &self.home,
            &self.options,
            trigger,
            &move || clock.get(),
            &mut self.backend,
        )
    }
    fn state(&self) -> Runtime {
        Directory::open(&self.home, false)
            .unwrap()
            .unwrap()
            .read()
            .unwrap()
    }
    fn save(&self, state: &Runtime) {
        Directory::open(&self.home, true)
            .unwrap()
            .unwrap()
            .save(state)
            .unwrap();
    }
    fn status(&self) -> Result<Value> {
        status(&self.home, &self.options, self.clock.get())
    }
    fn attempt(&self) -> Attempt {
        Attempt {
            id: uuid::Uuid::new_v4().to_string(),
            backend_key: self.options.backend_key.clone(),
            started_at: 100,
        }
    }
    fn planned(&self) -> Runtime {
        Runtime {
            inflight: Some(self.attempt()),
            ..Default::default()
        }
    }
}
fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}
fn write_private(path: &Path, raw: impl AsRef<[u8]>) {
    fs::write(path, raw).unwrap();
    mode(path, 0o600);
}

#[test]
fn disabled_manual_only_and_status_are_read_only_and_do_not_publish() {
    let mut f = Fixture::new();
    f.options.enabled = false;
    assert_eq!(f.status().unwrap()["operation"], "disabled");
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "disabled");
    assert!(f
        .tick(Trigger::Manual)
        .unwrap_err()
        .to_string()
        .contains("disabled"));
    f.options.enabled = true;
    f.options.interval_secs = None;
    assert_eq!(
        f.tick(Trigger::Scheduled).unwrap()["operation"],
        "manual_only"
    );
    assert!(!f.home.join("checkpoint-scheduler").exists());
    assert_eq!(f.tick(Trigger::Manual).unwrap()["created"], true);
    let path = f.home.join("checkpoint-scheduler/state.json");
    let before = fs::read(&path).unwrap();
    assert_eq!(f.status().unwrap()["next_due_at"], Value::Null);
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(f.backend.creates.len(), 1);
}

#[test]
fn completion_clock_drives_next_due_without_catch_up_or_read_side_effects() {
    let mut f = Fixture::new();
    assert_eq!(f.status().unwrap()["operation"], "due");
    f.backend.advance = 90;
    let first = f.tick(Trigger::Scheduled).unwrap();
    assert_eq!(first["next_due_at"], 250);
    assert_eq!(first["last_success"]["completed_at"], 190);
    let before = fs::read(f.home.join("checkpoint-scheduler/state.json")).unwrap();
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "not_due");
    assert_eq!(f.status().unwrap()["operation"], "not_due");
    assert_eq!(
        fs::read(f.home.join("checkpoint-scheduler/state.json")).unwrap(),
        before
    );
    f.clock.set(10_000);
    let second = f.tick(Trigger::Scheduled).unwrap();
    assert_eq!(second["next_due_at"], 10_150);
    assert_eq!(f.backend.creates.len(), 2);
    assert!(f.backend.reconciles.is_empty());
    f.clock.set(10_000);
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "not_due");
    f.clock.set(50);
    assert_eq!(f.status().unwrap()["operation"], "not_due");
    assert_eq!(f.backend.creates.len(), 2);
}

#[test]
fn publication_reply_loss_reconciles_the_same_id_after_restart_without_duplicate_creation() {
    let mut f = Fixture::new();
    f.backend.create.push_back(Create::LostReply);
    assert!(f
        .tick(Trigger::Scheduled)
        .unwrap_err()
        .to_string()
        .contains("retained"));
    let planned = f.state().inflight.unwrap();
    assert_eq!(f.backend.creates[0], planned);
    assert_eq!(f.status().unwrap()["operation"], "backoff");
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "not_due");
    f.clock.set(130);
    assert_eq!(f.status().unwrap()["operation"], "recovery_due");
    let recovered = f.tick(Trigger::Scheduled).unwrap();
    assert_eq!(recovered["reconciled"], true);
    assert_eq!(recovered["checkpoint_id"], planned.id);
    assert_eq!(recovered["created"], false);
    assert_eq!(f.backend.creates.len(), 1);
    assert_eq!(f.backend.reconciles, vec![planned]);
    assert_eq!(f.state().failure_count, 0);
    assert!(f.state().inflight.is_none());
}

#[test]
fn busy_backend_defers_without_counting_failures_or_changing_planned_identity() {
    let mut f = Fixture::new();
    f.backend
        .create
        .push_back(Create::Deferred("queue lease held".into()));
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "deferred");
    let a = f.state().inflight.unwrap();
    assert_eq!(f.state().failure_count, 0);
    assert_eq!(f.state().retry_at, Some(130));
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "not_due");
    f.clock.set(130);
    f.backend.resume.push_back(Resume::Deferred);
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "deferred");
    assert_eq!(f.state().failure_count, 0);
    assert_eq!(f.state().inflight.as_ref(), Some(&a));
    f.clock.set(160);
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["created"], true);
    assert_eq!(f.backend.creates, vec![a.clone(), a]);
}

#[test]
fn unknown_recovery_never_creates_and_exponential_backoff_is_bounded_then_reset() {
    let mut f = Fixture::new();
    f.backend
        .create
        .push_back(Create::Error("first failure".into()));
    f.tick(Trigger::Scheduled).unwrap_err();
    let id = f.state().inflight.unwrap().id;
    f.backend.resume.push_back(Resume::Error);
    f.clock.set(130);
    f.tick(Trigger::Scheduled).unwrap_err();
    assert_eq!(f.backend.creates.len(), 1);
    assert_eq!(f.state().retry_at, Some(190));
    for _ in 0..10 {
        f.clock.set(f.state().retry_at.unwrap());
        f.backend
            .create
            .push_back(Create::Error("host unavailable".into()));
        f.tick(Trigger::Scheduled).unwrap_err();
        assert_eq!(f.state().inflight.unwrap().id, id);
        assert!((30..=3600).contains(&(f.state().retry_at.unwrap() - f.clock.get())));
    }
    assert_eq!(f.state().retry_at.unwrap() - f.clock.get(), 3600);
    f.backend
        .create
        .push_back(Create::Deferred("busy after error".into()));
    let failures = f.state().failure_count;
    f.tick(Trigger::Manual).unwrap();
    assert_eq!(f.state().failure_count, failures);
    assert!(f.state().last_error.is_some());
    f.clock.set(f.state().retry_at.unwrap());
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["created"], true);
    assert_eq!(f.state().failure_count, 0);
    assert!(f.state().last_error.is_none());
    assert!(f.state().retry_at.is_none());
}

#[test]
fn a_changed_backend_is_reconciled_before_a_new_storage_identity_can_publish() {
    let mut f = Fixture::new();
    f.backend
        .create
        .push_back(Create::Deferred("not yet published".into()));
    f.tick(Trigger::Scheduled).unwrap();
    let old = f.state().inflight.unwrap();
    f.options.backend_key = "vault:two".into();
    f.backend.resume.push_back(Resume::Deferred);
    f.tick(Trigger::Manual).unwrap();
    assert_eq!(f.backend.creates.len(), 1);
    assert_eq!(f.state().inflight.unwrap(), old);
    f.backend.resume.push_back(Resume::Retry);
    let created = f.tick(Trigger::Manual).unwrap();
    assert_eq!(created["created"], true);
    assert_eq!(f.backend.creates[1].backend_key, "vault:two");
    assert_ne!(f.backend.creates[1].id, old.id);
    assert_eq!(f.backend.reconciles[0], old);
    assert_eq!(f.backend.reconciles[1], old);
    let mut next = Fixture::new();
    next.backend.create.push_back(Create::LostReply);
    next.tick(Trigger::Scheduled).unwrap_err();
    next.options.backend_key = "vault:new".into();
    next.backend.resume.push_back(Resume::Completed);
    assert_eq!(next.tick(Trigger::Manual).unwrap()["reconciled"], true);
    assert_eq!(next.status().unwrap()["operation"], "due");
    assert_eq!(next.backend.creates.len(), 1);
    next.tick(Trigger::Scheduled).unwrap();
    assert_eq!(next.backend.creates.len(), 2);
}

#[test]
fn one_process_lease_prevents_overlapping_manual_and_scheduled_calls() {
    let mut f = Fixture::new();
    let directory = Directory::open(&f.home, true).unwrap().unwrap();
    let held = directory.lock().unwrap().unwrap();
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["operation"], "busy");
    assert_eq!(f.tick(Trigger::Manual).unwrap()["operation"], "busy");
    assert!(f.backend.creates.is_empty());
    drop(held);
    assert_eq!(f.tick(Trigger::Manual).unwrap()["created"], true);
    assert!(lock_result(-1, std::io::Error::from_raw_os_error(libc::EIO)).is_err());
}

#[test]
fn every_planning_and_completion_persistence_failure_preserves_recovery_authority() {
    for cut in [1, 2] {
        for after_write in [false, true] {
            let mut f = Fixture::new();
            let clock = f.clock.clone();
            let mut writes = 0;
            let result = tick_with_save(
                &f.home,
                &f.options,
                Trigger::Scheduled,
                &move || clock.get(),
                &mut f.backend,
                |d, s| {
                    writes += 1;
                    if writes == cut && !after_write {
                        bail!("injected persistence failure");
                    }
                    d.save(s)?;
                    if writes == cut && after_write {
                        bail!("injected fsync outcome uncertainty");
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(f.backend.creates.len(), usize::from(cut == 2));
            f.clock.set(100);
            let reply = f.tick(Trigger::Scheduled).unwrap();
            if cut == 2 && after_write {
                assert_eq!(reply["operation"], "not_due");
            } else if cut == 2 {
                assert_eq!(reply["reconciled"], true);
            } else {
                assert_eq!(reply["created"], true);
            }
            assert_eq!(f.backend.creates.len(), 1);
        }
    }
}

#[test]
fn failed_retry_and_deferral_status_saves_keep_the_original_durable_attempt() {
    for action in [
        Create::Error("host failed".into()),
        Create::Deferred("host busy".into()),
    ] {
        let mut f = Fixture::new();
        f.backend.create.push_back(action);
        let clock = f.clock.clone();
        let mut writes = 0;
        assert!(tick_with_save(
            &f.home,
            &f.options,
            Trigger::Scheduled,
            &move || clock.get(),
            &mut f.backend,
            |d, s| {
                writes += 1;
                if writes == 2 {
                    bail!("could not record outcome");
                }
                d.save(s)
            }
        )
        .is_err());
        let retained = f.state().inflight.unwrap();
        assert_eq!(f.backend.creates, vec![retained.clone()]);
        assert_eq!(f.tick(Trigger::Scheduled).unwrap()["created"], true);
        assert_eq!(f.backend.creates[1], retained);
    }
    let mut f = Fixture::new();
    f.save(&f.planned());
    f.backend.resume.push_back(Resume::Deferred);
    let clock = f.clock.clone();
    assert!(tick_with_save(
        &f.home,
        &f.options,
        Trigger::Scheduled,
        &move || clock.get(),
        &mut f.backend,
        |_, _| bail!("save blocked")
    )
    .is_err());
    assert!(f.backend.creates.is_empty());
    assert!(f.state().inflight.is_some());
}

#[test]
fn wrong_backend_identity_and_invalid_completion_clocks_do_not_report_success() {
    let mut f = Fixture::new();
    f.backend.create.push_back(Create::WrongId);
    assert!(f
        .tick(Trigger::Scheduled)
        .unwrap_err()
        .to_string()
        .contains("retained"));
    assert!(f.state().inflight.is_some());
    let mut f = Fixture::new();
    f.backend.advance = MAX_TIMESTAMP - 100;
    assert!(f.tick(Trigger::Scheduled).is_err());
    assert!(f.state().inflight.is_some());
    assert_eq!(f.backend.creates.len(), 1);
    f.clock.set(500);
    f.backend.advance = 0;
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["reconciled"], true);
    assert_eq!(f.backend.creates.len(), 1);
    let mut f = Fixture::new();
    f.clock.set(-1);
    assert!(f.tick(Trigger::Manual).is_err());
    assert!(f.status().is_err());
    assert!(f.backend.creates.is_empty());
    assert!(after(i64::MAX, 1).is_err());
    assert!(after(MAX_TIMESTAMP, 1).is_err());
}

#[test]
fn policy_and_retained_state_bounds_reject_invalid_inputs_without_resetting_them() {
    let mut f = Fixture::new();
    for interval in [0, 59, MAX_INTERVAL_SECS + 1] {
        f.options.interval_secs = Some(interval);
        assert!(f.tick(Trigger::Manual).is_err());
    }
    f.options.interval_secs = Some(MAX_INTERVAL_SECS);
    assert!(f.options.validate().is_ok());
    for key in ["".to_string(), "x".repeat(129), "key\n".into()] {
        f.options.backend_key = key;
        assert!(f.status().is_err());
    }
    f.options.backend_key = "vault:one".into();
    f.options.interval_secs = Some(60);
    let directory = Directory::open(&f.home, true).unwrap().unwrap();
    let path = directory.path.join("state.json");
    for raw in [
        b"".as_slice(),
        b"not JSON",
        b"{}",
        b"{\"version\":1,\"failure_count\":0,\"unknown\":true}",
    ] {
        write_private(&path, raw);
        let before = fs::read(&path).unwrap();
        assert!(f.status().is_err());
        assert!(f.tick(Trigger::Scheduled).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    write_private(&path, vec![b' '; (MAX_STATE_BYTES + 1) as usize]);
    assert!(directory.read().is_err());
    assert!(decode(&vec![b' '; (MAX_STATE_BYTES + 1) as usize]).is_err());
    fs::remove_file(&path).unwrap();
    let mut states = vec![];
    let mut s = Runtime::default();
    s.version = 2;
    states.push(s);
    let mut s = f.planned();
    s.inflight.as_mut().unwrap().id = "NOT-AN-ID".into();
    states.push(s);
    let mut s = f.planned();
    s.inflight.as_mut().unwrap().id = s.inflight.as_ref().unwrap().id.to_uppercase();
    states.push(s);
    let mut s = f.planned();
    s.inflight.as_mut().unwrap().backend_key.clear();
    states.push(s);
    let mut s = f.planned();
    s.inflight.as_mut().unwrap().started_at = -1;
    states.push(s);
    let mut s = Runtime::default();
    s.last_success = Some(Success {
        checkpoint_id: uuid::Uuid::new_v4().to_string(),
        backend_key: "".into(),
        completed_at: 0,
    });
    states.push(s);
    let mut s = f.planned();
    s.retry_at = Some(50);
    states.push(s);
    let mut s = Runtime::default();
    s.failure_count = 1;
    states.push(s);
    for text in ["".to_string(), "x".repeat(1025), "bad\n".into()] {
        let mut s = f.planned();
        s.last_error = Some(text);
        states.push(s);
    }
    for state in states {
        write_private(&path, serde_json::to_vec(&state).unwrap());
        assert!(directory.read().is_err());
        assert!(directory.save(&state).is_err());
    }
    fs::remove_file(&path).unwrap();
    let mut enormous = Runtime::default();
    enormous.last_error = Some("x".repeat(MAX_STATE_BYTES as usize));
    assert!(directory
        .save(&enormous)
        .unwrap_err()
        .to_string()
        .contains("size bound"));
    assert!(f.backend.creates.is_empty());
}

#[test]
fn private_paths_refuse_links_nonregular_files_and_untrusted_permissions() {
    let f = Fixture::new();
    let directory = Directory::open(&f.home, true).unwrap().unwrap();
    let path = directory.path.join("state.json");
    let outside = f._temp.path().join("outside");
    write_private(&outside, b"original");
    symlink(&outside, &path).unwrap();
    assert!(directory.read().is_err());
    fs::remove_file(&path).unwrap();
    fs::hard_link(&outside, &path).unwrap();
    assert!(directory.read().is_err());
    fs::remove_file(&path).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"original");
    fs::create_dir(&path).unwrap();
    assert!(directory.read().is_err());
    fs::remove_dir(&path).unwrap();
    write_private(&path, serde_json::to_vec(&Runtime::default()).unwrap());
    mode(&path, 0o644);
    assert!(directory.read().is_err());
    fs::remove_file(&path).unwrap();
    symlink(&outside, directory.path.join("lock")).unwrap();
    assert!(directory.lock().is_err());
    fs::remove_file(directory.path.join("lock")).unwrap();
    write_private(&directory.path.join("lock"), b"not a lease");
    assert!(directory.lock().is_err());
    fs::remove_file(directory.path.join("lock")).unwrap();
    let linked_lock = f._temp.path().join("lock-source");
    write_private(&linked_lock, b"");
    fs::hard_link(&linked_lock, directory.path.join("lock")).unwrap();
    assert!(directory.lock().is_err());
    fs::remove_file(directory.path.join("lock")).unwrap();
    mode(&directory.path, 0o755);
    assert!(Directory::open(&f.home, false).is_err());
    mode(&directory.path, 0o700);
    fs::remove_dir(&directory.path).unwrap();
    symlink(f._temp.path(), &directory.path).unwrap();
    assert!(Directory::open(&f.home, true).is_err());
    fs::remove_file(&directory.path).unwrap();
    write_private(&directory.path, b"not a directory");
    assert!(Directory::open(&f.home, false).is_err());
    fs::remove_file(&directory.path).unwrap();
    let fake_home = f._temp.path().join("home-link");
    symlink(&f.home, &fake_home).unwrap();
    assert!(Directory::open(&fake_home, false).is_err());
}

#[test]
fn inaccessible_scheduler_paths_and_atomic_save_errors_are_not_ignored() {
    let f = Fixture::new();
    let directory = Directory::open(&f.home, true).unwrap().unwrap();
    let path = directory.path.join("state.json");
    write_private(&path, b"invalid");
    assert!(directory.save(&Runtime::default()).is_err());
    fs::remove_file(path).unwrap();
    mode(&f.home, 0o000);
    let read = Directory::open(&f.home, false);
    let create = Directory::open(&f.home, true);
    mode(&f.home, 0o700);
    assert!(read.is_err());
    assert!(create.is_err());
    mode(&directory.path, 0o500);
    let save = directory.save(&Runtime::default());
    mode(&directory.path, 0o700);
    assert!(save.is_err());
    assert!(fs::read_dir(&directory.path).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".schedule-")));
}

#[test]
fn diagnostics_are_bounded_and_retry_counters_saturate_safely() {
    assert_eq!(diagnostic("\n\0"), "Checkpoint operation did not finish");
    let unicode = diagnostic(&"界".repeat(600));
    assert!(unicode.len() <= 1024);
    assert!(!unicode.is_empty());
    let mut f = Fixture::new();
    let mut state = f.planned();
    state.failure_count = u32::MAX;
    state.last_error = Some("previous".into());
    state.retry_at = Some(0);
    f.save(&state);
    f.backend.create.push_back(Create::Error("x".repeat(2000)));
    f.tick(Trigger::Manual).unwrap_err();
    assert_eq!(f.state().failure_count, u32::MAX);
    assert_eq!(f.state().last_error.unwrap().len(), 1024);
    assert_eq!(f.state().retry_at, Some(3700));
}

#[test]
fn retry_deadline_overflow_preserves_attempt_for_recovery_after_clock_correction() {
    let mut f = Fixture::new();
    f.backend.advance = MAX_TIMESTAMP - 100;
    f.backend
        .create
        .push_back(Create::Error("host failed at clock boundary".into()));
    assert!(f.tick(Trigger::Scheduled).is_err());
    let attempt = f.state().inflight.unwrap();
    assert_eq!(f.backend.creates, vec![attempt.clone()]);
    f.backend.advance = 0;
    f.clock.set(500);
    assert_eq!(f.tick(Trigger::Scheduled).unwrap()["created"], true);
    assert_eq!(f.backend.creates[1], attempt);
}

#[test]
fn contradictory_receipts_and_detached_failure_state_are_never_reset_as_idle() {
    let f = Fixture::new();
    let mut states = Vec::new();
    let mut orphan = Runtime::default();
    orphan.last_deferred = Some("busy".into());
    orphan.retry_at = Some(100);
    states.push(orphan);
    let mut missing_deadline = f.planned();
    missing_deadline.last_deferred = Some("busy".into());
    states.push(missing_deadline);
    let mut error_without_attempt_count = f.planned();
    error_without_attempt_count.last_error = Some("host error".into());
    error_without_attempt_count.retry_at = Some(100);
    states.push(error_without_attempt_count);
    let mut contradictory = f.planned();
    contradictory.last_success = Some(Success {
        checkpoint_id: contradictory.inflight.as_ref().unwrap().id.clone(),
        backend_key: f.options.backend_key.clone(),
        completed_at: 100,
    });
    states.push(contradictory);
    let directory = Directory::open(&f.home, true).unwrap().unwrap();
    for state in states {
        let raw = serde_json::to_vec(&state).unwrap();
        write_private(&directory.path.join("state.json"), &raw);
        assert!(f.status().is_err());
        assert_eq!(fs::read(directory.path.join("state.json")).unwrap(), raw);
    }
}

#[test]
fn unsynced_parent_directory_never_authorizes_a_checkpoint_publication() {
    for scenario in 0..3 {
        let mut f = Fixture::new();
        if scenario == 1 {
            Directory::open(&f.home, true).unwrap();
        }
        if scenario == 2 {
            f.tick(Trigger::Manual).unwrap();
        }
        let state_path = f.home.join("checkpoint-scheduler/state.json");
        let prior = fs::read(&state_path).ok();
        let publications = f.backend.creates.len();
        let clock = f.clock.clone();
        let expected_parent = f.home.clone();
        let result = tick_with_save(
            &f.home,
            &f.options,
            Trigger::Manual,
            &move || clock.get(),
            &mut f.backend,
            |directory, state| {
                directory.save_with_parent_sync(state, |parent| {
                    assert_eq!(parent, expected_parent);
                    bail!("injected parent-directory fsync failure");
                })
            },
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("parent-directory fsync"));
        assert_eq!(f.backend.creates.len(), publications);
        assert_eq!(fs::read(&state_path).ok(), prior);
        assert_eq!(f.tick(Trigger::Manual).unwrap()["created"], true);
        assert_eq!(f.backend.creates.len(), publications + 1);
    }
}
