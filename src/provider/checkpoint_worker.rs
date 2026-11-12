//! Run only due checkpoint work without blocking the bridge or starving behind
//! read-only metadata scans. Durable attempt recovery belongs to checkpoints.
use super::admin::Priority;
use anyhow::Result;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

const POLL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Worker {
    active: Option<JoinHandle<()>>,
    next_poll: Option<Instant>,
}

impl Worker {
    pub(super) fn schedule(
        &mut self,
        home: &Path,
        coordinator: Arc<Mutex<()>>,
        priority: Arc<Priority>,
    ) -> bool {
        self.schedule_with(
            home,
            coordinator,
            priority,
            Instant::now(),
            crate::checkpoints::scheduled_due,
            crate::checkpoints::scheduled,
        )
    }

    fn schedule_with<D, F>(
        &mut self,
        home: &Path,
        coordinator: Arc<Mutex<()>>,
        priority: Arc<Priority>,
        now: Instant,
        due: D,
        run: F,
    ) -> bool
    where
        D: FnOnce(&Path) -> Result<bool>,
        F: FnOnce(&Path) -> Result<Value> + Send + 'static,
    {
        if self.active.as_ref().is_some_and(|task| !task.is_finished()) {
            return false;
        }
        self.active.take();
        if self.next_poll.is_some_and(|next| now < next) {
            return false;
        }
        self.next_poll = Some(now + POLL);
        match due(home) {
            Ok(false) => return false,
            Err(_) => {
                eprintln!("RDrive checkpoint scheduler: configuration or schedule could not be read; inspect checkpoint status.");
                return false;
            }
            Ok(true) => (),
        }
        if priority.requested() {
            return false;
        }
        let home: PathBuf = home.into();
        self.active = Some(tokio::spawn(async move {
            // This cancels only existing read-only phases. A remote mutation
            // already holding the coordinator completes its durable receipt.
            let _waiting = priority.request();
            let guard = coordinator.lock_owned().await;
            let result = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                run(&home)
            })
            .await;
            match result {
                Ok(Ok(_))=>(),
                Ok(Err(_))=>eprintln!("RDrive checkpoint scheduler: creation did not complete; inspect checkpoint status."),
                Err(_)=>eprintln!("RDrive checkpoint scheduler: worker interrupted or panicked; retained work will be reconciled."),
            }
        }));
        true
    }

    /// Keep the current process alive until an admitted OS/file operation ends.
    /// Process death is separately covered by the durable attempt protocol.
    pub(super) async fn shutdown(&mut self) {
        if let Some(task) = self.active.take() {
            if task.await.is_err() {
                eprintln!("RDrive checkpoint scheduler: task interrupted before normal shutdown.");
            }
        }
    }
}

#[cfg(test)]
#[path = "checkpoint_worker_tests.rs"]
mod tests;
