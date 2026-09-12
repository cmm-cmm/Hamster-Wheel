use std::{
    fmt, io,
    path::PathBuf,
    process::Stdio,
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
    sync::Mutex,
    task::JoinHandle,
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

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct PhpWorkerConfig {
    pub executable: PathBuf,
    pub script: PathBuf,
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
    inner: Mutex<WorkerSlot>,
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
            inner: Mutex::new(WorkerSlot::Starting),
        }
    }

    pub async fn start(&self) -> Result<WorkerStart, PhpWorkerError> {
        let mut inner = self.inner.lock().await;
        if !matches!(*inner, WorkerSlot::Starting) {
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
                *inner = WorkerSlot::Unavailable(message.clone());
                return Err(PhpWorkerError::Startup(message));
            }
            Err(message) => {
                terminate_child(&mut child).await;
                let _ = stderr_task.await;
                *inner = WorkerSlot::Unavailable(message.clone());
                return Err(PhpWorkerError::Startup(message));
            }
        };

        let report = WorkerStart {
            pid: ready.pid,
            elapsed: started.elapsed(),
        };
        *inner = WorkerSlot::Running(Box::new(RunningWorker {
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

    pub async fn state(&self) -> WorkerState {
        match &*self.inner.lock().await {
            WorkerSlot::Starting => WorkerState::Starting,
            WorkerSlot::Running(worker) => worker.state,
            WorkerSlot::Unavailable(_) => WorkerState::Unavailable,
            WorkerSlot::Stopped => WorkerState::Stopped,
        }
    }

    pub async fn pid(&self) -> Option<u32> {
        match &*self.inner.lock().await {
            WorkerSlot::Running(worker) => Some(worker.pid),
            WorkerSlot::Starting | WorkerSlot::Unavailable(_) | WorkerSlot::Stopped => None,
        }
    }

    pub(crate) async fn execute(
        &self,
        request: WorkerRequest,
    ) -> Result<WorkerResponse, PhpWorkerError> {
        let mut inner = self.inner.lock().await;
        let prior_exit = match &mut *inner {
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
                fail_locked_worker(&mut inner, &message).await;
                return Err(PhpWorkerError::Request(message));
            }
        };
        if let Some(status) = prior_exit {
            let message = format!("process exited before request with status {status}");
            *inner = WorkerSlot::Unavailable(message.clone());
            return Err(PhpWorkerError::Unavailable(message));
        }

        let transaction = {
            let worker = match &mut *inner {
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
                if let WorkerSlot::Running(worker) = &mut *inner {
                    worker.state = WorkerState::Ready;
                }
                Ok(response)
            }
            Err(message) => {
                fail_locked_worker(&mut inner, &message).await;
                Err(PhpWorkerError::Request(message))
            }
        }
    }

    pub async fn shutdown(&self) -> Result<(), PhpWorkerError> {
        let mut inner = self.inner.lock().await;
        let slot = std::mem::replace(&mut *inner, WorkerSlot::Stopped);
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
            Err(error) => {
                eprintln!("PHP worker request failed: {error}");
                simple_response(StatusCode::BAD_GATEWAY, "PHP Worker Unavailable")
            }
        }
    }
}

async fn fail_locked_worker(inner: &mut WorkerSlot, reason: &str) {
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

    /// Locates a PHP CLI binary: the `HAMSTER_PHP` override, the vendored
    /// Windows development runtime, then the first `php` on `PATH`. Mirrors the
    /// resolution used by `tests/php_worker.rs` and `scripts/`.
    fn locate_php() -> Option<PathBuf> {
        if let Some(value) = std::env::var_os("HAMSTER_PHP") {
            let candidate = PathBuf::from(value);
            return candidate.is_file().then_some(candidate);
        }
        let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
        if vendored.is_file() {
            return Some(vendored);
        }
        // The targeted version first: a distribution's generic `php` is often
        // an older branch than the one this project ships against.
        let names: &[&str] = if cfg!(windows) {
            &["php8.5.exe", "php.exe"]
        } else {
            &["php8.5", "php"]
        };
        std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
            names
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
    }

    /// Reports a skipped test on stderr so a missing PHP runtime never looks
    /// like a pass.
    fn php_available(test: &str) -> bool {
        if locate_php().is_some() {
            return true;
        }
        eprintln!(
            "skipping {test}: no PHP CLI found; set HAMSTER_PHP, install php, or vendor php/8.5/php.exe"
        );
        false
    }

    fn fixture(script: &str) -> PhpWorkerConfig {
        let executable = locate_php().expect("PHP-dependent test requires a PHP CLI");
        PhpWorkerConfig {
            executable,
            script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/php-worker")
                .join(script),
        }
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
    async fn persistent_worker_preserves_request_data_and_binary_payloads() {
        if !php_available("persistent_worker_preserves_request_data_and_binary_payloads") {
            return;
        }
        let worker = PhpWorker::new(fixture("worker.php"));
        let started = worker.start().await.unwrap();
        assert_eq!(worker.pid().await, Some(started.pid));

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
        assert_eq!(worker.state().await, WorkerState::Stopped);
        assert_eq!(worker.pid().await, None);
    }

    #[tokio::test]
    async fn concurrent_transactions_are_serialized_without_protocol_corruption() {
        if !php_available("concurrent_transactions_are_serialized_without_protocol_corruption") {
            return;
        }
        let worker = Arc::new(PhpWorker::new(fixture("worker.php")));
        worker.start().await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let worker = Arc::clone(&worker);
            tasks.push(tokio::spawn(async move {
                worker
                    .execute(request("GET", "/hello", "", Vec::new()))
                    .await
            }));
        }
        for task in tasks {
            let response = task.await.unwrap().unwrap();
            assert_eq!(response.body, b"Hello from PHP");
        }
        let state = worker
            .execute(request("GET", "/state", "", Vec::new()))
            .await
            .unwrap();
        assert!(
            String::from_utf8(state.body)
                .unwrap()
                .contains("\"request\":17")
        );
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_failures_are_detected_before_ready() {
        if !php_available("startup_failures_are_detected_before_ready") {
            return;
        }
        for script in ["exit_before_ready.php", "malformed_ready.php"] {
            let worker = PhpWorker::with_timeouts(
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
            assert_eq!(worker.state().await, WorkerState::Unavailable);
        }

        let worker = PhpWorker::with_timeouts(
            fixture("slow_ready.php"),
            WorkerTimeouts {
                startup: Duration::from_millis(50),
                ..WorkerTimeouts::default()
            },
        );
        let error = worker.start().await.unwrap_err();
        assert!(error.to_string().contains("READY timeout"));
        assert_eq!(worker.state().await, WorkerState::Unavailable);
    }

    #[tokio::test]
    async fn malformed_truncated_timeout_and_exit_fail_closed() {
        if !php_available("malformed_truncated_timeout_and_exit_fail_closed") {
            return;
        }
        for path in ["/malformed", "/truncated"] {
            let worker = PhpWorker::new(fixture("worker.php"));
            worker.start().await.unwrap();
            assert!(
                worker
                    .execute(request("GET", path, "", Vec::new()))
                    .await
                    .is_err()
            );
            assert_eq!(worker.state().await, WorkerState::Unavailable);
            assert!(matches!(
                worker
                    .execute(request("GET", "/hello", "", Vec::new()))
                    .await,
                Err(PhpWorkerError::Unavailable(_))
            ));
            worker.shutdown().await.unwrap();
        }

        let worker = PhpWorker::with_timeouts(
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
        assert_eq!(worker.state().await, WorkerState::Unavailable);
        worker.shutdown().await.unwrap();

        let worker = PhpWorker::with_timeouts(
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
        assert_eq!(worker.state().await, WorkerState::Unavailable);
        worker.shutdown().await.unwrap();

        let worker = PhpWorker::new(fixture("worker.php"));
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
        assert_eq!(worker.state().await, WorkerState::Unavailable);
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cleans_up_ready_and_used_workers() {
        if !php_available("shutdown_cleans_up_ready_and_used_workers") {
            return;
        }
        for send_request in [false, true] {
            let worker = PhpWorker::new(fixture("worker.php"));
            worker.start().await.unwrap();
            if send_request {
                worker
                    .execute(request("GET", "/hello", "", Vec::new()))
                    .await
                    .unwrap();
            }
            worker.shutdown().await.unwrap();
            assert_eq!(worker.state().await, WorkerState::Stopped);
            assert_eq!(worker.pid().await, None);
        }

        let worker = PhpWorker::new(fixture("worker.php"));
        worker.start().await.unwrap();
        worker
            .execute(request("GET", "/exit-after-response", "", Vec::new()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.shutdown().await.unwrap();
        assert_eq!(worker.state().await, WorkerState::Stopped);
    }

    #[tokio::test]
    async fn shutdown_waits_for_busy_request_then_stops_worker() {
        if !php_available("shutdown_waits_for_busy_request_then_stops_worker") {
            return;
        }
        let worker = Arc::new(PhpWorker::new(fixture("worker.php")));
        worker.start().await.unwrap();

        let request_worker = Arc::clone(&worker);
        let request_task = tokio::spawn(async move {
            request_worker
                .execute(request("GET", "/delay", "ms=200", Vec::new()))
                .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;

        let shutdown_started = Instant::now();
        worker.shutdown().await.unwrap();
        let response = request_task.await.unwrap().unwrap();
        assert_eq!(response.body, b"delayed");
        assert!(shutdown_started.elapsed() >= Duration::from_millis(100));
        assert_eq!(worker.state().await, WorkerState::Stopped);
        assert_eq!(worker.pid().await, None);
    }
}
