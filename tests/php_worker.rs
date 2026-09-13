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

use hamster_wheel::{
    config,
    php_worker::{PoolSnapshot, WorkerLifecycleState, WorkerSpawnReason},
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

struct TestServer {
    address: SocketAddr,
    runtime: Arc<config::RuntimeConfig>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

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
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, async {
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
        assert!(self.runtime.php_worker().unwrap().pids().is_empty());
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

fn php_runtime_with_script(
    workers: usize,
    queue_capacity: usize,
    max_requests: Option<u64>,
    max_worker_age: Option<u64>,
    script: &Path,
) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let mut source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\n",
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

    let detecting = server.request(&close_request("/hello")).await;
    assert!(detecting.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
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

    let mut detecting = Vec::new();
    for _ in 0..4 {
        let address = server.address;
        detecting.push(tokio::spawn(async move {
            request_at(address, &close_request("/hello")).await
        }));
    }
    for response in detecting {
        assert!(
            response
                .await
                .unwrap()
                .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n")
        );
    }
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
        assert_ne!(SetConsoleCtrlHandler(None, 1), 0);
        assert_ne!(GenerateConsoleCtrlEvent(0, 0), 0);
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
    }
    if unsafe { GetConsoleCP() } == 0 {
        eprintln!(
            "SKIPPED: ctrl_c_stops_server_and_reaps_pool requires an attached Windows console"
        );
        return;
    }

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
