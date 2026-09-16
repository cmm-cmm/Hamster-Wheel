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
        async move { router.serve(request, transport).await }
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
        let limits = HttpLimits::for_test(4, Duration::from_millis(200), 8 * 1_024);
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
        let limits = HttpLimits::for_test(1, Duration::from_millis(250), 8 * 1_024);
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
    async fn http1_buffer_limit_rejects_oversized_head_and_server_recovers() {
        let limits = HttpLimits::for_test(4, Duration::from_secs(1), 8 * 1_024);
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
}
