use std::{
    fmt, io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
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
    sync::{Mutex, mpsc, oneshot},
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
        let mut child = command.spawn().map_err(|source| PhpWorkerError::Spawn {
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

        let handshake = timeout(self.timeouts.startup, read_frame(&mut stdout)).await;
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
    pids: RwLock<Vec<Option<u32>>>,
}

enum PoolLifecycle {
    New,
    Running(PoolRuntime),
    Stopped,
}

struct PoolRuntime {
    shutdown_tx: Option<oneshot::Sender<()>>,
    dispatcher: JoinHandle<()>,
    workers: JoinSet<Result<(), PhpWorkerError>>,
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
    Shutdown,
}

impl PhpWorkerPool {
    pub fn new(config: PhpWorkerPoolConfig) -> Self {
        let workers = config.workers;
        let worker_configs = vec![config.worker.clone(); workers];
        Self {
            shared: Arc::new(PoolShared {
                config,
                worker_configs,
                jobs_tx: OnceLock::new(),
                accepting: AtomicBool::new(false),
                lifecycle: Mutex::new(PoolLifecycle::New),
                pids: RwLock::new(vec![None; workers]),
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
        };
        Self {
            shared: Arc::new(PoolShared {
                config,
                worker_configs,
                jobs_tx: OnceLock::new(),
                accepting: AtomicBool::new(false),
                lifecycle: Mutex::new(PoolLifecycle::New),
                pids: RwLock::new(vec![None; workers]),
            }),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.shared.config.workers
    }

    pub fn queue_capacity(&self) -> usize {
        self.shared.config.queue_capacity
    }

    pub fn pids(&self) -> Vec<u32> {
        self.shared
            .pids
            .read()
            .expect("PHP worker PID lock poisoned")
            .iter()
            .flatten()
            .copied()
            .collect()
    }

    pub async fn start(&self) -> Result<Vec<WorkerStart>, PhpWorkerError> {
        let mut lifecycle = self.shared.lifecycle.lock().await;
        if !matches!(*lifecycle, PoolLifecycle::New) {
            return Err(PhpWorkerError::Startup(
                "worker pool start was requested more than once".to_owned(),
            ));
        }

        let mut startup = JoinSet::new();
        for (index, config) in self.shared.worker_configs.iter().cloned().enumerate() {
            startup.spawn(async move {
                let mut worker = PhpWorker::new(config);
                let result = worker.start().await;
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
            *lifecycle = PoolLifecycle::Stopped;
            return Err(error);
        }

        let (jobs_tx, jobs_rx) = mpsc::channel(self.shared.config.queue_capacity);
        let (idle_tx, idle_rx) = mpsc::channel(self.shared.config.workers);
        let mut worker_txs = Vec::with_capacity(self.shared.config.workers);
        let mut worker_tasks = JoinSet::new();
        let mut reports = Vec::with_capacity(self.shared.config.workers);

        for (index, entry) in workers.into_iter().enumerate() {
            let (worker, report) = entry.expect("all PHP workers reached READY");
            reports.push(report);
            self.shared
                .pids
                .write()
                .expect("PHP worker PID lock poisoned")[index] = Some(report.pid);
            let (worker_tx, worker_rx) = mpsc::channel(1);
            worker_txs.push(worker_tx);
            worker_tasks.spawn(worker_task(
                index,
                worker,
                worker_rx,
                idle_tx.clone(),
                Arc::clone(&self.shared),
            ));
        }
        // Only worker tasks may own idle senders. Channel closure is the
        // correctness signal that no healthy worker remains.
        drop(idle_tx);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let shared = Arc::clone(&self.shared);
        let dispatcher = tokio::spawn(dispatcher_task(
            jobs_rx,
            idle_rx,
            worker_txs,
            shutdown_rx,
            shared,
        ));
        self.shared.jobs_tx.set(jobs_tx).map_err(|_| {
            PhpWorkerError::Startup("worker pool job channel was initialized twice".to_owned())
        })?;
        self.shared.accepting.store(true, Ordering::Release);
        *lifecycle = PoolLifecycle::Running(PoolRuntime {
            shutdown_tx: Some(shutdown_tx),
            dispatcher,
            workers: worker_tasks,
        });
        Ok(reports)
    }

    pub async fn shutdown(&self) -> Result<(), PhpWorkerError> {
        self.shared.accepting.store(false, Ordering::Release);
        let runtime = {
            let mut lifecycle = self.shared.lifecycle.lock().await;
            match std::mem::replace(&mut *lifecycle, PoolLifecycle::Stopped) {
                PoolLifecycle::Running(runtime) => Some(runtime),
                PoolLifecycle::New | PoolLifecycle::Stopped => None,
            }
        };
        let Some(mut runtime) = runtime else {
            return Ok(());
        };

        if let Some(shutdown_tx) = runtime.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        let mut first_error = None;
        while let Some(joined) = runtime.workers.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        PhpWorkerError::Shutdown(format!("worker task failed: {error}"))
                    });
                }
            }
        }
        if let Err(error) = runtime.dispatcher.await {
            first_error.get_or_insert_with(|| {
                PhpWorkerError::Shutdown(format!("dispatcher task failed: {error}"))
            });
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
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

async fn worker_task(
    index: usize,
    mut worker: PhpWorker,
    mut worker_rx: mpsc::Receiver<WorkerCommand>,
    idle_tx: mpsc::Sender<usize>,
    shared: Arc<PoolShared>,
) -> Result<(), PhpWorkerError> {
    let _pid_guard = WorkerPidGuard {
        shared: Arc::clone(&shared),
        index,
    };
    if idle_tx.send(index).await.is_err() {
        return worker.shutdown().await;
    }

    while let Some(command) = worker_rx.recv().await {
        match command {
            WorkerCommand::Job(job) => {
                if job.response_tx.is_closed() {
                    if idle_tx.send(index).await.is_err() {
                        break;
                    }
                    continue;
                }
                match worker.execute(job.request).await {
                    Ok(response) => {
                        let _ = job.response_tx.send(Ok(response));
                        if idle_tx.send(index).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = job.response_tx.send(Err(PoolJobError::Worker(message)));
                        eprintln!("PHP worker {index} became unavailable: {error}");
                        return Ok(());
                    }
                }
            }
            WorkerCommand::Shutdown => {
                return worker.shutdown().await;
            }
        }
    }

    worker.shutdown().await
}

struct WorkerPidGuard {
    shared: Arc<PoolShared>,
    index: usize,
}

impl Drop for WorkerPidGuard {
    fn drop(&mut self) {
        if let Ok(mut pids) = self.shared.pids.write()
            && let Some(pid) = pids.get_mut(self.index)
        {
            *pid = None;
        }
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

async fn dispatcher_task(
    mut jobs_rx: mpsc::Receiver<PhpJob>,
    mut idle_rx: mpsc::Receiver<usize>,
    worker_txs: Vec<mpsc::Sender<WorkerCommand>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    shared: Arc<PoolShared>,
) {
    // Covers normal return, panic, and task abort. Dropping jobs_rx and any
    // locally held job also closes their response oneshots, so callers cannot
    // remain parked if this single dispatcher task exits unexpectedly.
    let _availability_guard = DispatcherAvailabilityGuard {
        shared: Arc::clone(&shared),
    };
    let mut draining = false;
    loop {
        let job = if draining {
            jobs_rx.recv().await
        } else {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    shared.accepting.store(false, Ordering::Release);
                    jobs_rx.close();
                    draining = true;
                    jobs_rx.recv().await
                }
                job = jobs_rx.recv() => job,
            }
        };
        let Some(mut job) = job else {
            break;
        };
        if job.response_tx.is_closed() {
            continue;
        }

        loop {
            let Some(worker_id) = idle_rx.recv().await else {
                shared.accepting.store(false, Ordering::Release);
                jobs_rx.close();
                let _ = job.response_tx.send(Err(PoolJobError::Unavailable));
                while let Some(queued) = jobs_rx.recv().await {
                    let _ = queued.response_tx.send(Err(PoolJobError::Unavailable));
                }
                return;
            };
            match worker_txs[worker_id].try_send(WorkerCommand::Job(job)) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Closed(WorkerCommand::Job(returned))) => {
                    job = returned;
                }
                Err(mpsc::error::TrySendError::Full(WorkerCommand::Job(returned))) => {
                    debug_assert!(false, "idle worker mailbox unexpectedly full");
                    let _ = returned.response_tx.send(Err(PoolJobError::Unavailable));
                    break;
                }
                Err(_) => unreachable!("dispatcher only sends job commands"),
            }
        }
    }

    shared.accepting.store(false, Ordering::Release);
    for worker_tx in worker_txs {
        let _ = worker_tx.send(WorkerCommand::Shutdown).await;
    }
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
            Vec::new(),
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
