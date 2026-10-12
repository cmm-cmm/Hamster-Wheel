use std::{
    fmt,
    future::{Future, poll_fn},
    io,
    net::SocketAddr,
    sync::Arc,
    task::Poll,
};

use hyper::{body::Body as _, server::conn::http1};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::time::{Instant, timeout};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};

use crate::{
    access_log::{AccessLog, MaybeObserved},
    admin::{AdminState, Settings as AdminSettings},
    config::{HttpLimits, PhpPoolsError, RuntimeConfig},
    lingering_close::{CloseSignals, LingeringClose},
    metrics::ServerMetrics,
    router::HostRouter,
    tls::{CertificateScope, TlsListener},
    transport::{ConnectionInfo, Scheme, TransportInfo},
    write_progress::WriteProgressTimeout,
};

#[derive(Debug)]
pub enum ServerError {
    Io(io::Error),
    #[cfg(windows)]
    ChildContainment(Arc<crate::windows_child_containment::ContainmentError>),
    Worker(PhpPoolsError),
    IoWithWorkerCleanup {
        source: io::Error,
        cleanup: PhpPoolsError,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "server I/O: {error}"),
            #[cfg(windows)]
            Self::ChildContainment(error) => {
                write!(formatter, "Windows PHP child containment: {error}")
            }
            Self::Worker(error) => write!(formatter, "{error}"),
            Self::IoWithWorkerCleanup { source, cleanup } => write!(
                formatter,
                "server I/O: {source}; additional worker cleanup failure: {cleanup}"
            ),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            #[cfg(windows)]
            Self::ChildContainment(error) => Some(error.as_ref()),
            Self::Worker(error) => Some(error),
            Self::IoWithWorkerCleanup { source, .. } => Some(source),
        }
    }
}

impl From<io::Error> for ServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PhpPoolsError> for ServerError {
    fn from(error: PhpPoolsError) -> Self {
        Self::Worker(error)
    }
}

pub async fn run(config: Arc<RuntimeConfig>) -> Result<(), ServerError> {
    // Reported by the control endpoint's `status`, so it is taken before any
    // startup work rather than when the endpoint happens to be bound.
    #[cfg(windows)]
    let started = std::time::Instant::now();
    #[cfg(windows)]
    if !config.php_workers().is_empty() {
        crate::windows_child_containment::establish().map_err(ServerError::ChildContainment)?;
    }

    // Started before any worker or listener: a log target that cannot be
    // written is a startup failure, not something an operator discovers under
    // load. Absent configuration means no queue and no writer task at all.
    let (access_log, log_writer) = match config.access_log() {
        None => (None, None),
        Some(settings) => {
            let (log, writer) = crate::access_log::start(settings)?;
            outln!(
                "Access log writing to {} (queue {}, query {})",
                settings.target_description(),
                settings.queue_capacity(),
                if settings.include_query() {
                    "included"
                } else {
                    "omitted"
                }
            );
            (Some(log), Some(writer))
        }
    };

    // Counters exist only when something reads them, so a server with no
    // administrative listener pays nothing per request.
    let (metrics, admin_listener) = match config.admin() {
        None => (None, None),
        Some(settings) => {
            let listener = crate::admin::bind(settings)
                .await
                .map_err(|error| admin_bind_error(settings, error))?;
            (Some(ServerMetrics::new()), Some((settings, listener)))
        }
    };

    let workers = match config.start_workers().await {
        Ok(workers) => workers,
        Err(mut startup_error) => {
            // start_workers rolls back pools that completed startup. This
            // second convergence is intentional: the top-level owner must
            // still attempt every pool, including the pool that failed and
            // pools not yet reached, before it returns without a listener.
            if let Err(cleanup_error) = config.shutdown_workers().await {
                startup_error.append(cleanup_error);
            }
            return Err(ServerError::Worker(startup_error));
        }
    };
    for worker in workers {
        outln!(
            "PHP worker pool {} worker {} ready after {:.3} ms",
            worker.label,
            worker.pid,
            worker.elapsed.as_secs_f64() * 1_000.0
        );
    }
    for upstream in config.upstreams() {
        // The address was resolved once during configuration and is never
        // re-resolved, so it is printed where the operator will look for it.
        outln!(
            "Proxy upstream {} -> http://{}",
            upstream.label(),
            upstream.address()
        );
    }
    for backend in config.fastcgi_backends() {
        // Hamster does not start or supervise these endpoints; saying where
        // it will connect is what lets an operator match them up.
        let servers = backend
            .servers()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        outln!("FastCGI {} -> {servers}", backend.label());
    }
    // Every listener is bound before any is served, so a failure to bind the
    // second one never leaves the first serving on its own.
    let mut listeners = Vec::with_capacity(2);
    if let Some(address) = config.listen_address() {
        match TcpListener::bind(address).await {
            Ok(socket) => listeners.push(Listener::cleartext(socket)),
            Err(error) => return Err(stop_workers_after(&config, error).await),
        }
    }
    if let Some(tls) = config.tls() {
        match TcpListener::bind(tls.address()).await {
            Ok(socket) => listeners.push(Listener::tls(socket, Arc::clone(tls))),
            Err(error) => return Err(stop_workers_after(&config, error).await),
        }
    }
    let console_shutdown = match shutdown_signal() {
        Ok(shutdown) => shutdown,
        Err(error) => return Err(stop_workers_after(&config, error).await),
    };
    // Finish fallible signal setup before starting administrative tasks. A
    // later startup error must not leave a control task owning the config.
    let reload = if let Some(tls) = config.tls() {
        match certificate_reload_signal(Arc::clone(tls)) {
            Ok(reload) => Some(reload),
            Err(error) => return Err(stop_workers_after(&config, error).await),
        }
    } else {
        None
    };
    // A control `stop` must reach the same graceful path as Ctrl+C, so the two
    // triggers are merged into the one future `serve_listeners` already takes.
    // The endpoint is bound before serving: a name already in use is a startup
    // failure, not something discovered when an operator first needs it.
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    // Read by the control endpoint's `status` and by the administrative
    // `/ready`. Both keep answering while connections drain, so a health poll
    // during a stop must be able to see that the stop is deliberate.
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Annotated because the only assignment is Windows-only; without this the
    // type could not be inferred on other targets.
    let mut control_task: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)> = None;
    #[cfg(windows)]
    if let Some(name) = config.control_name() {
        match crate::control::ControlListener::bind(name) {
            Ok(listener) => {
                outln!("Control endpoint listening on {}", listener.path());
                let config = Arc::clone(&config);
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                control_task = Some((
                    shutdown_tx,
                    tokio::spawn(listener.serve(
                        config,
                        started,
                        Arc::clone(&draining),
                        access_log.clone(),
                        stop_tx.clone(),
                        shutdown_rx,
                    )),
                ));
            }
            Err(error) => return Err(stop_workers_after(&config, error).await),
        }
    }
    let drain_flag = Arc::clone(&draining);
    let shutdown = async move {
        // The sender is held here for the lifetime of the wait. Dropping it
        // would close the channel, `recv` would return `None` at once, and a
        // server with no control endpoint would shut down the moment it
        // started serving.
        let _stop_tx = stop_tx;
        tokio::select! {
            () = console_shutdown => {}
            _ = stop_rx.recv() => outln!("Control endpoint requested shutdown"),
        }
        // Set before the graceful drain starts, so the first `status` or
        // `/ready` that can observe a drain already reports it.
        drain_flag.store(true, std::sync::atomic::Ordering::Release);
    };
    let mut reload_task = reload.map(tokio::spawn);
    // Printed once at startup, beside the other readiness lines. A refused
    // request is only explainable if the operator can see what the ceiling was
    // set to, and a configured limit is not something a running server should
    // have to be asked for.
    match config.http_limits().max_body_memory() {
        0 => outln!("Request body memory budget disabled (server.max_body_memory = 0)"),
        limit => outln!("Request body memory budget {limit} bytes"),
    }
    for listener in &listeners {
        let address = listener.address();
        match &listener.tls {
            None => outln!("Hamster Wheel listening on http://{address}"),
            Some(tls) => {
                outln!(
                    "Hamster Wheel listening on https://{address} ({} certificate(s), TLS 1.2/1.3, ALPN http/1.1, handshake timeout {} s)",
                    tls.certificates().len(),
                    tls.handshake_timeout().as_secs()
                );
                // Report the current generation at startup and after reload.
                for certificate in tls.certificates().iter() {
                    outln!(
                        "TLS certificate {} for {}: valid until {} ({} days)",
                        certificate.field,
                        certificate.names,
                        certificate.not_after,
                        certificate.days_left
                    );
                    if let Some(note) = certificate.note {
                        outln!("TLS certificate {}: note: {note}", certificate.field);
                    }
                }
            }
        }
    }

    let router = config.router_arc();
    // Started after the application listeners are bound and reported, and
    // stopped after they drain: a load balancer must be able to read `/ready`
    // for the whole of a graceful stop.
    let (admin_shutdown, admin_task) = match admin_listener {
        None => (None, None),
        Some((settings, listener)) => {
            outln!(
                "Admin listener on http://{} serving /health, /ready and /metrics",
                settings.listen()
            );
            if settings.is_public() {
                errln!(
                    "Admin listener {} is not a loopback address: health, readiness and metrics are reachable from the network, with no TLS and no authentication",
                    settings.listen()
                );
            }
            let state = Arc::new(AdminState {
                config: Arc::clone(&config),
                started: admin_started(),
                draining: Arc::clone(&draining),
                access_log: access_log.clone(),
                metrics: metrics
                    .clone()
                    .expect("counters exist with an admin listener"),
            });
            let (stop_tx, stop_rx) = watch::channel(false);
            (
                Some(stop_tx),
                Some(tokio::spawn(crate::admin::serve(listener, state, stop_rx))),
            )
        }
    };

    let serve_result = serve_listeners(
        listeners,
        router,
        config.http_limits(),
        access_log.clone(),
        metrics.clone(),
        shutdown,
    )
    .await;
    if let Some(stop_tx) = admin_shutdown {
        let _ = stop_tx.send(true);
    }
    if let Some(task) = admin_task {
        let _ = task.await;
    }
    if let Some(task) = reload_task.take() {
        task.abort();
        let _ = task.await;
    }
    // Accepting another control request after the listeners have stopped would
    // answer for a server that is already going away.
    if let Some((shutdown_tx, task)) = control_task.take() {
        let _ = shutdown_tx.send(());
        let _ = task.await;
    }
    // Every connection task has ended, so no further record can be produced.
    // Dropping the last sink closes the queue, which is what ends the writer;
    // waiting for it is what keeps the last requests served out of a lost
    // tail.
    drop(access_log);
    if let Some(writer) = log_writer {
        writer.finish().await;
    }
    // Every downstream connection, and therefore every active proxy exchange,
    // has ended. Close what remains idle instead of leaving it to process exit.
    for upstream in config.upstreams() {
        upstream.close_idle();
    }
    let shutdown_result = config.shutdown_workers().await;
    match (serve_result, shutdown_result) {
        (Ok(()), Ok(())) => {
            outln!("Hamster Wheel stopped");
            Ok(())
        }
        (Err(error), Ok(())) => Err(ServerError::Io(error)),
        (Ok(()), Err(error)) => Err(ServerError::Worker(error)),
        (Err(source), Err(cleanup)) => Err(ServerError::IoWithWorkerCleanup { source, cleanup }),
    }
}

async fn stop_workers_after(config: &RuntimeConfig, error: io::Error) -> ServerError {
    match config.shutdown_workers().await {
        Ok(()) => ServerError::Io(error),
        Err(cleanup) => ServerError::IoWithWorkerCleanup {
            source: error,
            cleanup,
        },
    }
}

/// A bound socket and what the server does with a connection it accepts:
/// serve HTTP on it directly, or first complete a TLS handshake.
pub struct Listener {
    socket: TcpListener,
    tls: Option<Arc<TlsListener>>,
}

impl Listener {
    pub const fn cleartext(socket: TcpListener) -> Self {
        Self { socket, tls: None }
    }

    pub const fn tls(socket: TcpListener, tls: Arc<TlsListener>) -> Self {
        Self {
            socket,
            tls: Some(tls),
        }
    }

    fn address(&self) -> String {
        self.socket.local_addr().map_or_else(
            |error| format!("<unknown: {error}>"),
            |address| address.to_string(),
        )
    }
}

/// Keeps the active-connection gauge honest however a task ends.
struct ConnectionGuard(Arc<ServerMetrics>);

impl ConnectionGuard {
    fn new(metrics: Arc<ServerMetrics>) -> Self {
        metrics.connection_opened();
        Self(metrics)
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connection_closed();
    }
}

fn admin_started() -> std::time::Instant {
    std::time::Instant::now()
}

fn admin_bind_error(settings: AdminSettings, error: io::Error) -> ServerError {
    ServerError::Io(io::Error::new(
        error.kind(),
        format!(
            "cannot bind the admin listener {}: {error}",
            settings.listen()
        ),
    ))
}

/// What every connection served by one accept loop shares.
///
/// These three are cloned per connection and never change for its lifetime.
/// Carrying them as one value keeps the serving functions' signatures about
/// the connection rather than about the server's wiring.
#[derive(Clone)]
struct ServeContext {
    router: Arc<HostRouter>,
    http_limits: HttpLimits,
    access_log: Option<Arc<AccessLog>>,
    metrics: Option<Arc<ServerMetrics>>,
}

/// Serves one cleartext listener. Kept as the entry point every existing
/// integration test and probe uses.
pub async fn serve_with_router<F>(
    listener: TcpListener,
    router: Arc<HostRouter>,
    http_limits: HttpLimits,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    serve_listeners(
        vec![Listener::cleartext(listener)],
        router,
        http_limits,
        None,
        None,
        shutdown,
    )
    .await
}

/// Serves every listener from one accept loop.
///
/// All listeners share one `max_connections` admission bound and one
/// `JoinSet`: a connection still in its TLS handshake holds a slot exactly as
/// an HTTP connection does, which is what makes the handshake deadline the
/// bound on how long a silent peer can occupy one.
pub async fn serve_listeners<F>(
    listeners: Vec<Listener>,
    router: Arc<HostRouter>,
    http_limits: HttpLimits,
    access_log: Option<Arc<AccessLog>>,
    metrics: Option<Arc<ServerMetrics>>,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);

    let context = ServeContext {
        router,
        http_limits,
        access_log,
        metrics,
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut connections = JoinSet::new();
    let mut next_listener = 0;

    loop {
        tokio::select! {
            accept_result = accept_any(&listeners, &mut next_listener), if connections.len() < http_limits.max_connections() => {
                let (index, stream, peer_addr) = accept_result?;
                let accepted_at = Instant::now();
                let local_addr = match stream.local_addr() {
                    Ok(local_addr) => local_addr,
                    Err(error) => {
                        errln!("failed to identify accepted connection: {error}");
                        continue;
                    }
                };
                let transport = TransportInfo::new(peer_addr, local_addr);
                let tls = listeners[index].tls.clone();
                let shutdown_rx = shutdown_rx.clone();
                let context = context.clone();

                connections.spawn(async move {
                    serve_connection(stream, transport, tls, accepted_at, context, shutdown_rx)
                        .await;
                });
            }
            join_result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = join_result {
                    errln!("connection task failed: {error}");
                }
            }
            () = &mut shutdown => break,
        }
    }

    drop(listeners);
    let _ = shutdown_tx.send(true);

    while let Some(join_result) = connections.join_next().await {
        if let Err(error) = join_result {
            errln!("connection task failed during shutdown: {error}");
        }
    }

    outln!("HTTP listeners drained");
    Ok(())
}

/// Accepts from whichever listener is ready. The first listener polled
/// rotates, so a flood on one cannot starve the other.
async fn accept_any(
    listeners: &[Listener],
    next: &mut usize,
) -> io::Result<(usize, TcpStream, SocketAddr)> {
    poll_fn(|cx| {
        let count = listeners.len();
        for offset in 0..count {
            let index = (*next + offset) % count;
            if let Poll::Ready(result) = listeners[index].socket.poll_accept(cx) {
                *next = (index + 1) % count;
                return Poll::Ready(result.map(|(stream, peer)| (index, stream, peer)));
            }
        }
        Poll::Pending
    })
    .await
}

async fn serve_connection(
    stream: TcpStream,
    transport: TransportInfo,
    tls: Option<Arc<TlsListener>>,
    accepted_at: Instant,
    context: ServeContext,
    shutdown: watch::Receiver<bool>,
) {
    let http_limits = context.http_limits;
    // Counted here rather than in the accept loop so the gauge follows the
    // task that actually holds the connection, including a TLS handshake that
    // never becomes a request.
    let _connection = context.metrics.clone().map(ConnectionGuard::new);
    // The write deadline wraps the socket, beneath TLS. It must observe bytes
    // the kernel accepted, not plaintext rustls buffered: above TLS it would
    // count buffering as progress, and a flush or close_notify stalled on a
    // peer that stopped reading would never time out.
    //
    // Lingering close sits beneath both: it acts only when the connection is
    // shut down with request bytes still unread (`src/lingering_close.rs`).
    let lingering = LingeringClose::new(stream, http_limits.body_read_timeout());
    let close_signals = lingering.signals();
    let stream = WriteProgressTimeout::new(lingering, http_limits.write_progress_timeout());
    let Some(tls) = tls else {
        let connection = ConnectionInfo::new(transport, Scheme::Http);
        serve_http(stream, connection, None, context, close_signals, shutdown).await;
        return;
    };
    // Boxed so that a cleartext connection's task is not sized for the TLS
    // handshake and stream state it never uses: inline, that state machine
    // measured about 4 KiB more per idle cleartext connection.
    Box::pin(serve_tls(
        stream,
        transport,
        tls,
        accepted_at,
        context,
        close_signals,
        shutdown,
    ))
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn serve_tls(
    stream: WriteProgressTimeout<LingeringClose<TcpStream>>,
    transport: TransportInfo,
    tls: Arc<TlsListener>,
    accepted_at: Instant,
    context: ServeContext,
    close_signals: Arc<CloseSignals>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Total, from accept: no HTTP deadline applies before HTTP exists, and a
    // shutdown cancels the handshake at once because there is no request to
    // drain. Either way the socket and rustls state drop with this future.
    let deadline = accepted_at + tls.handshake_timeout();
    let accepted = tokio::select! {
        accepted = tls.handshake(stream, deadline) => accepted,
        _ = shutdown.wait_for(|stop| *stop) => None,
    };
    if let Some((stream, scope)) = accepted {
        let connection = ConnectionInfo::new(transport, Scheme::Https);
        serve_http(
            stream,
            connection,
            Some(scope),
            context,
            close_signals,
            shutdown,
        )
        .await;
    }
}

/// The one HTTP serving path. Cleartext and TLS converge here. Handlers know
/// which through the trusted scheme only; the router additionally receives the
/// presented certificate's scope, which limits the hosts a TLS connection may
/// be served for.
async fn serve_http<I>(
    io: I,
    connection_info: ConnectionInfo,
    scope: Option<Arc<CertificateScope>>,
    context: ServeContext,
    close_signals: Arc<CloseSignals>,
    mut shutdown: watch::Receiver<bool>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ServeContext {
        router,
        http_limits,
        access_log,
        metrics,
    } = context;
    let transport = connection_info.transport();
    let request_signals = Arc::clone(&close_signals);
    let service =
        hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
            // Whether this request may leave body bytes unread when the
            // connection closes; see `LingeringClose`.
            request_signals.request_declared_body(!request.body().is_end_stream());
            let router = Arc::clone(&router);
            let scope = scope.clone();
            // Taken before the handler runs so the record measures the whole
            // request, and so a handler cannot change what was asked for.
            let started = std::time::Instant::now();
            let start = access_log
                .as_ref()
                .map(|log| log.begin(&request, connection_info));
            let access_log = access_log.clone();
            let metrics = metrics.clone();
            async move {
                let served: Result<_, std::convert::Infallible> = router
                    .serve(
                        request,
                        connection_info,
                        http_limits.body_read_timeout(),
                        scope.as_deref(),
                    )
                    .await;
                served.map(|response| {
                    MaybeObserved::wrap(
                        started,
                        access_log.as_ref(),
                        start,
                        metrics.as_ref(),
                        response,
                    )
                })
            }
        });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(http_limits.header_read_timeout())
        .max_buf_size(http_limits.http1_max_buffer_size());
    let connection = builder.serve_connection(TokioIo::new(io), service);
    tokio::pin!(connection);

    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result && should_report_connection_error(&error) {
                errln!(
                    "failed to serve connection from {} to {}: {error}",
                    transport.peer_addr(),
                    transport.local_addr()
                );
            }
        }
        // `wait_for` also sees a stop signalled before this connection began
        // serving, for example while its TLS handshake was completing.
        stopped = async { shutdown.wait_for(|stop| *stop).await.is_ok() } => {
            if stopped {
                close_signals.server_stopping();
                connection.as_mut().graceful_shutdown();
                // Graceful shutdown stops accepting another request but a peer
                // that no longer reads a streaming response can otherwise hold
                // this connection task, its file handle, and process shutdown
                // forever. The ordinary header deadline is the connection's
                // existing maximum idle lifetime; use the same finite interval
                // to drain, then drop the connection and response body. Dropping
                // an HTTP future still cannot cancel or replay an owned worker
                // transaction; the pool's cancellation contract remains the
                // authority for application work.
                if let Ok(Err(error)) = timeout(
                    http_limits.header_read_timeout(),
                    connection.as_mut(),
                )
                .await
                    && should_report_connection_error(&error)
                {
                    errln!(
                        "failed to gracefully close connection from {} to {}: {error}",
                        transport.peer_addr(),
                        transport.local_addr()
                    );
                }
            }
        }
    }
}

pub(crate) fn should_report_connection_error(error: &hyper::Error) -> bool {
    // These are controlled by the remote peer and are expected admission
    // outcomes. Synchronously logging every one would turn malformed traffic
    // into a stderr backpressure path.
    !(error.is_parse()
        || error.is_timeout()
        || error_chain_has_io_kind(error, io::ErrorKind::TimedOut)
        || error.is_incomplete_message()
        || error.is_closed()
        || error.is_canceled()
        // TLS only: a peer that closes without close_notify, which browsers
        // routinely do on idle keep-alive connections, and a malformed or
        // undecryptable record. Both are peer-controlled.
        || error_chain_has_io_kind(error, io::ErrorKind::UnexpectedEof)
        || error_chain_has_io_kind(error, io::ErrorKind::InvalidData)
        // Ending a connection whose peer is already gone. Over TLS, Hyper's
        // shutdown writes close_notify, so a client that resets an idle
        // keep-alive connection surfaces here; cleartext has no such write.
        || (error.is_shutdown() && peer_is_gone(error)))
}

fn peer_is_gone(error: &hyper::Error) -> bool {
    [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::NotConnected,
    ]
    .into_iter()
    .any(|kind| error_chain_has_io_kind(error, kind))
}

pub(crate) fn error_chain_has_io_kind(
    error: &(dyn std::error::Error + 'static),
    kind: io::ErrorKind,
) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == kind)
        {
            return true;
        }
        current = error.source();
    }
    false
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}

#[cfg(windows)]
fn certificate_reload_signal(tls: Arc<TlsListener>) -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::windows::ctrl_break()?;
    Ok(async move {
        while signal.recv().await.is_some() {
            report_certificate_reload(&tls).await;
        }
    })
}

#[cfg(not(windows))]
fn certificate_reload_signal(tls: Arc<TlsListener>) -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    Ok(async move {
        while signal.recv().await.is_some() {
            report_certificate_reload(&tls).await;
        }
    })
}

async fn report_certificate_reload(tls: &TlsListener) {
    match tls.reload_certificates().await {
        Ok(reports) => {
            outln!(
                "TLS certificate reload succeeded ({} certificate(s))",
                reports.len()
            );
            for certificate in reports.iter() {
                outln!(
                    "TLS certificate {} for {}: valid until {} ({} days)",
                    certificate.field,
                    certificate.names,
                    certificate.not_after,
                    certificate.days_left
                );
                if let Some(note) = certificate.note {
                    outln!("TLS certificate {}: note: {note}", certificate.field);
                }
            }
        }
        Err(error) => {
            errln!("TLS certificate reload failed; previous set remains active: {error}")
        }
    }
}

#[cfg(not(windows))]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}

#[cfg(test)]
mod tls_tests;

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, path::Path, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::oneshot,
    };

    use super::*;
    use crate::body_budget::BodyBudget;
    use crate::router::{Handler, HostRouter, LocationRouter, VirtualHost};

    #[cfg(windows)]
    struct ContainmentFailureReset;

    #[cfg(windows)]
    impl Drop for ContainmentFailureReset {
        fn drop(&mut self) {
            crate::windows_child_containment::force_setup_failure(false);
        }
    }

    #[cfg(windows)]
    fn toml_literal(path: &Path) -> String {
        format!("'{}'", path.display())
    }

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

    fn short_file_router(path: Arc<std::path::PathBuf>, declared_length: u64) -> HostRouter {
        let locations = Arc::new(
            LocationRouter::new(
                vec![("/broken", Handler::short_file(path, declared_length))],
                Handler::transport_echo(),
            )
            .expect("valid locations"),
        );
        HostRouter::new(vec![
            VirtualHost::new("127.0.0.1", locations).expect("valid host"),
        ])
        .expect("valid router")
    }

    fn static_router(document_root: Arc<std::path::PathBuf>) -> HostRouter {
        let locations = Arc::new(
            LocationRouter::new(Vec::new(), Handler::static_files(document_root))
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

    #[cfg(windows)]
    #[tokio::test]
    async fn php_containment_failure_precedes_worker_spawn_and_listener_bind() {
        // Outside the dynamic range, so no concurrently running test can be
        // given this port and make the final probe connect.
        let address = crate::test_ports::unused_outside_dynamic_range();
        let executable = std::env::current_exe().expect("test executable path");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
        let source = format!(
            "[server]\nlisten = \"{address}\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\nqueue_capacity = 1\n",
            toml_literal(&executable),
            toml_literal(&script),
        );
        let runtime = Arc::new(
            crate::config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
                .expect("test PHP config should compile"),
        );

        crate::windows_child_containment::force_setup_failure(true);
        let _reset = ContainmentFailureReset;
        let error = run(Arc::clone(&runtime))
            .await
            .expect_err("containment failure must fail server startup");

        assert!(matches!(error, ServerError::ChildContainment(_)));
        assert!(
            error
                .to_string()
                .contains("injected containment setup failure"),
            "{error}"
        );
        assert!(runtime.php_workers()[0].pids().is_empty());
        assert!(
            TcpStream::connect(address).await.is_err(),
            "listener became available after containment setup failed"
        );
    }

    /// Both listeners are bound before either serves: when the HTTPS address
    /// is taken, the cleartext socket that did bind is released again and
    /// `run` fails instead of serving on one listener.
    #[tokio::test]
    async fn a_tls_bind_failure_leaves_no_listener_serving() {
        let pki = crate::tls::test_support::Pki::new("bind-failure");
        let leaf = pki.leaf("localhost", &["localhost"]);
        let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("occupy");
        let taken = occupied.local_addr().expect("occupied address");
        let cleartext = crate::test_ports::unused_outside_dynamic_range();
        let literal = |path: &Path| format!("'{}'", path.display());
        let source = format!(
            "[server]\nlisten = \"{cleartext}\"\n\n[server.tls]\nlisten = \"{taken}\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\n\n[[hosts]]\nname = \"localhost\"\nlocations = [{{ path = \"/\", handler = \"not_found\" }}]\n",
            literal(&leaf.cert),
            literal(&leaf.key),
        );
        let runtime = Arc::new(
            crate::config::compile_str(&source, Path::new(env!("CARGO_MANIFEST_DIR")))
                .expect("the configuration itself is valid"),
        );

        let error = tokio::time::timeout(Duration::from_secs(5), run(runtime))
            .await
            .expect("run returns instead of serving")
            .expect_err("an HTTPS address in use must fail startup");
        assert!(
            matches!(&error, ServerError::Io(io) if io.kind() == io::ErrorKind::AddrInUse),
            "{error}"
        );
        assert!(
            TcpStream::connect(cleartext).await.is_err(),
            "the cleartext listener must not be left serving"
        );
        drop(occupied);
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
        let expected = format!("peer={peer_addr};local={local_addr};scheme=http");

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
                    format!("peer={peer_addr};local={local_addr};scheme=http")
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

    #[tokio::test]
    async fn midstream_response_error_closes_connection_and_server_recovers() {
        let fixture = std::env::temp_dir().join(format!(
            "hamster-wheel-short-response-{}",
            std::process::id()
        ));
        std::fs::write(&fixture, b"short").expect("write short response fixture");

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(short_file_router(Arc::new(fixture.clone()), 10)),
            HttpLimits::default(),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        let mut client = connect(listen_addr).await;
        client
            .write_all(
                b"GET /broken HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n\
                  GET /healthy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write broken and pipelined requests");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .expect("body error must close the connection")
            .expect("read partial response");
        // What is guaranteed is that the corrupt response never looks complete,
        // not how much of it arrives. Hyper 1.11 buffers body chunks and, when
        // the body fails inside that loop, drops the connection without
        // flushing them; a pipelined request left unread can also turn the
        // close into a reset. So the head and the five bytes may or may not
        // arrive: 1 in 600 stressed runs lost them before TLS, 2 in 600 after
        // (`doc/TLS_V1.md`, Verification). Asserting their delivery made this
        // test fail intermittently.
        let response_text = String::from_utf8_lossy(&response);
        if !response.is_empty() {
            assert!(
                response_text.starts_with("HTTP/1.1 200 OK\r\n"),
                "{} bytes: {response_text:?}",
                response.len()
            );
            assert!(response_text.contains("content-length: 10\r\n"));
            let head_end = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map_or(response.len(), |index| index + 4);
            let body = &response[head_end..];
            assert!(
                b"short".starts_with(body),
                "only a prefix of the file may arrive, got {body:?}"
            );
        }
        assert!(
            response_text.matches("HTTP/1.1 200 OK").count() <= 1,
            "the pipelined request must not run after a corrupt response body"
        );

        let mut recovered = connect(listen_addr).await;
        recovered
            .write_all(b"GET /healthy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .expect("write recovery request");
        assert!(
            read_response_body(&mut recovered)
                .await
                .starts_with("peer=")
        );

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
        std::fs::remove_file(fixture).expect("remove short response fixture");
    }

    #[tokio::test]
    async fn stalled_response_write_releases_connection_admission() {
        let root = std::env::temp_dir().join(format!(
            "hamster-wheel-write-progress-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("create write-progress root");
        let large = root.join("large.bin");
        let fixture = std::fs::File::create(&large).expect("create large response fixture");
        fixture
            .set_len(32 * 1_024 * 1_024)
            .expect("size large response fixture");
        drop(fixture);
        std::fs::write(root.join("healthy.txt"), b"healthy").expect("write healthy fixture");
        let canonical_root = Arc::new(
            tokio::fs::canonicalize(&root)
                .await
                .expect("canonicalize write-progress root"),
        );

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let limits =
            HttpLimits::for_test(1, Duration::from_secs(5), Duration::from_secs(1), 8 * 1_024)
                .with_write_progress_timeout(Duration::from_millis(200));
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(static_router(canonical_root)),
            limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        let mut stalled = connect(listen_addr).await;
        stalled
            .write_all(b"GET /large.bin HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("request large static file");
        let head = read_response_head(&mut stalled).await;
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));

        // max_connections=1 means this connection cannot be accepted and
        // served until the stalled writer releases its admission slot.
        let mut waiting = connect(listen_addr).await;
        waiting
            .write_all(b"GET /healthy.txt HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .expect("queue healthy connection in the listen backlog");
        let body = tokio::time::timeout(Duration::from_secs(3), read_response_body(&mut waiting))
            .await
            .expect("write-progress timeout must release connection admission");
        assert_eq!(body, "healthy");

        let mut partial = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stalled.read_to_end(&mut partial))
            .await
            .expect("timed-out partial response must close")
            .expect("drain partial response");
        assert!(
            partial.len() < 32 * 1_024 * 1_024,
            "a timed-out response must not silently become a complete response"
        );

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
        std::fs::remove_file(root.join("healthy.txt")).expect("remove healthy fixture");
        std::fs::remove_file(&large).expect("timed-out stream released its file handle");
        std::fs::remove_dir(&root).expect("remove write-progress root");
    }

    #[tokio::test]
    async fn shutdown_drops_a_static_stream_stalled_on_the_client() {
        let root = std::env::temp_dir().join(format!(
            "hamster-wheel-streaming-shutdown-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("create static stream root");
        let large = root.join("large.bin");
        let fixture = std::fs::File::create(&large).expect("create large static fixture");
        fixture
            .set_len(32 * 1_024 * 1_024)
            .expect("size large static fixture");
        drop(fixture);
        let canonical_root = Arc::new(
            tokio::fs::canonicalize(&root)
                .await
                .expect("canonicalize static stream root"),
        );

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind listener");
        let listen_addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let limits = HttpLimits::for_test(
            1,
            Duration::from_millis(300),
            Duration::from_secs(1),
            8 * 1_024,
        );
        let server = tokio::spawn(serve_with_router(
            listener,
            Arc::new(static_router(canonical_root)),
            limits,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        let mut client = connect(listen_addr).await;
        client
            .write_all(b"GET /large.bin HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("request large static file");
        let head = read_response_head(&mut client).await;
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(
            head.windows(24)
                .any(|window| window == b"content-length: 33554432")
        );

        shutdown_tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stalled static stream must not hold shutdown forever")
            .expect("join server")
            .expect("server result");

        // Completion of the connection task must have dropped the FileBody and
        // its open handle before the server reports shutdown complete.
        std::fs::remove_file(&large).expect("stream file handle was released");
        std::fs::remove_dir(&root).expect("remove static stream root");
        drop(client);
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
        //
        // The client waits for `100 Continue` before sending the body, as
        // curl does for large bodies, so the refusal must come without asking
        // for it. A client that pushes the body unconditionally is refused
        // the same way, but Hyper then closes with that body unread and
        // Windows answers with a reset that can destroy the 503 before the
        // client reads it: that race, not the budget, made this test fail
        // intermittently (`doc/TLS_V1.md`, Verification).
        let mut refused = connect(listen_addr).await;
        refused
            .write_all(
                format!(
                    "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nExpect: 100-continue\r\nContent-Length: {BODY}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write request head");
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

    /// A client that sends its whole body before reading, as .NET's HttpClient
    /// does, must receive the early refusal rather than a reset. Without
    /// lingering close the server closed with the body unread and Windows
    /// answered the next segment with a reset: .NET saw "connection reset" in
    /// 10 of 10 two-megabyte uploads and never read the response.
    #[tokio::test]
    async fn an_early_refusal_reaches_a_client_that_sends_its_whole_body_first() {
        // Larger than the 16 MiB discard bound an earlier version had, which
        // turned every bigger refused upload back into a reset.
        const BODY: usize = 20 * 1_024 * 1_024;
        let budget = BodyBudget::new(64 * 1_024);
        let limits = HttpLimits::for_test(
            16,
            Duration::from_secs(10),
            Duration::from_secs(10),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) =
            start_budget_server(limits, Arc::clone(&budget), None).await;

        // Even attempts send everything at once. Odd ones send the head and a
        // little body, pause while the server refuses and closes, then send
        // the rest, as a client writing in bursts does. At that close nothing
        // is waiting on the socket, so only the request's declared body says
        // the client is still sending.
        for attempt in 0..6 {
            let mut client = connect(listen_addr).await;
            let head =
                format!("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {BODY}\r\n\r\n");
            let mut request = head.into_bytes();
            request.extend(std::iter::repeat_n(b'x', BODY));
            let pause_after = if attempt % 2 == 1 {
                1_024
            } else {
                request.len()
            };
            let send = async {
                client.write_all(&request[..pause_after]).await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                client.write_all(&request[pause_after..]).await
            };
            tokio::time::timeout(Duration::from_secs(10), send)
                .await
                .expect("the server keeps reading until the body is sent")
                .unwrap_or_else(|error| panic!("attempt {attempt}: sending the body: {error}"));
            let mut answer = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut answer))
                .await
                .expect("the refusal is followed by a close")
                .unwrap_or_else(|error| panic!("attempt {attempt}: reading the refusal: {error}"));
            // Declared over the handler's body limit: refused as the PHP
            // handler refuses a body over the protocol limit, before reading.
            assert!(
                answer.starts_with(b"HTTP/1.1 413 "),
                "attempt {attempt}: {:?}",
                String::from_utf8_lossy(&answer[..answer.len().min(120)])
            );
        }
        assert_eq!(budget.reserved(), 0);

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    /// Lingering is decided per connection. A closing connection whose last
    /// request had no body, and nothing waiting, is closed at once: a client
    /// that stays silent after the FIN cannot hold its connection slot for the
    /// linger timeout.
    #[tokio::test]
    async fn a_closing_connection_without_a_body_does_not_linger() {
        let budget = BodyBudget::new(64 * 1_024);
        let limits = HttpLimits::for_test(
            1,
            Duration::from_millis(300),
            Duration::from_secs(10),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) =
            start_budget_server(limits, Arc::clone(&budget), None).await;

        // One request without a body that asks to close, then silence: the
        // client reads to the server's FIN but never closes its own side.
        let mut idle = connect(listen_addr).await;
        idle.write_all(
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .expect("write request");
        let started = std::time::Instant::now();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), idle.read_to_end(&mut rest))
            .await
            .expect("the idle connection is closed")
            .expect("read to the close");
        // `max_connections` is 1: the slot must be free again at once, well
        // inside even the one-second linger idle bound.
        let mut next = connect(listen_addr).await;
        next.write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("write request");
        let _ = read_response_body(&mut next).await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the slot was held for {:?}",
            started.elapsed()
        );
        drop(idle);

        shutdown_tx.send(()).expect("signal shutdown");
        server.await.expect("join server").expect("server result");
    }

    /// A client refused early whose body then stops arriving is lingered on
    /// only for the idle bound. The total bound is `body_read_timeout` (ten
    /// seconds here), so it is the idle bound that frees the slot.
    #[tokio::test]
    async fn a_stalled_upload_frees_its_slot_after_the_linger_idle_bound() {
        let budget = BodyBudget::new(64 * 1_024);
        let limits = HttpLimits::for_test(
            1,
            Duration::from_secs(10),
            Duration::from_secs(10),
            8 * 1_024,
        );
        let (listen_addr, shutdown_tx, server) =
            start_budget_server(limits, Arc::clone(&budget), None).await;

        // Declared over the handler's limit, so refused at once; one body byte
        // arrives and then nothing, with the connection left open.
        let mut stalled = connect(listen_addr).await;
        stalled
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4194304\r\n\r\nx")
            .await
            .expect("write a partial body");
        let started = std::time::Instant::now();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stalled.read_to_end(&mut answer))
            .await
            .expect("the stalled upload is refused and closed")
            .expect("read the refusal");
        assert!(
            answer.starts_with(b"HTTP/1.1 413 "),
            "{:?}",
            String::from_utf8_lossy(&answer)
        );

        // `max_connections` is 1 and the stalled client never closes.
        let mut next = connect(listen_addr).await;
        next.write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("write request");
        let _ = tokio::time::timeout(Duration::from_secs(4), read_response_body(&mut next))
            .await
            .expect("the slot is released");
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(2_500),
            "the slot was held for {waited:?}; the idle bound is {:?}",
            crate::lingering_close::LINGER_IDLE
        );
        drop(stalled);

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
        // The chunk announces 128 KiB but the client stops one byte past the
        // budget, so the refusal happens with nothing left unread. Unread
        // bytes at close make Windows reset the connection, which could
        // destroy the 503 before the client reads it.
        let oversized = 128 * 1_024;
        large
            .write_all(format!("{:x}\r\n", oversized).as_bytes())
            .await
            .expect("write chunk size");
        large
            .write_all(&vec![b'z'; budget.limit() + 1])
            .await
            .expect("write one byte more than the budget");
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
