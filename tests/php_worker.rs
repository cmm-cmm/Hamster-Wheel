use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hamster_wheel::{config, server};
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
        let runtime = Arc::new(php_runtime(workers, queue_capacity));
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

fn php_runtime(workers: usize, queue_capacity: usize) -> config::RuntimeConfig {
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "PHP integration test requires repository php/8.5/php.exe"
    );
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\n",
        toml_literal(&executable),
        toml_literal(&script)
    );
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
async fn worker_protocol_failure_returns_controlled_bad_gateway() {
    let server = TestServer::start().await;
    for (path, status) in [
        ("/malformed", b"HTTP/1.1 502 Bad Gateway\r\n".as_slice()),
        ("/hello", b"HTTP/1.1 503 Service Unavailable\r\n".as_slice()),
    ] {
        let response = server
            .request(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        assert!(response.starts_with(status));
    }
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
async fn one_failed_worker_leaves_pool_serving_degraded() {
    let server = TestServer::start_with(2, 16).await;
    let initial: HashSet<u32> = server
        .runtime
        .php_worker()
        .unwrap()
        .pids()
        .into_iter()
        .collect();
    assert_eq!(initial.len(), 2);
    let failed = server.request(&close_request("/crash?ms=10")).await;
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.runtime.php_worker().unwrap().pids().len() != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    for _ in 0..8 {
        let response = server.request(&close_request("/state")).await;
        let (headers, body) = split_response(&response);
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(initial.contains(&(json_number(body, "pid") as u32)));
    }
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires repository php/8.5/php.exe"]
async fn zero_workers_fail_pending_and_new_requests_without_deadlock() {
    let server = TestServer::start_with(1, 8).await;
    let start = marker_path("zero-start");
    let crash_path = format!("/crash?ms=200&start_marker={}", url_encode(&start));
    let address = server.address;
    let crashing =
        tokio::spawn(async move { request_at(address, &close_request(&crash_path)).await });
    wait_for_marker(&start).await;
    let address = server.address;
    let pending = tokio::spawn(async move { request_at(address, &close_request("/hello")).await });

    let failed = tokio::time::timeout(Duration::from_secs(2), crashing)
        .await
        .expect("crashing request must not hang")
        .unwrap();
    assert!(failed.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    let pending = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("pending request must fail when idle channel closes")
        .unwrap();
    assert!(pending.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
    let new_request = tokio::time::timeout(
        Duration::from_secs(1),
        server.request(&close_request("/hello")),
    )
    .await
    .expect("new request must fail fast");
    assert!(new_request.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
    let _ = tokio::fs::remove_file(start).await;
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
