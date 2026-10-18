//! What a published generation changes, and what it deliberately does not.
//!
//! The 0.8 exit evidence for reload is that "accepted requests finish under
//! the old generation and new requests use the published generation", so these
//! tests keep one connection open across a reload and read the access log to
//! see which generation each request was served under. The log is the only
//! place that answer is written down, which is why the two features are tested
//! together.

use std::{
    fs,
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
    time::timeout,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/reload-tests")
            .join(format!("{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(directory.join("one")).unwrap();
        fs::create_dir_all(directory.join("two")).unwrap();
        fs::write(directory.join("one/index.html"), "one").unwrap();
        fs::write(directory.join("two/index.html"), "two").unwrap();
        Self(directory)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// A configuration whose only resource is a listener, serving one static root.
fn source(root: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n\n[server.access_log]\npath = \"access.jsonl\"\n\n\
         [[hosts]]\nname = \"localhost\"\naliases = [\"127.0.0.1\"]\n\n\
         [[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"{root}\"\n"
    )
}

/// One request on a connection that is kept open afterwards.
async fn keep_alive_request(stream: &mut TcpStream, target: &str) -> String {
    let head = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut answer = Vec::new();
    let mut byte = [0_u8; 1];
    // Reads to the end of a known three-byte body without closing, so the
    // connection stays usable for the next request.
    timeout(Duration::from_secs(10), async {
        while !ends_a_small_response(&answer) {
            let read = stream.read(&mut byte).await.unwrap();
            if read == 0 {
                break;
            }
            answer.push(byte[0]);
        }
    })
    .await
    .expect("the server should answer");
    String::from_utf8_lossy(&answer).into_owned()
}

/// Whether the bytes so far are a complete response with a 3-byte body.
fn ends_a_small_response(answer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(answer);
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.len() >= 3,
        None => false,
    }
}

fn record_generations(log: &Path) -> Vec<(u64, String)> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"kind\":\"access\""))
        .map(|line| {
            let generation = field(line, "generation").parse().unwrap();
            (generation, field(line, "target"))
        })
        .collect()
}

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
    let end = rest.find([',', '}']).expect("a separator");
    rest[..end].to_owned()
}

struct Served {
    address: SocketAddr,
    generations: Arc<server::Generations>,
    log_path: PathBuf,
    log: Arc<access_log::AccessLog>,
    writer: access_log::LogWriter,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Served {
    async fn finish(self) -> Vec<(u64, String)> {
        let _ = self.shutdown.send(());
        timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the serving task should end")
            .unwrap()
            .unwrap();
        drop(self.log);
        timeout(Duration::from_secs(10), self.writer.finish())
            .await
            .expect("the log writer should flush");
        record_generations(&self.log_path)
    }
}

async fn serve(config_path: &Path) -> Served {
    let runtime = Arc::new(config::load_file(config_path).expect("the configuration compiles"));
    let settings = runtime.access_log().expect("the test logs to a file");
    let log_path = settings.path().expect("a file").to_path_buf();
    let (log, writer) = access_log::start(settings).expect("the log opens");
    let generations = server::Generations::new(Arc::clone(&runtime));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::serve_listeners(
        vec![server::Listener::cleartext(socket)],
        server::Serving::Published(Arc::clone(&generations)),
        Some(Arc::clone(&log)),
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    Served {
        address,
        generations,
        log_path,
        log,
        writer,
        shutdown,
        task,
    }
}

#[tokio::test]
async fn a_connection_keeps_the_generation_it_was_accepted_under() {
    let scratch = Scratch::new("inflight");
    let config = scratch.file("hamster.toml");
    fs::write(&config, source("one")).unwrap();
    let served = serve(&config).await;

    // Generation 1 serves the first root.
    let mut kept = TcpStream::connect(served.address).await.unwrap();
    let first = keep_alive_request(&mut kept, "/index.html").await;
    assert!(first.ends_with("one"), "{first}");

    // A change that owns nothing is published in place.
    fs::write(&config, source("two")).unwrap();
    let published = served.generations.reload().await.expect("the reload");
    assert_eq!(published.generation(), 2);

    // The connection accepted under generation 1 still serves generation 1,
    // which is what lets a request finish under the configuration that
    // admitted it.
    let second = keep_alive_request(&mut kept, "/index.html").await;
    assert!(
        second.ends_with("one"),
        "an accepted connection changed generation mid-flight: {second}"
    );
    drop(kept);

    // A connection accepted afterwards sees the published generation.
    let mut fresh = TcpStream::connect(served.address).await.unwrap();
    let third = keep_alive_request(&mut fresh, "/index.html").await;
    assert!(third.ends_with("two"), "{third}");
    drop(fresh);

    let records = served.finish().await;
    assert_eq!(
        records
            .iter()
            .map(|(generation, _)| *generation)
            .collect::<Vec<_>>(),
        vec![1, 1, 2],
        "every request records the generation that served it: {records:?}"
    );
}

#[tokio::test]
async fn a_refused_reload_leaves_the_serving_generation_untouched() {
    let scratch = Scratch::new("refused");
    let config = scratch.file("hamster.toml");
    fs::write(&config, source("one")).unwrap();
    let served = serve(&config).await;

    // A file that no longer compiles.
    fs::write(&config, "[server]\nlisten = \"not-an-address\"\n").unwrap();
    let refusal = served.generations.reload().await;
    assert!(refusal.is_err(), "an invalid file was published");
    assert_eq!(served.generations.current().generation(), 1);

    // A valid file that would have to replace the listener.
    fs::write(&config, source("two").replace("127.0.0.1:0", "127.0.0.1:1")).unwrap();
    match served.generations.reload().await {
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("server.listen"), "{message}");
        }
        Ok(_) => panic!("a listener change was published in place"),
    }
    assert_eq!(served.generations.current().generation(), 1);

    // And the server is still serving the generation it started with.
    let mut stream = TcpStream::connect(served.address).await.unwrap();
    let answer = keep_alive_request(&mut stream, "/index.html").await;
    assert!(answer.ends_with("one"), "{answer}");
    drop(stream);

    let records = served.finish().await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, 1);
}
