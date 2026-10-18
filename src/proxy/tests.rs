//! Reverse proxy regressions over real sockets.
//!
//! Upstreams here are raw TCP scripts rather than an HTTP library, so a test
//! can see exactly which bytes, framing and EOF the upstream received, and can
//! stall or crash at a precise point. Each test locks an invariant, not a
//! detail of how Hamster happens to implement it.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use hyper::header::HeaderValue;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

use super::{ProxyConfig, Upstream, UpstreamSnapshot};
use crate::{
    config::HttpLimits,
    router::{Handler, HostRouter, LocationRouter, VirtualHost},
    server::serve_with_router,
};

/// Test-only fault injection, reached through `Upstream::faults`.
#[derive(Default)]
pub(super) struct Faults {
    read_eofs: AtomicUsize,
    connect_delay_ms: AtomicU64,
}

impl Faults {
    pub(super) fn take_read_eof(&self) -> bool {
        self.read_eofs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |armed| {
                armed.checked_sub(1)
            })
            .is_ok()
    }

    pub(super) fn connect_delay(&self) -> Option<Duration> {
        match self.connect_delay_ms.load(Ordering::Acquire) {
            0 => None,
            milliseconds => Some(Duration::from_millis(milliseconds)),
        }
    }

    /// The next `count` reads on this upstream's connections return EOF, as
    /// if the upstream had closed a keep-alive connection. A parked idle
    /// connection does not read until a request wakes its driver, so the EOF
    /// lands exactly in the stale keep-alive window.
    fn arm_read_eofs(&self, count: usize) {
        self.read_eofs.store(count, Ordering::Release);
    }

    fn delay_connect(&self, delay: Duration) {
        self.connect_delay_ms.store(
            u64::try_from(delay.as_millis()).expect("test delay fits"),
            Ordering::Release,
        );
    }
}

const GUARD: Duration = Duration::from_secs(10);

fn proxy_config(address: SocketAddr) -> ProxyConfig {
    ProxyConfig {
        label: Arc::from("127.0.0.1/"),
        address,
        authority: HeaderValue::from_str(&address.to_string()).expect("authority"),
        connect_timeout: Duration::from_secs(2),
        response_header_timeout: Duration::from_secs(5),
        progress_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(30),
        max_idle: 8,
        preserve_host: true,
        max_buf_size: 64 * 1_024,
    }
}

fn upstream_with(address: SocketAddr, tweak: impl FnOnce(&mut ProxyConfig)) -> Arc<Upstream> {
    let mut config = proxy_config(address);
    tweak(&mut config);
    Upstream::new(config)
}

struct Proxy {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    server: JoinHandle<std::io::Result<()>>,
}

impl Proxy {
    async fn start(upstream: &Arc<Upstream>) -> Self {
        Self::start_hosts(vec![("127.0.0.1", Arc::clone(upstream))], limits()).await
    }

    async fn start_hosts(hosts: Vec<(&str, Arc<Upstream>)>, limits: HttpLimits) -> Self {
        let virtual_hosts = hosts
            .into_iter()
            .map(|(name, upstream)| {
                let locations = Arc::new(
                    LocationRouter::new(
                        vec![("/", Handler::proxy(upstream))],
                        Handler::not_found(),
                    )
                    .expect("valid locations"),
                );
                VirtualHost::new(name, locations).expect("valid host")
            })
            .collect();
        let router = Arc::new(HostRouter::new(virtual_hosts).expect("valid router"));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(listener, router, limits, async move {
            let _ = shutdown_rx.await;
        }));
        Self {
            address,
            shutdown: Some(shutdown),
            server,
        }
    }

    async fn stop(mut self) -> Duration {
        let started = Instant::now();
        let _ = self.shutdown.take().expect("running").send(());
        timeout(GUARD, &mut self.server)
            .await
            .expect("proxy stops within the guard")
            .expect("server task")
            .expect("server result");
        started.elapsed()
    }
}

fn limits() -> HttpLimits {
    HttpLimits::for_test(
        64,
        Duration::from_secs(5),
        Duration::from_secs(5),
        64 * 1_024,
    )
}

/// An upstream listener. A small receive buffer, inherited by accepted
/// sockets, keeps kernel buffering small enough for backpressure to show.
async fn upstream_listener(receive_buffer: Option<u32>) -> TcpListener {
    let socket = TcpSocket::new_v4().expect("upstream socket");
    if let Some(size) = receive_buffer {
        socket.set_recv_buffer_size(size).expect("receive buffer");
    }
    socket
        .bind((Ipv4Addr::LOCALHOST, 0).into())
        .expect("bind upstream");
    socket.listen(64).expect("listen upstream")
}

async fn accept(listener: &TcpListener) -> TcpStream {
    timeout(GUARD, listener.accept())
        .await
        .expect("upstream connection within the guard")
        .expect("accept upstream")
        .0
}

async fn no_connection_within(listener: &TcpListener, window: Duration) -> bool {
    timeout(window, listener.accept()).await.is_err()
}

async fn client(address: SocketAddr) -> TcpStream {
    TcpStream::connect(address).await.expect("connect proxy")
}

async fn small_client(address: SocketAddr, send: u32, receive: u32) -> TcpStream {
    let socket = TcpSocket::new_v4().expect("client socket");
    socket.set_send_buffer_size(send).expect("send buffer");
    socket
        .set_recv_buffer_size(receive)
        .expect("receive buffer");
    socket.connect(address).await.expect("connect proxy")
}

/// Reads one message head, leaving any bytes after it in `buffer`.
/// Returns `None` on EOF before any byte of a head.
async fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8(buffer[..end + 4].to_vec()).expect("ASCII head");
            buffer.drain(..end + 4);
            return Some(head);
        }
        let mut chunk = [0_u8; 4_096];
        let count = timeout(GUARD, stream.read(&mut chunk))
            .await
            .expect("head within the guard")
            .unwrap_or(0);
        if count == 0 {
            assert!(buffer.is_empty(), "EOF inside a message head");
            return None;
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
}

async fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>, needed: usize) -> bool {
    while buffer.len() < needed {
        let mut chunk = [0_u8; 16 * 1_024];
        let count = timeout(GUARD, stream.read(&mut chunk))
            .await
            .expect("body within the guard")
            .unwrap_or(0);
        if count == 0 {
            return false;
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
    true
}

async fn read_exact_body(stream: &mut TcpStream, buffer: &mut Vec<u8>, length: usize) -> Vec<u8> {
    assert!(
        fill(stream, buffer, length).await,
        "EOF inside a sized body"
    );
    buffer.drain(..length).collect()
}

/// Decodes a chunked body. Returns the data and the raw trailer section.
async fn read_chunked(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> (Vec<u8>, String) {
    let mut data = Vec::new();
    loop {
        let line_end = loop {
            if let Some(end) = buffer.windows(2).position(|window| window == b"\r\n") {
                break end;
            }
            let needed = buffer.len() + 1;
            assert!(fill(stream, buffer, needed).await, "EOF inside chunk size");
        };
        let line = String::from_utf8(buffer[..line_end].to_vec()).expect("ASCII chunk size");
        buffer.drain(..line_end + 2);
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .expect("hex chunk size");
        if size == 0 {
            let mut trailers = String::new();
            loop {
                let end = loop {
                    if let Some(end) = buffer.windows(2).position(|window| window == b"\r\n") {
                        break end;
                    }
                    let needed = buffer.len() + 1;
                    assert!(fill(stream, buffer, needed).await, "EOF inside trailers");
                };
                let line = String::from_utf8(buffer[..end].to_vec()).expect("ASCII trailer");
                buffer.drain(..end + 2);
                if line.is_empty() {
                    return (data, trailers);
                }
                trailers.push_str(&line);
                trailers.push('\n');
            }
        }
        assert!(
            fill(stream, buffer, size + 2).await,
            "EOF inside chunk data"
        );
        data.extend_from_slice(&buffer[..size]);
        buffer.drain(..size + 2);
    }
}

fn header_values(head: &str, name: &str) -> Vec<String> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .filter(|(field, _)| field.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
        .collect()
}

fn header(head: &str, name: &str) -> Option<String> {
    let values = header_values(head, name);
    assert!(values.len() <= 1, "{name} repeated in {head}");
    values.into_iter().next()
}

fn status(head: &str) -> u16 {
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code")
}

/// Reads a request the way an origin server would: head plus framed body.
async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    let head = read_head(stream, buffer).await?;
    let body = if header(&head, "transfer-encoding").is_some() {
        read_chunked(stream, buffer).await.0
    } else if let Some(length) = header(&head, "content-length") {
        read_exact_body(stream, buffer, length.parse().expect("length")).await
    } else {
        Vec::new()
    };
    Some((head, body))
}

/// Reads a response the way a client would.
async fn read_response(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    bodiless: bool,
) -> (String, Vec<u8>) {
    let head = read_head(stream, buffer)
        .await
        .expect("a response head before EOF");
    if bodiless || matches!(status(&head), 204 | 304) {
        return (head, Vec::new());
    }
    let body = if header(&head, "transfer-encoding").is_some() {
        read_chunked(stream, buffer).await.0
    } else if let Some(length) = header(&head, "content-length") {
        read_exact_body(stream, buffer, length.parse().expect("length")).await
    } else {
        let mut rest = std::mem::take(buffer);
        timeout(GUARD, stream.read_to_end(&mut rest))
            .await
            .expect("close-delimited body")
            .expect("read close-delimited body");
        rest
    };
    (head, body)
}

async fn get(address: SocketAddr, host: &str, target: &str) -> (String, Vec<u8>) {
    let mut stream = client(address).await;
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    read_response(&mut stream, &mut Vec::new(), false).await
}

/// Counts accepted connections and requests; answers every request with
/// `ok` on a keep-alive connection until the proxy closes it.
#[derive(Default)]
struct Responder {
    connections: AtomicUsize,
    requests: AtomicUsize,
    bodies: std::sync::Mutex<Vec<Vec<u8>>>,
}

fn spawn_responder(listener: TcpListener) -> (Arc<Responder>, JoinHandle<()>) {
    let responder = Arc::new(Responder::default());
    let state = Arc::clone(&responder);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            state.connections.fetch_add(1, Ordering::SeqCst);
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                while let Some((head, body)) = read_request(&mut stream, &mut buffer).await {
                    state.requests.fetch_add(1, Ordering::SeqCst);
                    state.bodies.lock().expect("bodies").push(body);
                    let reply: &[u8] = if head.starts_with("HEAD ") {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n"
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                    };
                    if stream.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (responder, task)
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < GUARD,
            "{what} did not happen within the guard"
        );
        sleep(Duration::from_millis(10)).await;
    }
    started.elapsed()
}

async fn quiesced(upstream: &Upstream) -> UpstreamSnapshot {
    eventually("exchange release", || {
        upstream.snapshot().active_exchanges == 0
    })
    .await;
    upstream.snapshot()
}

// ----- HTTP rewriting --------------------------------------------------------

#[tokio::test]
async fn strips_connection_nominated_and_hop_by_hop_fields_in_both_directions() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = client(proxy.address).await;
    stream
        .write_all(
            b"GET /h HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive, X-Hop\r\nX-Hop: secret\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nTE: gzip\r\nProxy-Authorization: Basic Zm9v\r\nX-Keep: yes\r\n\r\n",
        )
        .await
        .unwrap();

    let mut origin = accept(&listener).await;
    let mut buffer = Vec::new();
    let head = read_head(&mut origin, &mut buffer).await.unwrap();
    for removed in [
        "x-hop",
        "keep-alive",
        "proxy-connection",
        "te",
        "proxy-authorization",
        "connection",
        "upgrade",
    ] {
        assert!(
            header(&head, removed).is_none(),
            "{removed} crossed: {head}"
        );
    }
    assert_eq!(header(&head, "x-keep").as_deref(), Some("yes"));

    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: X-Resp\r\nX-Resp: internal\r\nKeep-Alive: timeout=9\r\nProxy-Authenticate: Basic\r\nX-Other: ok\r\n\r\nok")
        .await
        .unwrap();
    let (response, body) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&response), 200);
    assert_eq!(body, b"ok");
    for removed in ["x-resp", "keep-alive", "proxy-authenticate"] {
        assert!(
            header(&response, removed).is_none(),
            "{removed} crossed: {response}"
        );
    }
    assert_eq!(header(&response, "x-other").as_deref(), Some("ok"));
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn relays_request_and_response_trailers() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST /t HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nTrailer: x-checksum\r\nTE: trailers\r\n\r\n5\r\nhello\r\n0\r\nx-checksum: abc\r\n\r\n")
        .await
        .unwrap();

    let mut origin = accept(&listener).await;
    let mut buffer = Vec::new();
    let head = read_head(&mut origin, &mut buffer).await.unwrap();
    assert_eq!(header(&head, "trailer").as_deref(), Some("x-checksum"));
    assert_eq!(header(&head, "te").as_deref(), Some("trailers"));
    let (data, trailers) = read_chunked(&mut origin, &mut buffer).await;
    assert_eq!(data, b"hello");
    assert!(trailers.contains("x-checksum: abc"), "{trailers:?}");

    origin
        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: x-digest\r\n\r\n2\r\nok\r\n0\r\nx-digest: 42\r\n\r\n")
        .await
        .unwrap();
    let mut buffer = Vec::new();
    let response = read_head(&mut stream, &mut buffer).await.unwrap();
    assert_eq!(status(&response), 200);
    assert_eq!(header(&response, "trailer").as_deref(), Some("x-digest"));
    let (body, trailers) = read_chunked(&mut stream, &mut buffer).await;
    assert_eq!(body, b"ok");
    assert!(trailers.contains("x-digest: 42"), "{trailers:?}");
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn replaces_client_supplied_forwarding_metadata() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Forwarded-For: 203.0.113.9\r\nX-Forwarded-For: 198.51.100.1\r\nForwarded: for=203.0.113.9;proto=https\r\nX-Real-IP: 203.0.113.9\r\nX-Forwarded-Host: evil.test\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Port: 443\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let head = read_head(&mut origin, &mut Vec::new()).await.unwrap();

    assert_eq!(header_values(&head, "x-forwarded-for"), ["127.0.0.1"]);
    assert_eq!(header_values(&head, "x-forwarded-proto"), ["http"]);
    assert_eq!(header_values(&head, "x-forwarded-host"), ["127.0.0.1"]);
    for spoofed in ["forwarded", "x-real-ip", "x-forwarded-port"] {
        assert!(
            header(&head, spoofed).is_none(),
            "{spoofed} crossed: {head}"
        );
    }
    assert!(!head.contains("203.0.113.9") && !head.contains("evil.test"));
    origin
        .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
        .await
        .unwrap();
    let (response, _) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&response), 204);
    proxy.stop().await;
}

/// The scheme the client used reaches the upstream from the listener that
/// accepted it: a TLS client is forwarded as https even when it claims http,
/// and the upstream hop itself remains cleartext.
#[tokio::test]
async fn an_https_client_is_forwarded_with_the_scheme_its_listener_established() {
    use crate::{
        server::{Listener, serve_listeners},
        tls::test_support::{Pki, connect, listener as tls_listener, source},
    };

    let pki = Pki::new("proxy-scheme");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let locations = Arc::new(
        LocationRouter::new(vec![("/", Handler::proxy(upstream))], Handler::not_found())
            .expect("valid locations"),
    );
    let router = Arc::new(
        HostRouter::new(vec![
            VirtualHost::new("localhost", locations).expect("host"),
        ])
        .expect("router"),
    );
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind TLS proxy");
    let address = socket.local_addr().unwrap();
    let tls = tls_listener(
        address,
        Duration::from_secs(5),
        vec![source(&leaf, &["localhost"], false)],
    );
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_listeners(
        vec![Listener::tls(socket, tls)],
        router,
        limits(),
        None,
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut stream = connect(&pki.client(&[b"http/1.1"]), address, "localhost")
        .await
        .expect("TLS handshake");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: http\r\nForwarded: proto=http\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let head = read_head(&mut origin, &mut Vec::new()).await.unwrap();
    assert_eq!(header_values(&head, "x-forwarded-proto"), ["https"]);
    assert!(header(&head, "forwarded").is_none(), "{head}");
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(GUARD, stream.read_to_end(&mut response))
        .await
        .expect("response within the guard")
        .expect("clean TLS close");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"\r\n\r\nok"));

    let _ = shutdown.send(());
    timeout(GUARD, server)
        .await
        .expect("stops")
        .expect("task")
        .expect("server");
}

#[tokio::test]
async fn absolute_form_becomes_origin_form_under_the_routed_authority() {
    for preserve_host in [true, false] {
        let listener = upstream_listener(None).await;
        let upstream_address = listener.local_addr().unwrap();
        let upstream = upstream_with(upstream_address, |config| {
            config.preserve_host = preserve_host;
        });
        let proxy = Proxy::start(&upstream).await;
        let authority = format!("127.0.0.1:{}", proxy.address.port());

        let mut stream = client(proxy.address).await;
        let request = format!(
            "GET http://{authority}/p/q?x=1 HTTP/1.1\r\nHost: ignored.invalid\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut origin = accept(&listener).await;
        let head = read_head(&mut origin, &mut Vec::new()).await.unwrap();

        assert!(
            head.starts_with("GET /p/q?x=1 HTTP/1.1\r\n"),
            "not origin-form: {head}"
        );
        let expected_host = if preserve_host {
            authority.clone()
        } else {
            upstream_address.to_string()
        };
        assert_eq!(header(&head, "host"), Some(expected_host));
        assert_eq!(header(&head, "x-forwarded-host"), Some(authority));
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let (response, _) = read_response(&mut stream, &mut Vec::new(), false).await;
        assert_eq!(status(&response), 200);
        proxy.stop().await;
    }
}

#[tokio::test]
async fn a_get_with_a_chunked_body_keeps_its_body() {
    // Hyper's client assumes a GET of unknown length has no body and would
    // silently send none unless chunked framing is explicit.
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET /q HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let (head, body) = read_request(&mut origin, &mut Vec::new()).await.unwrap();
    assert_eq!(
        header(&head, "transfer-encoding").as_deref(),
        Some("chunked")
    );
    assert_eq!(body, b"abc");
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    read_response(&mut stream, &mut Vec::new(), false).await;
    proxy.stop().await;
}

#[tokio::test]
async fn content_length_and_transfer_encoding_leave_only_chunked_framing() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let (head, body) = read_request(&mut origin, &mut Vec::new()).await.unwrap();
    assert!(header(&head, "content-length").is_none(), "{head}");
    assert_eq!(
        header(&head, "transfer-encoding").as_deref(),
        Some("chunked")
    );
    assert_eq!(body, b"hello");
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    read_response(&mut stream, &mut Vec::new(), false).await;
    proxy.stop().await;
}

#[tokio::test]
async fn unsupported_request_transfer_coding_is_rejected_before_upstream_connect() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: gzip, chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n")
        .await
        .unwrap();
    let (head, _) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(
        status(&head),
        400,
        "unsupported transfer coding must fail closed: {head}"
    );
    assert!(no_connection_within(&listener, Duration::from_millis(300)).await);
    assert_eq!(upstream.snapshot().connections_opened, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn unsupported_response_transfer_coding_is_a_bad_gateway() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    origin
        .write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        )
        .await
        .unwrap();
    let (head, _) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(
        status(&head),
        502,
        "unsupported transfer coding must fail closed: {head}"
    );
    assert_eq!(upstream.snapshot().idle_connections, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn unsupported_request_targets_do_not_reach_the_upstream() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    for request in [
        &b"OPTIONS * HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"[..],
        &b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"[..],
    ] {
        let mut stream = client(proxy.address).await;
        stream.write_all(request).await.unwrap();
        let (head, _) = read_response(&mut stream, &mut Vec::new(), false).await;
        assert_eq!(
            status(&head),
            400,
            "unsupported target must fail closed: {head}"
        );
    }
    assert!(no_connection_within(&listener, Duration::from_millis(300)).await);
    assert_eq!(upstream.snapshot().connections_opened, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn a_response_framed_by_both_lengths_is_relayed_but_its_connection_is_closed() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    let mut buffer = Vec::new();

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut first = accept(&listener).await;
    read_head(&mut first, &mut Vec::new()).await.unwrap();
    first
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream, &mut buffer, false).await;
    assert_eq!(status(&head), 200, "{head}");
    assert_eq!(
        body, b"hello",
        "Transfer-Encoding wins, as RFC 9112 requires"
    );
    // A smuggling signal: the exchange is relayed, the connection never reused.
    assert!(
        read_head(&mut first, &mut Vec::new()).await.is_none(),
        "the ambiguous connection must be closed, not parked"
    );

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut second = accept(&listener).await;
    read_head(&mut second, &mut Vec::new()).await.unwrap();
    second
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
        .await
        .unwrap();
    let (_, body) = read_response(&mut stream, &mut buffer, false).await;
    assert_eq!(body, b"ok");
    let snapshot = upstream.snapshot();
    assert_eq!(snapshot.connections_opened, 2);
    assert_eq!(snapshot.connections_reused, 0);
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn conflicting_content_lengths_never_reach_the_upstream() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!")
        .await
        .unwrap();
    let mut response = Vec::new();
    let _ = timeout(GUARD, stream.read_to_end(&mut response)).await;
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(no_connection_within(&listener, Duration::from_millis(300)).await);
    assert_eq!(upstream.snapshot().connections_opened, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn head_keeps_the_upstream_length_and_sends_no_body() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    let mut buffer = Vec::new();

    stream
        .write_all(b"HEAD /r HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let mut origin_buffer = Vec::new();
    read_head(&mut origin, &mut origin_buffer).await.unwrap();
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nContent-Type: text/plain\r\n\r\n")
        .await
        .unwrap();
    let (head, _) = read_response(&mut stream, &mut buffer, true).await;
    assert_eq!(status(&head), 200);
    assert_eq!(header(&head, "content-length").as_deref(), Some("1234"));

    // The same downstream connection must still be at a message boundary.
    stream
        .write_all(b"GET /r HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    read_head(&mut origin, &mut origin_buffer).await.unwrap();
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream, &mut buffer, false).await;
    assert_eq!(status(&head), 200);
    assert_eq!(body, b"body");
    assert_eq!(upstream.snapshot().connections_opened, 1);
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn a_head_response_with_conflicting_lengths_is_a_bad_gateway() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"HEAD / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nContent-Length: 20\r\n\r\n")
        .await
        .unwrap();
    let (head, _) = read_response(&mut stream, &mut Vec::new(), true).await;
    assert_eq!(status(&head), 502);
    proxy.stop().await;
}

#[tokio::test]
async fn no_content_and_not_modified_carry_no_body_and_keep_the_connection() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    let mut buffer = Vec::new();
    let mut origin: Option<TcpStream> = None;
    let mut origin_buffer = Vec::new();

    for (reply, expected) in [
        (
            &b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nX-A: 1\r\n\r\n"[..],
            204,
        ),
        (
            &b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nContent-Length: 50\r\n\r\n"[..],
            304,
        ),
        (&b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"[..], 200),
    ] {
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        if origin.is_none() {
            origin = Some(accept(&listener).await);
        }
        let upstream_stream = origin.as_mut().unwrap();
        read_head(upstream_stream, &mut origin_buffer)
            .await
            .unwrap();
        upstream_stream.write_all(reply).await.unwrap();
        let (head, body) = read_response(&mut stream, &mut buffer, false).await;
        assert_eq!(status(&head), expected, "{head}");
        if expected == 304 {
            assert_eq!(header(&head, "etag").as_deref(), Some("\"v1\""));
        }
        if expected != 200 {
            assert!(header(&head, "content-length").is_none(), "{head}");
            assert!(body.is_empty());
        } else {
            assert_eq!(body, b"ok");
        }
    }
    assert_eq!(upstream.snapshot().connections_opened, 1);
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn expect_continue_is_answered_by_hamster_and_not_forwarded() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST /e HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let origin_task = tokio::spawn(async move {
        let mut origin = accept(&listener).await;
        let mut buffer = Vec::new();
        let (head, body) = read_request(&mut origin, &mut buffer).await.unwrap();
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        (head, body)
    });
    let mut buffer = Vec::new();
    let interim = read_head(&mut stream, &mut buffer).await.unwrap();
    assert!(interim.starts_with("HTTP/1.1 100 Continue"), "{interim}");
    stream.write_all(b"hello").await.unwrap();
    let (head, body) = read_response(&mut stream, &mut buffer, false).await;
    assert_eq!(status(&head), 200);
    assert_eq!(body, b"ok");
    let (origin_head, origin_body) = origin_task.await.unwrap();
    assert!(header(&origin_head, "expect").is_none(), "{origin_head}");
    assert_eq!(origin_body, b"hello");
    proxy.stop().await;
}

#[tokio::test]
async fn an_unrequested_switching_protocols_is_a_bad_gateway() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let head = read_head(&mut origin, &mut Vec::new()).await.unwrap();
    assert!(header(&head, "upgrade").is_none(), "{head}");
    origin
        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await
        .unwrap();
    let (response, _) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&response), 502);
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.idle_connections, 0, "a 101 connection was pooled");
    drop(stream);
    proxy.stop().await;
}

#[tokio::test]
async fn a_response_head_larger_than_the_buffer_bound_is_a_bad_gateway() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.max_buf_size = 16 * 1_024;
    });
    let proxy = Proxy::start(&upstream).await;
    let request = tokio::spawn(get(proxy.address, "127.0.0.1", "/"));
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    let oversized = format!(
        "HTTP/1.1 200 OK
X-Large: {}
Content-Length: 0

",
        "a".repeat(32 * 1_024)
    );
    let _ = origin.write_all(oversized.as_bytes()).await;
    let (head, body) = request.await.unwrap();
    assert_eq!(status(&head), 502);
    assert_eq!(body, b"Invalid Upstream Response");
    proxy.stop().await;
}

// ----- Streaming and backpressure -----------------------------------------------

/// The most bytes that kernel and Hyper buffers can plausibly hold between a
/// writer and a reader on loopback. Anything that buffered a whole payload
/// would exceed it by several times.
#[cfg(windows)]
const BUFFER_SLACK: u64 = 8 * 1_024 * 1_024;
/// Linux autotunes loopback receive buffers up to `net.ipv4.tcp_rmem`'s
/// maximum, which some kernels set to 32 MiB. With that ceiling the slow-client
/// test measured up to 9.3 MiB in flight while the upstream still stopped far
/// short of `LARGE_PAYLOAD`; with a 6 MiB ceiling it peaked at 7.7 MiB. Whole
/// payload buffering still exceeds this bound eight times over.
#[cfg(not(windows))]
const BUFFER_SLACK: u64 = 16 * 1_024 * 1_024;
const LARGE_PAYLOAD: u64 = 128 * 1_024 * 1_024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_upstream_backpressures_the_uploading_client() {
    let listener = upstream_listener(Some(64 * 1_024)).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let written = Arc::new(AtomicU64::new(0));
    let mut stream = small_client(proxy.address, 64 * 1_024, 64 * 1_024).await;
    let writer_written = Arc::clone(&written);
    let writer = tokio::spawn(async move {
        let head = format!(
            "POST /up HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {LARGE_PAYLOAD}\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        let chunk = vec![b'u'; 64 * 1_024];
        while writer_written.load(Ordering::Relaxed) < LARGE_PAYLOAD {
            if stream.write_all(&chunk).await.is_err() {
                return;
            }
            writer_written.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    });

    let mut origin = accept(&listener).await;
    let mut buffer = Vec::new();
    read_head(&mut origin, &mut buffer).await.unwrap();
    let mut consumed = buffer.len() as u64;
    let started = Instant::now();
    let mut chunk = vec![0_u8; 16 * 1_024];
    while started.elapsed() < Duration::from_millis(1_500) {
        consumed += origin.read(&mut chunk).await.unwrap() as u64;
        sleep(Duration::from_millis(20)).await;
    }
    let client_written = written.load(Ordering::Relaxed);
    println!("slow upstream: client wrote {client_written}, upstream consumed {consumed}");
    assert!(
        client_written < LARGE_PAYLOAD,
        "the whole upload was accepted while the upstream read {consumed} bytes"
    );
    assert!(
        client_written.saturating_sub(consumed) < BUFFER_SLACK,
        "{client_written} written against {consumed} consumed"
    );
    writer.abort();
    drop(origin);
    quiesced(&upstream).await;
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_client_backpressures_the_upstream_response() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = small_client(proxy.address, 64 * 1_024, 64 * 1_024).await;
    stream
        .write_all(b"GET /down HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let written = Arc::new(AtomicU64::new(0));
    let origin_written = Arc::clone(&written);
    let mut origin = accept(&listener).await;
    let origin_task = tokio::spawn(async move {
        read_head(&mut origin, &mut Vec::new()).await.unwrap();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {LARGE_PAYLOAD}\r\n\r\n");
        origin.write_all(head.as_bytes()).await.unwrap();
        let chunk = vec![b'd'; 64 * 1_024];
        while origin_written.load(Ordering::Relaxed) < LARGE_PAYLOAD {
            if origin.write_all(&chunk).await.is_err() {
                return;
            }
            origin_written.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    });

    let mut consumed = 0_u64;
    let started = Instant::now();
    let mut chunk = vec![0_u8; 16 * 1_024];
    while started.elapsed() < Duration::from_millis(1_500) {
        consumed += stream.read(&mut chunk).await.unwrap() as u64;
        sleep(Duration::from_millis(20)).await;
    }
    let upstream_written = written.load(Ordering::Relaxed);
    println!("slow client: upstream wrote {upstream_written}, client consumed {consumed}");
    assert!(upstream_written < LARGE_PAYLOAD);
    assert!(
        upstream_written.saturating_sub(consumed) < BUFFER_SLACK,
        "{upstream_written} written against {consumed} consumed"
    );
    drop(stream);
    timeout(GUARD, origin_task).await.unwrap().unwrap();
    quiesced(&upstream).await;
    proxy.stop().await;
}

#[tokio::test]
async fn a_client_that_aborts_an_upload_never_gives_the_upstream_a_complete_body() {
    for chunked in [false, true] {
        let listener = upstream_listener(None).await;
        let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
        let proxy = Proxy::start(&upstream).await;

        let mut stream = client(proxy.address).await;
        let framing = if chunked {
            "Transfer-Encoding: chunked".to_owned()
        } else {
            "Content-Length: 8388608".to_owned()
        };
        let head = format!("POST /abort HTTP/1.1\r\nHost: 127.0.0.1\r\n{framing}\r\n\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
        let payload = vec![b'a'; 1_024 * 1_024];
        if chunked {
            stream
                .write_all(format!("{:x}\r\n", payload.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(&payload).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
        } else {
            stream.write_all(&payload).await.unwrap();
        }

        let mut origin = accept(&listener).await;
        let mut buffer = Vec::new();
        read_head(&mut origin, &mut buffer).await.unwrap();
        let mut received = std::mem::take(&mut buffer);
        // Wait until the upstream has most of what was sent, then abort.
        while received.len() < 512 * 1_024 {
            let mut chunk = vec![0_u8; 64 * 1_024];
            let count = origin.read(&mut chunk).await.unwrap();
            received.extend_from_slice(&chunk[..count]);
        }
        drop(stream);

        let ended = timeout(GUARD, async {
            loop {
                let mut chunk = vec![0_u8; 64 * 1_024];
                match origin.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(count) => received.extend_from_slice(&chunk[..count]),
                }
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "the upstream kept waiting for the aborted body"
        );
        if chunked {
            assert!(
                !received.windows(5).any(|window| window == b"0\r\n\r\n"),
                "an aborted chunked upload was terminated cleanly"
            );
        } else {
            assert!(received.len() < 8 * 1_024 * 1_024);
        }
        quiesced(&upstream).await;
        proxy.stop().await;
    }
}

#[tokio::test]
async fn a_client_that_aborts_a_download_aborts_the_upstream_and_its_connection() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    let origin_task = tokio::spawn(async move {
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {LARGE_PAYLOAD}\r\n\r\n");
        origin.write_all(head.as_bytes()).await.unwrap();
        let chunk = vec![b'd'; 64 * 1_024];
        let mut written = 0_u64;
        while written < LARGE_PAYLOAD {
            if origin.write_all(&chunk).await.is_err() {
                return written;
            }
            written += chunk.len() as u64;
        }
        written
    });
    let mut chunk = vec![0_u8; 64 * 1_024];
    let mut read = 0;
    while read < 256 * 1_024 {
        read += stream.read(&mut chunk).await.unwrap();
    }
    drop(stream);

    let written = timeout(GUARD, origin_task)
        .await
        .expect("the upstream write must fail once the client is gone")
        .unwrap();
    assert!(written < LARGE_PAYLOAD);
    let snapshot = quiesced(&upstream).await;
    assert_eq!(
        snapshot.idle_connections, 0,
        "an aborted exchange was pooled"
    );

    // The next exchange must open a new upstream connection.
    let (responder, responder_task) = spawn_responder(listener);
    let (head, body) = get(proxy.address, "127.0.0.1", "/next").await;
    assert_eq!(status(&head), 200);
    assert_eq!(body, b"ok");
    assert_eq!(responder.connections.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.snapshot().connections_reused, 0);
    responder_task.abort();
    proxy.stop().await;
}

#[tokio::test]
async fn early_response_reaches_the_client_while_it_is_still_uploading() {
    let listener = upstream_listener(Some(64 * 1_024)).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;

    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 67108864\r\n\r\n")
        .await
        .unwrap();
    stream.write_all(&vec![b'x'; 256 * 1_024]).await.unwrap();

    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    origin
        .write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 3\r\n\r\nbig")
        .await
        .unwrap();

    // The client has sent 256 KiB of 64 MiB and sends nothing more.
    let started = Instant::now();
    let (head, body) = timeout(
        Duration::from_secs(3),
        read_response(&mut stream, &mut Vec::new(), false),
    )
    .await
    .expect("the early response must not wait for the upload");
    assert_eq!(status(&head), 413);
    assert_eq!(body, b"big");
    println!("early response after {:?}", started.elapsed());

    // An exchange whose request never finished is never reused.
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.idle_connections, 0);
    drop(origin);
    drop(stream);
    proxy.stop().await;
}

// ----- Bounded no-progress states --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upstream_that_stops_reading_while_the_client_disappears_is_reclaimed() {
    // The upstream accepts the head and never reads the body. Hyper then stops
    // polling the downstream body, so the downstream socket is not read and a
    // client disconnect is invisible. Only the upstream write-progress
    // deadline can end this exchange.
    let progress_timeout = Duration::from_millis(1_000);
    let listener = upstream_listener(Some(64 * 1_024)).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.progress_timeout = progress_timeout;
    });
    let proxy = Proxy::start(&upstream).await;

    let mut stream = small_client(proxy.address, 64 * 1_024, 64 * 1_024).await;
    stream
        .write_all(b"POST /stall HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1073741824\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();

    // Upload until every buffer between client and upstream is full, then
    // disconnect well inside the progress interval, so the disconnect happens
    // while the exchange is still waiting on the upstream.
    let chunk = vec![b's'; 64 * 1_024];
    let mut sent = 0_u64;
    while let Ok(result) = timeout(Duration::from_millis(200), stream.write_all(&chunk)).await {
        result.expect("the exchange ended before the client disconnected");
        sent += chunk.len() as u64;
        assert!(sent < 1_024 * 1_024 * 1_024, "the upload never stalled");
    }
    println!("upload stalled after {sent} bytes; client disconnects");
    assert_eq!(upstream.snapshot().active_exchanges, 1);
    drop(stream);

    let reclaimed = eventually("reclaiming the stalled exchange", || {
        upstream.snapshot().active_exchanges == 0
    })
    .await;
    println!("reclaimed {reclaimed:?} after the client disconnected");
    assert!(reclaimed < progress_timeout + Duration::from_secs(2));
    assert!(upstream.snapshot().upstream_timeouts >= 1);
    // The upstream socket is closed, not left open behind the exchange.
    let mut drained = vec![0_u8; 1_024 * 1_024];
    let closed = timeout(GUARD, async {
        loop {
            match origin.read(&mut drained).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the upstream connection outlived its exchange"
    );
    proxy.stop().await;
}

#[tokio::test]
async fn a_stalled_client_upload_is_bounded_by_the_progress_timeout() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.progress_timeout = Duration::from_millis(300);
    });
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(
            b"POST /slow HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1000\r\n\r\n0123456789",
        )
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    let mut buffer = Vec::new();
    read_head(&mut origin, &mut buffer).await.unwrap();

    let started = Instant::now();
    let (head, _) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&head), 408);
    assert!(started.elapsed() < Duration::from_secs(3));
    // The upstream sees the body end without its declared length.
    let mut rest = buffer;
    let _ = timeout(GUARD, origin.read_to_end(&mut rest)).await;
    assert!(rest.len() < 1_000);
    quiesced(&upstream).await;
    proxy.stop().await;
}

#[tokio::test]
async fn a_progressing_upload_is_not_cut_by_the_response_header_timeout() {
    // The head deadline starts when the request has been sent. A long upload
    // that keeps making progress is bounded by the progress deadline instead.
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.response_header_timeout = Duration::from_millis(300);
        config.progress_timeout = Duration::from_millis(600);
    });
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST /trickle HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let origin_task = tokio::spawn(async move {
        let mut origin = accept(&listener).await;
        let (_, body) = read_request(&mut origin, &mut Vec::new()).await.unwrap();
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        body
    });
    for byte in b"0123456789" {
        sleep(Duration::from_millis(150)).await;
        stream.write_all(&[*byte]).await.unwrap();
    }
    let (head, body) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&head), 200, "{head}");
    assert_eq!(body, b"ok");
    assert_eq!(origin_task.await.unwrap(), b"0123456789");
    proxy.stop().await;
}

#[tokio::test]
async fn a_stalled_upstream_response_body_aborts_the_downstream() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.progress_timeout = Duration::from_millis(300);
    });
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789")
        .await
        .unwrap();

    let started = Instant::now();
    let mut buffer = Vec::new();
    let head = read_head(&mut stream, &mut buffer).await.unwrap();
    assert_eq!(status(&head), 200);
    let _ = timeout(GUARD, stream.read_to_end(&mut buffer)).await;
    assert_eq!(
        buffer, b"0123456789",
        "a stalled body must end short, not complete"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.upstream_timeouts, 1);
    assert_eq!(snapshot.idle_connections, 0);
    drop(origin);
    proxy.stop().await;
}

#[tokio::test]
async fn slow_but_progressing_response_bodies_are_allowed() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.progress_timeout = Duration::from_millis(400);
    });
    let proxy = Proxy::start(&upstream).await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    let origin_task = tokio::spawn(async move {
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
            .await
            .unwrap();
        // 1.5 s in total, far past the 400 ms deadline, but never 400 ms idle.
        for byte in b"0123456789" {
            sleep(Duration::from_millis(150)).await;
            origin.write_all(&[*byte]).await.unwrap();
        }
    });
    let (head, body) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&head), 200);
    assert_eq!(body, b"0123456789");
    origin_task.await.unwrap();
    proxy.stop().await;
}

#[tokio::test]
async fn an_upstream_that_crashes_mid_response_never_yields_a_clean_short_response() {
    for chunked in [false, true] {
        let listener = upstream_listener(None).await;
        let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
        let proxy = Proxy::start(&upstream).await;
        let mut stream = client(proxy.address).await;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut origin = accept(&listener).await;
        read_head(&mut origin, &mut Vec::new()).await.unwrap();
        let partial: &[u8] = if chunked {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\n0123456789\r\n"
        } else {
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n0123456789"
        };
        origin.write_all(partial).await.unwrap();
        drop(origin);

        let mut buffer = Vec::new();
        let head = read_head(&mut stream, &mut buffer).await.unwrap();
        assert_eq!(status(&head), 200, "committed before the crash");
        let _ = timeout(GUARD, stream.read_to_end(&mut buffer)).await;
        if chunked {
            assert!(
                !buffer.windows(5).any(|window| window == b"0\r\n\r\n"),
                "a truncated chunked response was terminated cleanly: {:?}",
                String::from_utf8_lossy(&buffer)
            );
        } else {
            assert_eq!(header(&head, "content-length").as_deref(), Some("1000"));
            assert!(buffer.len() < 1_000);
        }
        let snapshot = quiesced(&upstream).await;
        assert_eq!(snapshot.upstream_failures, 1);
        assert_eq!(snapshot.idle_connections, 0);
        proxy.stop().await;
    }
}

// ----- Failure mapping --------------------------------------------------------

#[tokio::test]
async fn a_refused_connection_is_a_bad_gateway() {
    let address = crate::test_ports::unused_outside_dynamic_range();
    // Windows reports a refused loopback connect only after retrying the SYN;
    // the connect budget must outlast that for the refusal, rather than the
    // timeout, to be what is observed.
    let upstream = upstream_with(address, |config| {
        config.connect_timeout = Duration::from_secs(8);
    });
    let proxy = Proxy::start(&upstream).await;
    let started = Instant::now();
    let (head, body) = get(proxy.address, "127.0.0.1", "/").await;
    println!("refused connect reported after {:?}", started.elapsed());
    assert_eq!(status(&head), 502);
    assert_eq!(body, b"Bad Gateway");
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.connect_failures, 1);
    assert_eq!(snapshot.connections_opened, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn a_connect_timeout_is_a_gateway_timeout() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.connect_timeout = Duration::from_millis(200);
    });
    upstream.faults.delay_connect(Duration::from_secs(5));
    let proxy = Proxy::start(&upstream).await;
    let started = Instant::now();
    let (head, body) = get(proxy.address, "127.0.0.1", "/").await;
    assert_eq!(status(&head), 504);
    assert_eq!(body, b"Upstream Connect Timeout");
    assert!(started.elapsed() < Duration::from_secs(2));
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.upstream_timeouts, 1);
    proxy.stop().await;
}

#[tokio::test]
async fn a_missing_response_head_is_a_gateway_timeout_and_the_connection_is_discarded() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.response_header_timeout = Duration::from_millis(300);
    });
    let proxy = Proxy::start(&upstream).await;
    let started = Instant::now();
    let request = tokio::spawn(get(proxy.address, "127.0.0.1", "/silent"));
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    let (head, body) = request.await.unwrap();
    assert_eq!(status(&head), 504);
    assert_eq!(body, b"Upstream Response Timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.idle_connections, 0);
    // The silent connection is closed rather than pooled.
    let mut rest = Vec::new();
    assert!(
        timeout(GUARD, origin.read_to_end(&mut rest)).await.is_ok(),
        "the timed-out upstream connection stayed open"
    );
    proxy.stop().await;
}

#[tokio::test]
async fn a_request_the_upstream_received_is_never_sent_again() {
    for request in [
        &b"GET /once HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"[..],
        &b"POST /once HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload"[..],
    ] {
        let listener = upstream_listener(None).await;
        let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
        let proxy = Proxy::start(&upstream).await;
        let mut stream = client(proxy.address).await;
        stream.write_all(request).await.unwrap();

        // The upstream reads the whole request and dies without answering.
        let mut origin = accept(&listener).await;
        read_request(&mut origin, &mut Vec::new()).await.unwrap();
        drop(origin);

        let (head, _) = read_response(&mut stream, &mut Vec::new(), false).await;
        assert_eq!(status(&head), 502);
        assert!(
            no_connection_within(&listener, Duration::from_millis(500)).await,
            "a possibly-started request was replayed"
        );
        let snapshot = quiesced(&upstream).await;
        assert_eq!(snapshot.safe_retries, 0);
        assert_eq!(snapshot.connections_opened, 1);
        proxy.stop().await;
    }
}

#[tokio::test]
async fn an_unsent_request_on_a_stale_connection_is_retried_exactly_once() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let (responder, responder_task) = spawn_responder(listener);

    let (head, _) = get(proxy.address, "127.0.0.1", "/first").await;
    assert_eq!(status(&head), 200);
    eventually("the first connection to become idle", || {
        upstream.snapshot().idle_connections == 1
    })
    .await;
    // Let the idle connection's driver park before the fault is armed.
    sleep(Duration::from_millis(100)).await;
    upstream.faults.arm_read_eofs(1);

    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"POST /second HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream, &mut Vec::new(), false).await;
    assert_eq!(status(&head), 200, "{head}");
    assert_eq!(body, b"ok");

    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.safe_retries, 1);
    assert_eq!(snapshot.connections_opened, 2);
    assert_eq!(responder.connections.load(Ordering::SeqCst), 2);
    assert_eq!(responder.requests.load(Ordering::SeqCst), 2);
    let bodies = responder.bodies.lock().unwrap().clone();
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.as_slice() == b"payload")
            .count(),
        1,
        "the retried request executed more than once"
    );
    responder_task.abort();
    proxy.stop().await;
}

#[tokio::test]
async fn a_second_unsent_failure_is_not_retried_again() {
    // Both the stale connection and the fresh one fail before sending. The
    // retry budget is one, so the second proof of non-delivery ends in 502.
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let (responder, responder_task) = spawn_responder(listener);
    get(proxy.address, "127.0.0.1", "/first").await;
    eventually("idle", || upstream.snapshot().idle_connections == 1).await;
    sleep(Duration::from_millis(100)).await;

    upstream.faults.arm_read_eofs(2);
    let (head, _) = get(proxy.address, "127.0.0.1", "/second").await;
    assert_eq!(status(&head), 502, "{head}");
    let snapshot = quiesced(&upstream).await;
    assert_eq!(snapshot.safe_retries, 1);
    assert_eq!(snapshot.connections_opened, 2);
    assert_eq!(responder.requests.load(Ordering::SeqCst), 1);
    responder_task.abort();
    proxy.stop().await;
}

// ----- Connection reuse -------------------------------------------------------

#[tokio::test]
async fn sequential_exchanges_reuse_one_upstream_connection() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start(&upstream).await;
    let (responder, responder_task) = spawn_responder(listener);
    for index in 0..20 {
        let (head, body) = get(proxy.address, "127.0.0.1", &format!("/{index}")).await;
        assert_eq!(status(&head), 200);
        assert_eq!(body, b"ok");
    }
    let snapshot = quiesced(&upstream).await;
    assert_eq!(responder.connections.load(Ordering::SeqCst), 1);
    assert_eq!(responder.requests.load(Ordering::SeqCst), 20);
    assert_eq!(snapshot.connections_opened, 1);
    assert_eq!(snapshot.connections_reused, 19);
    responder_task.abort();
    proxy.stop().await;
}

#[tokio::test]
async fn an_idle_connection_past_its_idle_timeout_is_not_reused() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.idle_timeout = Duration::from_millis(200);
    });
    let proxy = Proxy::start(&upstream).await;
    let (responder, responder_task) = spawn_responder(listener);
    get(proxy.address, "127.0.0.1", "/a").await;
    sleep(Duration::from_millis(400)).await;
    get(proxy.address, "127.0.0.1", "/b").await;
    assert_eq!(responder.connections.load(Ordering::SeqCst), 2);
    assert_eq!(upstream.snapshot().connections_reused, 0);
    responder_task.abort();
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_idle_bounds_the_connections_kept_open() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |config| {
        config.max_idle = 2;
    });
    let proxy = Proxy::start(&upstream).await;
    let (_responder, responder_task) = spawn_responder(listener);
    let requests: Vec<_> = (0..8)
        .map(|index| {
            let address = proxy.address;
            tokio::spawn(async move { get(address, "127.0.0.1", &format!("/{index}")).await })
        })
        .collect();
    for request in requests {
        assert_eq!(status(&request.await.unwrap().0), 200);
    }
    let snapshot = quiesced(&upstream).await;
    assert!(snapshot.idle_connections <= 2, "{snapshot:?}");
    responder_task.abort();
    proxy.stop().await;
}

// ----- Lifecycle and isolation --------------------------------------------------

#[tokio::test]
async fn graceful_shutdown_bounds_an_active_proxied_stream() {
    let listener = upstream_listener(None).await;
    let upstream = upstream_with(listener.local_addr().unwrap(), |_| {});
    let drain = Duration::from_millis(500);
    let proxy = Proxy::start_hosts(
        vec![("127.0.0.1", Arc::clone(&upstream))],
        HttpLimits::for_test(64, drain, Duration::from_secs(5), 64 * 1_024),
    )
    .await;
    let mut stream = client(proxy.address).await;
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut origin = accept(&listener).await;
    read_head(&mut origin, &mut Vec::new()).await.unwrap();
    let origin_task = tokio::spawn(async move {
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        // An endless stream that keeps making progress.
        loop {
            if origin.write_all(b"5\r\ntick\n\r\n").await.is_err() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    });
    read_head(&mut stream, &mut Vec::new()).await.unwrap();
    let client_task = tokio::spawn(async move {
        let mut chunk = [0_u8; 1_024];
        while let Ok(Ok(count)) = timeout(GUARD, stream.read(&mut chunk)).await {
            if count == 0 {
                break;
            }
        }
    });
    sleep(Duration::from_millis(200)).await;

    let stopped_in = proxy.stop().await;
    println!("shutdown with an active stream took {stopped_in:?}");
    assert!(stopped_in < drain + Duration::from_secs(2));
    timeout(GUARD, origin_task)
        .await
        .expect("the upstream stream must be closed by shutdown")
        .unwrap();
    timeout(GUARD, client_task).await.unwrap().unwrap();
    assert_eq!(upstream.snapshot().active_exchanges, 0);
}

#[tokio::test]
async fn upstreams_of_different_hosts_are_independent() {
    let dead_address = crate::test_ports::unused_outside_dynamic_range();
    let listener = upstream_listener(None).await;
    let broken = upstream_with(dead_address, |config| {
        config.connect_timeout = Duration::from_secs(8);
    });
    let healthy = upstream_with(listener.local_addr().unwrap(), |_| {});
    let proxy = Proxy::start_hosts(
        vec![
            ("a.test", Arc::clone(&broken)),
            ("b.test", Arc::clone(&healthy)),
        ],
        limits(),
    )
    .await;
    let (responder, responder_task) = spawn_responder(listener);
    for _ in 0..3 {
        assert_eq!(status(&get(proxy.address, "a.test", "/").await.0), 502);
        assert_eq!(status(&get(proxy.address, "b.test", "/").await.0), 200);
    }
    let broken = quiesced(&broken).await;
    let healthy = quiesced(&healthy).await;
    assert_eq!(broken.connect_failures, 3);
    assert_eq!(broken.connections_opened, 0);
    assert_eq!(healthy.connect_failures, 0);
    assert_eq!(healthy.connections_opened, 1);
    assert_eq!(healthy.connections_reused, 2);
    assert_eq!(responder.requests.load(Ordering::SeqCst), 3);
    responder_task.abort();
    proxy.stop().await;
}

// ----- Pure helpers -----------------------------------------------------------

#[test]
fn hop_by_hop_removal_follows_every_connection_field_and_token() {
    use hyper::HeaderMap;
    let mut headers = HeaderMap::new();
    headers.append("connection", HeaderValue::from_static("close, X-A"));
    headers.append("connection", HeaderValue::from_static(" x-b ,"));
    headers.append("x-a", HeaderValue::from_static("1"));
    headers.append("x-b", HeaderValue::from_static("2"));
    headers.append("x-c", HeaderValue::from_static("3"));
    headers.append("trailer", HeaderValue::from_static("x-sum"));
    super::remove_hop_by_hop(&mut headers);
    assert!(headers.get("x-a").is_none());
    assert!(headers.get("x-b").is_none());
    assert!(headers.get("connection").is_none());
    assert_eq!(headers.get("x-c").unwrap(), "3");
    assert_eq!(headers.get("trailer").unwrap(), "x-sum");
}

#[test]
fn only_a_trailers_coding_is_accepted_from_te() {
    use hyper::HeaderMap;
    let cases = [
        ("trailers", true),
        ("gzip, Trailers", true),
        ("trailers;q=0.5", true),
        ("gzip", false),
        ("trailersx", false),
    ];
    for (value, expected) in cases {
        let mut headers = HeaderMap::new();
        headers.insert("te", HeaderValue::from_static(value));
        assert_eq!(super::te_accepts_trailers(&headers), expected, "{value}");
    }
}
