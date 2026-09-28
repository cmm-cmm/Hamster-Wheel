//! Boundaries the TLS listener adds, exercised over real sockets: the total
//! handshake deadline and the admission slot it holds, shutdown during a
//! handshake, SNI certificate selection versus Host routing, ALPN, the
//! socket write deadline beneath TLS, graceful drain, and close_notify.

use std::{
    io::Cursor,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use hyper::StatusCode;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use tokio_rustls::rustls::{ClientConnection, pki_types::ServerName};

use super::{Listener, serve_listeners};
use crate::{
    config::HttpLimits,
    router::{Handler, HostRouter, LocationRouter, VirtualHost},
    tls::{
        TlsListener,
        test_support::{Pki, connect, listener as tls_listener, recording_client, source},
    },
};

const GUARD: Duration = Duration::from_secs(10);
const HTTP_1_1: &[u8] = b"http/1.1";

struct Server {
    cleartext: Option<SocketAddr>,
    tls_address: SocketAddr,
    tls: Arc<TlsListener>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(
        router: HostRouter,
        limits: HttpLimits,
        with_cleartext: bool,
        build: impl FnOnce(SocketAddr) -> Arc<TlsListener>,
    ) -> Self {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind TLS listener");
        let tls_address = socket.local_addr().expect("TLS address");
        let tls = build(tls_address);
        let mut listeners = vec![Listener::tls(socket, Arc::clone(&tls))];
        let mut cleartext = None;
        if with_cleartext {
            let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind cleartext listener");
            cleartext = Some(socket.local_addr().expect("cleartext address"));
            listeners.insert(0, Listener::cleartext(socket));
        }
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_listeners(
            listeners,
            Arc::new(router),
            limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        Self {
            cleartext,
            tls_address,
            tls,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn stop(mut self) -> Duration {
        let started = Instant::now();
        let _ = self.shutdown.take().expect("running").send(());
        timeout(GUARD, &mut self.task)
            .await
            .expect("server stops within the guard")
            .expect("server task")
            .expect("server result");
        started.elapsed()
    }
}

fn limits(max_connections: usize) -> HttpLimits {
    HttpLimits::for_test(
        max_connections,
        Duration::from_secs(5),
        Duration::from_secs(5),
        64 * 1_024,
    )
}

fn router(hosts: Vec<(&str, Handler)>) -> HostRouter {
    HostRouter::new(
        hosts
            .into_iter()
            .map(|(name, handler)| {
                let locations =
                    Arc::new(LocationRouter::new(Vec::new(), handler).expect("valid locations"));
                VirtualHost::new(name, locations).expect("valid host")
            })
            .collect(),
    )
    .expect("valid router")
}

fn replace_certificate(
    target: &crate::tls::test_support::Leaf,
    replacement: &crate::tls::test_support::Leaf,
) {
    std::fs::copy(&replacement.cert, &target.cert).expect("replace certificate file");
    std::fs::copy(&replacement.key, &target.key).expect("replace key file");
}

async fn presented_certificate(
    address: SocketAddr,
    name: &str,
) -> tokio_rustls::rustls::pki_types::CertificateDer<'static> {
    let (connector, seen) = recording_client(true, &[]);
    let _stream = connect(&connector, address, name)
        .await
        .expect("new TLS handshake");
    seen.lock().unwrap().clone().expect("presented certificate")
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + GUARD;
    while !condition() {
        assert!(Instant::now() < deadline, "{what} never happened");
        sleep(Duration::from_millis(5)).await;
    }
}

/// Reads a message head, leaving any following bytes in `buffer`.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S, buffer: &mut Vec<u8>) -> String {
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8(buffer[..end + 4].to_vec()).expect("ASCII head");
            buffer.drain(..end + 4);
            return head;
        }
        let mut chunk = [0_u8; 4_096];
        let count = timeout(GUARD, stream.read(&mut chunk))
            .await
            .expect("head within the guard")
            .expect("read head");
        assert_ne!(count, 0, "connection closed before a response head");
        buffer.extend_from_slice(&chunk[..count]);
    }
}

async fn read_response<S: AsyncRead + Unpin>(stream: &mut S) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let head = read_head(stream, &mut buffer).await;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("numeric length"))
        })
        .expect("content-length");
    while buffer.len() < length {
        let mut chunk = [0_u8; 4_096];
        let count = timeout(GUARD, stream.read(&mut chunk))
            .await
            .expect("body within the guard")
            .expect("read body");
        assert_ne!(count, 0, "connection closed inside a body");
        buffer.extend_from_slice(&chunk[..count]);
    }
    assert_eq!(buffer.len(), length, "no bytes beyond the response");
    (head, buffer)
}

#[tokio::test]
async fn both_listeners_share_one_router_and_each_carries_its_own_trusted_scheme() {
    let pki = Pki::new("scheme");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![
            ("localhost", Handler::transport_echo()),
            ("127.0.0.1", Handler::transport_echo()),
        ]),
        limits(8),
        true,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    // A client that verifies the chain and name, as a browser would.
    let mut tls = connect(&pki.client(&[HTTP_1_1]), server.tls_address, "localhost")
        .await
        .expect("a verifying client completes the handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(HTTP_1_1));
    let peer = tls.get_ref().0.local_addr().expect("client address");
    let local = tls.get_ref().0.peer_addr().expect("server address");
    let expected = format!("peer={peer};local={local};scheme=https");

    // Keep-alive over TLS; a forged forwarding header changes nothing.
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: http\r\n\r\n")
        .await
        .expect("first request");
    let (head, body) = read_response(&mut tls).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_eq!(String::from_utf8(body).unwrap(), expected);
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("second request");
    assert_eq!(
        String::from_utf8(read_response(&mut tls).await.1).unwrap(),
        expected
    );
    // rustls reports a TCP close without close_notify as UnexpectedEof, so a
    // clean end here proves Hyper's shutdown reached the TLS layer.
    let mut rest = Vec::new();
    let end = timeout(GUARD, tls.read_to_end(&mut rest))
        .await
        .expect("connection closes after Connection: close");
    assert!(end.is_ok(), "TLS must end with close_notify, got {end:?}");
    assert!(rest.is_empty());

    // The cleartext listener of the same server says http.
    let mut clear = TcpStream::connect(server.cleartext.expect("cleartext listener"))
        .await
        .expect("connect cleartext");
    clear
        .write_all(
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("cleartext request");
    let body = String::from_utf8(read_response(&mut clear).await.1).unwrap();
    assert!(body.ends_with(";scheme=http"), "{body}");

    server.stop().await;
}

#[tokio::test]
async fn sni_selects_the_certificate_and_a_host_it_does_not_cover_is_misdirected() {
    let pki = Pki::new("sni");
    let exact = pki.leaf("exact", &["a.test"]);
    let wildcard = pki.leaf("wildcard", &["*.b.test"]);
    let fallback = pki.leaf("fallback", &["fallback.test"]);
    let server = Server::start(
        router(vec![
            ("a.test", Handler::simple(StatusCode::OK, "site a")),
            ("x.b.test", Handler::simple(StatusCode::OK, "site b")),
            ("fallback.test", Handler::simple(StatusCode::OK, "fallback")),
        ]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![
                    source(&exact, &["a.test"], false),
                    source(&wildcard, &["*.b.test"], false),
                    source(&fallback, &["fallback.test"], true),
                ],
            )
        },
    )
    .await;

    for (name, send_sni, expected) in [
        ("a.test", true, &exact.der),
        ("x.b.test", true, &wildcard.der),
        // A wildcard covers exactly one label.
        ("y.x.b.test", true, &fallback.der),
        ("unknown.test", true, &fallback.der),
        ("a.test", false, &fallback.der),
    ] {
        let (connector, seen) = recording_client(send_sni, &[]);
        let stream = connect(&connector, server.tls_address, name)
            .await
            .unwrap_or_else(|error| panic!("{name} (sni={send_sni}): {error}"));
        assert_eq!(
            seen.lock().unwrap().as_ref(),
            Some(expected),
            "{name} (sni={send_sni}) selected the wrong certificate"
        );
        drop(stream);
    }

    // A verifying client accepts the wildcard certificate for its name.
    connect(&pki.client(&[]), server.tls_address, "x.b.test")
        .await
        .expect("the wildcard chain verifies");

    // The connection presented the a.test certificate, so it is not
    // authoritative for x.b.test even though that host exists: 421, and the
    // connection stays usable for a host it does cover.
    let (connector, _) = recording_client(true, &[]);
    let mut stream = connect(&connector, server.tls_address, "a.test")
        .await
        .expect("handshake as a.test");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x.b.test\r\n\r\n")
        .await
        .expect("request for another host");
    let (head, body) = read_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 421 "), "{head}");
    assert_eq!(body, b"Misdirected Request");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("request for the covered host");
    assert_eq!(read_response(&mut stream).await.1, b"site a");

    assert_eq!(server.tls.handshakes_rejected(), 0);
    server.stop().await;
}

#[tokio::test]
async fn without_a_default_certificate_an_unknown_or_absent_sni_fails_the_handshake() {
    let pki = Pki::new("no-default");
    let leaf = pki.leaf("a", &["a.test"]);
    let server = Server::start(
        router(vec![("a.test", Handler::simple(StatusCode::OK, "site a"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["a.test"], false)],
            )
        },
    )
    .await;

    let (with_sni, _) = recording_client(true, &[]);
    let (without_sni, _) = recording_client(false, &[]);
    assert!(
        connect(&with_sni, server.tls_address, "unknown.test")
            .await
            .is_err()
    );
    assert!(
        connect(&without_sni, server.tls_address, "a.test")
            .await
            .is_err()
    );
    wait_until("two rejected handshakes", || {
        server.tls.handshakes_rejected() == 2
    })
    .await;
    connect(&with_sni, server.tls_address, "a.test")
        .await
        .expect("a configured name still works");

    server.stop().await;
}

#[tokio::test]
async fn the_listener_negotiates_http_1_1_or_nothing() {
    let pki = Pki::new("alpn");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    // No HTTP/2 engine exists, so offering only h2 must not succeed.
    assert!(
        connect(&pki.client(&[b"h2"]), server.tls_address, "localhost")
            .await
            .is_err()
    );
    wait_until("the h2-only handshake is rejected", || {
        server.tls.handshakes_rejected() == 1
    })
    .await;

    let both = connect(
        &pki.client(&[b"h2", HTTP_1_1]),
        server.tls_address,
        "localhost",
    )
    .await
    .expect("h2 plus http/1.1");
    assert_eq!(both.get_ref().1.alpn_protocol(), Some(HTTP_1_1));

    // No ALPN at all means HTTP/1.1.
    let mut plain = connect(&pki.client(&[]), server.tls_address, "localhost")
        .await
        .expect("no ALPN");
    assert_eq!(plain.get_ref().1.alpn_protocol(), None);
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("request without ALPN");
    assert_eq!(read_response(&mut plain).await.1, b"ok");

    server.stop().await;
}

#[tokio::test]
async fn a_trickled_handshake_is_dropped_at_its_total_deadline_and_frees_its_slot() {
    const DEADLINE: Duration = Duration::from_millis(400);
    let pki = Pki::new("deadline");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(1),
        false,
        |address| {
            tls_listener(
                address,
                DEADLINE,
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    // A handshake record header announcing 512 bytes, then one byte every
    // 50 ms. Bytes keep arriving far more often than the deadline, so only a
    // total deadline, not an inactivity timer, can end this.
    let started = Instant::now();
    let trickle = TcpStream::connect(server.tls_address)
        .await
        .expect("connect trickle");
    let (mut reader, mut writer) = trickle.into_split();
    writer
        .write_all(&[0x16, 0x03, 0x01, 0x02, 0x00])
        .await
        .expect("record header");
    let feeder = tokio::spawn(async move {
        loop {
            sleep(Duration::from_millis(50)).await;
            if writer.write_all(&[0x01]).await.is_err() {
                break;
            }
        }
    });

    // The only admission slot is held; this client waits behind it.
    let connector = pki.client(&[HTTP_1_1]);
    let address = server.tls_address;
    let queued = tokio::spawn(async move {
        let mut stream = connect(&connector, address, "localhost")
            .await
            .expect("the queued client is served once the slot frees");
        let served_at = Instant::now();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("queued request");
        (served_at, read_response(&mut stream).await.1)
    });

    let mut discard = [0_u8; 64];
    timeout(GUARD, async {
        while matches!(reader.read(&mut discard).await, Ok(count) if count > 0) {}
    })
    .await
    .expect("the trickled handshake must be closed");
    let dropped_after = started.elapsed();
    feeder.abort();
    assert!(
        dropped_after >= DEADLINE - Duration::from_millis(50),
        "closed too early: {dropped_after:?}"
    );
    assert!(
        dropped_after < DEADLINE * 4,
        "the deadline did not bound the handshake: {dropped_after:?}"
    );

    let (served_at, body) = timeout(GUARD, queued)
        .await
        .expect("queued client within the guard")
        .expect("queued client task");
    assert_eq!(body, b"ok");
    assert!(served_at - started >= DEADLINE - Duration::from_millis(50));
    assert_eq!(server.tls.handshakes_timed_out(), 1);

    server.stop().await;
}

#[tokio::test]
async fn shutdown_cancels_a_pending_handshake_without_waiting_for_its_deadline() {
    let pki = Pki::new("shutdown-handshake");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(30),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    let mut silent = TcpStream::connect(server.tls_address)
        .await
        .expect("connect silent peer");
    // Let the accept loop admit it and start the handshake.
    sleep(Duration::from_millis(100)).await;
    let stopped_in = server.stop().await;
    assert!(
        stopped_in < Duration::from_secs(2),
        "shutdown waited on a handshake: {stopped_in:?}"
    );
    let mut byte = [0_u8; 1];
    let closed = timeout(GUARD, silent.read(&mut byte))
        .await
        .expect("the socket is closed");
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
}

#[tokio::test]
async fn cleartext_http_sent_to_the_tls_listener_receives_no_http_response() {
    let pki = Pki::new("plaintext");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    let mut plain = TcpStream::connect(server.tls_address)
        .await
        .expect("connect");
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("plaintext request");
    let mut answer = Vec::new();
    let _ = timeout(GUARD, plain.read_to_end(&mut answer))
        .await
        .expect("the connection is closed");
    assert!(
        !answer.starts_with(b"HTTP/"),
        "a TLS listener must never answer in cleartext: {answer:?}"
    );
    wait_until("the plaintext handshake is rejected", || {
        server.tls.handshakes_rejected() == 1
    })
    .await;

    let mut tls = connect(&pki.client(&[HTTP_1_1]), server.tls_address, "localhost")
        .await
        .expect("the listener still serves TLS");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("request");
    assert_eq!(read_response(&mut tls).await.1, b"ok");

    server.stop().await;
}

struct LargeFile {
    root: PathBuf,
}

impl LargeFile {
    fn create(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hamster-wheel-tls-files-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create static root");
        let large = std::fs::File::create(root.join("large.bin")).expect("create large file");
        large.set_len(32 * 1_024 * 1_024).expect("size large file");
        std::fs::write(root.join("healthy.txt"), b"healthy").expect("write healthy file");
        Self { root }
    }

    fn handler(&self) -> Handler {
        Handler::static_files(Arc::new(
            std::fs::canonicalize(&self.root).expect("canonical root"),
        ))
    }

    /// Fails while any open handle, such as a live streaming body, remains.
    fn remove(self) {
        std::fs::remove_file(self.root.join("large.bin")).expect("the file handle was released");
        std::fs::remove_file(self.root.join("healthy.txt")).expect("remove healthy file");
        std::fs::remove_dir(&self.root).expect("remove static root");
    }
}

#[tokio::test]
async fn a_tls_client_that_stops_reading_is_released_by_the_socket_write_deadline() {
    let pki = Pki::new("write-deadline");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let files = LargeFile::create("write-deadline");
    let server = Server::start(
        router(vec![("localhost", files.handler())]),
        limits(1).with_write_progress_timeout(Duration::from_millis(200)),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;
    let connector = pki.client(&[HTTP_1_1]);

    let mut stalled = connect(&connector, server.tls_address, "localhost")
        .await
        .expect("stalled client handshake");
    stalled
        .write_all(b"GET /large.bin HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request the large file");
    let head = read_head(&mut stalled, &mut Vec::new()).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    // max_connections = 1: this handshake cannot even begin until the stalled
    // exchange gives its slot back.
    let waiting = timeout(Duration::from_secs(5), async {
        let mut stream = connect(&connector, server.tls_address, "localhost")
            .await
            .expect("waiting client handshake");
        stream
            .write_all(b"GET /healthy.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("healthy request");
        read_response(&mut stream).await.1
    })
    .await
    .expect("the socket write deadline beneath TLS must release the slot");
    assert_eq!(waiting, b"healthy");

    drop(stalled);
    server.stop().await;
    files.remove();
}

/// Lingering close beneath TLS: a request refused before its body is read
/// reaches a client that sends the whole body first, over HTTPS as over
/// cleartext. Hyper's shutdown sends close_notify through rustls, then the
/// socket wrapper beneath it drains the remaining TLS records.
#[tokio::test]
async fn an_early_refusal_over_tls_reaches_a_client_that_sends_its_whole_body_first() {
    const BODY: usize = 20 * 1_024 * 1_024;
    let pki = Pki::new("tls-early-refusal");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let budget = crate::body_budget::BodyBudget::new(64 * 1_024);
    let server = Server::start(
        router(vec![("localhost", Handler::body_echo(budget, None))]),
        limits(4),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;
    let connector = pki.client(&[HTTP_1_1]);

    // Even attempts send everything at once; odd ones pause after a little
    // body while the server refuses and closes.
    for attempt in 0..4 {
        let mut stream = connect(&connector, server.tls_address, "localhost")
            .await
            .expect("handshake");
        let head = format!("POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {BODY}\r\n\r\n");
        let mut request = head.into_bytes();
        request.extend(std::iter::repeat_n(b'x', BODY));
        let pause_after = if attempt % 2 == 1 {
            1_024
        } else {
            request.len()
        };
        timeout(GUARD, async {
            stream.write_all(&request[..pause_after]).await?;
            sleep(Duration::from_millis(200)).await;
            stream.write_all(&request[pause_after..]).await?;
            stream.flush().await
        })
        .await
        .expect("the server keeps reading until the body is sent")
        .unwrap_or_else(|error| panic!("attempt {attempt}: sending the body: {error}"));
        let head = read_head(&mut stream, &mut Vec::new()).await;
        assert!(
            head.starts_with("HTTP/1.1 413 "),
            "attempt {attempt}: {head}"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn graceful_shutdown_bounds_a_tls_download_stalled_on_the_client() {
    let pki = Pki::new("drain");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let files = LargeFile::create("drain");
    let server = Server::start(
        router(vec![("localhost", files.handler())]),
        HttpLimits::for_test(
            4,
            Duration::from_millis(300),
            Duration::from_secs(1),
            64 * 1_024,
        ),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;

    let mut stalled = connect(&pki.client(&[HTTP_1_1]), server.tls_address, "localhost")
        .await
        .expect("handshake");
    stalled
        .write_all(b"GET /large.bin HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request the large file");
    let head = read_head(&mut stalled, &mut Vec::new()).await;
    assert!(head.contains("content-length: 33554432"), "{head}");

    let stopped_in = server.stop().await;
    assert!(
        stopped_in < Duration::from_secs(3),
        "a stalled TLS download held shutdown for {stopped_in:?}"
    );
    files.remove();
    drop(stalled);
}

#[tokio::test]
async fn a_tls_peer_that_vanishes_mid_handshake_is_counted_as_abandoned() {
    let pki = Pki::new("abandon");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&leaf, &["localhost"], false)],
            )
        },
    )
    .await;
    let mut partial = TcpStream::connect(server.tls_address)
        .await
        .expect("connect");
    partial
        .write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01])
        .await
        .expect("partial record");
    drop(partial);
    wait_until("the abandoned handshake is counted", || {
        server.tls.handshakes_abandoned() == 1
    })
    .await;
    assert_eq!(server.tls.handshakes_rejected(), 0);
    assert_eq!(server.tls.handshakes_timed_out(), 0);
    server.stop().await;
}

/// Pins where the write deadline sits. Here the response fits in rustls's
/// own buffer, so `poll_write` succeeds and only `poll_flush` meets the
/// stalled socket. Beneath TLS the deadline sees that stall; above TLS the
/// flush would be delegated and wait forever.
#[tokio::test]
async fn a_flush_stalled_beneath_tls_times_out_instead_of_waiting_forever() {
    use crate::write_progress::WriteProgressTimeout;
    use tokio::io::AsyncWriteExt;

    let pki = Pki::new("flush");
    let leaf = pki.leaf("localhost", &["localhost"]);
    let tls = tls_listener(
        "127.0.0.1:0".parse().unwrap(),
        Duration::from_secs(5),
        vec![source(&leaf, &["localhost"], false)],
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1_024);
    let server_io = WriteProgressTimeout::new(server_io, Duration::from_millis(200));
    let connector = pki.client(&[HTTP_1_1]);
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let (server, client) = tokio::join!(
        tls.handshake(server_io, Instant::now() + GUARD),
        connector.connect(name, client_io)
    );
    let (mut server, _scope) = server.expect("server handshake");
    let _client = client.expect("client handshake");

    // The client never reads again. 40 KiB fits rustls's 64 KiB buffer but
    // not the 16 KiB pipe.
    let written = timeout(GUARD, server.write(&[b'x'; 40 * 1_024]))
        .await
        .expect("rustls accepts plaintext into its buffer")
        .expect("write");
    assert_eq!(written, 40 * 1_024);
    let flushed = timeout(GUARD, server.flush())
        .await
        .expect("the flush must end by the write deadline, not the test guard");
    assert_eq!(
        flushed.expect_err("a stalled flush is an error").kind(),
        std::io::ErrorKind::TimedOut
    );
}

/// Status line and body of one request on a fresh connection that presented
/// whatever certificate `sni` selects (`None` sends no SNI).
async fn request_over(address: SocketAddr, sni: Option<&str>, request: &str) -> (String, Vec<u8>) {
    let (connector, _) = recording_client(sni.is_some(), &[]);
    let mut stream = connect(&connector, address, sni.unwrap_or("fallback.test"))
        .await
        .expect("handshake");
    stream.write_all(request.as_bytes()).await.expect("request");
    let (head, body) = read_response(&mut stream).await;
    (head.lines().next().unwrap_or_default().to_owned(), body)
}

/// RFC 9110 sections 4.3.3 and 15.5.20: over TLS, Hamster serves a request
/// only for a host the presented certificate's configured names cover. A
/// certificate naming several hosts serves all of them; the default
/// certificate serves only its own names; an IP name authorizes an IP Host;
/// a configured virtual host that no certificate names is not reachable over
/// TLS at all, but still over cleartext.
#[tokio::test]
async fn a_tls_connection_serves_only_the_hosts_its_certificate_names() {
    let pki = Pki::new("scope");
    let shared = pki.leaf("shared", &["a.test", "b.test"]);
    let fallback = pki.leaf("fallback", &["fallback.test", "127.0.0.1"]);
    let server = Server::start(
        router(vec![
            ("a.test", Handler::simple(StatusCode::OK, "site a")),
            ("b.test", Handler::simple(StatusCode::OK, "site b")),
            ("fallback.test", Handler::simple(StatusCode::OK, "fallback")),
            ("127.0.0.1", Handler::simple(StatusCode::OK, "by address")),
            (
                "uncovered.test",
                Handler::simple(StatusCode::OK, "uncovered"),
            ),
        ]),
        limits(8),
        true,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![
                    source(&shared, &["a.test", "b.test"], false),
                    source(&fallback, &["fallback.test", "127.0.0.1"], true),
                ],
            )
        },
    )
    .await;
    let get = |host: &str| format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");

    for (sni, host, status, body) in [
        (Some("a.test"), "b.test", "200", &b"site b"[..]),
        (
            Some("a.test"),
            "fallback.test",
            "421",
            b"Misdirected Request",
        ),
        (
            Some("a.test"),
            "uncovered.test",
            "421",
            b"Misdirected Request",
        ),
        (
            Some("a.test"),
            "unknown.test",
            "421",
            b"Misdirected Request",
        ),
        (None, "127.0.0.1", "200", b"by address"),
        (None, "a.test", "421", b"Misdirected Request"),
        (Some("unknown.test"), "fallback.test", "200", b"fallback"),
        (
            Some("unknown.test"),
            "b.test",
            "421",
            b"Misdirected Request",
        ),
    ] {
        let (status_line, received) = request_over(server.tls_address, sni, &get(host)).await;
        assert!(
            status_line.starts_with(&format!("HTTP/1.1 {status} ")),
            "sni={sni:?} host={host}: {status_line}"
        );
        assert_eq!(received, body, "sni={sni:?} host={host}");
    }

    // Absolute-form is judged by its target authority, not by Host.
    let (status_line, _) = request_over(
        server.tls_address,
        Some("a.test"),
        "GET https://fallback.test/ HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status_line.starts_with("HTTP/1.1 421 "), "{status_line}");

    // Cleartext has no certificate and keeps routing every configured host.
    let mut clear = TcpStream::connect(server.cleartext.expect("cleartext listener"))
        .await
        .expect("connect cleartext");
    clear
        .write_all(get("uncovered.test").as_bytes())
        .await
        .expect("cleartext request");
    assert_eq!(read_response(&mut clear).await.1, b"uncovered");

    server.stop().await;
}

#[tokio::test]
async fn successful_reload_keeps_old_tls_and_cleartext_connections_serving() {
    let pki = Pki::new("reload-keepalive");
    let old = pki.leaf("old", &["localhost"]);
    let new = pki.leaf("new", &["localhost"]);
    let server = Server::start(
        router(vec![(
            "localhost",
            Handler::simple(StatusCode::OK, "alive"),
        )]),
        limits(8),
        true,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&old, &["localhost"], true)],
            )
        },
    )
    .await;

    let mut old_connection = connect(&pki.client(&[HTTP_1_1]), server.tls_address, "localhost")
        .await
        .expect("old handshake");
    assert_eq!(
        old_connection.get_ref().1.peer_certificates().unwrap()[0],
        old.der
    );
    old_connection
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_response(&mut old_connection).await.1, b"alive");

    replace_certificate(&old, &new);
    server
        .tls
        .reload_certificates()
        .await
        .expect("valid reload");
    assert_eq!(
        presented_certificate(server.tls_address, "localhost").await,
        new.der
    );
    connect(&pki.client(&[HTTP_1_1]), server.tls_address, "localhost")
        .await
        .expect("verifying client accepts new certificate");

    old_connection
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_response(&mut old_connection).await.1, b"alive");
    assert_eq!(
        old_connection.get_ref().1.peer_certificates().unwrap()[0],
        old.der
    );

    let mut clear = TcpStream::connect(server.cleartext.unwrap()).await.unwrap();
    clear
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_response(&mut clear).await.1, b"alive");
    server.stop().await;
}

#[tokio::test]
async fn reload_is_all_or_nothing_across_sni_entries_and_successive_generations() {
    let pki = Pki::new("reload-multiple");
    let a1 = pki.leaf("a1", &["a.test"]);
    let b1 = pki.leaf("b1", &["b.test"]);
    let a2 = pki.leaf("a2", &["a.test"]);
    let b2 = pki.leaf("b2", &["b.test"]);
    let a3 = pki.leaf("a3", &["a.test"]);
    let b3 = pki.leaf("b3", &["b.test"]);
    let wrong_name = pki.leaf("wrong-name", &["wrong.test"]);
    let server = Server::start(
        router(vec![
            ("a.test", Handler::simple(StatusCode::OK, "a")),
            ("b.test", Handler::simple(StatusCode::OK, "b")),
        ]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![
                    source(&a1, &["a.test"], false),
                    source(&b1, &["b.test"], true),
                ],
            )
        },
    )
    .await;
    let mut old_a = connect(&pki.client(&[HTTP_1_1]), server.tls_address, "a.test")
        .await
        .unwrap();

    replace_certificate(&a1, &a2);
    std::fs::copy(&b2.cert, &b1.cert).unwrap();
    let error = server
        .tls
        .reload_certificates()
        .await
        .expect_err("mismatched B key fails entire set");
    assert!(error.contains("cannot be used together"), "{error}");
    assert_eq!(
        presented_certificate(server.tls_address, "a.test").await,
        a1.der
    );
    assert_eq!(
        presented_certificate(server.tls_address, "b.test").await,
        b1.der
    );

    std::fs::copy(&b2.key, &b1.key).unwrap();
    server
        .tls
        .reload_certificates()
        .await
        .expect("both replacements valid");
    assert_eq!(
        presented_certificate(server.tls_address, "a.test").await,
        a2.der
    );
    assert_eq!(
        presented_certificate(server.tls_address, "b.test").await,
        b2.der
    );
    let (no_sni, seen) = recording_client(false, &[]);
    connect(&no_sni, server.tls_address, "b.test")
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().as_ref(), Some(&b2.der));

    // Old connections retain their original authority scope after the swap.
    old_a
        .write_all(b"GET / HTTP/1.1\r\nHost: b.test\r\n\r\n")
        .await
        .unwrap();
    assert!(
        read_response(&mut old_a)
            .await
            .0
            .starts_with("HTTP/1.1 421 ")
    );
    old_a
        .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_response(&mut old_a).await.1, b"a");

    replace_certificate(&a1, &wrong_name);
    let error = server
        .tls
        .reload_certificates()
        .await
        .expect_err("SAN mismatch fails reload");
    assert!(error.contains("is not covered"), "{error}");
    assert_eq!(
        presented_certificate(server.tls_address, "a.test").await,
        a2.der
    );
    assert_eq!(
        presented_certificate(server.tls_address, "b.test").await,
        b2.der
    );

    replace_certificate(&a1, &a3);
    replace_certificate(&b1, &b3);
    server
        .tls
        .reload_certificates()
        .await
        .expect("second valid reload");
    assert_eq!(
        presented_certificate(server.tls_address, "a.test").await,
        a3.der
    );
    assert_eq!(
        presented_certificate(server.tls_address, "b.test").await,
        b3.der
    );
    server.stop().await;
}

#[tokio::test]
async fn a_handshake_in_progress_finishes_with_its_original_certificate_set() {
    let pki = Pki::new("reload-inflight");
    let old = pki.leaf("old", &["localhost"]);
    let new = pki.leaf("new", &["localhost"]);
    let server = Server::start(
        router(vec![("localhost", Handler::simple(StatusCode::OK, "ok"))]),
        limits(8),
        false,
        |address| {
            tls_listener(
                address,
                Duration::from_secs(5),
                vec![source(&old, &["localhost"], true)],
            )
        },
    )
    .await;

    let config = pki.client(&[HTTP_1_1]);
    let mut client = ClientConnection::new(
        Arc::clone(config.config()),
        ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let mut hello = Vec::new();
    client.write_tls(&mut hello).unwrap();
    let mut socket = TcpStream::connect(server.tls_address).await.unwrap();
    socket.write_all(&hello[..5]).await.unwrap();
    wait_until("server captured the old handshake snapshot", || {
        server.tls.handshakes_started() == 1
    })
    .await;

    replace_certificate(&old, &new);
    server
        .tls
        .reload_certificates()
        .await
        .expect("reload while handshake waits for ClientHello");
    socket.write_all(&hello[5..]).await.unwrap();
    while client.is_handshaking() {
        let mut bytes = [0_u8; 8_192];
        let count = timeout(GUARD, socket.read(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(count, 0, "server closed in-flight handshake");
        client.read_tls(&mut Cursor::new(&bytes[..count])).unwrap();
        client.process_new_packets().unwrap();
        if client.wants_write() {
            let mut reply = Vec::new();
            client.write_tls(&mut reply).unwrap();
            socket.write_all(&reply).await.unwrap();
        }
    }
    assert_eq!(client.peer_certificates().unwrap()[0], old.der);
    assert_eq!(
        presented_certificate(server.tls_address, "localhost").await,
        new.der
    );
    server.stop().await;
}
