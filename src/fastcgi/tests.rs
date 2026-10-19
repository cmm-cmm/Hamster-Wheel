//! Path resolution on real directories, and the protocol against a scripted
//! FastCGI server behind a real Hamster listener. `tests/php_cgi.rs` runs the
//! same handler against `php-cgi` itself.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

use super::{
    BEGIN_REQUEST, END_REQUEST, FastCgiBackend, FastCgiConfig, KEEP_CONN, PARAMS, RecordReader,
    STDERR, STDIN, STDOUT, Target, record, resolve,
};
use crate::{
    config::HttpLimits,
    router::{Handler, HostRouter, LocationRouter, VirtualHost},
    server::serve_with_router,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A document root that removes itself.
struct Root {
    path: PathBuf,
}

impl Root {
    fn new(files: &[(&str, &str)]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hamster-fastcgi-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        for (name, content) in files {
            let file = path.join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
        std::fs::create_dir_all(&path).unwrap();
        Self {
            path: std::fs::canonicalize(&path).unwrap(),
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn target(root: &Root, uri_path: &str) -> Target {
    resolve(&root.path, "index.php", uri_path).unwrap()
}

fn script_of(target: Target) -> (String, String) {
    match target {
        Target::Script {
            script_name,
            path_info,
            ..
        } => (script_name, path_info),
        other => panic!("expected a script, got {other:?}"),
    }
}

fn is_static(target: &Target) -> bool {
    matches!(target, Target::Static { .. })
}

// ----- Path resolution ------------------------------------------------------

#[test]
fn scripts_path_info_and_the_front_controller_resolve_as_a_php_server_would() {
    let root = Root::new(&[
        ("index.php", "front"),
        ("app.php", "app"),
        ("style.css", "css"),
        ("sub/index.php", "sub"),
        ("docs/index.html", "docs"),
    ]);
    assert_eq!(
        script_of(target(&root, "/")),
        ("/index.php".into(), String::new())
    );
    assert_eq!(
        script_of(target(&root, "/app.php")),
        ("/app.php".into(), String::new())
    );
    assert_eq!(
        script_of(target(&root, "/app.php/extra/path")),
        ("/app.php".into(), "/extra/path".into())
    );
    // A route the front controller owns.
    assert_eq!(
        script_of(target(&root, "/blog/2026/post")),
        ("/index.php".into(), String::new())
    );
    assert_eq!(
        script_of(target(&root, "/sub/")),
        ("/sub/index.php".into(), String::new())
    );
    assert!(is_static(&target(&root, "/docs/")));
    assert!(is_static(&target(&root, "/style.css")));
    // A named script that does not exist is not handed to the front controller.
    assert!(matches!(target(&root, "/missing.php"), Target::NotFound));
    assert!(matches!(
        target(&root, "/missing.php/extra"),
        Target::NotFound
    ));
}

#[test]
fn without_a_front_controller_an_unknown_path_is_not_found() {
    let root = Root::new(&[("style.css", "css")]);
    assert!(matches!(target(&root, "/anything"), Target::NotFound));
    assert!(matches!(target(&root, "/"), Target::NotFound));
}

#[test]
fn traversal_encoded_separators_nul_and_bad_encoding_are_refused() {
    let root = Root::new(&[("index.php", "front")]);
    for path in [
        "/../secret.php",
        "/%2e%2e/secret.php",
        "/a%2fb.php",
        "/a%5cb.php",
        "/a%00.php",
    ] {
        assert!(matches!(target(&root, path), Target::Forbidden), "{path}");
    }
    assert!(matches!(target(&root, "/%zz"), Target::BadRequest));
}

#[cfg(windows)]
#[test]
fn a_script_is_never_served_as_its_source_under_any_windows_spelling() {
    let root = Root::new(&[("index.php", "<?php secret"), ("app.php", "<?php app")]);
    for path in ["/APP.PHP", "/app.PHP"] {
        assert_eq!(
            script_of(target(&root, path)).0,
            "/app.php",
            "{path} runs the file the filesystem opens, named as it is stored"
        );
    }
    // The canonical root is a verbatim `\\?\` path, so Windows does not strip
    // trailing dots or spaces beneath it: these name no file and go to the
    // front controller. What matters is that none is served as source.
    for path in ["/app.php.", "/app.php%20", "/app.php.%20.", "/index.php."] {
        let resolved = target(&root, path);
        assert!(!is_static(&resolved), "{path} would send PHP source");
        assert_eq!(script_of(resolved).0, "/index.php", "{path}");
    }
    // Stream syntax is refused before the filesystem sees it.
    assert!(matches!(
        target(&root, "/app.php::$DATA"),
        Target::Forbidden
    ));
}

#[cfg(windows)]
#[test]
fn a_junction_out_of_the_root_is_forbidden_for_scripts_and_files() {
    let root = Root::new(&[("index.php", "front")]);
    let outside = Root::new(&[("secret.php", "<?php"), ("secret.txt", "secret")]);
    let link = root.path.join("linked");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside.path)
        .output()
        .unwrap();
    assert!(status.status.success(), "create junction: {status:?}");
    for path in [
        "/linked/secret.php",
        "/linked/secret.txt",
        "/linked/secret.php/info",
    ] {
        assert!(matches!(target(&root, path), Target::Forbidden), "{path}");
    }
    std::fs::remove_dir(&link).unwrap();
}

// ----- Protocol against a scripted server ----------------------------------

#[derive(Debug, Default)]
struct Received {
    params: HashMap<String, String>,
    stdin: Vec<u8>,
}

fn decode_params(bytes: &[u8]) -> HashMap<String, String> {
    fn length(bytes: &[u8], at: &mut usize) -> usize {
        if bytes[*at] < 128 {
            *at += 1;
            usize::from(bytes[*at - 1])
        } else {
            let value = u32::from_be_bytes(bytes[*at..*at + 4].try_into().unwrap()) & 0x7fff_ffff;
            *at += 4;
            value as usize
        }
    }
    let mut params = HashMap::new();
    let mut at = 0;
    while at < bytes.len() {
        let name_length = length(bytes, &mut at);
        let value_length = length(bytes, &mut at);
        let name = String::from_utf8(bytes[at..at + name_length].to_vec()).unwrap();
        at += name_length;
        let value = String::from_utf8(bytes[at..at + value_length].to_vec()).unwrap();
        at += value_length;
        params.insert(name, value);
    }
    params
}

/// Reads BEGIN_REQUEST and PARAMS, and STDIN too when asked.
async fn receive(stream: &mut TcpStream, read_stdin: bool) -> Received {
    let mut reader = RecordReader::new(stream);
    let begin = reader.next().await.unwrap().unwrap();
    assert_eq!(begin.kind, BEGIN_REQUEST);
    assert_eq!(
        &begin.content[..3],
        &[0, 1, 0],
        "responder role, no keep-conn"
    );
    let mut params = Vec::new();
    loop {
        let record = reader.next().await.unwrap().unwrap();
        assert_eq!(record.kind, PARAMS);
        if record.content.is_empty() {
            break;
        }
        params.extend_from_slice(&record.content);
    }
    let mut received = Received {
        params: decode_params(&params),
        stdin: Vec::new(),
    };
    if read_stdin {
        loop {
            let record = reader.next().await.unwrap().unwrap();
            assert_eq!(record.kind, STDIN);
            if record.content.is_empty() {
                break;
            }
            received.stdin.extend_from_slice(&record.content);
        }
    }
    received
}

fn records(parts: &[(u8, &[u8])], end: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, content) in parts {
        for chunk in content.chunks(super::MAX_CONTENT) {
            record(*kind, chunk, &mut out);
        }
    }
    if end {
        record(END_REQUEST, &[0, 0, 0, 0, 0, 0, 0, 0], &mut out);
    }
    out
}

struct Server {
    address: SocketAddr,
    backend: Arc<FastCgiBackend>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
    _root: Root,
}

impl Server {
    async fn start(servers: Vec<SocketAddr>, tweak: impl FnOnce(&mut FastCgiConfig)) -> Self {
        let root = Root::new(&[
            ("index.php", "front"),
            ("app.php", "app"),
            ("style.css", "body { color: red }"),
        ]);
        let mut config = FastCgiConfig {
            label: Arc::from("localhost/"),
            document_root: root.path.clone(),
            index: "index.php".to_owned(),
            servers,
            concurrency: 1,
            connect_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(5),
            keep_connections: false,
            resolve_cache: Duration::ZERO,
        };
        tweak(&mut config);
        let backend = FastCgiBackend::new(config);
        let locations = Arc::new(
            LocationRouter::new(Vec::new(), Handler::fastcgi(Arc::clone(&backend))).unwrap(),
        );
        let router =
            HostRouter::new(vec![VirtualHost::new("localhost", locations).unwrap()]).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let limits = HttpLimits::for_test(
            64,
            Duration::from_secs(10),
            Duration::from_secs(10),
            64 * 1024,
        );
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_with_router(
            listener,
            Arc::new(router),
            limits,
            async {
                let _ = shutdown_rx.await;
            },
        ));
        Self {
            address,
            backend,
            shutdown: Some(shutdown),
            task,
            _root: root,
        }
    }

    async fn stop(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.task).await;
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    complete: bool,
}

impl Reply {
    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Sends a whole request and reads to EOF; `complete` is false when a
/// chunked body ended without its terminating chunk.
async fn exchange(address: SocketAddr, request: &[u8]) -> Reply {
    parse_reply(&raw_exchange(address, request).await)
}

async fn raw_exchange(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut bytes))
        .await
        .expect("a reply within fifteen seconds");
    bytes
}

fn parse_reply(bytes: &[u8]) -> Reply {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(bytes)));
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let raw = &bytes[end + 4..];
    let chunked = headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked");
    let (body, complete) = if chunked {
        dechunk(raw)
    } else {
        (raw.to_vec(), true)
    };
    Reply {
        status,
        headers,
        body,
        complete,
    }
}

fn dechunk(mut raw: &[u8]) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    loop {
        let Some(line) = raw.windows(2).position(|window| window == b"\r\n") else {
            return (body, false);
        };
        let size =
            usize::from_str_radix(std::str::from_utf8(&raw[..line]).unwrap().trim(), 16).unwrap();
        raw = &raw[line + 2..];
        if size == 0 {
            return (body, true);
        }
        if raw.len() < size + 2 {
            body.extend_from_slice(&raw[..raw.len().min(size)]);
            return (body, false);
        }
        body.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

async fn fake() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

#[tokio::test]
async fn cgi_variables_and_the_body_reach_the_backend_and_the_response_streams_back() {
    let (listener, backend_address) = fake().await;
    let server = Server::start(vec![backend_address], |_| {}).await;
    let body = vec![b'x'; 200_000];
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let received = receive(&mut stream, true).await;
        let reply = records(
            &[
                (STDOUT, b"Status: 201 Created\r\nSet-Cookie: a=1\r\n"),
                (STDERR, b"PHP Notice: something\n"),
                (
                    STDOUT,
                    b"Set-Cookie: b=2\r\nContent-Type: text/plain\r\nContent-Length: 99\r\nConnection: close\r\n\r\nhel",
                ),
                (STDOUT, b"lo"),
            ],
            true,
        );
        stream.write_all(&reply).await.unwrap();
        received
    });

    let mut request = format!(
        "POST /app.php/extra/path?x=1&y=%20 HTTP/1.1\r\nHost: localhost:8443\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nCookie: c=1\r\nCookie: d=2\r\nX-Repeat: one\r\nX-Repeat: two\r\nProxy: http://evil.test\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let reply = exchange(server.address, &request).await;
    let received = backend.await.unwrap();
    let param = |name: &str| received.params.get(name).map(String::as_str);

    assert_eq!(reply.status, 201);
    assert_eq!(reply.body, b"hello");
    assert!(reply.complete);
    assert_eq!(reply.all("set-cookie"), ["a=1", "b=2"]);
    assert_eq!(reply.all("content-type"), ["text/plain"]);
    // The backend claimed 99 bytes. Its length is never trusted; a response
    // that arrived whole is sent with the length Hamster actually counted.
    assert_eq!(
        reply.all("content-length"),
        ["5"],
        "the backend's length is not trusted"
    );

    assert_eq!(received.stdin, body);
    assert_eq!(param("REQUEST_METHOD"), Some("POST"));
    assert_eq!(param("REQUEST_URI"), Some("/app.php/extra/path?x=1&y=%20"));
    assert_eq!(param("QUERY_STRING"), Some("x=1&y=%20"));
    assert_eq!(param("SCRIPT_NAME"), Some("/app.php"));
    assert_eq!(param("PATH_INFO"), Some("/extra/path"));
    assert!(param("SCRIPT_FILENAME").unwrap().ends_with("app.php"));
    assert!(
        !param("SCRIPT_FILENAME").unwrap().starts_with(r"\\?\"),
        "PHP gets a plain path"
    );
    assert!(!param("DOCUMENT_ROOT").unwrap().starts_with(r"\\?\"));
    assert_eq!(param("CONTENT_LENGTH"), Some("200000"));
    assert_eq!(param("CONTENT_TYPE"), Some("application/octet-stream"));
    assert_eq!(param("HTTP_COOKIE"), Some("c=1; d=2"));
    assert_eq!(param("HTTP_X_REPEAT"), Some("one, two"));
    assert_eq!(param("HTTP_PROXY"), None, "httpoxy");
    assert_eq!(param("SERVER_NAME"), Some("localhost"));
    assert_eq!(param("HTTP_HOST"), Some("localhost:8443"));
    assert_eq!(param("REMOTE_ADDR"), Some("127.0.0.1"));
    assert_eq!(param("SERVER_PROTOCOL"), Some("HTTP/1.1"));
    assert_eq!(param("REQUEST_SCHEME"), Some("http"));
    assert_eq!(param("HTTPS"), None);
    assert_eq!(param("REDIRECT_STATUS"), Some("200"));
    assert_eq!(param("GATEWAY_INTERFACE"), Some("CGI/1.1"));
    server.stop().await;
}

async fn one_reply(listener: TcpListener, reply: Vec<u8>) {
    let (mut stream, _) = listener.accept().await.unwrap();
    receive(&mut stream, true).await;
    stream.write_all(&reply).await.unwrap();
}

const GET: &[u8] = b"GET /app.php HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn a_location_without_status_redirects_and_a_malformed_head_is_a_bad_gateway() {
    for (reply, status) in [
        (
            records(&[(STDOUT, b"Location: /elsewhere\r\n\r\n")], true),
            302,
        ),
        (
            records(&[(STDOUT, b"Content-Type: text/plain\n\nlf only")], true),
            200,
        ),
        (records(&[], true), 502),
        (
            records(&[(STDOUT, b"not a header line\r\n\r\n")], true),
            502,
        ),
        (records(&[(STDOUT, b"Status: 99x\r\n\r\n")], true), 502),
        (
            records(&[(STDOUT, b"Content-Type: text/plain\r\n")], true),
            502,
        ),
    ] {
        let (listener, address) = fake().await;
        let server = Server::start(vec![address], |_| {}).await;
        let backend = tokio::spawn(one_reply(listener, reply));
        let answer = exchange(server.address, GET).await;
        backend.await.unwrap();
        assert_eq!(answer.status, status, "{answer:?}");
        server.stop().await;
    }
}

#[tokio::test]
async fn a_head_larger_than_the_limit_is_a_bad_gateway() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let mut head = Vec::new();
    while head.len() <= super::MAX_RESPONSE_HEAD {
        head.extend_from_slice(b"X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
    }
    let backend = tokio::spawn(one_reply(listener, records(&[(STDOUT, &head)], true)));
    let answer = exchange(server.address, GET).await;
    assert_eq!(answer.status, 502);
    backend.await.unwrap();
    assert_eq!(server.backend.snapshot().backend_failures, 1);
    server.stop().await;
}

#[tokio::test]
async fn a_silent_backend_is_a_gateway_timeout_and_frees_its_slot() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |config| {
        config.response_timeout = Duration::from_millis(500);
    })
    .await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(stream);
    });
    let started = Instant::now();
    let answer = exchange(server.address, GET).await;
    assert_eq!(answer.status, 504);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let snapshot = server.backend.snapshot();
    assert_eq!((snapshot.timeouts, snapshot.active_exchanges), (1, 0));
    backend.abort();
    server.stop().await;
}

#[tokio::test]
async fn shutdown_waits_for_a_started_silent_exchange_to_time_out_without_replay() {
    let (listener, address) = fake().await;
    let mut server = Server::start(vec![address], |config| {
        config.response_timeout = Duration::from_millis(400);
    })
    .await;
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        started_tx.send(()).unwrap();
        let _ = release_rx.await;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "a possibly executed request was replayed"
        );
    });
    let http = tokio::spawn(exchange(server.address, GET));
    started_rx.await.unwrap();

    let shutdown_started = Instant::now();
    server.shutdown.take().unwrap().send(()).unwrap();
    assert_eq!(http.await.unwrap().status, 504);
    tokio::time::timeout(Duration::from_secs(5), &mut server.task)
        .await
        .expect("server shutdown must converge after the response deadline")
        .unwrap()
        .unwrap();
    let shutdown_elapsed = shutdown_started.elapsed();
    assert_eq!(server.backend.snapshot().active_exchanges, 0);
    release_tx.send(()).unwrap();
    backend.await.unwrap();
    println!("silent endpoint: graceful shutdown completed in {shutdown_elapsed:?}");
}

#[test]
fn unicode_path_segments_never_panic_during_php_script_detection() {
    let root = Root::new(&[
        ("index.php", "front"),
        ("éabc", "ordinary"),
        ("é.php", "script"),
    ]);
    assert!(is_static(&target(&root, "/%C3%A9abc")));
    assert_eq!(script_of(target(&root, "/%C3%A9.php")).0, "/é.php");
}

#[tokio::test]
async fn a_started_request_is_not_replayed_to_another_endpoint_after_timeout() {
    let (first, first_address) = fake().await;
    let (second, second_address) = fake().await;
    let server = Server::start(vec![first_address, second_address], |config| {
        config.response_timeout = Duration::from_millis(250);
    })
    .await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = first.accept().await.unwrap();
        // This marks that the first endpoint received the complete request.
        receive(&mut stream, true).await;
        std::future::pending::<()>().await;
    });
    assert_eq!(exchange(server.address, GET).await.status, 504);
    // No second endpoint may see a request that may already have executed.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), second.accept())
            .await
            .is_err()
    );
    assert_eq!(server.backend.snapshot().timeouts, 1);
    backend.abort();
    server.stop().await;
}

#[tokio::test]
async fn a_started_request_is_not_replayed_after_backend_disconnect() {
    let (first, first_address) = fake().await;
    let (second, second_address) = fake().await;
    let server = Server::start(vec![first_address, second_address], |_| {}).await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = first.accept().await.unwrap();
        receive(&mut stream, true).await;
        // The backend may already have committed a side effect. Closing now
        // cannot authorize a second execution on the other endpoint.
    });
    assert_eq!(exchange(server.address, GET).await.status, 502);
    backend.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), second.accept())
            .await
            .is_err()
    );
    assert_eq!(server.backend.snapshot().backend_failures, 1);
    let fresh = tokio::spawn(one_reply(
        second,
        records(&[(STDOUT, b"Status: 200 OK\r\n\r\nfresh")], true),
    ));
    let answer = exchange(server.address, GET).await;
    assert_eq!(
        (answer.status, answer.body.as_slice()),
        (200, &b"fresh"[..])
    );
    fresh.await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn a_stall_after_the_head_ends_the_body_at_the_response_timeout() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |config| {
        config.response_timeout = Duration::from_millis(500);
    })
    .await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        stream
            .write_all(&records(
                &[(STDOUT, b"Content-Type: text/plain\r\n\r\npartial")],
                false,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(stream);
    });
    let answer = exchange(server.address, GET).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, b"partial");
    assert!(
        !answer.complete,
        "a stalled response must not look complete"
    );
    assert_eq!(server.backend.snapshot().active_exchanges, 0);
    backend.abort();
    server.stop().await;
}

#[tokio::test]
async fn a_backend_that_closes_mid_body_never_produces_a_complete_response() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        stream
            .write_all(&records(
                &[(STDOUT, b"Content-Type: text/plain\r\n\r\nhalf")],
                false,
            ))
            .await
            .unwrap();
    });
    let raw = raw_exchange(server.address, GET).await;
    backend.await.unwrap();
    // A close that has already arrived with the head is seen before anything
    // reaches the client, which then gets a 502. A later close fails the body
    // after commit: Hyper closes the connection and may not flush what it had
    // buffered, so the client can see nothing at all. Whatever it sees, a
    // response never looks complete.
    if !raw.is_empty() {
        let answer = parse_reply(&raw);
        if answer.status != 502 {
            assert_eq!(answer.status, 200);
            assert!(b"half".starts_with(&answer.body));
            assert!(!answer.complete);
        }
    }
    assert_eq!(server.backend.snapshot().backend_failures, 1);
    server.stop().await;
}

#[tokio::test]
async fn a_refused_connection_moves_to_the_next_server_and_all_refused_is_a_bad_gateway() {
    let dead = crate::test_ports::unused_outside_dynamic_range();
    let (listener, live) = fake().await;
    let server = Server::start(vec![dead, live], |_| {}).await;
    let backend = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive(&mut stream, true).await;
            stream
                .write_all(&records(&[(STDOUT, b"Status: 200 OK\r\n\r\nlive")], true))
                .await
                .unwrap();
        }
    });
    for _ in 0..3 {
        let answer = exchange(server.address, GET).await;
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"live"[..]));
    }
    backend.await.unwrap();
    assert!(server.backend.snapshot().connect_failures >= 1);
    server.stop().await;

    let server = Server::start(vec![dead], |_| {}).await;
    let answer = exchange(server.address, GET).await;
    assert_eq!(answer.status, 502);
    assert_eq!(server.backend.snapshot().active_exchanges, 0);
    server.stop().await;
}

#[tokio::test]
async fn operator_managed_endpoint_recovers_without_restarting_hamster() {
    let address = crate::test_ports::unused_outside_dynamic_range();
    let server = Server::start(vec![address], |_| {}).await;

    // A FastCGI endpoint is operator-owned: its absence must not prevent the
    // HTTP listener from starting, and a refused connect must not poison the
    // configured endpoint for later requests.
    assert_eq!(exchange(server.address, GET).await.status, 502);
    assert_eq!(server.backend.snapshot().active_exchanges, 0);

    let listener = TcpListener::bind(address).await.unwrap();
    let backend = tokio::spawn(one_reply(
        listener,
        records(&[(STDOUT, b"Status: 200 OK\r\n\r\nrecovered")], true),
    ));
    let answer = exchange(server.address, GET).await;
    assert_eq!(
        (answer.status, answer.body.as_slice()),
        (200, &b"recovered"[..])
    );
    backend.await.unwrap();
    assert_eq!(server.backend.snapshot().active_exchanges, 0);
    server.stop().await;
}

#[tokio::test]
async fn slots_bound_concurrency_and_a_request_that_cannot_get_one_is_refused() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |config| {
        config.connect_timeout = Duration::from_millis(500);
    })
    .await;
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        stream
            .write_all(&records(&[(STDOUT, b"Status: 200 OK\r\n\r\nslow")], true))
            .await
            .unwrap();
        // The refused request never reaches the backend.
        tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err()
    });
    let address = server.address;
    let holder = tokio::spawn(async move { exchange(address, GET).await });
    started_rx.await.unwrap();
    let refused = exchange(server.address, GET).await;
    assert_eq!(refused.status, 503);
    assert_eq!(refused.body, b"FastCGI Busy");
    release_tx.send(()).unwrap();
    assert_eq!(holder.await.unwrap().body, b"slow");
    assert!(backend.await.unwrap(), "only one exchange at a time");
    assert_eq!(server.backend.snapshot().busy_refusals, 1);
    server.stop().await;
}

#[cfg(windows)]
#[tokio::test]
async fn canceled_blocking_connect_keeps_its_slot_until_the_attempt_exits() {
    let root = Root::new(&[("index.php", "front")]);
    let backend = FastCgiBackend::new(FastCgiConfig {
        label: Arc::from("test/"),
        document_root: root.path.clone(),
        index: "index.php".to_owned(),
        servers: vec!["127.0.0.1:32000".parse().unwrap()],
        concurrency: 1,
        connect_timeout: Duration::from_secs(1),
        response_timeout: Duration::from_secs(1),
        keep_connections: false,
        resolve_cache: Duration::ZERO,
    });
    let permit = Arc::clone(&backend.slots).acquire_owned().await.unwrap();
    backend
        .counters
        .active_exchanges
        .fetch_add(1, AtomicOrdering::AcqRel);
    let server = backend.claim_server(&[]).unwrap();
    let attempt = super::ConnectAttempt::new(
        &backend,
        server,
        super::ExchangePermit {
            backend: Arc::clone(&backend),
            _permit: permit,
        },
    );
    let claim = Arc::new(std::sync::Mutex::new(Some(attempt)));
    let worker_claim = Arc::clone(&claim);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let pending = super::PendingConnect {
        claim,
        task: tokio::task::spawn_blocking(move || {
            let attempt = worker_claim.lock().unwrap().take().unwrap();
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(attempt);
            finished_tx.send(()).unwrap();
            None
        }),
    };
    started_rx.await.unwrap();
    drop(pending); // The closure has taken ownership, so abort cannot release it early.
    assert_eq!(backend.slots.available_permits(), 0);
    assert_eq!(backend.in_flight[0].load(AtomicOrdering::Acquire), 1);
    assert_eq!(backend.snapshot().active_exchanges, 1);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), finished_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(backend.slots.available_permits(), 1);
    assert_eq!(backend.in_flight[0].load(AtomicOrdering::Acquire), 0);
    assert_eq!(backend.snapshot().active_exchanges, 0);
}

#[cfg(windows)]
#[test]
fn queued_connect_timeout_and_cancellation_release_slots_before_blocking_pool_recovers() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.await.unwrap();

        let (listener, address) = fake().await;
        let root = Root::new(&[("index.php", "front")]);
        let backend = FastCgiBackend::new(FastCgiConfig {
            label: Arc::from("queued-connect/"),
            document_root: root.path.clone(),
            index: "index.php".to_owned(),
            servers: vec![address],
            concurrency: 1,
            connect_timeout: Duration::from_millis(100),
            response_timeout: Duration::from_secs(1),
            keep_connections: false,
            resolve_cache: Duration::ZERO,
        });
        let permit = Arc::clone(&backend.slots).acquire_owned().await.unwrap();
        backend
            .counters
            .active_exchanges
            .fetch_add(1, AtomicOrdering::AcqRel);
        let server = backend.claim_server(&[]).unwrap();
        let attempt = super::ConnectAttempt::new(
            &backend,
            server,
            super::ExchangePermit {
                backend: Arc::clone(&backend),
                _permit: permit,
            },
        );
        let started = Instant::now();
        let outcome = super::connect_server(&backend, address, attempt).await;
        let waiter_elapsed = started.elapsed();
        assert!(matches!(outcome, super::ConnectOutcome::Detached(_)));

        // The queued closure has not started. A canceled waiter must revoke
        // it and release the claim without waiting for unrelated blockers.
        let timed_out_reclaimed = tokio::time::timeout(
            Duration::from_millis(250),
            Arc::clone(&backend.slots).acquire_owned(),
        )
        .await;
        let timeout_reclaim_elapsed = started.elapsed();
        assert_eq!(backend.blocking_connect_slots.available_permits(), 0);
        let Ok(permit) = timed_out_reclaimed else {
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            panic!("a timed-out queued connect retained the slot behind the blocker");
        };
        drop(permit);

        let permit = Arc::clone(&backend.slots).acquire_owned().await.unwrap();
        backend
            .counters
            .active_exchanges
            .fetch_add(1, AtomicOrdering::AcqRel);
        let server = backend.claim_server(&[]).unwrap();
        let attempt = super::ConnectAttempt::new(
            &backend,
            server,
            super::ExchangePermit {
                backend: Arc::clone(&backend),
                _permit: permit,
            },
        );
        let fallback_started = Instant::now();
        let fallback = super::connect_server(&backend, address, attempt).await;
        let fallback_elapsed = fallback_started.elapsed();
        assert!(
            !matches!(fallback, super::ConnectOutcome::Detached(_)),
            "a full blocking-connect queue must use cancellable async I/O"
        );
        drop(fallback);
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        println!(
            "queued connect: timeout waiter={waiter_elapsed:?}, timeout reclaim={timeout_reclaim_elapsed:?}, async fallback={fallback_elapsed:?}, reclaimed_before_blocker_release=true"
        );
        assert_eq!(backend.snapshot().active_exchanges, 0);
        assert_eq!(backend.in_flight[0].load(AtomicOrdering::Acquire), 0);
        let blocking_permit = tokio::time::timeout(
            Duration::from_secs(1),
            Arc::clone(&backend.blocking_connect_slots).acquire_owned(),
        )
        .await
        .expect("queued closure must eventually release its queue capacity")
        .unwrap();
        drop(blocking_permit);

        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_second_tx, release_second_rx) = std::sync::mpsc::channel();
        let second_blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_second_rx.recv().unwrap();
        });
        entered_rx.await.unwrap();
        let permit = Arc::clone(&backend.slots).acquire_owned().await.unwrap();
        backend
            .counters
            .active_exchanges
            .fetch_add(1, AtomicOrdering::AcqRel);
        let server = backend.claim_server(&[]).unwrap();
        let attempt = super::ConnectAttempt::new(
            &backend,
            server,
            super::ExchangePermit {
                backend: Arc::clone(&backend),
                _permit: permit,
            },
        );
        let queued_canceled_at = Instant::now();
        let mut waiting = Box::pin(super::connect_server(&backend, address, attempt));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(waiting.as_mut(), &mut context).is_pending());
        assert_eq!(backend.blocking_connect_slots.available_permits(), 0);
        drop(waiting); // A client cancels before the queued closure can start.
        let queued_cancel_reclaimed = tokio::time::timeout(
            Duration::from_millis(250),
            Arc::clone(&backend.slots).acquire_owned(),
        )
        .await;
        let queued_cancel_elapsed = queued_canceled_at.elapsed();
        release_second_tx.send(()).unwrap();
        second_blocker.await.unwrap();
        println!(
            "queued cancellation reclaimed={queued_cancel_elapsed:?}, before_blocker_release={}",
            queued_cancel_reclaimed.is_ok()
        );
        assert!(queued_cancel_reclaimed.is_ok());
        drop(queued_cancel_reclaimed.unwrap());
        assert_eq!(backend.snapshot().active_exchanges, 0);
        assert_eq!(backend.in_flight[0].load(AtomicOrdering::Acquire), 0);

        // An async fallback can complete a TCP handshake just as its HTTP
        // waiter disappears. A connection is harmless; no FastCGI frame may
        // follow cancellation, including from the queued blocking closures.
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
        {
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte))
                .await
                .expect("canceled connection must close")
                .unwrap();
            assert_eq!(read, 0, "a FastCGI frame arrived after cancellation");
        }
    });
}

#[tokio::test]
async fn a_chunked_request_body_needs_a_length() {
    let server = Server::start(
        vec![crate::test_ports::unused_outside_dynamic_range()],
        |_| {},
    )
    .await;
    let answer = exchange(
        server.address,
        b"POST /app.php HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(answer.status, 411);
    server.stop().await;
}

/// A script may answer, and even finish, before reading its input. The body
/// is written by its own task, so neither side waits for the other.
#[tokio::test]
async fn a_backend_that_answers_before_reading_its_input_does_not_deadlock() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let response = vec![b'r'; 1024 * 1024];
    let expected = response.clone();
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, false).await;
        let mut head = b"Content-Type: application/octet-stream\r\n\r\n".to_vec();
        head.extend_from_slice(&response);
        stream
            .write_all(&records(&[(STDOUT, &head)], true))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
    let body = vec![b'x'; 4 * 1024 * 1024];
    let mut request = format!(
        "POST /app.php HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let answer = exchange(server.address, &request).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, expected);
    assert!(answer.complete);
    backend.await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn head_requests_get_the_script_head_without_a_body() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let received = receive(&mut stream, true).await;
        stream
            .write_all(&records(&[(STDOUT, b"X-Script: yes\r\n\r\nbody")], true))
            .await
            .unwrap();
        received
    });
    let answer = exchange(
        server.address,
        b"HEAD /app.php HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(backend.await.unwrap().params["REQUEST_METHOD"], "HEAD");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.all("x-script"), ["yes"]);
    assert!(answer.body.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn static_files_are_served_without_the_backend() {
    let server = Server::start(
        vec![crate::test_ports::unused_outside_dynamic_range()],
        |_| {},
    )
    .await;
    let answer = exchange(
        server.address,
        b"GET /style.css HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, b"body { color: red }");
    assert!(answer.all("content-type")[0].starts_with("text/css"));
    let refused = exchange(
        server.address,
        b"POST /style.css HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(refused.status, 405);
    let snapshot = server.backend.snapshot();
    assert_eq!((snapshot.static_files, snapshot.scripts), (2, 0));
    assert_eq!(
        snapshot.connect_failures, 0,
        "the backend was never contacted"
    );
    server.stop().await;
}

#[test]
fn backend_paths_drop_the_verbatim_prefix() {
    assert_eq!(
        super::display_path(Path::new(r"\\?\C:\www\app")),
        r"C:\www\app"
    );
    assert_eq!(
        super::display_path(Path::new(r"\\?\UNC\server\share\app")),
        r"\\server\share\app"
    );
    assert_eq!(super::display_path(Path::new("/srv/app")), "/srv/app");
}

// ----- Kept connections -----------------------------------------------------

/// Reads one kept-connection request from a reader that persists across the
/// connection's exchanges: BEGIN_REQUEST must ask for FCGI_KEEP_CONN.
async fn receive_kept<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut RecordReader<R>,
    read_stdin: bool,
) -> Received {
    let begin = reader.next().await.unwrap().unwrap();
    assert_eq!(begin.kind, BEGIN_REQUEST);
    assert_eq!(
        &begin.content[..3],
        &[0, 1, KEEP_CONN],
        "responder role, keep-conn"
    );
    let mut params = Vec::new();
    loop {
        let record = reader.next().await.unwrap().unwrap();
        assert_eq!(record.kind, PARAMS);
        if record.content.is_empty() {
            break;
        }
        params.extend_from_slice(&record.content);
    }
    let mut received = Received {
        params: decode_params(&params),
        stdin: Vec::new(),
    };
    if read_stdin {
        loop {
            let record = reader.next().await.unwrap().unwrap();
            assert_eq!(record.kind, STDIN);
            if record.content.is_empty() {
                break;
            }
            received.stdin.extend_from_slice(&record.content);
        }
    }
    received
}

fn keep(config: &mut FastCgiConfig) {
    config.keep_connections = true;
}

fn ok_reply(body: &[u8]) -> Vec<u8> {
    let mut head = b"Content-Type: text/plain\r\n\r\n".to_vec();
    head.extend_from_slice(body);
    records(&[(STDOUT, &head)], true)
}

#[tokio::test]
async fn kept_connections_carry_consecutive_requests_and_their_bodies() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], keep).await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.split();
        let mut reader = RecordReader::new(read);
        let mut bodies = Vec::new();
        for round in 0..3_u8 {
            let received = receive_kept(&mut reader, true).await;
            bodies.push(received.stdin);
            write.write_all(&ok_reply(&[b'0' + round])).await.unwrap();
        }
        let second = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
        (bodies, second.is_err())
    });

    let get = exchange(server.address, GET).await;
    let post = exchange(
        server.address,
        b"POST /app.php HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    let again = exchange(server.address, GET).await;
    let (bodies, no_second_connection) = backend.await.unwrap();

    assert_eq!((get.status, get.body.as_slice()), (200, &b"0"[..]));
    assert_eq!((post.status, post.body.as_slice()), (200, &b"1"[..]));
    assert_eq!((again.status, again.body.as_slice()), (200, &b"2"[..]));
    assert_eq!(bodies, [b"".to_vec(), b"hello".to_vec(), b"".to_vec()]);
    assert!(no_second_connection);
    let snapshot = server.backend.snapshot();
    assert_eq!(
        (snapshot.connections_opened, snapshot.connections_reused),
        (1, 2)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_kept_connection_the_backend_closed_while_idle_is_replaced() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], keep).await;
    let backend = tokio::spawn(async move {
        for round in 0..2_u8 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = RecordReader::new(&mut stream);
            receive_kept(&mut reader, true).await;
            drop(reader);
            stream.write_all(&ok_reply(&[b'0' + round])).await.unwrap();
            // Closing after the answer leaves Hamster an idle connection
            // that is already dead when the next request arrives.
        }
    });

    let first = exchange(server.address, GET).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = exchange(server.address, GET).await;
    backend.await.unwrap();

    assert_eq!((first.status, first.body.as_slice()), (200, &b"0"[..]));
    assert_eq!((second.status, second.body.as_slice()), (200, &b"1"[..]));
    let snapshot = server.backend.snapshot();
    assert_eq!(
        (snapshot.connections_opened, snapshot.connections_reused),
        (2, 0)
    );
    assert_eq!(snapshot.backend_failures, 0);
    server.stop().await;
}

#[tokio::test]
async fn a_connection_is_not_kept_when_its_request_body_was_not_all_sent() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], keep).await;
    let backend = tokio::spawn(async move {
        // The first script answers without reading its input, so the body
        // writer is still sending when END_REQUEST arrives.
        let (mut first, _) = listener.accept().await.unwrap();
        let mut reader = RecordReader::new(&mut first);
        receive_kept(&mut reader, false).await;
        drop(reader);
        first.write_all(&ok_reply(b"early")).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let mut reader = RecordReader::new(&mut second);
        receive_kept(&mut reader, true).await;
        drop(reader);
        second.write_all(&ok_reply(b"fresh")).await.unwrap();
        drop(first);
    });

    let body = vec![b'x'; 4 * 1024 * 1024];
    let mut request = format!(
        "POST /app.php HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let early = exchange(server.address, &request).await;
    let fresh = exchange(server.address, GET).await;
    backend.await.unwrap();

    assert_eq!((early.status, early.body.as_slice()), (200, &b"early"[..]));
    assert_eq!((fresh.status, fresh.body.as_slice()), (200, &b"fresh"[..]));
    let snapshot = server.backend.snapshot();
    assert_eq!(
        (snapshot.connections_opened, snapshot.connections_reused),
        (2, 0)
    );
    server.stop().await;
}

#[tokio::test]
async fn bytes_after_end_request_keep_the_connection_from_being_reused() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], keep).await;
    let backend = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut reader = RecordReader::new(&mut first);
        receive_kept(&mut reader, true).await;
        drop(reader);
        let mut reply = ok_reply(b"one");
        record(STDOUT, b"stray", &mut reply);
        first.write_all(&reply).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let mut reader = RecordReader::new(&mut second);
        receive_kept(&mut reader, true).await;
        drop(reader);
        second.write_all(&ok_reply(b"two")).await.unwrap();
        drop(first);
    });

    let one = exchange(server.address, GET).await;
    let two = exchange(server.address, GET).await;
    backend.await.unwrap();

    assert_eq!((one.status, one.body.as_slice()), (200, &b"one"[..]));
    assert_eq!((two.status, two.body.as_slice()), (200, &b"two"[..]));
    assert_eq!(server.backend.snapshot().connections_opened, 2);
    server.stop().await;
}

#[tokio::test]
async fn without_keep_connections_every_exchange_opens_its_own_connection() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let backend = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive(&mut stream, true).await;
            stream.write_all(&ok_reply(b"x")).await.unwrap();
        }
    });
    assert_eq!(exchange(server.address, GET).await.status, 200);
    assert_eq!(exchange(server.address, GET).await.status, 200);
    backend.await.unwrap();
    let snapshot = server.backend.snapshot();
    assert_eq!(
        (snapshot.connections_opened, snapshot.connections_reused),
        (2, 0)
    );
    server.stop().await;
}

// ----- Resolution cache -----------------------------------------------------

async fn answer_scripts(listener: TcpListener, count: usize) {
    for _ in 0..count {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        stream.write_all(&ok_reply(b"ran")).await.unwrap();
    }
}

#[tokio::test]
async fn a_script_resolution_is_reused_only_until_resolve_cache_expires() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |config| {
        config.resolve_cache = Duration::from_millis(500);
    })
    .await;
    let backend = tokio::spawn(answer_scripts(listener, 2));

    assert_eq!(exchange(server.address, GET).await.status, 200);
    std::fs::remove_file(server._root.path.join("app.php")).unwrap();
    // Within the cache period the script that was found is still run; PHP
    // itself would report the missing file.
    assert_eq!(exchange(server.address, GET).await.status, 200);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Once it expires, the path is resolved again and the backend is not
    // contacted for a script that no longer exists.
    assert_eq!(exchange(server.address, GET).await.status, 404);
    backend.await.unwrap();

    let snapshot = server.backend.snapshot();
    assert_eq!((snapshot.scripts, snapshot.resolutions_reused), (2, 1));
    server.stop().await;
}

#[tokio::test]
async fn without_resolve_cache_every_request_resolves_afresh() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |_| {}).await;
    let backend = tokio::spawn(answer_scripts(listener, 1));

    assert_eq!(exchange(server.address, GET).await.status, 200);
    std::fs::remove_file(server._root.path.join("app.php")).unwrap();
    assert_eq!(exchange(server.address, GET).await.status, 404);
    backend.await.unwrap();

    assert_eq!(server.backend.snapshot().resolutions_reused, 0);
    server.stop().await;
}

#[test]
fn the_resolution_cache_stays_bounded_whatever_paths_clients_send() {
    let root = Root::new(&[("index.php", "front")]);
    let backend = FastCgiBackend::new(FastCgiConfig {
        label: Arc::from("bounded/"),
        document_root: root.path.clone(),
        index: "index.php".to_owned(),
        servers: vec!["127.0.0.1:9".parse().unwrap()],
        concurrency: 1,
        connect_timeout: Duration::from_secs(1),
        response_timeout: Duration::from_secs(1),
        keep_connections: true,
        resolve_cache: Duration::from_secs(60),
    });
    let script = root.path.join("index.php");
    for request in 0..super::RESOLVE_CACHE_ENTRIES + 10 {
        backend.remember_script(&format!("/{request}"), &script, "/index.php", "");
        assert!(backend.resolved.lock().unwrap().len() <= super::RESOLVE_CACHE_ENTRIES);
    }
    assert!(
        backend.cached_script("/0").is_none(),
        "the full cache was dropped"
    );
    assert!(
        backend
            .cached_script(&format!("/{}", super::RESOLVE_CACHE_ENTRIES + 9))
            .is_some()
    );
}
