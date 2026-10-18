//! Structured per-request logging that cannot block the request path.
//!
//! A request-path write to a file or to stdout is a syscall whose duration
//! belongs to whoever is reading the other end. The hot path therefore only
//! builds one small owned record and offers it to a bounded queue; a single
//! writer task formats JSON and performs every write. A full queue is never
//! waited on: the record is dropped and counted, and the count is reported in
//! the log itself and through the control endpoint, so a gap in the log can
//! never be mistaken for a gap in traffic.
//!
//! Records are JSON Lines: one object per line, `kind` naming the record type.
//! The format is hand-written rather than taken from a serializer, because a
//! log line is the one place where an injected newline or quote must not be
//! able to forge a second record; `escape_json` is tested for exactly that.
//!
//! Headers are deliberately not logged. The only header value that reaches a
//! record is the authority, which the router has already validated, and query
//! strings are omitted unless an operator asks for them, in which case the
//! values of credential-shaped keys are replaced. Nothing here writes a
//! cookie, an `Authorization` value or a request body.

use std::{
    fmt::Write as _,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use hyper::{
    Request, Response, StatusCode,
    body::{Body, Frame, SizeHint},
    header::HOST,
};
use tokio::{
    io::{AsyncWriteExt, Stdout},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{metrics::ServerMetrics, response_body::ResponseBody, transport::ConnectionInfo};

/// Queue depth when an operator does not choose one.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1_024;
/// Smallest useful queue: below this a single burst is all drops.
pub const MIN_QUEUE_CAPACITY: usize = 16;
/// Largest queue accepted. The queue holds owned records, so its depth is a
/// memory budget, not a free dial.
pub const MAX_QUEUE_CAPACITY: usize = 65_536;

/// Records formatted and written in one batch.
///
/// A batch bounds how much the writer holds at once and how often it writes;
/// it does not bound the queue, which the configuration owns.
const MAX_BATCH: usize = 256;

/// Query keys whose values are replaced when query logging is enabled.
///
/// Matching is case-insensitive and exact: a key is either a credential or it
/// is not, and a substring rule would silently redact ordinary keys such as
/// `keyword` while still missing `x-token`. Operators who need a different
/// policy should leave query logging off.
const REDACTED_QUERY_KEYS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "code",
    "id_token",
    "key",
    "passwd",
    "password",
    "pwd",
    "refresh_token",
    "secret",
    "session",
    "sessionid",
    "sig",
    "signature",
    "token",
];

/// What replaces a redacted query value.
const REDACTED: &str = "REDACTED";

/// The lifecycle events this server reports.
///
/// A fixed list rather than free strings at each call site: an event name is
/// something an operator greps for and a dashboard groups by, so a typo must
/// not be able to invent a new one.
pub mod event {
    pub const STARTED: &str = "started";
    pub const LISTENER_BOUND: &str = "listener_bound";
    pub const WORKER_READY: &str = "worker_ready";
    pub const RELOAD_PUBLISHED: &str = "reload_published";
    pub const RELOAD_REFUSED: &str = "reload_refused";
    pub const CERTIFICATES_RELOADED: &str = "certificates_reloaded";
    pub const CERTIFICATE_RELOAD_REFUSED: &str = "certificate_reload_refused";
    pub const DRAINING: &str = "draining";
    pub const LISTENERS_DRAINED: &str = "listeners_drained";
    pub const STOPPED: &str = "stopped";
}

/// One value in a lifecycle record.
///
/// Typed at the call site so the writer can render each kind correctly and
/// escape text in the one place escaping is tested.
#[derive(Debug)]
pub enum Detail {
    Text(Box<str>),
    Number(u64),
    Signed(i64),
    Seconds(f64),
    Flag(bool),
}

impl From<&str> for Detail {
    fn from(value: &str) -> Self {
        Self::Text(Box::from(value))
    }
}

impl From<String> for Detail {
    fn from(value: String) -> Self {
        Self::Text(value.into_boxed_str())
    }
}

impl From<u64> for Detail {
    fn from(value: u64) -> Self {
        Self::Number(value)
    }
}

impl From<u32> for Detail {
    fn from(value: u32) -> Self {
        Self::Number(u64::from(value))
    }
}

impl From<usize> for Detail {
    fn from(value: usize) -> Self {
        Self::Number(value as u64)
    }
}

impl From<i64> for Detail {
    fn from(value: i64) -> Self {
        Self::Signed(value)
    }
}

impl From<f64> for Detail {
    fn from(value: f64) -> Self {
        Self::Seconds(value)
    }
}

impl From<bool> for Detail {
    fn from(value: bool) -> Self {
        Self::Flag(value)
    }
}

/// The sink lifecycle records go to, once a server has started one.
///
/// A process has one log, and lifecycle events happen in places that have no
/// reason to be handed a sink: a pool supervisor, a signal handler, the
/// shutdown path. This is set once before any of them runs and only read
/// afterwards, so it is a relaxed load rather than a lock, and it is
/// deliberately **not** how the request path reaches the log: that keeps its
/// own `Arc`, so no global is read per request.
///
/// It holds a `Weak` on purpose. An owning handle here would keep the queue
/// open after the server dropped its own sink at shutdown, so the writer
/// would wait for a sender that nothing was going to close and the process
/// would never exit. The sink's lifetime stays exactly where it was: with
/// `server::run`.
static LIFECYCLE: std::sync::OnceLock<Weak<AccessLog>> = std::sync::OnceLock::new();

/// Makes lifecycle records go to this sink. Later calls are ignored.
pub fn install_lifecycle(sink: &Arc<AccessLog>) {
    let _ = LIFECYCLE.set(Arc::downgrade(sink));
}

/// Submits one lifecycle record, or does nothing when no log is configured.
pub fn lifecycle_record(event: &'static str, fields: Vec<(&'static str, Detail)>) {
    if let Some(sink) = LIFECYCLE.get().and_then(Weak::upgrade) {
        sink.submit(Record::Lifecycle {
            at_ms: unix_millis(),
            event,
            fields,
        });
    }
}

/// Smallest rotation size accepted.
///
/// Below this a single batch could rotate several times, which would turn the
/// log into a stream of nearly empty archives.
pub const MIN_ROTATE_BYTES: u64 = 4 * 1_024;

/// Most archives kept beside the active file.
pub const MAX_KEEP: usize = 64;

/// Archives kept when rotation is on and the operator did not choose.
pub const DEFAULT_KEEP: usize = 4;

/// The compiled `[server.access_log]` section.
///
/// Absent from the configuration means no queue, no writer task and no
/// per-request work at all.
#[derive(Clone, Debug)]
pub struct Settings {
    path: Option<PathBuf>,
    include_query: bool,
    queue_capacity: usize,
    /// Size at which the active file is rotated, when rotation is on.
    rotate_bytes: Option<u64>,
    /// Archives kept beside the active file.
    keep: usize,
}

impl Settings {
    pub(crate) fn new(
        path: Option<PathBuf>,
        include_query: bool,
        queue_capacity: usize,
        rotate_bytes: Option<u64>,
        keep: usize,
    ) -> Self {
        Self {
            path,
            include_query,
            queue_capacity,
            rotate_bytes,
            keep,
        }
    }

    /// The size at which the active file is rotated, if it is.
    pub fn rotate_bytes(&self) -> Option<u64> {
        self.rotate_bytes
    }

    /// How many archives are kept beside the active file.
    pub fn keep(&self) -> usize {
        self.keep
    }

    /// The log file, or `None` when records go to stdout.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn include_query(&self) -> bool {
        self.include_query
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// The startup line's description of where records go.
    pub fn target_description(&self) -> String {
        match &self.path {
            None => "stdout".to_owned(),
            Some(path) => path.display().to_string(),
        }
    }

    /// How rotation is reported at startup and by the control endpoint.
    pub fn rotation_description(&self) -> String {
        match self.rotate_bytes {
            None => "off".to_owned(),
            Some(bytes) => format!("{bytes} bytes, {} kept", self.keep),
        }
    }
}

/// Counters an operator can ask about while the server runs.
#[derive(Debug, Default)]
struct Counters {
    written: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
}

/// The sink the request path holds.
pub struct AccessLog {
    sender: mpsc::Sender<Record>,
    counters: Arc<Counters>,
    next_id: AtomicU64,
    include_query: bool,
}

/// The writer task's handle, kept by the server so shutdown can flush.
pub struct LogWriter {
    task: JoinHandle<()>,
}

impl LogWriter {
    /// Waits for the writer to drain the queue and flush.
    ///
    /// The caller drops every sink clone first, which is what ends the writer
    /// loop. Records already queued are written: a stop must not silently
    /// discard the last requests a server served.
    pub async fn finish(self) {
        let _ = self.task.await;
    }
}

/// Opens the target and starts the writer task.
///
/// The file is opened here, before listeners bind, so a log path that cannot
/// be written is a startup failure rather than a surprise under load.
pub fn start(settings: &Settings) -> io::Result<(Arc<AccessLog>, LogWriter)> {
    let sink = match settings.path() {
        None => Sink::Stdout(tokio::io::stdout()),
        Some(path) => Sink::File(FileSink::open(
            path.to_path_buf(),
            settings.rotate_bytes(),
            settings.keep(),
        )?),
    };
    let (sender, receiver) = mpsc::channel(settings.queue_capacity());
    let counters = Arc::new(Counters::default());
    let task = tokio::spawn(write_records(receiver, sink, Arc::clone(&counters)));
    let log = AccessLog {
        sender,
        counters,
        next_id: AtomicU64::new(1),
        include_query: settings.include_query(),
    };
    Ok((Arc::new(log), LogWriter { task }))
}

impl AccessLog {
    /// Captures what is known before the handler runs.
    pub(crate) fn begin<B>(
        &self,
        request: &Request<B>,
        connection: ConnectionInfo,
        generation: u64,
    ) -> RequestStart {
        let path = request.uri().path();
        let target = match (self.include_query, request.uri().query()) {
            (true, Some(query)) => {
                let mut target = String::with_capacity(path.len() + query.len() + 1);
                target.push_str(path);
                target.push('?');
                redact_query(query, &mut target);
                target.into_boxed_str()
            }
            _ => Box::from(path),
        };
        let host = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .or_else(|| request.uri().host())
            .map(Box::from);
        RequestStart {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            generation,
            at_ms: unix_millis(),
            method: Box::from(request.method().as_str()),
            target,
            host,
            version: version_name(request.version()),
            scheme: connection.scheme().as_str(),
            peer: connection.transport().peer_addr(),
        }
    }

    /// Offers a finished record to the writer, or counts it as dropped.
    fn submit(&self, record: Record) {
        if self.sender.try_send(record).is_err() {
            // Full, or the writer is gone. Either way the request path waits
            // for nothing: a log is a report about traffic, never a condition
            // for serving it.
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records that are already counted but not yet written.
    pub fn queued(&self) -> usize {
        self.sender.max_capacity() - self.sender.capacity()
    }

    pub fn written(&self) -> u64 {
        self.counters.written.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> u64 {
        self.counters.failed.load(Ordering::Relaxed)
    }
}

/// Everything known about a request before its response exists.
pub(crate) struct RequestStart {
    id: u64,
    /// The configuration generation that admitted this request, so a record
    /// can be placed in the configuration it was served under.
    generation: u64,
    at_ms: u64,
    method: Box<str>,
    target: Box<str>,
    host: Option<Box<str>>,
    version: &'static str,
    scheme: &'static str,
    peer: SocketAddr,
}

/// One finished request, or one note from the log about itself.
enum Record {
    Access(Box<AccessRecord>),
    /// Something the server did, rather than something a client asked for.
    Lifecycle {
        at_ms: u64,
        event: &'static str,
        fields: Vec<(&'static str, Detail)>,
    },
    Dropped {
        at_ms: u64,
        count: u64,
    },
}

struct AccessRecord {
    start: RequestStart,
    status: u16,
    head_micros: u64,
    total_micros: u64,
    bytes: u64,
    complete: bool,
    error: Option<&'static str>,
}

impl Record {
    fn write_json(&self, out: &mut String) {
        match self {
            Self::Lifecycle {
                at_ms,
                event,
                fields,
            } => {
                let _ = write!(out, r#"{{"kind":"lifecycle","ts_ms":{at_ms},"event":""#);
                escape_json(event, out);
                out.push('"');
                for (key, value) in fields {
                    let _ = write!(out, r#","{key}":"#);
                    match value {
                        Detail::Text(text) => {
                            out.push('"');
                            escape_json(text, out);
                            out.push('"');
                        }
                        Detail::Number(number) => {
                            let _ = write!(out, "{number}");
                        }
                        Detail::Signed(number) => {
                            let _ = write!(out, "{number}");
                        }
                        Detail::Seconds(seconds) => {
                            let _ = write!(out, "{seconds:.3}");
                        }
                        Detail::Flag(flag) => {
                            let _ = write!(out, "{flag}");
                        }
                    }
                }
                out.push('}');
            }
            Self::Dropped { at_ms, count } => {
                let _ = write!(
                    out,
                    r#"{{"kind":"log_dropped","ts_ms":{at_ms},"count":{count}}}"#
                );
            }
            Self::Access(record) => {
                let start = &record.start;
                let _ = write!(
                    out,
                    r#"{{"kind":"access","ts_ms":{},"id":{},"generation":{},"method":""#,
                    start.at_ms, start.id, start.generation
                );
                escape_json(&start.method, out);
                out.push_str(r#"","target":""#);
                escape_json(&start.target, out);
                out.push('"');
                if let Some(host) = &start.host {
                    out.push_str(r#","host":""#);
                    escape_json(host, out);
                    out.push('"');
                }
                let _ = write!(
                    out,
                    r#","version":"{}","scheme":"{}","peer":"{}","status":{},"head_us":{},"total_us":{},"bytes":{},"complete":{}"#,
                    start.version,
                    start.scheme,
                    start.peer,
                    record.status,
                    record.head_micros,
                    record.total_micros,
                    record.bytes,
                    record.complete
                );
                if let Some(error) = record.error {
                    out.push_str(r#","error":""#);
                    escape_json(error, out);
                    out.push('"');
                }
                out.push('}');
            }
        }
    }
}

/// The reason an error response was produced, carried on the response itself.
///
/// Every error status in this crate comes from one constructor
/// (`handler::simple_response`), whose body text is already the reason. An
/// extension therefore reaches the log without threading an argument through
/// routing, static files, FastCGI, the worker pool and the proxy.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorReason(pub(crate) &'static str);

/// Wraps the response body so a request is observed when the body ends.
///
/// This is the one place a finished request is reported, which is why the
/// access log and the metrics counters are both fed from here: they cannot
/// disagree about a request's status, size, duration or completeness.
///
/// `Plain` exists so that a server with neither consumer keeps the body it
/// already had: the only cost is one enum branch per frame.
pub(crate) enum MaybeObserved {
    Plain(ResponseBody),
    Observed(ObservedBody),
}

impl MaybeObserved {
    /// Wraps `response` when anything observes requests, leaving its body
    /// untouched when nothing does.
    pub(crate) fn wrap(
        started: Instant,
        log: Option<&Arc<AccessLog>>,
        start: Option<RequestStart>,
        metrics: Option<&Arc<ServerMetrics>>,
        response: Response<ResponseBody>,
    ) -> Response<Self> {
        let logging = log.zip(start);
        if logging.is_none() && metrics.is_none() {
            return response.map(Self::Plain);
        }
        let head_micros = started.elapsed().as_micros() as u64;
        let status = response.status();
        // A body of known length is complete once those bytes are written:
        // hyper stops polling a sized body there and drops it without a final
        // end-of-stream, so `complete` cannot be read from the poll alone.
        let expected = response.body().size_hint().exact();
        let mut response = response;
        let error = response
            .extensions_mut()
            .remove::<ErrorReason>()
            .map(|reason| reason.0)
            .or_else(|| default_error_reason(status));
        response.map(|body| {
            Self::Observed(ObservedBody {
                inner: body,
                // Boxed so that `MaybeObserved::Plain` is not sized for the
                // state an observed response carries: every response,
                // including one from a server that observes nothing, is moved
                // through this enum.
                pending: Some(Box::new(Pending {
                    log: logging.as_ref().map(|(log, _)| Arc::clone(log)),
                    start: logging.map(|(_, start)| start),
                    metrics: metrics.map(Arc::clone),
                    at: started,
                    status: status.as_u16(),
                    head_micros,
                    expected,
                    error,
                })),
                bytes: 0,
                ended: false,
            })
        })
    }
}

/// A status that is an error but reached the client without a reason.
///
/// Rather than log an error with an empty cause, say which class it was. A
/// 404 for a path that does not exist is not a server fault and gets nothing.
fn default_error_reason(status: StatusCode) -> Option<&'static str> {
    if status.is_server_error() {
        Some("server error")
    } else {
        None
    }
}

impl Body for MaybeObserved {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::Plain(body) => Pin::new(body).poll_frame(cx),
            Self::Observed(body) => Pin::new(body).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Plain(body) => body.is_end_stream(),
            Self::Observed(body) => body.inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Plain(body) => body.size_hint(),
            Self::Observed(body) => body.inner.size_hint(),
        }
    }
}

struct Pending {
    log: Option<Arc<AccessLog>>,
    start: Option<RequestStart>,
    metrics: Option<Arc<ServerMetrics>>,
    /// Request start, kept separately because metrics are observed without a
    /// log record and therefore without a `RequestStart`.
    at: Instant,
    status: u16,
    head_micros: u64,
    /// Body length when the response declared one.
    expected: Option<u64>,
    error: Option<&'static str>,
}

/// A body that counts what the client was actually sent.
///
/// The record is emitted when the body ends, fails or is dropped, so a
/// response whose connection died halfway is logged with the bytes that
/// really went out and `complete: false`, rather than as a clean 200.
pub(crate) struct ObservedBody {
    inner: ResponseBody,
    pending: Option<Box<Pending>>,
    bytes: u64,
    ended: bool,
}

impl ObservedBody {
    fn emit(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let total_micros = pending.at.elapsed().as_micros() as u64;
        let complete = self.ended || pending.expected == Some(self.bytes);
        if let Some(metrics) = &pending.metrics {
            metrics.request_finished(pending.status, self.bytes, complete, total_micros);
        }
        if let (Some(log), Some(start)) = (pending.log, pending.start) {
            log.submit(Record::Access(Box::new(AccessRecord {
                start,
                status: pending.status,
                head_micros: pending.head_micros,
                total_micros,
                bytes: self.bytes,
                complete,
                error: pending.error,
            })));
        }
    }
}

impl Body for ObservedBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                // The client did not receive a complete response; the record
                // says so and keeps the byte count that did go out.
                this.emit();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.ended = true;
                this.emit();
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        // A dropped body is the normal shape of a client that went away, and
        // of a connection ended by shutdown. Both must still be logged.
        self.emit();
    }
}

enum Sink {
    Stdout(Stdout),
    File(FileSink),
}

/// The active log file, its path and how large it is.
///
/// Rotation is the writer task's work: it is the only thing that holds this
/// handle, so closing, renaming and reopening cannot race with a record being
/// written, and nothing on the request path waits for any of it.
struct FileSink {
    file: tokio::fs::File,
    path: PathBuf,
    written: u64,
    rotate_bytes: Option<u64>,
    keep: usize,
    /// A rotation that failed is reported once; records keep going to the file
    /// that is already open, because a log that stops writing is worse than a
    /// log that grew past its bound.
    reported_failure: bool,
}

impl FileSink {
    fn open(path: PathBuf, rotate_bytes: Option<u64>, keep: usize) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // Appending to an existing file counts what is already there, so a
        // restart cannot grow the file past its bound.
        let written = file.metadata().map(|data| data.len()).unwrap_or(0);
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            path,
            written,
            rotate_bytes,
            keep,
            reported_failure: false,
        })
    }

    /// Bytes that still fit under the bound, or `None` when there is none.
    fn room(&self) -> Option<u64> {
        self.rotate_bytes
            .map(|limit| limit.saturating_sub(self.written))
    }

    /// Rotates, reporting a failure once and carrying on.
    ///
    /// A log that stops writing because a rename failed is worse than a log
    /// that grew past its bound, so a failed rotation keeps the open file.
    async fn rotate_or_report(&mut self) {
        match self.rotate().await {
            Ok(()) => self.reported_failure = false,
            Err(error) => {
                if !self.reported_failure {
                    errln!(
                        "access log rotation failed, still writing to {}: {error}",
                        self.path.display()
                    );
                    self.reported_failure = true;
                }
                // Treat the bound as satisfied for now; the next batch tries
                // again rather than rotating on every record.
                self.written = 0;
            }
        }
    }

    async fn rotate(&mut self) -> io::Result<()> {
        // The handle must be closed before the rename on Windows, so the
        // active file is flushed and replaced in one step the writer owns.
        self.file.flush().await?;
        let path = self.path.clone();
        let keep = self.keep;
        tokio::task::spawn_blocking(move || shift_archives(&path, keep))
            .await
            .map_err(io::Error::other)??;
        let reopened = FileSink::open(self.path.clone(), self.rotate_bytes, self.keep)?;
        self.file = reopened.file;
        self.written = reopened.written;
        Ok(())
    }

    /// Writes whole records, rotating at the bound rather than past it.
    ///
    /// `records` is newline-terminated JSON Lines. The split is always at a
    /// record boundary: a record is never divided across two files, and a
    /// single record larger than the bound goes through whole into an empty
    /// file rather than being dropped.
    async fn write(&mut self, records: &[u8]) -> io::Result<()> {
        let mut rest = records;
        while !rest.is_empty() {
            let take = match self.room() {
                None => rest.len(),
                Some(room) => whole_records_within(rest, room),
            };
            if take == 0 {
                if self.written == 0 {
                    // One record is larger than the whole bound.
                    let whole = first_record(rest);
                    self.append(&rest[..whole]).await?;
                    rest = &rest[whole..];
                    continue;
                }
                self.rotate_or_report().await;
                continue;
            }
            self.append(&rest[..take]).await?;
            rest = &rest[take..];
        }
        Ok(())
    }

    async fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes).await?;
        self.file.flush().await?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

/// The length of the longest prefix of `records` that is whole records and
/// fits in `room`.
fn whole_records_within(records: &[u8], room: u64) -> usize {
    let room = usize::try_from(room)
        .unwrap_or(usize::MAX)
        .min(records.len());
    match records[..room].iter().rposition(|byte| *byte == b'\n') {
        Some(index) => index + 1,
        None => 0,
    }
}

/// The length of the first record, including its newline.
fn first_record(records: &[u8]) -> usize {
    records
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(records.len(), |index| index + 1)
}

/// Moves `path` to `path.1`, shifting existing archives up and dropping the
/// oldest beyond `keep`.
///
/// With `keep` of zero the active file is simply removed, which is what an
/// operator asking for no archives means.
fn shift_archives(path: &Path, keep: usize) -> io::Result<()> {
    let archive = |index: usize| {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    };
    if keep == 0 {
        return std::fs::remove_file(path);
    }
    let oldest = archive(keep);
    if oldest.exists() {
        std::fs::remove_file(&oldest)?;
    }
    for index in (1..keep).rev() {
        let from = archive(index);
        if from.exists() {
            std::fs::rename(&from, archive(index + 1))?;
        }
    }
    std::fs::rename(path, archive(1))
}

impl Sink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => {
                stdout.write_all(bytes).await?;
                stdout.flush().await
            }
            Self::File(file) => file.write(bytes).await,
        }
    }
}

/// The only task that formats and writes records.
async fn write_records(
    mut receiver: mpsc::Receiver<Record>,
    mut sink: Sink,
    counters: Arc<Counters>,
) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    let mut buffer = String::with_capacity(8 * 1_024);
    let mut reported_drops = 0_u64;
    let mut reported_failure = false;
    loop {
        let received = receiver.recv_many(&mut batch, MAX_BATCH).await;
        if received == 0 {
            break;
        }
        buffer.clear();
        for record in batch.drain(..) {
            record.write_json(&mut buffer);
            buffer.push('\n');
        }
        // Drops are reported in the log itself, immediately after the records
        // that were queued around them, so the gap is visible where it
        // happened rather than only in a counter.
        let dropped = counters.dropped.load(Ordering::Relaxed);
        if dropped > reported_drops {
            Record::Dropped {
                at_ms: unix_millis(),
                count: dropped - reported_drops,
            }
            .write_json(&mut buffer);
            buffer.push('\n');
            reported_drops = dropped;
        }
        match sink.write(buffer.as_bytes()).await {
            Ok(()) => {
                counters
                    .written
                    .fetch_add(received as u64, Ordering::Relaxed);
            }
            Err(error) => {
                counters
                    .failed
                    .fetch_add(received as u64, Ordering::Relaxed);
                if !reported_failure {
                    // Once: a log target that has gone away would otherwise
                    // report every batch, and this report goes to the same
                    // stream the records may be using.
                    errln!("access log write failed; records are being dropped: {error}");
                    reported_failure = true;
                }
            }
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

fn version_name(version: hyper::Version) -> &'static str {
    match version {
        hyper::Version::HTTP_09 => "HTTP/0.9",
        hyper::Version::HTTP_10 => "HTTP/1.0",
        hyper::Version::HTTP_11 => "HTTP/1.1",
        hyper::Version::HTTP_2 => "HTTP/2.0",
        hyper::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/?",
    }
}

/// Appends `query` with credential-shaped values replaced.
///
/// Separators are preserved so the result is still the client's query, minus
/// the values an operator must not keep in a log file.
fn redact_query(query: &str, out: &mut String) {
    let mut first = true;
    for pair in query.split('&') {
        if !first {
            out.push('&');
        }
        first = false;
        match pair.split_once('=') {
            None => out.push_str(pair),
            Some((key, value)) => {
                out.push_str(key);
                out.push('=');
                if is_redacted_key(key) {
                    out.push_str(REDACTED);
                } else {
                    out.push_str(value);
                }
            }
        }
    }
}

fn is_redacted_key(key: &str) -> bool {
    REDACTED_QUERY_KEYS
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

/// Appends `value` as the inside of a JSON string.
///
/// A record is one line, and a client controls the request target, so an
/// unescaped newline or quote would let a request write its own log record.
fn escape_json(value: &str, out: &mut String) {
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            control if control < '\u{20}' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            ordinary => out.push(ordinary),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;
    use crate::transport::{Scheme, TransportInfo};

    fn escaped(value: &str) -> String {
        let mut out = String::new();
        escape_json(value, &mut out);
        out
    }

    fn redacted(query: &str) -> String {
        let mut out = String::new();
        redact_query(query, &mut out);
        out
    }

    fn connection() -> ConnectionInfo {
        ConnectionInfo::new(
            TransportInfo::new(
                SocketAddr::from((Ipv4Addr::new(127, 0, 0, 2), 51_000)),
                SocketAddr::from((Ipv4Addr::LOCALHOST, 8_080)),
            ),
            Scheme::Http,
        )
    }

    fn settings(include_query: bool) -> Settings {
        Settings::new(None, include_query, MIN_QUEUE_CAPACITY, None, DEFAULT_KEEP)
    }

    fn record_of(start: RequestStart, status: u16, bytes: u64, complete: bool) -> Record {
        Record::Access(Box::new(AccessRecord {
            start,
            status,
            head_micros: 10,
            total_micros: 20,
            bytes,
            complete,
            error: None,
        }))
    }

    #[test]
    fn a_request_target_cannot_forge_a_second_record() {
        // The one escaping that matters: a line break in a client-controlled
        // value would otherwise end the record and start another.
        let forged = escaped("/a\n{\"kind\":\"access\",\"status\":200}");
        assert!(!forged.contains('\n'), "{forged}");
        assert!(forged.starts_with("/a\\n"), "{forged}");
        assert_eq!(escaped(r#"say "hi"\no"#), r#"say \"hi\"\\no"#);
        assert_eq!(escaped("tab\there"), "tab\\there");
        assert_eq!(escaped("bell\u{07}"), "bell\\u0007");
        assert_eq!(escaped("carriage\r"), "carriage\\r");
        // Text outside ASCII is already valid inside a JSON string.
        assert_eq!(escaped("khách/á"), "khách/á");
    }

    #[test]
    fn credential_shaped_query_values_are_replaced_and_others_are_kept() {
        assert_eq!(redacted("page=2&sort=name"), "page=2&sort=name");
        assert_eq!(redacted("token=abc123"), "token=REDACTED");
        assert_eq!(redacted("API_KEY=abc"), "API_KEY=REDACTED");
        assert_eq!(
            redacted("page=2&access_token=abc&q=hamster"),
            "page=2&access_token=REDACTED&q=hamster"
        );
        // A key that merely contains a credential word is not a credential.
        assert_eq!(redacted("keyword=key"), "keyword=key");
        // Shapes a client can send that must not panic or lose separators.
        assert_eq!(redacted("flag"), "flag");
        assert_eq!(redacted("token="), "token=REDACTED");
        assert_eq!(redacted("a=1&&b=2"), "a=1&&b=2");
        assert_eq!(redacted("sig=a=b"), "sig=REDACTED");
    }

    #[tokio::test]
    async fn a_query_string_is_omitted_unless_it_was_asked_for() {
        let (log, _writer) = start(&settings(false)).unwrap();
        let request = Request::builder()
            .uri("/search?q=hamster&token=secret")
            .header(HOST, "localhost")
            .body(())
            .unwrap();
        let start_without = log.begin(&request, connection(), 1);
        assert_eq!(&*start_without.target, "/search");

        let (log, _writer) = super::start(&settings(true)).unwrap();
        let start_with = log.begin(&request, connection(), 1);
        assert_eq!(&*start_with.target, "/search?q=hamster&token=REDACTED");
        assert_eq!(start_with.host.as_deref(), Some("localhost"));
        assert_eq!(&*start_with.method, "GET");
        assert_eq!(start_with.scheme, "http");
    }

    #[tokio::test]
    async fn a_record_is_one_line_of_json_with_the_fields_an_operator_reads() {
        let (log, _writer) = start(&settings(false)).unwrap();
        let request = Request::builder()
            .uri("/index.html")
            .header(HOST, "localhost")
            .body(())
            .unwrap();
        let mut line = String::new();
        record_of(log.begin(&request, connection(), 1), 200, 1_234, true).write_json(&mut line);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.starts_with(r#"{"kind":"access","ts_ms":"#), "{line}");
        for field in [
            r#""id":1"#,
            r#""method":"GET""#,
            r#""target":"/index.html""#,
            r#""host":"localhost""#,
            r#""version":"HTTP/1.1""#,
            r#""scheme":"http""#,
            r#""peer":"127.0.0.2:51000""#,
            r#""status":200"#,
            r#""bytes":1234"#,
            r#""complete":true"#,
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
        // No reason on a success, and never a header value.
        assert!(!line.contains("error"), "{line}");
        assert!(!line.to_ascii_lowercase().contains("cookie"), "{line}");
    }

    #[tokio::test]
    async fn ids_are_unique_per_request_so_records_can_be_correlated() {
        let (log, _writer) = start(&settings(false)).unwrap();
        let request = Request::builder().uri("/").body(()).unwrap();
        let first = log.begin(&request, connection(), 1);
        let second = log.begin(&request, connection(), 1);
        assert_eq!((first.id, second.id), (1, 2));
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_counts_instead_of_waiting() {
        // The writer is never polled here, so the queue can only fill. This is
        // the shape of a log target slower than the traffic.
        let (sender, _receiver) = mpsc::channel(2);
        let log = AccessLog {
            sender,
            counters: Arc::new(Counters::default()),
            next_id: AtomicU64::new(1),
            include_query: false,
        };
        let request = Request::builder().uri("/").body(()).unwrap();
        for _ in 0..5 {
            log.submit(record_of(
                log.begin(&request, connection(), 1),
                200,
                0,
                true,
            ));
        }
        assert_eq!(log.dropped(), 3);
        assert_eq!(log.queued(), 2);
        assert_eq!(log.written(), 0);
    }

    #[test]
    fn a_lifecycle_record_is_one_line_with_its_typed_fields() {
        let mut line = String::new();
        Record::Lifecycle {
            at_ms: 1_700_000_000_000,
            event: event::WORKER_READY,
            fields: vec![
                ("pool", "localhost/php".into()),
                ("pid", 4_321_u32.into()),
                ("startup_ms", 149.9742_f64.into()),
                ("graceful", true.into()),
                ("delta", (-3_i64).into()),
            ],
        }
        .write_json(&mut line);
        assert_eq!(
            line,
            r#"{"kind":"lifecycle","ts_ms":1700000000000,"event":"worker_ready","pool":"localhost/php","pid":4321,"startup_ms":149.974,"graceful":true,"delta":-3}"#
        );
    }

    #[test]
    fn a_lifecycle_value_cannot_forge_a_record() {
        // Reasons come from error text, which can contain anything a path or
        // an operating system put in it.
        let mut line = String::new();
        Record::Lifecycle {
            at_ms: 1,
            event: event::RELOAD_REFUSED,
            fields: vec![("reason", "broke\\n{\"kind\":\"lifecycle\"}".into())],
        }
        .write_json(&mut line);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.ends_with(r#"}"#), "{line}");
        assert_eq!(line.matches(r#""kind""#).count(), 1, "{line}");
    }

    #[test]
    fn rotation_takes_whole_records_up_to_the_bound() {
        let records = b"aaaa\nbbbb\ncccc\n";
        // Room for two whole records takes exactly two.
        assert_eq!(whole_records_within(records, 10), 10);
        assert_eq!(whole_records_within(records, 12), 10);
        assert_eq!(whole_records_within(records, 15), 15);
        // Room for part of a record takes none of it, so the caller rotates.
        assert_eq!(whole_records_within(records, 4), 0);
        assert_eq!(whole_records_within(records, 0), 0);
        // And a record larger than the bound is still measured whole.
        assert_eq!(first_record(records), 5);
        assert_eq!(first_record(b"no newline"), 10);
    }

    #[test]
    fn archives_shift_up_and_the_oldest_is_dropped() {
        let directory = std::env::temp_dir().join(format!(
            "hamster-rotate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("access.jsonl");
        let archive = |index: usize| directory.join(format!("access.jsonl.{index}"));

        std::fs::write(&path, "first").unwrap();
        shift_archives(&path, 2).unwrap();
        assert!(!path.exists(), "the active file was rotated away");
        assert_eq!(std::fs::read_to_string(archive(1)).unwrap(), "first");

        std::fs::write(&path, "second").unwrap();
        shift_archives(&path, 2).unwrap();
        assert_eq!(std::fs::read_to_string(archive(1)).unwrap(), "second");
        assert_eq!(std::fs::read_to_string(archive(2)).unwrap(), "first");

        // The third rotation drops the oldest rather than keeping three.
        std::fs::write(&path, "third").unwrap();
        shift_archives(&path, 2).unwrap();
        assert_eq!(std::fs::read_to_string(archive(1)).unwrap(), "third");
        assert_eq!(std::fs::read_to_string(archive(2)).unwrap(), "second");
        assert!(
            !archive(3).exists(),
            "more archives were kept than asked for"
        );

        // Keeping none means the active file is simply removed.
        std::fs::write(&path, "fourth").unwrap();
        shift_archives(&path, 0).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_dropped_note_reports_how_many_records_were_lost() {
        let mut line = String::new();
        Record::Dropped {
            at_ms: 1_700_000_000_000,
            count: 7,
        }
        .write_json(&mut line);
        assert_eq!(
            line,
            r#"{"kind":"log_dropped","ts_ms":1700000000000,"count":7}"#
        );
    }

    #[test]
    fn only_a_server_fault_gets_a_default_reason() {
        assert_eq!(
            default_error_reason(StatusCode::INTERNAL_SERVER_ERROR),
            Some("server error")
        );
        assert_eq!(default_error_reason(StatusCode::NOT_FOUND), None);
        assert_eq!(default_error_reason(StatusCode::OK), None);
    }
}
