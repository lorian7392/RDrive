//! Aggregate payload throttling and progress shared across independent clients.
//! Counters describe bytes consumed by the transport, not remote commit receipts.
use anyhow::{anyhow, bail, Result};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{IsTerminal, Write},
    sync::{
        atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressOutput {
    Off,
    Auto,
    Json,
}

#[derive(Clone)]
pub struct TransferContext {
    shared: Arc<Shared>,
    scope: Option<String>,
}
struct Shared {
    limiter: Option<Limiter>,
    speed: Mutex<RateMeter>,
    next_id: AtomicU64,
    transferred: AtomicU64,
    declared: AtomicU64,
    unknown: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
    output: ProgressOutput,
    tty: bool,
    render: Mutex<Instant>,
}
struct Limiter {
    rate: u64,
    capacity: f64,
    bucket: tokio::sync::Mutex<Bucket>,
}
struct Bucket {
    tokens: f64,
    updated: Instant,
}
struct Job {
    id: u64,
    label: Option<String>,
    operation: String,
    path: String,
    total: Option<u64>,
    resumed: u64,
    transferred: AtomicU64,
    speed: Mutex<RateMeter>,
    state: AtomicU8,
    error: Mutex<Option<String>>,
}
struct Handle {
    shared: Arc<Shared>,
    job: Arc<Job>,
}
struct RateMeter {
    started: Instant,
    samples: VecDeque<(Instant, u64)>,
}
impl RateMeter {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            samples: VecDeque::new(),
        }
    }
    fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        self.expire(now);
        if self.samples.is_empty() {
            self.started = now;
        }
        if let Some((at, value)) = self.samples.back_mut() {
            if now.duration_since(*at) < Duration::from_millis(100) {
                *value = value.saturating_add(bytes);
                return;
            }
        }
        self.samples.push_back((now, bytes));
    }
    fn expire(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(2))
        {
            self.samples.pop_front();
        }
    }
    fn value(&mut self) -> f64 {
        let now = Instant::now();
        self.expire(now);
        let bytes = self
            .samples
            .iter()
            .map(|(_, bytes)| u128::from(*bytes))
            .sum::<u128>();
        bytes as f64
            / now
                .duration_since(self.started)
                .as_secs_f64()
                .clamp(0.05, 2.0)
    }
}
#[derive(Clone)]
pub struct Transfer {
    handle: Arc<Handle>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub id: u64,
    pub label: Option<String>,
    pub operation: String,
    pub path: String,
    pub total: Option<u64>,
    pub resumed: u64,
    pub transferred: u64,
    pub bytes: u64,
    pub bytes_per_second: f64,
    pub eta_seconds: Option<f64>,
    pub state: &'static str,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AggregateProgress {
    pub transferred: u64,
    pub total: Option<u64>,
    pub bytes_per_second: f64,
    pub eta_seconds: Option<f64>,
    pub active_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub limit_bytes_per_second: Option<u64>,
    pub jobs: Vec<JobProgress>,
}

static GLOBAL: OnceLock<TransferContext> = OnceLock::new();
pub fn set_global_context(context: TransferContext) -> Result<()> {
    GLOBAL
        .set(context)
        .map_err(|_| anyhow!("transfer context was already initialized"))
}
pub fn global_context() -> TransferContext {
    GLOBAL.get_or_init(TransferContext::default).clone()
}
pub fn install_global(context: TransferContext) -> Result<()> {
    set_global_context(context)
}
pub fn global() -> TransferContext {
    global_context()
}

impl Default for TransferContext {
    fn default() -> Self {
        Self::new(None, ProgressOutput::Off).expect("unlimited quiet transfer context")
    }
}
impl TransferContext {
    pub fn new(limit_bytes_per_second: Option<u64>, output: ProgressOutput) -> Result<Self> {
        if limit_bytes_per_second == Some(0) {
            bail!("bandwidth limit must be greater than zero");
        }
        let now = Instant::now();
        let limiter = limit_bytes_per_second.map(|rate| {
            // At most 100 ms of initial burst, capped at 64 KiB. A large request
            // waits through multiple refills rather than bypassing the limit.
            let capacity = (rate as f64 / 10.0).clamp(1.0, 65536.0);
            Limiter {
                rate,
                capacity,
                bucket: tokio::sync::Mutex::new(Bucket {
                    tokens: capacity,
                    updated: now,
                }),
            }
        });
        Ok(Self {
            shared: Arc::new(Shared {
                limiter,
                speed: Mutex::new(RateMeter::new()),
                next_id: AtomicU64::new(1),
                transferred: AtomicU64::new(0),
                declared: AtomicU64::new(0),
                unknown: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                failed: AtomicUsize::new(0),
                jobs: Mutex::new(BTreeMap::new()),
                output,
                tty: std::io::stderr().is_terminal(),
                render: Mutex::new(now.checked_sub(Duration::from_secs(1)).unwrap_or(now)),
            }),
            scope: None,
        })
    }

    pub fn scoped(&self, label: impl Into<String>) -> Self {
        Self {
            shared: self.shared.clone(),
            scope: Some(label.into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn poison_registry_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.shared.jobs.lock().unwrap();
            panic!("synthetic poisoned registry");
        }));
    }

    pub fn start(
        &self,
        operation: &str,
        path: &str,
        total: Option<u64>,
        resumed: u64,
    ) -> Result<Transfer> {
        if total.is_some_and(|value| value < resumed) {
            bail!("resumed byte count exceeds transfer size");
        }
        let id = self
            .shared
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow!("too many transfer jobs"))?;
        if let Some(total) = total {
            checked_add(&self.shared.declared, total - resumed)?;
        } else {
            self.shared.unknown.fetch_add(1, Ordering::Relaxed);
        }
        let job = Arc::new(Job {
            id,
            label: self.scope.clone(),
            operation: operation.into(),
            path: path.into(),
            total,
            resumed,
            transferred: AtomicU64::new(0),
            speed: Mutex::new(RateMeter::new()),
            state: AtomicU8::new(0),
            error: Mutex::new(None),
        });
        {
            let mut jobs = self
                .shared
                .jobs
                .lock()
                .map_err(|_| anyhow!("transfer registry is unavailable"))?;
            if jobs.len() >= 2048 {
                if let Some(id) = jobs
                    .iter()
                    .find_map(|(id, job)| (job.state.load(Ordering::Relaxed) != 0).then_some(*id))
                {
                    jobs.remove(&id);
                }
            }
            jobs.insert(id, job.clone());
        }
        let transfer = Transfer {
            handle: Arc::new(Handle {
                shared: self.shared.clone(),
                job,
            }),
        };
        self.shared
            .emit("started", Some(&transfer.handle.job), true);
        Ok(transfer)
    }

    pub fn snapshot(&self) -> AggregateProgress {
        self.shared.snapshot()
    }

    pub(crate) fn scope_label(&self) -> Option<&str> { self.scope.as_deref() }

    /// Select before cloning paths and computing rates. Finder polls only its
    /// own payload operation rather than copying the process-wide history.
    pub(crate) fn scoped_jobs(&self) -> Vec<JobProgress> {
        self.shared.jobs.lock().map(|jobs| jobs.values()
            .filter(|job| job.label == self.scope).map(|job| job.snapshot()).collect()).unwrap_or_default()
    }

    /// Keep protocol blocks within the limiter's burst allowance. This avoids
    /// a low configured rate accumulating a large credit before one large I/O.
    pub fn chunk_size(&self, maximum: usize) -> usize {
        self.shared
            .limiter
            .as_ref()
            .map(|limiter| {
                (limiter.capacity.floor() as usize)
                    .max(1)
                    .min(maximum.max(1))
            })
            .unwrap_or(maximum.max(1))
    }

    /// Optional periodic reporter. Drops automatically with the context; callers
    /// can also abort the returned task during orderly shutdown.
    pub fn reporter(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.shared);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                if shared.output != ProgressOutput::Off && shared.snapshot().active_jobs != 0 {
                    shared.emit("progress", None, false);
                }
            }
        })
    }
}

impl Transfer {
    pub async fn acquire(&self, bytes: usize) -> Result<()> {
        let Some(limiter) = &self.handle.shared.limiter else {
            return Ok(());
        };
        let mut remaining = bytes as u64;
        let mut bucket = limiter.bucket.lock().await;
        while remaining != 0 {
            let now = Instant::now();
            bucket.tokens = (bucket.tokens
                + now.duration_since(bucket.updated).as_secs_f64() * limiter.rate as f64)
                .min(limiter.capacity);
            bucket.updated = now;
            let available = (bucket.tokens.floor() as u64).min(remaining);
            bucket.tokens -= available as f64;
            remaining -= available;
            if remaining == 0 {
                break;
            }
            let needed = (remaining as f64).min(limiter.capacity) - bucket.tokens;
            let delay = Duration::from_secs_f64((needed / limiter.rate as f64).max(0.000_001));
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    pub fn record(&self, bytes: u64) -> Result<()> {
        if self.handle.job.state.load(Ordering::Acquire) != 0 {
            bail!("cannot record bytes on a finished transfer");
        }
        self.handle
            .job
            .transferred
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value
                    .checked_add(bytes)
                    .filter(|next| self.handle.job.resumed.checked_add(*next).is_some())
            })
            .map_err(|_| anyhow!("transfer byte counter overflow"))?;
        checked_add(&self.handle.shared.transferred, bytes)?;
        if let Ok(mut speed) = self.handle.job.speed.lock() {
            speed.record(bytes);
        }
        if let Ok(mut speed) = self.handle.shared.speed.lock() {
            speed.record(bytes);
        }
        self.handle
            .shared
            .emit("progress", Some(&self.handle.job), false);
        Ok(())
    }
    pub fn finish(&self) {
        self.handle.finish(1, None);
    }
    pub fn fail(&self, message: &str) {
        self.handle.finish(2, Some(message));
    }
    pub fn snapshot(&self) -> JobProgress {
        self.handle.job.snapshot()
    }
}
impl Handle {
    fn finish(&self, state: u8, error: Option<&str>) {
        if self
            .job
            .state
            .compare_exchange(0, state, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if let Some(message) = error {
            if let Ok(mut value) = self.job.error.lock() {
                *value = Some(message.into());
            }
        }
        let transferred = self.job.transferred.load(Ordering::Relaxed);
        if let Some(total) = self.job.total {
            let expected = total - self.job.resumed;
            if transferred < expected {
                self.shared
                    .declared
                    .fetch_sub(expected - transferred, Ordering::Relaxed);
            } else if transferred > expected {
                let _ = checked_add(&self.shared.declared, transferred - expected);
            }
        } else {
            let _ = checked_add(&self.shared.declared, transferred);
            self.shared.unknown.fetch_sub(1, Ordering::Relaxed);
        }
        if state == 1 {
            self.shared.completed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.shared.failed.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.emit(
            if state == 1 { "finished" } else { "failed" },
            Some(&self.job),
            true,
        );
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.finish(2, Some("interrupted or cancelled"));
    }
}
impl Job {
    fn snapshot(&self) -> JobProgress {
        let transferred = self.transferred.load(Ordering::Relaxed);
        let bytes = self.resumed.saturating_add(transferred);
        let speed = self
            .speed
            .lock()
            .map(|mut meter| meter.value())
            .unwrap_or(0.0);
        JobProgress {
            id: self.id,
            label: self.label.clone(),
            operation: self.operation.clone(),
            path: self.path.clone(),
            total: self.total,
            resumed: self.resumed,
            transferred,
            bytes,
            bytes_per_second: speed,
            eta_seconds: self.total.and_then(|total| {
                (speed > 0.0).then(|| total.saturating_sub(bytes) as f64 / speed)
            }),
            state: match self.state.load(Ordering::Acquire) {
                0 => "running",
                1 => "complete",
                _ => "failed",
            },
            error: self.error.lock().ok().and_then(|value| value.clone()),
        }
    }
}
impl Shared {
    fn snapshot(&self) -> AggregateProgress {
        let transferred = self.transferred.load(Ordering::Relaxed);
        let speed = self
            .speed
            .lock()
            .map(|mut meter| meter.value())
            .unwrap_or(0.0);
        let jobs = self
            .jobs
            .lock()
            .map(|jobs| jobs.values().map(|job| job.snapshot()).collect::<Vec<_>>())
            .unwrap_or_default();
        let total = (self.unknown.load(Ordering::Relaxed) == 0)
            .then(|| self.declared.load(Ordering::Relaxed));
        AggregateProgress {
            transferred,
            total,
            bytes_per_second: speed,
            eta_seconds: total.and_then(|total| {
                (speed > 0.0).then(|| total.saturating_sub(transferred) as f64 / speed)
            }),
            active_jobs: jobs.iter().filter(|job| job.state == "running").count(),
            completed_jobs: self.completed.load(Ordering::Relaxed),
            failed_jobs: self.failed.load(Ordering::Relaxed),
            limit_bytes_per_second: self.limiter.as_ref().map(|limiter| limiter.rate),
            jobs,
        }
    }

    fn emit(&self, event: &str, job: Option<&Arc<Job>>, force: bool) {
        if self.output != ProgressOutput::Off {
            self.emit_to(event, job, force, &mut std::io::stderr().lock());
        }
    }

    fn emit_to(&self, event: &str, job: Option<&Arc<Job>>, force: bool, stderr: &mut dyn Write) {
        if self.output == ProgressOutput::Off {
            return;
        }
        let Ok(mut last) = self.render.lock() else {
            return;
        };
        if !force && last.elapsed() < Duration::from_millis(250) {
            return;
        }
        *last = Instant::now();
        let mut aggregate = self.snapshot();
        let job = job.map(|job| job.snapshot());
        if self.output == ProgressOutput::Json || !self.tty {
            aggregate.jobs.retain(|candidate| {
                candidate.state == "running"
                    || job
                        .as_ref()
                        .is_some_and(|current| current.id == candidate.id)
            });
            let value = serde_json::json!({"type":"transfer_progress","event":event,"job":job,"aggregate":aggregate});
            if let Ok(value) = serde_json::to_string(&value) {
                let _ = writeln!(stderr, "{value}");
            }
        } else {
            let active = job.or_else(|| {
                aggregate
                    .jobs
                    .iter()
                    .rev()
                    .find(|job| job.state == "running")
                    .cloned()
            });
            if let Some(job) = active {
                let percentage = job
                    .total
                    .filter(|total| *total != 0)
                    .map(|total| (job.bytes as f64 / total as f64 * 100.0).clamp(0.0, 100.0));
                let fraction = percentage
                    .map(|value| format!("{value:5.1}%"))
                    .unwrap_or_else(|| "  --%".into());
                let filled = percentage
                    .map(|value| (value / 100.0 * 16.0).floor() as usize)
                    .unwrap_or(0)
                    .min(16);
                let bar = format!("[{}{}]", "=".repeat(filled), " ".repeat(16 - filled));
                let eta = job
                    .eta_seconds
                    .map(|value| format!("{value:.0}s"))
                    .unwrap_or_else(|| "--".into());
                let label = job
                    .label
                    .as_deref()
                    .unwrap_or(&job.path)
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            '?'
                        } else {
                            character
                        }
                    })
                    .take(60)
                    .collect::<String>();
                let _ = write!(
                    stderr,
                    "\r\x1b[2K{bar} {fraction} {} {} {:.1} KiB/s ETA {} | {} active, total {:.1} KiB/s",
                    job.operation,
                    label,
                    job.bytes_per_second / 1024.0,
                    eta,
                    aggregate.active_jobs,
                    aggregate.bytes_per_second / 1024.0
                );
                if force && event != "started" {
                    let _ = writeln!(stderr);
                }
                let _ = stderr.flush();
            }
        }
    }
}
fn checked_add(counter: &AtomicU64, bytes: u64) -> Result<()> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(bytes)
        })
        .map(|_| ())
        .map_err(|_| anyhow!("transfer byte counter overflow"))
}

/// Apply backpressure to a streamed body and record each payload chunk. The
/// request owner keeps a cloned handle and finishes it only after server success.
pub fn meter_stream<S, B, E>(stream: S, transfer: Transfer) -> impl Stream<Item = Result<B>> + Send
where
    S: Stream<Item = std::result::Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    futures_util::stream::try_unfold(
        (Box::pin(stream), transfer),
        |(mut stream, transfer)| async move {
            match stream.next().await {
                None => Ok(None),
                Some(Err(_)) => {
                    transfer.fail("transport stream interrupted");
                    Err(anyhow!("transport stream interrupted"))
                }
                Some(Ok(bytes)) => {
                    let length = bytes.as_ref().len();
                    transfer.acquire(length).await?;
                    transfer.record(length as u64)?;
                    Ok(Some((bytes, (stream, transfer))))
                }
            }
        },
    )
}

#[cfg(test)]
pub(crate) fn exhaust_context_ids(context: &TransferContext) {
    context.shared.next_id.store(u64::MAX, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_and_json_progress_are_observable_and_control_characters_are_sanitized() {
        let mut context = TransferContext::new(None, ProgressOutput::Auto).unwrap();
        Arc::get_mut(&mut context.shared).unwrap().tty = true;
        let job = context.scoped("a\nb\x1b").start("download", "path", Some(20), 0).unwrap();
        job.record(10).unwrap();
        let mut output = Vec::new();
        context.shared.emit_to("progress", Some(&job.handle.job), true, &mut output);
        let value = String::from_utf8(output).unwrap();
        assert!(value.contains("50.0%") && value.contains("[========        ]"), "{value}");
        assert!(value.contains("a?b?") && value.contains("ETA"));
        let unknown = context.start("upload", "unknown", None, 0).unwrap();
        let mut output = Vec::new();
        context.shared.emit_to("progress", None, true, &mut output);
        assert!(String::from_utf8(output).unwrap().contains("--%"));
        let empty = context.start("upload", "zero", Some(0), 0).unwrap();
        empty.finish();
        unknown.fail("synthetic transport error");
        job.finish();
        let mut output = Vec::new();
        context.shared.emit_to("progress", None, true, &mut output);
        assert!(output.is_empty(), "completed jobs must not occupy the progress line");
        let context = TransferContext::new(None, ProgressOutput::Json).unwrap();
        let first = context.start("get", "finished", Some(1), 0).unwrap();
        first.record(1).unwrap(); first.finish();
        let second = context.start("get", "current", None, 0).unwrap();
        let mut output = Vec::new();
        context.shared.emit_to("progress", Some(&second.handle.job), true, &mut output);
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["event"], "progress");
        assert_eq!(value["aggregate"]["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(value["aggregate"]["jobs"][0]["path"], "current");
    }

    #[test]
    fn registry_bounds_completed_jobs_and_rejects_id_and_declared_size_overflow() {
        let context = TransferContext::default();
        for _ in 0..2050 { context.start("get", "x", Some(0), 0).unwrap().finish(); }
        assert_eq!(context.shared.jobs.lock().unwrap().len(), 2048);
        context.shared.next_id.store(u64::MAX, Ordering::Relaxed);
        assert!(context.start("get", "x", None, 0).err().unwrap().to_string().contains("too many"));
        context.shared.next_id.store(1, Ordering::Relaxed);
        context.shared.declared.store(u64::MAX, Ordering::Relaxed);
        assert!(context.start("get", "x", Some(1), 0).err().unwrap().to_string().contains("overflow"));
        assert!(context.start("get", "x", Some(1), 2).is_err());
        assert!(TransferContext::new(Some(0), ProgressOutput::Off).is_err());
    }

    #[test]
    fn finished_transfer_and_poisoned_registry_fail_without_panic() {
        let context = TransferContext::default();
        let transfer = context.start("get", "x", Some(1), 0).unwrap();
        transfer.record(2).unwrap(); transfer.finish(); transfer.finish();
        assert_eq!(context.snapshot().total, Some(2));
        assert!(transfer.record(1).is_err());
        let job = context.start("get", "overflow", None, 0).unwrap();
        job.handle.job.transferred.store(u64::MAX, Ordering::Relaxed);
        assert!(job.record(1).is_err());
        context.poison_registry_for_test();
        assert!(context.start("get", "x", None, 0).err().unwrap().to_string().contains("registry"));
        assert!(context.snapshot().jobs.is_empty());
    }

    #[test]
    fn disabled_or_poisoned_progress_output_never_writes() {
        let context = TransferContext::default(); let mut output = Vec::new();
        context.shared.emit_to("progress", None, true, &mut output);
        assert!(output.is_empty());
        let context = TransferContext::new(None, ProgressOutput::Json).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = context.shared.render.lock().unwrap(); panic!("synthetic progress lock failure");
        }));
        context.shared.emit_to("progress", None, true, &mut output);
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn reporter_stops_when_context_and_handles_are_released() {
        let context = TransferContext::new(None, ProgressOutput::Json).unwrap();
        let transfer = context.start("get", "reporter", None, 0).unwrap();
        let reporter = context.reporter();
        tokio::time::sleep(Duration::from_millis(280)).await;
        transfer.finish(); drop(transfer); drop(context);
        tokio::time::timeout(Duration::from_secs(1), reporter).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn metered_stream_counts_payload_and_reports_transport_failure_without_source_text() {
        let context = TransferContext::default();
        let transfer = context.start("stream", "ok", Some(3), 0).unwrap();
        let input = futures_util::stream::iter([Ok::<_, &str>(b"abc".to_vec())]);
        let chunks = meter_stream(input, transfer.clone()).collect::<Vec<_>>().await;
        assert_eq!(chunks.len(), 1); assert_eq!(chunks[0].as_ref().unwrap(), b"abc");
        assert_eq!(transfer.snapshot().transferred, 3); transfer.finish();
        let failed = context.start("stream", "failed", None, 0).unwrap();
        let input = futures_util::stream::iter([Err::<Vec<u8>, _>("private error text")]);
        let chunks = meter_stream(input, failed.clone()).collect::<Vec<_>>().await;
        assert_eq!(chunks[0].as_ref().unwrap_err().to_string(), "transport stream interrupted");
        assert_eq!(failed.snapshot().state, "failed");
        assert_eq!(failed.snapshot().transferred, 0);
    }

    #[test]
    fn global_installation_is_process_local_and_rejects_replacement() {
        const MARKER: &str = "RDRIVE_TRANSFER_GLOBAL_TEST_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "transfer::tests::global_installation_is_process_local_and_rejects_replacement"])
                .env(MARKER, "1").output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        }
        install_global(TransferContext::default()).unwrap();
        assert!(install_global(TransferContext::default()).is_err());
        assert_eq!(global().snapshot().active_jobs, 0);
    }
    #[tokio::test]
    async fn shared_limiter_caps_parallel_jobs_instead_of_each_client_separately() {
        let context = TransferContext::new(Some(20_000), ProgressOutput::Off).unwrap();
        let first = context
            .scoped("a")
            .start("upload", "first", Some(10_000), 0)
            .unwrap();
        let second = context
            .scoped("b")
            .start("upload", "second", Some(10_000), 0)
            .unwrap();
        let begin = Instant::now();
        let (a, b) = tokio::join!(first.acquire(10_000), second.acquire(10_000));
        a.unwrap();
        b.unwrap();
        first.record(10_000).unwrap();
        second.record(10_000).unwrap();
        first.finish();
        second.finish();
        assert!(
            begin.elapsed() >= Duration::from_millis(850),
            "aggregate bandwidth exceeded configured rate"
        );
        let aggregate = context.snapshot();
        assert_eq!(aggregate.transferred, 20_000);
        assert_eq!(aggregate.completed_jobs, 2);
        assert_eq!(aggregate.active_jobs, 0);
    }
    #[test]
    fn resumed_bytes_are_not_falsely_counted_as_network_traffic() {
        let context = TransferContext::default();
        let transfer = context.start("download", "file", Some(1000), 600).unwrap();
        transfer.record(400).unwrap();
        transfer.finish();
        assert_eq!(transfer.snapshot().bytes, 1000);
        assert_eq!(context.snapshot().transferred, 400);
        assert_eq!(context.snapshot().total, Some(400));
    }
    #[test]
    fn cancelling_last_handle_records_failure_and_prevents_fake_completion() {
        let context = TransferContext::default();
        let transfer = context.start("upload", "file", Some(10), 0).unwrap();
        transfer.record(3).unwrap();
        let clone = transfer.clone();
        drop(transfer);
        assert_eq!(context.snapshot().active_jobs, 1);
        drop(clone);
        assert_eq!(context.snapshot().failed_jobs, 1);
        assert_eq!(context.snapshot().completed_jobs, 0);
    }
}
