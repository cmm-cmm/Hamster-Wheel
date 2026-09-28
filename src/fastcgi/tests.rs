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
    BEGIN_REQUEST, END_REQUEST, FastCgiBackend, FastCgiConfig, PARAMS, RecordReader, STDERR, STDIN,
    STDOUT, Target, record, resolve,
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
    assert!(
        reply.all("content-length").is_empty(),
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
    // Hyper closes the connection when a body fails and may not flush what it
    // had buffered, so the client can see nothing at all. Whatever it sees,
    // the response never looks complete.
    if !raw.is_empty() {
        let answer = parse_reply(&raw);
        assert_eq!(answer.status, 200);
        assert!(b"half".starts_with(&answer.body));
        assert!(!answer.complete);
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
async fn slots_bound_concurrency_and_a_request_that_cannot_get_one_is_refused() {
    let (listener, address) = fake().await;
    let server = Server::start(vec![address], |config| {
        config.connect_timeout = Duration::from_millis(500);
    })
    .await;
    let backend = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive(&mut stream, true).await;
        tokio::time::sleep(Duration::from_millis(1_500)).await;
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
    tokio::time::sleep(Duration::from_millis(200)).await;
    let refused = exchange(server.address, GET).await;
    assert_eq!(refused.status, 503);
    assert_eq!(refused.body, b"FastCGI Busy");
    assert_eq!(holder.await.unwrap().body, b"slow");
    assert!(backend.await.unwrap(), "only one exchange at a time");
    assert_eq!(server.backend.snapshot().busy_refusals, 1);
    server.stop().await;
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
