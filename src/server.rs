use std::{fmt, future::Future, io, sync::Arc};

use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

use crate::{
    config::{HttpLimits, RuntimeConfig},
    php_worker::PhpWorkerError,
    router::HostRouter,
    transport::TransportInfo,
};

#[derive(Debug)]
pub enum ServerError {
    Io(io::Error),
    Worker(PhpWorkerError),
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "server I/O: {error}"),
            Self::Worker(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Worker(error) => Some(error),
        }
    }
}

impl From<io::Error> for ServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PhpWorkerError> for ServerError {
    fn from(error: PhpWorkerError) -> Self {
        Self::Worker(error)
    }
}

pub async fn run(config: Arc<RuntimeConfig>) -> Result<(), ServerError> {
    for worker in config.start_workers().await? {
        println!(
            "PHP worker {} ready after {:.3} ms",
            worker.pid,
            worker.elapsed.as_secs_f64() * 1_000.0
        );
    }
    let address = config.listen_address();
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = config.shutdown_workers().await;
            return Err(ServerError::Io(error));
        }
    };
    let shutdown = match shutdown_signal() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            let _ = config.shutdown_workers().await;
            return Err(ServerError::Io(error));
        }
    };
    // Printed once at startup, beside the other readiness lines. A refused
    // request is only explainable if the operator can see what the ceiling was
    // set to, and a configured limit is not something a running server should
    // have to be asked for.
    match config.http_limits().max_body_memory() {
        0 => println!("Request body memory budget disabled (server.max_body_memory = 0)"),
        limit => println!("Request body memory budget {limit} bytes"),
    }
    println!("Hamster Wheel listening on http://{address}");

    let router = config.router_arc();
    let serve_result = serve_with_router(listener, router, config.http_limits(), shutdown).await;
    let shutdown_result = config.shutdown_workers().await;
    serve_result?;
    shutdown_result?;
    Ok(())
}

pub async fn serve_with_router<F>(
    listener: TcpListener,
    router: Arc<HostRouter>,
    http_limits: HttpLimits,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            accept_result = listener.accept(), if connections.len() < http_limits.max_connections() => {
                let (stream, peer_addr) = accept_result?;
                let local_addr = match stream.local_addr() {
                    Ok(local_addr) => local_addr,
                    Err(error) => {
                        eprintln!("failed to identify accepted connection: {error}");
                        continue;
                    }
                };
                let transport = TransportInfo::new(peer_addr, local_addr);
                let shutdown_rx = shutdown_rx.clone();
                let router = Arc::clone(&router);

                connections.spawn(async move {
                    serve_connection(stream, transport, router, http_limits, shutdown_rx).await;
                });
            }
            join_result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = join_result {
                    eprintln!("connection task failed: {error}");
                }
            }
            () = &mut shutdown => break,
        }
    }

    drop(listener);
    let _ = shutdown_tx.send(true);

    while let Some(join_result) = connections.join_next().await {
        if let Err(error) = join_result {
            eprintln!("connection task failed during shutdown: {error}");
        }
    }

    println!("Hamster Wheel stopped");
    Ok(())
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    transport: TransportInfo,
    router: Arc<HostRouter>,
    http_limits: HttpLimits,
    mut shutdown: watch::Receiver<bool>,
) {
    let service = hyper::service::service_fn(move |request| {
        let router = Arc::clone(&router);
        async move {
            router
                .serve(request, transport, http_limits.body_read_timeout())
                .await
        }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(http_limits.header_read_timeout())
        .max_buf_size(http_limits.http1_max_buffer_size());
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);

    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result && should_report_connection_error(&error) {
                eprintln!(
                    "failed to serve connection from {} to {}: {error}",
                    transport.peer_addr(),
                    transport.local_addr()
                );
            }
        }
        changed = shutdown.changed() => {
            if changed.is_ok() && *shutdown.borrow() {
                connection.as_mut().graceful_shutdown();
                if let Err(error) = connection.await && should_report_connection_error(&error) {
                    eprintln!(
                        "failed to gracefully close connection from {} to {}: {error}",
                        transport.peer_addr(),
                        transport.local_addr()
                    );
                }
            }
        }
    }
}

fn should_report_connection_error(error: &hyper::Error) -> bool {
    // These are controlled by the remote peer and are expected admission
    // outcomes. Synchronously logging every one would turn malformed traffic
    // into a stderr backpressure path.
    !(error.is_parse()
        || error.is_timeout()
        || error.is_incomplete_message()
        || error.is_closed()
        || error.is_canceled())
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}

#[cfg(not(windows))]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::oneshot,
    };

    use super::*;
    use crate::body_budget::BodyBudget;
    use crate::router::{Handler, HostRouter, LocationRouter, VirtualHost};

    fn transport_router() -> HostRouter {
        let locations = Arc::new(
            LocationRouter::new(Vec::new(), Handler::transport_echo()).expect("valid locations"),
        );
        HostRouter::new(vec![
            VirtualHost::new("127.0.0.1", locations).expect("valid host"),
        ])
        .expect("valid router")
    }

    fn body_router(budget: Arc<BodyBudget>, hold: Option<Duration>) -> HostRouter {
        let locations = Arc::new(
            LocationRouter::new(Vec::new(), Handler::body_echo(budget, hold))
                .expect("valid locations"),
        );
        HostRouter::new(vec![
            VirtualHost::new("127.0.0.1", locations).expect("valid host"),
        ])
        .expect("valid router")
    }

    async fn start_test_server(
        http_limits: HttpLimits,
    ) -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(transport_router()),
            http_limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        (listen_addr, shutdown_tx, server)
    }

    async fn start_body_server(
        http_limits: HttpLimits,
    ) -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(body_router(BodyBudget::disabled(), None)),
            http_limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        (listen_addr, shutdown_tx, server)
    }

    async fn connect(listen_addr: std::net::SocketAddr) -> TcpStream {
        TcpStream::connect(listen_addr)
            .await
            .expect("connect client")
    }

    async fn read_response_body(stream: &mut TcpStream) -> String {
        let mut response = Vec::new();
        let header_end = loop {
            if let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0_u8; 512];
            let count = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .expect("response header timeout")
                .expect("read response");
            assert_ne!(count, 0, "connection closed before response headers");
            response.extend_from_slice(&chunk[..count]);
        };

        let headers = std::str::from_utf8(&response[..header_end]).expect("ASCII response headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length").then(|| {
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("numeric content length")
                    })
                })
            })
            .expect("content-length header");

        while response.len() - header_end < content_length {
            let mut chunk = [0_u8; 512];
            let count = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .expect("response body timeout")
                .expect("read response body");
            assert_ne!(count, 0, "connection closed before response body");
            response.extend_from_slice(&chunk[..count]);
        }

        String::from_utf8(response[header_end..header_end + content_length].to_vec())
            .expect("UTF-8 response body")
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

    #[tokio::test]
    async fn carries_actual_socket_identity_across_keep_alive_requests() {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        assert!(listen_addr.ip().is_unspecified());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(transport_router()),
            HttpLimits::default(),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        let mut malformed = TcpStream::connect((Ipv4Addr::LOCALHOST, listen_addr.port()))
            .await
            .expect("connect malformed client");
        malformed
            .write_all(b"GET / NOT-HTTP\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write malformed request");
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut discard = [0_u8; 512];
            while malformed.read(&mut discard).await.expect("read close") != 0 {}
        })
        .await
        .expect("malformed connection must be closed");

        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listen_addr.port()))
            .await
            .expect("connect client");
        let peer_addr = client.local_addr().expect("client address");
        let local_addr = client.peer_addr().expect("server address");
        assert!(!local_addr.ip().is_unspecified());
        let expected = format!("peer={peer_addr};local={local_addr}");

        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                  Forwarded: for=203.0.113.7\r\n\
                  X-Forwarded-For: 198.51.100.9\r\n\r\n",
            )
            .await
            .expect("write first request");
        let first = read_response_body(&mut client).await;
        assert_eq!(first, expected);
        assert!(!first.contains("203.0.113.7"));
        assert!(!first.contains("198.51.100.9"));

        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                  X-Forwarded-For: 192.0.2.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write second request");
        assert_eq!(read_response_body(&mut client).await, expected);
        drop(client);

        let mut clients = JoinSet::new();
        for _ in 0..32 {
            clients.spawn(async move {
                let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listen_addr.port()))
                    .await
                    .expect("connect concurrent client");
                let peer_addr = client.local_addr().expect("concurrent client address");
                let local_addr = client.peer_addr().expect("concurrent server address");
                client
                    .write_all(
                        b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                          X-Forwarded-For: 203.0.113.200\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write concurrent request");
                assert_eq!(
                    read_response_body(&mut client).await,
                    format!("peer={peer_addr};local={local_addr}")
                );
            });
        }
        while let Some(result) = clients.join_next().await {
            result.expect("join concurrent client");
        }

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    #[tokio::test]
    async fn header_timeout_bounds_partial_and_idle_connections() {
        let limits = HttpLimits::for_test(
            4,
            Duration::from_millis(200),
            Duration::from_secs(1),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) = start_test_server(limits).await;

        let mut partial = connect(listen_addr).await;
        partial
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1")
            .await
            .expect("write partial header");
        let mut byte = [0_u8; 1];
        let count = tokio::time::timeout(Duration::from_secs(1), partial.read(&mut byte))
            .await
            .expect("partial header should time out")
            .expect("read partial connection close");
        assert_eq!(count, 0, "timed-out partial header must close");

        let mut idle = connect(listen_addr).await;
        idle.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write complete request");
        let body = read_response_body(&mut idle).await;
        assert!(body.starts_with("peer="));
        let count = tokio::time::timeout(Duration::from_secs(1), idle.read(&mut byte))
            .await
            .expect("idle keep-alive should time out")
            .expect("read idle connection close");
        assert_eq!(count, 0, "timed-out keep-alive must close");

        let mut healthy = connect(listen_addr).await;
        healthy
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .expect("write recovery request");
        assert!(read_response_body(&mut healthy).await.starts_with("peer="));

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    #[tokio::test]
    async fn connection_limit_pauses_accept_and_shutdown_remains_bounded() {
        // Hyper's graceful shutdown finishes a request head already being read;
        // the header deadline, rather than cancellation of a possible service
        // future, is what bounds this case.
        let limits = HttpLimits::for_test(
            1,
            Duration::from_millis(250),
            Duration::from_secs(1),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) = start_test_server(limits).await;

        let mut first = connect(listen_addr).await;
        first
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write first request");
        assert!(read_response_body(&mut first).await.starts_with("peer="));
        first
            .write_all(b"GET / HTTP/1.1\r\n")
            .await
            .expect("write partial next request");

        let mut queued = connect(listen_addr).await;
        queued
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write queued request");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_response_body(&mut queued))
                .await
                .is_err(),
            "a second connection must not be dispatched while the slot is owned"
        );

        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), read_response_body(&mut queued))
                .await
                .expect("queued connection should be admitted after a slot is released")
                .starts_with("peer=")
        );
        queued
            .write_all(b"GET / HTTP/1.1\r\n")
            .await
            .expect("write partial request before shutdown");

        shutdown_tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("shutdown at the connection limit must be bounded")
            .expect("join server")
            .expect("server result");

        let mut byte = [0_u8; 1];
        let count = queued
            .read(&mut byte)
            .await
            .expect("read graceful shutdown close");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn body_timeout_releases_connection_and_preserves_keep_alive() {
        let limits = HttpLimits::for_test(
            1,
            Duration::from_secs(1),
            Duration::from_millis(300),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) = start_body_server(limits).await;

        let mut slow = connect(listen_addr).await;
        slow.write_all(
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n",
        )
        .await
        .expect("write incomplete body headers");
        let interim = read_response_head(&mut slow).await;
        assert!(
            interim.starts_with(b"HTTP/1.1 100 Continue\r\n"),
            "body consumer should receive an interim continue response: {:?}",
            String::from_utf8_lossy(&interim)
        );
        slow.write_all(b"x").await.expect("write first body byte");

        let mut queued = connect(listen_addr).await;
        queued
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\ndone")
            .await
            .expect("write queued request");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_response_head(&mut queued))
                .await
                .is_err(),
            "queued connection must wait while the slow body owns the only slot"
        );

        // Bytes arrive more often than the deadline. This distinguishes a total
        // body-read deadline from an inactivity timer that is reset per chunk.
        let slow_started = tokio::time::Instant::now();
        let slow_writer = tokio::spawn(async move {
            let (mut reader, mut writer) = slow.into_split();
            let reader_task = tokio::spawn(async move {
                let mut response = Vec::new();
                let result =
                    tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut response))
                        .await;
                (result, response)
            });
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(75)).await;
                if writer.write_all(b"x").await.is_err() {
                    break;
                }
            }
            reader_task.await.expect("slow response reader task")
        });
        let (result, slow_response) = slow_writer.await.expect("slow client task");
        let elapsed = slow_started.elapsed();
        let read_result = result.expect("body timeout should close the slow connection");
        assert!(
            read_result.is_ok(),
            "timed-out body response should be readable, got {read_result:?}"
        );
        assert!(
            slow_response.starts_with(b"HTTP/1.1 408 Request Timeout\r\n"),
            "slow body must receive 408, got {:?}",
            String::from_utf8_lossy(&slow_response)
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "body timeout should close before the slow writer finishes: {elapsed:?}"
        );

        assert_eq!(read_response_body(&mut queued).await, "done");
        queued
            .write_all(
                b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await
            .expect("write keep-alive recovery request");
        assert_eq!(read_response_body(&mut queued).await, "ok");

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    #[tokio::test]
    async fn shutdown_waits_for_body_deadline_but_not_forever() {
        let limits = HttpLimits::for_test(
            1,
            Duration::from_secs(1),
            Duration::from_millis(200),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) = start_body_server(limits).await;
        let mut slow = connect(listen_addr).await;
        slow.write_all(
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n",
        )
        .await
        .expect("write incomplete body headers");
        assert!(
            read_response_head(&mut slow)
                .await
                .starts_with(b"HTTP/1.1 100 Continue\r\n")
        );
        slow.write_all(b"x").await.expect("write first body byte");

        shutdown_tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("shutdown should wait only for body deadline")
            .expect("join server")
            .expect("server result");
        drop(slow);
    }

    #[tokio::test]
    async fn http1_buffer_limit_rejects_oversized_head_and_server_recovers() {
        let limits =
            HttpLimits::for_test(4, Duration::from_secs(1), Duration::from_secs(1), 8 * 1_024);
        let (listen_addr, shutdown_tx, server) = start_test_server(limits).await;

        let mut oversized = connect(listen_addr).await;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Oversized: {}\r\n\r\n",
            "x".repeat(9 * 1_024)
        );
        oversized
            .write_all(request.as_bytes())
            .await
            .expect("write oversized request head");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), oversized.read_to_end(&mut response))
            .await
            .expect("oversized request should be closed")
            .expect("read oversized response");
        assert!(
            response.starts_with(b"HTTP/1.1 431 "),
            "oversized request should receive 431, got {:?}",
            String::from_utf8_lossy(&response)
        );

        let mut too_many = connect(listen_addr).await;
        let mut request = String::from("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for index in 0..100 {
            request.push_str(&format!("X-{index}: x\r\n"));
        }
        request.push_str("\r\n");
        too_many
            .write_all(request.as_bytes())
            .await
            .expect("write request with too many headers");
        response.clear();
        tokio::time::timeout(Duration::from_secs(1), too_many.read_to_end(&mut response))
            .await
            .expect("too many headers should close")
            .expect("read too-many-headers response");
        assert!(
            response.starts_with(b"HTTP/1.1 431 "),
            "the locked 100-header parser bound should return 431, got {:?}",
            String::from_utf8_lossy(&response)
        );

        let mut healthy = connect(listen_addr).await;
        healthy
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .expect("write recovery request");
        assert!(read_response_body(&mut healthy).await.starts_with("peer="));

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    async fn start_budget_server(
        http_limits: HttpLimits,
        budget: Arc<BodyBudget>,
        hold: Option<Duration>,
    ) -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(body_router(budget, hold)),
            http_limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        (listen_addr, shutdown_tx, server)
    }

    /// Waits on the budget counter rather than on a duration, so the test
    /// asserts the state it cares about instead of guessing how long a
    /// loaded machine needs to reach it.
    async fn wait_for_reserved(budget: &Arc<BodyBudget>, expected: usize, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while budget.reserved() != expected {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: reserved stayed at {} instead of reaching {expected}",
                budget.reserved()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn write_sized_body(stream: &mut TcpStream, bytes: usize) {
        let head = format!("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {bytes}\r\n\r\n");
        stream
            .write_all(head.as_bytes())
            .await
            .expect("write request head");
        stream
            .write_all(&vec![b'x'; bytes])
            .await
            .expect("write request body");
    }

    #[tokio::test]
    async fn the_body_budget_caps_concurrent_bodies_and_releases_every_one() {
        const BODY: usize = 64 * 1_024;
        const ADMITTED: usize = 3;

        let budget = BodyBudget::new(BODY * ADMITTED);
        let limits = HttpLimits::for_test(
            16,
            Duration::from_secs(10),
            Duration::from_secs(10),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) = start_budget_server(
            limits,
            Arc::clone(&budget),
            Some(Duration::from_millis(750)),
        )
        .await;

        // Saturate the budget with bodies the handler is still holding.
        let mut held = Vec::new();
        for _ in 0..ADMITTED {
            let mut stream = connect(listen_addr).await;
            write_sized_body(&mut stream, BODY).await;
            held.push(stream);
        }
        wait_for_reserved(&budget, BODY * ADMITTED, "saturating the budget").await;

        // One more body of the same, individually legal, size must be refused
        // now, and refused without waiting for anything to be released.
        let mut refused = connect(listen_addr).await;
        write_sized_body(&mut refused, BODY).await;
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), refused.read_to_end(&mut answer))
            .await
            .expect("an over-budget request must be answered immediately, not queued")
            .expect("read the refusal");
        assert!(
            answer.starts_with(b"HTTP/1.1 503 "),
            "over-budget body should be refused with 503, got {:?}",
            String::from_utf8_lossy(&answer[..answer.len().min(64)])
        );
        // The reason is carried in the body, because Hyper writes the canonical
        // reason phrase for the status. This is the signal that separates a
        // budget refusal from the pool's own 503s, so it is asserted rather
        // than left to change silently.
        assert!(
            answer.ends_with(b"Request Body Memory Budget"),
            "a budget refusal must name itself, got {:?}",
            String::from_utf8_lossy(&answer)
        );
        assert_eq!(
            budget.reserved(),
            BODY * ADMITTED,
            "a refusal must not consume or release budget"
        );
        assert_eq!(budget.rejections(), 1);

        // Every admitted body was served whole, and returning them returns the
        // budget.
        for mut stream in held {
            let body = read_response_body(&mut stream).await;
            assert_eq!(body.len(), BODY, "an admitted body must be echoed whole");
        }
        wait_for_reserved(&budget, 0, "releasing served bodies").await;

        // The ceiling is a ceiling, not a fuse: the server serves again.
        let mut recovered = connect(listen_addr).await;
        write_sized_body(&mut recovered, BODY).await;
        assert_eq!(read_response_body(&mut recovered).await.len(), BODY);
        wait_for_reserved(&budget, 0, "releasing the recovery body").await;

        assert!(
            budget.peak() <= budget.limit(),
            "peak {} crossed the {} byte ceiling",
            budget.peak(),
            budget.limit()
        );

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    #[tokio::test]
    async fn a_body_that_times_out_or_disconnects_returns_its_reservation() {
        const BODY: usize = 64 * 1_024;

        let budget = BodyBudget::new(BODY * 4);
        let limits = HttpLimits::for_test(
            16,
            Duration::from_secs(10),
            Duration::from_millis(300),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) =
            start_budget_server(limits, Arc::clone(&budget), None).await;

        // A declared body charges the budget before a byte of it arrives, so
        // sending slowly cannot be the cheap way past the ceiling.
        let mut slow = connect(listen_addr).await;
        slow.write_all(
            format!("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {BODY}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write head only");
        slow.write_all(b"x").await.expect("write one body byte");
        wait_for_reserved(&budget, BODY, "charging a declared body up front").await;

        let head = tokio::time::timeout(Duration::from_secs(2), read_response_head(&mut slow))
            .await
            .expect("the body deadline must fire");
        assert!(
            head.starts_with(b"HTTP/1.1 408 "),
            "a stalled body should still time out, got {:?}",
            String::from_utf8_lossy(&head[..head.len().min(64)])
        );
        wait_for_reserved(&budget, 0, "a timed-out body").await;
        drop(slow);

        // The same, but the client vanishes instead of waiting for the 408.
        let mut vanishing = connect(listen_addr).await;
        vanishing
            .write_all(
                format!("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {BODY}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write head only");
        vanishing
            .write_all(b"x")
            .await
            .expect("write one body byte");
        wait_for_reserved(&budget, BODY, "charging the vanishing client").await;
        drop(vanishing);
        wait_for_reserved(&budget, 0, "a disconnected body").await;

        // And the budget is still usable afterwards.
        let mut healthy = connect(listen_addr).await;
        write_sized_body(&mut healthy, BODY).await;
        assert_eq!(read_response_body(&mut healthy).await.len(), BODY);
        wait_for_reserved(&budget, 0, "the recovery body").await;

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    #[tokio::test]
    async fn an_undeclared_body_is_charged_as_it_grows_and_refused_when_it_cannot_grow() {
        // No Content-Length, so nothing can be charged up front. The reader
        // must acquire budget as the buffer grows, and give it all back.
        const CHUNK: usize = 16 * 1_024;

        let budget = BodyBudget::new(32 * 1_024);
        let limits = HttpLimits::for_test(
            16,
            Duration::from_secs(10),
            Duration::from_secs(10),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) =
            start_budget_server(limits, Arc::clone(&budget), None).await;

        let mut small = connect(listen_addr).await;
        small
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .expect("write chunked head");
        small
            .write_all(format!("{:x}\r\n", CHUNK).as_bytes())
            .await
            .expect("write chunk size");
        small
            .write_all(&vec![b'y'; CHUNK])
            .await
            .expect("write chunk data");
        small
            .write_all(b"\r\n0\r\n\r\n")
            .await
            .expect("write last chunk");
        assert_eq!(read_response_body(&mut small).await.len(), CHUNK);
        wait_for_reserved(&budget, 0, "a completed chunked body").await;
        assert_eq!(budget.rejections(), 0, "a body that fits is not refused");

        // Now one that outgrows the whole budget mid-stream. It must be
        // refused rather than allowed to allocate past the ceiling.
        let mut large = connect(listen_addr).await;
        large
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .expect("write chunked head");
        let oversized = 128 * 1_024;
        large
            .write_all(format!("{:x}\r\n", oversized).as_bytes())
            .await
            .expect("write chunk size");
        let _ = large.write_all(&vec![b'z'; oversized]).await;
        let _ = large.write_all(b"\r\n0\r\n\r\n").await;
        let head = tokio::time::timeout(Duration::from_secs(2), read_response_head(&mut large))
            .await
            .expect("an over-budget chunked body must be answered");
        assert!(
            head.starts_with(b"HTTP/1.1 503 "),
            "a chunked body that outgrows the budget should be refused with 503, got {:?}",
            String::from_utf8_lossy(&head[..head.len().min(64)])
        );
        wait_for_reserved(&budget, 0, "a refused chunked body").await;
        assert!(
            budget.peak() <= budget.limit(),
            "peak {} crossed the {} byte ceiling",
            budget.peak(),
            budget.limit()
        );

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }
}
