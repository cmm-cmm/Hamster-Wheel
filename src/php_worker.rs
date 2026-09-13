use std::{
    collections::VecDeque,
    fmt,
    future::pending,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{HeaderName, HeaderValue, SERVER},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use crate::{
    config::SERVER_NAME,
    handler::{HandlerResponse, simple_response},
    php_protocol::{
        MAX_HEADERS, ProtocolError, WorkerRequest, WorkerResponse, decode_ready, decode_response,
        decode_shutdown_ack, encode_request, encode_shutdown, read_frame, write_frame,
    },
};

pub const MAX_REQUEST_BODY: usize = 1024 * 1024;
pub const MAX_WORKERS: usize = 64;
pub const MAX_QUEUE_CAPACITY: usize = 128;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const STABILITY_RESET_PERIOD: Duration = Duration::from_secs(60);
const MAX_CONSECUTIVE_FAILURES: usize = 10;
const RECYCLE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct PhpWorkerConfig {
    pub executable: PathBuf,
    pub script: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PhpWorkerPoolConfig {
    pub worker: PhpWorkerConfig,
    pub workers: usize,
    pub queue_capacity: usize,
    pub max_requests: Option<u64>,
    pub max_worker_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerLifecycleState {
    Starting,
    Backoff,
    Bootstrapping,
    Ready,
    Busy,
    Recycling,
    Unavailable,
    Retired,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSpawnReason {
    Initial,
    Respawn,
    MaxRequests,
    MaxAge,
}

#[derive(Clone, Debug)]
pub struct WorkerSnapshot {
    pub slot: usize,
    pub generation: u64,
    pub pid: Option<u32>,
    pub state: WorkerLifecycleState,
    pub spawn_reason: WorkerSpawnReason,
    pub request_count: u64,
    pub spawned_at: Option<Instant>,
    pub consecutive_failures: usize,
    pub last_exit_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PoolSnapshot {
    pub target_workers: usize,
    pub ready_workers: usize,
    pub total_respawns: u64,
    pub workers: Vec<WorkerSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServingPhase {
    Serving,
    ShuttingDown,
}

struct SpawnGate {
    phase: StdMutex<ServingPhase>,
}

impl SpawnGate {
    fn new() -> Self {
        Self {
            phase: StdMutex::new(ServingPhase::Serving),
        }
    }

    fn begin_shutdown(&self) {
        *self.phase.lock().unwrap_or_else(|error| error.into_inner()) = ServingPhase::ShuttingDown;
    }

    fn is_serving(&self) -> bool {
        *self.phase.lock().unwrap_or_else(|error| error.into_inner()) == ServingPhase::Serving
    }
}

async fn wait_for_shutdown_phase(phase: &mut watch::Receiver<ServingPhase>) {
    loop {
        if *phase.borrow() == ServingPhase::ShuttingDown || phase.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Ready,
    Busy,
    Unavailable,
    Stopped,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerStart {
    pub pid: u32,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum PhpWorkerError {
    Spawn {
        executable: PathBuf,
        source: io::Error,
    },
    Startup(String),
    Unavailable(String),
    Request(String),
    Shutdown(String),
}

impl fmt::Display for PhpWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { executable, source } => {
                write!(
                    formatter,
                    "cannot spawn PHP {}: {source}",
                    executable.display()
                )
            }
            Self::Startup(message) => write!(formatter, "PHP worker startup failed: {message}"),
            Self::Unavailable(message) => write!(formatter, "PHP worker unavailable: {message}"),
            Self::Request(message) => write!(formatter, "PHP worker request failed: {message}"),
            Self::Shutdown(message) => write!(formatter, "PHP worker shutdown failed: {message}"),
        }
    }
}

impl std::error::Error for PhpWorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Startup(_) | Self::Unavailable(_) | Self::Request(_) | Self::Shutdown(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
struct WorkerTimeouts {
    startup: Duration,
    request: Duration,
    shutdown: Duration,
}

impl Default for WorkerTimeouts {
    fn default() -> Self {
        Self {
            startup: STARTUP_TIMEOUT,
            request: REQUEST_TIMEOUT,
            shutdown: SHUTDOWN_TIMEOUT,
        }
    }
}

pub struct PhpWorker {
    config: PhpWorkerConfig,
    timeouts: WorkerTimeouts,
    inner: WorkerSlot,
}

enum WorkerSlot {
    Starting,
    Running(Box<RunningWorker>),
    Unavailable(String),
    Stopped,
}

struct RunningWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_task: JoinHandle<()>,
    pid: u32,
    next_request_id: u64,
    state: WorkerState,
}

impl PhpWorker {
    pub fn new(config: PhpWorkerConfig) -> Self {
        Self::with_timeouts(config, WorkerTimeouts::default())
    }

    fn with_timeouts(config: PhpWorkerConfig, timeouts: WorkerTimeouts) -> Self {
        Self {
            config,
            timeouts,
            inner: WorkerSlot::Starting,
        }
    }

    pub async fn start(&mut self) -> Result<WorkerStart, PhpWorkerError> {
        self.start_inner(None, None).await
    }

    async fn start_guarded(
        &mut self,
        spawn_gate: &SpawnGate,
        shutdown_phase: Option<&mut watch::Receiver<ServingPhase>>,
    ) -> Result<WorkerStart, PhpWorkerError> {
        self.start_inner(Some(spawn_gate), shutdown_phase).await
    }

    async fn start_inner(
        &mut self,
        spawn_gate: Option<&SpawnGate>,
        shutdown_phase: Option<&mut watch::Receiver<ServingPhase>>,
    ) -> Result<WorkerStart, PhpWorkerError> {
        if !matches!(self.inner, WorkerSlot::Starting) {
            return Err(PhpWorkerError::Startup(
                "worker start was requested more than once".to_owned(),
            ));
        }

        let started = Instant::now();
        let mut command = Command::new(&self.config.executable);
        command
            .arg(&self.config.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = if let Some(spawn_gate) = spawn_gate {
            let phase = spawn_gate
                .phase
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *phase == ServingPhase::ShuttingDown {
                return Err(PhpWorkerError::Startup(
                    "pool shutdown started before process spawn".to_owned(),
                ));
            }
            command.spawn()
        } else {
            command.spawn()
        }
        .map_err(|source| PhpWorkerError::Spawn {
            executable: self.config.executable.clone(),
            source,
        })?;
        let spawned_pid = child.id().ok_or_else(|| {
            PhpWorkerError::Startup("spawned PHP process has no process ID".to_owned())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PhpWorkerError::Startup("PHP worker stdin was not piped".to_owned()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| PhpWorkerError::Startup("PHP worker stdout was not piped".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| PhpWorkerError::Startup("PHP worker stderr was not piped".to_owned()))?;
        let stderr_task = spawn_stderr_drain(stderr);

        let handshake = match shutdown_phase {
            Some(shutdown_phase) => {
                tokio::select! {
                    handshake = timeout(self.timeouts.startup, read_frame(&mut stdout)) => handshake,
                    _ = wait_for_shutdown_phase(shutdown_phase) => {
                        terminate_child(&mut child).await;
                        let _ = stderr_task.await;
                        let message = "pool shutdown interrupted replacement bootstrap".to_owned();
                        self.inner = WorkerSlot::Unavailable(message.clone());
                        return Err(PhpWorkerError::Startup(message));
                    }
                }
            }
            None => timeout(self.timeouts.startup, read_frame(&mut stdout)).await,
        };
        let ready = match handshake {
            Ok(Ok(payload)) => decode_ready(&payload).map_err(|error| error.to_string()),
            Ok(Err(error)) => Err(startup_io_context(&mut child, error).await),
            Err(_) => Err(format!(
                "READY timeout after {} ms",
                self.timeouts.startup.as_millis()
            )),
        };
        let ready = match ready {
            Ok(ready) if ready.pid == spawned_pid => ready,
            Ok(ready) => {
                terminate_child(&mut child).await;
                let _ = stderr_task.await;
                let message = format!(
                    "READY PID {} does not match spawned PID {spawned_pid}",
                    ready.pid
                );
                self.inner = WorkerSlot::Unavailable(message.clone());
                return Err(PhpWorkerError::Startup(message));
            }
            Err(message) => {
                terminate_child(&mut child).await;
                let _ = stderr_task.await;
                self.inner = WorkerSlot::Unavailable(message.clone());
                return Err(PhpWorkerError::Startup(message));
            }
        };

        let report = WorkerStart {
            pid: ready.pid,
            elapsed: started.elapsed(),
        };
        self.inner = WorkerSlot::Running(Box::new(RunningWorker {
            child,
            stdin,
            stdout,
            stderr_task,
            pid: ready.pid,
            next_request_id: 1,
            state: WorkerState::Ready,
        }));
        Ok(report)
    }

    pub fn state(&self) -> WorkerState {
        match &self.inner {
            WorkerSlot::Starting => WorkerState::Starting,
            WorkerSlot::Running(worker) => worker.state,
            WorkerSlot::Unavailable(_) => WorkerState::Unavailable,
            WorkerSlot::Stopped => WorkerState::Stopped,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        match &self.inner {
            WorkerSlot::Running(worker) => Some(worker.pid),
            WorkerSlot::Starting | WorkerSlot::Unavailable(_) | WorkerSlot::Stopped => None,
        }
    }

    pub(crate) async fn execute(
        &mut self,
        request: WorkerRequest,
    ) -> Result<WorkerResponse, PhpWorkerError> {
        let prior_exit = match &mut self.inner {
            WorkerSlot::Running(worker) => worker.child.try_wait(),
            WorkerSlot::Unavailable(reason) => {
                return Err(PhpWorkerError::Unavailable(reason.clone()));
            }
            WorkerSlot::Starting => {
                return Err(PhpWorkerError::Unavailable(
                    "worker has not completed startup".to_owned(),
                ));
            }
            WorkerSlot::Stopped => {
                return Err(PhpWorkerError::Unavailable("worker is stopped".to_owned()));
            }
        };
        let prior_exit = match prior_exit {
            Ok(status) => status,
            Err(error) => {
                let message = format!("cannot inspect worker process: {error}");
                fail_worker(&mut self.inner, &message).await;
                return Err(PhpWorkerError::Request(message));
            }
        };
        if let Some(status) = prior_exit {
            let message = format!("process exited before request with status {status}");
            self.inner = WorkerSlot::Unavailable(message.clone());
            return Err(PhpWorkerError::Unavailable(message));
        }

        let transaction = {
            let worker = match &mut self.inner {
                WorkerSlot::Running(worker) => worker,
                _ => unreachable!("running worker was checked above"),
            };
            worker.state = WorkerState::Busy;
            let request_id = worker.next_request_id;
            worker.next_request_id = worker.next_request_id.wrapping_add(1).max(1);
            let payload = match encode_request(request_id, &request) {
                Ok(payload) => payload,
                Err(error) => {
                    worker.state = WorkerState::Ready;
                    return Err(PhpWorkerError::Request(error.to_string()));
                }
            };
            let operation = async {
                write_frame(&mut worker.stdin, &payload).await?;
                let payload = read_frame(&mut worker.stdout).await?;
                let response = decode_response(&payload)?;
                if response.request_id != request_id {
                    return Err(ProtocolError::Invalid(format!(
                        "response ID {} does not match request ID {request_id}",
                        response.request_id
                    )));
                }
                Ok(response)
            };
            match timeout(self.timeouts.request, operation).await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err(format!(
                    "timeout after {} ms",
                    self.timeouts.request.as_millis()
                )),
            }
        };

        match transaction {
            Ok(response) => {
                if let WorkerSlot::Running(worker) = &mut self.inner {
                    worker.state = WorkerState::Ready;
                }
                Ok(response)
            }
            Err(message) => {
                fail_worker(&mut self.inner, &message).await;
                Err(PhpWorkerError::Request(message))
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), PhpWorkerError> {
        let slot = std::mem::replace(&mut self.inner, WorkerSlot::Stopped);
        let WorkerSlot::Running(mut worker) = slot else {
            return Ok(());
        };

        match worker.child.try_wait() {
            Ok(Some(_)) => {
                let _ = worker.stderr_task.await;
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut worker.child).await;
                let _ = worker.stderr_task.await;
                return Err(PhpWorkerError::Shutdown(format!(
                    "cannot inspect worker process: {error}"
                )));
            }
        }

        let shutdown_started = Instant::now();
        let graceful = timeout(self.timeouts.shutdown, async {
            write_frame(&mut worker.stdin, &encode_shutdown()).await?;
            let acknowledgment = read_frame(&mut worker.stdout).await?;
            decode_shutdown_ack(&acknowledgment)?;
            worker.stdin.shutdown().await?;
            worker.child.wait().await.map_err(ProtocolError::Io)
        })
        .await;

        match graceful {
            Ok(Ok(status)) if status.success() => {
                let _ = worker.stderr_task.await;
                Ok(())
            }
            Ok(Ok(status)) => {
                let _ = worker.stderr_task.await;
                Err(PhpWorkerError::Shutdown(format!(
                    "worker exited with status {status}"
                )))
            }
            Ok(Err(error)) => {
                let remaining = self
                    .timeouts
                    .shutdown
                    .saturating_sub(shutdown_started.elapsed());
                if matches!(timeout(remaining, worker.child.wait()).await, Ok(Ok(_))) {
                    let _ = worker.stderr_task.await;
                    return Ok(());
                }
                terminate_child(&mut worker.child).await;
                let _ = worker.stderr_task.await;
                Err(PhpWorkerError::Shutdown(error.to_string()))
            }
            Err(_) => {
                terminate_child(&mut worker.child).await;
                let _ = worker.stderr_task.await;
                Err(PhpWorkerError::Shutdown(format!(
                    "timeout after {} ms; worker was killed",
                    self.timeouts.shutdown.as_millis()
                )))
            }
        }
    }
}

pub struct PhpWorkerPool {
    shared: Arc<PoolShared>,
}

struct PoolShared {
    config: PhpWorkerPoolConfig,
    worker_configs: Vec<PhpWorkerConfig>,
    jobs_tx: OnceLock<mpsc::Sender<PhpJob>>,
    accepting: AtomicBool,
    lifecycle: Mutex<PoolLifecycle>,
    spawn_gate: SpawnGate,
    slots: Vec<RwLock<WorkerSnapshot>>,
    live_workers: AtomicUsize,
    ready_workers: AtomicUsize,
    total_respawns: AtomicU64,
    recycle_owner: AtomicUsize,
}

enum PoolLifecycle {
    New,
    Starting,
    Running(PoolRuntime),
    Stopped,
}

struct PoolRuntime {
    shutdown_tx: Option<oneshot::Sender<()>>,
    supervisor: JoinHandle<Result<(), PhpWorkerError>>,
}

struct PhpJob {
    request: WorkerRequest,
    response_tx: oneshot::Sender<Result<WorkerResponse, PoolJobError>>,
}

enum PoolJobError {
    Worker(String),
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
enum PoolExecuteError {
    QueueFull,
    Unavailable,
    Worker(String),
}

enum WorkerCommand {
    Job(PhpJob),
}

/// A dispatch capability for exactly one generation.
///
/// Deliberately not `Clone`. A generation may have at most one capability
/// outstanding, which is what bounds its capacity-1 mailbox to a single
/// buffered job and makes `TrySendError::Full` unreachable. That bound is held
/// only by the shape of `run_worker_generation`, which publishes one
/// capability per command it consumes; cloning one would break it silently.
struct IdleWorker {
    slot: usize,
    generation: u64,
    mailbox: mpsc::Sender<WorkerCommand>,
}

enum GenerationOutcome {
    Failed(String),
    Recycle(WorkerSpawnReason),
    Stopped(Result<(), PhpWorkerError>),
    BootstrapFailed(String),
    CancelledBeforeSpawn,
}

struct GenerationExit {
    slot: usize,
    generation: u64,
    idle_tx: mpsc::Sender<IdleWorker>,
    outcome: GenerationOutcome,
}

struct GenerationSpec {
    slot: usize,
    generation: u64,
    spawn_reason: WorkerSpawnReason,
    delay: Duration,
    idle_tx: mpsc::Sender<IdleWorker>,
    phase_rx: watch::Receiver<ServingPhase>,
    worker_shutdown_rx: watch::Receiver<bool>,
    shared: Arc<PoolShared>,
}

struct InitialGenerationSpec {
    slot: usize,
    generation: u64,
    spawn_reason: WorkerSpawnReason,
    worker: PhpWorker,
    worker_tx: mpsc::Sender<WorkerCommand>,
    worker_rx: mpsc::Receiver<WorkerCommand>,
    idle_tx: mpsc::Sender<IdleWorker>,
    worker_shutdown_rx: watch::Receiver<bool>,
    shared: Arc<PoolShared>,
    spawned_at: Instant,
}

impl PhpWorkerPool {
    pub fn new(config: PhpWorkerPoolConfig) -> Self {
        let workers = config.workers;
        let worker_configs = vec![config.worker.clone(); workers];
        let slots = (0..workers)
            .map(|slot| RwLock::new(initial_worker_snapshot(slot)))
            .collect();
        Self {
            shared: Arc::new(PoolShared {
                config,
                worker_configs,
                jobs_tx: OnceLock::new(),
                accepting: AtomicBool::new(false),
                lifecycle: Mutex::new(PoolLifecycle::New),
                spawn_gate: SpawnGate::new(),
                slots,
                live_workers: AtomicUsize::new(0),
                ready_workers: AtomicUsize::new(0),
                total_respawns: AtomicU64::new(0),
                recycle_owner: AtomicUsize::new(0),
            }),
        }
    }

    #[cfg(test)]
    fn with_worker_configs(worker_configs: Vec<PhpWorkerConfig>, queue_capacity: usize) -> Self {
        let workers = worker_configs.len();
        let config = PhpWorkerPoolConfig {
            worker: worker_configs[0].clone(),
            workers,
            queue_capacity,
            max_requests: None,
            max_worker_age: None,
        };
        let slots = (0..workers)
            .map(|slot| RwLock::new(initial_worker_snapshot(slot)))
            .collect();
        Self {
            shared: Arc::new(PoolShared {
                config,
                worker_configs,
                jobs_tx: OnceLock::new(),
                accepting: AtomicBool::new(false),
                lifecycle: Mutex::new(PoolLifecycle::New),
                spawn_gate: SpawnGate::new(),
                slots,
                live_workers: AtomicUsize::new(0),
                ready_workers: AtomicUsize::new(0),
                total_respawns: AtomicU64::new(0),
                recycle_owner: AtomicUsize::new(0),
            }),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.shared.config.workers
    }

    pub fn queue_capacity(&self) -> usize {
        self.shared.config.queue_capacity
    }

    pub fn max_requests(&self) -> Option<u64> {
        self.shared.config.max_requests
    }

    pub fn max_worker_age(&self) -> Option<Duration> {
        self.shared.config.max_worker_age
    }

    pub fn pids(&self) -> Vec<u32> {
        self.snapshot()
            .workers
            .into_iter()
            .filter_map(|worker| worker.pid)
            .collect()
    }

    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            target_workers: self.shared.config.workers,
            ready_workers: self.shared.ready_workers.load(Ordering::Acquire),
            total_respawns: self.shared.total_respawns.load(Ordering::Acquire),
            workers: self
                .shared
                .slots
                .iter()
                .map(|slot| {
                    slot.read()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                })
                .collect(),
        }
    }

    pub async fn start(&self) -> Result<Vec<WorkerStart>, PhpWorkerError> {
        {
            let mut lifecycle = self.shared.lifecycle.lock().await;
            if !matches!(*lifecycle, PoolLifecycle::New) {
                return Err(PhpWorkerError::Startup(
                    "worker pool start was requested more than once".to_owned(),
                ));
            }
            *lifecycle = PoolLifecycle::Starting;
        }

        let mut startup = JoinSet::new();
        for (index, config) in self.shared.worker_configs.iter().cloned().enumerate() {
            let shared = Arc::clone(&self.shared);
            startup.spawn(async move {
                let mut worker = PhpWorker::new(config);
                let result = worker.start_guarded(&shared.spawn_gate, None).await;
                (index, worker, result)
            });
        }

        let mut workers = Vec::with_capacity(self.shared.config.workers);
        workers.resize_with(self.shared.config.workers, || None);
        let mut first_error = None;
        while let Some(joined) = startup.join_next().await {
            match joined {
                Ok((index, worker, Ok(report))) => workers[index] = Some((worker, report)),
                Ok((index, _worker, Err(error))) => {
                    first_error.get_or_insert_with(|| {
                        PhpWorkerError::Startup(format!("worker {index}: {error}"))
                    });
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        PhpWorkerError::Startup(format!("worker startup task failed: {error}"))
                    });
                }
            }
        }

        if let Some(error) = first_error {
            let mut cleanup = JoinSet::new();
            for (mut worker, _) in workers.into_iter().flatten() {
                cleanup.spawn(async move { worker.shutdown().await });
            }
            while cleanup.join_next().await.is_some() {}
            *self.shared.lifecycle.lock().await = PoolLifecycle::Stopped;
            return Err(error);
        }

        if !self.shared.spawn_gate.is_serving() {
            let mut cleanup = JoinSet::new();
            for (mut worker, _) in workers.into_iter().flatten() {
                cleanup.spawn(async move { worker.shutdown().await });
            }
            while cleanup.join_next().await.is_some() {}
            *self.shared.lifecycle.lock().await = PoolLifecycle::Stopped;
            return Err(PhpWorkerError::Startup(
                "worker pool shutdown started during startup".to_owned(),
            ));
        }

        let (jobs_tx, jobs_rx) = mpsc::channel(self.shared.config.queue_capacity);
        let idle_capacity = self.shared.config.workers.saturating_mul(2).max(1);
        let (idle_tx, idle_rx) = mpsc::channel(idle_capacity);
        let (worker_shutdown_tx, worker_shutdown_rx) = watch::channel(false);
        let (phase_tx, phase_rx) = watch::channel(ServingPhase::Serving);
        let mut generation_tasks = JoinSet::new();
        let mut reports = Vec::with_capacity(self.shared.config.workers);

        for (slot, entry) in workers.into_iter().enumerate() {
            let (worker, report) = entry.expect("all PHP workers reached READY");
            let spawned_at = Instant::now();
            self.shared
                .mark_ready(slot, 1, report.pid, WorkerSpawnReason::Initial, spawned_at);
            reports.push(report);
            let (worker_tx, worker_rx) = mpsc::channel(1);
            spawn_initial_generation(
                &mut generation_tasks,
                InitialGenerationSpec {
                    slot,
                    generation: 1,
                    spawn_reason: WorkerSpawnReason::Initial,
                    worker,
                    worker_tx,
                    worker_rx,
                    idle_tx: idle_tx.clone(),
                    worker_shutdown_rx: worker_shutdown_rx.clone(),
                    shared: Arc::clone(&self.shared),
                    spawned_at,
                },
            );
        }
        // No supervisor keeper exists. Each sender is owned by a live
        // generation and later transferred to its replacement task.
        drop(idle_tx);

        let (dispatcher_shutdown_tx, dispatcher_shutdown_rx) = oneshot::channel();
        let shared = Arc::clone(&self.shared);
        let dispatcher = tokio::spawn(dispatcher_task(
            jobs_rx,
            idle_rx,
            dispatcher_shutdown_rx,
            shared,
        ));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let supervisor = tokio::spawn(supervisor_task(
            generation_tasks,
            dispatcher,
            dispatcher_shutdown_tx,
            worker_shutdown_tx,
            phase_tx,
            phase_rx,
            shutdown_rx,
            Arc::clone(&self.shared),
        ));
        self.shared.jobs_tx.set(jobs_tx).map_err(|_| {
            PhpWorkerError::Startup("worker pool job channel was initialized twice".to_owned())
        })?;
        let mut lifecycle = self.shared.lifecycle.lock().await;
        if !matches!(*lifecycle, PoolLifecycle::Starting) {
            drop(lifecycle);
            self.shared.accepting.store(false, Ordering::Release);
            self.shared.spawn_gate.begin_shutdown();
            let _ = shutdown_tx.send(());
            let _ = supervisor.await;
            return Err(PhpWorkerError::Startup(
                "worker pool shutdown started during startup".to_owned(),
            ));
        }
        self.shared.accepting.store(true, Ordering::Release);
        *lifecycle = PoolLifecycle::Running(PoolRuntime {
            shutdown_tx: Some(shutdown_tx),
            supervisor,
        });
        Ok(reports)
    }

    pub async fn shutdown(&self) -> Result<(), PhpWorkerError> {
        self.shared.accepting.store(false, Ordering::Release);
        self.shared.spawn_gate.begin_shutdown();
        let runtime = {
            let mut lifecycle = self.shared.lifecycle.lock().await;
            match std::mem::replace(&mut *lifecycle, PoolLifecycle::Stopped) {
                PoolLifecycle::Running(runtime) => Some(runtime),
                PoolLifecycle::New | PoolLifecycle::Starting | PoolLifecycle::Stopped => None,
            }
        };
        let Some(mut runtime) = runtime else {
            return Ok(());
        };

        if let Some(shutdown_tx) = runtime.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        match runtime.supervisor.await {
            Ok(result) => result,
            Err(error) => Err(PhpWorkerError::Shutdown(format!(
                "lifecycle supervisor failed: {error}"
            ))),
        }
    }

    pub(crate) async fn serve_http(&self, request: Request<Incoming>) -> HandlerResponse {
        let request = match http_request(request).await {
            Ok(request) => request,
            Err(HttpRequestError::TooLarge) => {
                return simple_response(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large");
            }
            Err(HttpRequestError::TooManyHeaders) => {
                return simple_response(
                    StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                    "Too Many Headers",
                );
            }
            Err(HttpRequestError::Body(error)) => {
                eprintln!("failed to read request body for PHP worker: {error}");
                return simple_response(StatusCode::BAD_REQUEST, "Invalid Request Body");
            }
        };

        match self.execute(request).await {
            Ok(response) => match http_response(response) {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("invalid PHP worker response: {error}");
                    simple_response(StatusCode::BAD_GATEWAY, "PHP Worker Error")
                }
            },
            Err(PoolExecuteError::QueueFull) => {
                simple_response(StatusCode::SERVICE_UNAVAILABLE, "PHP Worker Queue Full")
            }
            Err(PoolExecuteError::Unavailable) => simple_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "PHP Worker Pool Unavailable",
            ),
            Err(PoolExecuteError::Worker(error)) => {
                eprintln!("PHP worker request failed: {error}");
                simple_response(StatusCode::BAD_GATEWAY, "PHP Worker Unavailable")
            }
        }
    }

    async fn execute(&self, request: WorkerRequest) -> Result<WorkerResponse, PoolExecuteError> {
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(PoolExecuteError::Unavailable);
        }
        if self.shared.live_workers.load(Ordering::Acquire) == 0 {
            return Err(PoolExecuteError::Unavailable);
        }
        let jobs_tx = self
            .shared
            .jobs_tx
            .get()
            .ok_or(PoolExecuteError::Unavailable)?;
        let (response_tx, response_rx) = oneshot::channel();
        let job = PhpJob {
            request,
            response_tx,
        };
        match jobs_tx.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(PoolExecuteError::QueueFull);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(PoolExecuteError::Unavailable);
            }
        }
        match response_rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(PoolJobError::Unavailable)) => Err(PoolExecuteError::Unavailable),
            Ok(Err(PoolJobError::Worker(error))) => Err(PoolExecuteError::Worker(error)),
            Err(_) => Err(PoolExecuteError::Unavailable),
        }
    }
}

fn initial_worker_snapshot(slot: usize) -> WorkerSnapshot {
    WorkerSnapshot {
        slot,
        generation: 0,
        pid: None,
        state: WorkerLifecycleState::Starting,
        spawn_reason: WorkerSpawnReason::Initial,
        request_count: 0,
        spawned_at: None,
        consecutive_failures: 0,
        last_exit_reason: None,
    }
}

impl PoolShared {
    fn update_slot(&self, slot: usize, update: impl FnOnce(&mut WorkerSnapshot)) {
        let mut snapshot = self.slots[slot]
            .write()
            .unwrap_or_else(|error| error.into_inner());
        update(&mut snapshot);
    }

    fn begin_generation(
        &self,
        slot: usize,
        generation: u64,
        spawn_reason: WorkerSpawnReason,
        delay: Duration,
    ) {
        self.update_slot(slot, |snapshot| {
            snapshot.generation = generation;
            snapshot.pid = None;
            snapshot.state = if delay.is_zero() {
                WorkerLifecycleState::Bootstrapping
            } else {
                WorkerLifecycleState::Backoff
            };
            snapshot.spawn_reason = spawn_reason;
            snapshot.request_count = 0;
            snapshot.spawned_at = None;
        });
    }

    fn mark_ready(
        &self,
        slot: usize,
        generation: u64,
        pid: u32,
        spawn_reason: WorkerSpawnReason,
        spawned_at: Instant,
    ) {
        self.update_slot(slot, |snapshot| {
            snapshot.generation = generation;
            snapshot.pid = Some(pid);
            snapshot.state = WorkerLifecycleState::Ready;
            snapshot.spawn_reason = spawn_reason;
            snapshot.request_count = 0;
            snapshot.spawned_at = Some(spawned_at);
        });
        self.live_workers.fetch_add(1, Ordering::AcqRel);
        self.ready_workers.fetch_add(1, Ordering::AcqRel);
    }

    fn mark_busy(&self, slot: usize, generation: u64) {
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation && snapshot.state == WorkerLifecycleState::Ready {
                snapshot.state = WorkerLifecycleState::Busy;
                self.ready_workers.fetch_sub(1, Ordering::AcqRel);
            }
        });
    }

    fn mark_ready_after_job(&self, slot: usize, generation: u64) {
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation && snapshot.state == WorkerLifecycleState::Busy {
                snapshot.state = WorkerLifecycleState::Ready;
                self.ready_workers.fetch_add(1, Ordering::AcqRel);
            }
        });
    }

    fn record_success(&self, slot: usize, generation: u64, spawned_at: Instant) -> u64 {
        let mut request_count = 0;
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation {
                snapshot.request_count = snapshot.request_count.saturating_add(1);
                request_count = snapshot.request_count;
                if spawned_at.elapsed() >= STABILITY_RESET_PERIOD {
                    snapshot.consecutive_failures = 0;
                }
            }
        });
        request_count
    }

    fn mark_recycling(&self, slot: usize, generation: u64) {
        self.update_slot(slot, |snapshot| {
            if snapshot.generation != generation {
                return;
            }
            if snapshot.state == WorkerLifecycleState::Ready {
                self.ready_workers.fetch_sub(1, Ordering::AcqRel);
            }
            snapshot.state = WorkerLifecycleState::Recycling;
        });
    }

    fn mark_unavailable(&self, slot: usize, generation: u64) {
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation {
                if snapshot.state == WorkerLifecycleState::Ready {
                    self.ready_workers.fetch_sub(1, Ordering::AcqRel);
                }
                snapshot.state = WorkerLifecycleState::Unavailable;
            }
        });
    }

    fn record_exit(&self, exit: &GenerationExit, reason: String) {
        self.update_slot(exit.slot, |snapshot| {
            if snapshot.generation != exit.generation {
                return;
            }
            if snapshot.pid.take().is_some() {
                self.live_workers.fetch_sub(1, Ordering::AcqRel);
            }
            if snapshot.state == WorkerLifecycleState::Ready {
                self.ready_workers.fetch_sub(1, Ordering::AcqRel);
            }
            snapshot.last_exit_reason = Some(reason);
            snapshot.state = WorkerLifecycleState::Unavailable;
        });
    }

    fn increment_failure(&self, slot: usize) -> usize {
        let mut failures = 0;
        self.update_slot(slot, |snapshot| {
            snapshot.consecutive_failures = snapshot.consecutive_failures.saturating_add(1);
            failures = snapshot.consecutive_failures;
        });
        failures
    }

    fn retire_slot(&self, slot: usize) {
        self.update_slot(slot, |snapshot| {
            snapshot.pid = None;
            snapshot.state = WorkerLifecycleState::Retired;
        });
    }

    fn stop_slot(&self, slot: usize, generation: u64) {
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation {
                if snapshot.pid.take().is_some() {
                    self.live_workers.fetch_sub(1, Ordering::AcqRel);
                }
                if snapshot.state == WorkerLifecycleState::Ready {
                    self.ready_workers.fetch_sub(1, Ordering::AcqRel);
                }
                snapshot.state = WorkerLifecycleState::Stopped;
            }
        });
    }

    fn try_acquire_recycle(&self, slot: usize) -> bool {
        self.recycle_owner
            .compare_exchange(0, slot + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release_recycle(&self, slot: usize) {
        let _ =
            self.recycle_owner
                .compare_exchange(slot + 1, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

fn spawn_initial_generation(tasks: &mut JoinSet<GenerationExit>, spec: InitialGenerationSpec) {
    let slot = spec.slot;
    let generation = spec.generation;
    let fallback_idle_tx = spec.idle_tx.clone();
    tasks.spawn(async move {
        let task = tokio::spawn(run_worker_generation(
            spec.slot,
            spec.generation,
            spec.spawn_reason,
            spec.worker,
            spec.worker_tx,
            spec.worker_rx,
            spec.idle_tx,
            spec.worker_shutdown_rx,
            spec.shared,
            Some(spec.spawned_at),
        ));
        match task.await {
            Ok(exit) => exit,
            Err(error) => GenerationExit {
                slot,
                generation,
                idle_tx: fallback_idle_tx,
                outcome: GenerationOutcome::Failed(format!(
                    "generation task terminated unexpectedly: {error}"
                )),
            },
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_generation(
    slot: usize,
    generation: u64,
    spawn_reason: WorkerSpawnReason,
    mut worker: PhpWorker,
    worker_tx: mpsc::Sender<WorkerCommand>,
    mut worker_rx: mpsc::Receiver<WorkerCommand>,
    idle_tx: mpsc::Sender<IdleWorker>,
    mut shutdown_rx: watch::Receiver<bool>,
    shared: Arc<PoolShared>,
    already_ready_at: Option<Instant>,
) -> GenerationExit {
    let spawned_at = already_ready_at.unwrap_or_else(Instant::now);
    if already_ready_at.is_none() {
        let pid = worker
            .pid()
            .expect("started PHP generation must have a PID");
        shared.mark_ready(slot, generation, pid, spawn_reason, spawned_at);
    }
    if matches!(
        spawn_reason,
        WorkerSpawnReason::MaxRequests | WorkerSpawnReason::MaxAge
    ) {
        shared.release_recycle(slot);
    }
    let idle = || IdleWorker {
        slot,
        generation,
        mailbox: worker_tx.clone(),
    };
    if idle_tx.send(idle()).await.is_err() {
        let result = worker.shutdown().await;
        return GenerationExit {
            slot,
            generation,
            idle_tx,
            outcome: GenerationOutcome::Stopped(result),
        };
    }

    let mut next_recycle_check = shared
        .config
        .max_worker_age
        .map(|maximum| (spawned_at + maximum, WorkerSpawnReason::MaxAge));
    loop {
        let command = tokio::select! {
            command = worker_rx.recv() => command,
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    None
                } else {
                    continue;
                }
            }
            _ = async {
                match next_recycle_check {
                    Some((deadline, _)) => tokio::time::sleep_until(deadline.into()).await,
                    None => pending::<()>().await,
                }
            } => {
                let Some((_, reason)) = next_recycle_check else {
                    continue;
                };
                if shared.try_acquire_recycle(slot) {
                    shared.mark_recycling(slot, generation);
                    worker_rx.close();
                    let outcome = drain_and_stop_generation(
                        slot,
                        generation,
                        &mut worker,
                        &mut worker_rx,
                        &shared,
                        Some(reason),
                        spawned_at,
                    ).await;
                    return GenerationExit { slot, generation, idle_tx, outcome };
                }
                next_recycle_check = Some((Instant::now() + RECYCLE_RETRY_DELAY, reason));
                continue;
            }
        };
        let Some(command) = command else {
            worker_rx.close();
            let outcome = drain_and_stop_generation(
                slot,
                generation,
                &mut worker,
                &mut worker_rx,
                &shared,
                None,
                spawned_at,
            )
            .await;
            return GenerationExit {
                slot,
                generation,
                idle_tx,
                outcome,
            };
        };
        match command {
            WorkerCommand::Job(job) => {
                if job.response_tx.is_closed() {
                    if idle_tx.send(idle()).await.is_err() {
                        break;
                    }
                    continue;
                }
                shared.mark_busy(slot, generation);
                match worker.execute(job.request).await {
                    Ok(response) => {
                        let request_count = shared.record_success(slot, generation, spawned_at);
                        let _ = job.response_tx.send(Ok(response));
                        if *shutdown_rx.borrow() {
                            worker_rx.close();
                            let outcome = drain_and_stop_generation(
                                slot,
                                generation,
                                &mut worker,
                                &mut worker_rx,
                                &shared,
                                None,
                                spawned_at,
                            )
                            .await;
                            return GenerationExit {
                                slot,
                                generation,
                                idle_tx,
                                outcome,
                            };
                        }
                        let recycle_reason = if shared
                            .config
                            .max_requests
                            .is_some_and(|maximum| request_count >= maximum)
                        {
                            Some(WorkerSpawnReason::MaxRequests)
                        } else if shared
                            .config
                            .max_worker_age
                            .is_some_and(|maximum| spawned_at.elapsed() >= maximum)
                        {
                            Some(WorkerSpawnReason::MaxAge)
                        } else {
                            None
                        };
                        if let Some(reason) = recycle_reason {
                            if shared.try_acquire_recycle(slot) {
                                shared.mark_recycling(slot, generation);
                                worker_rx.close();
                                let outcome = drain_and_stop_generation(
                                    slot,
                                    generation,
                                    &mut worker,
                                    &mut worker_rx,
                                    &shared,
                                    Some(reason),
                                    spawned_at,
                                )
                                .await;
                                return GenerationExit {
                                    slot,
                                    generation,
                                    idle_tx,
                                    outcome,
                                };
                            }
                            next_recycle_check =
                                Some((Instant::now() + RECYCLE_RETRY_DELAY, reason));
                        }
                        shared.mark_ready_after_job(slot, generation);
                        if idle_tx.send(idle()).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = job.response_tx.send(Err(PoolJobError::Worker(message)));
                        shared.mark_unavailable(slot, generation);
                        eprintln!(
                            "PHP worker slot {slot} generation {generation} became unavailable: {error}"
                        );
                        return GenerationExit {
                            slot,
                            generation,
                            idle_tx,
                            outcome: GenerationOutcome::Failed(error.to_string()),
                        };
                    }
                }
            }
        }
    }

    let result = worker.shutdown().await;
    GenerationExit {
        slot,
        generation,
        idle_tx,
        outcome: GenerationOutcome::Stopped(result),
    }
}

async fn drain_and_stop_generation(
    slot: usize,
    generation: u64,
    worker: &mut PhpWorker,
    worker_rx: &mut mpsc::Receiver<WorkerCommand>,
    shared: &PoolShared,
    recycle_reason: Option<WorkerSpawnReason>,
    spawned_at: Instant,
) -> GenerationOutcome {
    while let Some(WorkerCommand::Job(job)) = worker_rx.recv().await {
        if job.response_tx.is_closed() {
            continue;
        }
        shared.mark_busy(slot, generation);
        match worker.execute(job.request).await {
            Ok(response) => {
                shared.record_success(slot, generation, spawned_at);
                let _ = job.response_tx.send(Ok(response));
            }
            Err(error) => {
                let message = error.to_string();
                let _ = job.response_tx.send(Err(PoolJobError::Worker(message)));
                shared.mark_unavailable(slot, generation);
                return GenerationOutcome::Failed(error.to_string());
            }
        }
    }
    match worker.shutdown().await {
        Ok(()) => match recycle_reason {
            Some(reason) => GenerationOutcome::Recycle(reason),
            None => GenerationOutcome::Stopped(Ok(())),
        },
        Err(error) if recycle_reason.is_some() => GenerationOutcome::Failed(format!(
            "planned recycle failed to stop generation: {error}"
        )),
        Err(error) => GenerationOutcome::Stopped(Err(error)),
    }
}

async fn replacement_generation_task(mut spec: GenerationSpec) -> GenerationExit {
    if !spec.delay.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(spec.delay) => {}
            changed = spec.phase_rx.changed() => {
                if changed.is_err() || *spec.phase_rx.borrow() == ServingPhase::ShuttingDown {
                    return GenerationExit {
                        slot: spec.slot,
                        generation: spec.generation,
                        idle_tx: spec.idle_tx,
                        outcome: GenerationOutcome::CancelledBeforeSpawn,
                    };
                }
            }
        }
    }
    if !spec.shared.spawn_gate.is_serving() {
        return GenerationExit {
            slot: spec.slot,
            generation: spec.generation,
            idle_tx: spec.idle_tx,
            outcome: GenerationOutcome::CancelledBeforeSpawn,
        };
    }
    spec.shared.update_slot(spec.slot, |snapshot| {
        snapshot.state = WorkerLifecycleState::Bootstrapping;
    });
    let mut worker = PhpWorker::new(spec.shared.worker_configs[spec.slot].clone());
    match worker
        .start_guarded(&spec.shared.spawn_gate, Some(&mut spec.phase_rx))
        .await
    {
        Ok(_) if spec.shared.spawn_gate.is_serving() => {
            let (worker_tx, worker_rx) = mpsc::channel(1);
            run_worker_generation(
                spec.slot,
                spec.generation,
                spec.spawn_reason,
                worker,
                worker_tx,
                worker_rx,
                spec.idle_tx,
                spec.worker_shutdown_rx,
                spec.shared,
                None,
            )
            .await
        }
        Ok(_) => {
            let result = worker.shutdown().await;
            GenerationExit {
                slot: spec.slot,
                generation: spec.generation,
                idle_tx: spec.idle_tx,
                outcome: GenerationOutcome::Stopped(result),
            }
        }
        Err(error) => GenerationExit {
            slot: spec.slot,
            generation: spec.generation,
            idle_tx: spec.idle_tx,
            outcome: GenerationOutcome::BootstrapFailed(error.to_string()),
        },
    }
}

fn backoff_delay(consecutive_failures: usize) -> Duration {
    if consecutive_failures <= 1 {
        return Duration::ZERO;
    }
    let exponent = (consecutive_failures - 2).min(6) as u32;
    Duration::from_millis((100_u64 << exponent).min(5_000))
}

fn replacement_delay(consecutive_failures: usize) -> Option<Duration> {
    (consecutive_failures <= MAX_CONSECUTIVE_FAILURES).then(|| backoff_delay(consecutive_failures))
}

fn spawn_replacement(
    tasks: &mut JoinSet<GenerationExit>,
    exit: GenerationExit,
    spawn_reason: WorkerSpawnReason,
    delay: Duration,
    phase_rx: &watch::Receiver<ServingPhase>,
    worker_shutdown_rx: &watch::Receiver<bool>,
    shared: &Arc<PoolShared>,
) {
    let generation = exit.generation.saturating_add(1);
    let slot = exit.slot;
    shared.begin_generation(exit.slot, generation, spawn_reason, delay);
    shared.total_respawns.fetch_add(1, Ordering::AcqRel);
    let spec = GenerationSpec {
        slot: exit.slot,
        generation,
        spawn_reason,
        delay,
        idle_tx: exit.idle_tx,
        phase_rx: phase_rx.clone(),
        worker_shutdown_rx: worker_shutdown_rx.clone(),
        shared: Arc::clone(shared),
    };
    let fallback_idle_tx = spec.idle_tx.clone();
    tasks.spawn(async move {
        let task = tokio::spawn(replacement_generation_task(spec));
        match task.await {
            Ok(exit) => exit,
            Err(error) => GenerationExit {
                slot,
                generation,
                idle_tx: fallback_idle_tx,
                outcome: GenerationOutcome::Failed(format!(
                    "replacement task terminated unexpectedly: {error}"
                )),
            },
        }
    });
}

fn handle_serving_exit(
    tasks: &mut JoinSet<GenerationExit>,
    exit: GenerationExit,
    phase_rx: &watch::Receiver<ServingPhase>,
    worker_shutdown_rx: &watch::Receiver<bool>,
    shared: &Arc<PoolShared>,
) {
    let (prior_reason, was_recycling) = {
        let snapshot = shared.slots[exit.slot]
            .read()
            .unwrap_or_else(|error| error.into_inner());
        (
            snapshot.spawn_reason,
            snapshot.state == WorkerLifecycleState::Recycling,
        )
    };
    match &exit.outcome {
        GenerationOutcome::Failed(reason) | GenerationOutcome::BootstrapFailed(reason) => {
            shared.record_exit(&exit, reason.clone());
            if was_recycling
                || matches!(
                    prior_reason,
                    WorkerSpawnReason::MaxRequests | WorkerSpawnReason::MaxAge
                )
            {
                shared.release_recycle(exit.slot);
            }
            let failures = shared.increment_failure(exit.slot);
            let Some(delay) = replacement_delay(failures) else {
                eprintln!(
                    "PHP worker slot {} retired after {} consecutive failures: {reason}",
                    exit.slot, failures
                );
                shared.retire_slot(exit.slot);
                return;
            };
            eprintln!(
                "PHP worker slot {} scheduling generation {} after {} ms (failure {failures})",
                exit.slot,
                exit.generation.saturating_add(1),
                delay.as_millis()
            );
            spawn_replacement(
                tasks,
                exit,
                WorkerSpawnReason::Respawn,
                delay,
                phase_rx,
                worker_shutdown_rx,
                shared,
            );
        }
        GenerationOutcome::Recycle(reason) => {
            let spawn_reason = *reason;
            shared.record_exit(&exit, format!("planned recycle: {reason:?}"));
            spawn_replacement(
                tasks,
                exit,
                spawn_reason,
                Duration::ZERO,
                phase_rx,
                worker_shutdown_rx,
                shared,
            );
        }
        GenerationOutcome::Stopped(result) => {
            let reason = match result {
                Ok(()) => "generation stopped".to_owned(),
                Err(error) => error.to_string(),
            };
            eprintln!(
                "PHP worker slot {} generation {} stopped unexpectedly: {reason}",
                exit.slot, exit.generation
            );
            shared.record_exit(&exit, reason);
            shared.stop_slot(exit.slot, exit.generation);
        }
        GenerationOutcome::CancelledBeforeSpawn => {
            shared.stop_slot(exit.slot, exit.generation);
        }
    }
}

fn handle_shutdown_exit(
    exit: GenerationExit,
    shared: &PoolShared,
    first_error: &mut Option<PhpWorkerError>,
) {
    let (prior_reason, was_recycling) = {
        let snapshot = shared.slots[exit.slot]
            .read()
            .unwrap_or_else(|error| error.into_inner());
        (
            snapshot.spawn_reason,
            snapshot.state == WorkerLifecycleState::Recycling,
        )
    };
    if was_recycling
        || matches!(
            prior_reason,
            WorkerSpawnReason::MaxRequests | WorkerSpawnReason::MaxAge
        )
    {
        shared.release_recycle(exit.slot);
    }
    let reason = match &exit.outcome {
        GenerationOutcome::Failed(reason) | GenerationOutcome::BootstrapFailed(reason) => {
            reason.clone()
        }
        GenerationOutcome::Recycle(reason) => format!("planned recycle: {reason:?}"),
        GenerationOutcome::Stopped(Ok(())) => "generation stopped during shutdown".to_owned(),
        GenerationOutcome::Stopped(Err(error)) => {
            first_error.get_or_insert_with(|| {
                PhpWorkerError::Shutdown(format!(
                    "slot {} generation {}: {error}",
                    exit.slot, exit.generation
                ))
            });
            error.to_string()
        }
        GenerationOutcome::CancelledBeforeSpawn => "replacement cancelled by shutdown".to_owned(),
    };
    shared.record_exit(&exit, reason);
    shared.stop_slot(exit.slot, exit.generation);
}

#[allow(clippy::too_many_arguments)]
async fn supervisor_task(
    mut generations: JoinSet<GenerationExit>,
    mut dispatcher: JoinHandle<()>,
    dispatcher_shutdown_tx: oneshot::Sender<()>,
    worker_shutdown_tx: watch::Sender<bool>,
    phase_tx: watch::Sender<ServingPhase>,
    phase_rx: watch::Receiver<ServingPhase>,
    mut shutdown_rx: oneshot::Receiver<()>,
    shared: Arc<PoolShared>,
) -> Result<(), PhpWorkerError> {
    let mut dispatcher_shutdown_tx = Some(dispatcher_shutdown_tx);
    let mut dispatcher_finished = None;
    let mut first_error = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            result = &mut dispatcher => {
                dispatcher_finished = Some(result);
                break;
            }
            joined = generations.join_next(), if !generations.is_empty() => {
                match joined {
                    Some(Ok(exit)) => handle_serving_exit(
                        &mut generations,
                        exit,
                        &phase_rx,
                        &worker_shutdown_tx.subscribe(),
                        &shared,
                    ),
                    Some(Err(error)) => {
                        first_error = Some(PhpWorkerError::Shutdown(format!(
                            "worker generation task failed: {error}"
                        )));
                        break;
                    }
                    None => {}
                }
            }
        }
    }

    shared.accepting.store(false, Ordering::Release);
    shared.spawn_gate.begin_shutdown();
    let _ = phase_tx.send(ServingPhase::ShuttingDown);

    if dispatcher_finished.is_none() {
        if let Some(shutdown_tx) = dispatcher_shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        loop {
            tokio::select! {
                result = &mut dispatcher => {
                    dispatcher_finished = Some(result);
                    break;
                }
                joined = generations.join_next(), if !generations.is_empty() => {
                    match joined {
                        Some(Ok(exit)) => handle_shutdown_exit(exit, &shared, &mut first_error),
                        Some(Err(error)) => {
                            first_error.get_or_insert_with(|| PhpWorkerError::Shutdown(format!(
                                "worker generation task failed during dispatcher drain: {error}"
                            )));
                        }
                        None => {}
                    }
                }
            }
        }
    }

    if let Some(Err(error)) = dispatcher_finished {
        first_error.get_or_insert_with(|| {
            PhpWorkerError::Shutdown(format!("dispatcher task failed: {error}"))
        });
    }

    // The dispatcher has converged, so closing idle mailboxes cannot strand an
    // accepted job waiting for capacity that will never be published.
    let _ = worker_shutdown_tx.send(true);
    while let Some(joined) = generations.join_next().await {
        match joined {
            Ok(exit) => handle_shutdown_exit(exit, &shared, &mut first_error),
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    PhpWorkerError::Shutdown(format!(
                        "worker generation task failed during shutdown: {error}"
                    ))
                });
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct DispatcherAvailabilityGuard {
    shared: Arc<PoolShared>,
}

impl Drop for DispatcherAvailabilityGuard {
    fn drop(&mut self) {
        self.shared.accepting.store(false, Ordering::Release);
    }
}

/// Accepts a freshly published capability. Stale capabilities are dropped only
/// once the stash exceeds one live token per slot, which keeps the steady-state
/// cost at a length comparison while still bounding the stash.
fn stash_idle(stash: &mut VecDeque<IdleWorker>, published: IdleWorker, workers: usize) {
    stash.push_back(published);
    if stash.len() > workers {
        stash.retain(|idle_worker| !idle_worker.mailbox.is_closed());
    }
}

/// Returns the next capability whose generation can still receive a job.
fn take_idle(stash: &mut VecDeque<IdleWorker>) -> Option<IdleWorker> {
    while let Some(idle_worker) = stash.pop_front() {
        if !idle_worker.mailbox.is_closed() {
            return Some(idle_worker);
        }
    }
    None
}

async fn dispatcher_task(
    mut jobs_rx: mpsc::Receiver<PhpJob>,
    mut idle_rx: mpsc::Receiver<IdleWorker>,
    mut shutdown_rx: oneshot::Receiver<()>,
    shared: Arc<PoolShared>,
) {
    // Covers normal return, panic, and task abort. Dropping jobs_rx and any
    // locally held job also closes their response oneshots, so callers cannot
    // remain parked if this single dispatcher task exits unexpectedly.
    let _availability_guard = DispatcherAvailabilityGuard {
        shared: Arc::clone(&shared),
    };
    // Idle capabilities are drained continuously, not only while a job is
    // held. A pool with no traffic would otherwise fill the idle channel with
    // stale capabilities and block the next generation inside its publish
    // before it can arm its own age deadline, which stalled max_worker_age.
    let mut idle = VecDeque::with_capacity(shared.config.workers);
    let mut idle_closed = false;
    let mut draining = false;
    loop {
        let job = loop {
            if draining {
                tokio::select! {
                    biased;
                    job = jobs_rx.recv() => break job,
                    published = idle_rx.recv(), if !idle_closed => match published {
                        Some(idle_worker) => {
                            stash_idle(&mut idle, idle_worker, shared.config.workers);
                        }
                        None => idle_closed = true,
                    },
                }
            } else {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        shared.accepting.store(false, Ordering::Release);
                        jobs_rx.close();
                        draining = true;
                    }
                    job = jobs_rx.recv() => break job,
                    published = idle_rx.recv(), if !idle_closed => match published {
                        Some(idle_worker) => {
                            stash_idle(&mut idle, idle_worker, shared.config.workers);
                        }
                        None => idle_closed = true,
                    },
                }
            }
        };
        let Some(mut job) = job else {
            break;
        };
        if job.response_tx.is_closed() {
            continue;
        }

        loop {
            let idle_worker = match take_idle(&mut idle) {
                Some(idle_worker) => idle_worker,
                None => {
                    let published = if idle_closed {
                        None
                    } else {
                        idle_rx.recv().await
                    };
                    match published {
                        Some(idle_worker) => idle_worker,
                        None => {
                            shared.accepting.store(false, Ordering::Release);
                            jobs_rx.close();
                            let _ = job.response_tx.send(Err(PoolJobError::Unavailable));
                            while let Some(queued) = jobs_rx.recv().await {
                                let _ = queued.response_tx.send(Err(PoolJobError::Unavailable));
                            }
                            return;
                        }
                    }
                }
            };
            match idle_worker.mailbox.try_send(WorkerCommand::Job(job)) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Closed(WorkerCommand::Job(returned))) => {
                    job = returned;
                }
                Err(mpsc::error::TrySendError::Full(WorkerCommand::Job(returned))) => {
                    eprintln!(
                        "PHP worker slot {} generation {} published an invalid full idle mailbox",
                        idle_worker.slot, idle_worker.generation
                    );
                    let _ = returned.response_tx.send(Err(PoolJobError::Unavailable));
                    break;
                }
            }
        }
    }

    shared.accepting.store(false, Ordering::Release);
}

async fn fail_worker(inner: &mut WorkerSlot, reason: &str) {
    let slot = std::mem::replace(inner, WorkerSlot::Unavailable(reason.to_owned()));
    if let WorkerSlot::Running(mut worker) = slot {
        terminate_child(&mut worker.child).await;
        let _ = worker.stderr_task.await;
    }
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn startup_io_context(child: &mut Child, error: ProtocolError) -> String {
    match child.try_wait() {
        Ok(Some(status)) => format!("process exited before READY with status {status}: {error}"),
        Ok(None) => error.to_string(),
        Err(wait_error) => format!("{error}; process status unavailable: {wait_error}"),
    }
}

fn spawn_stderr_drain(mut stderr: tokio::process::ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(length) => eprintln!(
                    "PHP worker stderr: {}",
                    String::from_utf8_lossy(&buffer[..length]).trim_end()
                ),
                Err(error) => {
                    eprintln!("failed to drain PHP worker stderr: {error}");
                    break;
                }
            }
        }
    })
}

enum HttpRequestError {
    TooLarge,
    TooManyHeaders,
    Body(hyper::Error),
}

async fn http_request(request: Request<Incoming>) -> Result<WorkerRequest, HttpRequestError> {
    let (parts, mut body) = request.into_parts();
    if parts.headers.len() > MAX_HEADERS {
        return Err(HttpRequestError::TooManyHeaders);
    }
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(HttpRequestError::Body)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(data.len()) > MAX_REQUEST_BODY {
            return Err(HttpRequestError::TooLarge);
        }
        bytes.extend_from_slice(&data);
    }
    let uri = parts.uri.to_string();
    let path = parts.uri.path();
    let query = parts.uri.query().unwrap_or_default();
    let headers = parts
        .headers
        .iter()
        .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect();
    Ok(WorkerRequest {
        method: parts.method.as_str().as_bytes().to_vec(),
        uri: uri.into_bytes(),
        path: path.as_bytes().to_vec(),
        query: query.as_bytes().to_vec(),
        headers,
        body: bytes.to_vec(),
    })
}

fn http_response(worker: WorkerResponse) -> Result<HandlerResponse, String> {
    if !(200..=599).contains(&worker.status) {
        return Err(format!("invalid final status {}", worker.status));
    }
    let status = StatusCode::from_u16(worker.status).map_err(|error| error.to_string())?;
    let mut response = Response::new(Full::new(Bytes::from(worker.body)));
    *response.status_mut() = status;
    for (raw_name, raw_value) in worker.headers {
        let name = HeaderName::from_bytes(&raw_name).map_err(|error| error.to_string())?;
        if reserved_response_header(&name) {
            return Err(format!("worker may not set response header {name}"));
        }
        let value = HeaderValue::from_bytes(&raw_value).map_err(|error| error.to_string())?;
        response.headers_mut().append(name, value);
    }
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static(SERVER_NAME));
    Ok(response)
}

fn reserved_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "server"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn fixture(script: &str) -> PhpWorkerConfig {
        let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
        assert!(
            executable.is_file(),
            "PHP-dependent test requires repository php/8.5/php.exe"
        );
        PhpWorkerConfig {
            executable,
            script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/php-worker")
                .join(script),
        }
    }

    fn pool(script: &str, workers: usize, queue_capacity: usize) -> Arc<PhpWorkerPool> {
        Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
            worker: fixture(script),
            workers,
            queue_capacity,
            max_requests: None,
            max_worker_age: None,
        }))
    }

    fn request(method: &str, path: &str, query: &str, body: Vec<u8>) -> WorkerRequest {
        let uri = if query.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{query}")
        };
        WorkerRequest {
            method: method.as_bytes().to_vec(),
            uri: uri.into_bytes(),
            path: path.as_bytes().to_vec(),
            query: query.as_bytes().to_vec(),
            headers: vec![
                (b"x-repeat".to_vec(), b"one".to_vec()),
                (b"x-repeat".to_vec(), b"two".to_vec()),
            ],
            body,
        }
    }

    #[tokio::test]
    async fn dispatcher_abort_releases_pending_and_future_callers() {
        let pool = pool("worker.php", 1, 8);
        let (jobs_tx, jobs_rx) = mpsc::channel(8);
        let (_idle_tx, idle_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        pool.shared.jobs_tx.set(jobs_tx.clone()).unwrap();
        pool.shared.accepting.store(true, Ordering::Release);

        let (response_tx, response_rx) = oneshot::channel();
        jobs_tx
            .send(PhpJob {
                request: request("GET", "/hello", "", Vec::new()),
                response_tx,
            })
            .await
            .unwrap();

        let dispatcher = tokio::spawn(dispatcher_task(
            jobs_rx,
            idle_rx,
            shutdown_rx,
            Arc::clone(&pool.shared),
        ));
        timeout(Duration::from_secs(1), async {
            while jobs_tx.capacity() != 8 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dispatcher should take ownership of the pending job");

        dispatcher.abort();
        assert!(dispatcher.await.unwrap_err().is_cancelled());
        let result = timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("pending caller must not hang");
        assert!(result.is_err());
        assert!(!pool.shared.accepting.load(Ordering::Acquire));
        assert!(matches!(
            pool.execute(request("GET", "/hello", "", Vec::new())).await,
            Err(PoolExecuteError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn stale_generation_channel_cannot_receive_a_job() {
        let pool = Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
            worker: PhpWorkerConfig {
                executable: PathBuf::from("unused-php"),
                script: PathBuf::from("unused-worker.php"),
            },
            workers: 1,
            queue_capacity: 8,
            max_requests: None,
            max_worker_age: None,
        }));
        let (jobs_tx, jobs_rx) = mpsc::channel(1);
        let (idle_tx, idle_rx) = mpsc::channel(2);
        let (worker_tx, worker_rx) = mpsc::channel(1);
        drop(worker_rx);
        idle_tx
            .send(IdleWorker {
                slot: 0,
                generation: 1,
                mailbox: worker_tx,
            })
            .await
            .unwrap();
        drop(idle_tx);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        jobs_tx
            .send(PhpJob {
                request: request("GET", "/hello", "", Vec::new()),
                response_tx,
            })
            .await
            .unwrap();
        drop(jobs_tx);

        dispatcher_task(jobs_rx, idle_rx, shutdown_rx, Arc::clone(&pool.shared)).await;
        assert!(matches!(
            response_rx.await.unwrap(),
            Err(PoolJobError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn dispatcher_drains_idle_capabilities_without_a_pending_job() {
        let pool = Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
            worker: PhpWorkerConfig {
                executable: PathBuf::from("unused-php"),
                script: PathBuf::from("unused-worker.php"),
            },
            workers: 1,
            queue_capacity: 8,
            max_requests: None,
            max_worker_age: None,
        }));
        let (jobs_tx, jobs_rx) = mpsc::channel(8);
        // The production capacity for a single-slot pool.
        let (idle_tx, idle_rx) = mpsc::channel(2);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = tokio::spawn(dispatcher_task(
            jobs_rx,
            idle_rx,
            shutdown_rx,
            Arc::clone(&pool.shared),
        ));

        // Recycled generations publish and stop while no job is pending. The
        // third publish blocked forever before the dispatcher drained idle
        // capabilities continuously, which stalled max_worker_age recycling.
        for generation in 1..=5 {
            let (worker_tx, worker_rx) = mpsc::channel(1);
            drop(worker_rx);
            timeout(
                Duration::from_secs(5),
                idle_tx.send(IdleWorker {
                    slot: 0,
                    generation,
                    mailbox: worker_tx,
                }),
            )
            .await
            .expect("publishing an idle capability must not block on an idle pool")
            .expect("dispatcher must still own the idle receiver");
        }

        let (worker_tx, mut worker_rx) = mpsc::channel(1);
        idle_tx
            .send(IdleWorker {
                slot: 0,
                generation: 6,
                mailbox: worker_tx,
            })
            .await
            .unwrap();
        let (response_tx, _response_rx) = oneshot::channel();
        jobs_tx
            .send(PhpJob {
                request: request("GET", "/hello", "", Vec::new()),
                response_tx,
            })
            .await
            .unwrap();

        let command = timeout(Duration::from_secs(5), worker_rx.recv())
            .await
            .expect("the live generation must receive the job");
        assert!(matches!(command, Some(WorkerCommand::Job(_))));

        drop(jobs_tx);
        drop(idle_tx);
        timeout(Duration::from_secs(5), dispatcher)
            .await
            .expect("dispatcher must converge")
            .unwrap();
    }

    #[test]
    fn repeated_crashes_apply_backoff() {
        let expected = [0, 100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000, 5_000];
        for (failures, milliseconds) in (1..=MAX_CONSECUTIVE_FAILURES).zip(expected) {
            assert_eq!(backoff_delay(failures), Duration::from_millis(milliseconds));
        }
    }

    #[test]
    fn no_respawn_storm() {
        assert_eq!(MAX_CONSECUTIVE_FAILURES, 10);
        assert_eq!(backoff_delay(1), Duration::ZERO);
        assert!(
            (2..=MAX_CONSECUTIVE_FAILURES)
                .map(backoff_delay)
                .all(|delay| delay >= Duration::from_millis(100))
        );
        assert_eq!(backoff_delay(usize::MAX), Duration::from_secs(5));
        assert_eq!(
            replacement_delay(MAX_CONSECUTIVE_FAILURES),
            Some(Duration::from_secs(5))
        );
        assert_eq!(replacement_delay(MAX_CONSECUTIVE_FAILURES + 1), None);
    }

    #[test]
    fn rejects_invalid_status_and_headers_from_worker() {
        let invalid_status = WorkerResponse {
            request_id: 1,
            status: 99,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(http_response(invalid_status).is_err());
        let invalid_header = WorkerResponse {
            request_id: 1,
            status: 200,
            headers: vec![(b"bad header".to_vec(), b"value".to_vec())],
            body: Vec::new(),
        };
        assert!(http_response(invalid_header).is_err());
        let reserved = WorkerResponse {
            request_id: 1,
            status: 200,
            headers: vec![(b"content-length".to_vec(), b"7".to_vec())],
            body: Vec::new(),
        };
        assert!(http_response(reserved).is_err());
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn persistent_worker_preserves_request_data_and_binary_payloads() {
        let mut worker = PhpWorker::new(fixture("worker.php"));
        let started = worker.start().await.unwrap();
        assert_eq!(worker.pid(), Some(started.pid));

        let empty = worker
            .execute(request("GET", "/state", "", Vec::new()))
            .await
            .unwrap();
        let state = String::from_utf8(empty.body).unwrap();
        assert!(state.contains(&format!("\"pid\":{}", started.pid)));
        assert!(state.contains("\"bootstrap\":1"));
        assert!(state.contains("\"request\":1"));

        let inspected = worker
            .execute(request(
                "POST",
                "/inspect",
                "name=hamster&mode=worker",
                b"request body".to_vec(),
            ))
            .await
            .unwrap();
        let inspected = String::from_utf8(inspected.body).unwrap();
        assert!(inspected.contains("\"method\":\"POST\""));
        assert!(inspected.contains("\"uri\":\"/inspect?name=hamster&mode=worker\""));
        assert!(inspected.contains("\"query\":\"name=hamster&mode=worker\""));
        assert!(inspected.contains("[\"x-repeat\",\"one\"]"));
        assert!(inspected.contains("[\"x-repeat\",\"two\"]"));
        assert!(inspected.contains("\"body\":\"request body\""));

        let binary = vec![0, 10, 13, 128, 255];
        let echoed = worker
            .execute(request("POST", "/echo", "", binary.clone()))
            .await
            .unwrap();
        assert_eq!(echoed.status, 201);
        assert_eq!(echoed.body, binary);
        assert_eq!(
            echoed
                .headers
                .iter()
                .filter(|(name, _)| name == b"x-repeat")
                .count(),
            2
        );

        let large = vec![b'x'; 256 * 1024];
        let echoed = worker
            .execute(request("POST", "/large", "", large.clone()))
            .await
            .unwrap();
        assert_eq!(echoed.body, large);
        worker.shutdown().await.unwrap();
        assert_eq!(worker.state(), WorkerState::Stopped);
        assert_eq!(worker.pid(), None);
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn one_worker_pool_serializes_without_protocol_corruption() {
        let pool = pool("worker.php", 1, 32);
        pool.start().await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let pool = Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                pool.execute(request("GET", "/hello", "", Vec::new())).await
            }));
        }
        for task in tasks {
            let response = task.await.unwrap().unwrap();
            assert_eq!(response.body, b"Hello from PHP");
        }
        let state = pool
            .execute(request("GET", "/state", "", Vec::new()))
            .await
            .unwrap();
        assert!(
            String::from_utf8(state.body)
                .unwrap()
                .contains("\"request\":17")
        );
        pool.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn startup_failures_are_detected_before_ready() {
        for script in ["exit_before_ready.php", "malformed_ready.php"] {
            let mut worker = PhpWorker::with_timeouts(
                fixture(script),
                WorkerTimeouts {
                    startup: Duration::from_millis(500),
                    ..WorkerTimeouts::default()
                },
            );
            assert!(matches!(
                worker.start().await,
                Err(PhpWorkerError::Startup(_))
            ));
            assert_eq!(worker.state(), WorkerState::Unavailable);
        }

        let mut worker = PhpWorker::with_timeouts(
            fixture("slow_ready.php"),
            WorkerTimeouts {
                startup: Duration::from_millis(50),
                ..WorkerTimeouts::default()
            },
        );
        let error = worker.start().await.unwrap_err();
        assert!(error.to_string().contains("READY timeout"));
        assert_eq!(worker.state(), WorkerState::Unavailable);
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn partial_pool_startup_failure_stops_workers_that_reached_ready() {
        let pool = PhpWorkerPool::with_worker_configs(
            vec![fixture("worker.php"), fixture("malformed_ready.php")],
            8,
        );
        let error = pool.start().await.unwrap_err();
        assert!(error.to_string().contains("worker"));
        assert!(pool.pids().is_empty());
        pool.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn malformed_truncated_timeout_and_exit_fail_closed() {
        for path in ["/malformed", "/truncated"] {
            let mut worker = PhpWorker::new(fixture("worker.php"));
            worker.start().await.unwrap();
            assert!(
                worker
                    .execute(request("GET", path, "", Vec::new()))
                    .await
                    .is_err()
            );
            assert_eq!(worker.state(), WorkerState::Unavailable);
            assert!(matches!(
                worker
                    .execute(request("GET", "/hello", "", Vec::new()))
                    .await,
                Err(PhpWorkerError::Unavailable(_))
            ));
            worker.shutdown().await.unwrap();
        }

        let mut worker = PhpWorker::with_timeouts(
            fixture("worker.php"),
            WorkerTimeouts {
                request: Duration::from_millis(50),
                ..WorkerTimeouts::default()
            },
        );
        worker.start().await.unwrap();
        let error = worker
            .execute(request("GET", "/delay", "ms=500", Vec::new()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timeout"));
        assert_eq!(worker.state(), WorkerState::Unavailable);
        worker.shutdown().await.unwrap();

        let mut worker = PhpWorker::with_timeouts(
            fixture("exit_mid_request.php"),
            WorkerTimeouts {
                request: Duration::from_secs(1),
                ..WorkerTimeouts::default()
            },
        );
        worker.start().await.unwrap();
        assert!(
            worker
                .execute(request("POST", "/large", "", vec![b'x'; MAX_REQUEST_BODY]))
                .await
                .is_err()
        );
        assert_eq!(worker.state(), WorkerState::Unavailable);
        worker.shutdown().await.unwrap();

        let mut worker = PhpWorker::new(fixture("worker.php"));
        worker.start().await.unwrap();
        let response = worker
            .execute(request("GET", "/exit-after-response", "", Vec::new()))
            .await
            .unwrap();
        assert_eq!(response.body, b"exiting");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            worker
                .execute(request("GET", "/hello", "", Vec::new()))
                .await
                .is_err()
        );
        assert_eq!(worker.state(), WorkerState::Unavailable);
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn shutdown_cleans_up_ready_and_used_workers() {
        for send_request in [false, true] {
            let mut worker = PhpWorker::new(fixture("worker.php"));
            worker.start().await.unwrap();
            if send_request {
                worker
                    .execute(request("GET", "/hello", "", Vec::new()))
                    .await
                    .unwrap();
            }
            worker.shutdown().await.unwrap();
            assert_eq!(worker.state(), WorkerState::Stopped);
            assert_eq!(worker.pid(), None);
        }

        let mut worker = PhpWorker::new(fixture("worker.php"));
        worker.start().await.unwrap();
        worker
            .execute(request("GET", "/exit-after-response", "", Vec::new()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.shutdown().await.unwrap();
        assert_eq!(worker.state(), WorkerState::Stopped);
    }

    #[tokio::test]
    #[ignore = "requires repository php/8.5/php.exe"]
    async fn shutdown_waits_for_busy_request_then_stops_worker() {
        let pool = pool("worker.php", 1, 8);
        pool.start().await.unwrap();

        let request_pool = Arc::clone(&pool);
        let request_task = tokio::spawn(async move {
            request_pool
                .execute(request("GET", "/delay", "ms=200", Vec::new()))
                .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;

        let shutdown_started = Instant::now();
        pool.shutdown().await.unwrap();
        let response = request_task.await.unwrap().unwrap();
        assert_eq!(response.body, b"delayed");
        assert!(shutdown_started.elapsed() >= Duration::from_millis(100));
        assert!(pool.pids().is_empty());
    }

    #[tokio::test]
    #[ignore = "100,000-request PHP correctness soak; requires repository php/8.5/php.exe"]
    async fn one_hundred_thousand_request_pool_soak_preserves_workers_and_state() {
        const REQUESTS: usize = 100_000;
        const CONCURRENCY: usize = 32;
        let pool = pool("worker.php", 4, 128);
        let starts = pool.start().await.unwrap();
        let expected_pids = starts
            .iter()
            .map(|start| start.pid)
            .collect::<std::collections::HashSet<_>>();

        let mut initial = JoinSet::new();
        for _ in 0..4 {
            let pool = Arc::clone(&pool);
            initial.spawn(async move {
                pool.execute(request("GET", "/state-delay", "ms=25", Vec::new()))
                    .await
                    .unwrap()
            });
        }
        let mut initial_pids = std::collections::HashSet::new();
        while let Some(response) = initial.join_next().await {
            let body = String::from_utf8(response.unwrap().body).unwrap();
            initial_pids.insert(json_field(&body, "pid") as u32);
            assert_eq!(json_field(&body, "bootstrap"), 1);
            assert_eq!(json_field(&body, "request"), 1);
        }
        assert_eq!(initial_pids, expected_pids);

        let mut clients = JoinSet::new();
        for _ in 0..CONCURRENCY {
            let pool = Arc::clone(&pool);
            let count = REQUESTS / CONCURRENCY;
            clients.spawn(async move {
                for _ in 0..count {
                    let response = pool
                        .execute(request("GET", "/hello", "", Vec::new()))
                        .await
                        .unwrap();
                    assert_eq!(response.body, b"Hello from PHP");
                }
            });
        }
        while let Some(client) = clients.join_next().await {
            client.unwrap();
        }

        let mut final_states = JoinSet::new();
        for _ in 0..4 {
            let pool = Arc::clone(&pool);
            final_states.spawn(async move {
                pool.execute(request("GET", "/state-delay", "ms=25", Vec::new()))
                    .await
                    .unwrap()
            });
        }
        let mut final_pids = std::collections::HashSet::new();
        let mut final_request_sum = 0_u64;
        while let Some(response) = final_states.join_next().await {
            let body = String::from_utf8(response.unwrap().body).unwrap();
            final_pids.insert(json_field(&body, "pid") as u32);
            assert_eq!(json_field(&body, "bootstrap"), 1);
            final_request_sum += json_field(&body, "request");
        }
        assert_eq!(final_pids, expected_pids);
        assert_eq!(final_request_sum, (REQUESTS + 8) as u64);
        pool.shutdown().await.unwrap();
        assert!(pool.pids().is_empty());
    }

    fn json_field(body: &str, field: &str) -> u64 {
        let marker = format!("\"{field}\":");
        let start = body.find(&marker).unwrap() + marker.len();
        let end = body[start..]
            .find(|character: char| !character.is_ascii_digit())
            .map_or(body.len(), |offset| start + offset);
        body[start..end].parse().unwrap()
    }
}
