use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
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
    async fn start(executable: &Path) -> Self {
        let runtime = Arc::new(php_runtime(executable));
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
        let mut stream = TcpStream::connect(self.address)
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
    }
}

/// Locates a PHP CLI binary for the integration tests.
///
/// Resolution order keeps every supported environment working without editing
/// the tests: an explicit `HAMSTER_PHP` override, the Windows development
/// runtime vendored under `php/`, then the first `php` on `PATH` (Linux,
/// macOS, and the container images built from `Dockerfile`).
fn locate_php() -> Option<PathBuf> {
    if let Some(value) = env::var_os("HAMSTER_PHP") {
        let candidate = PathBuf::from(value);
        return candidate.is_file().then_some(candidate);
    }
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    if vendored.is_file() {
        return Some(vendored);
    }
    php_on_path()
}

fn php_on_path() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["php.exe"]
    } else {
        &["php"]
    };
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        names
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Reports a skipped test so a missing PHP runtime never looks like a pass.
fn skipped(test: &str) {
    eprintln!(
        "skipping {test}: no PHP CLI found; set HAMSTER_PHP, install php, or vendor php/8.5/php.exe"
    );
}

fn php_runtime(executable: &Path) -> config::RuntimeConfig {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\n",
        toml_literal(executable),
        toml_literal(&script)
    );
    config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("PHP runtime config should compile")
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
async fn one_php_process_bootstraps_once_across_one_hundred_http_requests() {
    let Some(php) = locate_php() else {
        return skipped("one_php_process_bootstraps_once_across_one_hundred_http_requests");
    };
    let server = TestServer::start(&php).await;
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
        server.runtime.php_worker().unwrap().pid().await,
        pid.map(|pid| pid as u32)
    );
    server.stop().await;
}

#[tokio::test]
async fn http_mapping_preserves_query_headers_body_and_custom_response() {
    let Some(php) = locate_php() else {
        return skipped("http_mapping_preserves_query_headers_body_and_custom_response");
    };
    let server = TestServer::start(&php).await;
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
async fn php_handler_preserves_keep_alive() {
    let Some(php) = locate_php() else {
        return skipped("php_handler_preserves_keep_alive");
    };
    let server = TestServer::start(&php).await;
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
async fn worker_protocol_failure_returns_controlled_bad_gateway() {
    let Some(php) = locate_php() else {
        return skipped("worker_protocol_failure_returns_controlled_bad_gateway");
    };
    let server = TestServer::start(&php).await;
    for path in ["/malformed", "/hello"] {
        let response = server
            .request(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    }
    server.stop().await;
}
