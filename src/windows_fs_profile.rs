//! Profiling-only filesystem executors for the 0.0.2-P3 investigation.
//!
//! This module is excluded from normal builds. It deliberately mirrors the
//! production four-operation sequence without changing the static handler.

use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle, ThreadId},
    time::{Duration, Instant},
};

use tokio::{sync::oneshot, task::JoinSet};

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Noop,
    Metadata,
    Canonicalize,
    MetadataCanonicalize,
    Read,
    FullCurrentSequence,
}

impl Operation {
    pub const ALL: [Self; 5] = [
        Self::Metadata,
        Self::Canonicalize,
        Self::MetadataCanonicalize,
        Self::Read,
        Self::FullCurrentSequence,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Noop => "noop",
            Self::Metadata => "metadata",
            Self::Canonicalize => "canonicalize",
            Self::MetadataCanonicalize => "metadata_canonicalize",
            Self::Read => "read",
            Self::FullCurrentSequence => "full_current_sequence",
        }
    }
}

#[derive(Clone)]
pub enum Executor {
    TokioSpawnBlocking,
    Dedicated(Arc<DedicatedPool>),
}

impl Executor {
    pub fn dedicated(worker_count: usize) -> io::Result<Self> {
        Ok(Self::Dedicated(Arc::new(DedicatedPool::new(worker_count)?)))
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::TokioSpawnBlocking => "spawn_blocking",
            Self::Dedicated(_) => "dedicated",
        }
    }

    pub fn configured_workers(&self) -> Option<usize> {
        match self {
            Self::TokioSpawnBlocking => None,
            Self::Dedicated(pool) => Some(pool.worker_count()),
        }
    }

    async fn execute(&self, action: FsAction) -> io::Result<TimedOutput> {
        match self {
            Self::TokioSpawnBlocking => timed_spawn_blocking(action).await,
            Self::Dedicated(pool) => pool.execute(action).await,
        }
    }
}

pub struct DedicatedPool {
    sender: mpsc::Sender<Message>,
    worker_count: usize,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl DedicatedPool {
    fn new(worker_count: usize) -> io::Result<Self> {
        if worker_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dedicated worker count must be greater than zero",
            ));
        }

        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let worker = thread::Builder::new()
                .name(format!("hw-fs-profile-{index}"))
                .spawn(move || dedicated_worker(receiver))?;
            workers.push(worker);
        }

        Ok(Self {
            sender,
            worker_count,
            workers: Mutex::new(workers),
        })
    }

    fn worker_count(&self) -> usize {
        self.worker_count
    }

    async fn execute(&self, action: FsAction) -> io::Result<TimedOutput> {
        let submitted = Instant::now();
        let (completion_sender, completion_receiver) = oneshot::channel();
        self.sender
            .send(Message::Run(Job {
                action,
                submitted,
                completion: completion_sender,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "dedicated worker stopped"))?;

        let mut result = completion_receiver.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "dedicated worker dropped its result",
            )
        })?;
        result.timing.completion = Instant::now().duration_since(result.finished);
        result.timing.total = Instant::now().duration_since(submitted);
        result.result.map(|output| TimedOutput {
            output,
            timing: result.timing,
        })
    }
}

impl Drop for DedicatedPool {
    fn drop(&mut self) {
        for _ in 0..self.worker_count {
            let _ = self.sender.send(Message::Shutdown);
        }
        if let Ok(workers) = self.workers.get_mut() {
            for worker in workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
}

enum Message {
    Run(Job),
    Shutdown,
}

struct Job {
    action: FsAction,
    submitted: Instant,
    completion: oneshot::Sender<WorkerResult>,
}

struct WorkerResult {
    result: io::Result<FsOutput>,
    timing: Timing,
    finished: Instant,
}

fn dedicated_worker(receiver: Arc<Mutex<mpsc::Receiver<Message>>>) {
    loop {
        let message = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(message) = message else {
            return;
        };
        let Message::Run(job) = message else {
            break;
        };
        let started = Instant::now();
        let result = perform(job.action);
        let finished = Instant::now();
        let timing = Timing {
            queue_wait: started.duration_since(job.submitted),
            execution: finished.duration_since(started),
            completion: Duration::ZERO,
            total: Duration::ZERO,
            thread_id: thread::current().id(),
        };
        let _ = job.completion.send(WorkerResult {
            result,
            timing,
            finished,
        });
    }
}

#[derive(Debug)]
enum FsAction {
    Noop,
    Metadata(PathBuf),
    Canonicalize(PathBuf),
    Read(PathBuf),
}

enum FsOutput {
    Unit,
    Metadata(std::fs::Metadata),
    Path(PathBuf),
    Bytes(Vec<u8>),
}

fn perform(action: FsAction) -> io::Result<FsOutput> {
    match action {
        FsAction::Noop => Ok(FsOutput::Unit),
        FsAction::Metadata(path) => std::fs::metadata(path).map(FsOutput::Metadata),
        FsAction::Canonicalize(path) => std::fs::canonicalize(path).map(FsOutput::Path),
        FsAction::Read(path) => std::fs::read(path).map(FsOutput::Bytes),
    }
}

async fn timed_spawn_blocking(action: FsAction) -> io::Result<TimedOutput> {
    let submitted = Instant::now();
    let (result, started, finished, thread_id) = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result = perform(action);
        let finished = Instant::now();
        (result, started, finished, thread::current().id())
    })
    .await
    .map_err(io::Error::other)?;
    let completed = Instant::now();
    result.map(|output| TimedOutput {
        output,
        timing: Timing {
            queue_wait: started.duration_since(submitted),
            execution: finished.duration_since(started),
            completion: completed.duration_since(finished),
            total: completed.duration_since(submitted),
            thread_id,
        },
    })
}

struct TimedOutput {
    output: FsOutput,
    timing: Timing,
}

#[derive(Clone, Copy)]
struct Timing {
    queue_wait: Duration,
    execution: Duration,
    completion: Duration,
    total: Duration,
    thread_id: ThreadId,
}

struct RequestTiming {
    queue_wait: Duration,
    execution: Duration,
    completion: Duration,
    total: Duration,
    thread_ids: Vec<ThreadId>,
}

impl RequestTiming {
    fn new() -> Self {
        Self {
            queue_wait: Duration::ZERO,
            execution: Duration::ZERO,
            completion: Duration::ZERO,
            total: Duration::ZERO,
            thread_ids: Vec::with_capacity(4),
        }
    }

    fn record(&mut self, timing: Timing) {
        self.queue_wait += timing.queue_wait;
        self.execution += timing.execution;
        self.completion += timing.completion;
        self.thread_ids.push(timing.thread_id);
    }
}

#[derive(Clone, Debug)]
pub struct ProfileSummary {
    pub request_count: u64,
    pub wall_seconds: f64,
    pub requests_per_second: f64,
    pub threads_seen: usize,
    pub queue_p50_us: f64,
    pub queue_p95_us: f64,
    pub queue_p99_us: f64,
    pub execution_p50_us: f64,
    pub execution_p95_us: f64,
    pub execution_p99_us: f64,
    pub completion_p50_us: f64,
    pub completion_p95_us: f64,
    pub completion_p99_us: f64,
    pub total_p50_us: f64,
    pub total_p95_us: f64,
    pub total_p99_us: f64,
}

pub async fn profile(
    executor: Executor,
    operation: Operation,
    canonical_root: Arc<PathBuf>,
    paths: Arc<Vec<PathBuf>>,
    concurrency: usize,
    request_count: u64,
) -> io::Result<ProfileSummary> {
    if concurrency == 0 || paths.is_empty() || request_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "concurrency, path count, and request count must be non-zero",
        ));
    }

    let next_request = Arc::new(AtomicU64::new(0));
    let mut tasks = JoinSet::new();
    let started = Instant::now();
    for _ in 0..concurrency {
        let executor = executor.clone();
        let canonical_root = Arc::clone(&canonical_root);
        let paths = Arc::clone(&paths);
        let next_request = Arc::clone(&next_request);
        tasks.spawn(async move {
            let capacity =
                usize::try_from(request_count / concurrency as u64 + 1).unwrap_or(usize::MAX);
            let mut samples = Vec::with_capacity(capacity);
            loop {
                let index = next_request.fetch_add(1, Ordering::Relaxed);
                if index >= request_count {
                    break;
                }
                let path = paths[index as usize % paths.len()].clone();
                samples.push(run_operation(&executor, operation, &canonical_root, path).await?);
            }
            Ok::<_, io::Error>(samples)
        });
    }

    let capacity = usize::try_from(request_count).unwrap_or(usize::MAX);
    let mut samples = Vec::with_capacity(capacity);
    while let Some(result) = tasks.join_next().await {
        samples.append(&mut result.map_err(io::Error::other)??);
    }
    let wall_seconds = started.elapsed().as_secs_f64();
    Ok(summarize(samples, request_count, wall_seconds))
}

async fn run_operation(
    executor: &Executor,
    operation: Operation,
    canonical_root: &Path,
    path: PathBuf,
) -> io::Result<RequestTiming> {
    let pipeline_started = Instant::now();
    let mut sample = RequestTiming::new();

    match operation {
        Operation::Noop => {
            let result = executor.execute(FsAction::Noop).await?;
            consume_unit(result.output)?;
            sample.record(result.timing);
        }
        Operation::Metadata => {
            let result = executor.execute(FsAction::Metadata(path)).await?;
            consume_metadata(result.output)?;
            sample.record(result.timing);
        }
        Operation::Canonicalize => {
            let result = executor.execute(FsAction::Canonicalize(path)).await?;
            consume_path(result.output)?;
            sample.record(result.timing);
        }
        Operation::MetadataCanonicalize => {
            let result = executor.execute(FsAction::Metadata(path.clone())).await?;
            consume_metadata(result.output)?;
            sample.record(result.timing);
            let result = executor.execute(FsAction::Canonicalize(path)).await?;
            consume_path(result.output)?;
            sample.record(result.timing);
        }
        Operation::Read => {
            let result = executor.execute(FsAction::Read(path)).await?;
            consume_bytes(result.output)?;
            sample.record(result.timing);
        }
        Operation::FullCurrentSequence => {
            let result = executor.execute(FsAction::Metadata(path.clone())).await?;
            consume_metadata(result.output)?;
            sample.record(result.timing);

            let result = executor.execute(FsAction::Canonicalize(path)).await?;
            let canonical = consume_path(result.output)?;
            sample.record(result.timing);
            if !canonical.starts_with(canonical_root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "canonical target escaped the profiling root",
                ));
            }

            let result = executor
                .execute(FsAction::Metadata(canonical.clone()))
                .await?;
            let metadata = consume_metadata(result.output)?;
            sample.record(result.timing);
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "profiling target is not a regular file",
                ));
            }

            let result = executor.execute(FsAction::Read(canonical)).await?;
            consume_bytes(result.output)?;
            sample.record(result.timing);
        }
    }

    sample.total = pipeline_started.elapsed();
    Ok(sample)
}

fn consume_unit(output: FsOutput) -> io::Result<()> {
    match output {
        FsOutput::Unit => Ok(()),
        _ => Err(output_mismatch()),
    }
}

fn consume_metadata(output: FsOutput) -> io::Result<std::fs::Metadata> {
    match output {
        FsOutput::Metadata(metadata) => Ok(metadata),
        _ => Err(output_mismatch()),
    }
}

fn consume_path(output: FsOutput) -> io::Result<PathBuf> {
    match output {
        FsOutput::Path(path) => Ok(path),
        _ => Err(output_mismatch()),
    }
}

fn consume_bytes(output: FsOutput) -> io::Result<Vec<u8>> {
    match output {
        FsOutput::Bytes(bytes) => Ok(bytes),
        _ => Err(output_mismatch()),
    }
}

fn output_mismatch() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "filesystem executor returned an unexpected result",
    )
}

fn summarize(samples: Vec<RequestTiming>, request_count: u64, wall_seconds: f64) -> ProfileSummary {
    let mut queue = Vec::with_capacity(samples.len());
    let mut execution = Vec::with_capacity(samples.len());
    let mut completion = Vec::with_capacity(samples.len());
    let mut total = Vec::with_capacity(samples.len());
    let mut threads = HashSet::new();
    for sample in samples {
        queue.push(to_microseconds(sample.queue_wait));
        execution.push(to_microseconds(sample.execution));
        completion.push(to_microseconds(sample.completion));
        total.push(to_microseconds(sample.total));
        threads.extend(sample.thread_ids);
    }

    ProfileSummary {
        request_count,
        wall_seconds,
        requests_per_second: request_count as f64 / wall_seconds,
        threads_seen: threads.len(),
        queue_p50_us: percentile(&mut queue, 0.50),
        queue_p95_us: percentile(&mut queue, 0.95),
        queue_p99_us: percentile(&mut queue, 0.99),
        execution_p50_us: percentile(&mut execution, 0.50),
        execution_p95_us: percentile(&mut execution, 0.95),
        execution_p99_us: percentile(&mut execution, 0.99),
        completion_p50_us: percentile(&mut completion, 0.50),
        completion_p95_us: percentile(&mut completion, 0.95),
        completion_p99_us: percentile(&mut completion, 0.99),
        total_p50_us: percentile(&mut total, 0.50),
        total_p95_us: percentile(&mut total, 0.95),
        total_p99_us: percentile(&mut total, 0.99),
    }
}

fn percentile(samples: &mut [f64], percentile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    let index = ((samples.len() - 1) as f64 * percentile).floor() as usize;
    samples[index]
}

fn to_microseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn dedicated_pool_rejects_zero_workers() {
        assert!(Executor::dedicated(0).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedicated_pool_runs_the_current_sequence() -> io::Result<()> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "hamster-wheel-p3-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&directory)?;
        let path = directory.join("index.html");
        std::fs::write(&path, b"profiling fixture")?;
        let canonical_root = Arc::new(std::fs::canonicalize(&directory)?);
        let paths = Arc::new(vec![path]);
        let executor = Executor::dedicated(2)?;

        let result = profile(
            executor.clone(),
            Operation::FullCurrentSequence,
            canonical_root,
            paths,
            4,
            100,
        )
        .await?;

        assert_eq!(result.request_count, 100);
        assert!((1..=2).contains(&result.threads_seen));
        drop(executor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
