use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::task::{JoinSet, spawn_blocking};

const STAGE_NAMES: [&str; 4] = [
    "metadata_candidate",
    "canonicalize_target",
    "metadata_canonical",
    "read_file",
];

#[derive(Clone, Copy, Debug)]
struct OperationTiming {
    queue_wait: Duration,
    execution: Duration,
    completion: Duration,
    total: Duration,
}

#[derive(Debug, Default)]
struct OperationSamples {
    queue_wait_us: Vec<f64>,
    execution_us: Vec<f64>,
    completion_us: Vec<f64>,
    total_us: Vec<f64>,
}

impl OperationSamples {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            queue_wait_us: Vec::with_capacity(capacity),
            execution_us: Vec::with_capacity(capacity),
            completion_us: Vec::with_capacity(capacity),
            total_us: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, timing: OperationTiming) {
        self.queue_wait_us.push(to_microseconds(timing.queue_wait));
        self.execution_us.push(to_microseconds(timing.execution));
        self.completion_us.push(to_microseconds(timing.completion));
        self.total_us.push(to_microseconds(timing.total));
    }

    fn append(&mut self, other: &mut Self) {
        self.queue_wait_us.append(&mut other.queue_wait_us);
        self.execution_us.append(&mut other.execution_us);
        self.completion_us.append(&mut other.completion_us);
        self.total_us.append(&mut other.total_us);
    }

    fn summarize(mut self, name: &'static str) -> OperationSummary {
        OperationSummary {
            name,
            queue_p50_us: percentile(&mut self.queue_wait_us, 0.50),
            queue_p95_us: percentile(&mut self.queue_wait_us, 0.95),
            queue_p99_us: percentile(&mut self.queue_wait_us, 0.99),
            execution_p50_us: percentile(&mut self.execution_us, 0.50),
            execution_p95_us: percentile(&mut self.execution_us, 0.95),
            execution_p99_us: percentile(&mut self.execution_us, 0.99),
            completion_p50_us: percentile(&mut self.completion_us, 0.50),
            completion_p95_us: percentile(&mut self.completion_us, 0.95),
            completion_p99_us: percentile(&mut self.completion_us, 0.99),
            total_p50_us: percentile(&mut self.total_us, 0.50),
            total_p95_us: percentile(&mut self.total_us, 0.95),
            total_p99_us: percentile(&mut self.total_us, 0.99),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OperationSummary {
    pub name: &'static str,
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

#[derive(Clone, Debug)]
pub struct BlockingPipelineSummary {
    pub stages: Vec<OperationSummary>,
    pub pipeline_p50_us: f64,
    pub pipeline_p95_us: f64,
    pub pipeline_p99_us: f64,
}

struct WorkerSamples {
    stages: [OperationSamples; 4],
    pipeline_us: Vec<f64>,
}

impl WorkerSamples {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            stages: std::array::from_fn(|_| OperationSamples::with_capacity(capacity)),
            pipeline_us: Vec::with_capacity(capacity),
        }
    }

    fn append(&mut self, other: &mut Self) {
        for (stage, other_stage) in self.stages.iter_mut().zip(other.stages.iter_mut()) {
            stage.append(other_stage);
        }
        self.pipeline_us.append(&mut other.pipeline_us);
    }
}

pub async fn profile_filesystem_pipeline(
    document_root: PathBuf,
    concurrency: usize,
    request_count: u64,
) -> io::Result<BlockingPipelineSummary> {
    let canonical_root = Arc::new(tokio::fs::canonicalize(document_root).await?);
    let next_request = Arc::new(AtomicU64::new(0));
    let mut workers = JoinSet::new();

    for _ in 0..concurrency {
        let canonical_root = Arc::clone(&canonical_root);
        let next_request = Arc::clone(&next_request);
        let worker_capacity =
            usize::try_from(request_count / concurrency as u64 + 1).unwrap_or(usize::MAX);
        workers.spawn(async move {
            profile_worker(canonical_root, next_request, request_count, worker_capacity).await
        });
    }

    let capacity = usize::try_from(request_count).unwrap_or(usize::MAX);
    let mut combined = WorkerSamples::with_capacity(capacity);
    while let Some(worker_result) = workers.join_next().await {
        let mut worker = worker_result.map_err(io::Error::other)??;
        combined.append(&mut worker);
    }

    let stages = combined
        .stages
        .into_iter()
        .zip(STAGE_NAMES)
        .map(|(samples, name)| samples.summarize(name))
        .collect();
    Ok(BlockingPipelineSummary {
        stages,
        pipeline_p50_us: percentile(&mut combined.pipeline_us, 0.50),
        pipeline_p95_us: percentile(&mut combined.pipeline_us, 0.95),
        pipeline_p99_us: percentile(&mut combined.pipeline_us, 0.99),
    })
}

pub async fn profile_noop_blocking(
    concurrency: usize,
    operation_count: u64,
) -> io::Result<OperationSummary> {
    let next_operation = Arc::new(AtomicU64::new(0));
    let mut workers = JoinSet::new();

    for _ in 0..concurrency {
        let next_operation = Arc::clone(&next_operation);
        workers.spawn(async move {
            let capacity =
                usize::try_from(operation_count / concurrency as u64 + 1).unwrap_or(usize::MAX);
            let mut samples = OperationSamples::with_capacity(capacity);
            loop {
                let index = next_operation.fetch_add(1, Ordering::Relaxed);
                if index >= operation_count {
                    break;
                }
                let (_, timing) = timed_blocking(|| Ok(())).await?;
                samples.push(timing);
            }
            Ok::<_, io::Error>(samples)
        });
    }

    let capacity = usize::try_from(operation_count).unwrap_or(usize::MAX);
    let mut combined = OperationSamples::with_capacity(capacity);
    while let Some(worker_result) = workers.join_next().await {
        let mut worker = worker_result.map_err(io::Error::other)??;
        combined.append(&mut worker);
    }
    Ok(combined.summarize("spawn_blocking_noop"))
}

async fn profile_worker(
    canonical_root: Arc<PathBuf>,
    next_request: Arc<AtomicU64>,
    request_count: u64,
    capacity: usize,
) -> io::Result<WorkerSamples> {
    let mut samples = WorkerSamples::with_capacity(capacity);

    loop {
        let index = next_request.fetch_add(1, Ordering::Relaxed);
        if index >= request_count {
            break;
        }

        let pipeline_started = Instant::now();
        let candidate = canonical_root.join("index.html");

        let metadata_path = candidate.clone();
        let (_, timing) = timed_blocking(move || std::fs::metadata(metadata_path)).await?;
        samples.stages[0].push(timing);

        let (canonical, timing) = timed_blocking(move || std::fs::canonicalize(candidate)).await?;
        samples.stages[1].push(timing);
        if !canonical.starts_with(canonical_root.as_ref()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "canonical target escaped the document root",
            ));
        }

        let canonical_for_metadata = canonical.clone();
        let (_, timing) = timed_blocking(move || std::fs::metadata(canonical_for_metadata)).await?;
        samples.stages[2].push(timing);

        let (_, timing) = timed_blocking(move || std::fs::read(canonical)).await?;
        samples.stages[3].push(timing);
        samples
            .pipeline_us
            .push(to_microseconds(pipeline_started.elapsed()));
    }

    Ok(samples)
}

async fn timed_blocking<T, F>(operation: F) -> io::Result<(T, OperationTiming)>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let submitted = Instant::now();
    let (result, started, finished) = spawn_blocking(move || {
        let started = Instant::now();
        let result = operation();
        let finished = Instant::now();
        (result, started, finished)
    })
    .await
    .map_err(io::Error::other)?;
    let completed = Instant::now();

    let timing = OperationTiming {
        queue_wait: started.duration_since(submitted),
        execution: finished.duration_since(started),
        completion: completed.duration_since(finished),
        total: completed.duration_since(submitted),
    };
    Ok((result?, timing))
}

fn percentile(samples: &mut [f64], percentile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    let index = ((samples.len() - 1) as f64 * percentile).floor() as usize;
    samples[index]
}

fn to_microseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}
