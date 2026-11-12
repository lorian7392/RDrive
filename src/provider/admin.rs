//! Schedule configuration changes between durable synchronization phases.
use super::*;
use std::{future::Future, sync::atomic::{AtomicUsize, Ordering}, time::Duration};
use tokio::{io::{AsyncRead, AsyncWrite}, sync::Notify};

#[derive(Default)]
pub(super) struct Priority {
    waiting: AtomicUsize,
    changed: Notify,
}

struct Waiting<'a>(&'a Priority);
impl Drop for Waiting<'_> {
    fn drop(&mut self) { self.0.waiting.fetch_sub(1, Ordering::SeqCst); }
}
impl Priority {
    fn request(&self) -> Waiting<'_> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        Waiting(self)
    }
    pub(super) fn requested(&self) -> bool { self.waiting.load(Ordering::SeqCst) != 0 }
    async fn requested_async(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.requested() { return; }
            changed.await;
        }
    }
}

pub(super) struct Service {
    pub registration: Arc<crate::profile_admin::Registration>,
    pub priority: Arc<Priority>,
}

/// Only read-only background requests use this cancellation point. Uploads and
/// other remote mutations finish through their existing receipt/recovery path.
pub(super) enum ReadPhase<T> { Finished(T), TimedOut, Deferred }

pub(super) async fn read_phase<F: Future>(priority: Option<&Priority>, timeout: Duration, read: F) -> ReadPhase<F::Output> {
    if let Some(priority) = priority {
        tokio::select! {
            biased;
            _ = priority.requested_async() => ReadPhase::Deferred,
            result = tokio::time::timeout(timeout, read) => match result { Ok(value) => ReadPhase::Finished(value), Err(_) => ReadPhase::TimedOut },
        }
    } else { match tokio::time::timeout(timeout, read).await { Ok(value) => ReadPhase::Finished(value), Err(_) => ReadPhase::TimedOut } }
}

pub(super) async fn handle<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut BufReader<R>, writer: &mut W, bytes: &[u8],
    service: &Service, coordinator: &tokio::sync::Mutex<()>,
) -> Result<()> {
    let command = match service.registration.parse(bytes) {
        Ok(command) => command,
        Err(error) => {
            let id = serde_json::from_slice::<Value>(bytes).ok()
                .and_then(|value| value["command"]["request_id"].as_str().map(str::to_owned)).unwrap_or_default();
            let reply = service.registration.reply(&id, &Err(error));
            writer.write_all(format!("{reply}\n").as_bytes()).await?;
            return Ok(());
        }
    };
    let _waiting = service.priority.request();
    let operation = async {
        let _guard = coordinator.lock().await;
        apply_when_available(service.registration.home(), &command).await
    };
    tokio::pin!(operation);
    let mut closed = [0u8; 1];
    let result = tokio::select! {
        biased;
        _ = reader.read(&mut closed) => return Ok(()),
        result = &mut operation => result,
    };
    let reply = service.registration.reply(&command.request_id, &result);
    writer.write_all(format!("{reply}\n").as_bytes()).await?;
    writer.shutdown().await?;
    Ok(())
}

async fn apply_when_available(home: &Path, command: &crate::profile_admin::Command) -> Result<Value> {
    loop {
        match Store::open(home.to_path_buf()) {
            Ok(mut store) => return crate::profile_admin::apply(&mut store, command),
            Err(error) if error.to_string().contains("another rdrive process is using this state directory") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configuration_waiters_cancel_reads_and_release_priority_when_cancelled() {
        let priority = Arc::new(Priority::default());
        assert!(matches!(read_phase(None, Duration::from_secs(1), async { 7 }).await, ReadPhase::Finished(7)));
        assert!(matches!(read_phase(None, Duration::ZERO, std::future::pending::<()>()).await, ReadPhase::TimedOut));
        let waiting = priority.request();
        assert!(matches!(read_phase(Some(&priority), Duration::from_secs(1), async { panic!("queued admin must run before a new read") }).await, ReadPhase::Deferred));
        drop(waiting);
        assert!(!priority.requested());
        assert!(matches!(read_phase(Some(&priority), Duration::from_secs(1), async { 9 }).await, ReadPhase::Finished(9)));
        assert!(matches!(read_phase(Some(&priority), Duration::ZERO, std::future::pending::<()>()).await, ReadPhase::TimedOut));
        let (started, ready) = tokio::sync::oneshot::channel();
        let active = priority.clone();
        let task = tokio::spawn(async move { read_phase(Some(&active), Duration::from_secs(5), async {
            started.send(()).unwrap();
            std::future::pending::<()>().await
        }).await });
        ready.await.unwrap();
        let waiting = priority.request();
        assert!(matches!(task.await.unwrap(), ReadPhase::Deferred));
        drop(waiting);
        assert!(!priority.requested());
    }

    #[tokio::test]
    async fn an_external_writer_is_waited_for_and_invalid_state_is_not_recovered_as_success() {
        let root=tempfile::tempdir().unwrap();let home=root.path().join("state");
        let store=Store::open(home.clone()).unwrap();store.save_config().unwrap();
        let command=crate::profile_admin::Command::new(crate::profile_admin::Change::Add {
            name:"second".into(),profile:crate::config::Profile {
                protocol:crate::config::Protocol::Smb,url:"smb://example.invalid/share".into(),
                username:None,password_env:None,keychain_internet:false,allow_http:false,
                timeout_secs:5,smb_encryption:true,nextcloud_upload_url:None,
            } },None).unwrap();
        let applying=apply_when_available(&home,&command);tokio::pin!(applying);
        assert!(tokio::time::timeout(Duration::from_millis(45),&mut applying).await.is_err());
        assert!(!store.config.profiles.contains_key("second"));
        drop(store);
        assert_eq!(applying.await.unwrap()["saved"],true);
        let config=fs::read(home.join("config.json")).unwrap();
        fs::write(home.join("config.json"),b"invalid config").unwrap();
        assert!(apply_when_available(&home,&command).await.unwrap_err().to_string().contains("invalid JSON"));
        fs::write(home.join("config.json"),config).unwrap();
        assert!(crate::client_read::config(&home).unwrap().profiles.contains_key("second"));
    }
}
