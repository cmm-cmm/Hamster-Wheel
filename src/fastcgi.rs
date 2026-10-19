//! FastCGI v1: a `fastcgi` location runs ordinary PHP through FastCGI
//! endpoints the operator starts (for example `php-cgi -b 127.0.0.1:9000`).
//!
//! This is the compatibility path for applications written against a real
//! SAPI: `header()`, `setcookie()`, native sessions, superglobals and
//! `move_uploaded_file()` all work, because PHP itself parses the request.
//! Hamster does not manage the endpoint processes in this slice.
//!
//! One location owns one `FastCgiBackend`: its document root, its servers and
//! a fixed number of exchange slots (`servers x concurrency`). `php-cgi` on
//! Windows serves one connection at a time per process.
//!
//! With `keep_connections` (the default) every request asks the backend to
//! keep its connection (`FCGI_KEEP_CONN`), and a connection whose exchange
//! ended cleanly -- END_REQUEST read, nothing buffered after it, the whole
//! request body sent -- waits in its server's idle list for the next exchange
//! on that server. Measured against `php-cgi` on Windows, opening a
//! connection per request cost about 250 us of the 320 us a minimal script
//! took. Any other ending closes the connection.
//!
//! ```text
//! request path --resolve--> static file | script + PATH_INFO | front controller
//! Incoming body --writer task--> FCGI_STDIN  (concurrently with)
//! FCGI_STDOUT --head parse--> Response --FastCgiBody--> client
//! ```
//!
//! At-most-once: a request is sent to another server only when connecting
//! failed, so nothing was sent. Once `FCGI_BEGIN_REQUEST` is written the
//! exchange is never repeated. An idle connection is checked before reuse and
//! discarded if the backend closed it or sent anything while idle; a backend
//! that closes it between that check and the request answers 502 like any
//! other failure after the request was sent, and is never retried.

use std::{
    fmt, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::{Buf, Bytes, BytesMut};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Frame, Incoming, SizeHint},
    header::{self, HeaderName, HeaderValue},
};
use tokio::{
    io::{AsyncRead, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};

use crate::{
    access::{AccessPolicy, is_php_source},
    config::SERVER_NAME,
    handler::{HandlerResponse, simple_response},
    response_body::ResponseBody,
    static_files::{self, Lookup, PathError},
    transport::ConnectionInfo,
    write_progress::{ProgressDeadline, WriteProgressTimeout},
};

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_INDEX: &str = "index.php";
pub const MAX_SERVERS: usize = 64;
pub const MAX_CONCURRENCY: usize = 256;
/// Largest response head (status and header lines) accepted from a backend.
pub const MAX_RESPONSE_HEAD: usize = 64 * 1024;

const VERSION: u8 = 1;
const BEGIN_REQUEST: u8 = 1;
const END_REQUEST: u8 = 3;
const PARAMS: u8 = 4;
const STDIN: u8 = 5;
const STDOUT: u8 = 6;
const STDERR: u8 = 7;
const RESPONDER: u16 = 1;
/// `FCGI_KEEP_CONN`: the backend keeps the connection after END_REQUEST.
const KEEP_CONN: u8 = 1;
const REQUEST_ID: u16 = 1;
const MAX_CONTENT: usize = 65_535;
const FAILURE_NOTICE_PERIOD_MS: u64 = 60_000;
/// Output already received with the head, up to this size, is collected so
/// that an exchange whose END_REQUEST has also arrived finishes at once.
const EARLY_BODY_LIMIT: usize = 64 * 1024;
/// Bound on remembered script resolutions per location.
const RESOLVE_CACHE_ENTRIES: usize = 4096;
pub const DEFAULT_RESOLVE_CACHE: Duration = Duration::from_secs(1);
pub const MAX_RESOLVE_CACHE_SECONDS: u64 = 60;
#[cfg(windows)]
const MAX_BLOCKING_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Compiled, immutable configuration of one `fastcgi` location.
#[derive(Clone, Debug)]
pub struct FastCgiConfig {
    /// `<host><path>` of the owning location, used in every diagnostic.
    pub label: Arc<str>,
    /// Canonical document root. Scripts and static files resolve inside it.
    pub document_root: PathBuf,
    /// Front controller and directory index, relative to the root.
    pub index: String,
    pub servers: Vec<SocketAddr>,
    /// Exchanges one server accepts at once. `php-cgi` on Windows serves one.
    pub concurrency: usize,
    /// Waiting for a free exchange slot, and then for the TCP connection.
    pub connect_timeout: Duration,
    /// Inactivity while waiting for backend output, head and body alike.
    pub response_timeout: Duration,
    /// Reuse a connection for later exchanges on the same server.
    pub keep_connections: bool,
    /// How long a request path's script resolution is reused. Zero resolves
    /// every request.
    pub resolve_cache: Duration,
}

/// Counters for one backend. Reading them never touches the request path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FastCgiSnapshot {
    /// Script exchanges from slot acquisition until their body ends.
    pub active_exchanges: usize,
    pub scripts: u64,
    pub static_files: u64,
    pub connect_failures: u64,
    /// Requests refused because every slot stayed busy for `connect_timeout`.
    pub busy_refusals: u64,
    pub timeouts: u64,
    /// Missing, malformed or truncated responses.
    pub backend_failures: u64,
    /// TCP connections opened to a server.
    pub connections_opened: u64,
    /// Exchanges that ran on a kept connection instead of a new one.
    pub connections_reused: u64,
    /// Requests whose script came from the resolution cache.
    pub resolutions_reused: u64,
}

#[derive(Default)]
struct Counters {
    active_exchanges: AtomicUsize,
    scripts: AtomicU64,
    static_files: AtomicU64,
    connect_failures: AtomicU64,
    busy_refusals: AtomicU64,
    timeouts: AtomicU64,
    backend_failures: AtomicU64,
    connections_opened: AtomicU64,
    connections_reused: AtomicU64,
    resolutions_reused: AtomicU64,
}

/// A request path's script, as resolution found it, until `until`.
struct CachedScript {
    filename: PathBuf,
    script_name: String,
    path_info: String,
    until: std::time::Instant,
}

pub struct FastCgiBackend {
    config: FastCgiConfig,
    /// `DOCUMENT_ROOT` as PHP should see it: without the `\\?\` prefix that
    /// Windows canonicalization adds.
    document_root_param: String,
    slots: Arc<Semaphore>,
    /// Bounds closures queued in Tokio's blocking pool even after their HTTP
    /// waiters have released exchange slots.
    #[cfg(windows)]
    blocking_connect_slots: Arc<Semaphore>,
    in_flight: Box<[AtomicUsize]>,
    /// Kept connections per server, at most `concurrency` each: a connection
    /// exists only while its exchange holds that server's claim or waits here.
    idle: Box<[std::sync::Mutex<Vec<TcpStream>>]>,
    /// Script resolutions by raw request path, at most `RESOLVE_CACHE_ENTRIES`.
    resolved: std::sync::Mutex<std::collections::HashMap<Box<str>, CachedScript>>,
    next: AtomicUsize,
    counters: Counters,
    #[cfg(feature = "profiling")]
    profile: crate::fastcgi_profile::FastCgiProfile,
    #[cfg(feature = "profiling")]
    profile_async_connect: std::sync::atomic::AtomicBool,
    started: std::time::Instant,
    next_notice_ms: AtomicU64,
}

impl fmt::Debug for FastCgiBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FastCgiBackend")
            .field("label", &self.config.label)
            .field("servers", &self.config.servers)
            .finish_non_exhaustive()
    }
}

impl FastCgiBackend {
    pub fn new(config: FastCgiConfig) -> Arc<Self> {
        let permits = config.servers.len() * config.concurrency;
        Arc::new(Self {
            document_root_param: display_path(&config.document_root),
            slots: Arc::new(Semaphore::new(permits)),
            #[cfg(windows)]
            blocking_connect_slots: Arc::new(Semaphore::new(permits)),
            in_flight: config.servers.iter().map(|_| AtomicUsize::new(0)).collect(),
            idle: config
                .servers
                .iter()
                .map(|_| std::sync::Mutex::new(Vec::new()))
                .collect(),
            resolved: std::sync::Mutex::new(std::collections::HashMap::new()),
            next: AtomicUsize::new(0),
            counters: Counters::default(),
            #[cfg(feature = "profiling")]
            profile: crate::fastcgi_profile::FastCgiProfile::default(),
            #[cfg(feature = "profiling")]
            profile_async_connect: std::sync::atomic::AtomicBool::new(false),
            started: std::time::Instant::now(),
            next_notice_ms: AtomicU64::new(0),
            config,
        })
    }

    pub fn label(&self) -> &str {
        &self.config.label
    }

    pub fn servers(&self) -> &[SocketAddr] {
        &self.config.servers
    }

    pub fn snapshot(&self) -> FastCgiSnapshot {
        let counters = &self.counters;
        FastCgiSnapshot {
            active_exchanges: counters.active_exchanges.load(Ordering::Acquire),
            scripts: counters.scripts.load(Ordering::Relaxed),
            static_files: counters.static_files.load(Ordering::Relaxed),
            connect_failures: counters.connect_failures.load(Ordering::Relaxed),
            busy_refusals: counters.busy_refusals.load(Ordering::Relaxed),
            timeouts: counters.timeouts.load(Ordering::Relaxed),
            backend_failures: counters.backend_failures.load(Ordering::Relaxed),
            connections_opened: counters.connections_opened.load(Ordering::Relaxed),
            connections_reused: counters.connections_reused.load(Ordering::Relaxed),
            resolutions_reused: counters.resolutions_reused.load(Ordering::Relaxed),
        }
    }

    /// The script `uri_path` resolved to, if that was within `resolve_cache`.
    fn cached_script(&self, uri_path: &str) -> Option<Target> {
        if self.config.resolve_cache.is_zero() {
            return None;
        }
        let cache = self
            .resolved
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = cache.get(uri_path)?;
        if std::time::Instant::now() >= entry.until {
            return None;
        }
        self.counters
            .resolutions_reused
            .fetch_add(1, Ordering::Relaxed);
        Some(Target::Script {
            filename: entry.filename.clone(),
            script_name: entry.script_name.clone(),
            path_info: entry.path_info.clone(),
        })
    }

    /// Remembers a script resolution. Request paths are chosen by clients and
    /// a front controller turns any of them into a script, so the cache is
    /// bounded: when full it first drops expired entries, then everything.
    fn remember_script(&self, uri_path: &str, filename: &Path, script_name: &str, path_info: &str) {
        if self.config.resolve_cache.is_zero() {
            return;
        }
        let now = std::time::Instant::now();
        let mut cache = self
            .resolved
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.len() >= RESOLVE_CACHE_ENTRIES && !cache.contains_key(uri_path) {
            cache.retain(|_, entry| entry.until > now);
            if cache.len() >= RESOLVE_CACHE_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            uri_path.into(),
            CachedScript {
                filename: filename.to_path_buf(),
                script_name: script_name.to_owned(),
                path_info: path_info.to_owned(),
                until: now + self.config.resolve_cache,
            },
        );
    }

    /// A kept connection to `server` that the backend has neither closed nor
    /// written to since its last exchange ended. The caller holds that
    /// server's claim.
    fn take_idle(&self, server: usize) -> Option<TcpStream> {
        loop {
            let candidate = self.idle[server]
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop()?;
            if quiet(&candidate) {
                return Some(candidate);
            }
        }
    }

    /// Keeps a connection whose exchange ended cleanly for the next exchange
    /// on `server`. The caller still holds that server's claim.
    fn keep_idle(&self, server: usize, stream: TcpStream) {
        self.idle[server]
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(stream);
    }

    #[cfg(feature = "profiling")]
    pub fn take_profile(&self) -> Vec<crate::fastcgi_profile::FastCgiStageTiming> {
        self.profile.take()
    }

    #[cfg(feature = "profiling")]
    pub fn set_profile_async_connect(&self, enabled: bool) {
        self.profile_async_connect.store(enabled, Ordering::Release);
    }

    /// Claims one exchange on a server not in `tried`. The caller holds a
    /// semaphore permit, so some server has room unless all were tried.
    fn claim_server(&self, tried: &[usize]) -> Option<usize> {
        let count = self.config.servers.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for offset in 0..count {
            let index = (start + offset) % count;
            if tried.contains(&index) {
                continue;
            }
            let in_flight = &self.in_flight[index];
            let mut current = in_flight.load(Ordering::Acquire);
            while current < self.config.concurrency {
                match in_flight.compare_exchange(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(index),
                    Err(actual) => current = actual,
                }
            }
        }
        None
    }

    fn release_server(&self, index: usize) {
        self.in_flight[index].fetch_sub(1, Ordering::AcqRel);
    }

    fn record_failure(&self, counter: &AtomicU64, what: fmt::Arguments<'_>) {
        let total = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let due = self.next_notice_ms.load(Ordering::Relaxed);
        if elapsed_ms < due
            || self
                .next_notice_ms
                .compare_exchange(
                    due,
                    elapsed_ms.saturating_add(FAILURE_NOTICE_PERIOD_MS),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }
        errln!(
            "FastCGI {}: {what} ({total} of this kind since start; reported at most once per {} s)",
            self.config.label,
            FAILURE_NOTICE_PERIOD_MS / 1_000
        );
    }
}

/// Whether an idle connection is still open and silent: a non-blocking peek
/// on the socket itself reports would-block. End of stream means the backend
/// closed it; data is a protocol violation between exchanges; either way it
/// is not reused. The peek is a direct system call rather than a reactor
/// read, whose readiness may not yet reflect a close that already arrived.
fn quiet(stream: &TcpStream) -> bool {
    #[cfg(windows)]
    let socket = {
        use std::os::windows::io::{AsRawSocket, FromRawSocket};
        // SAFETY: the socket stays owned by `stream`, which outlives this
        // borrowed view; `ManuallyDrop` keeps the view from closing it.
        std::mem::ManuallyDrop::new(unsafe {
            std::net::TcpStream::from_raw_socket(stream.as_raw_socket())
        })
    };
    #[cfg(unix)]
    let socket = {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: as above, for the file descriptor.
        std::mem::ManuallyDrop::new(unsafe { std::net::TcpStream::from_raw_fd(stream.as_raw_fd()) })
    };
    let mut probe = [0_u8; 1];
    matches!(socket.peek(&mut probe), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
}

/// The permit owns the active-exchange count, including while a Windows
/// blocking connect continues after its HTTP waiter is canceled or times out.
struct ExchangePermit {
    backend: Arc<FastCgiBackend>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ExchangePermit {
    fn drop(&mut self) {
        self.backend
            .counters
            .active_exchanges
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// One exchange slot: a semaphore permit and the server it was claimed on.
struct Slot {
    backend: Arc<FastCgiBackend>,
    server: usize,
    _permit: ExchangePermit,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.backend.release_server(self.server);
    }
}

/// Carries both ownership claims into a connect operation. If the HTTP task
/// vanishes, a detached blocking connect still holds the bounded permit and
/// server claim until the connect task exits, and sends no FastCGI request bytes.
struct ConnectAttempt {
    backend: Arc<FastCgiBackend>,
    server: Option<usize>,
    permit: Option<ExchangePermit>,
}

impl ConnectAttempt {
    fn new(backend: &Arc<FastCgiBackend>, server: usize, permit: ExchangePermit) -> Self {
        Self {
            backend: Arc::clone(backend),
            server: Some(server),
            permit: Some(permit),
        }
    }

    fn into_slot(mut self) -> Slot {
        Slot {
            backend: Arc::clone(&self.backend),
            server: self.server.take().expect("claimed server"),
            _permit: self.permit.take().expect("exchange permit"),
        }
    }

    fn into_permit(mut self) -> ExchangePermit {
        self.permit.take().expect("exchange permit")
    }
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            self.backend.release_server(server);
        }
    }
}

enum ConnectOutcome {
    Connected(ConnectAttempt, TcpStream),
    Failed(ConnectAttempt, io::Error),
    /// The spawned connect still owns the claims, or dropped them on panic.
    #[cfg(windows)]
    Detached(io::Error),
}

/// A per-connect ownership handoff. Aborting a queued `spawn_blocking` task
/// does not promptly drop its captured values, so the HTTP waiter must be
/// able to reclaim an attempt that the blocking closure has not taken yet.
/// Once taken, only that closure owns the attempt until connect returns.
#[cfg(windows)]
struct PendingConnect {
    claim: Arc<std::sync::Mutex<Option<ConnectAttempt>>>,
    task: JoinHandle<Option<(ConnectAttempt, io::Result<std::net::TcpStream>)>>,
}

#[cfg(windows)]
impl Drop for PendingConnect {
    fn drop(&mut self) {
        self.claim
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.task.abort();
    }
}

async fn connect_server(
    backend: &FastCgiBackend,
    address: SocketAddr,
    attempt: ConnectAttempt,
) -> ConnectOutcome {
    let duration = backend.config.connect_timeout;
    #[cfg(windows)]
    {
        // The blocking path is for Windows php-cgi: one exchange per endpoint.
        // A multiplexing endpoint or a long connect deadline could otherwise
        // occupy many non-cancelable blocking tasks during an outage or delay
        // runtime shutdown, so those cases retain cancellable async I/O.
        if backend.config.concurrency != 1 || duration > MAX_BLOCKING_CONNECT_TIMEOUT {
            return async_connect(address, duration, attempt).await;
        }
        #[cfg(feature = "profiling")]
        if backend.profile_async_connect.load(Ordering::Acquire) {
            return async_connect(address, duration, attempt).await;
        }
        // A canceled queued closure can outlive its exchange claim until a
        // blocking thread dequeues it. Do not enqueue an unbounded number of
        // such closures while the blocking pool is saturated.
        let Ok(blocking_permit) = Arc::clone(&backend.blocking_connect_slots).try_acquire_owned()
        else {
            return async_connect(address, duration, attempt).await;
        };
        let deadline = std::time::Instant::now() + duration;
        let claim = Arc::new(std::sync::Mutex::new(Some(attempt)));
        let worker_claim = Arc::clone(&claim);
        let mut pending = PendingConnect {
            claim,
            task: tokio::task::spawn_blocking(move || {
                let _blocking_permit = blocking_permit;
                let attempt = worker_claim
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()?;
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let result = if remaining.is_zero() {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "connect deadline elapsed",
                    ))
                } else {
                    std::net::TcpStream::connect_timeout(&address, remaining).and_then(|stream| {
                        stream.set_nonblocking(true)?;
                        Ok(stream)
                    })
                };
                Some((attempt, result))
            }),
        };
        match timeout_at(Instant::from_std(deadline), &mut pending.task).await {
            Ok(Ok(Some((attempt, Ok(stream))))) => match TcpStream::from_std(stream) {
                Ok(stream) => ConnectOutcome::Connected(attempt, stream),
                Err(error) => ConnectOutcome::Failed(attempt, error),
            },
            Ok(Ok(Some((attempt, Err(error))))) => ConnectOutcome::Failed(attempt, error),
            Ok(Ok(None)) => ConnectOutcome::Detached(io::Error::other("connect was canceled")),
            Ok(Err(error)) => ConnectOutcome::Detached(io::Error::other(error)),
            Err(_) => ConnectOutcome::Detached(io::Error::new(
                io::ErrorKind::TimedOut,
                "connect deadline elapsed",
            )),
        }
    }
    #[cfg(not(windows))]
    async_connect(address, duration, attempt).await
}

async fn async_connect(
    address: SocketAddr,
    duration: Duration,
    attempt: ConnectAttempt,
) -> ConnectOutcome {
    match timeout(duration, TcpStream::connect(address)).await {
        Ok(Ok(stream)) => ConnectOutcome::Connected(attempt, stream),
        Ok(Err(error)) => ConnectOutcome::Failed(attempt, error),
        Err(_) => ConnectOutcome::Failed(
            attempt,
            io::Error::new(io::ErrorKind::TimedOut, "connect deadline elapsed"),
        ),
    }
}

// ----- Path resolution ------------------------------------------------------

/// What a request path names beneath the document root.
#[derive(Debug)]
pub(crate) enum Target {
    Script {
        /// Final location of the script, as opened and contained.
        filename: PathBuf,
        /// URI path of the script, from the root.
        script_name: String,
        /// Remainder of the request path after the script, or empty.
        path_info: String,
    },
    Static {
        path: PathBuf,
        file: std::fs::File,
        length: u64,
    },
    NotFound,
    Forbidden,
    BadRequest,
}

/// Decides what a request path runs or serves. Blocking.
///
/// 1. The first segment ending in `.php` (any case) names the script if it is
///    an existing file inside the root; what follows it is `PATH_INFO`. A
///    `.php` name that does not exist is 404, never the front controller.
/// 2. Otherwise an existing file is served as a static file, unless its final
///    location ends in `.php`: Windows opens `index.php.` or `index.php ` as
///    `index.php`, and a script's source must never be sent.
/// 3. A directory runs its `index` script if present, else serves its
///    `index.html`, else falls through.
/// 4. Anything else runs the front controller, `index` at the root.
///
/// Every decision is made on the object actually opened, and an object whose
/// final location is outside the root is 403, as for the `static` handler.
pub(crate) fn resolve(document_root: &Path, index: &str, uri_path: &str) -> io::Result<Target> {
    let segments = match static_files::decode_segments(uri_path) {
        Ok(segments) => segments,
        Err(PathError::InvalidEncoding) => return Ok(Target::BadRequest),
        Err(PathError::Traversal) => return Ok(Target::Forbidden),
    };

    if let Some(position) = segments.iter().position(|segment| is_php_name(segment)) {
        let relative: PathBuf = segments[..=position].iter().collect();
        return Ok(
            match static_files::lookup_within(document_root, &relative)? {
                Lookup::File { path, .. } => script(document_root, path, &segments[position + 1..]),
                Lookup::Outside => Target::Forbidden,
                Lookup::Directory | Lookup::Missing => Target::NotFound,
            },
        );
    }

    let relative: PathBuf = segments.iter().collect();
    match static_files::lookup_within(document_root, &relative)? {
        Lookup::File { path, file, length } => {
            return Ok(if is_php_path(&path) {
                script(document_root, path, &[])
            } else {
                Target::Static { path, file, length }
            });
        }
        Lookup::Outside => return Ok(Target::Forbidden),
        Lookup::Directory => {
            for name in [index, "index.html"] {
                match static_files::lookup_within(document_root, &relative.join(name))? {
                    Lookup::File { path, file, length } => {
                        return Ok(if is_php_path(&path) {
                            script(document_root, path, &[])
                        } else {
                            Target::Static { path, file, length }
                        });
                    }
                    Lookup::Outside => return Ok(Target::Forbidden),
                    Lookup::Directory | Lookup::Missing => {}
                }
            }
        }
        Lookup::Missing => {}
    }

    Ok(
        match static_files::lookup_within(document_root, Path::new(index))? {
            Lookup::File { path, .. } if is_php_path(&path) => script(document_root, path, &[]),
            Lookup::Outside => Target::Forbidden,
            _ => Target::NotFound,
        },
    )
}

fn script(document_root: &Path, filename: PathBuf, rest: &[String]) -> Target {
    let relative = filename
        .strip_prefix(document_root)
        .expect("a contained path is beneath the root");
    let mut script_name = String::new();
    for component in relative.components() {
        script_name.push('/');
        script_name.push_str(&component.as_os_str().to_string_lossy());
    }
    let path_info = rest.iter().fold(String::new(), |mut path, segment| {
        path.push('/');
        path.push_str(segment);
        path
    });
    Target::Script {
        filename,
        script_name,
        path_info,
    }
}

fn is_php_name(name: &str) -> bool {
    name.as_bytes()
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".php"))
}

fn is_php_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
}

/// A path as a program should see it: `\\?\C:\x` becomes `C:\x` and
/// `\\?\UNC\server\share` becomes `\\server\share`.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        text.into_owned()
    }
}

// ----- Records --------------------------------------------------------------

fn record(kind: u8, content: &[u8], out: &mut Vec<u8>) {
    debug_assert!(content.len() <= MAX_CONTENT);
    let [id_high, id_low] = REQUEST_ID.to_be_bytes();
    let [length_high, length_low] = (content.len() as u16).to_be_bytes();
    out.extend_from_slice(&[
        VERSION,
        kind,
        id_high,
        id_low,
        length_high,
        length_low,
        0,
        0,
    ]);
    out.extend_from_slice(content);
}

/// A stream of records of one kind, ended by an empty record.
fn stream(kind: u8, content: &[u8], out: &mut Vec<u8>) {
    for chunk in content.chunks(MAX_CONTENT) {
        record(kind, chunk, out);
    }
    record(kind, &[], out);
}

fn name_value(name: &[u8], value: &[u8], out: &mut Vec<u8>) {
    for length in [name.len(), value.len()] {
        if length < 128 {
            out.push(length as u8);
        } else {
            out.extend_from_slice(&((length as u32) | 0x8000_0000).to_be_bytes());
        }
    }
    out.extend_from_slice(name);
    out.extend_from_slice(value);
}

#[derive(Debug)]
pub(crate) struct Record {
    pub(crate) kind: u8,
    pub(crate) content: Bytes,
}

/// Reads whole records from a backend connection.
pub(crate) struct RecordReader<R> {
    inner: R,
    buffer: BytesMut,
}

impl<R: AsyncRead + Unpin> RecordReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
        }
    }

    /// The next record, `None` at a clean end of stream.
    pub(crate) fn poll_record(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<Record>>> {
        loop {
            if self.buffer.len() >= 8 {
                if self.buffer[0] != VERSION {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("FastCGI record version {}", self.buffer[0]),
                    )));
                }
                let content = usize::from(u16::from_be_bytes([self.buffer[4], self.buffer[5]]));
                let padding = usize::from(self.buffer[6]);
                if self.buffer.len() >= 8 + content + padding {
                    let kind = self.buffer[1];
                    let mut whole = self.buffer.split_to(8 + content + padding);
                    whole.advance(8);
                    whole.truncate(content);
                    return Poll::Ready(Ok(Some(Record {
                        kind,
                        content: whole.freeze(),
                    })));
                }
            }
            let mut chunk = [0_u8; 16 * 1024];
            let mut read = ReadBuf::new(&mut chunk);
            match Pin::new(&mut self.inner).poll_read(cx, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) if read.filled().is_empty() => {
                    return Poll::Ready(if self.buffer.is_empty() {
                        Ok(None)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "FastCGI connection closed inside a record",
                        ))
                    });
                }
                Poll::Ready(Ok(())) => self.buffer.extend_from_slice(read.filled()),
            }
        }
    }

    pub(crate) async fn next(&mut self) -> io::Result<Option<Record>> {
        std::future::poll_fn(|cx| self.poll_record(cx)).await
    }

    /// Whether bytes past the last returned record have already been read.
    fn has_buffered(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

// ----- Serving --------------------------------------------------------------

type BackendStream = WriteProgressTimeout<TcpStream>;

/// Serves one request through `backend`. Returns a synthetic response only
/// before the backend's response head is accepted.
pub(crate) async fn serve(
    backend: &Arc<FastCgiBackend>,
    access: &AccessPolicy,
    request: Request<Incoming>,
    connection: ConnectionInfo,
    body_read_timeout: Duration,
) -> HandlerResponse {
    if request.method() == Method::CONNECT {
        return simple_response(StatusCode::NOT_IMPLEMENTED, "Not Implemented");
    }
    // A script found for this path within `resolve_cache` is used again
    // without touching the filesystem: on Windows each open costs tens of
    // microseconds and the opens serialize under concurrency, and PHP opens
    // the script by its path anyway. Static files and refusals are always
    // resolved afresh, and the access policy below applies to every request.
    if let Some(target) = backend.cached_script(request.uri().path()) {
        return serve_target(
            backend,
            access,
            request,
            connection,
            body_read_timeout,
            target,
        )
        .await;
    }
    let root = backend.config.document_root.clone();
    let index = backend.config.index.clone();
    let uri_path = request.uri().path().to_owned();
    let target = match tokio::task::spawn_blocking(move || resolve(&root, &index, &uri_path)).await
    {
        Ok(Ok(target)) => {
            if let Target::Script {
                filename,
                script_name,
                path_info,
            } = &target
            {
                backend.remember_script(request.uri().path(), filename, script_name, path_info);
            }
            target
        }
        Ok(Err(error)) => {
            errln!(
                "FastCGI {}: resolving a request path failed: {error}",
                backend.label()
            );
            return simple_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
        Err(error) => {
            errln!(
                "FastCGI {}: path resolution task failed: {error}",
                backend.label()
            );
            return simple_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    };
    serve_target(
        backend,
        access,
        request,
        connection,
        body_read_timeout,
        target,
    )
    .await
}

/// Serves what a request path resolved to, fresh or from the cache alike.
async fn serve_target(
    backend: &Arc<FastCgiBackend>,
    access: &AccessPolicy,
    request: Request<Incoming>,
    connection: ConnectionInfo,
    body_read_timeout: Duration,
    target: Target,
) -> HandlerResponse {
    match target {
        Target::BadRequest => simple_response(StatusCode::BAD_REQUEST, "Bad Request"),
        Target::Forbidden => simple_response(StatusCode::FORBIDDEN, "Forbidden"),
        Target::NotFound => simple_response(StatusCode::NOT_FOUND, "Not Found"),
        Target::Static { path, file, length } => {
            if access.denies_final_path(&backend.config.document_root, &path)
                || is_php_source(&path)
            {
                return simple_response(StatusCode::NOT_FOUND, "Not Found");
            }
            backend
                .counters
                .static_files
                .fetch_add(1, Ordering::Relaxed);
            serve_static(request.method(), &path, file, length)
        }
        Target::Script {
            filename,
            script_name,
            path_info,
        } => {
            if access.denies_final_path(&backend.config.document_root, &filename) {
                return simple_response(StatusCode::NOT_FOUND, "Not Found");
            }
            backend.counters.scripts.fetch_add(1, Ordering::Relaxed);
            let params = cgi_params(
                backend,
                &request,
                connection,
                &filename,
                &script_name,
                &path_info,
            );
            run_script(backend, request, params, body_read_timeout).await
        }
    }
}

fn serve_static(method: &Method, path: &Path, file: std::fs::File, length: u64) -> HandlerResponse {
    if method == Method::HEAD {
        return static_files::file_response(path, ResponseBody::buffered(Bytes::new()), length);
    }
    if method != Method::GET {
        let mut response = simple_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    static_files::file_response(
        path,
        ResponseBody::file(tokio::fs::File::from_std(file), length),
        length,
    )
}

/// CGI/1.1 meta-variables (RFC 3875) plus what PHP's CGI SAPI expects.
fn cgi_params(
    backend: &FastCgiBackend,
    request: &Request<Incoming>,
    connection: ConnectionInfo,
    filename: &Path,
    script_name: &str,
    path_info: &str,
) -> Vec<u8> {
    let transport = connection.transport();
    let uri = request.uri();
    let request_uri = uri
        .path_and_query()
        .map_or_else(|| uri.path().to_owned(), |value| value.as_str().to_owned());
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let server_name = host
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host, |(name, _)| name);
    let protocol = match request.version() {
        hyper::Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let https = connection.scheme().as_str() == "https";

    let mut out = Vec::with_capacity(2048);
    let mut add = |name: &str, value: &str| name_value(name.as_bytes(), value.as_bytes(), &mut out);
    add("GATEWAY_INTERFACE", "CGI/1.1");
    add("SERVER_SOFTWARE", SERVER_NAME);
    add("SERVER_PROTOCOL", protocol);
    add("REQUEST_METHOD", request.method().as_str());
    add("REQUEST_URI", &request_uri);
    add("QUERY_STRING", uri.query().unwrap_or_default());
    add("DOCUMENT_ROOT", &backend.document_root_param);
    add("SCRIPT_FILENAME", &display_path(filename));
    add("SCRIPT_NAME", script_name);
    if !path_info.is_empty() {
        add("PATH_INFO", path_info);
        add(
            "PATH_TRANSLATED",
            &format!(
                "{}{}",
                backend.document_root_param,
                path_info.replace('/', std::path::MAIN_SEPARATOR_STR)
            ),
        );
    }
    add("REMOTE_ADDR", &transport.peer_addr().ip().to_string());
    add("REMOTE_PORT", &transport.peer_addr().port().to_string());
    add("SERVER_ADDR", &transport.local_addr().ip().to_string());
    add("SERVER_PORT", &transport.local_addr().port().to_string());
    add("SERVER_NAME", server_name);
    add("REQUEST_SCHEME", connection.scheme().as_str());
    if https {
        add("HTTPS", "on");
    }
    // php-cgi refuses to run without it while cgi.force_redirect is on, its
    // default: the variable says a server, not a browser, invoked it.
    add("REDIRECT_STATUS", "200");
    if let Some(value) = request.headers().get(header::CONTENT_TYPE) {
        add("CONTENT_TYPE", &String::from_utf8_lossy(value.as_bytes()));
    }
    if let Some(value) = request.headers().get(header::CONTENT_LENGTH) {
        add("CONTENT_LENGTH", &String::from_utf8_lossy(value.as_bytes()));
    }

    let mut fields: Vec<(String, String)> = Vec::new();
    for (name, value) in request.headers() {
        // `Proxy` would become HTTP_PROXY, which many HTTP clients inside the
        // application read as their outgoing proxy (httpoxy, CVE-2016-5385).
        if name == header::CONTENT_TYPE
            || name == header::CONTENT_LENGTH
            || name.as_str() == "proxy"
        {
            continue;
        }
        let variable = format!(
            "HTTP_{}",
            name.as_str().to_ascii_uppercase().replace('-', "_")
        );
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        // One variable per field, every value joined as a CGI server joins
        // them (RFC 3875 4.1.18); Cookie with "; ".
        match fields
            .iter_mut()
            .find(|(existing, _)| *existing == variable)
        {
            Some((_, joined)) => {
                joined.push_str(if name == header::COOKIE { "; " } else { ", " });
                joined.push_str(&value);
            }
            None => fields.push((variable, value)),
        }
    }
    for (name, value) in &fields {
        add(name, value);
    }
    out
}

enum WriterEnd {
    /// The whole body and its end record were sent. The write half comes back
    /// so that a kept connection can be reassembled.
    Done(WriteHalf<BackendStream>),
    /// The client's body did not arrive within `body_read_timeout`.
    ClientTimedOut,
    ClientFailed,
    BackendFailed,
}

/// The request-sending side of one exchange.
enum Writer {
    /// The body is still being streamed by its own task.
    Sending(JoinHandle<WriterEnd>),
    /// Everything was sent; the write half is held, never shut down, because
    /// a half-closed socket may read to a backend as an aborted request.
    Sent(WriteHalf<BackendStream>),
    /// Aborted or failed: the connection cannot be kept.
    Gone,
}

impl Writer {
    fn abort(&mut self) {
        if let Self::Sending(task) = std::mem::replace(self, Self::Gone) {
            task.abort();
        }
    }

    /// The write half, if the whole request is known to have been sent.
    fn poll_sent(&mut self, cx: &mut Context<'_>) -> Option<WriteHalf<BackendStream>> {
        if let Self::Sending(task) = self
            && task.is_finished()
        {
            *self = match Pin::new(task).poll(cx) {
                Poll::Ready(Ok(WriterEnd::Done(half))) => Self::Sent(half),
                _ => Self::Gone,
            };
        }
        match std::mem::replace(self, Self::Gone) {
            Self::Sent(half) => Some(half),
            other => {
                *self = other;
                None
            }
        }
    }
}

async fn run_script(
    backend: &Arc<FastCgiBackend>,
    request: Request<Incoming>,
    params: Vec<u8>,
    body_read_timeout: Duration,
) -> HandlerResponse {
    #[cfg(feature = "profiling")]
    let slot_started = std::time::Instant::now();
    let has_body = !request.body().is_end_stream();
    if has_body && !request.headers().contains_key(header::CONTENT_LENGTH) {
        // PHP needs CONTENT_LENGTH before it reads the body, and a chunked
        // body would have to be buffered whole to learn it.
        return simple_response(StatusCode::LENGTH_REQUIRED, "Length Required");
    }
    let body_deadline = Instant::now() + body_read_timeout;
    let is_head = request.method() == Method::HEAD;

    let Ok(Ok(permit)) = timeout(
        backend.config.connect_timeout,
        Arc::clone(&backend.slots).acquire_owned(),
    )
    .await
    else {
        backend.record_failure(
            &backend.counters.busy_refusals,
            format_args!("every exchange slot stayed busy for the connect timeout"),
        );
        return simple_response(StatusCode::SERVICE_UNAVAILABLE, "FastCGI Busy");
    };
    #[cfg(feature = "profiling")]
    let slot_acquired = std::time::Instant::now();
    backend
        .counters
        .active_exchanges
        .fetch_add(1, Ordering::AcqRel);

    // Connecting is the only step that may move to another server: until the
    // first record is written, the backend has seen nothing.
    let mut tried = Vec::new();
    let mut permit = Some(ExchangePermit {
        backend: Arc::clone(backend),
        _permit: permit,
    });
    #[cfg(feature = "profiling")]
    let connect_started = std::time::Instant::now();
    let (slot, stream) = loop {
        let Some(server) = backend.claim_server(&tried) else {
            return simple_response(StatusCode::BAD_GATEWAY, "FastCGI Unavailable");
        };
        tried.push(server);
        let attempt = ConnectAttempt::new(
            backend,
            server,
            permit.take().expect("one permit per exchange"),
        );
        if let Some(stream) = backend.take_idle(server) {
            backend
                .counters
                .connections_reused
                .fetch_add(1, Ordering::Relaxed);
            break (attempt.into_slot(), stream);
        }
        let address = backend.config.servers[server];
        match connect_server(backend, address, attempt).await {
            ConnectOutcome::Connected(attempt, stream) => {
                let _ = stream.set_nodelay(true);
                backend
                    .counters
                    .connections_opened
                    .fetch_add(1, Ordering::Relaxed);
                break (attempt.into_slot(), stream);
            }
            ConnectOutcome::Failed(attempt, error) => {
                permit = Some(attempt.into_permit());
                backend.record_failure(
                    &backend.counters.connect_failures,
                    format_args!("connecting to {address} failed: {error}"),
                );
            }
            #[cfg(windows)]
            ConnectOutcome::Detached(error) => {
                backend.record_failure(
                    &backend.counters.connect_failures,
                    format_args!("connecting to {address} failed: {error}"),
                );
                return simple_response(StatusCode::BAD_GATEWAY, "FastCGI Unavailable");
            }
        }
    };
    #[cfg(feature = "profiling")]
    let connected = std::time::Instant::now();

    let stream = WriteProgressTimeout::new(stream, backend.config.response_timeout);
    let (reader, mut writer) = tokio::io::split(stream);

    let mut preamble = Vec::with_capacity(params.len() + 64);
    let [role_high, role_low] = RESPONDER.to_be_bytes();
    let flags = if backend.config.keep_connections {
        KEEP_CONN
    } else {
        0
    };
    record(
        BEGIN_REQUEST,
        &[role_high, role_low, flags, 0, 0, 0, 0, 0],
        &mut preamble,
    );
    stream_records(PARAMS, &params, &mut preamble);
    if !has_body {
        record(STDIN, &[], &mut preamble);
    }
    if let Err(error) = writer.write_all(&preamble).await {
        backend.record_failure(
            &backend.counters.backend_failures,
            format_args!("sending the request failed: {error}"),
        );
        drop(slot);
        return simple_response(StatusCode::BAD_GATEWAY, "FastCGI Error");
    }
    #[cfg(feature = "profiling")]
    let written = std::time::Instant::now();

    let mut writer_task = if has_body {
        Writer::Sending(tokio::spawn(write_body(
            request.into_body(),
            writer,
            body_deadline,
        )))
    } else {
        Writer::Sent(writer)
    };

    let mut reader = RecordReader::new(reader);
    let head = read_head(backend, &mut reader, &mut writer_task).await;
    #[cfg(feature = "profiling")]
    if head.is_ok() {
        backend
            .profile
            .record(crate::fastcgi_profile::FastCgiStageTiming {
                slot_wait: slot_acquired.duration_since(slot_started),
                tcp_connect: connected.duration_since(connect_started),
                preamble_write: written.duration_since(connected),
                backend_head: std::time::Instant::now().duration_since(written),
            });
    }
    match head {
        Ok((head, rest)) => match parse_head(&head) {
            Ok(mut response) => {
                if is_head {
                    writer_task.abort();
                    drop(slot);
                    *response.body_mut() = ResponseBody::buffered(Bytes::new());
                    return response;
                }
                let mut body = FastCgiBody {
                    reader: Some(reader),
                    pending: (!rest.is_empty()).then_some(rest),
                    deadline: ProgressDeadline::new(backend.config.response_timeout),
                    writer: writer_task,
                    slot: Some(slot),
                    finished: false,
                };
                let (parts, _) = response.into_parts();
                let body = match body.finish_if_arrived() {
                    Arrived::Whole(bytes) => ResponseBody::buffered(bytes),
                    Arrived::Failed => {
                        return simple_response(StatusCode::BAD_GATEWAY, "FastCGI Error");
                    }
                    Arrived::Partly => ResponseBody::fastcgi(body),
                };
                Response::from_parts(parts, body)
            }
            Err(problem) => {
                writer_task.abort();
                backend.record_failure(
                    &backend.counters.backend_failures,
                    format_args!("invalid response head: {problem}"),
                );
                simple_response(StatusCode::BAD_GATEWAY, "FastCGI Error")
            }
        },
        Err(failure) => {
            writer_task.abort();
            drop(slot);
            match failure {
                HeadFailure::TimedOut => {
                    backend.record_failure(
                        &backend.counters.timeouts,
                        format_args!(
                            "no response within the response timeout of {} s",
                            backend.config.response_timeout.as_secs()
                        ),
                    );
                    simple_response(StatusCode::GATEWAY_TIMEOUT, "FastCGI Response Timeout")
                }
                HeadFailure::ClientTimedOut => {
                    simple_response(StatusCode::REQUEST_TIMEOUT, "Request Body Timeout")
                }
                HeadFailure::ClientFailed => {
                    simple_response(StatusCode::BAD_REQUEST, "Bad Request")
                }
                HeadFailure::Backend(problem) => {
                    backend.record_failure(
                        &backend.counters.backend_failures,
                        format_args!("{problem}"),
                    );
                    simple_response(StatusCode::BAD_GATEWAY, "FastCGI Error")
                }
            }
        }
    }
}

fn stream_records(kind: u8, content: &[u8], out: &mut Vec<u8>) {
    stream(kind, content, out);
}

/// Streams the client's body as FCGI_STDIN, concurrently with the response:
/// a script that answers before reading its input, or writes more than a
/// socket buffer before reading, must not deadlock against this writer.
async fn write_body(
    mut body: Incoming,
    mut writer: WriteHalf<BackendStream>,
    deadline: Instant,
) -> WriterEnd {
    use http_body_util::BodyExt;
    loop {
        let frame = match timeout_at(deadline, body.frame()).await {
            Err(_) => return WriterEnd::ClientTimedOut,
            Ok(None) => break,
            Ok(Some(Err(_))) => return WriterEnd::ClientFailed,
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let mut out = Vec::with_capacity(data.len() + 16);
        for chunk in data.chunks(MAX_CONTENT) {
            record(STDIN, chunk, &mut out);
        }
        if writer.write_all(&out).await.is_err() {
            return WriterEnd::BackendFailed;
        }
    }
    let mut end = Vec::with_capacity(8);
    record(STDIN, &[], &mut end);
    if writer.write_all(&end).await.is_err() {
        return WriterEnd::BackendFailed;
    }
    WriterEnd::Done(writer)
}

#[derive(Debug)]
enum HeadFailure {
    TimedOut,
    ClientTimedOut,
    ClientFailed,
    Backend(String),
}

/// Reads FCGI_STDOUT until the blank line that ends the CGI header block.
/// Returns the head and whatever body bytes arrived with it.
async fn read_head(
    backend: &FastCgiBackend,
    reader: &mut RecordReader<ReadHalf<BackendStream>>,
    writer: &mut Writer,
) -> Result<(Vec<u8>, Bytes), HeadFailure> {
    let mut head = Vec::new();
    loop {
        let next = async {
            match writer {
                Writer::Sending(task) => {
                    tokio::select! {
                        biased;
                        record = reader.next() => Ok(record),
                        end = task => Err(end),
                    }
                }
                _ => Ok(reader.next().await),
            }
        };
        let record = match timeout(backend.config.response_timeout, next).await {
            Err(_) => return Err(HeadFailure::TimedOut),
            Ok(Err(end)) => {
                *writer = Writer::Gone;
                match end {
                    Ok(WriterEnd::Done(half)) => *writer = Writer::Sent(half),
                    Ok(WriterEnd::ClientTimedOut) => return Err(HeadFailure::ClientTimedOut),
                    Ok(WriterEnd::ClientFailed) => return Err(HeadFailure::ClientFailed),
                    // The backend may still answer; keep reading.
                    Ok(WriterEnd::BackendFailed) | Err(_) => {}
                }
                continue;
            }
            Ok(Ok(record)) => record,
        };
        let record = match record {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(HeadFailure::Backend(
                    "connection closed before a response head".to_owned(),
                ));
            }
            Err(error) => return Err(HeadFailure::Backend(format!("reading failed: {error}"))),
        };
        match record.kind {
            STDOUT => {
                head.extend_from_slice(&record.content);
                if let Some(end) = head_end(&head) {
                    let rest = Bytes::copy_from_slice(&head[end..]);
                    head.truncate(end);
                    return Ok((head, rest));
                }
                if head.len() > MAX_RESPONSE_HEAD {
                    return Err(HeadFailure::Backend(format!(
                        "response head exceeds {MAX_RESPONSE_HEAD} bytes"
                    )));
                }
            }
            STDERR => log_stderr(&backend.config.label, &record.content),
            END_REQUEST => {
                return Err(HeadFailure::Backend(if head.is_empty() {
                    "the script ended without output".to_owned()
                } else {
                    "the script ended inside its response head".to_owned()
                }));
            }
            _ => {}
        }
    }
}

fn head_end(head: &[u8]) -> Option<usize> {
    let crlf = head
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4);
    let lf = head
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|at| at + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn log_stderr(label: &str, content: &[u8]) {
    let text = String::from_utf8_lossy(content);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        errln!("FastCGI {label} stderr: {}", line.trim_end());
    }
}

/// Turns a CGI header block into a response (RFC 3875 6.2-6.3).
fn parse_head(head: &[u8]) -> Result<HandlerResponse, String> {
    let text = std::str::from_utf8(head).map_err(|_| "response head is not UTF-8".to_owned())?;
    let mut response = Response::new(ResponseBody::buffered(Bytes::new()));
    let mut status = None;
    let mut location = false;
    for line in text.split('\n').map(|line| line.trim_end_matches('\r')) {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("header line without a colon: {line:?}"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value
                .get(..3)
                .and_then(|code| code.parse::<u16>().ok())
                .and_then(|code| StatusCode::from_u16(code).ok())
                .ok_or_else(|| format!("invalid Status {value:?}"))?;
            status = Some(code);
            continue;
        }
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        if is_hop_by_hop(&name) || name == header::CONTENT_LENGTH || name == header::SERVER {
            continue;
        }
        if name == header::LOCATION {
            location = true;
        }
        let value =
            HeaderValue::from_str(value).map_err(|_| format!("invalid value for {name}"))?;
        response.headers_mut().append(name, value);
    }
    *response.status_mut() = status.unwrap_or(if location {
        StatusCode::FOUND
    } else {
        StatusCode::OK
    });
    response
        .headers_mut()
        .insert(header::SERVER, HeaderValue::from_static(SERVER_NAME));
    Ok(response)
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// The response body: FCGI_STDOUT after the head, until FCGI_END_REQUEST.
/// It owns the exchange slot and the request-body writer.
pub(crate) struct FastCgiBody {
    /// `None` only once the body has finished.
    reader: Option<RecordReader<ReadHalf<BackendStream>>>,
    pending: Option<Bytes>,
    deadline: ProgressDeadline,
    writer: Writer,
    slot: Option<Slot>,
    finished: bool,
}

impl fmt::Debug for FastCgiBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FastCgiBody")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// What `finish_if_arrived` found already received.
enum Arrived {
    /// The whole body and END_REQUEST: the exchange is over.
    Whole(Bytes),
    /// The response failed before anything was sent to the client.
    Failed,
    /// More is coming; the body streams, holding what arrived so far.
    Partly,
}

impl FastCgiBody {
    /// Reads, without waiting, the records that have already arrived. A
    /// response that ends within them -- the common case for a small page,
    /// whose END_REQUEST arrives with its output -- completes the exchange
    /// here: the connection returns to its server's idle list and the slot is
    /// released before the response is even handed to Hyper, instead of after
    /// Hyper has written the head and polled the body to its end.
    fn finish_if_arrived(&mut self) -> Arrived {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut collected = BytesMut::new();
        loop {
            match Pin::new(&mut *self).poll_frame(&mut cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        collected.extend_from_slice(&data);
                    }
                    if collected.len() > EARLY_BODY_LIMIT {
                        break;
                    }
                }
                Poll::Ready(None) => return Arrived::Whole(collected.freeze()),
                Poll::Ready(Some(Err(_))) => return Arrived::Failed,
                Poll::Pending => break,
            }
        }
        if !collected.is_empty() {
            self.pending = Some(collected.freeze());
        }
        Arrived::Partly
    }

    fn end(&mut self) {
        self.finished = true;
        self.reader = None;
        self.writer.abort();
        drop(self.slot.take());
    }

    /// END_REQUEST was read. The connection is kept only when nothing
    /// follows it in the buffer and the whole request was sent; the slot is
    /// released after the connection is back in the idle list, so the next
    /// exchange on this server finds it.
    fn complete(&mut self, cx: &mut Context<'_>) {
        if let Some(slot) = &self.slot
            && slot.backend.config.keep_connections
            && let Some(reader) = self.reader.take_if(|reader| !reader.has_buffered())
            && let Some(write_half) = self.writer.poll_sent(cx)
        {
            let stream = reader.into_inner().unsplit(write_half).into_inner();
            slot.backend.keep_idle(slot.server, stream);
        }
        self.end();
    }

    fn fail(&mut self, what: String) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if let Some(slot) = &self.slot {
            slot.backend.record_failure(
                &slot.backend.counters.backend_failures,
                format_args!("response body failed after commit: {what}"),
            );
        }
        self.end();
        Poll::Ready(Some(Err(io::Error::other(what))))
    }
}

impl Body for FastCgiBody {
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
        if let Some(pending) = this.pending.take() {
            return Poll::Ready(Some(Ok(Frame::data(pending))));
        }
        loop {
            let Some(reader) = this.reader.as_mut() else {
                return Poll::Ready(None);
            };
            match reader.poll_record(cx) {
                Poll::Pending => {
                    return match this.deadline.poll_stalled(cx) {
                        Poll::Ready(()) => {
                            if let Some(slot) = &this.slot {
                                slot.backend
                                    .counters
                                    .timeouts
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            this.fail("no response bytes within the response timeout".to_owned())
                        }
                        Poll::Pending => Poll::Pending,
                    };
                }
                Poll::Ready(Ok(Some(record))) => {
                    this.deadline.progress();
                    match record.kind {
                        STDOUT if !record.content.is_empty() => {
                            return Poll::Ready(Some(Ok(Frame::data(record.content))));
                        }
                        STDERR => {
                            if let Some(slot) = &this.slot {
                                log_stderr(&slot.backend.config.label, &record.content);
                            }
                        }
                        END_REQUEST => {
                            this.complete(cx);
                            return Poll::Ready(None);
                        }
                        _ => {}
                    }
                }
                Poll::Ready(Ok(None)) => {
                    return this
                        .fail("connection closed before the end of the response".to_owned());
                }
                Poll::Ready(Err(error)) => return this.fail(format!("reading failed: {error}")),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl Drop for FastCgiBody {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

#[cfg(test)]
mod tests;
