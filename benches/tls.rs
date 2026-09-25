//! TLS listener release measurements: HTTPS against cleartext HTTP served by
//! the same Hamster process, handshake cost, and memory under idle, stalled
//! and slow TLS clients.
//!
//! Hamster runs as the release binary in its own process, so its CPU time and
//! private memory are measured apart from the load generator. Build release
//! first:
//!
//! ```text
//! cargo build --release --target x86_64-pc-windows-msvc
//! cargo bench --target x86_64-pc-windows-msvc --bench tls --features profiling -- <suite>
//! ```
//!
//! Suites: `all` (default), `matrix`, `handshake`, `memory`, `slow`.
//! `HAMSTER_BINARY` overrides the binary. `CLEARTEXT_ONLY=1` omits the TLS
//! listener, which lets a binary that predates TLS run the cleartext rows of
//! `matrix` and `handshake` as an A/B baseline.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, client::conn::http1::SendRequest};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
};
use tokio_rustls::{
    TlsConnector,
    rustls::{
        ClientConfig, RootCertStore,
        client::Resumption,
        pki_types::{CertificateDer, ServerName},
    },
};

const MIB: f64 = 1_024.0 * 1_024.0;
const RUNS: usize = 3;

#[tokio::main(flavor = "multi_thread", worker_threads = 6)]
async fn main() -> io::Result<()> {
    let suite = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .unwrap_or_else(|| "all".to_owned());
    let executable = hamster_executable();
    if !executable.is_file() {
        eprintln!(
            "{} not found; run `cargo build --release --target x86_64-pc-windows-msvc` first",
            executable.display()
        );
        std::process::exit(2);
    }
    let fixture = Fixture::create()?;
    println!("# TLS listener benchmark");
    println!();
    println!(
        "binary: {}; bench runtime: 6 worker threads; runs per cell: {RUNS}; medians reported; TLS listener: {}",
        executable.display(),
        if cleartext_only() { "omitted" } else { "on" }
    );
    match suite.as_str() {
        "all" => {
            matrix(&fixture).await?;
            handshake(&fixture).await?;
            memory(&fixture).await?;
            slow(&fixture).await?;
        }
        "matrix" => matrix(&fixture).await?,
        "handshake" => handshake(&fixture).await?,
        "memory" => memory(&fixture).await?,
        "slow" => slow(&fixture).await?,
        other => {
            eprintln!("unknown suite {other:?}; expected all|matrix|handshake|memory|slow");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn hamster_executable() -> PathBuf {
    std::env::var_os("HAMSTER_BINARY").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/x86_64-pc-windows-msvc/release/hamster-wheel.exe")
        },
        PathBuf::from,
    )
}

fn cleartext_only() -> bool {
    std::env::var_os("CLEARTEXT_ONLY").is_some_and(|value| value == "1")
}

// ----- Scenarios ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    const fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

fn schemes() -> &'static [Scheme] {
    if cleartext_only() {
        &[Scheme::Http]
    } else {
        &[Scheme::Http, Scheme::Https]
    }
}

struct Run {
    latencies: Vec<Duration>,
    bytes: u64,
    elapsed: Duration,
    cpu_seconds: f64,
}

impl Run {
    fn rps(&self) -> f64 {
        self.latencies.len() as f64 / self.elapsed.as_secs_f64()
    }

    fn mib_per_second(&self) -> f64 {
        self.bytes as f64 / MIB / self.elapsed.as_secs_f64()
    }

    fn cpu_us_per_request(&self) -> f64 {
        self.cpu_seconds * 1_000_000.0 / self.latencies.len().max(1) as f64
    }

    fn percentile(&self, quantile: f64) -> f64 {
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        if sorted.is_empty() {
            return f64::NAN;
        }
        let index = ((sorted.len() as f64 * quantile).ceil() as usize).clamp(1, sorted.len()) - 1;
        sorted[index].as_secs_f64() * 1_000.0
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn print_runs(label: &str, scheme: Scheme, concurrency: usize, runs: &[Run]) {
    let join = |value: fn(&Run) -> f64, digits: usize| {
        runs.iter()
            .map(|run| format!("{:.*}", digits, value(run)))
            .collect::<Vec<_>>()
            .join(" / ")
    };
    println!(
        "| {label} | {} | {concurrency} | {:.0} ({}) | {:.1} | {:.3} | {:.3} | {:.1} | {} |",
        scheme.label(),
        median(runs.iter().map(Run::rps).collect()),
        join(Run::rps, 0),
        median(runs.iter().map(Run::mib_per_second).collect()),
        median(runs.iter().map(|run| run.percentile(0.50)).collect()),
        median(runs.iter().map(|run| run.percentile(0.99)).collect()),
        median(runs.iter().map(Run::cpu_us_per_request).collect()),
        runs.iter().map(|run| run.latencies.len()).sum::<usize>(),
    );
}

async fn matrix(fixture: &Fixture) -> io::Result<()> {
    println!();
    println!("## Keep-alive GET: cleartext versus TLS on one process");
    println!();
    println!(
        "Each worker holds one keep-alive connection for the window (3 s; 4 s for 64 MiB) and issues back-to-back GETs of a static file. Server CPU is Hamster's user+kernel time over the window."
    );
    println!();
    println!(
        "| Payload | Scheme | C | RPS median (runs) | MiB/s | p50 ms | p99 ms | Hamster CPU us/req | requests |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for (label, path, window) in [
        ("GET 23 B", "/tiny.txt", Duration::from_secs(3)),
        ("GET 1 MiB", "/1m.bin", Duration::from_secs(3)),
        ("GET 64 MiB", "/64m.bin", Duration::from_secs(4)),
    ] {
        for concurrency in [1, 32] {
            for &scheme in schemes() {
                let mut runs = Vec::new();
                for _ in 0..RUNS {
                    let hamster = Hamster::start(fixture).await?;
                    warm(&hamster, fixture, scheme).await?;
                    runs.push(load(&hamster, fixture, scheme, path, concurrency, window).await);
                }
                print_runs(label, scheme, concurrency, &runs);
            }
        }
    }
    Ok(())
}

async fn handshake(fixture: &Fixture) -> io::Result<()> {
    println!();
    println!("## New connection per request");
    println!();
    println!(
        "Every request opens a connection, sends `GET /tiny.txt` reads the response and then resets the connection (zero linger), so neither side accumulates TIME_WAIT across tens of thousands of connections. `https` performs a full handshake each time (client resumption disabled); `https resumed` lets the client resume a TLS 1.3 session. Latency includes TCP connect and the handshake, not the close. Window 3 s."
    );
    println!();
    println!(
        "| Mode | Scheme | C | connections/s median (runs) | MiB/s | p50 ms | p99 ms | Hamster CPU us/conn | connections |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for concurrency in [1, 8] {
        let mut modes = vec![("new", Scheme::Http, false)];
        if !cleartext_only() {
            modes.push(("new full", Scheme::Https, false));
            modes.push(("new resumed", Scheme::Https, true));
        }
        for (label, scheme, resumed) in modes {
            let mut runs = Vec::new();
            for _ in 0..RUNS {
                let hamster = Hamster::start(fixture).await?;
                warm(&hamster, fixture, scheme).await?;
                runs.push(
                    load_new_connections(
                        &hamster,
                        fixture,
                        scheme,
                        resumed,
                        concurrency,
                        Duration::from_secs(3),
                    )
                    .await,
                );
            }
            print_runs(label, scheme, concurrency, &runs);
        }
    }
    Ok(())
}

async fn memory(fixture: &Fixture) -> io::Result<()> {
    println!();
    println!("## Memory per connection");
    println!();
    println!(
        "Idle: N keep-alive connections, each after one `GET /tiny.txt`, then held open. Stalled: each client uses a 64 KiB receive buffer, requests `/1g.bin` (1 GiB) and never reads. Private bytes are sampled 1.5 s after the last client is in place, inside the 30 s write-progress deadline."
    );
    println!();
    println!(
        "| State | Scheme | N | Hamster private MiB (runs) | delta vs N=0 MiB | per connection KiB |"
    );
    println!("|---|---|---:|---:|---:|---:|");
    for state in ["idle", "stalled"] {
        for &scheme in schemes() {
            let mut baseline = None;
            let counts: &[usize] = if state == "idle" {
                &[0, 256, 1_024]
            } else {
                &[0, 1, 16, 64]
            };
            for &count in counts {
                let mut samples = Vec::new();
                for _ in 0..RUNS {
                    let hamster = Hamster::start(fixture).await?;
                    warm(&hamster, fixture, scheme).await?;
                    let held = hold(&hamster, fixture, scheme, state, count).await?;
                    tokio::time::sleep(Duration::from_millis(1_500)).await;
                    samples.push(hamster.memory().0 as f64 / MIB);
                    drop(held);
                }
                let joined = samples
                    .iter()
                    .map(|value| format!("{value:.2}"))
                    .collect::<Vec<_>>()
                    .join(" / ");
                let private = median(samples);
                let base = *baseline.get_or_insert(private);
                let delta = private - base;
                let per = if count == 0 {
                    0.0
                } else {
                    delta * 1_024.0 / count as f64
                };
                println!(
                    "| {state} | {} | {count} | {private:.2} ({joined}) | {delta:.2} | {per:.1} |",
                    scheme.label()
                );
            }
        }
    }
    Ok(())
}

async fn slow(fixture: &Fixture) -> io::Result<()> {
    println!();
    println!("## One 1 GiB download: slow reader and full speed");
    println!();
    println!(
        "Slow: the client reads 64 KiB then sleeps 8 ms (about 7 MiB/s) for 6 s while Hamster's private bytes are sampled every 250 ms. Full: the client reads `/1g.bin` to the end as fast as it can; peak is the process's peak private commit. One fresh process per run."
    );
    println!();
    println!(
        "| Reader | Scheme | MiB read (runs) | Hamster private MiB before | max during | peak commit MiB | seconds |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|");
    for reader in ["slow", "full"] {
        for &scheme in schemes() {
            let mut read = Vec::new();
            let mut before = Vec::new();
            let mut during = Vec::new();
            let mut peak = Vec::new();
            let mut seconds = Vec::new();
            for _ in 0..RUNS {
                let hamster = Hamster::start(fixture).await?;
                warm(&hamster, fixture, scheme).await?;
                before.push(hamster.memory().0 as f64 / MIB);
                let (bytes, max, elapsed) =
                    download(&hamster, fixture, scheme, reader == "slow").await?;
                read.push(bytes as f64 / MIB);
                during.push(max as f64 / MIB);
                peak.push(hamster.memory().1 as f64 / MIB);
                seconds.push(elapsed.as_secs_f64());
            }
            let joined = read
                .iter()
                .map(|value| format!("{value:.0}"))
                .collect::<Vec<_>>()
                .join(" / ");
            println!(
                "| {reader} | {} | {:.0} ({joined}) | {:.2} | {:.2} | {:.2} | {:.2} |",
                scheme.label(),
                median(read),
                median(before),
                median(during),
                median(peak),
                median(seconds)
            );
        }
    }
    Ok(())
}

// ----- Load ---------------------------------------------------------------------

async fn warm(hamster: &Hamster, fixture: &Fixture, scheme: Scheme) -> io::Result<()> {
    let mut sender = open(hamster, fixture, scheme, false).await?;
    for _ in 0..16 {
        get(&mut sender, "/tiny.txt", false).await?;
    }
    Ok(())
}

async fn load(
    hamster: &Hamster,
    fixture: &Fixture,
    scheme: Scheme,
    path: &'static str,
    concurrency: usize,
    window: Duration,
) -> Run {
    let mut senders = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        senders.push(
            open(hamster, fixture, scheme, false)
                .await
                .expect("open connection"),
        );
    }
    let cpu_before = hamster.cpu_seconds();
    let started = Instant::now();
    let deadline = started + window;
    let mut tasks = Vec::with_capacity(concurrency);
    for mut sender in senders {
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            let mut bytes = 0;
            while Instant::now() < deadline {
                let begun = Instant::now();
                match get(&mut sender, path, false).await {
                    Ok(read) => {
                        bytes += read;
                        latencies.push(begun.elapsed());
                    }
                    Err(error) => panic!("request failed: {error}"),
                }
            }
            (latencies, bytes)
        }));
    }
    collect(tasks, started, cpu_before, hamster).await
}

async fn load_new_connections(
    hamster: &Hamster,
    fixture: &Fixture,
    scheme: Scheme,
    resumed: bool,
    concurrency: usize,
    window: Duration,
) -> Run {
    let cpu_before = hamster.cpu_seconds();
    let started = Instant::now();
    let deadline = started + window;
    let address = hamster.address(scheme);
    let connector = fixture.connector(resumed);
    let mut tasks = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let connector = connector.clone();
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            let mut bytes = 0;
            while Instant::now() < deadline {
                let begun = Instant::now();
                bytes += one_request(address, scheme, &connector)
                    .await
                    .expect("request on a new connection");
                latencies.push(begun.elapsed());
            }
            (latencies, bytes)
        }));
    }
    collect(tasks, started, cpu_before, hamster).await
}

/// One raw `GET /tiny.txt` on a fresh connection, then an abortive close.
///
/// A Hyper client would shut its write side down when dropped, leaving the
/// client socket in TIME_WAIT; tens of thousands of those exhaust Windows'
/// 16,384 ephemeral ports. Dropping a zero-linger socket resets it instead.
async fn one_request(
    address: SocketAddr,
    scheme: Scheme,
    connector: &TlsConnector,
) -> io::Result<u64> {
    let stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;
    stream.set_zero_linger()?;
    match scheme {
        Scheme::Http => exchange(stream).await,
        Scheme::Https => {
            let stream = connector
                .connect(ServerName::try_from("localhost").expect("name"), stream)
                .await?;
            exchange(stream).await
        }
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) -> io::Result<u64> {
    stream
        .write_all(b"GET /tiny.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| io::Error::other("no content-length"))?;
        if buffer.len() >= end + 4 + length {
            return Ok(length as u64);
        }
    }
}

async fn collect(
    tasks: Vec<tokio::task::JoinHandle<(Vec<Duration>, u64)>>,
    started: Instant,
    cpu_before: f64,
    hamster: &Hamster,
) -> Run {
    let mut latencies = Vec::new();
    let mut bytes = 0;
    for task in tasks {
        let (mut part, read) = task.await.expect("load task");
        latencies.append(&mut part);
        bytes += read;
    }
    Run {
        latencies,
        bytes,
        elapsed: started.elapsed(),
        cpu_seconds: hamster.cpu_seconds() - cpu_before,
    }
}

async fn open(
    hamster: &Hamster,
    fixture: &Fixture,
    scheme: Scheme,
    resumed: bool,
) -> io::Result<SendRequest<Empty<Bytes>>> {
    connect(hamster.address(scheme), scheme, &fixture.connector(resumed)).await
}

async fn connect(
    address: SocketAddr,
    scheme: Scheme,
    connector: &TlsConnector,
) -> io::Result<SendRequest<Empty<Bytes>>> {
    let stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;
    match scheme {
        Scheme::Http => handshake_http(stream).await,
        Scheme::Https => {
            let stream = connector
                .connect(ServerName::try_from("localhost").expect("name"), stream)
                .await?;
            handshake_http(stream).await
        }
    }
}

async fn handshake_http<S>(stream: S) -> io::Result<SendRequest<Empty<Bytes>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
}

async fn get(
    sender: &mut SendRequest<Empty<Bytes>>,
    path: &'static str,
    close: bool,
) -> io::Result<u64> {
    let mut request = Request::get(path).header("host", "localhost");
    if close {
        request = request.header("connection", "close");
    }
    // The dispatcher may not have processed the previous body's end yet.
    sender.ready().await.map_err(io::Error::other)?;
    let response = sender
        .send_request(request.body(Empty::new()).expect("request"))
        .await
        .map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!("status {}", response.status())));
    }
    let mut body = response.into_body();
    let mut read = 0;
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(io::Error::other)?.into_data() {
            read += data.len() as u64;
        }
    }
    Ok(read)
}

/// A connection kept open only so that Hamster holds its state.
type Held = Box<dyn Send>;

async fn hold(
    hamster: &Hamster,
    fixture: &Fixture,
    scheme: Scheme,
    state: &str,
    count: usize,
) -> io::Result<Vec<Held>> {
    let mut held: Vec<Held> = Vec::with_capacity(count);
    for _ in 0..count {
        if state == "idle" {
            let mut sender = open(hamster, fixture, scheme, false).await?;
            get(&mut sender, "/tiny.txt", false).await?;
            held.push(Box::new(sender));
            continue;
        }
        let socket = TcpSocket::new_v4()?;
        socket.set_recv_buffer_size(64 * 1_024)?;
        let stream = socket.connect(hamster.address(scheme)).await?;
        let request = b"GET /1g.bin HTTP/1.1\r\nHost: localhost\r\n\r\n";
        match scheme {
            Scheme::Http => {
                let mut stream = stream;
                stream.write_all(request).await?;
                held.push(Box::new(stream));
            }
            Scheme::Https => {
                let mut stream = fixture
                    .connector(false)
                    .connect(ServerName::try_from("localhost").expect("name"), stream)
                    .await?;
                stream.write_all(request).await?;
                held.push(Box::new(stream));
            }
        }
    }
    Ok(held)
}

/// Downloads `/1g.bin`, slowly for 6 s or completely, sampling Hamster's
/// private bytes every 250 ms. Returns bytes read, the maximum sample and the
/// elapsed time.
async fn download(
    hamster: &Hamster,
    fixture: &Fixture,
    scheme: Scheme,
    slow: bool,
) -> io::Result<(u64, u64, Duration)> {
    let stream = TcpStream::connect(hamster.address(scheme)).await?;
    let request = b"GET /1g.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let started = Instant::now();
    let pid = hamster.child.id();
    let (bytes, max) = match scheme {
        Scheme::Http => {
            let mut stream = stream;
            stream.write_all(request).await?;
            read_body(&mut stream, slow, pid).await?
        }
        Scheme::Https => {
            let mut stream = fixture
                .connector(false)
                .connect(ServerName::try_from("localhost").expect("name"), stream)
                .await?;
            stream.write_all(request).await?;
            read_body(&mut stream, slow, pid).await?
        }
    };
    Ok((bytes, max, started.elapsed()))
}

async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    slow: bool,
    pid: u32,
) -> io::Result<(u64, u64)> {
    let mut buffer = vec![0_u8; 64 * 1_024];
    let started = Instant::now();
    let mut next_sample = started;
    let mut max = 0;
    let mut total = 0_u64;
    loop {
        if Instant::now() >= next_sample {
            max = max.max(windows::process_private_bytes(pid).0);
            next_sample += Duration::from_millis(250);
        }
        if slow && started.elapsed() >= Duration::from_secs(6) {
            break;
        }
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if slow {
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
    }
    Ok((total, max))
}

// ----- Fixture and process ------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    certificate: CertificateDer<'static>,
}

impl Fixture {
    fn create() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("hamster-tls-bench-{}", std::process::id()));
        let root = dir.join("root");
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("tiny.txt"), b"{\"message\":\"tls ok\"}\n\n\n")?;
        for (name, size) in [
            ("1m.bin", 1_u64 << 20),
            ("64m.bin", 64 << 20),
            ("1g.bin", 1 << 30),
        ] {
            let file = std::fs::File::create(root.join(name))?;
            file.set_len(size)?;
        }
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .map_err(io::Error::other)?;
        std::fs::write(dir.join("localhost.crt"), generated.cert.pem())?;
        std::fs::write(
            dir.join("localhost.key"),
            generated.signing_key.serialize_pem(),
        )?;
        Ok(Self {
            dir,
            certificate: generated.cert.der().clone(),
        })
    }

    fn connector(&self, resumption: bool) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.certificate.clone())
            .expect("trust bench certificate");
        let mut config = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("client versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        if !resumption {
            config.resumption = Resumption::disabled();
        }
        TlsConnector::from(Arc::new(config))
    }

    fn config(&self, http: SocketAddr, https: SocketAddr) -> String {
        let literal = |path: &Path| format!("'{}'", path.display());
        let tls = if cleartext_only() {
            String::new()
        } else {
            format!(
                "\n[server.tls]\nlisten = \"{https}\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\n",
                literal(&self.dir.join("localhost.crt")),
                literal(&self.dir.join("localhost.key")),
            )
        };
        format!(
            "[server]\nlisten = \"{http}\"\nmax_connections = 4096\n{tls}\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = {}\n",
            literal(&self.dir.join("root"))
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Hamster {
    child: Child,
    http: SocketAddr,
    https: SocketAddr,
    config: PathBuf,
}

impl Hamster {
    async fn start(fixture: &Fixture) -> io::Result<Self> {
        let free = || -> io::Result<SocketAddr> {
            let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            probe.local_addr()
        };
        let (http, https) = (free()?, free()?);
        let config = fixture.dir.join(format!("hamster-{}.toml", http.port()));
        std::fs::write(&config, fixture.config(http, https))?;
        let child = Command::new(hamster_executable())
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let hamster = Self {
            child,
            http,
            https,
            config,
        };
        let started = Instant::now();
        loop {
            let http_ready = TcpStream::connect(http).await.is_ok();
            let https_ready = cleartext_only() || TcpStream::connect(https).await.is_ok();
            if http_ready && https_ready {
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                return Err(io::Error::other("Hamster did not start listening"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(hamster)
    }

    const fn address(&self, scheme: Scheme) -> SocketAddr {
        match scheme {
            Scheme::Http => self.http,
            Scheme::Https => self.https,
        }
    }

    fn cpu_seconds(&self) -> f64 {
        windows::process_cpu_seconds(self.child.id())
    }

    /// Current and peak private commit, in bytes.
    fn memory(&self) -> (u64, u64) {
        windows::process_private_bytes(self.child.id())
    }
}

impl Drop for Hamster {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config);
    }
}

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, mem::MaybeUninit};

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x0010;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetProcessTimes(
            handle: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(
            handle: *mut c_void,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> i32;
    }

    pub fn process_cpu_seconds(pid: u32) -> f64 {
        // SAFETY: the handle is checked and closed; the out-parameters are
        // read only after the call reports success.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            assert!(!handle.is_null(), "OpenProcess failed");
            let mut creation = MaybeUninit::uninit();
            let mut exit = MaybeUninit::uninit();
            let mut kernel = MaybeUninit::<FileTime>::uninit();
            let mut user = MaybeUninit::<FileTime>::uninit();
            let success = GetProcessTimes(
                handle,
                creation.as_mut_ptr(),
                exit.as_mut_ptr(),
                kernel.as_mut_ptr(),
                user.as_mut_ptr(),
            );
            CloseHandle(handle);
            assert!(success != 0, "GetProcessTimes failed");
            let (kernel, user) = (kernel.assume_init(), user.assume_init());
            let ticks = ((u64::from(kernel.high) << 32) | u64::from(kernel.low))
                + ((u64::from(user.high) << 32) | u64::from(user.low));
            ticks as f64 / 10_000_000.0
        }
    }

    pub fn process_private_bytes(pid: u32) -> (u64, u64) {
        // SAFETY: as above; `cb` tells the API which structure it fills.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
            assert!(!handle.is_null(), "OpenProcess failed");
            let mut counters = ProcessMemoryCountersEx {
                cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
                ..ProcessMemoryCountersEx::default()
            };
            let success = K32GetProcessMemoryInfo(handle, &mut counters, counters.cb);
            CloseHandle(handle);
            assert!(success != 0, "K32GetProcessMemoryInfo failed");
            (
                counters.private_usage as u64,
                counters.peak_pagefile_usage as u64,
            )
        }
    }
}

#[cfg(not(windows))]
mod windows {
    pub fn process_cpu_seconds(_pid: u32) -> f64 {
        f64::NAN
    }

    pub fn process_private_bytes(_pid: u32) -> (u64, u64) {
        (0, 0)
    }
}
