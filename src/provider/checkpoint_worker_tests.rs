use super::*;
use anyhow::bail;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use tokio::sync::oneshot;

fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
fn success(_: &Path) -> Result<Value> {
    Ok(serde_json::json!({"completed":true}))
}
async fn wait_for(mut test: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !test() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_due_and_invalid_configuration_do_not_interrupt_metadata_or_create_work() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let now = Instant::now();
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now,
        |_| Ok(false),
        |_| panic!("not due")
    ));
    assert!(!priority.requested());
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + Duration::from_secs(1),
        |_| panic!("throttled"),
        success
    ));
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL,
        |_| bail!("invalid config"),
        success
    ));
    assert!(!priority.requested());
    assert!(coordinator.try_lock().is_ok());
    worker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_checkpoint_requests_priority_and_waits_for_the_existing_read_phase() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let held = coordinator.clone().lock_owned().await;
    let (sent, received) = oneshot::channel();
    let p = priority.clone();
    let owner = tokio::spawn(async move {
        wait_for(|| p.requested()).await;
        drop(held);
        sent.send(()).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        Instant::now(),
        |_| Ok(true),
        move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            success(Path::new("unused"))
        }
    ));
    received.await.unwrap();
    worker.shutdown().await;
    owner.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!priority.requested());
    assert!(coordinator.try_lock().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_capture_keeps_singleflight_priority_and_coordinator_until_shutdown_finishes() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let now = Instant::now();
    let (sent, started) = oneshot::channel();
    let (release, gate) = mpsc::channel();
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now,
        |_| Ok(true),
        move |_| {
            sent.send(()).unwrap();
            gate.recv().unwrap();
            success(Path::new("unused"))
        }
    ));
    started.await.unwrap();
    assert!(priority.requested());
    assert!(coordinator.try_lock().is_err());
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL,
        |_| panic!("active"),
        success
    ));
    release.send(()).unwrap();
    worker.shutdown().await;
    assert!(!priority.requested());
    assert!(coordinator.try_lock().is_ok());
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL,
        |_| Ok(true),
        success
    ));
    worker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn administrator_waiters_take_precedence_and_completed_work_can_be_polled_again() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let now = Instant::now();
    let waiting = priority.request();
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now,
        |_| Ok(true),
        |_| panic!("administrator priority")
    ));
    drop(waiting);
    assert!(!priority.requested());
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL,
        |_| Ok(true),
        success
    ));
    wait_for(|| worker.active.as_ref().unwrap().is_finished()).await;
    assert!(!worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL + Duration::from_secs(1),
        |_| panic!("poll interval"),
        success
    ));
    assert!(worker.active.is_none());
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL + POLL,
        |_| Ok(true),
        success
    ));
    worker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_or_panicked_blocking_tasks_release_admission_and_allow_later_work() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let now = Instant::now();
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now,
        |_| Ok(true),
        |_| bail!("synthetic creation failure")
    ));
    worker.shutdown().await;
    assert!(!priority.requested());
    assert!(coordinator.try_lock().is_ok());
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL,
        |_| Ok(true),
        |_| panic!("synthetic worker panic")
    ));
    worker.shutdown().await;
    assert!(!priority.requested());
    assert!(coordinator.try_lock().is_ok());
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        now + POLL + POLL,
        |_| Ok(true),
        success
    ));
    worker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_waiting_task_does_not_run_capture_and_shutdown_observes_it() {
    let home = home();
    let coordinator = Arc::new(Mutex::new(()));
    let priority = Arc::new(Priority::default());
    let mut worker = Worker::default();
    let held = coordinator.clone().lock_owned().await;
    assert!(worker.schedule_with(
        home.path(),
        coordinator.clone(),
        priority.clone(),
        Instant::now(),
        |_| Ok(true),
        |_| panic!("cancelled before admission")
    ));
    wait_for(|| priority.requested()).await;
    worker.active.as_ref().unwrap().abort();
    worker.shutdown().await;
    drop(held);
    assert!(!priority.requested());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_schedule_respects_disabled_configuration_without_creating_a_vault() {
    let fixture = home();
    let store = crate::state::Store::open(fixture.path().join("state")).unwrap();
    store.save_config().unwrap();
    let path = store.home.clone();
    drop(store);
    let mut worker = Worker::default();
    assert!(!worker.schedule(
        &path,
        Arc::new(Mutex::new(())),
        Arc::new(Priority::default())
    ));
    assert!(!path.join("checkpoints/vault").exists());
    worker.shutdown().await;
}
