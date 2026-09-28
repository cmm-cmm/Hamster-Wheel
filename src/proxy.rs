//! Reverse proxy v1: one HTTP/1.1 upstream endpoint per `proxy` location.
//!
//! The design is deliberately concrete. A location owns one `Arc<Upstream>`;
//! the upstream owns its resolved address, its timeouts, a small stack of idle
//! connections and its counters. There is no backend trait, no registry and no
//! load balancer.
//!
//! Ownership of one exchange:
//!
//! ```text
//! downstream Incoming -> RequestBody (progress deadline) -> SendRequest
//!                                                   upstream driver task
//! upstream Incoming -> ProxyBody (progress deadline, owns Lease) -> downstream
//! ```
//!
//! Neither body is collected and neither direction has a channel of its own,
//! so both directions keep Hyper's natural backpressure. The `Lease` is the
//! only path by which a connection returns to the idle stack, and it does so
//! only after the response reached a clean end *and* the request body was
//! fully handed to Hyper. Every other way a lease ends -- error, timeout,
//! cancellation, a dropped response body -- aborts the connection's driver.
//!
//! A request is re-sent only when Hyper hands it back unsent
//! (`TrySendError::take_message`), which happens only if the dispatcher never
//! took it from its queue. That is the single safe-retry proof; there is no
//! method-based retry.

use std::{
    collections::VecDeque,
    error::Error as StdError,
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri, Version,
    body::{Body, Frame, Incoming, SizeHint},
    client::conn::http1::{self as client_http1, SendRequest},
    header::{self, HeaderName, HeaderValue},
    http::{Extensions, uri},
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::Notify,
    task::AbortHandle,
    time::{sleep, timeout},
};

use crate::{
    handler::{HandlerResponse, simple_response},
    response_body::ResponseBody,
    server::error_chain_has_io_kind,
    transport::ConnectionInfo,
    write_progress::{ProgressDeadline, WriteProgressTimeout},
};

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Below the 5 s keep-alive timeout used by several common upstream servers,
/// so Hamster normally retires an idle connection before the upstream does.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(4);
pub const DEFAULT_MAX_IDLE: usize = 64;
pub const MAX_IDLE_LIMIT: usize = 4_096;

/// Upstream failure notices are rate limited like the body budget's: a dead
/// upstream must not turn request volume into stderr volume.
const FAILURE_NOTICE_PERIOD_MS: u64 = 60_000;

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
static X_FORWARDED_PORT: HeaderName = HeaderName::from_static("x-forwarded-port");
static X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
static KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
static PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

/// Compiled, immutable configuration of one upstream.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// `<host><path>` of the owning location, used in every diagnostic.
    pub label: Arc<str>,
    /// Resolved once at startup. There is no re-resolution.
    pub address: SocketAddr,
    /// `host[:port]` as configured; the Host sent when `preserve_host` is off.
    pub authority: HeaderValue,
    pub connect_timeout: Duration,
    pub response_header_timeout: Duration,
    pub progress_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_idle: usize,
    pub preserve_host: bool,
    /// Hyper's per-connection read/write buffer bound for upstream connections.
    pub max_buf_size: usize,
}

/// Counters for one upstream. Reading them never touches the request path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpstreamSnapshot {
    /// Exchanges from admission until their lease ends.
    pub active_exchanges: usize,
    pub idle_connections: usize,
    pub connections_opened: u64,
    /// Successful idle checkouts; an unsent failure after checkout still counts.
    pub connections_reused: u64,
    /// Requests re-sent because Hyper proved they were never started.
    pub safe_retries: u64,
    /// Refused, reset or timed-out connection attempts.
    pub connect_failures: u64,
    /// Connect, response-head and progress timeouts attributable to the upstream.
    pub upstream_timeouts: u64,
    /// Invalid or missing upstream responses, before or after commit.
    pub upstream_failures: u64,
}

#[derive(Default)]
struct Counters {
    active_exchanges: AtomicUsize,
    connections_opened: AtomicU64,
    connections_reused: AtomicU64,
    safe_retries: AtomicU64,
    connect_failures: AtomicU64,
    upstream_timeouts: AtomicU64,
    upstream_failures: AtomicU64,
}

pub struct Upstream {
    config: ProxyConfig,
    /// Oldest at the front, newest at the back. Checkout takes the newest, so
    /// if the newest has expired every older entry has as well.
    idle: Mutex<VecDeque<IdleConnection>>,
    counters: Counters,
    started: std::time::Instant,
    next_notice_ms: AtomicU64,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Upstream")
            .field("label", &self.config.label)
            .field("address", &self.config.address)
            .finish_non_exhaustive()
    }
}

impl Upstream {
    pub fn new(config: ProxyConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            idle: Mutex::new(VecDeque::new()),
            counters: Counters::default(),
            started: std::time::Instant::now(),
            next_notice_ms: AtomicU64::new(0),
            #[cfg(test)]
            faults: Arc::default(),
        })
    }

    pub fn label(&self) -> &str {
        &self.config.label
    }

    pub const fn address(&self) -> SocketAddr {
        self.config.address
    }

    pub fn snapshot(&self) -> UpstreamSnapshot {
        let counters = &self.counters;
        UpstreamSnapshot {
            active_exchanges: counters.active_exchanges.load(Ordering::Acquire),
            idle_connections: self.lock_idle().len(),
            connections_opened: counters.connections_opened.load(Ordering::Relaxed),
            connections_reused: counters.connections_reused.load(Ordering::Relaxed),
            safe_retries: counters.safe_retries.load(Ordering::Relaxed),
            connect_failures: counters.connect_failures.load(Ordering::Relaxed),
            upstream_timeouts: counters.upstream_timeouts.load(Ordering::Relaxed),
            upstream_failures: counters.upstream_failures.load(Ordering::Relaxed),
        }
    }

    /// Closes every idle connection. Active exchanges are untouched; they end
    /// with the downstream connections that own them.
    pub fn close_idle(&self) {
        let idle = std::mem::take(&mut *self.lock_idle());
        drop(idle);
    }

    fn lock_idle(&self) -> std::sync::MutexGuard<'_, VecDeque<IdleConnection>> {
        // A panic while holding this lock cannot leave the deque logically
        // inconsistent: every critical section is a single push or pop.
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the newest idle connection that is still usable.
    ///
    /// No network I/O happens under the lock. `ready()` is awaited outside it:
    /// a connection is only checked in after its exchange ended cleanly, so its
    /// driver reaches the idle state promptly, and a driver that cannot is
    /// bounded by the upstream write-progress timeout.
    async fn check_out(&self) -> Option<Connection> {
        loop {
            let entry = {
                let mut idle = self.lock_idle();
                let entry = idle.pop_back()?;
                if entry.since.elapsed() >= self.config.idle_timeout {
                    let expired = std::mem::take(&mut *idle);
                    drop(idle);
                    drop(expired);
                    drop(entry);
                    return None;
                }
                entry
            };
            let mut connection = entry.connection;
            if connection.sender.is_closed() {
                continue;
            }
            if connection.sender.ready().await.is_ok() {
                return Some(connection);
            }
        }
    }

    fn check_in(&self, connection: Connection) {
        if self.config.max_idle == 0 || connection.sender.is_closed() {
            return;
        }
        let evicted = {
            let mut idle = self.lock_idle();
            let evicted = if idle.len() >= self.config.max_idle {
                idle.pop_front()
            } else {
                None
            };
            idle.push_back(IdleConnection {
                connection,
                since: std::time::Instant::now(),
            });
            evicted
        };
        drop(evicted);
    }

    async fn connect(&self) -> Result<Connection, ConnectFailure> {
        let connect = async {
            #[cfg(test)]
            if let Some(delay) = self.faults.connect_delay() {
                sleep(delay).await;
            }
            TcpStream::connect(self.config.address).await
        };
        let stream = match timeout(self.config.connect_timeout, connect).await {
            Err(_) => return Err(ConnectFailure::TimedOut),
            Ok(Err(error)) => return Err(ConnectFailure::Io(error)),
            Ok(Ok(stream)) => stream,
        };
        // A request head and its first body frame are separate writes. Nagle
        // plus delayed ACK would hold the second one for a delayed-ACK period.
        let _ = stream.set_nodelay(true);
        let stream = UpstreamStream {
            inner: WriteProgressTimeout::new(stream, self.config.progress_timeout),
            #[cfg(test)]
            faults: Arc::clone(&self.faults),
        };
        let (sender, driver) = client_http1::Builder::new()
            .max_buf_size(self.config.max_buf_size)
            .handshake::<_, RequestBody>(TokioIo::new(stream))
            .await
            .map_err(ConnectFailure::Handshake)?;
        let driver = tokio::spawn(async move {
            // Errors surface to the exchange through the response future or
            // the response body; the driver itself has no one else to tell.
            let _ = driver.await;
        })
        .abort_handle();
        self.counters
            .connections_opened
            .fetch_add(1, Ordering::Relaxed);
        Ok(Connection { sender, driver })
    }

    fn record_failure(&self, counter: &AtomicU64, what: fmt::Arguments<'_>) {
        let total = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.claim_notice(elapsed_ms) {
            return;
        }
        errln!(
            "proxy upstream {} ({}): {what} ({total} of this kind since start; reported at most once per {} s)",
            self.config.label,
            self.config.address,
            FAILURE_NOTICE_PERIOD_MS / 1_000
        );
    }

    fn claim_notice(&self, now_ms: u64) -> bool {
        let due = self.next_notice_ms.load(Ordering::Relaxed);
        if due == u64::MAX || now_ms < due {
            return false;
        }
        self.next_notice_ms
            .compare_exchange(
                due,
                now_ms.saturating_add(FAILURE_NOTICE_PERIOD_MS),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

#[derive(Debug)]
enum ConnectFailure {
    TimedOut,
    Io(io::Error),
    Handshake(hyper::Error),
}

impl fmt::Display for ConnectFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("connect timed out"),
            Self::Io(error) => write!(formatter, "connect failed: {error}"),
            Self::Handshake(error) => write!(formatter, "handshake failed: {error}"),
        }
    }
}

/// One live upstream HTTP/1.1 connection. Dropping it aborts the driver, which
/// closes the socket; that is how every non-reusable exchange ends.
struct Connection {
    sender: SendRequest<RequestBody>,
    driver: AbortHandle,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

struct IdleConnection {
    connection: Connection,
    since: std::time::Instant,
}

/// The upstream socket. In test builds it can inject a read-side EOF, which is
/// how the stale keep-alive race is reproduced deterministically.
struct UpstreamStream {
    inner: WriteProgressTimeout<TcpStream>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
}

impl AsyncRead for UpstreamStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        #[cfg(test)]
        if self.faults.take_read_eof() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for UpstreamStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Ownership of one exchange, from admission to the end of the response body.
///
/// It is counted in `active_exchanges` for its whole life. Only `release`
/// can return its connection to the idle stack; dropping it any other way
/// aborts the connection.
struct Lease {
    upstream: Arc<Upstream>,
    connection: Option<Connection>,
    /// `None` when the request had no body.
    request: Option<Arc<RequestProgress>>,
}

impl Lease {
    fn new(upstream: &Arc<Upstream>) -> Self {
        upstream
            .counters
            .active_exchanges
            .fetch_add(1, Ordering::AcqRel);
        Self {
            upstream: Arc::clone(upstream),
            connection: None,
            request: None,
        }
    }

    fn request_complete(&self) -> bool {
        self.request
            .as_ref()
            .is_none_or(|progress| progress.state() == RequestState::Complete)
    }

    /// Returns the connection for reuse if, and only if, the request was fully
    /// handed to Hyper. The caller has already established a clean response end.
    fn release(mut self) {
        if let Some(connection) = self.connection.take()
            && self.request_complete()
        {
            self.upstream.check_in(connection);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        drop(self.connection.take());
        self.upstream
            .counters
            .active_exchanges
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestState {
    Streaming,
    Complete,
    /// The downstream client stopped sending for the whole progress interval.
    TimedOut,
    /// The downstream body failed: the client aborted or sent invalid framing.
    Failed,
}

/// Request-body state shared between the upstream driver, which polls the
/// body, and the exchange, which arms the response-head timer on completion
/// and decides reuse.
struct RequestProgress {
    state: AtomicU8,
    complete: Notify,
}

impl RequestProgress {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(RequestState::Streaming as u8),
            complete: Notify::new(),
        }
    }

    fn state(&self) -> RequestState {
        match self.state.load(Ordering::Acquire) {
            0 => RequestState::Streaming,
            1 => RequestState::Complete,
            2 => RequestState::TimedOut,
            _ => RequestState::Failed,
        }
    }

    fn finish(&self, state: RequestState) {
        if self
            .state
            .compare_exchange(
                RequestState::Streaming as u8,
                state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
            && state == RequestState::Complete
        {
            self.complete.notify_waiters();
        }
    }

    /// Resolves when the whole request body has been handed to Hyper. A body
    /// that fails never completes; the response future reports that failure.
    async fn completed(&self) {
        loop {
            let notified = self.complete.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.state() {
                RequestState::Complete => return,
                RequestState::Streaming => notified.await,
                RequestState::TimedOut | RequestState::Failed => {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

/// The body Hamster sends upstream: the downstream `Incoming`, unchanged and
/// uncopied, behind a progress deadline for the downstream client.
enum RequestBody {
    Empty,
    Stream {
        inner: Incoming,
        deadline: ProgressDeadline,
        progress: Arc<RequestProgress>,
    },
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let Self::Stream {
            inner,
            deadline,
            progress,
        } = self.get_mut()
        else {
            return Poll::Ready(None);
        };
        // Hyper only polls this while it can write upstream, so `Pending` here
        // means the downstream client has not sent anything. A stalled upstream
        // is bounded by the upstream socket's write-progress timeout instead.
        match Pin::new(&mut *inner).poll_frame(cx) {
            Poll::Pending => match deadline.poll_stalled(cx) {
                Poll::Ready(()) => {
                    progress.finish(RequestState::TimedOut);
                    Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "request body made no progress for {:.3} seconds",
                            deadline.timeout().as_secs_f64()
                        ),
                    ))))
                }
                Poll::Pending => Poll::Pending,
            },
            Poll::Ready(Some(Ok(frame))) => {
                deadline.progress();
                // Hyper stops polling once `is_end_stream` is true, so the end
                // must be recorded with the frame that reaches it.
                if inner.is_end_stream() {
                    progress.finish(RequestState::Complete);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                progress.finish(RequestState::Failed);
                Poll::Ready(Some(Err(io::Error::other(error))))
            }
            Poll::Ready(None) => {
                progress.finish(RequestState::Complete);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Stream { inner, .. } => inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Empty => SizeHint::with_exact(0),
            Self::Stream { inner, .. } => inner.size_hint(),
        }
    }
}

/// The downstream response body: the upstream `Incoming`, unchanged and
/// uncopied, behind a progress deadline for the upstream. It owns the lease.
pub(crate) struct ProxyBody {
    inner: Incoming,
    deadline: ProgressDeadline,
    lease: Option<Lease>,
    /// False when the response framing forbids reusing the connection.
    reusable: bool,
    finished: bool,
}

impl fmt::Debug for ProxyBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyBody")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ProxyBody {
    /// The upstream response ended at a message boundary.
    fn finish_cleanly(&mut self) {
        self.finished = true;
        if let Some(lease) = self.lease.take() {
            if self.reusable {
                lease.release();
            } else {
                drop(lease);
            }
        }
    }

    /// Anything else: the connection must not be reused.
    fn fail(&mut self) {
        self.finished = true;
        drop(self.lease.take());
    }

    fn upstream(&self) -> Option<&Arc<Upstream>> {
        self.lease.as_ref().map(|lease| &lease.upstream)
    }
}

impl Body for ProxyBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        // Hyper polls this only when the downstream connection can take
        // another frame, so a slow client is never mistaken for a slow
        // upstream: the deadline is armed only by an upstream `Pending`.
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => match this.deadline.poll_stalled(cx) {
                Poll::Ready(()) => {
                    if let Some(upstream) = this.upstream() {
                        upstream.record_failure(
                            &upstream.counters.upstream_timeouts,
                            format_args!("response body made no progress"),
                        );
                    }
                    let timeout = this.deadline.timeout();
                    this.fail();
                    Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "upstream response body made no progress for {:.3} seconds",
                            timeout.as_secs_f64()
                        ),
                    ))))
                }
                Poll::Pending => Poll::Pending,
            },
            Poll::Ready(Some(Ok(frame))) => {
                this.deadline.progress();
                if this.inner.is_end_stream() {
                    this.finish_cleanly();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(upstream) = this.upstream() {
                    upstream.record_failure(
                        &upstream.counters.upstream_failures,
                        format_args!("response body failed after commit: {error}"),
                    );
                }
                this.fail();
                Poll::Ready(Some(Err(io::Error::other(error))))
            }
            Poll::Ready(None) => {
                this.finish_cleanly();
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        if self.finished {
            SizeHint::with_exact(0)
        } else {
            self.inner.size_hint()
        }
    }
}

impl Drop for ProxyBody {
    fn drop(&mut self) {
        // A bodiless response (HEAD, 204, 304) can be complete before Hyper
        // ever polls it. Anything short of a clean end aborts the connection.
        if !self.finished && self.inner.is_end_stream() {
            self.finish_cleanly();
        }
    }
}

enum HeadOutcome {
    Response(Response<Incoming>),
    Failed(hyper::client::conn::TrySendError<Request<RequestBody>>),
    TimedOut,
}

/// Proxies one request. Returns a synthetic response only before commit.
pub(crate) async fn serve(
    upstream: &Arc<Upstream>,
    request: Request<Incoming>,
    connection: ConnectionInfo,
) -> HandlerResponse {
    if request.method() == Method::CONNECT {
        return simple_response(StatusCode::NOT_IMPLEMENTED, "Not Implemented");
    }
    let mut lease = Lease::new(upstream);
    let is_head = request.method() == Method::HEAD;
    let (mut request, progress) = match rewrite_request(&upstream.config, request, connection) {
        Ok(prepared) => prepared,
        Err((status, message)) => return simple_response(status, message),
    };
    lease.request.clone_from(&progress);

    let mut connection = match upstream.check_out().await {
        Some(connection) => {
            upstream
                .counters
                .connections_reused
                .fetch_add(1, Ordering::Relaxed);
            connection
        }
        None => match upstream.connect().await {
            Ok(connection) => connection,
            Err(failure) => return connect_failure_response(upstream, &failure),
        },
    };
    let mut retried = false;
    loop {
        let response = connection.sender.try_send_request(request);
        lease.connection = Some(connection);
        match await_head(response, progress.as_deref(), upstream).await {
            HeadOutcome::Response(response) => {
                return into_downstream_response(response, lease, is_head, upstream);
            }
            HeadOutcome::TimedOut => {
                upstream.record_failure(
                    &upstream.counters.upstream_timeouts,
                    format_args!("no response head within the response header timeout"),
                );
                return simple_response(StatusCode::GATEWAY_TIMEOUT, "Upstream Response Timeout");
            }
            HeadOutcome::Failed(mut error) => {
                // The one proof that the upstream never saw this request: Hyper
                // returned it without having taken it from the queue.
                let unsent = error.take_message();
                drop(lease.connection.take());
                match unsent {
                    Some(unsent) if !retried => {
                        retried = true;
                        upstream
                            .counters
                            .safe_retries
                            .fetch_add(1, Ordering::Relaxed);
                        request = unsent;
                        connection = match upstream.connect().await {
                            Ok(connection) => connection,
                            Err(failure) => {
                                return connect_failure_response(upstream, &failure);
                            }
                        };
                    }
                    _ => {
                        return send_failure_response(upstream, &error.into_error(), &progress);
                    }
                }
            }
        }
    }
}

/// Waits for the response head. Its timer starts only once the request body
/// has been fully handed to Hyper, so an upstream may answer early -- while the
/// client is still uploading -- and a long upload is never cut by a head
/// deadline. While uploading, the progress deadlines bound both directions.
async fn await_head(
    response: impl Future<
        Output = Result<
            Response<Incoming>,
            hyper::client::conn::TrySendError<Request<RequestBody>>,
        >,
    >,
    progress: Option<&RequestProgress>,
    upstream: &Upstream,
) -> HeadOutcome {
    let head_deadline = async {
        if let Some(progress) = progress {
            progress.completed().await;
        }
        sleep(upstream.config.response_header_timeout).await;
    };
    tokio::select! {
        biased;
        result = response => match result {
            Ok(response) => HeadOutcome::Response(response),
            Err(error) => HeadOutcome::Failed(error),
        },
        () = head_deadline => HeadOutcome::TimedOut,
    }
}

fn connect_failure_response(upstream: &Upstream, failure: &ConnectFailure) -> HandlerResponse {
    upstream.record_failure(
        &upstream.counters.connect_failures,
        format_args!("{failure}"),
    );
    match failure {
        ConnectFailure::TimedOut => {
            upstream
                .counters
                .upstream_timeouts
                .fetch_add(1, Ordering::Relaxed);
            simple_response(StatusCode::GATEWAY_TIMEOUT, "Upstream Connect Timeout")
        }
        ConnectFailure::Io(_) | ConnectFailure::Handshake(_) => {
            simple_response(StatusCode::BAD_GATEWAY, "Bad Gateway")
        }
    }
}

/// Maps a failure before any response head. The request may have started, so
/// nothing here is retried.
fn send_failure_response(
    upstream: &Upstream,
    error: &hyper::Error,
    progress: &Option<Arc<RequestProgress>>,
) -> HandlerResponse {
    // The downstream side is checked first: its own timeout also surfaces as
    // an I/O `TimedOut` inside Hyper's error chain.
    match progress.as_deref().map(RequestProgress::state) {
        Some(RequestState::TimedOut) => {
            return simple_response(StatusCode::REQUEST_TIMEOUT, "Request Body Timeout");
        }
        Some(RequestState::Failed) => {
            return simple_response(StatusCode::BAD_REQUEST, "Invalid Request Body");
        }
        Some(RequestState::Streaming | RequestState::Complete) | None => {}
    }
    if error_chain_has_io_kind(error, io::ErrorKind::TimedOut) {
        upstream.record_failure(
            &upstream.counters.upstream_timeouts,
            format_args!("upstream stopped accepting the request: {error}"),
        );
        return simple_response(StatusCode::GATEWAY_TIMEOUT, "Upstream Progress Timeout");
    }
    upstream.record_failure(
        &upstream.counters.upstream_failures,
        format_args!("no valid response: {error}"),
    );
    if error.is_parse() {
        simple_response(StatusCode::BAD_GATEWAY, "Invalid Upstream Response")
    } else {
        simple_response(StatusCode::BAD_GATEWAY, "Bad Gateway")
    }
}

fn into_downstream_response(
    response: Response<Incoming>,
    lease: Lease,
    is_head: bool,
    upstream: &Upstream,
) -> HandlerResponse {
    let (mut parts, body) = response.into_parts();
    // Hyper hands a 101 to the caller as an ordinary response. Upgrade is
    // stripped from every request, so a 101 is an upstream protocol violation,
    // and its connection is no longer HTTP. Dropping the lease closes it.
    if parts.status == StatusCode::SWITCHING_PROTOCOLS {
        upstream.record_failure(
            &upstream.counters.upstream_failures,
            format_args!("unrequested 101 Switching Protocols"),
        );
        return simple_response(StatusCode::BAD_GATEWAY, "Invalid Upstream Response");
    }
    let Ok(reusable) = rewrite_response_headers(&mut parts.headers, parts.status, is_head) else {
        upstream.record_failure(
            &upstream.counters.upstream_failures,
            format_args!("unsupported transfer coding or invalid HEAD Content-Length"),
        );
        return simple_response(StatusCode::BAD_GATEWAY, "Invalid Upstream Response");
    };
    parts.version = Version::HTTP_11;
    parts.extensions = Extensions::new();
    let body = ProxyBody {
        inner: body,
        deadline: ProgressDeadline::new(upstream.config.progress_timeout),
        lease: Some(lease),
        reusable,
        finished: false,
    };
    Response::from_parts(parts, ResponseBody::proxy(body))
}

/// The request to send upstream, and its body progress when it has a body.
type UpstreamRequest = (Request<RequestBody>, Option<Arc<RequestProgress>>);

/// Builds the upstream request from the downstream one.
fn rewrite_request(
    config: &ProxyConfig,
    request: Request<Incoming>,
    connection: ConnectionInfo,
) -> Result<UpstreamRequest, (StatusCode, &'static str)> {
    let (mut parts, body) = request.into_parts();

    // Origin-form only. The router has already moved an absolute-form
    // authority into Host, so nothing is lost by dropping it here.
    let path_and_query = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| uri::PathAndQuery::from_static("/"));
    let mut uri_parts = uri::Parts::default();
    uri_parts.path_and_query = Some(path_and_query);
    parts.uri = Uri::from_parts(uri_parts)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid Request Target"))?;

    // The router validated exactly one Host and made it the effective
    // authority, so this is what selected the virtual host.
    let effective_authority = parts
        .headers
        .get(header::HOST)
        .cloned()
        .ok_or((StatusCode::BAD_REQUEST, "Missing Host"))?;
    // Hyper removes the final chunk framing, but it does not decode earlier
    // transfer codings. Stripping such a coding would silently change the
    // meaning of the bytes delivered to the upstream.
    if unsupported_transfer_coding(&parts.headers) {
        return Err((StatusCode::BAD_REQUEST, "Unsupported Transfer-Encoding"));
    }
    let declared_zero_length = parts.headers.contains_key(header::CONTENT_LENGTH);
    let wants_trailers = te_accepts_trailers(&parts.headers);

    let headers = &mut parts.headers;
    remove_hop_by_hop(headers);
    // Hyper decoded the downstream framing and re-encodes it from the body.
    headers.remove(header::CONTENT_LENGTH);
    // Hamster answers the expectation on its own hop when the body is first
    // read, and streams without waiting; forwarding it would only add an
    // interim response on the upstream hop.
    headers.remove(header::EXPECT);
    // Edge proxy: nothing a client says about earlier hops is trusted.
    headers.remove(header::FORWARDED);
    headers.remove(&X_FORWARDED_FOR);
    headers.remove(&X_FORWARDED_HOST);
    headers.remove(&X_FORWARDED_PROTO);
    headers.remove(&X_FORWARDED_PORT);
    headers.remove(&X_REAL_IP);

    let host = if config.preserve_host {
        effective_authority.clone()
    } else {
        config.authority.clone()
    };
    headers.insert(header::HOST, host);
    let peer = HeaderValue::from_str(&connection.transport().peer_addr().ip().to_string())
        .expect("an IP address is a valid header value");
    headers.insert(X_FORWARDED_FOR.clone(), peer);
    // The scheme of the client's own hop, established by the listener that
    // accepted it. The upstream hop is always cleartext and says nothing here.
    headers.insert(
        X_FORWARDED_PROTO.clone(),
        HeaderValue::from_static(connection.scheme().as_str()),
    );
    headers.insert(X_FORWARDED_HOST.clone(), effective_authority);
    // The downstream client accepts trailers and Hamster relays them, so the
    // upstream may send them. Every other TE coding is this hop's business.
    if wants_trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    parts.version = Version::HTTP_11;
    parts.extensions = Extensions::new();

    if body.is_end_stream() {
        // Hyper sends no length for an empty body. Keep an explicit zero the
        // client sent, since some servers require a length on POST.
        if declared_zero_length {
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        }
        return Ok((Request::from_parts(parts, RequestBody::Empty), None));
    }
    if body.size_hint().exact().is_none() {
        // Hyper assumes GET, HEAD and CONNECT have no body when the length is
        // unknown and would silently send none. Chunked must be explicit.
        parts.headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
    }
    let progress = Arc::new(RequestProgress::new());
    let body = RequestBody::Stream {
        inner: body,
        deadline: ProgressDeadline::new(config.progress_timeout),
        progress: Arc::clone(&progress),
    };
    Ok((Request::from_parts(parts, body), Some(progress)))
}

/// Normalizes an untrusted upstream response head for the downstream hop.
///
/// Returns whether the framing allows reusing the connection, or an error for
/// unsupported transfer coding or untrustworthy HEAD length fields.
fn rewrite_response_headers(
    headers: &mut HeaderMap,
    status: StatusCode,
    is_head: bool,
) -> Result<bool, ()> {
    // Hyper decodes chunk framing only. Do not relay a body with an earlier
    // transfer coding after removing the field that describes that coding.
    if unsupported_transfer_coding(headers) {
        return Err(());
    }
    // RFC 9112 6.3: a message with both is a smuggling signal. Hyper already
    // let Transfer-Encoding win; the connection is still not reused.
    let reusable = !(headers.contains_key(header::TRANSFER_ENCODING)
        && headers.contains_key(header::CONTENT_LENGTH));
    remove_hop_by_hop(headers);
    if is_head && status != StatusCode::NO_CONTENT && status != StatusCode::NOT_MODIFIED {
        // A HEAD response carries the length a GET would have. Hyper does not
        // validate it because there is no body to frame, so Hamster does.
        let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
        if let Some(first) = values.next() {
            let length = parse_content_length(first).ok_or(())?;
            if values.any(|value| parse_content_length(value) != Some(length)) {
                return Err(());
            }
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
    } else {
        // Hyper re-derives the length from the body it actually streams. For
        // 204 and 304 there is no body and a stale length would be misleading.
        headers.remove(header::CONTENT_LENGTH);
    }
    Ok(reusable)
}

fn unsupported_transfer_coding(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::TRANSFER_ENCODING).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_some()
        || !value
            .to_str()
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
}

fn parse_content_length(value: &HeaderValue) -> Option<u64> {
    let text = value.to_str().ok()?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Removes every hop-by-hop field, including those nominated by `Connection`.
fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let nominated: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    headers.remove(header::CONNECTION);
    headers.remove(&KEEP_ALIVE);
    headers.remove(&PROXY_CONNECTION);
    headers.remove(header::PROXY_AUTHENTICATE);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove(header::TE);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::UPGRADE);
}

fn te_accepts_trailers(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::TE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            coding
                .split(';')
                .next()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case("trailers"))
        })
}

impl StdError for ConnectFailure {}

#[cfg(test)]
mod tests;
