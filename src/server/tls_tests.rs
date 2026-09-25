//! Boundaries the TLS listener adds, exercised over real sockets: the total
//! handshake deadline and the admission slot it holds, shutdown during a
//! handshake, SNI certificate selection versus Host routing, ALPN, the
//! socket write deadline beneath TLS, graceful drain, and close_notify.

use std::{
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
async fn sni_selects_the_certificate_while_the_host_header_selects_the_virtual_host() {
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

    // SNI chose the certificate; the effective authority still routes.
    let (connector, _) = recording_client(true, &[]);
    let mut stream = connect(&connector, server.tls_address, "a.test")
        .await
        .expect("handshake as a.test");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x.b.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("request for another host");
    assert_eq!(read_response(&mut stream).await.1, b"site b");

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
    let mut server = server.expect("server handshake");
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
