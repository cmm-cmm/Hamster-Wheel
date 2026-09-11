use std::{
    ffi::c_void,
    io::{self, Read, Write},
    mem::MaybeUninit,
    path::PathBuf,
    process::{Command as StdCommand, Stdio},
    time::Instant,
};

use hyper::{
    StatusCode,
    header::{HeaderName, HeaderValue},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter, Lines},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    time::{Duration, timeout},
};

const TRIALS: usize = 3;
const RAW_SIZES: [usize; 7] = [0, 16, 128, 1024, 16 * 1024, 64 * 1024, 1024 * 1024];
const FRAME_LIMIT: usize = 2 * 1024 * 1024;
const PIPE_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut arguments = std::env::args();
    let _program = arguments.next();
    let suite = arguments.next().unwrap_or_else(|| "all".to_owned());
    if suite == "--rust-echo-child" {
        let mode = arguments.next().unwrap_or_else(|| "default".to_owned());
        return rust_echo_child(&mode);
    }

    let paths = FixturePaths::discover()?;
    println!(
        "IPC_CONFIG,trials={TRIALS},php={},echo={},timeline={}",
        paths.php.display(),
        paths.echo.display(),
        paths.timeline.display()
    );

    match suite.as_str() {
        "all" => {
            raw_payload_suite(&paths).await?;
            framing_and_buffering_suite(&paths).await?;
            synchronous_stdio_suite(&paths)?;
            timeline_suite(&paths, "default").await?;
            timeline_suite(&paths, "pipe_blocking").await?;
        }
        "raw" => raw_payload_suite(&paths).await?,
        "variants" => framing_and_buffering_suite(&paths).await?,
        "blocking" => blocking_pipe_suite(&paths).await?,
        "sync" => synchronous_stdio_suite(&paths)?,
        "timeline" => timeline_suite(&paths, "default").await?,
        "timeline_blocking" => timeline_suite(&paths, "pipe_blocking").await?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "suite must be one of: all, raw, variants, blocking, sync, timeline, timeline_blocking",
            ));
        }
    }
    Ok(())
}

struct FixturePaths {
    php: PathBuf,
    echo: PathBuf,
    timeline: PathBuf,
}

impl FixturePaths {
    fn discover() -> io::Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let paths = Self {
            php: root.join("php/8.5/php.exe"),
            echo: root.join("fixtures/php-worker/ipc_echo.php"),
            timeline: root.join("fixtures/php-worker/ipc_timeline.php"),
        };
        for (name, path) in [
            ("local PHP 8.5 executable", &paths.php),
            ("echo fixture", &paths.echo),
            ("timeline fixture", &paths.timeline),
        ] {
            if !path.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{name} is missing: {}", path.display()),
                ));
            }
        }
        Ok(paths)
    }
}

#[derive(Clone, Copy, Debug)]
enum ParentWriteMode {
    Split,
    Contiguous,
    Buffered,
}

impl ParentWriteMode {
    const fn name(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Contiguous => "contiguous",
            Self::Buffered => "bufwriter",
        }
    }
}

enum PeerKind<'a> {
    Php {
        paths: &'a FixturePaths,
        mode: &'a str,
    },
    Rust {
        response_mode: &'a str,
    },
}

impl PeerKind<'_> {
    fn name(&self) -> String {
        match self {
            Self::Php { mode, .. } => format!("php_{mode}"),
            Self::Rust { response_mode } => format!("rust_{response_mode}"),
        }
    }
}

enum AsyncWriter {
    Direct(ChildStdin),
    Buffered(BufWriter<ChildStdin>),
}

impl AsyncWriter {
    async fn frame(&mut self, payload: &[u8], mode: ParentWriteMode) -> io::Result<()> {
        let length = u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload exceeds u32"))?;
        let prefix = length.to_be_bytes();
        match (self, mode) {
            (Self::Direct(writer), ParentWriteMode::Split) => {
                writer.write_all(&prefix).await?;
                writer.write_all(payload).await?;
                writer.flush().await
            }
            (Self::Direct(writer), ParentWriteMode::Contiguous) => {
                let mut frame = Vec::with_capacity(4 + payload.len());
                frame.extend_from_slice(&prefix);
                frame.extend_from_slice(payload);
                writer.write_all(&frame).await?;
                writer.flush().await
            }
            (Self::Buffered(writer), ParentWriteMode::Buffered) => {
                writer.write_all(&prefix).await?;
                writer.write_all(payload).await?;
                writer.flush().await
            }
            _ => unreachable!("writer representation matches its selected mode"),
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Direct(writer) => writer.shutdown().await,
            Self::Buffered(writer) => writer.shutdown().await,
        }
    }
}

struct AsyncPeer {
    child: Child,
    writer: AsyncWriter,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pid: u32,
    write_mode: ParentWriteMode,
}

impl AsyncPeer {
    async fn spawn(kind: PeerKind<'_>, write_mode: ParentWriteMode) -> io::Result<Self> {
        let mut command = match kind {
            PeerKind::Php { paths, mode } => {
                let mut command = Command::new(&paths.php);
                command.arg(&paths.echo).arg(mode);
                command
            }
            PeerKind::Rust { response_mode } => {
                let mut command = Command::new(std::env::current_exe()?);
                command.arg("--rust-echo-child").arg(response_mode);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child PID unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("stdin unavailable"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("stderr unavailable"))?;
        let writer = match write_mode {
            ParentWriteMode::Buffered => AsyncWriter::Buffered(BufWriter::new(stdin)),
            ParentWriteMode::Split | ParentWriteMode::Contiguous => AsyncWriter::Direct(stdin),
        };
        let ready = timeout(PIPE_TIMEOUT, read_async_frame(&mut stdout)).await??;
        if ready != b"READY" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "echo child did not send READY",
            ));
        }
        Ok(Self {
            child,
            writer,
            stdout,
            stderr,
            pid,
            write_mode,
        })
    }

    async fn round_trip(&mut self, payload: &[u8]) -> io::Result<Duration> {
        let started = Instant::now();
        timeout(PIPE_TIMEOUT, self.writer.frame(payload, self.write_mode)).await??;
        let echoed = timeout(PIPE_TIMEOUT, read_async_frame(&mut self.stdout)).await??;
        let elapsed = started.elapsed();
        if echoed != payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "echo payload mismatch",
            ));
        }
        Ok(elapsed)
    }

    async fn finish(self) -> io::Result<String> {
        let Self {
            mut child,
            mut writer,
            stdout,
            mut stderr,
            ..
        } = self;
        writer.shutdown().await?;
        drop(writer);
        drop(stdout);
        let status = timeout(PIPE_TIMEOUT, child.wait()).await??;
        let mut diagnostic = String::new();
        stderr.read_to_string(&mut diagnostic).await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "echo child exited with {status}: {diagnostic}"
            )));
        }
        Ok(diagnostic)
    }
}

async fn raw_payload_suite(paths: &FixturePaths) -> io::Result<()> {
    for payload_size in RAW_SIZES {
        let iterations = raw_iterations(payload_size);
        run_async_raw_case(
            PeerKind::Rust {
                response_mode: "default",
            },
            ParentWriteMode::Split,
            payload_size,
            iterations,
        )
        .await?;
        run_async_raw_case(
            PeerKind::Php {
                paths,
                mode: "default",
            },
            ParentWriteMode::Split,
            payload_size,
            iterations,
        )
        .await?;
    }
    Ok(())
}

async fn framing_and_buffering_suite(paths: &FixturePaths) -> io::Result<()> {
    let payload_size = 128;
    let iterations = 300;
    for (mode, write_mode) in [
        ("default", ParentWriteMode::Split),
        ("default", ParentWriteMode::Contiguous),
        ("default", ParentWriteMode::Buffered),
        ("split", ParentWriteMode::Split),
        ("unbuffered", ParentWriteMode::Split),
        ("stream_blocking", ParentWriteMode::Split),
        ("pipe_blocking", ParentWriteMode::Split),
        ("no_flush", ParentWriteMode::Split),
        ("batched", ParentWriteMode::Split),
    ] {
        run_async_raw_case(
            PeerKind::Php { paths, mode },
            write_mode,
            payload_size,
            iterations,
        )
        .await?;
    }
    Ok(())
}

async fn blocking_pipe_suite(paths: &FixturePaths) -> io::Result<()> {
    for mode in ["stream_blocking", "pipe_blocking"] {
        run_async_raw_case(
            PeerKind::Php { paths, mode },
            ParentWriteMode::Split,
            128,
            1_000,
        )
        .await?;
    }
    Ok(())
}

async fn run_async_raw_case(
    kind: PeerKind<'_>,
    write_mode: ParentWriteMode,
    payload_size: usize,
    iterations: usize,
) -> io::Result<()> {
    let peer_name = kind.name();
    let mut peer = AsyncPeer::spawn(kind, write_mode).await?;
    let payload = payload(payload_size);
    for _ in 0..20.min(iterations) {
        peer.round_trip(&payload).await?;
    }
    let mut trials = Vec::with_capacity(TRIALS);
    for run in 1..=TRIALS {
        let parent_cpu_start = process_cpu_seconds(std::process::id())?;
        let child_cpu_start = process_cpu_seconds(peer.pid)?;
        let wall_start = Instant::now();
        let mut latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            latencies.push(peer.round_trip(&payload).await?);
        }
        let wall = wall_start.elapsed().as_secs_f64();
        let parent_cpu = process_cpu_seconds(std::process::id())? - parent_cpu_start;
        let child_cpu = process_cpu_seconds(peer.pid)? - child_cpu_start;
        let result = RawTrial::new(latencies, wall, parent_cpu, child_cpu);
        print_raw(
            "IPC_RAW_RUN",
            &peer_name,
            write_mode,
            payload_size,
            run.to_string().as_str(),
            &result,
        );
        trials.push(result);
    }
    let median = RawTrial::median(&trials);
    print_raw(
        "IPC_RAW_MEDIAN",
        &peer_name,
        write_mode,
        payload_size,
        "median",
        &median,
    );
    let diagnostic = peer.finish().await?;
    for line in diagnostic.lines() {
        println!(
            "IPC_CHILD_DIAGNOSTIC,peer={peer_name},parent_write={},payload_bytes={payload_size},{line}",
            write_mode.name()
        );
    }
    Ok(())
}

struct RawTrial {
    stats: LatencyStats,
    wall_seconds: f64,
    rps: f64,
    parent_cpu_us: f64,
    child_cpu_us: f64,
    combined_cpu_us: f64,
}

impl RawTrial {
    fn new(latencies: Vec<Duration>, wall_seconds: f64, parent_cpu: f64, child_cpu: f64) -> Self {
        let operations = latencies.len() as f64;
        Self {
            stats: LatencyStats::new(&latencies),
            wall_seconds,
            rps: operations / wall_seconds,
            parent_cpu_us: parent_cpu * 1_000_000.0 / operations,
            child_cpu_us: child_cpu * 1_000_000.0 / operations,
            combined_cpu_us: (parent_cpu + child_cpu) * 1_000_000.0 / operations,
        }
    }

    fn median(trials: &[Self]) -> Self {
        Self {
            stats: LatencyStats::field_median(trials.iter().map(|trial| &trial.stats)),
            wall_seconds: median(trials.iter().map(|trial| trial.wall_seconds).collect()),
            rps: median(trials.iter().map(|trial| trial.rps).collect()),
            parent_cpu_us: median(trials.iter().map(|trial| trial.parent_cpu_us).collect()),
            child_cpu_us: median(trials.iter().map(|trial| trial.child_cpu_us).collect()),
            combined_cpu_us: median(trials.iter().map(|trial| trial.combined_cpu_us).collect()),
        }
    }
}

fn print_raw(
    prefix: &str,
    peer: &str,
    mode: ParentWriteMode,
    payload: usize,
    run: &str,
    trial: &RawTrial,
) {
    println!(
        "{prefix},peer={peer},parent_write={},payload_bytes={payload},run={run},rps={:.3},wall_s={:.6},p50_us={:.3},p95_us={:.3},p99_us={:.3},min_us={:.3},max_us={:.3},quantum_pct={:.3},parent_cpu_us={:.3},child_cpu_us={:.3},combined_cpu_us={:.3}",
        mode.name(),
        trial.rps,
        trial.wall_seconds,
        trial.stats.p50_us,
        trial.stats.p95_us,
        trial.stats.p99_us,
        trial.stats.min_us,
        trial.stats.max_us,
        trial.stats.quantum_pct,
        trial.parent_cpu_us,
        trial.child_cpu_us,
        trial.combined_cpu_us
    );
}

fn synchronous_stdio_suite(paths: &FixturePaths) -> io::Result<()> {
    for peer in ["rust", "php"] {
        let mut child = if peer == "rust" {
            StdCommand::new(std::env::current_exe()?)
                .arg("--rust-echo-child")
                .arg("default")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?
        } else {
            StdCommand::new(&paths.php)
                .arg(&paths.echo)
                .arg("default")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?
        };
        let pid = child.id();
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("stdin unavailable"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        let ready = read_sync_frame(&mut stdout)?;
        if ready != b"READY" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync child not ready",
            ));
        }
        let data = payload(128);
        for _ in 0..20 {
            sync_round_trip(&mut stdin, &mut stdout, &data)?;
        }
        let mut trials = Vec::new();
        for run in 1..=TRIALS {
            let parent_start = process_cpu_seconds(std::process::id())?;
            let child_start = process_cpu_seconds(pid)?;
            let wall_start = Instant::now();
            let mut latencies = Vec::with_capacity(500);
            for _ in 0..500 {
                latencies.push(sync_round_trip(&mut stdin, &mut stdout, &data)?);
            }
            let wall = wall_start.elapsed().as_secs_f64();
            let result = RawTrial::new(
                latencies,
                wall,
                process_cpu_seconds(std::process::id())? - parent_start,
                process_cpu_seconds(pid)? - child_start,
            );
            print_raw(
                "IPC_SYNC_RUN",
                peer,
                ParentWriteMode::Split,
                128,
                &run.to_string(),
                &result,
            );
            trials.push(result);
        }
        print_raw(
            "IPC_SYNC_MEDIAN",
            peer,
            ParentWriteMode::Split,
            128,
            "median",
            &RawTrial::median(&trials),
        );
        drop(stdin);
        let status = child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "sync {peer} child exited with {status}"
            )));
        }
    }
    Ok(())
}

fn sync_round_trip(
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    payload: &[u8],
) -> io::Result<Duration> {
    let started = Instant::now();
    stdin.write_all(&(payload.len() as u32).to_be_bytes())?;
    stdin.write_all(payload)?;
    stdin.flush()?;
    let echoed = read_sync_frame(stdout)?;
    if echoed != payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sync echo mismatch",
        ));
    }
    Ok(started.elapsed())
}

async fn timeline_suite(paths: &FixturePaths, mode: &str) -> io::Result<()> {
    let mut peer = TimelinePeer::spawn(paths, mode).await?;
    let iterations = 1_200;
    let mut trials = Vec::new();
    for run in 1..=TRIALS {
        let parent_start = process_cpu_seconds(std::process::id())?;
        let child_start = process_cpu_seconds(peer.pid)?;
        let mut samples = Vec::with_capacity(iterations);
        for index in 0..iterations {
            samples.push(
                peer.transaction((run * iterations + index + 1) as u64)
                    .await?,
            );
        }
        let parent_cpu = process_cpu_seconds(std::process::id())? - parent_start;
        let child_cpu = process_cpu_seconds(peer.pid)? - child_start;
        print_timeline_run(mode, run, &samples, parent_cpu, child_cpu);
        trials.push(TimelineTrial::new(&samples, parent_cpu, child_cpu));
    }
    print_timeline_median(mode, &trials);
    peer.finish().await?;
    Ok(())
}

struct TimelinePeer {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_lines: Lines<BufReader<ChildStderr>>,
    pid: u32,
}

impl TimelinePeer {
    async fn spawn(paths: &FixturePaths, mode: &str) -> io::Result<Self> {
        let mut child = Command::new(&paths.php)
            .arg(&paths.timeline)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("timeline PID unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("timeline stdin unavailable"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("timeline stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("timeline stderr unavailable"))?;
        let ready = read_async_frame(&mut stdout).await?;
        if ready.first() != Some(&1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "timeline READY invalid",
            ));
        }
        Ok(Self {
            child,
            stdin,
            stdout,
            stderr_lines: BufReader::new(stderr).lines(),
            pid,
        })
    }

    async fn transaction(&mut self, request_id: u64) -> io::Result<TimelineSample> {
        let body = payload(128);
        let t0 = Instant::now();
        let request = encode_production_request(request_id, &body);
        let t1 = Instant::now();
        write_async_frame_split(&mut self.stdin, &request).await?;
        let t2 = Instant::now();
        let response = read_async_frame(&mut self.stdout).await?;
        let t8 = Instant::now();
        decode_and_validate_response(&response, request_id, &body)?;
        let t9 = Instant::now();
        let line = timeout(PIPE_TIMEOUT, self.stderr_lines.next_line())
            .await??
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "timeline stderr ended"))?;
        let php = PhpTiming::parse(&line, request_id)?;
        let external_wait_ns = (t8 - t2).as_nanos() as f64;
        Ok(TimelineSample {
            rust_encode_us: duration_us(t1 - t0),
            rust_write_us: duration_us(t2 - t1),
            combined_transfer_wait_us: (external_wait_ns - php.total_ns as f64).max(0.0) / 1_000.0,
            php_decode_us: php.decode_ns as f64 / 1_000.0,
            php_handler_us: php.handler_ns as f64 / 1_000.0,
            php_encode_us: php.encode_ns as f64 / 1_000.0,
            php_write_us: php.write_ns as f64 / 1_000.0,
            rust_decode_us: duration_us(t9 - t8),
            total_us: duration_us(t9 - t0),
        })
    }

    async fn finish(mut self) -> io::Result<()> {
        write_async_frame_split(&mut self.stdin, &[4]).await?;
        let ack = read_async_frame(&mut self.stdout).await?;
        if ack != [5] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "timeline shutdown ACK invalid",
            ));
        }
        self.stdin.shutdown().await?;
        let status = self.child.wait().await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "timeline child exited with {status}"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct TimelineSample {
    rust_encode_us: f64,
    rust_write_us: f64,
    combined_transfer_wait_us: f64,
    php_decode_us: f64,
    php_handler_us: f64,
    php_encode_us: f64,
    php_write_us: f64,
    rust_decode_us: f64,
    total_us: f64,
}

struct TimelineTrial {
    stages: TimelineSample,
    first: LatencyStats,
    two_to_ten: LatencyStats,
    hundred_to_999: LatencyStats,
    thousand_plus: LatencyStats,
    parent_cpu_us: f64,
    child_cpu_us: f64,
}

impl TimelineTrial {
    fn new(samples: &[TimelineSample], parent_cpu: f64, child_cpu: f64) -> Self {
        let steady = &samples[99..];
        let operations = samples.len() as f64;
        Self {
            stages: timeline_stage_medians(steady),
            first: timeline_latency(&samples[..1]),
            two_to_ten: timeline_latency(&samples[1..10]),
            hundred_to_999: timeline_latency(&samples[99..999]),
            thousand_plus: timeline_latency(&samples[999..]),
            parent_cpu_us: parent_cpu * 1_000_000.0 / operations,
            child_cpu_us: child_cpu * 1_000_000.0 / operations,
        }
    }
}

fn print_timeline_run(
    mode: &str,
    run: usize,
    samples: &[TimelineSample],
    parent_cpu: f64,
    child_cpu: f64,
) {
    let trial = TimelineTrial::new(samples, parent_cpu, child_cpu);
    print_timeline("IPC_TIMELINE_RUN", mode, &run.to_string(), &trial);
}

fn print_timeline_median(mode: &str, trials: &[TimelineTrial]) {
    let field = |select: fn(&TimelineTrial) -> f64| median(trials.iter().map(select).collect());
    let stages = TimelineSample {
        rust_encode_us: field(|v| v.stages.rust_encode_us),
        rust_write_us: field(|v| v.stages.rust_write_us),
        combined_transfer_wait_us: field(|v| v.stages.combined_transfer_wait_us),
        php_decode_us: field(|v| v.stages.php_decode_us),
        php_handler_us: field(|v| v.stages.php_handler_us),
        php_encode_us: field(|v| v.stages.php_encode_us),
        php_write_us: field(|v| v.stages.php_write_us),
        rust_decode_us: field(|v| v.stages.rust_decode_us),
        total_us: field(|v| v.stages.total_us),
    };
    let stats = |select: fn(&TimelineTrial) -> &LatencyStats| {
        LatencyStats::field_median(trials.iter().map(select))
    };
    let median_trial = TimelineTrial {
        stages,
        first: stats(|v| &v.first),
        two_to_ten: stats(|v| &v.two_to_ten),
        hundred_to_999: stats(|v| &v.hundred_to_999),
        thousand_plus: stats(|v| &v.thousand_plus),
        parent_cpu_us: field(|v| v.parent_cpu_us),
        child_cpu_us: field(|v| v.child_cpu_us),
    };
    print_timeline("IPC_TIMELINE_MEDIAN", mode, "median", &median_trial);
}

fn print_timeline(prefix: &str, mode: &str, run: &str, trial: &TimelineTrial) {
    let s = trial.stages;
    println!(
        "{prefix},mode={mode},run={run},rust_encode_us={:.3},rust_write_us={:.3},combined_transfer_wait_us={:.3},php_decode_us={:.3},php_handler_us={:.3},php_encode_us={:.3},php_write_us={:.3},rust_decode_us={:.3},total_p50_us={:.3},parent_cpu_us={:.3},child_cpu_us={:.3},combined_cpu_us={:.3}",
        s.rust_encode_us,
        s.rust_write_us,
        s.combined_transfer_wait_us,
        s.php_decode_us,
        s.php_handler_us,
        s.php_encode_us,
        s.php_write_us,
        s.rust_decode_us,
        s.total_us,
        trial.parent_cpu_us,
        trial.child_cpu_us,
        trial.parent_cpu_us + trial.child_cpu_us
    );
    for (window, stats) in [
        ("first", &trial.first),
        ("2_10", &trial.two_to_ten),
        ("100_999", &trial.hundred_to_999),
        ("1000_plus", &trial.thousand_plus),
    ] {
        println!(
            "{prefix}_DISTRIBUTION,mode={mode},run={run},window={window},p50_us={:.3},p95_us={:.3},p99_us={:.3},min_us={:.3},max_us={:.3},quantum_pct={:.3}",
            stats.p50_us, stats.p95_us, stats.p99_us, stats.min_us, stats.max_us, stats.quantum_pct
        );
    }
}

fn timeline_stage_medians(samples: &[TimelineSample]) -> TimelineSample {
    let field = |select: fn(&TimelineSample) -> f64| median(samples.iter().map(select).collect());
    TimelineSample {
        rust_encode_us: field(|v| v.rust_encode_us),
        rust_write_us: field(|v| v.rust_write_us),
        combined_transfer_wait_us: field(|v| v.combined_transfer_wait_us),
        php_decode_us: field(|v| v.php_decode_us),
        php_handler_us: field(|v| v.php_handler_us),
        php_encode_us: field(|v| v.php_encode_us),
        php_write_us: field(|v| v.php_write_us),
        rust_decode_us: field(|v| v.rust_decode_us),
        total_us: field(|v| v.total_us),
    }
}

fn timeline_latency(samples: &[TimelineSample]) -> LatencyStats {
    LatencyStats::from_microseconds(samples.iter().map(|sample| sample.total_us).collect())
}

struct PhpTiming {
    decode_ns: u64,
    handler_ns: u64,
    encode_ns: u64,
    write_ns: u64,
    total_ns: u64,
}

impl PhpTiming {
    fn parse(line: &str, expected_id: u64) -> io::Result<Self> {
        let mut values = std::collections::HashMap::new();
        let mut parts = line.split(',');
        if parts.next() != Some("IPC_TIMING") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected timeline diagnostic: {line}"),
            ));
        }
        for part in parts {
            let (key, value) = part.split_once('=').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid timeline field")
            })?;
            values.insert(
                key,
                value
                    .parse::<u64>()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
        }
        let get = |key: &str| {
            values.get(key).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("missing timeline field {key}"),
                )
            })
        };
        if get("id")? != expected_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "timeline request ID mismatch",
            ));
        }
        Ok(Self {
            decode_ns: get("decode_ns")?,
            handler_ns: get("handler_ns")?,
            encode_ns: get("encode_ns")?,
            write_ns: get("write_ns")?,
            total_ns: get("php_total_ns")?,
        })
    }
}

#[derive(Clone, Copy)]
struct LatencyStats {
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    min_us: f64,
    max_us: f64,
    quantum_pct: f64,
}

impl LatencyStats {
    fn new(values: &[Duration]) -> Self {
        Self::from_microseconds(values.iter().copied().map(duration_us).collect())
    }

    fn from_microseconds(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        let quantum_hits = values
            .iter()
            .filter(|&&value| {
                let multiple = (value / 15_625.0).round();
                multiple >= 1.0 && (value - multiple * 15_625.0).abs() <= 1_000.0
            })
            .count();
        Self {
            p50_us: percentile(&values, 0.50),
            p95_us: percentile(&values, 0.95),
            p99_us: percentile(&values, 0.99),
            min_us: values[0],
            max_us: values[values.len() - 1],
            quantum_pct: quantum_hits as f64 * 100.0 / values.len() as f64,
        }
    }

    fn field_median<'a>(stats: impl Iterator<Item = &'a Self>) -> Self {
        let values = stats.copied().collect::<Vec<_>>();
        let field = |select: fn(&Self) -> f64| median(values.iter().map(select).collect());
        Self {
            p50_us: field(|v| v.p50_us),
            p95_us: field(|v| v.p95_us),
            p99_us: field(|v| v.p99_us),
            min_us: field(|v| v.min_us),
            max_us: field(|v| v.max_us),
            quantum_pct: field(|v| v.quantum_pct),
        }
    }
}

fn encode_production_request(request_id: u64, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![2];
    payload.extend_from_slice(&request_id.to_be_bytes());
    push_u16_bytes(&mut payload, b"POST");
    push_u32_bytes(&mut payload, b"/profile");
    push_u32_bytes(&mut payload, b"/profile");
    push_u32_bytes(&mut payload, b"");
    payload.extend_from_slice(&1_u16.to_be_bytes());
    push_u16_bytes(&mut payload, b"content-type");
    push_u32_bytes(&mut payload, b"application/octet-stream");
    push_u32_bytes(&mut payload, body);
    payload
}

fn decode_and_validate_response(
    payload: &[u8],
    expected_id: u64,
    expected_body: &[u8],
) -> io::Result<()> {
    let mut cursor = Cursor::new(payload);
    if cursor.take(1)? != [3] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response kind mismatch",
        ));
    }
    if cursor.u64()? != expected_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response ID mismatch",
        ));
    }
    StatusCode::from_u16(cursor.u16()?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let header_count = cursor.u16()?;
    for _ in 0..header_count {
        HeaderName::from_bytes(cursor.bytes_u16()?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        HeaderValue::from_bytes(cursor.bytes_u32()?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    if cursor.bytes_u32()? != expected_body || !cursor.finished() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response body/trailing data mismatch",
        ));
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cursor overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated payload"))?;
        self.position = end;
        Ok(value)
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes_u16(&mut self) -> io::Result<&'a [u8]> {
        let length = self.u16()? as usize;
        self.take(length)
    }
    fn bytes_u32(&mut self) -> io::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }
    fn finished(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn push_u16_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u16).to_be_bytes());
    output.extend_from_slice(value);
}
fn push_u32_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u32).to_be_bytes());
    output.extend_from_slice(value);
}

async fn write_async_frame_split(
    writer: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> io::Result<()> {
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

async fn read_async_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> io::Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

fn read_sync_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn rust_echo_child(response_mode: &str) -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    write_sync_frame(&mut stdout, b"READY", "default")?;
    while let Ok(payload) = read_sync_frame(&mut stdin) {
        write_sync_frame(&mut stdout, &payload, response_mode)?;
    }
    Ok(())
}

fn write_sync_frame(writer: &mut impl Write, payload: &[u8], mode: &str) -> io::Result<()> {
    let prefix = (payload.len() as u32).to_be_bytes();
    if mode == "split" {
        writer.write_all(&prefix)?;
        writer.write_all(payload)?;
    } else {
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(payload);
        writer.write_all(&frame)?;
    }
    writer.flush()
}

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}
fn raw_iterations(size: usize) -> usize {
    match size {
        0..=128 => 500,
        129..=1024 => 300,
        1025..=16384 => 150,
        16385..=65536 => 75,
        _ => 10,
    }
}
fn duration_us(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}
fn percentile(values: &[f64], percentile: f64) -> f64 {
    values[((values.len() - 1) as f64 * percentile).floor() as usize]
}
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

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
}

fn process_cpu_seconds(pid: u32) -> io::Result<f64> {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut creation = MaybeUninit::uninit();
    let mut exit = MaybeUninit::uninit();
    let mut kernel = MaybeUninit::uninit();
    let mut user = MaybeUninit::uninit();
    let success = unsafe {
        GetProcessTimes(
            handle,
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let (kernel, user) = unsafe { (kernel.assume_init(), user.assume_init()) };
    let ticks = ((u64::from(kernel.high) << 32) | u64::from(kernel.low))
        + ((u64::from(user.high) << 32) | u64::from(user.low));
    Ok(ticks as f64 / 10_000_000.0)
}
