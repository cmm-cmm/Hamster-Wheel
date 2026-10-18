use std::{
    collections::{BTreeMap, VecDeque},
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

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{
    Request, Response, StatusCode, Version,
    body::{Body, Incoming},
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
    body_budget::{BodyBudget, BodyReservation},
    config::SERVER_NAME,
    handler::{HandlerResponse, simple_response},
    php_protocol::{
        MAX_HEADERS, ProtocolError, WorkerHttpVersion, WorkerRequest, WorkerResponse, decode_ready,
        decode_response, decode_shutdown_ack, encode_request, encode_shutdown, read_frame,
        write_frame, write_frame_tracked,
    },
    response_body::ResponseBody,
    transport::ConnectionInfo,
};

pub const MAX_REQUEST_BODY: usize = 1024 * 1024;
pub const MAX_WORKERS: usize = 64;
pub const MAX_QUEUE_CAPACITY: usize = 128;

/// Default budget for a new generation to reach READY: spawn, application
/// bootstrap and the READY frame, for the initial workers and every
/// replacement. Warm bootstraps measured 60-400 ms, but the first start of an
/// application whose files the machine has not opened recently is dominated by
/// that first open: on the Windows development machine, with on-access
/// antivirus scanning enabled, reading 10,072 vendor files took 100.7 s the
/// first time and 0.47 s the second, and a Laravel stand-in's READY went from
/// 0.25 s warm to 3.4-3.8 s cold. `startup_timeout` lets a pool whose
/// application opens many files wait longer; see `doc/CONFIGURATION.md`.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Smallest configurable `startup_timeout`.
pub const MIN_STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest configurable `startup_timeout`. A supervisor waits this long for a
/// replacement before it counts the failure, so the ceiling also bounds how
/// long one stuck bootstrap can keep a slot out of service.
pub const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
/// Default ceiling on one application transaction, from the REQUEST frame
/// write through the RESPONSE read.
///
/// Kept at the historical five seconds so making the value configurable does
/// not silently change any existing deployment. No measurement establishes
/// five seconds as correct for a given application; it is a compatibility
/// default, and `doc/PHP_WORKER.md` says so.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Smallest configurable `request_timeout`.
///
/// This is not a taste threshold; it is derived from two measurements in
/// `benchmarks/results/worker_request_timeout_floor_20260917.md`.
///
/// The deadline covers the REQUEST frame write, PHP execution and the RESPONSE
/// frame read, so it must not be able to expire on Hamster's own protocol I/O
/// and be reported as an application timeout. The worst legal payload, a 1 MiB
/// body echoed back, round-tripped in 18.1 ms worst of twelve samples, so one
/// second leaves the application over 98% of the smallest allowed deadline.
///
/// Exceeding the deadline kills the worker, so the configured value is also
/// what bounds a slot's worst-case respawn rate at
/// `1 / (request_timeout + bootstrap)`. That model was measured and holds
/// exactly: 56.5 respawns per minute per slot at one second against 56.6
/// predicted. One second keeps a slot's bootstrap duty cycle under 30% even
/// against the worst real-application bootstrap this repository has measured
/// (403 ms), and that worst case only occurs when every request is already
/// timing out.
pub const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest configurable `request_timeout`. An application that legitimately
/// runs longer than an hour does not belong behind a synchronous HTTP request.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// `CREATE_NO_WINDOW` process creation flag; see the worker spawn.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long a generation must stay READY, with at least one successful
/// request, before its slot's failure history is forgiven.
///
/// Measured from the instant the generation reached READY, so bootstrap time
/// is excluded. Three times the default `request_timeout`: a generation that has served
/// successfully for longer than its own worst-case single-request budget,
/// three times over, has demonstrated more than "did not crash immediately".
///
/// This stays time based on purpose. Resetting on successful request *count*
/// would tie the respawn rate to application throughput, because a crash loop
/// reaches any request threshold sooner the faster the pool serves. A time
/// threshold bounds the worst-case respawn rate at `60 / 15 = 4` spawns per
/// minute per slot regardless of load.
const STABILITY_RESET_PERIOD: Duration = Duration::from_secs(15);
/// Failure count after which replacement leaves the fast backoff ladder and
/// enters the bounded cooldown band. Slots are never permanently retired.
const MAX_CONSECUTIVE_FAILURES: usize = 10;
/// First cooldown delay once the backoff ladder is exhausted.
const COOLDOWN_BASE: Duration = Duration::from_secs(30);
/// Ceiling for the cooldown band. A slot that can never start is retried at
/// this interval forever, which is the entire cost of never requiring an
/// operator restart after a transient host problem.
const COOLDOWN_MAX: Duration = Duration::from_secs(300);
const RECYCLE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// How long a pool must have no live worker before that is worth reporting.
///
/// A pool briefly drops to zero live workers during any ordinary replacement,
/// for exactly as long as the new child takes to reach READY. That is measured
/// at 38 ms for the test fixture and 58-403 ms for real Laravel applications
/// (`doc/BENCHMARKS.md`), so a threshold above the worst of those keeps routine
/// churn silent while still reporting anything a client could notice: the
/// backoff ladder holds a slot down for up to 5 s and the cooldown band for 30
/// to 300 s. One second is about 2.5 times the worst measured bootstrap.
const POOL_OUTAGE_NOTICE_AFTER: Duration = Duration::from_secs(1);
/// How many times a single job may be queued again after a worker proved it
/// never received the request. One covers the case this exists for -- a worker
/// that died while idle, detected by the next request -- while keeping a
/// crash-looping pool from bouncing one job between dead generations.
const MAX_NOT_STARTED_REDISPATCHES: u8 = 1;

#[derive(Clone, Default)]
pub struct PhpWorkerConfig {
    pub executable: PathBuf,
    pub script: PathBuf,
    /// Extra environment for the child, on top of the server's own environment.
    /// The pool never reads these; they exist so a worker script can be told
    /// where its application lives without the server knowing what it is.
    /// A `BTreeMap` keeps the spawn order deterministic.
    pub env: BTreeMap<String, String>,
}

/// Redacts environment values. Operators put database and cache credentials in
/// `php.env`, so a derived `Debug` would put them in whatever log or panic
/// message first formats a configuration. Names stay visible because they are
/// what makes a misconfiguration diagnosable.
impl fmt::Debug for PhpWorkerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PhpWorkerConfig")
            .field("executable", &self.executable)
            .field("script", &self.script)
            .field("env", &RedactedEnv(&self.env))
            .finish()
    }
}

struct RedactedEnv<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedEnv<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_map()
            .entries(self.0.keys().map(|name| (name, "<redacted>")))
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct PhpWorkerPoolConfig {
    pub worker: PhpWorkerConfig,
    /// How this pool names itself in diagnostics, normally its location path.
    ///
    /// Only one `php_worker` location is permitted today, so every line this
    /// appears on is currently unambiguous without it. It exists so that the
    /// diagnostics do not have to be revisited when more than one pool becomes
    /// possible, and so an operator greps one pool's lines by the same string
    /// they configured.
    pub label: Arc<str>,
    pub workers: usize,
    pub queue_capacity: usize,
    /// Ceiling on one application transaction for this pool. Exceeding it
    /// kills the worker, so the value also bounds the slot's worst-case
    /// respawn rate; `config` enforces the floor that keeps that bound useful.
    pub request_timeout: Duration,
    /// Budget for each generation of this pool to reach READY, initial and
    /// replacement alike.
    pub startup_timeout: Duration,
    pub max_requests: Option<u64>,
    pub max_worker_age: Option<Duration>,
    /// The process-wide request-body ceiling. Every PHP pool compiled from one
    /// configuration shares one budget, because the resource being bounded is
    /// process memory, not a pool.
    pub body_budget: Arc<BodyBudget>,
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
    /// Generations of this slot killed by the application execution timeout.
    ///
    /// Deliberately separate from `consecutive_failures`: it never decays and
    /// never schedules anything, so it stays a faithful count of how often the
    /// application ran past its deadline on this slot.
    pub execution_timeouts: u64,
    pub last_exit_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PoolSnapshot {
    pub target_workers: usize,
    /// Workers that exist and have reached READY, whether idle or serving.
    ///
    /// This is the pool's health signal and the one `execute` gates its 503
    /// on. Compare it against `target_workers`, never `ready_workers`.
    pub live_workers: usize,
    /// Workers that are READY *and idle*.
    ///
    /// `mark_busy` decrements this, so it is a measure of spare capacity, not
    /// of health: a completely healthy pool serving at full concurrency
    /// reports zero. Measured at 20% of samples on a four-worker pool that
    /// served 23,072 requests without a single failure. Do not build a
    /// degradation check on it.
    pub ready_workers: usize,
    pub total_respawns: u64,
    pub total_execution_timeouts: u64,
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

#[derive(Clone, Debug)]
pub struct WorkerStart {
    pub pid: u32,
    pub elapsed: Duration,
    /// The pool this worker belongs to, so a startup line names its pool.
    pub label: Arc<str>,
}

#[derive(Debug)]
pub enum PhpWorkerError {
    Spawn {
        executable: PathBuf,
        source: io::Error,
    },
    Startup(String),
    StartupCleanup {
        source: Box<PhpWorkerError>,
        cleanup: Vec<String>,
    },
    Unavailable(String),
    Request(String),
    /// The worker failed while it was provably still ignorant of the request:
    /// the process was already gone before a byte was written, or the frame
    /// write failed without transferring anything. The job may be dispatched
    /// again; every other failure may not.
    NotStarted(String),
    /// The application did not answer within its configured `request_timeout`.
    ///
    /// This is a *possibly-started* failure and is treated as such everywhere
    /// that matters: the worker is still killed and reaped, and the job is
    /// never dispatched again. It is separated from `Request` only so the
    /// lifecycle supervisor can tell an application that ran too long from a
    /// slot that cannot run, because those need opposite recovery policies.
    ExecutionTimeout(String),
    Shutdown(String),
}

/// A failed transaction plus the two facts the caller classifies it by.
///
/// `started` decides whether its job may be dispatched again and is the older,
/// stricter of the two. `timed_out` decides only which recovery policy the
/// slot receives; it never widens what may be retried, because a deadline that
/// expires says nothing about how far PHP got.
struct TransactionFailure {
    started: bool,
    timed_out: bool,
    message: String,
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
            Self::StartupCleanup { source, cleanup } => {
                write!(formatter, "{source}")?;
                for failure in cleanup {
                    write!(formatter, "; additional startup cleanup failure: {failure}")?;
                }
                Ok(())
            }
            Self::Unavailable(message) => write!(formatter, "PHP worker unavailable: {message}"),
            Self::Request(message) => write!(formatter, "PHP worker request failed: {message}"),
            Self::NotStarted(message) => write!(
                formatter,
                "PHP worker failed before the request started: {message}"
            ),
            Self::ExecutionTimeout(message) => write!(
                formatter,
                "PHP application exceeded its execution timeout: {message}"
            ),
            Self::Shutdown(message) => write!(formatter, "PHP worker shutdown failed: {message}"),
        }
    }
}

impl std::error::Error for PhpWorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::StartupCleanup { source, .. } => Some(source.as_ref()),
            Self::Startup(_)
            | Self::Unavailable(_)
            | Self::Request(_)
            | Self::NotStarted(_)
            | Self::ExecutionTimeout(_)
            | Self::Shutdown(_) => None,
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
            startup: DEFAULT_STARTUP_TIMEOUT,
            request: DEFAULT_REQUEST_TIMEOUT,
            shutdown: SHUTDOWN_TIMEOUT,
        }
    }
}

pub struct PhpWorker {
    config: PhpWorkerConfig,
    timeouts: WorkerTimeouts,
    /// The owning pool's label, carried so the child's own standard error can
    /// be attributed. Application output is the line that most needs a pool
    /// name once more than one application can be served.
    label: Arc<str>,
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

    fn with_label(config: PhpWorkerConfig, timeouts: WorkerTimeouts, label: Arc<str>) -> Self {
        Self {
            config,
            timeouts,
            label,
            inner: WorkerSlot::Starting,
        }
    }

    /// Builds a worker whose READY and transaction deadlines come from its
    /// pool's configuration. The shutdown deadline stays fixed: it bounds
    /// Hamster's own SHUTDOWN handshake, not the application.
    fn with_pool_timeouts(
        config: PhpWorkerConfig,
        startup: Duration,
        request: Duration,
        label: Arc<str>,
    ) -> Self {
        Self::with_label(
            config,
            WorkerTimeouts {
                startup,
                request,
                ..WorkerTimeouts::default()
            },
            label,
        )
    }

    fn with_timeouts(config: PhpWorkerConfig, timeouts: WorkerTimeouts) -> Self {
        Self::with_label(config, timeouts, Arc::from("php"))
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

        // `server::run` and `RuntimeConfig::start_workers` establish this at
        // their ownership boundaries. Keep the final spawn primitive fail
        // closed as well so a direct pool/worker caller cannot create an
        // uncontained PHP process. After the first call this is a OnceLock
        // read; replacement lifecycle logic remains unaware of Job Objects.
        #[cfg(windows)]
        crate::windows_child_containment::establish().map_err(|error| {
            PhpWorkerError::Startup(format!(
                "Windows PHP child containment could not be established: {error}"
            ))
        })?;

        let started = Instant::now();
        let mut command = Command::new(&self.config.executable);
        command
            .arg(&self.config.script)
            .envs(&self.config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A console delivers Ctrl+C and Ctrl+Break to every process attached
        // to it, and php.exe ends on either. Sharing Hamster's console would
        // let the key press that asks Hamster to stop (Ctrl+C) or to reload
        // certificates (Ctrl+Break) kill every worker mid-request, before the
        // drain and SHUTDOWN that are the pool's actual stop path. The worker
        // gets its own windowless console instead: its stdio are pipes and it
        // needs no console of Hamster's, and unlike DETACHED_PROCESS this
        // still gives console programs the application starts a console to
        // inherit rather than a new visible window each.
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
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
        let stderr_task = spawn_stderr_drain(stderr, Arc::clone(&self.label));

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
            label: Arc::clone(&self.label),
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
        request: &WorkerRequest,
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
            // The process was reaped before a single byte of this request was
            // written, so PHP demonstrably never saw it. This is the one class
            // of failure the never-retry invariant permits to be dispatched
            // again.
            let message = format!("process exited before request with status {status}");
            self.inner = WorkerSlot::Unavailable(message.clone());
            return Err(PhpWorkerError::NotStarted(message));
        }

        let transaction = {
            let worker = match &mut self.inner {
                WorkerSlot::Running(worker) => worker,
                _ => unreachable!("running worker was checked above"),
            };
            worker.state = WorkerState::Busy;
            let request_id = worker.next_request_id;
            worker.next_request_id = worker.next_request_id.wrapping_add(1).max(1);
            let payload = match encode_request(request_id, request) {
                Ok(payload) => payload,
                Err(error) => {
                    worker.state = WorkerState::Ready;
                    return Err(PhpWorkerError::Request(error.to_string()));
                }
            };
            let operation = async {
                // Everything past a frame write that moved bytes is
                // possibly-started: PHP may already be executing, so those
                // failures must never be dispatched again.
                if let Err(error) = write_frame_tracked(&mut worker.stdin, &payload).await {
                    return Err(TransactionFailure {
                        started: error.transferred > 0,
                        timed_out: false,
                        message: error.to_string(),
                    });
                }
                let read = async {
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
                read.await
                    .map_err(|error: ProtocolError| TransactionFailure {
                        started: true,
                        timed_out: false,
                        message: error.to_string(),
                    })
            };
            match timeout(self.timeouts.request, operation).await {
                Ok(result) => result,
                // A timeout says nothing about how far PHP got, so it is
                // always treated as started and is never dispatched again.
                // `timed_out` changes only which recovery policy the slot
                // receives, never what may be retried.
                Err(_) => Err(TransactionFailure {
                    started: true,
                    timed_out: true,
                    message: format!("timeout after {} ms", self.timeouts.request.as_millis()),
                }),
            }
        };

        match transaction {
            Ok(response) => {
                if let WorkerSlot::Running(worker) = &mut self.inner {
                    worker.state = WorkerState::Ready;
                }
                Ok(response)
            }
            Err(failure) => {
                // The kill is unconditional and stays that way. Protocol v1
                // has no cancel frame, so an abandoned transaction's worker
                // would write its RESPONSE into the next request's read. The
                // classification below decides recovery policy, not whether
                // the child is reaped.
                fail_worker(&mut self.inner, &failure.message).await;
                Err(match (failure.timed_out, failure.started) {
                    (true, _) => PhpWorkerError::ExecutionTimeout(failure.message),
                    (false, true) => PhpWorkerError::Request(failure.message),
                    (false, false) => PhpWorkerError::NotStarted(failure.message),
                })
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
    total_execution_timeouts: AtomicU64,
    recycle_owner: AtomicUsize,
    /// Milliseconds since `started` at which this pool last dropped to zero
    /// live workers, or 0 when it has at least one.
    ///
    /// A plain timestamp rather than a lock: the two writers are the
    /// `live_workers` transitions, and each is already serialized by the
    /// fetch that observed the crossing.
    outage_since_ms: AtomicU64,
    started: Instant,
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
    /// The body budget this job's bytes are charged against.
    ///
    /// It lives here, not in the HTTP handler, because the bytes do. A client
    /// that disconnects while its job is still queued drops the handler future
    /// but not the job, and releasing the reservation at that moment would let
    /// the counter fall behind the memory that is still held.
    _body: BodyReservation,
    response_tx: oneshot::Sender<Result<WorkerResponse, PoolJobError>>,
    /// How many times this job has already been queued again after a worker
    /// proved it never received the request. Bounds the retry so a pool that
    /// is crash-looping cannot bounce one job forever.
    redispatches: u8,
}

enum PoolJobError {
    Worker(String),
    ExecutionTimeout(String),
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
enum PoolExecuteError {
    QueueFull,
    Unavailable,
    Worker(String),
    ExecutionTimeout(String),
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
    /// The generation was killed because the application exceeded its
    /// execution timeout. Replacement is immediate and the slot's
    /// infrastructure failure history is untouched: a slot whose replacement
    /// reaches READY has not demonstrated that it cannot run.
    ExecutionTimeout(String),
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

async fn cleanup_startup_workers(workers: Vec<Option<(PhpWorker, WorkerStart)>>) -> Vec<String> {
    let mut cleanup = JoinSet::new();
    for (slot, entry) in workers.into_iter().enumerate() {
        if let Some((mut worker, _)) = entry {
            cleanup.spawn(async move { (slot, worker.shutdown().await) });
        }
    }
    let mut cleanup_errors = Vec::new();
    while let Some(result) = cleanup.join_next().await {
        match result {
            Ok((_, Ok(()))) => {}
            Ok((slot, Err(cleanup_error))) => {
                cleanup_errors.push(format!("worker {slot}: {cleanup_error}"));
            }
            Err(join_error) => {
                cleanup_errors.push(format!("worker cleanup task failed: {join_error}"));
            }
        }
    }
    cleanup_errors
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
                total_execution_timeouts: AtomicU64::new(0),
                recycle_owner: AtomicUsize::new(0),
                outage_since_ms: AtomicU64::new(0),
                started: Instant::now(),
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
            label: Arc::from("test"),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            max_requests: None,
            max_worker_age: None,
            body_budget: BodyBudget::disabled(),
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
                total_execution_timeouts: AtomicU64::new(0),
                recycle_owner: AtomicUsize::new(0),
                outage_since_ms: AtomicU64::new(0),
                started: Instant::now(),
            }),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.shared.config.workers
    }

    pub fn label(&self) -> &str {
        &self.shared.config.label
    }

    pub fn queue_capacity(&self) -> usize {
        self.shared.config.queue_capacity
    }

    pub fn request_timeout(&self) -> Duration {
        self.shared.config.request_timeout
    }

    pub fn startup_timeout(&self) -> Duration {
        self.shared.config.startup_timeout
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
            live_workers: self.shared.live_workers.load(Ordering::Acquire),
            ready_workers: self.shared.ready_workers.load(Ordering::Acquire),
            total_respawns: self.shared.total_respawns.load(Ordering::Acquire),
            total_execution_timeouts: self.shared.total_execution_timeouts.load(Ordering::Acquire),
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
                let mut worker = PhpWorker::with_pool_timeouts(
                    config,
                    shared.config.startup_timeout,
                    shared.config.request_timeout,
                    Arc::clone(&shared.config.label),
                );
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
            let cleanup_errors = cleanup_startup_workers(workers).await;
            *self.shared.lifecycle.lock().await = PoolLifecycle::Stopped;
            return if cleanup_errors.is_empty() {
                Err(error)
            } else {
                Err(PhpWorkerError::StartupCleanup {
                    source: Box::new(error),
                    cleanup: cleanup_errors,
                })
            };
        }

        if !self.shared.spawn_gate.is_serving() {
            let cleanup_errors = cleanup_startup_workers(workers).await;
            *self.shared.lifecycle.lock().await = PoolLifecycle::Stopped;
            let error =
                PhpWorkerError::Startup("worker pool shutdown started during startup".to_owned());
            return if cleanup_errors.is_empty() {
                Err(error)
            } else {
                Err(PhpWorkerError::StartupCleanup {
                    source: Box::new(error),
                    cleanup: cleanup_errors,
                })
            };
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

    pub(crate) async fn serve_http(
        &self,
        request: Request<Incoming>,
        connection: ConnectionInfo,
        body_read_timeout: Duration,
    ) -> HandlerResponse {
        let admitted = match http_request(
            request,
            connection,
            body_read_timeout,
            &self.shared.config.body_budget,
        )
        .await
        {
            Ok(admitted) => admitted,
            Err(error) => return body_admission_response(error),
        };

        match self.execute(admitted).await {
            Ok(response) => match http_response(response) {
                Ok(response) => response,
                Err(error) => {
                    errln!("invalid PHP worker response: {error}");
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
                errln!("PHP worker request failed: {error}");
                simple_response(StatusCode::BAD_GATEWAY, "PHP Worker Unavailable")
            }
            // The worker did not return within the configured deadline. A 504
            // describes that cause; it says nothing about whether PHP already
            // made side effects. The actor never replays this transaction.
            Err(PoolExecuteError::ExecutionTimeout(error)) => {
                errln!("PHP worker execution timeout: {error}");
                simple_response(StatusCode::GATEWAY_TIMEOUT, "PHP Worker Execution Timeout")
            }
        }
    }

    /// Reconstructs a body exactly as the PHP path does, then echoes it.
    ///
    /// `hold` lets a test keep the reservation alive after the response is
    /// written, which is how the aggregate ceiling is observed over real TCP
    /// without a PHP distribution.
    #[cfg(test)]
    pub(crate) async fn test_body_echo(
        request: Request<Incoming>,
        connection: ConnectionInfo,
        body_read_timeout: Duration,
        budget: &Arc<BodyBudget>,
        hold: Option<Duration>,
    ) -> HandlerResponse {
        match http_request(request, connection, body_read_timeout, budget).await {
            Ok(admitted) => {
                let response =
                    Response::new(ResponseBody::buffered(Bytes::from(admitted.request.body)));
                if let Some(hold) = hold {
                    tokio::time::sleep(hold).await;
                }
                drop(admitted.body);
                response
            }
            Err(error) => body_admission_response(error),
        }
    }

    async fn execute(&self, admitted: AdmittedRequest) -> Result<WorkerResponse, PoolExecuteError> {
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
            request: admitted.request,
            _body: admitted.body,
            response_tx,
            redispatches: 0,
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
            Ok(Err(PoolJobError::ExecutionTimeout(error))) => {
                Err(PoolExecuteError::ExecutionTimeout(error))
            }
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
        execution_timeouts: 0,
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
        let previous_live = self.live_workers.fetch_add(1, Ordering::AcqRel);
        self.ready_workers.fetch_add(1, Ordering::AcqRel);
        self.note_worker_live(previous_live);
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

    fn record_exit(self: &Arc<Self>, exit: &GenerationExit, reason: String) {
        let mut previous_live = 0;
        self.update_slot(exit.slot, |snapshot| {
            if snapshot.generation != exit.generation {
                return;
            }
            if snapshot.pid.take().is_some() {
                previous_live = self.live_workers.fetch_sub(1, Ordering::AcqRel);
            }
            if snapshot.state == WorkerLifecycleState::Ready {
                self.ready_workers.fetch_sub(1, Ordering::AcqRel);
            }
            snapshot.last_exit_reason = Some(reason);
            snapshot.state = WorkerLifecycleState::Unavailable;
        });
        self.note_worker_lost(previous_live);
    }

    /// Notes that a worker became live, reporting the end of a real outage.
    ///
    /// Called after `live_workers` has been incremented. Only the transition
    /// out of zero is interesting, and only when the pool was down longer than
    /// an ordinary replacement takes.
    fn note_worker_live(&self, previous_live: usize) {
        if previous_live != 0 || !self.spawn_gate.is_serving() {
            return;
        }
        let since_ms = self.outage_since_ms.swap(0, Ordering::AcqRel);
        if since_ms == 0 {
            return;
        }
        let outage = self.elapsed_ms().saturating_sub(since_ms);
        if outage < POOL_OUTAGE_NOTICE_AFTER.as_millis() as u64 {
            return;
        }
        errln!(
            "PHP worker pool {} recovered: {} of {} workers live after {outage} ms with none",
            self.config.label,
            self.live_workers.load(Ordering::Acquire),
            self.config.workers
        );
    }

    /// Notes that the pool may have lost its last worker.
    ///
    /// Called after `live_workers` has been decremented. It does not report
    /// immediately: every ordinary replacement passes through zero live
    /// workers for as long as a bootstrap takes, and announcing those produced
    /// far more "no live worker" lines than recoveries, which reads as a pile
    /// of unresolved outages. Instead it records the start and schedules one
    /// check, so a pool that is still down after `POOL_OUTAGE_NOTICE_AFTER`
    /// says so and a pool that simply replaced a worker stays silent. Both
    /// lines therefore use the same threshold and always appear in pairs.
    fn note_worker_lost(self: &Arc<Self>, previous_live: usize) {
        // Losing every worker is the point of a shutdown, not an outage.
        if previous_live != 1 || !self.spawn_gate.is_serving() {
            return;
        }
        let began = self.elapsed_ms().max(1);
        self.outage_since_ms.store(began, Ordering::Release);
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(POOL_OUTAGE_NOTICE_AFTER).await;
            // Still the same outage, and still ongoing. A recovery clears
            // `outage_since_ms`, and a later outage overwrites it, so an
            // unchanged value is what proves this check still applies.
            if shared.outage_since_ms.load(Ordering::Acquire) != began
                || shared.live_workers.load(Ordering::Acquire) != 0
                || !shared.spawn_gate.is_serving()
            {
                return;
            }
            errln!(
                "PHP worker pool {} has no live worker after {} ms; requests are refused until one is READY",
                shared.config.label,
                POOL_OUTAGE_NOTICE_AFTER.as_millis()
            );
        });
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Counts one execution timeout for a slot and for the pool.
    ///
    /// Deliberately does not touch `consecutive_failures`: the application ran
    /// too long, which is no evidence about whether this slot can start.
    fn record_execution_timeout(&self, slot: usize) -> u64 {
        self.total_execution_timeouts.fetch_add(1, Ordering::AcqRel);
        let mut timeouts = 0;
        self.update_slot(slot, |snapshot| {
            snapshot.execution_timeouts = snapshot.execution_timeouts.saturating_add(1);
            timeouts = snapshot.execution_timeouts;
        });
        timeouts
    }

    fn increment_failure(&self, slot: usize) -> usize {
        let mut failures = 0;
        self.update_slot(slot, |snapshot| {
            snapshot.consecutive_failures = snapshot.consecutive_failures.saturating_add(1);
            failures = snapshot.consecutive_failures;
        });
        failures
    }

    fn stop_slot(self: &Arc<Self>, slot: usize, generation: u64) {
        let mut previous_live = 0;
        self.update_slot(slot, |snapshot| {
            if snapshot.generation == generation {
                if snapshot.pid.take().is_some() {
                    previous_live = self.live_workers.fetch_sub(1, Ordering::AcqRel);
                }
                if snapshot.state == WorkerLifecycleState::Ready {
                    self.ready_workers.fetch_sub(1, Ordering::AcqRel);
                }
                snapshot.state = WorkerLifecycleState::Stopped;
            }
        });
        self.note_worker_lost(previous_live);
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
/// Owns one generation exclusively: its process, pipes, protocol state and
/// capacity-1 mailbox.
///
/// # Publishing a capability is a blocking send, and shutdown depends on it
///
/// `idle_tx.send(...).await` below blocks when the idle channel is full. That
/// is deliberate — it is what stops a pool with no traffic from filling the
/// idle channel with stale capabilities — but it creates an ordering
/// dependency that shutdown must respect:
///
/// * The dispatcher drains idle capabilities continuously, not only while it
///   holds a job. If it ever became lazy again, a worker could block here
///   forever and its generation task would never join, hanging shutdown.
/// * `supervisor_task` signals `worker_shutdown_tx` only *after* the
///   dispatcher has converged, and keeps joining generations while it waits.
///   Reversing that order would close the idle mailboxes while the dispatcher
///   still holds an accepted job, leaving it waiting for capacity that can
///   never be published.
///
/// Both halves are required. Changing either one — making the drain
/// conditional, or signalling workers before the dispatcher converges — can
/// deadlock lifecycle shutdown rather than fail visibly.
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
                match worker.execute(&job.request).await {
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
                        if let Some(job) = redispatch_not_started(&shared, &error, job) {
                            let _ = job.response_tx.send(Err(job_error(&error, message)));
                        }
                        shared.mark_unavailable(slot, generation);
                        errln!(
                            "PHP worker pool {} slot {slot} generation {generation} became unavailable: {error}",
                            shared.config.label
                        );
                        return GenerationExit {
                            slot,
                            generation,
                            idle_tx,
                            outcome: classify_generation_exit(&error),
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

/// Queues a job again when, and only when, the worker proved it never received
/// the request.
///
/// This is the second of the two provably-safe redispatch cases. The first is a
/// closed generation mailbox, which the dispatcher already handles: a closed
/// mailbox means the generation task is gone and never took the job. This one
/// means the process was already dead before any byte of the frame was written,
/// so PHP cannot have started executing. Every other failure, including a
/// timeout and any failure after a partial write, stays a 502.
///
/// Returns the job when it must be failed instead, and `None` when it has been
/// queued again or its client has already disconnected.
/// Maps a failed transaction onto the recovery policy its slot receives.
///
/// An execution timeout is the application's fault; every other failure is
/// evidence about the slot itself. Only the latter may move a slot along the
/// backoff ladder, because that ladder exists to stop a slot that cannot start
/// from spawning in a tight loop.
/// Maps a failed transaction onto what its client is told.
///
/// Mirrors `classify_generation_exit`: the same distinction that decides the
/// slot's recovery policy also decides which response the client receives, so
/// the two can never disagree about what happened.
fn job_error(error: &PhpWorkerError, message: String) -> PoolJobError {
    match error {
        PhpWorkerError::ExecutionTimeout(_) => PoolJobError::ExecutionTimeout(message),
        _ => PoolJobError::Worker(message),
    }
}

fn classify_generation_exit(error: &PhpWorkerError) -> GenerationOutcome {
    match error {
        PhpWorkerError::ExecutionTimeout(message) => {
            GenerationOutcome::ExecutionTimeout(message.clone())
        }
        other => GenerationOutcome::Failed(other.to_string()),
    }
}

fn redispatch_not_started(
    shared: &PoolShared,
    error: &PhpWorkerError,
    mut job: PhpJob,
) -> Option<PhpJob> {
    if !matches!(error, PhpWorkerError::NotStarted(_)) {
        return Some(job);
    }
    if job.redispatches >= MAX_NOT_STARTED_REDISPATCHES {
        return Some(job);
    }
    if job.response_tx.is_closed() {
        return None;
    }
    let Some(jobs_tx) = shared.jobs_tx.get() else {
        return Some(job);
    };
    job.redispatches += 1;
    match jobs_tx.try_send(job) {
        Ok(()) => None,
        Err(mpsc::error::TrySendError::Full(returned)) => Some(returned),
        Err(mpsc::error::TrySendError::Closed(returned)) => Some(returned),
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
        match worker.execute(&job.request).await {
            Ok(response) => {
                shared.record_success(slot, generation, spawned_at);
                let _ = job.response_tx.send(Ok(response));
            }
            Err(error) => {
                let message = error.to_string();
                // Admission is already closed by the time a drain runs, so the
                // queue will refuse the job and it is failed exactly as before.
                // The call is kept so there is one rule, not two.
                if let Some(job) = redispatch_not_started(shared, &error, job) {
                    let _ = job.response_tx.send(Err(job_error(&error, message)));
                }
                shared.mark_unavailable(slot, generation);
                return classify_generation_exit(&error);
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
    let mut worker = PhpWorker::with_pool_timeouts(
        spec.shared.worker_configs[spec.slot].clone(),
        spec.shared.config.startup_timeout,
        spec.shared.config.request_timeout,
        Arc::clone(&spec.shared.config.label),
    );
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

/// Delay for a slot whose failures have exhausted the fast backoff ladder.
///
/// Doubles from `COOLDOWN_BASE` up to `COOLDOWN_MAX` and stays there. The
/// shift is clamped before it is applied, so an unbounded failure count cannot
/// overflow the shift.
fn cooldown_delay(consecutive_failures: usize) -> Duration {
    let steps = consecutive_failures
        .saturating_sub(MAX_CONSECUTIVE_FAILURES + 1)
        .min(6) as u32;
    let millis = (COOLDOWN_BASE.as_millis() as u64) << steps;
    Duration::from_millis(millis.min(COOLDOWN_MAX.as_millis() as u64))
}

/// How long to wait before the next replacement generation for a slot.
///
/// Never returns "give up". A slot that keeps failing slows down to
/// `COOLDOWN_MAX` and keeps trying at that interval, so a temporary host
/// problem cannot turn into a permanent outage that needs an operator. The
/// delay depends only on the slot's own consecutive failure count, never on
/// HTTP throughput, concurrency, queue depth, or client behaviour, which is
/// what bounds the respawn rate independently of load.
fn replacement_delay(consecutive_failures: usize) -> Duration {
    if consecutive_failures <= MAX_CONSECUTIVE_FAILURES {
        backoff_delay(consecutive_failures)
    } else {
        cooldown_delay(consecutive_failures)
    }
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

/// Handles one generation that has ended while the pool is still serving.
///
/// # One live generation per slot
///
/// This is only ever reached from `generations.join_next()`, so the old
/// generation's task has already completed: its `PhpWorker`, child process,
/// pipes and mailbox receiver are dropped before `spawn_replacement` creates
/// the next generation for that slot. Two live generations for one slot
/// therefore never overlap.
///
/// That ordering is load-bearing, not incidental. It is what makes a closed
/// mailbox a reliable "this generation is gone" signal for `take_idle`, and it
/// is the first half of the stash bound proved in `stash_idle`. Spawning a
/// replacement before joining the old generation would break both, and would
/// also let a slot hold two PHP children at once.
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
        GenerationOutcome::ExecutionTimeout(reason) => {
            shared.record_exit(&exit, reason.clone());
            if was_recycling
                || matches!(
                    prior_reason,
                    WorkerSpawnReason::MaxRequests | WorkerSpawnReason::MaxAge
                )
            {
                shared.release_recycle(exit.slot);
            }
            let timeouts = shared.record_execution_timeout(exit.slot);
            errln!(
                "PHP worker pool {} slot {} scheduling generation {} immediately (execution timeout {timeouts}; slot failure history unchanged)",
                shared.config.label,
                exit.slot,
                exit.generation.saturating_add(1)
            );
            // Zero delay, and no `increment_failure`. The respawn rate is
            // already bounded by the deadline itself: a slot cannot time out
            // more often than once per `request_timeout` plus its bootstrap,
            // which is why `request_timeout` has a validated floor.
            spawn_replacement(
                tasks,
                exit,
                WorkerSpawnReason::Respawn,
                Duration::ZERO,
                phase_rx,
                worker_shutdown_rx,
                shared,
            );
        }
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
            let delay = replacement_delay(failures);
            errln!(
                "PHP worker pool {} slot {} scheduling generation {} after {} ms (failure {failures}{})",
                shared.config.label,
                exit.slot,
                exit.generation.saturating_add(1),
                delay.as_millis(),
                if failures > MAX_CONSECUTIVE_FAILURES {
                    ", cooldown"
                } else {
                    ""
                }
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
            errln!(
                "PHP worker pool {} slot {} generation {} stopped unexpectedly: {reason}",
                shared.config.label,
                exit.slot,
                exit.generation
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
    shared: &Arc<PoolShared>,
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
        GenerationOutcome::Failed(reason)
        | GenerationOutcome::BootstrapFailed(reason)
        | GenerationOutcome::ExecutionTimeout(reason) => reason.clone(),
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
///
/// # Bound
///
/// The stash holds at most `workers + 1` entries. The proof has two halves:
///
/// 1. At most one live capability exists per slot. `IdleWorker` is not `Clone`
///    and `run_worker_generation` publishes exactly one capability per command
///    it consumes. The supervisor joins a slot's old generation before
///    spawning its replacement (see `handle_serving_exit`), so two live
///    generations for one slot never overlap.
/// 2. Therefore a `retain` pass that drops closed mailboxes leaves at most
///    `workers` entries, and this function pushes at most one before checking.
///
/// A violation would mean the capability-per-generation invariant broke
/// somewhere upstream, so the assertion is placed here where it can catch it.
fn stash_idle(stash: &mut VecDeque<IdleWorker>, published: IdleWorker, workers: usize) {
    stash.push_back(published);
    if stash.len() > workers {
        stash.retain(|idle_worker| !idle_worker.mailbox.is_closed());
    }
    debug_assert!(
        stash.len() <= workers + 1,
        "idle stash holds {} capabilities for {workers} workers; at most one live \
         capability may exist per slot",
        stash.len()
    );
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
                    errln!(
                        "PHP worker pool {} slot {} generation {} published an invalid full idle mailbox",
                        shared.config.label,
                        idle_worker.slot,
                        idle_worker.generation
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

fn spawn_stderr_drain(mut stderr: tokio::process::ChildStderr, label: Arc<str>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(length) => errln!(
                    "PHP worker pool {label} stderr: {}",
                    String::from_utf8_lossy(&buffer[..length]).trim_end()
                ),
                Err(error) => {
                    errln!("failed to drain PHP worker pool {label} stderr: {error}");
                    break;
                }
            }
        }
    })
}

enum HttpRequestError {
    UnsupportedVersion,
    TooLarge,
    TooManyHeaders,
    Timeout,
    Body,
    BudgetExhausted,
}

fn body_admission_response(error: HttpRequestError) -> HandlerResponse {
    match error {
        HttpRequestError::UnsupportedVersion => simple_response(
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            "Unsupported HTTP Version",
        ),
        HttpRequestError::TooLarge => {
            simple_response(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
        }
        HttpRequestError::TooManyHeaders => simple_response(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "Too Many Headers",
        ),
        HttpRequestError::Timeout => {
            simple_response(StatusCode::REQUEST_TIMEOUT, "Request Body Timeout")
        }
        HttpRequestError::Body => simple_response(StatusCode::BAD_REQUEST, "Invalid Request Body"),
        // The same class as a full worker queue: the request is well formed,
        // the server is out of a bounded resource right now. It is deliberately
        // not 413, because the body is within its own per-request limit.
        HttpRequestError::BudgetExhausted => simple_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Request Body Memory Budget",
        ),
    }
}

/// A reconstructed request and the budget reservation covering its bytes.
///
/// The two travel together from here until the job is dropped, which is what
/// makes "reserved bytes are never less than buffered bytes" hold on every
/// path rather than only on the success path.
pub(crate) struct AdmittedRequest {
    request: WorkerRequest,
    body: BodyReservation,
}

/// How much a growing body over-reserves before it has to ask again.
///
/// The reader mirrors `Vec`'s own doubling instead of stepping by a fixed
/// block: linear steps would make a 1 MiB body realloc and copy dozens of
/// times, which costs far more than the atomic it would save.
const BODY_GROWTH_FLOOR: usize = 8 * 1_024;

async fn http_request(
    request: Request<Incoming>,
    connection: ConnectionInfo,
    body_read_timeout: Duration,
    budget: &Arc<BodyBudget>,
) -> Result<AdmittedRequest, HttpRequestError> {
    let (parts, mut body) = request.into_parts();
    let http_version = match parts.version {
        Version::HTTP_10 => WorkerHttpVersion::Http10,
        Version::HTTP_11 => WorkerHttpVersion::Http11,
        _ => return Err(HttpRequestError::UnsupportedVersion),
    };
    if parts.headers.len() > MAX_HEADERS {
        return Err(HttpRequestError::TooManyHeaders);
    }
    let declared = body.size_hint().lower();
    if declared > MAX_REQUEST_BODY as u64 {
        return Err(HttpRequestError::TooLarge);
    }
    // A request with no body touches neither the allocator nor the budget.
    // This is the whole static/GET path, and it must stay free.
    if body.is_end_stream() {
        return Ok(AdmittedRequest {
            request: build_worker_request(parts, http_version, connection, Vec::new()),
            body: BodyReservation::none(),
        });
    }

    // Charge the declared length before reading a byte. A client that declares
    // a megabyte and then trickles it must pay for the megabyte up front,
    // otherwise sending slowly would be the cheapest way past the ceiling.
    let declared = declared as usize;
    let Some(mut reservation) = budget.reserve(declared) else {
        return Err(HttpRequestError::BudgetExhausted);
    };

    let bytes = match timeout(
        body_read_timeout,
        read_http_body(&mut body, declared, &mut reservation),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(HttpRequestError::Timeout),
    };
    debug_assert!(
        !budget.is_enabled() || bytes.capacity() <= reservation.held(),
        "buffered {} bytes against a {} byte reservation",
        bytes.capacity(),
        reservation.held()
    );
    Ok(AdmittedRequest {
        request: build_worker_request(parts, http_version, connection, bytes),
        body: reservation,
    })
}

async fn read_http_body(
    body: &mut Incoming,
    declared: usize,
    reservation: &mut BodyReservation,
) -> Result<Vec<u8>, HttpRequestError> {
    // Bounded by the declared length, so a false declaration cannot make the
    // server allocate ahead of what it has actually been charged for.
    let mut bytes = Vec::with_capacity(declared.min(BODY_GROWTH_FLOOR));
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| HttpRequestError::Body)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let needed = bytes.len().saturating_add(data.len());
        if needed > MAX_REQUEST_BODY {
            return Err(HttpRequestError::TooLarge);
        }
        if needed > bytes.capacity() {
            // Mirror Vec's own doubling, floored so a trickle of tiny frames
            // does not consult the budget on every frame, and capped at the
            // per-request limit. `needed` is already inside that cap.
            let mut target = bytes.capacity().saturating_mul(2).max(BODY_GROWTH_FLOOR);
            if target > MAX_REQUEST_BODY {
                target = MAX_REQUEST_BODY;
            }
            let target = target.max(needed);
            // Reserve, then allocate. The other order would let the process
            // hold memory the counter has not admitted yet.
            if !reservation.grow_to(target) {
                return Err(HttpRequestError::BudgetExhausted);
            }
            bytes.reserve_exact(target - bytes.len());
        }
        bytes.extend_from_slice(&data);
    }
    Ok(bytes)
}

fn build_worker_request(
    parts: hyper::http::request::Parts,
    http_version: WorkerHttpVersion,
    connection: ConnectionInfo,
    bytes: Vec<u8>,
) -> WorkerRequest {
    let transport = connection.transport();
    let uri = parts.uri.to_string();
    let path = parts.uri.path();
    let query = parts.uri.query().unwrap_or_default();
    let headers = parts
        .headers
        .iter()
        .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect();
    WorkerRequest {
        http_version,
        scheme: connection.scheme(),
        peer_addr: transport.peer_addr(),
        local_addr: transport.local_addr(),
        method: parts.method.as_str().as_bytes().to_vec(),
        uri: uri.into_bytes(),
        path: path.as_bytes().to_vec(),
        query: query.as_bytes().to_vec(),
        headers,
        body: bytes,
    }
}

fn http_response(worker: WorkerResponse) -> Result<HandlerResponse, String> {
    if !(200..=599).contains(&worker.status) {
        return Err(format!("invalid final status {}", worker.status));
    }
    let status = StatusCode::from_u16(worker.status).map_err(|error| error.to_string())?;
    let mut response = Response::new(ResponseBody::buffered(Bytes::from(worker.body)));
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
    use std::{error::Error, sync::Arc};

    use super::*;

    /// The repository PHP runtime, vendored per platform like benches/php_ipc.rs.
    #[cfg(windows)]
    const PHP_RELATIVE_PATH: &str = "php/8.5/php.exe";
    #[cfg(not(windows))]
    const PHP_RELATIVE_PATH: &str = "php/8.5/php";

    fn worker_config(script: &str) -> PhpWorkerConfig {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        PhpWorkerConfig {
            executable: root.join(PHP_RELATIVE_PATH),
            script: root.join("fixtures/php-worker").join(script),
            ..Default::default()
        }
    }

    fn fixture(script: &str) -> PhpWorkerConfig {
        let config = worker_config(script);
        assert!(
            config.executable.is_file(),
            "PHP-dependent test requires repository {PHP_RELATIVE_PATH}"
        );
        config
    }

    fn pool(script: &str, workers: usize, queue_capacity: usize) -> Arc<PhpWorkerPool> {
        pool_with(fixture(script), workers, queue_capacity)
    }

    /// A pool whose workers are never started. Dispatcher tests drive its job
    /// channels directly, so they must not require a PHP runtime to exist.
    fn unstarted_pool(workers: usize, queue_capacity: usize) -> Arc<PhpWorkerPool> {
        pool_with(worker_config("worker.php"), workers, queue_capacity)
    }

    fn pool_with(
        worker: PhpWorkerConfig,
        workers: usize,
        queue_capacity: usize,
    ) -> Arc<PhpWorkerPool> {
        Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
            worker,
            workers,
            queue_capacity,
            label: Arc::from("test"),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            max_requests: None,
            max_worker_age: None,
            body_budget: BodyBudget::disabled(),
        }))
    }

    fn admitted(request: WorkerRequest) -> AdmittedRequest {
        AdmittedRequest {
            request,
            body: BodyReservation::none(),
        }
    }

    fn request(method: &str, path: &str, query: &str, body: Vec<u8>) -> WorkerRequest {
        let uri = if query.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{query}")
        };
        WorkerRequest {
            http_version: WorkerHttpVersion::Http11,
            scheme: crate::transport::Scheme::Http,
            peer_addr: "127.0.0.2:43210".parse().unwrap(),
            local_addr: "127.0.0.1:8080".parse().unwrap(),
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
    async fn only_a_provably_unstarted_job_is_queued_again_and_only_once() {
        let pool = unstarted_pool(1, 1);
        let (jobs_tx, mut jobs_rx) = mpsc::channel(1);
        pool.shared.jobs_tx.set(jobs_tx).unwrap();

        let job = |redispatches: u8| {
            let (response_tx, response_rx) = oneshot::channel();
            (
                PhpJob {
                    request: request("GET", "/hello", "", Vec::new()),
                    _body: BodyReservation::none(),
                    response_tx,
                    redispatches,
                },
                response_rx,
            )
        };
        let started = PhpWorkerError::Request("timeout".to_owned());
        let not_started = PhpWorkerError::NotStarted("process exited".to_owned());

        // A failure that may have started execution is never queued again, no
        // matter how many attempts are left.
        let (candidate, _keep_alive) = job(0);
        assert!(redispatch_not_started(&pool.shared, &started, candidate).is_some());

        // A provably unstarted job goes back to the queue, carrying its spent
        // attempt with it.
        let (candidate, _keep_alive) = job(0);
        assert!(redispatch_not_started(&pool.shared, &not_started, candidate).is_none());
        let queued = jobs_rx.try_recv().expect("the job must be queued again");
        assert_eq!(queued.redispatches, 1);

        // The second unstarted failure for the same job is where it stops.
        let (candidate, _keep_alive) = job(1);
        assert!(redispatch_not_started(&pool.shared, &not_started, candidate).is_some());

        // A full queue is refused rather than dropped, so the caller still
        // answers the client.
        let (filler, _filler_alive) = job(0);
        assert!(redispatch_not_started(&pool.shared, &not_started, filler).is_none());
        let (candidate, _keep_alive) = job(0);
        assert!(redispatch_not_started(&pool.shared, &not_started, candidate).is_some());

        // A client that has already disconnected is dropped, not queued.
        let (candidate, response_rx) = job(0);
        drop(response_rx);
        assert!(redispatch_not_started(&pool.shared, &not_started, candidate).is_none());
    }

    #[tokio::test]
    async fn dispatcher_abort_releases_pending_and_future_callers() {
        let pool = unstarted_pool(1, 8);
        let (jobs_tx, jobs_rx) = mpsc::channel(8);
        let (_idle_tx, idle_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        pool.shared.jobs_tx.set(jobs_tx.clone()).unwrap();
        pool.shared.accepting.store(true, Ordering::Release);

        let (response_tx, response_rx) = oneshot::channel();
        jobs_tx
            .send(PhpJob {
                request: request("GET", "/hello", "", Vec::new()),
                _body: BodyReservation::none(),
                response_tx,
                redispatches: 0,
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
            pool.execute(admitted(request("GET", "/hello", "", Vec::new())))
                .await,
            Err(PoolExecuteError::Unavailable)
        ));
    }

    #[test]
    fn worker_config_debug_hides_environment_values() {
        let config = PhpWorkerConfig {
            executable: PathBuf::from("php.exe"),
            script: PathBuf::from("worker.php"),
            env: BTreeMap::from([
                ("HAMSTER_LARAVEL_PATH".to_owned(), "/srv/app".to_owned()),
                ("DB_PASSWORD".to_owned(), "hunter2".to_owned()),
            ]),
        };

        let rendered = format!("{config:?}");
        assert!(
            rendered.contains("DB_PASSWORD"),
            "names must stay visible: {rendered}"
        );
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("/srv/app"),
            "values must not appear in Debug output: {rendered}"
        );
    }

    #[tokio::test]
    async fn stale_generation_channel_cannot_receive_a_job() {
        let pool = Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
            worker: PhpWorkerConfig {
                executable: PathBuf::from("unused-php"),
                script: PathBuf::from("unused-worker.php"),
                ..Default::default()
            },
            workers: 1,
            queue_capacity: 8,
            label: Arc::from("test"),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            max_requests: None,
            max_worker_age: None,
            body_budget: BodyBudget::disabled(),
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
                _body: BodyReservation::none(),
                response_tx,
                redispatches: 0,
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
                ..Default::default()
            },
            workers: 1,
            queue_capacity: 8,
            label: Arc::from("test"),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            max_requests: None,
            max_worker_age: None,
            body_budget: BodyBudget::disabled(),
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
                _body: BodyReservation::none(),
                response_tx,
                redispatches: 0,
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
            Duration::from_secs(5)
        );
    }

    /// Builds a capability whose generation is already gone: dropping the
    /// receiver is exactly what a finished `run_worker_generation` does.
    fn stale_capability(slot: usize, generation: u64) -> IdleWorker {
        let (mailbox, receiver) = mpsc::channel(1);
        drop(receiver);
        IdleWorker {
            slot,
            generation,
            mailbox,
        }
    }

    #[tokio::test]
    async fn take_idle_discards_stale_capabilities_and_dispatches_once() {
        // Several dead generations queued ahead of one live generation is the
        // shape produced by a burst of replacements: the dispatcher stashes
        // capabilities faster than it consumes them.
        let mut stash = VecDeque::new();
        stash.push_back(stale_capability(0, 1));
        stash.push_back(stale_capability(0, 2));
        stash.push_back(stale_capability(1, 4));
        let (live_mailbox, mut live_rx) = mpsc::channel(1);
        stash.push_back(IdleWorker {
            slot: 2,
            generation: 7,
            mailbox: live_mailbox,
        });

        let taken = take_idle(&mut stash).expect("the live capability must survive");
        assert_eq!(taken.slot, 2);
        assert_eq!(taken.generation, 7);
        assert!(
            stash.is_empty(),
            "every stale capability ahead of the live one must be discarded"
        );

        // The job reaches the live generation, and reaches it exactly once.
        // Redispatch here is only ever legal because a closed mailbox proves
        // that generation never received the job, so no possibly-started
        // transaction can be replayed.
        let (response_tx, _response_rx) = oneshot::channel();
        taken
            .mailbox
            .try_send(WorkerCommand::Job(PhpJob {
                _body: BodyReservation::none(),
                request: request("GET", "/", "", Vec::new()),
                response_tx,
                redispatches: 0,
            }))
            .expect("the live mailbox must accept the job");
        let WorkerCommand::Job(delivered) = live_rx.recv().await.expect("job must be delivered");
        assert_eq!(delivered.request.path, b"/".to_vec());
        assert!(
            live_rx.try_recv().is_err(),
            "the job must not be delivered more than once"
        );
    }

    #[test]
    fn take_idle_reports_empty_when_every_capability_is_stale() {
        let mut stash = VecDeque::new();
        stash.push_back(stale_capability(0, 1));
        stash.push_back(stale_capability(1, 1));
        assert!(take_idle(&mut stash).is_none());
        assert!(stash.is_empty());
    }

    #[test]
    fn stash_idle_stays_within_its_bound() {
        // One live capability per slot plus the freshly published one is the
        // documented ceiling; stale entries must not accumulate past it.
        let workers = 2;
        let mut stash = VecDeque::new();
        for generation in 1..=32 {
            stash_idle(&mut stash, stale_capability(0, generation), workers);
            assert!(
                stash.len() <= workers + 1,
                "stash grew to {} for {workers} workers",
                stash.len()
            );
        }
    }

    #[test]
    fn prolonged_failure_enters_bounded_cooldown() {
        // Past the ladder the delay keeps growing to a ceiling and stays
        // there. A slot is never permanently retired, so recovery after a
        // transient host problem needs no operator action.
        let expected = [30, 60, 120, 240, 300, 300, 300];
        for (index, seconds) in expected.into_iter().enumerate() {
            let failures = MAX_CONSECUTIVE_FAILURES + 1 + index;
            assert_eq!(
                replacement_delay(failures),
                Duration::from_secs(seconds),
                "failure {failures}"
            );
        }
        // Saturating shift: an unbounded failure count stays at the ceiling
        // rather than overflowing or wrapping back to a tight loop.
        assert_eq!(replacement_delay(usize::MAX), COOLDOWN_MAX);
        assert_eq!(replacement_delay(10_000), COOLDOWN_MAX);
    }

    #[test]
    fn respawn_rate_is_bounded_independently_of_load() {
        // The only inputs to the delay are the slot's own failure count and
        // fixed constants. Nothing here can be influenced by HTTP throughput,
        // concurrency, or how many requests failed, which is what keeps a
        // permanently broken slot from becoming a spawn storm.
        let ceiling = replacement_delay(usize::MAX);
        assert_eq!(ceiling, COOLDOWN_MAX);
        let spawns_per_hour = 3_600 / ceiling.as_secs();
        assert!(
            spawns_per_hour <= 12,
            "a permanently failing slot must not exceed 12 spawns per hour, got {spawns_per_hour}"
        );
        // The fast ladder is only reachable while the failure count is low.
        assert!(replacement_delay(MAX_CONSECUTIVE_FAILURES + 1) >= COOLDOWN_BASE);
    }

    #[test]
    fn stability_period_outlasts_a_single_request_budget() {
        // A generation counts as stable only after staying READY for longer
        // than its own worst-case single request, with margin. Keeping this
        // time based is what decouples respawn rate from throughput.
        // The outage notice must not fire for an ordinary replacement. The
        // worst bootstrap-to-READY this repository has measured is 403 ms for
        // a real Laravel application (`doc/BENCHMARKS.md`); the fixture is
        // ~38 ms. Anything at or below that would make routine churn look like
        // an outage.
        assert!(
            POOL_OUTAGE_NOTICE_AFTER >= Duration::from_millis(806),
            "the outage threshold must clear the worst measured bootstrap twice over"
        );
        assert!(STABILITY_RESET_PERIOD > DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(STABILITY_RESET_PERIOD, DEFAULT_REQUEST_TIMEOUT * 3);
        let worst_case_spawns_per_minute = 60 / STABILITY_RESET_PERIOD.as_secs();
        assert!(
            worst_case_spawns_per_minute <= 4,
            "a slot that fails just above the stability threshold must stay bounded"
        );
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
            .execute(&request("GET", "/state", "", Vec::new()))
            .await
            .unwrap();
        let state = String::from_utf8(empty.body).unwrap();
        assert!(state.contains(&format!("\"pid\":{}", started.pid)));
        assert!(state.contains("\"bootstrap\":1"));
        assert!(state.contains("\"request\":1"));

        let inspected = worker
            .execute(&request(
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
            .execute(&request("POST", "/echo", "", binary.clone()))
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
            .execute(&request("POST", "/large", "", large.clone()))
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
                pool.execute(admitted(request("GET", "/hello", "", Vec::new())))
                    .await
            }));
        }
        for task in tasks {
            let response = task.await.unwrap().unwrap();
            assert_eq!(response.body, b"Hello from PHP");
        }
        let state = pool
            .execute(admitted(request("GET", "/state", "", Vec::new())))
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
    async fn partial_pool_startup_preserves_cause_and_reports_cleanup_failure() {
        let pool = PhpWorkerPool::with_worker_configs(
            vec![
                fixture("ignores_shutdown.php"),
                fixture("malformed_ready.php"),
            ],
            8,
        );
        let error = pool.start().await.unwrap_err();
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("expected READY frame"), "{diagnostic}");
        assert!(
            diagnostic.contains("additional startup cleanup failure"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("worker 0"), "{diagnostic}");
        assert!(
            error.source().is_some(),
            "startup cause must remain chained"
        );
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
                    .execute(&request("GET", path, "", Vec::new()))
                    .await
                    .is_err()
            );
            assert_eq!(worker.state(), WorkerState::Unavailable);
            assert!(matches!(
                worker
                    .execute(&request("GET", "/hello", "", Vec::new()))
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
            .execute(&request("GET", "/delay", "ms=500", Vec::new()))
            .await
            .unwrap_err();
        // The variant, not just the message. This is what routes the failure
        // away from the slot's infrastructure backoff ladder, so a regression
        // that reclassified it would otherwise pass on the message alone.
        assert!(
            matches!(error, PhpWorkerError::ExecutionTimeout(_)),
            "an expired deadline is an application execution timeout: {error}"
        );
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
        let error = worker
            .execute(&request("POST", "/large", "", vec![b'x'; MAX_REQUEST_BODY]))
            .await
            .expect_err("a worker exiting after the frame prefix must fail the request");
        assert!(
            matches!(error, PhpWorkerError::Request(_)),
            "a partial frame write is possibly started and must not be retry-safe: {error}"
        );
        assert_eq!(worker.state(), WorkerState::Unavailable);
        worker.shutdown().await.unwrap();

        let mut worker = PhpWorker::new(fixture("worker.php"));
        worker.start().await.unwrap();
        let response = worker
            .execute(&request("GET", "/exit-after-response", "", Vec::new()))
            .await
            .unwrap();
        assert_eq!(response.body, b"exiting");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            worker
                .execute(&request("GET", "/hello", "", Vec::new()))
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
                    .execute(&request("GET", "/hello", "", Vec::new()))
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
            .execute(&request("GET", "/exit-after-response", "", Vec::new()))
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
                .execute(admitted(request("GET", "/delay", "ms=200", Vec::new())))
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
                pool.execute(admitted(request(
                    "GET",
                    "/state-delay",
                    "ms=25",
                    Vec::new(),
                )))
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
                        .execute(admitted(request("GET", "/hello", "", Vec::new())))
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
                pool.execute(admitted(request(
                    "GET",
                    "/state-delay",
                    "ms=25",
                    Vec::new(),
                )))
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
