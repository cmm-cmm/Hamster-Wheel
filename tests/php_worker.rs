use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod common;

use hamster_wheel::{
    config,
    php_worker::{PoolSnapshot, WorkerLifecycleState, WorkerSpawnReason},
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

struct TestServer {
    address: SocketAddr,
    runtime: Arc<config::RuntimeConfig>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

// External Laravel/PMS probes share one optimized scratch application and can
// perform real database/session work. Serializing only those probes prevents
// concurrent bootstrap from turning the fixed five-second product timeout into
// a harness artifact; ordinary PHP worker integration tests remain parallel.
static LARAVEL_APP_TEST_LOCK: Mutex<()> = Mutex::const_new(());

impl TestServer {
    async fn start() -> Self {
        Self::start_with(1, 32).await
    }

    async fn start_with(workers: usize, queue_capacity: usize) -> Self {
        Self::start_with_policy(workers, queue_capacity, None, None).await
    }

    async fn start_with_policy(
        workers: usize,
        queue_capacity: usize,
        max_requests: Option<u64>,
        max_worker_age: Option<u64>,
    ) -> Self {
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
        let runtime = Arc::new(php_runtime_with_script(
            workers,
            queue_capacity,
            max_requests,
            max_worker_age,
            &script,
        ));
        Self::start_runtime(runtime).await
    }

    async fn start_runtime(runtime: Arc<config::RuntimeConfig>) -> Self {
        runtime
            .start_workers()
            .await
            .expect("PHP worker should reach READY");
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let router = runtime.router_arc();
        let http_limits = runtime.http_limits();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, http_limits, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Self {
            address,
            runtime,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    async fn request(&self, request: &[u8]) -> Vec<u8> {
        request_at(self.address, request).await
    }

    async fn stop(mut self) {
        self.shutdown_tx
            .take()
            .expect("shutdown sender should exist")
            .send(())
            .expect("server should still be running");
        self.task
            .await
            .expect("server task should finish")
            .expect("server should stop cleanly");
        self.runtime
            .shutdown_workers()
            .await
            .expect("PHP worker should stop cleanly");
        assert!(
            self.runtime
                .php_workers()
                .iter()
                .all(|pool| pool.pids().is_empty())
        );
    }
}

async fn request_at(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("client should connect");
    stream
        .write_all(request)
        .await
        .expect("request should be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("response should be read");
    response
}

async fn request_at_from(
    address: SocketAddr,
    source_ip: Ipv4Addr,
    request: &[u8],
) -> (Vec<u8>, SocketAddr) {
    let socket = TcpSocket::new_v4().expect("IPv4 client socket should be created");
    socket
        .bind(SocketAddr::new(IpAddr::V4(source_ip), 0))
        .expect("loopback source address should bind");
    let mut stream = socket
        .connect(address)
        .await
        .expect("client should connect");
    let client = stream
        .local_addr()
        .expect("client should have a local endpoint");
    stream
        .write_all(request)
        .await
        .expect("request should be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("response should be read");
    (response, client)
}

fn php_runtime_with_script(
    workers: usize,
    queue_capacity: usize,
    max_requests: Option<u64>,
    max_worker_age: Option<u64>,
    script: &Path,
) -> config::RuntimeConfig {
    php_runtime_with_script_at(
        workers,
        queue_capacity,
        max_requests,
        max_worker_age,
        script,
        "127.0.0.1:8080",
    )
}

fn php_runtime_with_script_at(
    workers: usize,
    queue_capacity: usize,
    max_requests: Option<u64>,
    max_worker_age: Option<u64>,
    script: &Path,
    listen: &str,
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let mut source = format!(
        "[server]\nlisten = \"{listen}\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\n",
        toml_literal(&executable),
        toml_literal(script)
    );
    if let Some(max_requests) = max_requests {
        source.push_str(&format!("max_requests = {max_requests}\n"));
    }
    if let Some(max_worker_age) = max_worker_age {
        source.push_str(&format!("max_worker_age = {max_worker_age}\n"));
    }
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP runtime config should compile")
}

fn php_runtime_with_body_policy(
    workers: usize,
    queue_capacity: usize,
    max_connections: usize,
    body_read_timeout: u64,
    script: &Path,
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\nmax_connections = {max_connections}\nbody_read_timeout = {body_read_timeout}\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\n",
        toml_literal(&executable),
        toml_literal(script)
    );
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP body policy config should compile")
}

fn php_runtime_with_env(
    workers: usize,
    script: &Path,
    env: &[(&str, &str)],
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let mut source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = 8\n\n[hosts.locations.php.env]\n",
        toml_literal(&executable),
        toml_literal(script)
    );
    for (name, value) in env {
        source.push_str(&format!("{name} = \"{value}\"\n"));
    }
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP runtime config with env should compile")
}

fn marker_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "hamster-wheel-{name}-{}-{unique}.marker",
        std::process::id()
    ))
}

fn url_encode(value: &Path) -> String {
    value
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (*byte as char).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

async fn wait_for_marker(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(path).await {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("PHP marker should appear")
}

async fn wait_for_pool(
    pool: &hamster_wheel::php_worker::PhpWorkerPool,
    predicate: impl Fn(&PoolSnapshot) -> bool,
) -> PoolSnapshot {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = pool.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker pool must reach the expected lifecycle state; last snapshot: {snapshot:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn marker_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn all_workers_live(snapshot: &PoolSnapshot) -> bool {
    snapshot.workers.iter().all(|worker| {
        worker.pid.is_some()
            && matches!(
                worker.state,
                WorkerLifecycleState::Ready | WorkerLifecycleState::Busy
            )
    })
}

async fn kill_process(pid: u32) {
    let status = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .await
        .expect("taskkill must be available on Windows");
    assert!(status.success(), "taskkill must terminate PHP PID {pid}");
}

async fn process_exists(pid: u32) -> bool {
    let output = tokio::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .await
        .expect("tasklist must be available on Windows");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

fn close_request(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").into_bytes()
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn split_response(response: &[u8]) -> (&[u8], &[u8]) {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response should have a header boundary");
    (&response[..boundary + 4], &response[boundary + 4..])
}

async fn read_response_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    loop {
        if let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            return response[..index + 4].to_vec();
        }
        let mut chunk = [0_u8; 512];
        let count = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .expect("response header timeout")
            .expect("read response header");
        assert_ne!(count, 0, "connection closed before response headers");
        response.extend_from_slice(&chunk[..count]);
    }
}

fn response_cookie_header(headers: &[u8]) -> String {
    std::str::from_utf8(headers)
        .expect("response headers should be UTF-8")
        .split("\r\n")
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().split(';').next().unwrap().to_owned())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn json_number(body: &[u8], field: &str) -> u64 {
    let body = std::str::from_utf8(body).expect("state response should be UTF-8");
    let marker = format!("\"{field}\":");
    let start = body.find(&marker).expect("state field should exist") + marker.len();
    let end = body[start..]
        .find(|character: char| !character.is_ascii_digit())
        .map_or(body.len(), |offset| start + offset);
    body[start..end]
        .parse()
        .expect("state field should be numeric")
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn laravel_request_reconstruction_passes_its_own_checks() {
    // The repository does not vendor a Laravel application, so the worker's
    // request reconstruction is covered by a PHP script that checks itself
    // against a stub UploadedFile. Cookie parsing, multipart splitting and
    // PHP-style field names are where the defects were, and none of them needs
    // a framework to test.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let output = tokio::process::Command::new(executable)
        .arg(root.join("fixtures/php-worker/laravel_request_test.php"))
        .output()
        .await
        .expect("php must be runnable");
    assert!(
        output.status.success(),
        "request reconstruction checks failed:
{}
{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("checks passed"),
        "the check script must report what it verified"
    );
}

fn metadata_multi_pool_runtime(script: &Path) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"one.test\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\nqueue_capacity = 8\n\n[[hosts]]\nname = \"two.test\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\nqueue_capacity = 8\n",
        toml_literal(&executable),
        toml_literal(script),
        toml_literal(&executable),
        toml_literal(script),
    );
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("multi-pool metadata config should compile")
}

async fn read_one_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        assert!(
            response.len() < 16 * 1024,
            "response head exceeded test limit"
        );
        let mut byte = [0_u8; 1];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut byte))
            .await
            .expect("response head timeout")
            .expect("response head should be complete");
        response.push(byte[0]);
    }
    let head = std::str::from_utf8(&response).expect("response head should be UTF-8");
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("buffered PHP response should have Content-Length");
    let head_length = response.len();
    response.resize(head_length + length, 0);
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.read_exact(&mut response[head_length..]),
    )
    .await
    .expect("response body timeout")
    .expect("response body should be complete");
    response
}

fn assert_json_string(body: &[u8], field: &str, expected: &str) {
    let body = std::str::from_utf8(body).expect("JSON response should be UTF-8");
    let needle = format!("\"{field}\":\"{expected}\"");
    let slash_escaped = format!("\"{field}\":\"{}\"", expected.replace('/', "\\/"));
    assert!(
        body.contains(&needle) || body.contains(&slash_escaped),
        "missing {needle} in {body}"
    );
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn protocol_php_decoder_passes_adversarial_checks() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(executable.is_file());
    let output = tokio::process::Command::new(executable)
        .arg(root.join("fixtures/php-worker/protocol_test.php"))
        .output()
        .await
        .expect("php must be runnable");
    assert!(
        output.status.success(),
        "protocol checks failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"18 checks passed\n");
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn laravel_octane_adapter_stages_response_until_cleanup_commit() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let output = tokio::process::Command::new(executable)
        .arg(root.join("fixtures/php-worker/laravel_octane_client_test.php"))
        .output()
        .await
        .expect("php must be runnable");
    assert!(
        output.status.success(),
        "Octane protocol adapter checks failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("12 checks passed"),
        "the check script must report every verified boundary"
    );
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn laravel_worker_reserves_stdout_for_protocol_only() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let output = tokio::process::Command::new(executable)
        .arg(root.join("fixtures/php-worker/laravel_stdout_test.php"))
        .output()
        .await
        .expect("php must be runnable");
    assert!(
        output.status.success(),
        "stdout reservation checks failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"4 checks passed\n");
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn unsafe_laravel_reset_modes_fail_before_ready() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    for strategy in ["none", "targeted"] {
        let output = tokio::process::Command::new(&executable)
            .arg(root.join("fixtures/php-worker/laravel_worker.php"))
            .env("HAMSTER_LARAVEL_PATH", &root)
            .env("HAMSTER_LARAVEL_RESET", strategy)
            .output()
            .await
            .expect("php must be runnable");
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("no longer accepts unsafe reset strategies"),
            "unsafe strategy {strategy} must fail explicitly before READY: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "failure must not emit protocol bytes"
        );
    }
}

#[tokio::test]
#[ignore = "requires HAMSTER_LARAVEL_TEST_PATH pointing to an Octane-enabled Laravel app"]
async fn one_worker_enforces_laravel_octane_ownership_contract() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let app = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the Octane-enabled test app"),
    );
    let php_ini = std::env::var_os("HAMSTER_LARAVEL_TEST_PHP_INI")
        .map(PathBuf::from)
        .unwrap_or_else(|| app.join("php.ini"));
    let output = tokio::process::Command::new(executable)
        .arg(root.join("fixtures/php-worker/laravel_lifecycle_test.php"))
        .env("HAMSTER_LARAVEL_TEST_PATH", &app)
        .env("HAMSTER_PMS_PROBE", "1")
        .env("PHPRC", php_ini)
        .output()
        .await
        .expect("php must be runnable");
    assert!(
        output.status.success(),
        "Laravel ownership checks failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("checks passed"),
        "the lifecycle test must report every checked ownership boundary"
    );
}

#[tokio::test]
#[ignore = "requires HAMSTER_LARAVEL_TEST_PATH pointing to an Octane-enabled Laravel app with /up"]
async fn laravel_worker_serves_repeated_requests_through_the_protocol() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_worker.php");
    let app = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the Octane-enabled test app"),
    );
    let php_ini = std::env::var_os("HAMSTER_LARAVEL_TEST_PHP_INI")
        .map(PathBuf::from)
        .unwrap_or_else(|| app.join("php.ini"));
    let app = app.to_string_lossy().replace('\\', "/");
    let php_ini = php_ini.to_string_lossy().replace('\\', "/");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[
            ("HAMSTER_LARAVEL_PATH", &app),
            ("PHPRC", &php_ini),
            ("HAMSTER_PMS_PROBE", "1"),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;
    let initial = server.runtime.php_worker().unwrap().snapshot().workers[0].clone();

    for _ in 0..2 {
        let response = server
            .request(b"GET /up HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await;
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "Laravel health request failed: {}",
            String::from_utf8_lossy(&response)
        );
    }

    let after = server.runtime.php_worker().unwrap().snapshot().workers[0].clone();
    assert_eq!(after.generation, initial.generation);
    assert_eq!(after.pid, initial.pid);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires a freshly prepared Octane-enabled scratch PMS with HamsterPmsProbeServiceProvider"]
async fn laravel_receives_owned_metadata_and_does_not_trust_forged_forwarding_headers() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_worker.php");
    let app_path = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the prepared scratch PMS"),
    );
    let provider_manifest = std::fs::read_to_string(app_path.join("bootstrap/providers.php"))
        .expect("scratch PMS should have a provider manifest");
    assert!(provider_manifest.contains("HamsterPmsProbeServiceProvider"));
    let app = app_path.to_string_lossy().replace('\\', "/");
    let php_ini = std::env::var("HAMSTER_LARAVEL_TEST_PHP_INI")
        .unwrap_or_else(|_| format!("{app}/php.ini"))
        .replace('\\', "/");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[
            ("HAMSTER_LARAVEL_PATH", app.as_str()),
            ("PHPRC", php_ini.as_str()),
            ("APP_ENV", "production"),
            ("APP_DEBUG", "false"),
            ("PERF_MONITOR_ENABLED", "false"),
            ("HAMSTER_PMS_PROBE", "1"),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;
    let authority = format!("localhost:{}", server.address.port());
    let request = format!(
        "GET http://{authority}/__hamster/metadata HTTP/1.0\r\nHost: wrong.test\r\nX-Forwarded-For: 203.0.113.9\r\nX-Forwarded-Host: attacker.test\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Port: 443\r\nConnection: close\r\n\r\n"
    );
    let (response, client) = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 2),
        request.as_bytes(),
    )
    .await;
    let (head, body) = split_response(&response);
    assert!(
        head.starts_with(b"HTTP/1.0 200 OK\r\n"),
        "PMS metadata probe failed: {}",
        String::from_utf8_lossy(&response)
    );
    assert_json_string(body, "remote_addr", "127.0.0.2");
    assert_json_string(body, "remote_port", &client.port().to_string());
    assert_json_string(body, "server_addr", "127.0.0.1");
    assert_json_string(body, "server_port", &server.address.port().to_string());
    assert_json_string(body, "server_protocol", "HTTP/1.0");
    assert_json_string(body, "server_name", "localhost");
    assert_json_string(body, "http_host", &authority);
    assert_json_string(body, "request_scheme", "http");
    assert_json_string(body, "https", "off");
    assert_json_string(body, "client_ip", "127.0.0.2");
    assert_json_string(body, "host", "localhost");
    assert_json_string(body, "scheme", "http");
    assert_eq!(json_number(body, "port"), server.address.port() as u64);
    assert_json_string(body, "forwarded_for", "203.0.113.9");
    let first_pid = json_number(body, "pid");

    let (second, _) = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 3),
        b"GET /__hamster/metadata HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (_, second_body) = split_response(&second);
    assert_json_string(second_body, "remote_addr", "127.0.0.3");
    assert_json_string(second_body, "server_protocol", "HTTP/1.1");
    assert_eq!(json_number(second_body, "pid"), first_pid);
    assert!(
        !std::str::from_utf8(second_body)
            .unwrap()
            .contains("127.0.0.2")
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires an Octane-enabled scratch PMS with HamsterPmsProbeServiceProvider"]
async fn laravel_fresh_benchmark_baseline_serves_controller_requests() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_fresh_worker.php");
    let app = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the prepared scratch PMS"),
    )
    .to_string_lossy()
    .replace('\\', "/");
    let php_ini = std::env::var("HAMSTER_LARAVEL_TEST_PHP_INI")
        .unwrap_or_else(|_| format!("{app}/php.ini"))
        .replace('\\', "/");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[
            ("HAMSTER_LARAVEL_PATH", app.as_str()),
            ("PHPRC", php_ini.as_str()),
            ("APP_ENV", "production"),
            ("APP_DEBUG", "false"),
            ("PERF_MONITOR_ENABLED", "false"),
            ("HAMSTER_PMS_PROBE", "1"),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;
    let initial = server.runtime.php_worker().unwrap().snapshot().workers[0].clone();

    for _ in 0..2 {
        let response = server.request(&close_request("/__hamster/light")).await;
        let (headers, body) = split_response(&response);
        assert!(
            headers.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "fresh baseline failed: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(json_number(body, "pid"), initial.pid.unwrap() as u64);
        assert_eq!(json_number(body, "controller_calls"), 1);
    }

    let after = server.runtime.php_worker().unwrap().snapshot().workers[0].clone();
    assert_eq!(after.generation, initial.generation);
    assert_eq!(after.pid, initial.pid);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires an Octane-enabled scratch PMS with HamsterPmsProbeServiceProvider"]
async fn pms_w1_isolates_auth_and_retires_after_cleanup_failure() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_worker.php");
    let app = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the prepared scratch PMS"),
    );
    let provider_manifest = std::fs::read_to_string(app.join("bootstrap/providers.php"))
        .expect("scratch PMS should have a provider manifest");
    assert!(
        provider_manifest.contains("HamsterPmsProbeServiceProvider"),
        "scratch PMS must register the checked-in probe provider"
    );
    let app = app.to_string_lossy().replace('\\', "/");
    let php_ini = std::env::var("HAMSTER_LARAVEL_TEST_PHP_INI")
        .unwrap_or_else(|_| format!("{app}/php.ini"))
        .replace('\\', "/");
    let failure_marker = marker_path("pms-cleanup-failure");
    let failure_marker_env = failure_marker.to_string_lossy().replace('\\', "/");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[
            ("HAMSTER_LARAVEL_PATH", app.as_str()),
            ("PHPRC", php_ini.as_str()),
            ("SESSION_DRIVER", "file"),
            ("CACHE_STORE", "array"),
            ("QUEUE_CONNECTION", "sync"),
            ("APP_ENV", "production"),
            ("APP_DEBUG", "false"),
            ("PERF_MONITOR_ENABLED", "false"),
            ("HAMSTER_PMS_PROBE", "1"),
            ("HAMSTER_PMS_FAILURE_MARKER", failure_marker_env.as_str()),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();

    let seed = server.request(&close_request("/__hamster/auth-seed")).await;
    let (seed_headers, seed_body) = split_response(&seed);
    assert!(
        seed_headers.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "PMS auth seed failed: {}",
        String::from_utf8_lossy(&seed)
    );
    let user_id = json_number(seed_body, "id");
    let cookies = response_cookie_header(seed_headers);
    assert!(!cookies.is_empty(), "auth seed must issue a session cookie");

    for _ in 0..50 {
        let authenticated = server
            .request(
                format!(
                    "GET /__hamster/auth-state HTTP/1.1\r\nHost: localhost\r\nCookie: {cookies}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await;
        let (headers, body) = split_response(&authenticated);
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(json_number(body, "id"), user_id);

        let anonymous = server
            .request(&close_request("/__hamster/auth-state"))
            .await;
        let (headers, _) = split_response(&anonymous);
        assert!(
            headers.starts_with(b"HTTP/1.1 302 Found\r\n"),
            "anonymous request inherited authentication: {}",
            String::from_utf8_lossy(&anonymous)
        );
        assert!(
            String::from_utf8_lossy(headers).lines().any(|line| line
                .to_ascii_lowercase()
                .starts_with("location: http://localhost/login")),
            "anonymous request did not redirect to login: {}",
            String::from_utf8_lossy(headers)
        );
    }

    for _ in 0..2 {
        let light = server.request(&close_request("/__hamster/light")).await;
        let (headers, body) = split_response(&light);
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(json_number(body, "pid"), initial.pid.unwrap() as u64);
        assert_eq!(
            json_number(body, "controller_calls"),
            1,
            "the route must not retain its controller instance"
        );
    }

    let before_failure = pool.snapshot().workers[0].clone();
    assert_eq!(before_failure.generation, initial.generation);
    assert_eq!(before_failure.pid, initial.pid);
    let failed = server
        .request(
            b"GET /__hamster/fail-cleanup HTTP/1.1\r\nHost: localhost\r\nX-Hamster-Fail-Cleanup: 1\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(
        failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"),
        "cleanup failure must withhold the staged response: {}",
        String::from_utf8_lossy(&failed)
    );
    assert_eq!(
        marker_lines(&failure_marker),
        ["executed"],
        "a request which reached PHP must execute exactly once"
    );

    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recovered.workers[0].generation, initial.generation + 1);
    assert_ne!(recovered.workers[0].pid, initial.pid);
    let healthy = server.request(&close_request("/__hamster/light")).await;
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(marker_lines(&failure_marker), ["executed"]);

    let _ = tokio::fs::remove_file(failure_marker).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires an Octane-enabled scratch PMS with HamsterPmsProbeServiceProvider"]
async fn pms_w4_isolates_controllers_and_recovers_only_failed_slot() {
    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_worker.php");
    let app = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the prepared scratch PMS"),
    );
    let provider_manifest = std::fs::read_to_string(app.join("bootstrap/providers.php"))
        .expect("scratch PMS should have a provider manifest");
    assert!(
        provider_manifest.contains("HamsterPmsProbeServiceProvider"),
        "scratch PMS must register the checked-in probe provider"
    );
    let app = app.to_string_lossy().replace('\\', "/");
    let php_ini = std::env::var("HAMSTER_LARAVEL_TEST_PHP_INI")
        .unwrap_or_else(|_| format!("{app}/php.ini"))
        .replace('\\', "/");
    let failure_marker = marker_path("pms-w4-cleanup-failure");
    let failure_marker_env = failure_marker.to_string_lossy().replace('\\', "/");
    let runtime = Arc::new(php_runtime_with_env(
        4,
        &script,
        &[
            ("HAMSTER_LARAVEL_PATH", app.as_str()),
            ("PHPRC", php_ini.as_str()),
            ("SESSION_DRIVER", "file"),
            ("CACHE_STORE", "array"),
            ("QUEUE_CONNECTION", "sync"),
            ("APP_ENV", "production"),
            ("APP_DEBUG", "false"),
            ("PERF_MONITOR_ENABLED", "false"),
            ("HAMSTER_PMS_PROBE", "1"),
            ("HAMSTER_PMS_FAILURE_MARKER", failure_marker_env.as_str()),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot();
    assert_eq!(initial.ready_workers, 4);
    assert!(all_workers_live(&initial));

    let exercise_all_workers = |address| async move {
        let mut tasks = Vec::new();
        for _ in 0..4 {
            tasks.push(tokio::spawn(async move {
                request_at(
                    address,
                    b"GET /__hamster/light HTTP/1.1\r\nHost: localhost\r\nX-Hamster-Probe-Delay-Ms: 100\r\nConnection: close\r\n\r\n",
                )
                .await
            }));
        }

        let mut pids = HashSet::new();
        for task in tasks {
            let response = task.await.unwrap();
            let (headers, body) = split_response(&response);
            assert!(
                headers.starts_with(b"HTTP/1.1 200 OK\r\n"),
                "PMS W4 request failed: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(
                json_number(body, "controller_calls"),
                1,
                "each Octane sandbox must receive a fresh route controller"
            );
            pids.insert(json_number(body, "pid") as u32);
        }
        pids
    };

    let initial_pids = initial
        .workers
        .iter()
        .map(|worker| worker.pid.unwrap())
        .collect::<HashSet<_>>();
    assert_eq!(exercise_all_workers(server.address).await, initial_pids);
    assert_eq!(exercise_all_workers(server.address).await, initial_pids);

    let failed = server
        .request(
            b"GET /__hamster/fail-cleanup HTTP/1.1\r\nHost: localhost\r\nX-Hamster-Fail-Cleanup: 1\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(
        failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"),
        "cleanup failure must withhold the staged response: {}",
        String::from_utf8_lossy(&failed)
    );
    assert_eq!(marker_lines(&failure_marker), ["executed"]);

    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 4
            && snapshot
                .workers
                .iter()
                .zip(&initial.workers)
                .filter(|(current, previous)| current.generation > previous.generation)
                .count()
                == 1
    })
    .await;
    assert!(all_workers_live(&recovered));
    let changed_slots = recovered
        .workers
        .iter()
        .zip(&initial.workers)
        .filter(|(current, previous)| current.generation != previous.generation)
        .collect::<Vec<_>>();
    assert_eq!(changed_slots.len(), 1);
    let (replacement, failed_worker) = changed_slots[0];
    assert_eq!(replacement.generation, failed_worker.generation + 1);
    assert_ne!(replacement.pid, failed_worker.pid);
    for (current, previous) in recovered.workers.iter().zip(&initial.workers) {
        if current.slot != replacement.slot {
            assert_eq!(current.generation, previous.generation);
            assert_eq!(current.pid, previous.pid);
        }
    }

    let recovered_pids = recovered
        .workers
        .iter()
        .map(|worker| worker.pid.unwrap())
        .collect::<HashSet<_>>();
    assert_eq!(exercise_all_workers(server.address).await, recovered_pids);
    assert_eq!(marker_lines(&failure_marker), ["executed"]);

    let _ = tokio::fs::remove_file(failure_marker).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn protocol_v1_worker_is_rejected_before_the_server_binds() {
    let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/protocol_v1_ready.php");
    let runtime = Arc::new(php_runtime_with_script_at(
        1,
        8,
        None,
        None,
        &script,
        &address.to_string(),
    ));

    let error = tokio::time::timeout(Duration::from_secs(5), server::run(Arc::clone(&runtime)))
        .await
        .expect("version mismatch startup must be bounded")
        .expect_err("a protocol-v1 worker must not start a protocol-v2 server");
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("unsupported READY protocol version 1"),
        "{diagnostic}"
    );
    assert!(
        TcpStream::connect(address).await.is_err(),
        "listener must never bind after a worker version mismatch"
    );
    assert!(runtime.php_worker().unwrap().pids().is_empty());
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn protocol_v3_delivers_socket_http_and_effective_authority_metadata() {
    let server = TestServer::start().await;
    let absolute_authority = format!("localhost:{}", server.address.port());
    let request = format!(
        "GET http://{absolute_authority}/metadata HTTP/1.0\r\nHost: wrong.test\r\nX-Forwarded-For: 203.0.113.9\r\nX-Forwarded-Proto: https\r\nX-Real-IP: 198.51.100.7\r\nConnection: close\r\n\r\n"
    );
    let (response, client) = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 2),
        request.as_bytes(),
    )
    .await;
    let (head, body) = split_response(&response);
    assert!(head.starts_with(b"HTTP/1.0 200 OK\r\n"));
    assert_json_string(body, "http_version", "HTTP/1.0");
    assert_json_string(body, "peer_addr", "127.0.0.2");
    assert_eq!(json_number(body, "peer_port"), client.port() as u64);
    assert_json_string(body, "local_addr", "127.0.0.1");
    assert_eq!(
        json_number(body, "local_port"),
        server.address.port() as u64
    );
    assert_json_string(
        body,
        "uri",
        &format!("http://{absolute_authority}/metadata"),
    );
    let text = std::str::from_utf8(body).unwrap();
    assert!(
        text.contains(&format!("[\"host\",\"{absolute_authority}\"]")),
        "effective authority did not replace the conflicting Host: {text}"
    );
    assert!(text.contains("[\"x-forwarded-for\",\"203.0.113.9\"]"));
    assert!(text.contains("[\"x-real-ip\",\"198.51.100.7\"]"));

    // W1 forces both connections through the same persistent PHP process.
    // The second request must replace, not inherit, the first connection's
    // transport identity.
    let (second, second_client) = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 3),
        b"GET /metadata HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (_, second_body) = split_response(&second);
    assert_json_string(second_body, "http_version", "HTTP/1.1");
    assert_json_string(second_body, "peer_addr", "127.0.0.3");
    assert_eq!(
        json_number(second_body, "peer_port"),
        second_client.port() as u64
    );
    assert!(
        !std::str::from_utf8(second_body)
            .unwrap()
            .contains("127.0.0.2")
    );
    assert_eq!(json_number(body, "pid"), json_number(second_body, "pid"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn keep_alive_requests_reuse_connection_metadata_without_request_state_leak() {
    let server = TestServer::start().await;
    let socket = TcpSocket::new_v4().unwrap();
    socket
        .bind("127.0.0.2:0".parse().unwrap())
        .expect("loopback source should bind");
    let mut stream = socket.connect(server.address).await.unwrap();
    let client = stream.local_addr().unwrap();

    stream
        .write_all(
            b"GET /metadata HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.1\r\n\r\n",
        )
        .await
        .unwrap();
    let first = read_one_response(&mut stream).await;
    let (_, first_body) = split_response(&first);
    assert_json_string(first_body, "peer_addr", "127.0.0.2");
    assert_eq!(json_number(first_body, "peer_port"), client.port() as u64);
    assert!(
        std::str::from_utf8(first_body)
            .unwrap()
            .contains("203.0.113.1")
    );

    stream
        .write_all(
            b"GET /metadata HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 198.51.100.2\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let second = read_one_response(&mut stream).await;
    let (_, second_body) = split_response(&second);
    assert_json_string(second_body, "peer_addr", "127.0.0.2");
    assert_eq!(json_number(second_body, "peer_port"), client.port() as u64);
    let text = std::str::from_utf8(second_body).unwrap();
    assert!(text.contains("198.51.100.2"));
    assert!(!text.contains("203.0.113.1"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn independent_pools_do_not_share_request_metadata() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let runtime = Arc::new(metadata_multi_pool_runtime(&script));
    let server = TestServer::start_runtime(runtime).await;
    assert_eq!(server.runtime.php_workers().len(), 2);

    let one = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 2),
        b"GET /metadata HTTP/1.1\r\nHost: one.test\r\nConnection: close\r\n\r\n",
    );
    let two = request_at_from(
        server.address,
        Ipv4Addr::new(127, 0, 0, 3),
        b"GET /metadata HTTP/1.1\r\nHost: two.test\r\nConnection: close\r\n\r\n",
    );
    let ((one, one_client), (two, two_client)) = tokio::join!(one, two);
    let (_, one_body) = split_response(&one);
    let (_, two_body) = split_response(&two);
    assert_json_string(one_body, "peer_addr", "127.0.0.2");
    assert_eq!(json_number(one_body, "peer_port"), one_client.port() as u64);
    assert_json_string(two_body, "peer_addr", "127.0.0.3");
    assert_eq!(json_number(two_body, "peer_port"), two_client.port() as u64);
    assert_ne!(json_number(one_body, "pid"), json_number(two_body, "pid"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn one_php_process_bootstraps_once_across_one_hundred_http_requests() {
    let server = TestServer::start().await;
    let mut pid = None;
    for expected_request in 1..=100 {
        let response = server
            .request(b"GET /state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await;
        let (headers, body) = split_response(&response);
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        let current_pid = json_number(body, "pid");
        assert_eq!(*pid.get_or_insert(current_pid), current_pid);
        assert_eq!(json_number(body, "bootstrap"), 1);
        assert_eq!(json_number(body, "request"), expected_request);
    }
    assert_eq!(
        server.runtime.php_worker().unwrap().pids(),
        pid.map(|pid| vec![pid as u32]).unwrap_or_default()
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn http_mapping_preserves_query_headers_body_and_custom_response() {
    let server = TestServer::start().await;
    let inspect = server
        .request(
            b"POST /inspect?name=hamster HTTP/1.1\r\nHost: localhost\r\nX-Repeat: one\r\nX-Repeat: two\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
        )
        .await;
    let (headers, body) = split_response(&inspect);
    assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let body = std::str::from_utf8(body).unwrap();
    assert!(body.contains("\"method\":\"POST\""));
    assert!(body.contains("\"uri\":\"/inspect?name=hamster\""));
    assert!(body.contains("[\"x-repeat\",\"one\"]"));
    assert!(body.contains("[\"x-repeat\",\"two\"]"));
    assert!(body.contains("\"body\":\"data\""));

    let binary = [0_u8, 10, 13, 128, 255];
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        binary.len()
    )
    .into_bytes();
    request.extend_from_slice(&binary);
    let response = server.request(&request).await;
    let (headers, body) = split_response(&response);
    assert!(headers.starts_with(b"HTTP/1.1 201 Created\r\n"));
    assert!(
        headers
            .windows(b"x-php-worker: persistent".len())
            .any(|window| window.eq_ignore_ascii_case(b"x-php-worker: persistent"))
    );
    assert_eq!(body, binary);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn absolute_form_rewrites_host_before_persistent_application_dispatch() {
    let server = TestServer::start().await;
    let response = server
        .request(
            b"GET http://localhost/inspect?authority=target HTTP/1.1\r\nHost: ignored.test:9000\r\nConnection: close\r\n\r\n",
        )
        .await;

    let (headers, body) = split_response(&response);
    assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let body = std::str::from_utf8(body).unwrap();
    assert!(body.contains("\"uri\":\"http://localhost/inspect?authority=target\""));
    assert!(body.contains("[\"host\",\"localhost\"]"));
    assert!(!body.contains("ignored.test"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn incomplete_php_body_times_out_before_backend_dispatch() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let runtime = Arc::new(php_runtime_with_body_policy(1, 8, 1, 1, &script));
    let server = TestServer::start_runtime(runtime).await;
    let mut slow = TcpStream::connect(server.address).await.unwrap();
    slow.write_all(
        b"POST /inspect HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n",
    )
        .await
        .unwrap();
    assert!(
        read_response_head(&mut slow)
            .await
            .starts_with(b"HTTP/1.1 100 Continue\r\n")
    );
    slow.write_all(b"x").await.unwrap();

    let address = server.address;
    let state_task =
        tokio::spawn(async move { request_at(address, &close_request("/state")).await });
    let mut slow_response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), slow.read_to_end(&mut slow_response))
        .await
        .expect("incomplete body should be bounded")
        .expect("read timed-out body response");
    assert!(
        slow_response.starts_with(b"HTTP/1.1 408 Request Timeout\r\n"),
        "incomplete body should receive 408, got {:?}",
        String::from_utf8_lossy(&slow_response)
    );

    let state = state_task.await.expect("state request task");
    let (_, body) = split_response(&state);
    assert_eq!(json_number(body, "request"), 1);
    drop(slow);
    server.stop().await;
}

fn php_runtime_with_body_budget(
    workers: usize,
    queue_capacity: usize,
    max_connections: usize,
    max_body_memory: usize,
    script: &Path,
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\nmax_connections = {max_connections}\nbody_read_timeout = 10\nmax_body_memory = {max_body_memory}\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\n",
        toml_literal(&executable),
        toml_literal(script)
    );
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP body budget config should compile")
}

fn post_request(path: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

/// The aggregate ceiling must refuse work before PHP, never during it.
///
/// A budget rejection is only sound if it happens on the admission side of the
/// worker transaction. This asserts the consequence directly: the number of
/// requests the worker reports having executed equals the number of requests
/// that were answered 2xx, so nothing refused for memory was also started.
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn an_over_budget_body_is_refused_before_the_worker_sees_it() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    // One maximum body is the whole budget, so a single in-flight 1 MiB body
    // saturates it and anything concurrent must be refused.
    let runtime = Arc::new(php_runtime_with_body_budget(
        1,
        8,
        8,
        hamster_wheel::php_worker::MAX_REQUEST_BODY,
        &script,
    ));
    let server = TestServer::start_runtime(runtime).await;
    let address = server.address;

    // Baseline, before any load: /state itself is a request, so record it.
    let baseline = {
        let state = request_at(address, &close_request("/state")).await;
        let (_, body) = split_response(&state);
        json_number(body, "request")
    };

    // Hold the whole budget inside a worker that is deliberately slow.
    let large = vec![b'x'; hamster_wheel::php_worker::MAX_REQUEST_BODY];
    let holder =
        tokio::spawn(
            async move { request_at(address, &post_request("/delay?ms=1200", &large)).await },
        );
    // Let the holder's body be read and charged before competing for it.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let refused = request_at(address, &post_request("/echo", b"small")).await;
    assert!(
        refused.starts_with(b"HTTP/1.1 503 "),
        "a body that does not fit the aggregate ceiling should be refused with 503, got {:?}",
        String::from_utf8_lossy(&refused[..refused.len().min(64)])
    );
    // Both 503s a PHP location can produce look identical in the status line.
    // The body is what tells an operator which limit they hit.
    let (_, refused_body) = split_response(&refused);
    assert_eq!(
        refused_body, b"Request Body Memory Budget",
        "a budget refusal must be distinguishable from a full worker queue"
    );

    let held = holder.await.expect("holder task");
    assert!(
        held.starts_with(b"HTTP/1.1 200 "),
        "the admitted body should still be served, got {:?}",
        String::from_utf8_lossy(&held[..held.len().min(64)])
    );

    // The budget is returned, so the same small body now succeeds.
    let accepted = request_at(address, &post_request("/echo", b"small")).await;
    assert!(
        accepted.starts_with(b"HTTP/1.1 201 "),
        "the ceiling must release, got {:?}",
        String::from_utf8_lossy(&accepted[..accepted.len().min(64)])
    );

    let state = request_at(address, &close_request("/state")).await;
    let (_, body) = split_response(&state);
    // Since the baseline /state: the delayed POST, the accepted POST and this
    // /state itself. The refused POST must not appear.
    assert_eq!(
        json_number(body, "request") - baseline,
        3,
        "the worker must not have executed the refused request"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn php_handler_preserves_keep_alive() {
    let server = TestServer::start().await;
    let response = server
        .request(
            b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\nGET /json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
    let text = String::from_utf8(response).unwrap();
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 2);
    assert!(text.contains("Hello from PHPHTTP/1.1 200 OK"));
    assert!(text.ends_with("{\"message\":\"Hello from PHP\"}"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn worker_protocol_failure_respawns_without_retry() {
    let server = TestServer::start().await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    let execution_marker = marker_path("malformed-execution");
    let path = format!(
        "/malformed?execution_marker={}&execution_id=malformed-once",
        url_encode(&execution_marker)
    );

    // Category B: 0.3.0 supersedes permanent no-respawn while preserving
    // the correctness rule that the possibly-started request is never retried.
    let failed = server.request(&close_request(&path)).await;
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recovered.workers[0].slot, initial.slot);
    assert_eq!(recovered.workers[0].generation, initial.generation + 1);
    assert_ne!(recovered.workers[0].pid, initial.pid);
    assert_eq!(marker_lines(&execution_marker), ["malformed-once"]);

    let response = server.request(&close_request("/hello")).await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let _ = tokio::fs::remove_file(execution_marker).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn concurrent_http_requests_are_serialized_without_protocol_corruption() {
    let server = TestServer::start().await;
    let mut tasks = Vec::new();
    for value in 0_u8..16 {
        let address = server.address;
        tasks.push(tokio::spawn(async move {
            let body = [value, 0, 255, value];
            let mut request = format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            request.extend_from_slice(&body);
            (body, request_at(address, &request).await)
        }));
    }

    for task in tasks {
        let (expected, response) = task.await.unwrap();
        let (headers, body) = split_response(&response);
        assert!(headers.starts_with(b"HTTP/1.1 201 Created\r\n"));
        assert_eq!(body, expected);
    }

    let response = server
        .request(b"GET /state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    let (_, body) = split_response(&response);
    assert_eq!(json_number(body, "request"), 17);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn parallel_pool_bootstraps_once_and_keeps_per_worker_state() {
    let server = TestServer::start_with(4, 32).await;
    let mut first = Vec::new();
    for _ in 0..4 {
        let address = server.address;
        first.push(tokio::spawn(async move {
            request_at(address, &close_request("/state-delay?ms=75")).await
        }));
    }
    let mut counters = HashMap::new();
    for task in first {
        let response = task.await.unwrap();
        let (headers, body) = split_response(&response);
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(json_number(body, "bootstrap"), 1);
        assert_eq!(json_number(body, "request"), 1);
        counters.insert(json_number(body, "pid"), 1_u64);
    }
    assert_eq!(counters.len(), 4);

    let mut second = Vec::new();
    for _ in 0..4 {
        let address = server.address;
        second.push(tokio::spawn(async move {
            request_at(address, &close_request("/state-delay?ms=75")).await
        }));
    }
    for task in second {
        let response = task.await.unwrap();
        let (_, body) = split_response(&response);
        let pid = json_number(body, "pid");
        assert!(counters.contains_key(&pid));
        assert_eq!(json_number(body, "bootstrap"), 1);
        assert_eq!(json_number(body, "request"), 2);
    }
    assert_eq!(server.runtime.php_worker().unwrap().pids().len(), 4);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn requests_above_worker_count_queue_and_preserve_responses() {
    let server = TestServer::start_with(2, 32).await;
    let mut tasks = Vec::new();
    for value in 0_u8..16 {
        let address = server.address;
        tasks.push(tokio::spawn(async move {
            let body = [value, 0, 255, value];
            let mut request = format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            request.extend_from_slice(&body);
            (body, request_at(address, &request).await)
        }));
    }
    for task in tasks {
        let (expected, response) = task.await.unwrap();
        let (headers, body) = split_response(&response);
        assert!(headers.starts_with(b"HTTP/1.1 201 Created\r\n"));
        assert_eq!(body, expected);
    }
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn full_queue_returns_503_and_pool_recovers() {
    let server = TestServer::start_with(1, 1).await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let address = server.address;
        tasks.push(tokio::spawn(async move {
            request_at(address, &close_request("/delay?ms=200")).await
        }));
    }
    let mut ok = 0;
    let mut unavailable = 0;
    for task in tasks {
        let response = task.await.unwrap();
        if response.starts_with(b"HTTP/1.1 200 OK\r\n") {
            ok += 1;
        } else if response.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n") {
            unavailable += 1;
        } else {
            panic!(
                "unexpected overload response: {}",
                String::from_utf8_lossy(&response)
            );
        }
    }
    assert!(ok >= 1);
    assert!(unavailable >= 1);
    let verification = server.request(&close_request("/hello")).await;
    assert!(verification.starts_with(b"HTTP/1.1 200 OK\r\n"));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn disconnect_after_transaction_start_does_not_poison_worker() {
    let server = TestServer::start_with(1, 8).await;
    let start = marker_path("cancel-start");
    let done = marker_path("cancel-done");
    let path = format!(
        "/delay?ms=150&start_marker={}&done_marker={}",
        url_encode(&start),
        url_encode(&done)
    );
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream.write_all(&close_request(&path)).await.unwrap();
    let original_pid = wait_for_marker(&start).await.parse::<u32>().unwrap();
    drop(stream);
    assert_eq!(
        wait_for_marker(&done).await.parse::<u32>().unwrap(),
        original_pid
    );

    let response = server.request(&close_request("/state")).await;
    let (headers, body) = split_response(&response);
    assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(json_number(body, "pid") as u32, original_pid);
    assert_eq!(json_number(body, "request"), 2);
    let _ = tokio::fs::remove_file(start).await;
    let _ = tokio::fs::remove_file(done).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn queued_disconnect_is_skipped_before_php_execution() {
    let server = TestServer::start_with(1, 8).await;
    let active_start = marker_path("active-start");
    let active_done = marker_path("active-done");
    let skipped_marker = marker_path("queued-skipped");
    let active_path = format!(
        "/delay?ms=200&start_marker={}&done_marker={}",
        url_encode(&active_start),
        url_encode(&active_done)
    );
    let address = server.address;
    let active =
        tokio::spawn(async move { request_at(address, &close_request(&active_path)).await });
    wait_for_marker(&active_start).await;

    let queued_path = format!("/state?start_marker={}", url_encode(&skipped_marker));
    let mut queued = TcpStream::connect(server.address).await.unwrap();
    queued
        .write_all(&close_request(&queued_path))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(queued);
    wait_for_marker(&active_done).await;
    assert!(active.await.unwrap().starts_with(b"HTTP/1.1 200 OK\r\n"));

    let response = server.request(&close_request("/state")).await;
    let (_, body) = split_response(&response);
    assert_eq!(json_number(body, "request"), 2);
    assert!(!skipped_marker.exists());
    let _ = tokio::fs::remove_file(active_start).await;
    let _ = tokio::fs::remove_file(active_done).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn one_failed_worker_serves_degraded_then_recovers_target() {
    let server = TestServer::start_with(2, 16).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot();
    assert_eq!(initial.ready_workers, 2);
    let start = marker_path("degraded-start");
    let execution = marker_path("degraded-execution");
    let path = format!(
        "/crash?ms=200&start_marker={}&execution_marker={}&execution_id=crash-once",
        url_encode(&start),
        url_encode(&execution)
    );
    let address = server.address;
    let crashing = tokio::spawn(async move { request_at(address, &close_request(&path)).await });
    let failed_pid = wait_for_marker(&start).await.parse::<u32>().unwrap();
    let failed_slot = initial
        .workers
        .iter()
        .find(|worker| worker.pid == Some(failed_pid))
        .expect("crashing PID must identify its stable slot")
        .slot;
    let unaffected = server.request(&close_request("/state")).await;
    let (headers, body) = split_response(&unaffected);
    assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_ne!(json_number(body, "pid") as u32, failed_pid);

    // Category B: 0.3.0 replaces permanent W2 -> W1 degradation with
    // W2 -> W1 -> W2 recovery; the crashing request still fails exactly once.
    let failed = crashing.await.unwrap();
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 2
            && snapshot.workers[failed_slot].generation > initial.workers[failed_slot].generation
    })
    .await;
    assert_eq!(recovered.target_workers, 2);
    assert_eq!(recovered.workers[failed_slot].slot, failed_slot);
    assert_eq!(
        recovered.workers[failed_slot].generation,
        initial.workers[failed_slot].generation + 1
    );
    assert_ne!(
        recovered.workers[failed_slot].pid,
        initial.workers[failed_slot].pid
    );
    let unaffected_slot = 1 - failed_slot;
    assert_eq!(
        recovered.workers[unaffected_slot].generation,
        initial.workers[unaffected_slot].generation
    );
    assert_eq!(
        recovered.workers[unaffected_slot].pid,
        initial.workers[unaffected_slot].pid
    );
    assert_eq!(marker_lines(&execution), ["crash-once"]);
    let _ = tokio::fs::remove_file(start).await;
    let _ = tokio::fs::remove_file(execution).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn zero_workers_recover_without_idle_channel_deadlock() {
    let startup_marker = marker_path("delayed-replacement-startup");
    let mut script = marker_path("delayed-replacement-script");
    script.set_extension("php");
    let worker_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/php-worker/worker.php")
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "\\'");
    let startup_literal = startup_marker
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "\\'");
    std::fs::write(
        &script,
        format!(
            "<?php\n$marker = '{startup_literal}';\n$count = is_file($marker) ? (int) file_get_contents($marker) : 0;\nfile_put_contents($marker, (string) ($count + 1), LOCK_EX);\nif ($count > 0) {{ usleep(400000); }}\nrequire '{worker_script}';\n"
        ),
    )
    .unwrap();
    let runtime = Arc::new(php_runtime_with_script(1, 8, None, None, &script));
    let server = TestServer::start_runtime(runtime).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial_generation = pool.snapshot().workers[0].generation;
    let start = marker_path("zero-start");
    let crash_path = format!("/crash?ms=100&start_marker={}", url_encode(&start));
    let address = server.address;
    let crashing =
        tokio::spawn(async move { request_at(address, &close_request(&crash_path)).await });
    wait_for_marker(&start).await;
    let address = server.address;
    let pending = tokio::spawn(async move { request_at(address, &close_request("/hello")).await });

    // Category B: zero live workers is transient in 0.3.0 while the pending
    // replacement owns idle_tx; correctness still requires bounded responses.
    let failed = tokio::time::timeout(Duration::from_secs(2), crashing)
        .await
        .expect("crashing request must not hang")
        .unwrap();
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    let recovering = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 0
            && snapshot.workers[0].generation > initial_generation
            && snapshot.workers[0].state == WorkerLifecycleState::Bootstrapping
    })
    .await;
    assert_eq!(recovering.workers[0].generation, initial_generation + 1);
    let new_request = tokio::time::timeout(
        Duration::from_secs(1),
        server.request(&close_request("/hello")),
    )
    .await
    .expect("new request must fail fast");
    assert!(new_request.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
    let pending = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .expect("accepted request must resolve after replacement READY")
        .unwrap();
    assert!(pending.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let recovered = wait_for_pool(pool, |snapshot| snapshot.ready_workers == 1).await;
    assert_eq!(recovered.workers[0].generation, initial_generation + 1);
    let verification = server.request(&close_request("/hello")).await;
    assert!(verification.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let _ = tokio::fs::remove_file(start).await;
    server.stop().await;
    let _ = tokio::fs::remove_file(startup_marker).await;
    let _ = tokio::fs::remove_file(script).await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn respawn_after_idle_crash_restores_target() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    kill_process(initial.pid.unwrap()).await;

    // 0.4 supersedes the 0.3 expectation of a 502 here. The process was reaped
    // before a byte of the request frame was written, which proves PHP never
    // saw it, so the job is queued again instead of failing the client. The
    // execution marker is what keeps that honest: a replay writes a second
    // line, and this asserts there is exactly one.
    let execution = marker_path("idle-crash-redispatch");
    let path = format!(
        "/state?execution_marker={}&execution_id=redispatched-once",
        url_encode(&execution)
    );
    let detecting = server.request(&close_request(&path)).await;
    let (detecting_headers, detecting_body) = split_response(&detecting);
    assert!(
        detecting_headers.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "a provably unstarted request must be redispatched, not failed"
    );
    assert_ne!(
        json_number(detecting_body, "pid") as u32,
        initial.pid.unwrap()
    );
    assert_eq!(marker_lines(&execution), ["redispatched-once"]);
    let _ = tokio::fs::remove_file(execution).await;
    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recovered.target_workers, 1);
    assert_eq!(recovered.workers[0].slot, initial.slot);
    assert_eq!(recovered.workers[0].generation, initial.generation + 1);
    assert_ne!(recovered.workers[0].pid, initial.pid);
    assert!(
        server
            .request(&close_request("/hello"))
            .await
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn respawn_after_inflight_crash_restores_target() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    let started = marker_path("inflight-respawn-started");
    let path = format!("/crash?ms=25&start_marker={}", url_encode(&started));
    let failed = server.request(&close_request(&path)).await;
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recovered.workers[0].generation, initial.generation + 1);
    assert_ne!(recovered.workers[0].pid, initial.pid);
    assert!(started.exists());
    let _ = tokio::fs::remove_file(started).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn possibly_started_request_is_never_retried() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial_generation = pool.snapshot().workers[0].generation;
    let execution = marker_path("possibly-started-once");
    let path = format!(
        "/crash?execution_marker={}&execution_id=unique-crash",
        url_encode(&execution)
    );
    let failed = server.request(&close_request(&path)).await;
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial_generation
    })
    .await;
    assert_eq!(marker_lines(&execution), ["unique-crash"]);
    assert!(
        server
            .request(&close_request("/hello"))
            .await
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    let _ = tokio::fs::remove_file(execution).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn degraded_w4_to_w3_then_recovers() {
    let server = TestServer::start_with(4, 32).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot();
    let start = marker_path("w4-degraded-start");
    let path = format!("/crash?ms=200&start_marker={}", url_encode(&start));
    let address = server.address;
    let crashing = tokio::spawn(async move { request_at(address, &close_request(&path)).await });
    let failed_pid = wait_for_marker(&start).await.parse::<u32>().unwrap();
    let failed_slot = initial
        .workers
        .iter()
        .find(|worker| worker.pid == Some(failed_pid))
        .unwrap()
        .slot;

    let mut unaffected = Vec::new();
    for _ in 0..3 {
        let address = server.address;
        unaffected.push(tokio::spawn(async move {
            request_at(address, &close_request("/state-delay?ms=25")).await
        }));
    }
    for response in unaffected {
        let response = response.await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }
    assert!(
        crashing
            .await
            .unwrap()
            .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
    );
    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 4
            && snapshot.workers[failed_slot].generation > initial.workers[failed_slot].generation
    })
    .await;
    assert_eq!(recovered.target_workers, 4);
    assert_eq!(recovered.workers[failed_slot].slot, failed_slot);
    assert_eq!(
        recovered.workers[failed_slot].generation,
        initial.workers[failed_slot].generation + 1
    );
    let _ = tokio::fs::remove_file(start).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn all_workers_crash_then_pool_recovers() {
    let server = TestServer::start_with(4, 32).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot();
    for pid in initial.workers.iter().filter_map(|worker| worker.pid) {
        kill_process(pid).await;
    }

    // 0.4 made the status of these four requests genuinely non-deterministic,
    // and asserting a specific one was always wrong. Each request is dispatched
    // to a worker that is already dead, which proves PHP never saw it, so it is
    // queued again once; whether the retry finds a replacement ready or another
    // corpse is a race. What must hold is what this asserts: every request is
    // answered, none hangs, no 5xx other than the two the pool can legitimately
    // produce, and the request body executed at most once per client request.
    let execution = marker_path("all-crash-execution");
    let mut detecting = Vec::new();
    for index in 0..4 {
        let address = server.address;
        let path = format!(
            "/hello?execution_marker={}&execution_id=client-{index}",
            url_encode(&execution)
        );
        detecting.push(tokio::spawn(async move {
            request_at(address, &close_request(&path)).await
        }));
    }
    let mut served = 0;
    for response in detecting {
        let response = tokio::time::timeout(Duration::from_secs(10), response)
            .await
            .expect("every request must be answered")
            .unwrap();
        let status = &response[..15.min(response.len())];
        assert!(
            status.starts_with(b"HTTP/1.1 200 ")
                || status.starts_with(b"HTTP/1.1 502 ")
                || status.starts_with(b"HTTP/1.1 503 "),
            "unexpected status: {}",
            String::from_utf8_lossy(status)
        );
        if status.starts_with(b"HTTP/1.1 200 ") {
            served += 1;
        }
    }
    // At most once, not exactly once: a request answered 502 never ran, and a
    // request answered 200 ran exactly one time.
    let executions = marker_lines(&execution);
    let unique: HashSet<&String> = executions.iter().collect();
    assert_eq!(
        executions.len(),
        unique.len(),
        "a request executed more than once: {executions:?}"
    );
    assert_eq!(
        executions.len(),
        served,
        "every served request must have executed exactly once"
    );
    let _ = tokio::fs::remove_file(execution).await;

    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 4 && snapshot.workers.iter().all(|worker| worker.generation > 1)
    })
    .await;
    assert_eq!(recovered.total_respawns, 4);
    assert!(
        server
            .request(&close_request("/hello"))
            .await
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn graceful_recycle_after_max_requests() {
    let server = TestServer::start_with_policy(1, 8, Some(2), None).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    for _ in 0..2 {
        assert!(
            server
                .request(&close_request("/hello"))
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );
    }
    let recycled = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recycled.workers[0].generation, initial.generation + 1);
    assert_eq!(
        recycled.workers[0].spawn_reason,
        WorkerSpawnReason::MaxRequests
    );
    assert_eq!(recycled.workers[0].consecutive_failures, 0);
    assert!(
        server
            .request(&close_request("/hello"))
            .await
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn graceful_recycle_after_max_age() {
    let server = TestServer::start_with_policy(1, 8, None, Some(1)).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    let recycled = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 1 && snapshot.workers[0].generation > initial.generation
    })
    .await;
    assert_eq!(recycled.workers[0].generation, initial.generation + 1);
    assert_eq!(recycled.workers[0].spawn_reason, WorkerSpawnReason::MaxAge);
    assert!(
        server
            .request(&close_request("/hello"))
            .await
            .starts_with(b"HTTP/1.1 200 OK\r\n")
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn max_worker_age_recycles_a_pool_with_no_traffic() {
    let server = TestServer::start_with_policy(1, 8, None, Some(1)).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].generation;

    // No request is ever sent. Before the dispatcher drained idle
    // capabilities continuously, the pool published `workers * 2` tokens that
    // nobody consumed, the next generation blocked inside its publish before
    // arming its own age deadline, and recycling stopped at generation 3.
    let recycled = wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation >= initial + 4
    })
    .await;
    assert!(recycled.total_respawns >= 4);
    assert_eq!(recycled.workers[0].spawn_reason, WorkerSpawnReason::MaxAge);
    assert_eq!(recycled.workers[0].consecutive_failures, 0);

    // Ageing must keep advancing rather than settling on a fixed generation.
    // Serving after an age recycle is asserted by graceful_recycle_after_max_age;
    // this pool recycles every second, so it has no stable window to request in.
    wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation >= recycled.workers[0].generation + 2
    })
    .await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn voluntary_recycles_are_serialized_and_eventual() {
    let server = TestServer::start_with_policy(2, 8, Some(1), None).await;
    let pool = server.runtime.php_worker().unwrap();
    let mut requests = Vec::new();
    for _ in 0..2 {
        let address = server.address;
        requests.push(tokio::spawn(async move {
            request_at(address, &close_request("/delay?ms=50")).await
        }));
    }
    for request in requests {
        assert!(request.await.unwrap().starts_with(b"HTTP/1.1 200 OK\r\n"));
    }
    let recycled = wait_for_pool(pool, |snapshot| {
        snapshot.ready_workers == 2 && snapshot.workers.iter().all(|worker| worker.generation == 2)
    })
    .await;
    assert_eq!(recycled.total_respawns, 2);
    assert!(
        recycled
            .workers
            .iter()
            .all(|worker| worker.spawn_reason == WorkerSpawnReason::MaxRequests)
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn shutdown_during_backoff() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    for expected_generation in [2, 3] {
        assert!(
            server
                .request(&close_request("/crash"))
                .await
                .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
        );
        if expected_generation == 2 {
            wait_for_pool(pool, |snapshot| {
                snapshot.ready_workers == 1 && snapshot.workers[0].generation == expected_generation
            })
            .await;
        }
    }
    wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation == 3
            && snapshot.workers[0].state == WorkerLifecycleState::Backoff
    })
    .await;
    let started = Instant::now();
    server.stop().await;
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe; drives the backoff ladder to exhaustion"]
async fn shutdown_during_prolonged_failure_cooldown() {
    // A slot past MAX_CONSECUTIVE_FAILURES waits out a cooldown of at least
    // 30 seconds before its next replacement. Shutdown must cancel that wait
    // rather than block on it, and must still leave no PHP child behind.
    let server = TestServer::start_with(1, 8).await;
    // Hold the runtime separately: `stop` consumes the server, and these
    // assertions inspect the pool afterwards.
    let runtime = Arc::clone(&server.runtime);
    let pool = runtime.php_worker().unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    while pool.snapshot().workers[0].consecutive_failures <= 10 {
        assert!(
            Instant::now() < deadline,
            "slot never reached the cooldown band; last snapshot: {:#?}",
            pool.snapshot()
        );
        if pool.snapshot().ready_workers == 1 {
            let _ = server.request(&close_request("/crash")).await;
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    let snapshot = wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].state == WorkerLifecycleState::Backoff
    })
    .await;
    assert!(
        snapshot.workers[0].consecutive_failures > 10,
        "the slot must be in the cooldown band, not the fast ladder"
    );

    let started = Instant::now();
    server.stop().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown waited for the cooldown to expire: {elapsed:?}"
    );
    assert!(
        pool.pids().is_empty(),
        "shutdown must leave no PHP child behind"
    );
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe; drives one slot to exhaustion"]
async fn one_bad_slot_does_not_disturb_healthy_slots() {
    // Failure accounting, backoff and cooldown are per slot. Driving one slot
    // into the cooldown band must leave the other three serving normally: no
    // global lifecycle lock, no pool-wide cooldown.
    let server = TestServer::start_with(4, 32).await;
    let runtime = Arc::clone(&server.runtime);
    let pool = runtime.php_worker().unwrap();
    wait_for_pool(pool, |snapshot| snapshot.ready_workers == 4).await;

    let deadline = Instant::now() + Duration::from_secs(150);
    let mut kills = 0;
    while pool.snapshot().workers[0].consecutive_failures <= 10 {
        assert!(
            Instant::now() < deadline,
            "slot 0 never reached the cooldown band; last snapshot: {:#?}",
            pool.snapshot()
        );
        // Target slot 0 specifically, by PID taken from its own snapshot.
        if let Some(pid) = pool.snapshot().workers[0].pid {
            kill_process(pid).await;
            kills += 1;
            // A dead idle worker is only discovered when a job is assigned to
            // it, so drive traffic until the supervisor has observed the exit.
            // Requests that land on healthy slots simply succeed.
            let observed = Instant::now() + Duration::from_secs(20);
            while pool.snapshot().workers[0].pid == Some(pid) {
                assert!(
                    Instant::now() < observed,
                    "slot 0 exit was never observed; last snapshot: {:#?}",
                    pool.snapshot()
                );
                let _ = server.request(&close_request("/")).await;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    assert!(
        kills >= 10,
        "slot 0 must have been killed repeatedly: {kills}"
    );

    let snapshot = pool.snapshot();
    assert!(snapshot.workers[0].consecutive_failures > 10);
    for slot in 1..4 {
        assert_eq!(
            snapshot.workers[slot].consecutive_failures, 0,
            "healthy slot {slot} must not inherit slot 0's failure history"
        );
        assert!(
            matches!(
                snapshot.workers[slot].state,
                WorkerLifecycleState::Ready | WorkerLifecycleState::Busy
            ),
            "healthy slot {slot} must keep serving, was {:?}",
            snapshot.workers[slot].state
        );
    }
    assert!(
        snapshot.ready_workers >= 3,
        "three healthy workers must remain available, saw {}",
        snapshot.ready_workers
    );

    // The pool still serves while slot 0 sits in its cooldown.
    for _ in 0..10 {
        let response = server.request(&close_request("/")).await;
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "healthy slots must keep answering while one slot is in cooldown"
        );
    }

    server.stop().await;
    assert!(pool.pids().is_empty());
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe; drives the backoff ladder to exhaustion"]
async fn prolonged_failure_never_retires_a_slot_permanently() {
    // The 0.3 policy retired a slot permanently after more than ten
    // consecutive failures, which turned a long transient fault into an outage
    // that only a restart could clear. A slot must now stay replaceable: it
    // slows down, but it never stops being scheduled.
    let server = TestServer::start_with(1, 8).await;
    // Hold the runtime separately: `stop` consumes the server, and these
    // assertions inspect the pool afterwards.
    let runtime = Arc::clone(&server.runtime);
    let pool = runtime.php_worker().unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut generations_seen = 0;
    while pool.snapshot().workers[0].consecutive_failures <= 10 {
        assert!(
            Instant::now() < deadline,
            "slot never reached the cooldown band; last snapshot: {:#?}",
            pool.snapshot()
        );
        if pool.snapshot().ready_workers == 1 {
            generations_seen += 1;
            let _ = server.request(&close_request("/crash")).await;
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    let snapshot = pool.snapshot();
    assert!(
        generations_seen >= 10,
        "the slot must have been replaced repeatedly, saw {generations_seen}"
    );
    // Still a live slot with a scheduled replacement, not a terminal state.
    assert_eq!(snapshot.target_workers, 1);
    assert_eq!(snapshot.workers[0].state, WorkerLifecycleState::Backoff);
    assert!(snapshot.workers[0].generation > 10);
    // Admission stays honest while no generation is READY: callers get a
    // controlled refusal instead of blocking on capacity that is minutes away.
    assert_eq!(snapshot.ready_workers, 0);
    let response = server.request(&close_request("/")).await;
    assert!(
        response.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"),
        "degraded pool must refuse quickly, got: {}",
        String::from_utf8_lossy(&response[..response.len().min(40)])
    );

    server.stop().await;
    assert!(pool.pids().is_empty());
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn shutdown_during_replacement_bootstrap() {
    let startup_marker = marker_path("shutdown-bootstrap-count");
    let mut script = marker_path("shutdown-bootstrap-script");
    script.set_extension("php");
    let worker_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/php-worker/worker.php")
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "\\'");
    let startup_literal = startup_marker
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "\\'");
    std::fs::write(
        &script,
        format!(
            "<?php\n$marker = '{startup_literal}';\n$count = is_file($marker) ? (int) file_get_contents($marker) : 0;\nfile_put_contents($marker, (string) ($count + 1), LOCK_EX);\nif ($count > 0) {{ usleep(10000000); }}\nrequire '{worker_script}';\n"
        ),
    )
    .unwrap();
    let runtime = Arc::new(php_runtime_with_script(1, 8, None, None, &script));
    let server = TestServer::start_runtime(runtime).await;
    let pool = server.runtime.php_worker().unwrap();
    assert!(
        server
            .request(&close_request("/crash"))
            .await
            .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
    );
    wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation == 2
            && snapshot.workers[0].state == WorkerLifecycleState::Bootstrapping
    })
    .await;
    let started = Instant::now();
    server.stop().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = tokio::fs::remove_file(startup_marker).await;
    let _ = tokio::fs::remove_file(script).await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn shutdown_while_worker_busy_completes_transaction() {
    let server = TestServer::start_with(1, 8).await;
    let start = marker_path("busy-shutdown-start");
    let done = marker_path("busy-shutdown-done");
    let path = format!(
        "/delay?ms=150&start_marker={}&done_marker={}",
        url_encode(&start),
        url_encode(&done)
    );
    let address = server.address;
    let active = tokio::spawn(async move { request_at(address, &close_request(&path)).await });
    wait_for_marker(&start).await;
    server.stop().await;
    assert!(active.await.unwrap().starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(done.exists());
    let _ = tokio::fs::remove_file(start).await;
    let _ = tokio::fs::remove_file(done).await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn no_orphan_php_processes() {
    let server = TestServer::start_with(4, 32).await;
    let pids = server.runtime.php_worker().unwrap().pids();
    server.stop().await;
    for pid in pids {
        assert!(
            !process_exists(pid).await,
            "PHP PID {pid} survived shutdown"
        );
    }
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn generation_increments_while_slot_remains_stable() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    for expected_generation in 2..=3 {
        assert!(
            server
                .request(&close_request("/crash"))
                .await
                .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
        );
        let snapshot = wait_for_pool(pool, |snapshot| {
            snapshot.ready_workers == 1 && snapshot.workers[0].generation == expected_generation
        })
        .await;
        assert_eq!(snapshot.workers[0].slot, initial.slot);
        assert_ne!(snapshot.workers[0].pid, initial.pid);
    }
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn all_0_2_cancellation_invariants_remain_unchanged() {
    let server = TestServer::start_with(1, 8).await;
    let pool = server.runtime.php_worker().unwrap();
    let initial = pool.snapshot().workers[0].clone();
    let start = marker_path("v02-invariant-start");
    let done = marker_path("v02-invariant-done");
    let path = format!(
        "/delay?ms=100&start_marker={}&done_marker={}",
        url_encode(&start),
        url_encode(&done)
    );
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream.write_all(&close_request(&path)).await.unwrap();
    wait_for_marker(&start).await;
    drop(stream);
    wait_for_marker(&done).await;
    let verification = server.request(&close_request("/state")).await;
    let (_, body) = split_response(&verification);
    assert_eq!(json_number(body, "pid") as u32, initial.pid.unwrap());
    assert_eq!(pool.snapshot().workers[0].generation, initial.generation);
    assert_eq!(pool.snapshot().total_respawns, 0);
    let _ = tokio::fs::remove_file(start).await;
    let _ = tokio::fs::remove_file(done).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn cancellation_stress_keeps_every_worker_healthy() {
    let server = TestServer::start_with(4, 64).await;
    let expected: HashSet<u32> = server
        .runtime
        .php_worker()
        .unwrap()
        .pids()
        .into_iter()
        .collect();
    for round in 0..8 {
        let mut streams = Vec::new();
        let mut starts = Vec::new();
        let mut dones = Vec::new();
        for worker in 0..4 {
            let start = marker_path(&format!("stress-{round}-{worker}-start"));
            let done = marker_path(&format!("stress-{round}-{worker}-done"));
            let path = format!(
                "/delay?ms=20&start_marker={}&done_marker={}",
                url_encode(&start),
                url_encode(&done)
            );
            let mut stream = TcpStream::connect(server.address).await.unwrap();
            stream.write_all(&close_request(&path)).await.unwrap();
            streams.push(stream);
            starts.push(start);
            dones.push(done);
        }
        for marker in &starts {
            wait_for_marker(marker).await;
        }
        drop(streams);
        for marker in &dones {
            wait_for_marker(marker).await;
        }
        for marker in starts.into_iter().chain(dones) {
            let _ = tokio::fs::remove_file(marker).await;
        }
    }
    assert_eq!(
        server
            .runtime
            .php_worker()
            .unwrap()
            .pids()
            .into_iter()
            .collect::<HashSet<_>>(),
        expected
    );
    for _ in 0..16 {
        let response = server.request(&close_request("/hello")).await;
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }
    server.stop().await;
}

#[tokio::test]
#[ignore = "10,000-request lifecycle soak; requires repository php/8.5/php.exe"]
async fn long_correctness_soak_with_forced_failures() {
    let server = TestServer::start_with(4, 64).await;
    let pool = server.runtime.php_worker().unwrap();
    let execution = marker_path("lifecycle-soak-executions");
    let mut expected = Vec::with_capacity(10_004);

    for index in 0..10_000_u64 {
        if index > 0 && index % 2_500 == 0 {
            let id = format!("forced-{index}");
            expected.push(id.clone());
            let path = format!(
                "/crash?execution_marker={}&execution_id={id}",
                url_encode(&execution)
            );
            let before = pool.snapshot().total_respawns;
            assert!(
                server
                    .request(&close_request(&path))
                    .await
                    .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
            );
            wait_for_pool(pool, |snapshot| {
                snapshot.ready_workers == 4 && snapshot.total_respawns > before
            })
            .await;
        }
        let id = format!("request-{index}");
        expected.push(id.clone());
        let path = format!(
            "/hello?execution_marker={}&execution_id={id}",
            url_encode(&execution)
        );
        assert!(
            server
                .request(&close_request(&path))
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );
    }

    let actual = marker_lines(&execution);
    assert_eq!(actual.len(), expected.len());
    assert_eq!(
        actual.iter().collect::<HashSet<_>>().len(),
        actual.len(),
        "no request marker may be executed twice"
    );
    assert_eq!(
        actual.into_iter().collect::<HashSet<_>>(),
        expected.into_iter().collect()
    );
    assert_eq!(pool.snapshot().ready_workers, 4);
    let _ = tokio::fs::remove_file(execution).await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "60-second random-kill stress; requires repository php/8.5/php.exe"]
async fn random_kill_stress_under_continuous_load() {
    let server = TestServer::start_with(4, 128).await;
    let pool = server.runtime.php_worker().unwrap();
    let execution = marker_path("random-kill-executions");
    let stopping = Arc::new(AtomicBool::new(false));
    let next_id = Arc::new(AtomicU64::new(1));
    let stress_seconds = std::env::var("HAMSTER_STRESS_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let kill_interval = Duration::from_millis(stress_seconds.saturating_mul(1_000) / 8);
    let mut traffic = Vec::new();

    for _ in 0..4 {
        let stopping = Arc::clone(&stopping);
        let next_id = Arc::clone(&next_id);
        let marker = execution.clone();
        let address = server.address;
        traffic.push(tokio::spawn(async move {
            while !stopping.load(Ordering::Acquire) {
                let id = next_id.fetch_add(1, Ordering::AcqRel);
                let path = format!(
                    "/delay?ms=5&execution_marker={}&execution_id=stress-{id}",
                    url_encode(&marker)
                );
                let response = request_at(address, &close_request(&path)).await;
                assert!(
                    response.starts_with(b"HTTP/1.1 200 OK\r\n")
                        || response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
                        || response.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n")
                );
            }
        }));
    }

    let mut seed = 0x9e37_79b9_u64;
    for _ in 0..8 {
        tokio::time::sleep(kill_interval).await;
        let live = wait_for_pool(pool, all_workers_live).await;
        let respawns_before = live.total_respawns;
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let victim = &live.workers[(seed as usize) % live.workers.len()];
        kill_process(victim.pid.unwrap()).await;
        wait_for_pool(pool, |snapshot| {
            all_workers_live(snapshot) && snapshot.total_respawns > respawns_before
        })
        .await;
    }
    stopping.store(true, Ordering::Release);
    for task in traffic {
        task.await.unwrap();
    }
    let recovered = wait_for_pool(pool, |snapshot| snapshot.ready_workers == 4).await;
    assert!(
        recovered.total_respawns <= 8,
        "each kill permits at most one replacement"
    );
    assert!(recovered.total_respawns > 0);
    for _ in 0..32 {
        assert!(
            server
                .request(&close_request("/hello"))
                .await
                .starts_with(b"HTTP/1.1 200 OK\r\n")
        );
    }
    let executions = marker_lines(&execution);
    assert_eq!(
        executions.iter().collect::<HashSet<_>>().len(),
        executions.len(),
        "random kills must not cause duplicate execution"
    );
    let final_pids = recovered
        .workers
        .iter()
        .filter_map(|worker| worker.pid)
        .collect::<Vec<_>>();
    let _ = tokio::fs::remove_file(execution).await;
    server.stop().await;
    for pid in final_pids {
        assert!(
            !process_exists(pid).await,
            "PHP PID {pid} survived stress shutdown"
        );
    }
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn shutdown_drains_inflight_queued_and_disconnected_requests() {
    let server = TestServer::start_with(2, 8).await;
    let mut active_tasks = Vec::new();
    let mut starts = Vec::new();
    for worker in 0..2 {
        let start = marker_path(&format!("shutdown-{worker}-start"));
        let path = format!("/delay?ms=100&start_marker={}", url_encode(&start));
        let address = server.address;
        active_tasks.push(tokio::spawn(async move {
            request_at(address, &close_request(&path)).await
        }));
        starts.push(start);
    }
    for marker in &starts {
        wait_for_marker(marker).await;
    }

    let mut queued_tasks = Vec::new();
    for _ in 0..3 {
        let address = server.address;
        queued_tasks.push(tokio::spawn(async move {
            request_at(address, &close_request("/delay?ms=20")).await
        }));
    }
    let mut disconnected = TcpStream::connect(server.address).await.unwrap();
    disconnected
        .write_all(&close_request("/delay?ms=20"))
        .await
        .unwrap();
    drop(disconnected);
    tokio::time::sleep(Duration::from_millis(20)).await;

    server.stop().await;
    for task in active_tasks.into_iter().chain(queued_tasks) {
        let response = task.await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }
    for marker in starts {
        let _ = tokio::fs::remove_file(marker).await;
    }
}

#[cfg(windows)]
#[test]
#[ignore = "helper for the Windows Ctrl+C integration test"]
fn ctrl_c_helper_entry() {
    let Ok(target) = std::env::var("HAMSTER_CTRL_C_TARGET") else {
        return;
    };
    let target = target.parse::<u32>().unwrap();
    // 0 is CTRL_C_EVENT, 1 is CTRL_BREAK_EVENT.
    let event = std::env::var("HAMSTER_CTRL_EVENT").map_or(0, |event| event.parse().unwrap());
    // The helper is attached to the same console, so it receives the event
    // too. A null handler would ignore only Ctrl+C; this ignores both.
    unsafe extern "system" fn ignore_console_event(_: u32) -> i32 {
        1
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(process_id: u32) -> i32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }
    unsafe {
        FreeConsole();
        assert_ne!(AttachConsole(target), 0);
        assert_ne!(SetConsoleCtrlHandler(Some(ignore_console_event), 1), 0);
        assert_ne!(GenerateConsoleCtrlEvent(event, 0), 0);
    }
    std::thread::sleep(Duration::from_millis(200));
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe and a Windows console"]
async fn ctrl_c_stops_server_and_reaps_pool() {
    use std::{os::windows::process::CommandExt, process::Stdio};
    use tokio::io::{AsyncBufReadExt, BufReader};

    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleCP() -> u32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    if unsafe { GetConsoleCP() } == 0 {
        eprintln!(
            "SKIPPED: ctrl_c_stops_server_and_reaps_pool requires an attached Windows console"
        );
        return;
    }
    // A process that ignores Ctrl+C passes that attribute to every child it
    // creates, and shells disagree about setting it: a plain console session
    // does not, while some non-interactive shells do. Inherited through cargo
    // and this test binary, it reaches the server and silently swallows the
    // event, so the test would measure the launching shell instead of
    // Hamster. Clearing it here makes the server's disposition the same on
    // every machine. Verified: without this the server never sees CTRL_C_EVENT
    // and stays up; with it the server shuts down in about 0.2 s.
    // SAFETY: a null handler with `add = 0` only clears this process's own
    // ignore attribute; it installs nothing.
    unsafe { SetConsoleCtrlHandler(None, 0) };

    let config_path = marker_path("ctrl-config").with_extension("toml");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_hamster-wheel"));
    let php = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 2\nqueue_capacity = 8\n",
        toml_literal(&php),
        toml_literal(&script)
    );
    tokio::fs::write(&config_path, source).await.unwrap();

    common::end_children_with_this_process();
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--config")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().creation_flags(CREATE_NEW_CONSOLE);
    let mut child = command.spawn().unwrap();
    let server_pid = child.id().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut output = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            output.push(line);
        }
        output
    });
    let mut lines = BufReader::new(stdout).lines();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("Hamster Wheel listening") {
                return;
            }
        }
        panic!("server exited before listening");
    })
    .await
    .expect("server should become ready");

    let helper = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("ctrl_c_helper_entry")
        .env("HAMSTER_CTRL_C_TARGET", server_pid.to_string())
        .output()
        .unwrap();
    assert!(
        helper.status.success(),
        "Ctrl+C helper failed: {}{}",
        String::from_utf8_lossy(&helper.stdout),
        String::from_utf8_lossy(&helper.stderr)
    );
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            panic!("server did not stop after console signal");
        }
    };
    assert!(status.success());
    let diagnostics = stderr_task.await.unwrap();
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    let _ = tokio::fs::remove_file(config_path).await;
}

/// A server in its own console with both standard streams piped, which is how
/// a supervisor or log shipper runs it. The tests below close one of those
/// pipes while the server runs and then check that Hamster keeps its normal
/// failure and shutdown behavior instead of dying on the next output line.
#[cfg(windows)]
struct ConsoleServer {
    child: tokio::process::Child,
    pid: u32,
    address: SocketAddr,
    workers: Vec<WorkerProcess>,
    stdout: Option<tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>>,
    stderr: Option<tokio::process::ChildStderr>,
    config_path: PathBuf,
}

#[cfg(windows)]
impl ConsoleServer {
    /// `None` when there is no console to create a child console from. With
    /// `tls`, an HTTPS listener is added beside the cleartext one, which is
    /// what makes the server handle Ctrl+Break as a certificate reload.
    async fn start(name: &str, workers: usize, tls: bool) -> Option<Self> {
        use std::{os::windows::process::CommandExt, process::Stdio};
        use tokio::io::{AsyncBufReadExt, BufReader};

        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetConsoleCP() -> u32;
            fn SetConsoleCtrlHandler(
                handler: Option<unsafe extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }
        if unsafe { GetConsoleCP() } == 0 {
            eprintln!("SKIPPED: {name} requires an attached Windows console");
            return None;
        }
        // See ctrl_c_stops_server_and_reaps_pool: an inherited ignore-Ctrl+C
        // attribute would otherwise reach the server.
        // SAFETY: clears only this process's own ignore attribute.
        unsafe { SetConsoleCtrlHandler(None, 0) };

        let config_path = marker_path(name).with_extension("toml");
        let php = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
        let tls_section = if tls {
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
                .unwrap()
                .self_signed(&key)
                .unwrap();
            let cert_path = config_path.with_extension("pem");
            let key_path = config_path.with_extension("key");
            tokio::fs::write(&cert_path, cert.pem()).await.unwrap();
            tokio::fs::write(&key_path, key.serialize_pem())
                .await
                .unwrap();
            format!(
                "\n[server.tls]\nlisten = \"127.0.0.2:0\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\ndefault = true\n",
                toml_literal(&cert_path),
                toml_literal(&key_path)
            )
        } else {
            String::new()
        };
        let source = format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n{tls_section}\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = 8\n",
            toml_literal(&php),
            toml_literal(&script)
        );
        tokio::fs::write(&config_path, source).await.unwrap();

        common::end_children_with_this_process();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_hamster-wheel"));
        command
            .arg("--config")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().creation_flags(CREATE_NEW_CONSOLE);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let stderr = child.stderr.take();

        let mut worker_pids = Vec::new();
        let address = tokio::time::timeout(Duration::from_secs(10), async {
            // Listening lines are printed cleartext first, then HTTPS.
            let mut cleartext = None;
            while let Some(line) = stdout.next_line().await.unwrap() {
                if let Some(rest) = line.rsplit(" worker ").next()
                    && line.contains(" ready after ")
                {
                    worker_pids.push(rest.split(' ').next().unwrap().parse::<u32>().unwrap());
                }
                if let Some(address) = line.strip_prefix("Hamster Wheel listening on http://") {
                    cleartext = Some(address.parse::<SocketAddr>().unwrap());
                    if !tls {
                        return cleartext.unwrap();
                    }
                }
                if line.starts_with("Hamster Wheel listening on https://") {
                    return cleartext.expect("cleartext listener printed first");
                }
            }
            panic!("server exited before listening");
        })
        .await
        .expect("server should become ready");
        assert_eq!(worker_pids.len(), workers, "one ready line per worker");

        Some(Self {
            child,
            pid,
            address,
            workers: worker_pids.into_iter().map(WorkerProcess::open).collect(),
            stdout: Some(stdout),
            stderr,
            config_path,
        })
    }

    async fn get(&self, path: &str) -> String {
        console_get(self.address, path).await
    }

    /// Delivers a console control event to every process attached to the
    /// server's console, as a key press in that console would.
    async fn console_event(&self, event: u32) {
        let helper = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("ctrl_c_helper_entry")
            .env("HAMSTER_CTRL_C_TARGET", self.pid.to_string())
            .env("HAMSTER_CTRL_EVENT", event.to_string())
            .output()
            .await
            .unwrap();
        assert!(
            helper.status.success(),
            "console event helper failed: {}{}",
            String::from_utf8_lossy(&helper.stdout),
            String::from_utf8_lossy(&helper.stderr)
        );
    }

    /// Sends Ctrl+C and waits for the process to exit.
    async fn ctrl_c(&mut self) -> std::process::ExitStatus {
        self.console_event(0).await;
        match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                self.child.kill().await.unwrap();
                panic!("server did not stop after Ctrl+C");
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ConsoleServer {
    fn drop(&mut self) {
        for extension in ["toml", "pem", "key"] {
            let _ = std::fs::remove_file(self.config_path.with_extension(extension));
        }
    }
}

#[cfg(windows)]
async fn console_get(address: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&close_request(path)).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("response within ten seconds")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// Starts a slow request and returns once PHP is executing it, so a console
/// event sent next arrives while the request is in flight.
#[cfg(windows)]
async fn start_in_flight(address: SocketAddr, name: &str) -> JoinHandle<String> {
    let marker = marker_path(name);
    let path = format!("/delay?ms=1500&start_marker={}", url_encode(&marker));
    let request = tokio::spawn(async move { console_get(address, &path).await });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the slow request never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = std::fs::remove_file(&marker);
    request
}

/// A handle to a live worker, opened while it is known to be running. Unlike a
/// PID, which Windows reuses quickly while a parallel suite spawns php.exe, the
/// handle keeps naming this process after it exits and reports its exit code:
/// the fixture exits 0 after SHUTDOWN_ACK, while a kill reports 1.
#[cfg(windows)]
struct WorkerProcess {
    pid: u32,
    handle: isize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
    fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(handle: isize, exit_code: *mut u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[cfg(windows)]
impl WorkerProcess {
    fn open(pid: u32) -> Self {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        // SAFETY: plain Win32 call; a null result is checked below.
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert_ne!(handle, 0, "open worker {pid} while it is running");
        Self { pid, handle }
    }

    fn is_running(&self) -> bool {
        const WAIT_TIMEOUT: u32 = 0x0000_0102;
        // SAFETY: the handle is owned by self and open.
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT }
    }

    /// The exit code, once the process has exited within five seconds.
    fn exit_code(&self) -> u32 {
        const WAIT_OBJECT_0: u32 = 0;
        // SAFETY: the handle is owned by self and open.
        let waited = unsafe { WaitForSingleObject(self.handle, 5_000) };
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "worker {} is still running",
            self.pid
        );
        let mut code = 0;
        // SAFETY: as above; `code` outlives the call.
        assert_ne!(unsafe { GetExitCodeProcess(self.handle, &mut code) }, 0);
        code
    }
}

#[cfg(windows)]
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by self and closed exactly once.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(windows)]
fn collect_lines<R>(pipe: R) -> JoinHandle<Vec<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        let mut output = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            output.push(line);
        }
        output
    })
}

/// A supervisor that stops reading and closes the server's stdout must not
/// turn the next operational line, here the shutdown line, into a panic that
/// skips the orderly worker shutdown.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe and a Windows console"]
async fn a_closed_stdout_does_not_turn_ctrl_c_into_a_panic() {
    let Some(mut server) = ConsoleServer::start("stdout-closed", 2, false).await else {
        return;
    };
    let diagnostics = collect_lines(server.stderr.take().unwrap());
    drop(server.stdout.take());

    let response = server.get("/").await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");

    let status = server.ctrl_c().await;
    let diagnostics = diagnostics.await.unwrap();
    assert!(
        status.success(),
        "exit {status:?}, diagnostics: {diagnostics:?}"
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    for worker in &server.workers {
        assert_eq!(
            worker.exit_code(),
            0,
            "worker {} was not shut down",
            worker.pid
        );
    }
}

/// Diagnostics go to stderr from the connection tasks, the generation tasks
/// and the pool supervisor. With stderr closed, a worker crash must still be
/// answered, replaced and shut down exactly as it is with stderr open.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe and a Windows console"]
async fn a_closed_stderr_does_not_stop_a_crashed_worker_from_being_replaced() {
    let Some(mut server) = ConsoleServer::start("stderr-closed", 1, false).await else {
        return;
    };
    let output = collect_lines(server.stdout.take().unwrap().into_inner());
    drop(server.stderr.take());

    let first = server.get("/state").await;
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "{first}");
    let crashed = server.get("/crash").await;
    assert!(crashed.starts_with("HTTP/1.1 502 "), "{crashed}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let replacement = loop {
        let response = server.get("/state").await;
        if response.starts_with("HTTP/1.1 200 OK\r\n") {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "no replacement worker served within ten seconds: {response}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let replacement_pid = replacement
        .split("\"pid\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|pid| pid.parse::<u32>().ok())
        .expect("state response carries the worker pid");
    assert_ne!(
        server.workers[0].pid, replacement_pid,
        "the crashed worker cannot still be serving"
    );
    let replacement = WorkerProcess::open(replacement_pid);

    let status = server.ctrl_c().await;
    let output = output.await.unwrap();
    assert!(status.success(), "exit {status:?}, stdout: {output:?}");
    assert!(
        output.iter().any(|line| line == "Hamster Wheel stopped"),
        "{output:?}"
    );
    assert_eq!(server.workers[0].exit_code(), 42, "the crashed worker");
    assert_eq!(
        replacement.exit_code(),
        0,
        "the replacement was not shut down"
    );
}

/// Ctrl+C in the server's console is Hamster's stop request. Shutdown drains
/// accepted PHP work and then stops each worker with SHUTDOWN, so the console
/// event must reach Hamster only: a worker sharing the console would be ended
/// by the same key press, taking its in-flight request with it.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe and a Windows console"]
async fn ctrl_c_drains_an_in_flight_php_request() {
    let Some(mut server) = ConsoleServer::start("ctrl-c-drain", 1, false).await else {
        return;
    };
    let diagnostics = collect_lines(server.stderr.take().unwrap());
    let _output = collect_lines(server.stdout.take().unwrap().into_inner());

    let in_flight = start_in_flight(server.address, "ctrl-c-drain-start").await;
    let status = server.ctrl_c().await;
    let response = in_flight.await.unwrap();
    let diagnostics = diagnostics.await.unwrap();

    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n") && response.ends_with("delayed"),
        "the in-flight request must complete: {response:?}, diagnostics: {diagnostics:?}"
    );
    assert!(
        status.success(),
        "exit {status:?}, diagnostics: {diagnostics:?}"
    );
    assert_eq!(
        server.workers[0].exit_code(),
        0,
        "the worker must stop through SHUTDOWN, not the console event"
    );
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

/// Ctrl+Break in the server's console reloads TLS certificates and nothing
/// else. PHP workers must keep serving across it, including the request in
/// flight when it arrives.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe and a Windows console"]
async fn ctrl_break_reload_leaves_php_workers_serving() {
    let Some(mut server) = ConsoleServer::start("ctrl-break-php", 2, true).await else {
        return;
    };
    let diagnostics = collect_lines(server.stderr.take().unwrap());
    let output = collect_lines(server.stdout.take().unwrap().into_inner());

    let in_flight = start_in_flight(server.address, "ctrl-break-start").await;
    server.console_event(1).await;
    let response = in_flight.await.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n") && response.ends_with("delayed"),
        "the in-flight request must complete across a reload: {response:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    for worker in &server.workers {
        assert!(
            worker.is_running(),
            "worker {} ended on Ctrl+Break with {:#x}",
            worker.pid,
            worker.exit_code()
        );
    }
    let state = server.get("/state").await;
    assert!(state.starts_with("HTTP/1.1 200 OK\r\n"), "{state}");

    let status = server.ctrl_c().await;
    let output = output.await.unwrap();
    let diagnostics = diagnostics.await.unwrap();
    assert!(
        output
            .iter()
            .any(|line| line.starts_with("TLS certificate reload succeeded")),
        "the reload must have run: {output:?}"
    );
    assert!(
        status.success(),
        "exit {status:?}, diagnostics: {diagnostics:?}"
    );
    for worker in &server.workers {
        assert_eq!(
            worker.exit_code(),
            0,
            "worker {} was not shut down",
            worker.pid
        );
    }
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

/// A pool whose bootstrap outlasts the default five-second READY budget, as a
/// first start does when the machine has not opened the application's files
/// recently, fails to start by default and starts with `startup_timeout`. The
/// same budget applies to a replacement generation, which must not be killed
/// at five seconds either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php.exe"]
async fn startup_timeout_admits_a_bootstrap_slower_than_the_default_budget() {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/delayed_ready.php");
    let runtime = |extra: &str| {
        let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
        let source = format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\nqueue_capacity = 8\n{extra}\n[hosts.locations.php.env]\nHAMSTER_READY_DELAY_MS = \"6500\"\n",
            toml_literal(&executable),
            toml_literal(&script)
        );
        Arc::new(
            config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
                .expect("config compiles"),
        )
    };

    let default_budget = runtime("");
    let started = Instant::now();
    let error = default_budget
        .start_workers()
        .await
        .expect_err("a 6.5 s bootstrap exceeds the default five seconds");
    assert!(error.to_string().contains("READY timeout"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    default_budget.shutdown_workers().await.unwrap();

    let server = TestServer::start_runtime(runtime("startup_timeout = 12\n")).await;
    let pool = server.runtime.php_worker().unwrap();
    assert_eq!(pool.startup_timeout(), Duration::from_secs(12));
    let response = server.request(&close_request("/")).await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));

    // The replacement boots just as slowly and must be given the same budget.
    let crashed = server.request(&close_request("/crash")).await;
    assert!(crashed.starts_with(b"HTTP/1.1 502 "));
    let replaced = wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation == 2 && snapshot.live_workers == 1
    })
    .await;
    assert_eq!(
        replaced.workers[0].consecutive_failures, 1,
        "the replacement itself must not have failed at five seconds"
    );
    let response = server.request(&close_request("/")).await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    server.stop().await;
}

/// The worker environment is how a worker script is told which application it
/// serves, so the map has to arrive in the child process rather than merely
/// parse. A pool that silently drops it would serve nothing at all.
#[tokio::test]
#[ignore = "requires php/8.5/php.exe"]
async fn configured_environment_reaches_the_worker_process() {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/env_worker.php");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[
            ("HAMSTER_TEST_APP", "pms"),
            ("HAMSTER_TEST_MODE", "targeted"),
        ],
    ));
    let server = TestServer::start_runtime(runtime).await;

    for (name, expected) in [
        ("HAMSTER_TEST_APP", "pms"),
        ("HAMSTER_TEST_MODE", "targeted"),
        ("HAMSTER_TEST_ABSENT", "<unset>"),
    ] {
        let response = server
            .request(&close_request(&format!("/env?name={name}")))
            .await;
        let (_, body) = split_response(&response);
        assert_eq!(
            std::str::from_utf8(body).unwrap(),
            expected,
            "worker reported the wrong value for {name}"
        );
    }

    server.stop().await;
}

/// A replacement generation is a fresh process, so it has to be given the same
/// environment. Without this a pool degrades after the first crash into workers
/// that no longer know what to serve.
#[tokio::test]
#[ignore = "requires php/8.5/php.exe"]
async fn replacement_generation_receives_the_configured_environment() {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/env_worker.php");
    let runtime = Arc::new(php_runtime_with_env(
        1,
        &script,
        &[("HAMSTER_TEST_APP", "pms")],
    ));
    let server = TestServer::start_runtime(runtime).await;
    // Hold the runtime separately: `stop` consumes the server, and the pool is
    // inspected while the server is still serving.
    let runtime = Arc::clone(&server.runtime);
    let pool = runtime.php_worker().unwrap();
    let original = pool.snapshot().workers[0].pid;

    let _ = server.request(&close_request("/crash")).await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = pool.snapshot();
        if snapshot.ready_workers == 1 && snapshot.workers[0].pid != original {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replacement generation never became ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let response = server
        .request(&close_request("/env?name=HAMSTER_TEST_APP"))
        .await;
    let (_, body) = split_response(&response);
    assert_eq!(
        std::str::from_utf8(body).unwrap(),
        "pms",
        "replacement generation lost its environment"
    );

    server.stop().await;
}

// --- Execution timeout policy -------------------------------------------
//
// These pin the distinction the policy exists for: an application that runs
// past its deadline must not be charged to the slot's infrastructure failure
// history, while a slot that genuinely cannot run still walks the backoff
// ladder. The full fault-injection evidence that motivated them is in
// `benchmarks/results/worker_request_timeout_investigation_20260917.md`; only
// the minimum needed to detect a regression is kept here.

fn php_runtime_with_request_timeout(
    workers: usize,
    request_timeout: u64,
    script: &Path,
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = 8\nrequest_timeout = {request_timeout}\n",
        toml_literal(&executable),
        toml_literal(script)
    );
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP request-timeout config should compile")
}

async fn timeout_policy_server(workers: usize, request_timeout: u64) -> TestServer {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    TestServer::start_runtime(Arc::new(php_runtime_with_request_timeout(
        workers,
        request_timeout,
        &script,
    )))
    .await
}

/// The configured deadline is the one that applies, and the worker is still
/// killed and replaced when it expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn a_configured_request_timeout_replaces_the_worker_that_exceeded_it() {
    let server = timeout_policy_server(1, 1).await;
    let pool = server.runtime.php_worker().unwrap();
    let before = pool.snapshot().workers[0].clone();

    let started = Instant::now();
    let response = server.request(&close_request("/delay?ms=4000")).await;
    let elapsed = started.elapsed();

    let (head, body) = split_response(&response);
    assert!(
        String::from_utf8_lossy(head).starts_with("HTTP/1.1 504 "),
        "an execution timeout answers 504: {}",
        String::from_utf8_lossy(head)
    );
    // The body keeps the specific timeout observable while 504 states the
    // cause without implying that replay would be safe.
    assert_eq!(body, b"PHP Worker Execution Timeout");
    // The one-second configuration must be what fired, not the five-second
    // default; four seconds of application sleep would otherwise have run.
    assert!(
        elapsed < Duration::from_secs(3),
        "the configured one-second deadline should fire, not the default: {elapsed:?}"
    );

    let after = wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].generation > before.generation && all_workers_live(snapshot)
    })
    .await;
    assert_ne!(
        after.workers[0].pid, before.pid,
        "the worker that timed out must be replaced"
    );
    server.stop().await;
}

/// The policy itself: repeated execution timeouts never move the slot onto the
/// infrastructure backoff ladder, and ordinary traffic keeps being served.
///
/// Against the pre-policy tree this slot reaches `consecutive_failures == 5`
/// and its replacements are delayed 100, 200, 400 and 800 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn execution_timeouts_never_enter_the_infrastructure_backoff_ladder() {
    let server = timeout_policy_server(1, 1).await;
    let pool = server.runtime.php_worker().unwrap();

    for round in 1..=5_u32 {
        wait_for_pool(pool, |snapshot| snapshot.ready_workers > 0).await;
        let response = server.request(&close_request("/delay?ms=4000")).await;
        let (_, body) = split_response(&response);
        assert_eq!(body, b"PHP Worker Execution Timeout", "round {round}");

        // The 504 is sent by the generation task before that generation exits,
        // and the supervisor records the timeout only when it joins the exit,
        // so a snapshot taken the moment the response arrives can precede it
        // (seen once in six parallel suite runs). Wait for the record, then
        // require that it counted this timeout exactly once. The slot counter is
        // written after the pool total, so waiting on it covers both.
        let snapshot = wait_for_pool(pool, |snapshot| {
            snapshot.workers[0].execution_timeouts >= u64::from(round)
        })
        .await;
        assert_eq!(
            snapshot.workers[0].consecutive_failures, 0,
            "an execution timeout is not an infrastructure failure (round {round})"
        );
        assert_eq!(
            snapshot.workers[0].execution_timeouts,
            u64::from(round),
            "every execution timeout stays observable (round {round})"
        );
        assert_eq!(snapshot.total_execution_timeouts, u64::from(round));
    }

    // Five timeouts would have been failure 5 on the old ladder, which is an
    // 800 ms replacement delay. The pool must instead be immediately usable.
    wait_for_pool(pool, all_workers_live).await;
    let response = server.request(&close_request("/json")).await;
    assert!(
        String::from_utf8_lossy(split_response(&response).0).starts_with("HTTP/1.1 200 "),
        "ordinary traffic must be unaffected by execution timeouts"
    );
    assert_eq!(pool.snapshot().workers[0].consecutive_failures, 0);
    server.stop().await;
}

/// The other half of the policy: a slot that genuinely cannot run is still
/// charged, so separating the timeout did not disarm the ladder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn an_infrastructure_failure_still_counts_against_the_slot() {
    let server = timeout_policy_server(1, 1).await;
    let pool = server.runtime.php_worker().unwrap();

    // `/crash` exits mid-transaction, which is a possibly-started worker
    // failure rather than an application deadline.
    let response = server.request(&close_request("/crash?ms=0")).await;
    assert!(
        String::from_utf8_lossy(split_response(&response).0).starts_with("HTTP/1.1 502 "),
        "a crashed worker still answers 502"
    );
    let snapshot = wait_for_pool(pool, |snapshot| {
        snapshot.workers[0].consecutive_failures > 0
    })
    .await;
    assert_eq!(
        snapshot.workers[0].execution_timeouts, 0,
        "a crash is not an execution timeout"
    );
    server.stop().await;
}

/// An execution timeout is possibly-started, so its job is never dispatched
/// again. This is the at-most-once invariant, restated for the new class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn a_timed_out_request_is_never_executed_twice() {
    let server = timeout_policy_server(1, 1).await;
    let marker = marker_path("execution-timeout-once");
    let target = format!(
        "/delay?ms=4000&execution_marker={}&execution_id=timed-out",
        url_encode(&marker)
    );

    let response = server.request(&close_request(&target)).await;
    assert_eq!(split_response(&response).1, b"PHP Worker Execution Timeout");

    // Give any redispatch time to reach a replacement worker before counting.
    wait_for_pool(server.runtime.php_worker().unwrap(), all_workers_live).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        marker_lines(&marker),
        vec!["timed-out".to_owned()],
        "a possibly-started request must never run a second time"
    );
    let _ = std::fs::remove_file(&marker);
    server.stop().await;
}

// --- Pool health signal ---------------------------------------------------
//
// `ready_workers` counts idle workers, not healthy ones, so it reads zero on a
// perfectly healthy pool that is serving at full concurrency. `live_workers`
// is the health signal and the one `execute` gates its 503 on. These pin that
// distinction, because a degradation check built on the wrong counter would
// fire constantly under load.

/// The two counters measure different things, and only one of them is health.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn live_workers_is_the_health_signal_and_ready_workers_is_not() {
    let server = TestServer::start_with(2, 16).await;
    let pool = server.runtime.php_worker().unwrap();

    let idle = pool.snapshot();
    assert_eq!(idle.live_workers, 2, "a started pool has every worker live");
    assert_eq!(idle.ready_workers, 2, "and every worker idle");

    // Occupy one worker with a request that is slow but well inside the
    // deadline, so the pool stays completely healthy throughout.
    let address = server.address;
    let busy =
        tokio::spawn(async move { request_at(address, &close_request("/delay?ms=700")).await });
    let occupied = wait_for_pool(pool, |snapshot| snapshot.ready_workers < 2).await;
    assert_eq!(
        occupied.live_workers, 2,
        "a busy worker is still a live worker: {occupied:#?}"
    );
    assert!(
        occupied.ready_workers < occupied.live_workers,
        "ready_workers measures spare capacity, not health: {occupied:#?}"
    );

    let response = busy.await.expect("slow request task");
    assert!(
        String::from_utf8_lossy(split_response(&response).0).starts_with("HTTP/1.1 200 "),
        "the pool was healthy the whole time"
    );
    wait_for_pool(pool, |snapshot| snapshot.ready_workers == 2).await;
    server.stop().await;
}

/// `live_workers` tracks replacement, so it returns to target after a death.
///
/// The dip below target is deliberately not asserted: a replacement reaches
/// READY in about 35 ms, so observing the transient from outside is a race.
/// What matters for a health signal is that it is not stale afterwards, which
/// is what this pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn live_workers_returns_to_target_after_a_worker_is_killed() {
    let server = TestServer::start_with(2, 16).await;
    let pool = server.runtime.php_worker().unwrap();
    assert_eq!(pool.snapshot().live_workers, 2);

    let victim = pool.pids()[0];
    kill_process(victim).await;

    // An idle worker's death is detected passively, on its next assignment;
    // there is no stdout probe. `live_workers` therefore stays stale until
    // traffic reaches the dead generation, which is a real property of the
    // signal and the reason this drives requests rather than just waiting.
    let mut attempts = 0_u32;
    while pool.snapshot().total_respawns == 0 && attempts < 40 {
        let _ = server.request(&close_request("/json")).await;
        attempts += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        pool.snapshot().total_respawns > 0,
        "the pool must notice a killed worker once traffic reaches it"
    );

    let recovered = wait_for_pool(pool, |snapshot| {
        snapshot.live_workers == snapshot.target_workers && all_workers_live(snapshot)
    })
    .await;
    assert_eq!(
        recovered.live_workers, 2,
        "the health signal must follow the replacement, not go stale: {recovered:#?}"
    );
    assert!(!pool.pids().contains(&victim), "the dead PID is gone");
    server.stop().await;
}

/// Writes a self-signed certificate for `localhost` and returns its PEM
/// paths, DER, and the directory to remove.
fn localhost_certificate(tag: &str) -> (PathBuf, PathBuf, Vec<u8>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "hamster-wheel-php-tls-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create certificate directory");
    let generated =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let cert = dir.join("localhost.crt");
    let key = dir.join("localhost.key");
    std::fs::write(&cert, generated.cert.pem()).expect("write certificate");
    std::fs::write(&key, generated.signing_key.serialize_pem()).expect("write key");
    (cert, key, generated.cert.der().to_vec(), dir)
}

async fn https_request(
    address: SocketAddr,
    trusted: &[u8],
    request: &[u8],
) -> (Vec<u8>, SocketAddr) {
    use tokio_rustls::{
        TlsConnector,
        rustls::{
            ClientConfig, RootCertStore,
            pki_types::{CertificateDer, ServerName},
        },
    };
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(trusted.to_vec()))
        .expect("trust the test certificate");
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("client versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let stream = TcpStream::connect(address)
        .await
        .expect("client should connect");
    let client = stream.local_addr().expect("client address");
    let mut stream = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .expect("a verifying client completes the handshake");
    stream
        .write_all(request)
        .await
        .expect("request should be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("response should end with close_notify");
    (response, client)
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn protocol_v3_carries_the_listener_scheme_not_a_forwarding_header() {
    let (cert, key, der, dir) = localhost_certificate("scheme");
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    // Configuration is the path under test: the certificate is loaded and
    // checked while the config compiles, before any worker or socket exists.
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[server.tls]\nlisten = \"127.0.0.1:8443\"\nhandshake_timeout = 5\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\ndefault = true\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\n",
        toml_literal(&cert),
        toml_literal(&key),
        toml_literal(&executable),
        toml_literal(&script),
    );
    let runtime = Arc::new(
        config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("TLS PHP config should compile"),
    );
    let tls = Arc::clone(runtime.tls().expect("the TLS listener was compiled"));
    runtime
        .start_workers()
        .await
        .expect("PHP worker should reach READY");
    let clear_socket = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind cleartext");
    let tls_socket = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind TLS");
    let clear_address = clear_socket.local_addr().unwrap();
    let tls_address = tls_socket.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::serve_listeners(
        vec![
            server::Listener::cleartext(clear_socket),
            server::Listener::tls(tls_socket, tls),
        ],
        runtime.router_arc(),
        runtime.http_limits(),
        None,
        None,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    let (secure, client) = https_request(
        tls_address,
        &der,
        b"GET /metadata HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: http\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_response(&secure);
    assert!(
        head.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&secure)
    );
    assert_json_string(body, "scheme", "https");
    assert_json_string(body, "peer_addr", "127.0.0.1");
    assert_eq!(json_number(body, "peer_port"), client.port() as u64);
    assert_eq!(json_number(body, "local_port"), tls_address.port() as u64);
    let pid = json_number(body, "pid");

    // Same W1 process, cleartext listener: the scheme is replaced, not kept.
    let clear = request_at(
        clear_address,
        b"GET /metadata HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (_, clear_body) = split_response(&clear);
    assert_json_string(clear_body, "scheme", "http");
    assert_eq!(
        json_number(clear_body, "local_port"),
        clear_address.port() as u64
    );
    assert_eq!(json_number(clear_body, "pid"), pid);

    shutdown_tx
        .send(())
        .expect("server should still be running");
    task.await
        .expect("server task should finish")
        .expect("server should stop cleanly");
    runtime
        .shutdown_workers()
        .await
        .expect("PHP worker should stop cleanly");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
#[ignore = "requires a freshly prepared Octane-enabled scratch PMS with HamsterPmsProbeServiceProvider"]
async fn laravel_sees_https_from_the_tls_listener_and_not_from_a_forged_header() {
    use tokio_rustls::{
        TlsConnector,
        rustls::{
            ClientConfig, RootCertStore,
            pki_types::{CertificateDer, ServerName},
        },
    };

    let _app_guard = LARAVEL_APP_TEST_LOCK.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("fixtures/php-worker/laravel_worker.php");
    let app_path = PathBuf::from(
        std::env::var_os("HAMSTER_LARAVEL_TEST_PATH")
            .expect("HAMSTER_LARAVEL_TEST_PATH must identify the prepared scratch PMS"),
    );
    let app = app_path.to_string_lossy().replace('\\', "/");
    let php_ini = std::env::var("HAMSTER_LARAVEL_TEST_PHP_INI")
        .unwrap_or_else(|_| format!("{app}/php.ini"))
        .replace('\\', "/");
    let (cert, key, der, dir) = localhost_certificate("laravel");
    let executable = root.join("php/8.5/php.exe");
    let mut source = format!(
        "[server]\n\n[server.tls]\nlisten = \"127.0.0.1:8443\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\n\n[hosts.locations.php.env]\n",
        toml_literal(&cert),
        toml_literal(&key),
        toml_literal(&executable),
        toml_literal(&script),
    );
    for (name, value) in [
        ("HAMSTER_LARAVEL_PATH", app.as_str()),
        ("PHPRC", php_ini.as_str()),
        ("APP_ENV", "production"),
        ("APP_DEBUG", "false"),
        ("PERF_MONITOR_ENABLED", "false"),
        ("HAMSTER_PMS_PROBE", "1"),
    ] {
        source.push_str(&format!("{name} = {value:?}\n"));
    }
    // TLS only: no cleartext listener is configured at all.
    let runtime = Arc::new(
        config::compile_str(&source, &root).expect("TLS-only Laravel config should compile"),
    );
    assert!(runtime.listen_address().is_none());
    let tls = Arc::clone(runtime.tls().expect("TLS listener"));
    runtime
        .start_workers()
        .await
        .expect("Laravel worker should reach READY");
    let socket = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind TLS");
    let address = socket.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::serve_listeners(
        vec![server::Listener::tls(socket, tls)],
        runtime.router_arc(),
        runtime.http_limits(),
        None,
        None,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(der))
        .expect("trust certificate");
    let connector = TlsConnector::from(Arc::new(
        ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("client versions")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    // From 127.0.0.2, which the probe's Symfony policy does not trust, so a
    // forged forwarding scheme must not override the connection's https.
    let probe = |source_ip: Ipv4Addr, request: &'static [u8]| {
        let connector = connector.clone();
        async move {
            let socket = TcpSocket::new_v4().unwrap();
            socket
                .bind(SocketAddr::new(IpAddr::V4(source_ip), 0))
                .expect("bind source address");
            let stream = socket.connect(address).await.expect("connect");
            let client = stream.local_addr().unwrap();
            let mut stream = connector
                .connect(ServerName::try_from("localhost").unwrap(), stream)
                .await
                .expect("verified handshake");
            stream.write_all(request).await.expect("request");
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .await
                .expect("response ends with close_notify");
            (response, client)
        }
    };
    let (response, client) = probe(
        Ipv4Addr::new(127, 0, 0, 2),
        b"GET /__hamster/metadata HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: http\r\nX-Forwarded-Port: 80\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_response(&response);
    assert!(
        head.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "PMS metadata probe failed: {}",
        String::from_utf8_lossy(&response)
    );
    assert_json_string(body, "request_scheme", "https");
    assert_json_string(body, "https", "on");
    assert_json_string(body, "scheme", "https");
    assert_json_string(body, "remote_addr", "127.0.0.2");
    assert_json_string(body, "remote_port", &client.port().to_string());
    assert_json_string(body, "server_port", &address.port().to_string());
    assert_json_string(body, "client_ip", "127.0.0.2");
    assert_json_string(body, "host", "localhost");
    // Symfony takes the port from Host; a Host without one means the
    // scheme's default, so https here must mean 443 (cleartext would be 80).
    assert_eq!(json_number(body, "port"), 443);

    shutdown_tx
        .send(())
        .expect("server should still be running");
    task.await
        .expect("server task should finish")
        .expect("server should stop cleanly");
    runtime
        .shutdown_workers()
        .await
        .expect("Laravel worker should stop cleanly");
    let _ = std::fs::remove_dir_all(dir);
}
