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
        Arc,
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

/// The compiled `[server.access_log]` section.
///
/// Absent from the configuration means no queue, no writer task and no
/// per-request work at all.
#[derive(Clone, Debug)]
pub struct Settings {
    path: Option<PathBuf>,
    include_query: bool,
    queue_capacity: usize,
}

impl Settings {
    pub(crate) fn new(path: Option<PathBuf>, include_query: bool, queue_capacity: usize) -> Self {
        Self {
            path,
            include_query,
            queue_capacity,
        }
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
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            Sink::File(tokio::fs::File::from_std(file))
        }
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
    Dropped { at_ms: u64, count: u64 },
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
    File(tokio::fs::File),
}

impl Sink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => {
                stdout.write_all(bytes).await?;
                stdout.flush().await
            }
            Self::File(file) => {
                file.write_all(bytes).await?;
                file.flush().await
            }
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
        Settings::new(None, include_query, MIN_QUEUE_CAPACITY)
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
