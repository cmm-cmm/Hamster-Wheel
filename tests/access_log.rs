//! End-to-end behaviour of `[server.access_log]`.
//!
//! The records are what an operator will have after an incident, so these
//! tests read the actual file the server wrote rather than an in-process
//! buffer: one line per request, the fields an investigation needs, nothing a
//! client sent in a header, and a truthful mark when a response did not
//! finish.

use std::{
    fs,
    io::{self},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hamster_wheel::{access_log, config, server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

/// A test's own directory, kept on failure so a record can be read by hand.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/access-log-tests")
            .join(format!("{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(directory.join("public")).unwrap();
        Self(directory)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Kept when a test fails, because the record that failed it is the
        // evidence. Removed otherwise: these directories hold megabytes of
        // payload and log, and nothing reads them again.
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// A running server, its log and the means to stop both.
struct Served {
    address: SocketAddr,
    log_path: PathBuf,
    log: Arc<access_log::AccessLog>,
    writer: access_log::LogWriter,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<io::Result<()>>,
}

impl Served {
    /// Stops the server, flushes the writer and returns the written records.
    ///
    /// Dropping every sink is what closes the queue, so the writer can only
    /// be awaited after the serving task has ended.
    async fn finish(self) -> Vec<String> {
        let Self {
            log_path,
            log,
            writer,
            shutdown,
            task,
            ..
        } = self;
        let _ = shutdown.send(());
        let served = timeout(Duration::from_secs(10), task)
            .await
            .expect("the serving task should end")
            .expect("the serving task should not panic");
        served.expect("serving should end cleanly");
        drop(log);
        timeout(Duration::from_secs(10), writer.finish())
            .await
            .expect("the log writer should flush");
        read_records(&log_path)
    }
}

fn read_records(path: &Path) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Starts one cleartext listener with the given `[server.access_log]` body and
/// location table.
async fn serve(scratch: &Scratch, access_log_body: &str, locations: &str) -> Served {
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n\n[server.access_log]\n{access_log_body}\n\n\
         [[hosts]]\nname = \"localhost\"\naliases = [\"127.0.0.1\"]\n\n{locations}"
    );
    let runtime = Arc::new(
        config::compile_str(&source, &scratch.0).expect("the test configuration should compile"),
    );
    let settings = runtime
        .access_log()
        .expect("the test configuration asked for a log");
    let log_path = settings
        .path()
        .expect("these tests log to a file")
        .to_path_buf();
    let (log, writer) = access_log::start(settings).expect("the log should open");
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::serve_listeners(
        vec![server::Listener::cleartext(socket)],
        runtime.router_arc(),
        runtime.http_limits(),
        Some(Arc::clone(&log)),
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    Served {
        address,
        log_path,
        log,
        writer,
        shutdown,
        task,
    }
}

const STATIC_LOCATION: &str =
    "[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"public\"\n";

/// One request on its own connection, answered in full.
async fn request(address: SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut answer = Vec::new();
    timeout(Duration::from_secs(10), stream.read_to_end(&mut answer))
        .await
        .expect("the server should answer")
        .unwrap();
    String::from_utf8_lossy(&answer).into_owned()
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
}

/// The value of a `"key":value` field, as written in the record.
fn field(record: &str, key: &str) -> String {
    let marker = format!("\"{key}\":");
    let start = record
        .find(&marker)
        .unwrap_or_else(|| panic!("{key} missing from {record}"))
        + marker.len();
    let rest = &record[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"').expect("a closing quote");
        return quoted[..end].to_owned();
    }
    let end = rest
        .find([',', '}'])
        .expect("a field separator or the end of the record");
    rest[..end].to_owned()
}

/// Requests this many at once, which is the shape a burst arrives in.
const BURST: usize = 200;

/// Sends `count` concurrent requests and requires every one to be answered.
async fn answer_a_burst(address: SocketAddr, count: usize) {
    let mut clients = Vec::with_capacity(count);
    for _ in 0..count {
        clients.push(tokio::spawn(async move {
            request(address, &get("/index.html")).await
        }));
    }
    for client in clients {
        let answer = client.await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    }
}

#[tokio::test]
async fn each_request_becomes_one_json_line_an_operator_can_read() {
    let scratch = Scratch::new("records");
    fs::write(scratch.file("public/index.html"), "<p>hamster</p>").unwrap();
    let served = serve(&scratch, "path = \"access.jsonl\"", STATIC_LOCATION).await;

    let ok = request(served.address, &get("/index.html")).await;
    assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
    let missing = request(served.address, &get("/nowhere.html")).await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");

    let records = served.finish().await;
    assert_eq!(records.len(), 2, "{records:?}");
    let (first, second) = (&records[0], &records[1]);

    assert_eq!(field(first, "kind"), "access");
    assert_eq!(field(first, "id"), "1");
    assert_eq!(field(first, "method"), "GET");
    assert_eq!(field(first, "target"), "/index.html");
    assert_eq!(field(first, "host"), "localhost");
    assert_eq!(field(first, "version"), "HTTP/1.1");
    assert_eq!(field(first, "scheme"), "http");
    assert_eq!(field(first, "status"), "200");
    assert_eq!(field(first, "bytes"), "14");
    assert_eq!(field(first, "complete"), "true");
    assert!(!first.contains("\"error\""), "{first}");
    // The peer is the real socket, and the timings are present and finite.
    assert!(field(first, "peer").starts_with("127.0.0.1:"), "{first}");
    assert!(field(first, "head_us").parse::<u64>().is_ok(), "{first}");
    assert!(field(first, "total_us").parse::<u64>().is_ok(), "{first}");
    assert!(field(first, "ts_ms").parse::<u64>().unwrap() > 1_600_000_000_000);

    // A second request is a second record with its own id, and an error
    // status carries the reason the server chose it.
    assert_eq!(field(second, "id"), "2");
    assert_eq!(field(second, "status"), "404");
    assert_eq!(field(second, "error"), "Not Found");
}

#[tokio::test]
async fn nothing_a_client_sent_in_a_header_reaches_the_log() {
    let scratch = Scratch::new("secrets");
    fs::write(scratch.file("public/index.html"), "ok").unwrap();
    let served = serve(&scratch, "path = \"access.jsonl\"", STATIC_LOCATION).await;

    let answered = request(
        served.address,
        "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\
         Authorization: Bearer super-secret-token\r\n\
         Cookie: session=super-secret-cookie\r\n\
         User-Agent: hamster-test/1.0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(answered.starts_with("HTTP/1.1 200 OK"), "{answered}");

    let records = served.finish().await;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    for secret in [
        "super-secret-token",
        "super-secret-cookie",
        "Bearer",
        "hamster-test/1.0",
    ] {
        assert!(!record.contains(secret), "{secret} reached {record}");
    }
    // The authority is the one header value a record carries, and the router
    // has already validated it.
    assert_eq!(field(record, "host"), "localhost");
}

#[tokio::test]
async fn a_query_string_is_logged_only_when_asked_for_and_credentials_are_replaced() {
    let scratch = Scratch::new("query");
    fs::write(scratch.file("public/index.html"), "ok").unwrap();
    let target = "/index.html?page=2&token=super-secret-token";

    let omitted = serve(&scratch, "path = \"omitted.jsonl\"", STATIC_LOCATION).await;
    request(omitted.address, &get(target)).await;
    let records = omitted.finish().await;
    assert_eq!(field(&records[0], "target"), "/index.html");

    let included = serve(
        &scratch,
        "path = \"included.jsonl\"\ninclude_query = true",
        STATIC_LOCATION,
    )
    .await;
    request(included.address, &get(target)).await;
    let records = included.finish().await;
    let record = &records[0];
    assert_eq!(field(record, "target"), "/index.html?page=2&token=REDACTED");
    assert!(!record.contains("super-secret-token"), "{record}");
}

#[tokio::test]
async fn a_failing_backend_records_the_reason_the_status_was_chosen() {
    let scratch = Scratch::new("upstream");
    // A port that resolves but refuses: compilation only resolves the
    // address, so the failure happens where an operator would see it.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_address = closed.local_addr().unwrap();
    drop(closed);
    let locations = format!(
        "[[hosts.locations]]\npath = \"/\"\nhandler = \"proxy\"\n\n\
         [hosts.locations.proxy]\nupstream = \"http://{closed_address}\"\nconnect_timeout = 2\n"
    );
    let served = serve(&scratch, "path = \"access.jsonl\"", &locations).await;

    let answered = request(served.address, &get("/anything")).await;
    assert!(answered.starts_with("HTTP/1.1 50"), "{answered}");

    let records = served.finish().await;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert!(field(record, "status").starts_with("50"), "{record}");
    // The server's own label for the failure, not an invented one.
    assert!(!field(record, "error").is_empty(), "{record}");
}

#[tokio::test]
async fn a_response_the_client_abandoned_is_recorded_as_incomplete() {
    let scratch = Scratch::new("abandoned");
    // Larger than any socket buffer, so the response cannot be written before
    // the client goes away.
    let payload = vec![b'h'; 2 * 1_024 * 1_024];
    fs::write(scratch.file("public/big.bin"), &payload).unwrap();
    let served = serve(&scratch, "path = \"access.jsonl\"", STATIC_LOCATION).await;

    let mut stream = TcpStream::connect(served.address).await.unwrap();
    stream.write_all(get("/big.bin").as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut head = [0_u8; 64];
    let read = timeout(Duration::from_secs(10), stream.read(&mut head))
        .await
        .expect("the server should start answering")
        .unwrap();
    assert!(read > 0);
    // Gone before the body finished.
    drop(stream);

    let records = served.finish().await;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(field(record, "status"), "200");
    assert_eq!(
        field(record, "complete"),
        "false",
        "an abandoned response was recorded as complete: {record}"
    );
    let bytes = field(record, "bytes").parse::<u64>().unwrap();
    assert!(
        bytes < payload.len() as u64,
        "the whole body cannot have been sent: {record}"
    );
}

#[tokio::test]
async fn a_burst_within_the_queue_loses_no_record() {
    let scratch = Scratch::new("burst");
    fs::write(scratch.file("public/index.html"), "ok").unwrap();
    let served = serve(&scratch, "path = \"access.jsonl\"", STATIC_LOCATION).await;

    answer_a_burst(served.address, BURST).await;
    assert_eq!(
        served.log.dropped(),
        0,
        "records were dropped under a burst"
    );
    assert_eq!(served.log.failed(), 0, "records could not be written");

    let records = served.finish().await;
    assert_eq!(records.len(), BURST, "a record was lost");
    assert!(
        records
            .iter()
            .all(|record| field(record, "status") == "200"),
        "every request was answered 200"
    );
}

#[tokio::test]
async fn a_queue_too_small_for_a_burst_drops_records_and_says_so_in_the_log() {
    let scratch = Scratch::new("overflow");
    fs::write(scratch.file("public/index.html"), "ok").unwrap();
    // The smallest queue the configuration allows, against a burst that is
    // deliberately larger: the point is what the server does when the log
    // cannot keep up, which must never be to make the client wait.
    let served = serve(
        &scratch,
        "path = \"access.jsonl\"
queue_capacity = 16",
        STATIC_LOCATION,
    )
    .await;

    answer_a_burst(served.address, BURST).await;
    let dropped = served.log.dropped();
    assert!(dropped > 0, "this burst is meant to overflow the queue");
    assert_eq!(served.log.failed(), 0);

    let records = served.finish().await;
    let access: Vec<&String> = records
        .iter()
        .filter(|record| field(record, "kind") == "access")
        .collect();
    let notes: Vec<&String> = records
        .iter()
        .filter(|record| field(record, "kind") == "log_dropped")
        .collect();
    // Every request is accounted for: written plus dropped is the traffic.
    assert_eq!(
        access.len() + dropped as usize,
        BURST,
        "records went missing without being counted: {} written, {dropped} dropped",
        access.len()
    );
    // And the gap is visible in the log itself, not only in a counter.
    assert!(!notes.is_empty(), "no drop was reported in the log");
    let reported: u64 = notes
        .iter()
        .map(|note| field(note, "count").parse::<u64>().unwrap())
        .sum();
    assert!(
        reported > 0 && reported <= dropped,
        "reported {reported} of {dropped} drops"
    );
}
