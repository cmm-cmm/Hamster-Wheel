//! Reverse proxy release measurements: direct upstream versus Hamster proxy.
//!
//! The upstream and the load generator run in this process; Hamster runs as
//! the release binary in its own process, so its CPU time and private memory
//! are measured separately from both ends. Build release first:
//!
//! ```text
//! cargo build --release --target x86_64-pc-windows-msvc
//! cargo bench --target x86_64-pc-windows-msvc --bench proxy --features profiling -- <suite>
//! ```
//!
//! Suites: `all` (default: `matrix`, `buffers`, `memory`), `matrix`, `buffers`,
//! `memory`, `probe`, `idle`. `probe` is the buffer-bound reproducer for the
//! Windows loopback stall documented in `doc/REVERSE_PROXY_V1.md`; it reads
//! `PROBE_BUFFERS` (comma-separated bytes, default `65536,417792`) and
//! `PROBE_CONCURRENCY` (default 1).

use std::{
    convert::Infallible,
    io,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Frame, Incoming, SizeHint},
    server::conn::http1 as server_http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpSocket, TcpStream},
};

const TINY: &[u8] = b"{\"message\":\"proxy ok\"}\n";
const MIB: u64 = 1_024 * 1_024;
const CHUNK: usize = 64 * 1_024;
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
    println!("# Reverse proxy benchmark");
    println!();
    println!(
        "binary: {}; bench runtime: 6 worker threads; runs per cell: {RUNS}; medians reported",
        executable.display()
    );
    let upstream = Upstream::start().await?;
    match suite.as_str() {
        "all" => {
            matrix(&upstream).await?;
            buffers(&upstream).await?;
            memory(&upstream).await?;
        }
        "matrix" => matrix(&upstream).await?,
        "buffers" => buffers(&upstream).await?,
        "memory" => memory(&upstream).await?,
        "probe" => probe(&upstream).await?,
        "idle" => idle(&upstream).await?,
        other => {
            eprintln!("unknown suite {other:?}; expected all|matrix|buffers|memory|probe|idle");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn hamster_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/x86_64-pc-windows-msvc/release/hamster-wheel.exe")
}

// ----- Scenarios ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Payload {
    Tiny,
    OneMiB,
    SixtyFourMiB,
    Upload64MiB,
}

impl Payload {
    const fn label(self) -> &'static str {
        match self {
            Self::Tiny => "GET 23 B",
            Self::OneMiB => "GET 1 MiB",
            Self::SixtyFourMiB => "GET 64 MiB",
            Self::Upload64MiB => "POST 64 MiB",
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::Tiny => "/tiny",
            Self::OneMiB => "/1m",
            Self::SixtyFourMiB => "/64m",
            Self::Upload64MiB => "/upload",
        }
    }

    const fn window(self) -> Duration {
        match self {
            Self::Tiny | Self::OneMiB => Duration::from_secs(3),
            Self::SixtyFourMiB | Self::Upload64MiB => Duration::from_secs(4),
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Direct,
    Proxied,
}

struct RunResult {
    requests: u64,
    errors: u64,
    elapsed: Duration,
    latencies: Vec<Duration>,
    hamster_cpu: Option<f64>,
    bench_cpu: f64,
    upstream_connections: u64,
    hamster_private: Option<u64>,
    hamster_peak_private: Option<u64>,
}

async fn matrix(upstream: &Upstream) -> io::Result<()> {
    println!();
    println!(
        "## Matrix: direct upstream vs Hamster proxy (http1_max_buffer_size 64 KiB, max_idle 64)"
    );
    println!();
    println!(
        "| Payload | C | Mode | RPS | p50 ms | p95 ms | p99 ms | Hamster CPU us/req | bench CPU us/req | upstream conns | Hamster private MiB (end / peak) | errors |"
    );
    println!("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---|---:|");
    for payload in [
        Payload::Tiny,
        Payload::OneMiB,
        Payload::SixtyFourMiB,
        Payload::Upload64MiB,
    ] {
        for concurrency in [1, 32, 128] {
            for mode in [Mode::Direct, Mode::Proxied] {
                let mut runs = Vec::new();
                for _ in 0..RUNS {
                    runs.push(measure(upstream, payload, concurrency, mode, 64 * 1_024, 64).await?);
                }
                print_row(payload, concurrency, mode, &runs);
            }
        }
    }
    Ok(())
}

async fn buffers(upstream: &Upstream) -> io::Result<()> {
    println!();
    println!(
        "## Upstream/downstream HTTP/1 buffer bound (server.http1_max_buffer_size), proxied only"
    );
    println!();
    println!(
        "| Buffer | Payload | C | RPS | p50 ms | p99 ms | Hamster CPU us/req | Hamster private MiB (end / peak) | errors |"
    );
    println!("|---:|---|---:|---:|---:|---:|---:|---|---:|");
    for buffer in [16 * 1_024, 64 * 1_024, 417_792] {
        for (payload, concurrency) in [
            (Payload::Tiny, 32),
            (Payload::SixtyFourMiB, 1),
            (Payload::SixtyFourMiB, 32),
            (Payload::Upload64MiB, 1),
        ] {
            let mut runs = Vec::new();
            for _ in 0..RUNS {
                runs.push(
                    measure(upstream, payload, concurrency, Mode::Proxied, buffer, 64).await?,
                );
            }
            let rps = median(runs.iter().map(rps_of).collect());
            let p50 = median(runs.iter().map(|run| percentile(run, 0.50)).collect());
            let p99 = median(runs.iter().map(|run| percentile(run, 0.99)).collect());
            let cpu = median(runs.iter().map(hamster_cpu_per_request).collect());
            let end = median(
                runs.iter()
                    .map(|run| run.hamster_private.unwrap_or(0) as f64 / MIB as f64)
                    .collect(),
            );
            let peak = median(
                runs.iter()
                    .map(|run| run.hamster_peak_private.unwrap_or(0) as f64 / MIB as f64)
                    .collect(),
            );
            let errors: u64 = runs.iter().map(|run| run.errors).sum();
            println!(
                "| {buffer} | {} | {concurrency} | {rps:.1} | {p50:.3} | {p99:.3} | {cpu:.1} | {end:.2} / {peak:.2} | {errors} |",
                payload.label()
            );
        }
    }
    Ok(())
}

/// Connection churn when concurrency exceeds `max_idle`.
async fn idle(upstream: &Upstream) -> io::Result<()> {
    println!();
    println!("## max_idle versus concurrency (GET 23 B, C128)");
    println!();
    println!(
        "| max_idle | RPS | p50 ms | p99 ms | Hamster CPU us/req | new upstream conns in window |"
    );
    println!("|---:|---:|---:|---:|---:|---:|");
    for max_idle in [64, 128] {
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(
                measure(
                    upstream,
                    Payload::Tiny,
                    128,
                    Mode::Proxied,
                    64 * 1_024,
                    max_idle,
                )
                .await?,
            );
        }
        println!(
            "| {max_idle} | {:.1} | {:.3} | {:.3} | {:.1} | {:.0} |",
            median(runs.iter().map(rps_of).collect()),
            median(runs.iter().map(|run| percentile(run, 0.50)).collect()),
            median(runs.iter().map(|run| percentile(run, 0.99)).collect()),
            median(runs.iter().map(hamster_cpu_per_request).collect()),
            median(
                runs.iter()
                    .map(|run| run.upstream_connections as f64)
                    .collect()
            ),
        );
    }
    Ok(())
}

/// Buffer-bound cells at one concurrency, with a direct baseline.
async fn probe(upstream: &Upstream) -> io::Result<()> {
    let concurrency: usize = std::env::var("PROBE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    println!();
    println!("## Probe: transfers by buffer bound, C{concurrency}");
    println!();
    println!(
        "`>=5 ms` is the share of requests that took at least 5 ms: the Windows loopback stall shows up there first."
    );
    println!();
    println!(
        "| Buffer | Payload | Mode | RPS | p50 ms | p95 ms | p99 ms | >=5 ms | Hamster CPU us/req |"
    );
    println!("|---:|---|---|---:|---:|---:|---:|---:|---:|");
    let buffers: Vec<usize> = std::env::var("PROBE_BUFFERS")
        .ok()
        .map(|list| {
            list.split(',')
                .filter_map(|value| value.parse().ok())
                .collect()
        })
        .unwrap_or_else(|| vec![64 * 1_024, 417_792]);
    let payloads = [Payload::OneMiB, Payload::SixtyFourMiB, Payload::Upload64MiB];
    // Direct first: the stall is a property of Windows loopback TCP, and the
    // direct path shows it too, only less often.
    for payload in payloads {
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(measure(upstream, payload, concurrency, Mode::Direct, 64 * 1_024, 64).await?);
        }
        print_probe_row("-", payload, "direct", &runs);
    }
    for buffer in buffers {
        for payload in payloads {
            let mut runs = Vec::new();
            for _ in 0..RUNS {
                runs.push(
                    measure(upstream, payload, concurrency, Mode::Proxied, buffer, 64).await?,
                );
            }
            print_probe_row(&buffer.to_string(), payload, "proxy", &runs);
        }
    }
    Ok(())
}

fn print_probe_row(buffer: &str, payload: Payload, mode: &str, runs: &[RunResult]) {
    let cpu = if mode == "direct" {
        "-".to_owned()
    } else {
        format!(
            "{:.1}",
            median(runs.iter().map(hamster_cpu_per_request).collect())
        )
    };
    println!(
        "| {buffer} | {} | {mode} | {:.1} | {:.3} | {:.3} | {:.3} | {:.1}% | {cpu} |",
        payload.label(),
        median(runs.iter().map(rps_of).collect()),
        median(runs.iter().map(|run| percentile(run, 0.50)).collect()),
        median(runs.iter().map(|run| percentile(run, 0.95)).collect()),
        median(runs.iter().map(|run| percentile(run, 0.99)).collect()),
        median(
            runs.iter()
                .map(|run| share_at_least(run, Duration::from_millis(5)) * 100.0)
                .collect()
        ),
    );
}

async fn measure(
    upstream: &Upstream,
    payload: Payload,
    concurrency: usize,
    mode: Mode,
    buffer: usize,
    max_idle: usize,
) -> io::Result<RunResult> {
    let hamster = match mode {
        Mode::Direct => None,
        Mode::Proxied => Some(Hamster::start(upstream.address, buffer, max_idle).await?),
    };
    let target = hamster
        .as_ref()
        .map_or(upstream.address, |hamster| hamster.address);

    // Warm-up opens every client connection and, through Hamster, the
    // upstream pool, so the measured window sees steady state.
    load(target, payload, concurrency, Duration::from_millis(750)).await;

    let hamster_cpu_before = hamster.as_ref().map(Hamster::cpu_seconds);
    let bench_cpu_before = windows::process_cpu_seconds(std::process::id());
    let connections_before = upstream.connections.load(Ordering::Relaxed);
    let outcome = load(target, payload, concurrency, payload.window()).await;
    let connections_after = upstream.connections.load(Ordering::Relaxed);
    let bench_cpu_after = windows::process_cpu_seconds(std::process::id());
    let hamster_cpu_after = hamster.as_ref().map(Hamster::cpu_seconds);
    let (hamster_private, hamster_peak_private) = hamster
        .as_ref()
        .map(Hamster::memory)
        .map_or((None, None), |(private, peak)| (Some(private), Some(peak)));
    drop(hamster);

    Ok(RunResult {
        requests: outcome.requests,
        errors: outcome.errors,
        elapsed: outcome.elapsed,
        latencies: outcome.latencies,
        hamster_cpu: hamster_cpu_before
            .zip(hamster_cpu_after)
            .map(|(before, after)| after - before),
        bench_cpu: bench_cpu_after - bench_cpu_before,
        upstream_connections: connections_after - connections_before,
        hamster_private,
        hamster_peak_private,
    })
}

fn rps_of(run: &RunResult) -> f64 {
    run.requests as f64 / run.elapsed.as_secs_f64()
}

fn hamster_cpu_per_request(run: &RunResult) -> f64 {
    run.hamster_cpu
        .map_or(0.0, |cpu| cpu * 1_000_000.0 / run.requests.max(1) as f64)
}

fn percentile(run: &RunResult, quantile: f64) -> f64 {
    if run.latencies.is_empty() {
        return f64::NAN;
    }
    let mut sorted = run.latencies.clone();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 - 1.0) * quantile).round() as usize;
    sorted[index].as_secs_f64() * 1_000.0
}

fn share_at_least(run: &RunResult, bound: Duration) -> f64 {
    if run.latencies.is_empty() {
        return f64::NAN;
    }
    run.latencies
        .iter()
        .filter(|latency| **latency >= bound)
        .count() as f64
        / run.latencies.len() as f64
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn print_row(payload: Payload, concurrency: usize, mode: Mode, runs: &[RunResult]) {
    let rps = median(runs.iter().map(rps_of).collect());
    let p50 = median(runs.iter().map(|run| percentile(run, 0.50)).collect());
    let p95 = median(runs.iter().map(|run| percentile(run, 0.95)).collect());
    let p99 = median(runs.iter().map(|run| percentile(run, 0.99)).collect());
    let bench_cpu = median(
        runs.iter()
            .map(|run| run.bench_cpu * 1_000_000.0 / run.requests.max(1) as f64)
            .collect(),
    );
    let connections = median(
        runs.iter()
            .map(|run| run.upstream_connections as f64)
            .collect(),
    );
    let errors: u64 = runs.iter().map(|run| run.errors).sum();
    let (mode_label, hamster_cpu, memory) = match mode {
        Mode::Direct => ("direct", "-".to_owned(), "-".to_owned()),
        Mode::Proxied => {
            let cpu = median(runs.iter().map(hamster_cpu_per_request).collect());
            let end = median(
                runs.iter()
                    .map(|run| run.hamster_private.unwrap_or(0) as f64 / MIB as f64)
                    .collect(),
            );
            let peak = median(
                runs.iter()
                    .map(|run| run.hamster_peak_private.unwrap_or(0) as f64 / MIB as f64)
                    .collect(),
            );
            (
                "proxy",
                format!("{cpu:.1}"),
                format!("{end:.2} / {peak:.2}"),
            )
        }
    };
    println!(
        "| {} | {concurrency} | {mode_label} | {rps:.1} | {p50:.3} | {p95:.3} | {p99:.3} | {hamster_cpu} | {bench_cpu:.1} | {connections:.0} | {memory} | {errors} |",
        payload.label()
    );
}

// ----- Memory slope -------------------------------------------------------------

async fn memory(upstream: &Upstream) -> io::Result<()> {
    println!();
    println!("## Memory slope: stalled exchanges held open through Hamster");
    println!();
    println!(
        "Stalled download: each client sends `GET /64m` with a 64 KiB receive buffer and never reads. \
Stalled upload: the upstream never reads `POST /hold`; each client writes until a write makes no progress for 300 ms. \
Private bytes are sampled 1.5 s after the last client is in place, well inside the 30 s progress timeout."
    );
    println!();
    println!(
        "| Buffer | Stall | N | Hamster private MiB | delta vs N=0 MiB | per exchange KiB | client bytes accepted MiB |"
    );
    println!("|---:|---|---:|---:|---:|---:|---:|");
    for buffer in [64 * 1_024, 417_792] {
        for stall in ["download", "upload"] {
            let mut baseline = None;
            for count in [0_usize, 1, 8, 32, 64] {
                let mut samples = Vec::new();
                let mut accepted = Vec::new();
                for _ in 0..RUNS {
                    let hamster = Hamster::start(upstream.address, buffer, 64).await?;
                    // One ordinary exchange so the runtime and pool exist.
                    load(
                        hamster.address,
                        Payload::Tiny,
                        1,
                        Duration::from_millis(200),
                    )
                    .await;
                    let (clients, pushed) = match stall {
                        "download" => (stalled_downloads(hamster.address, count).await?, 0),
                        _ => stalled_uploads(hamster.address, count).await?,
                    };
                    tokio::time::sleep(Duration::from_millis(1_500)).await;
                    samples.push(hamster.memory().0 as f64 / MIB as f64);
                    accepted.push(pushed as f64 / MIB as f64);
                    drop(clients);
                    drop(hamster);
                }
                let private = median(samples);
                let base = *baseline.get_or_insert(private);
                let delta = private - base;
                let per = if count == 0 {
                    0.0
                } else {
                    delta * 1_024.0 / count as f64
                };
                println!(
                    "| {buffer} | {stall} | {count} | {private:.2} | {delta:.2} | {per:.1} | {:.1} |",
                    median(accepted)
                );
            }
        }
    }
    Ok(())
}

async fn small_receive_client(address: SocketAddr) -> io::Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    socket.set_recv_buffer_size(64 * 1_024)?;
    socket.set_send_buffer_size(64 * 1_024)?;
    socket.connect(address).await
}

async fn stalled_downloads(address: SocketAddr, count: usize) -> io::Result<Vec<TcpStream>> {
    let mut clients = Vec::with_capacity(count);
    for _ in 0..count {
        let mut stream = small_receive_client(address).await?;
        stream
            .write_all(b"GET /64m HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await?;
        clients.push(stream);
    }
    Ok(clients)
}

async fn stalled_uploads(address: SocketAddr, count: usize) -> io::Result<(Vec<TcpStream>, u64)> {
    let mut clients = Vec::with_capacity(count);
    let chunk = vec![b'u'; CHUNK];
    let mut pushed = 0_u64;
    for _ in 0..count {
        let mut stream = small_receive_client(address).await?;
        stream
            .write_all(
                b"POST /hold HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1073741824\r\n\r\n",
            )
            .await?;
        while let Ok(result) =
            tokio::time::timeout(Duration::from_millis(300), stream.write_all(&chunk)).await
        {
            result?;
            pushed += CHUNK as u64;
        }
        clients.push(stream);
    }
    Ok((clients, pushed / count.max(1) as u64))
}

// ----- Load generator ---------------------------------------------------------

struct LoadOutcome {
    requests: u64,
    errors: u64,
    elapsed: Duration,
    latencies: Vec<Duration>,
}

/// `concurrency` keep-alive connections, each issuing requests back to back
/// until the window closes. In-flight requests finish and are counted.
async fn load(
    target: SocketAddr,
    payload: Payload,
    concurrency: usize,
    window: Duration,
) -> LoadOutcome {
    let started = Instant::now();
    let deadline = started + window;
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        workers.push(tokio::spawn(worker(target, payload, deadline)));
    }
    let mut outcome = LoadOutcome {
        requests: 0,
        errors: 0,
        elapsed: Duration::ZERO,
        latencies: Vec::new(),
    };
    for worker in workers {
        let (latencies, errors) = worker.await.expect("load worker");
        outcome.requests += latencies.len() as u64;
        outcome.errors += errors;
        outcome.latencies.extend(latencies);
    }
    outcome.elapsed = started.elapsed();
    outcome
}

async fn worker(target: SocketAddr, payload: Payload, deadline: Instant) -> (Vec<Duration>, u64) {
    let mut latencies = Vec::new();
    let mut errors = 0;
    let mut sender = None;
    while Instant::now() < deadline {
        if sender.is_none() {
            match connect(target).await {
                Ok(connected) => sender = Some(connected),
                Err(_) => {
                    errors += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            }
        }
        let request = build_request(payload);
        let started = Instant::now();
        let result = async {
            let response = sender
                .as_mut()
                .expect("connected")
                .send_request(request)
                .await
                .map_err(|error| error.to_string())?;
            if response.status() != StatusCode::OK {
                return Err(format!("status {}", response.status()));
            }
            let mut body = response.into_body();
            while let Some(frame) = body.frame().await {
                frame.map_err(|error| error.to_string())?;
            }
            Ok::<(), String>(())
        }
        .await;
        match result {
            Ok(()) => latencies.push(started.elapsed()),
            Err(_) => {
                errors += 1;
                sender = None;
            }
        }
    }
    (latencies, errors)
}

async fn connect(
    target: SocketAddr,
) -> Result<
    hyper::client::conn::http1::SendRequest<BenchBody>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    let stream = TcpStream::connect(target).await?;
    stream.set_nodelay(true)?;
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
}

fn build_request(payload: Payload) -> Request<BenchBody> {
    let (method, body) = match payload {
        Payload::Upload64MiB => (Method::POST, BenchBody::repeat(64 * MIB)),
        _ => (Method::GET, BenchBody::Empty),
    };
    Request::builder()
        .method(method)
        .uri(payload.path())
        .header("host", "127.0.0.1")
        .body(body)
        .expect("valid request")
}

/// An empty body, or `length` bytes of one shared chunk. No payload is
/// allocated per request, so the generator's own memory stays flat.
enum BenchBody {
    Empty,
    Repeat { chunk: Bytes, remaining: u64 },
}

impl BenchBody {
    fn repeat(length: u64) -> Self {
        static CHUNK_BYTES: std::sync::OnceLock<Bytes> = std::sync::OnceLock::new();
        let chunk = CHUNK_BYTES
            .get_or_init(|| Bytes::from(vec![b'p'; CHUNK]))
            .clone();
        Self::Repeat {
            chunk,
            remaining: length,
        }
    }
}

impl Body for BenchBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Self::Empty => Poll::Ready(None),
            Self::Repeat { chunk, remaining } => {
                if *remaining == 0 {
                    return Poll::Ready(None);
                }
                let take = (*remaining).min(chunk.len() as u64) as usize;
                *remaining -= take as u64;
                Poll::Ready(Some(Ok(Frame::data(chunk.slice(..take)))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Repeat { remaining, .. } => *remaining == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Empty => SizeHint::with_exact(0),
            Self::Repeat { remaining, .. } => SizeHint::with_exact(*remaining),
        }
    }
}

// ----- Upstream -----------------------------------------------------------------

struct Upstream {
    address: SocketAddr,
    connections: Arc<AtomicU64>,
}

impl Upstream {
    async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let connections = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let _ = stream.set_nodelay(true);
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = server_http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service_fn(respond))
                        .await;
                });
            }
        });
        println!("upstream: hyper http1 on {address}, in the bench process");
        Ok(Self {
            address,
            connections,
        })
    }
}

async fn respond(request: Request<Incoming>) -> Result<Response<BenchBody>, Infallible> {
    let body = match request.uri().path() {
        "/tiny" => BenchBody::Repeat {
            chunk: Bytes::from_static(TINY),
            remaining: TINY.len() as u64,
        },
        "/1m" => BenchBody::repeat(MIB),
        "/64m" => BenchBody::repeat(64 * MIB),
        "/upload" => {
            let mut body = request.into_body();
            while let Some(frame) = body.frame().await {
                if frame.is_err() {
                    break;
                }
            }
            BenchBody::Repeat {
                chunk: Bytes::from_static(TINY),
                remaining: TINY.len() as u64,
            }
        }
        "/hold" => {
            // Never read the body and never answer: the upload stalls.
            let _body = request.into_body();
            std::future::pending::<()>().await;
            unreachable!()
        }
        _ => {
            let mut response = Response::new(BenchBody::Empty);
            *response.status_mut() = StatusCode::NOT_FOUND;
            return Ok(response);
        }
    };
    Ok(Response::new(body))
}

// ----- Hamster process ----------------------------------------------------------

struct Hamster {
    child: Child,
    address: SocketAddr,
    _config: TempConfig,
}

struct TempConfig(PathBuf);

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Hamster {
    async fn start(upstream: SocketAddr, buffer: usize, max_idle: usize) -> io::Result<Self> {
        let port = {
            let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            probe.local_addr()?.port()
        };
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let path = std::env::temp_dir().join(format!(
            "hamster-proxy-bench-{}-{port}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                "[server]\nlisten = \"{address}\"\nmax_connections = 4096\nhttp1_max_buffer_size = {buffer}\n\n[[hosts]]\nname = \"127.0.0.1\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"proxy\"\n\n[hosts.locations.proxy]\nupstream = \"http://{upstream}\"\nmax_idle = {max_idle}\n"
            ),
        )?;
        let config = TempConfig(path);
        let child = Command::new(hamster_executable())
            .arg("--config")
            .arg(&config.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let hamster = Self {
            child,
            address,
            _config: config,
        };
        let started = Instant::now();
        while TcpStream::connect(address).await.is_err() {
            if started.elapsed() > Duration::from_secs(10) {
                return Err(io::Error::other("Hamster did not start listening"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(hamster)
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
