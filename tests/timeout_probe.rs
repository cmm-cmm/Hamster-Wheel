//! Fault-injection probe for the fixed persistent-worker request timeout.
//!
//! This file is investigation instrumentation, not a regression suite. It
//! drives real `php.exe` children through the real pool and prints the
//! slot/generation/failure trajectory so the cascade hypothesis can be
//! confirmed or refuted from measurement rather than from source reading.
//!
//! Every test is `#[ignore]`d and needs the repository PHP runtime
//! (`support::PHP_RELATIVE_PATH`). Run one at a time:
//! `cargo test --test timeout_probe <name> -- --ignored --nocapture`

mod support;

use support::PHP_RELATIVE_PATH;

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use hamster_wheel::{
    config,
    php_worker::{PhpWorkerPool, PoolSnapshot},
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

struct Probe {
    address: SocketAddr,
    runtime: Arc<config::RuntimeConfig>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
    started: Instant,
}

impl Probe {
    async fn start(workers: usize, queue_capacity: usize) -> Self {
        Self::start_with_request_timeout(workers, queue_capacity, 5).await
    }

    async fn start_with_request_timeout(
        workers: usize,
        queue_capacity: usize,
        request_timeout: u64,
    ) -> Self {
        let runtime = Arc::new(probe_runtime(workers, queue_capacity, request_timeout));
        runtime
            .start_workers()
            .await
            .expect("every probe worker should reach READY");
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("probe listener should bind");
        let address = listener.local_addr().expect("probe listener address");
        let router = runtime.router_arc();
        let http_limits = runtime.http_limits();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, http_limits, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Self {
            address,
            runtime,
            shutdown_tx: Some(shutdown_tx),
            task,
            started: Instant::now(),
        }
    }

    fn pool(&self) -> &Arc<PhpWorkerPool> {
        self.runtime.php_worker().expect("probe pool should exist")
    }

    fn snapshot(&self) -> PoolSnapshot {
        self.pool().snapshot()
    }

    /// Milliseconds since the probe's listener started, so every printed line
    /// shares one timeline.
    fn now_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn report(&self, label: &str) {
        let snapshot = self.snapshot();
        let slots: Vec<String> = snapshot
            .workers
            .iter()
            .map(|worker| {
                format!(
                    "s{}:g{} {:?} pid={} fail={}",
                    worker.slot,
                    worker.generation,
                    worker.state,
                    worker
                        .pid
                        .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
                    worker.consecutive_failures
                )
            })
            .collect();
        println!(
            "[{:>6} ms] {label:<28} ready={}/{} respawns={} | {}",
            self.now_ms(),
            snapshot.ready_workers,
            snapshot.target_workers,
            snapshot.total_respawns,
            slots.join(" | ")
        );
    }

    /// Blocks until at least one worker is READY, returning how long that took.
    ///
    /// A pool with no live worker refuses immediately, so a probe that does not
    /// wait measures the refusal path instead of the timeout path.
    async fn wait_for_ready(&self, patience: Duration) -> Duration {
        let clock = Instant::now();
        loop {
            if self.snapshot().ready_workers > 0 {
                return clock.elapsed();
            }
            assert!(
                clock.elapsed() < patience,
                "no worker reached READY within {patience:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Blocks until every slot is READY, returning how long that took.
    async fn wait_for_target(&self, patience: Duration) -> Duration {
        let clock = Instant::now();
        loop {
            let snapshot = self.snapshot();
            if snapshot.ready_workers == snapshot.target_workers {
                return clock.elapsed();
            }
            assert!(
                clock.elapsed() < patience,
                "pool never reached target within {patience:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Issues one request and returns its status line plus how long it took.
    async fn get(&self, target: &str) -> (String, Duration) {
        let clock = Instant::now();
        let response = request_at(self.address, target).await;
        (status_line(&response), clock.elapsed())
    }

    async fn stop(mut self) {
        self.shutdown_tx
            .take()
            .expect("probe shutdown sender")
            .send(())
            .expect("probe server should still run");
        (&mut self.task)
            .await
            .expect("probe server task should finish")
            .expect("probe server should stop cleanly");
        self.runtime
            .shutdown_workers()
            .await
            .expect("probe pool should stop cleanly");
        let leaked = self.pool().pids();
        println!("[{:>6} ms] stopped; leaked pids={leaked:?}", self.now_ms());
        assert!(leaked.is_empty(), "probe must not leak PHP children");
    }
}

async fn request_at(address: SocketAddr, target: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("probe client should connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("probe request should be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("probe response should be read");
    response
}

fn status_line(response: &[u8]) -> String {
    let text = String::from_utf8_lossy(response);
    text.lines()
        .next()
        .unwrap_or("<no status line>")
        .trim_end()
        .to_owned()
}

fn probe_runtime(
    workers: usize,
    queue_capacity: usize,
    request_timeout: u64,
) -> config::RuntimeConfig {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join(PHP_RELATIVE_PATH);
    let script = root.join("fixtures/php-worker/worker.php");
    assert!(
        executable.is_file(),
        "timeout probe requires repository {PHP_RELATIVE_PATH}"
    );
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = {queue_capacity}\nrequest_timeout = {request_timeout}\n",
        toml_literal(&executable),
        toml_literal(&script)
    );
    config::compile_str(&source, &root).expect("probe config should compile")
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// E1: one ordinary slow request on a single-worker pool.
///
/// Establishes the base cost of a timeout: what the client sees, whether the
/// child is really replaced, and how long the pool is unable to serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn e1_one_slow_request_on_one_worker() {
    let probe = Probe::start(1, 8).await;
    probe.report("before");
    let before = probe.snapshot();

    let (status, elapsed) = probe.get("/delay?ms=6000").await;
    println!(
        "[{:>6} ms] slow request -> {status} after {} ms",
        probe.now_ms(),
        elapsed.as_millis()
    );
    probe.report("immediately after");

    // How long until the pool can answer an ordinary request again.
    let recovery = Instant::now();
    let mut attempts = 0_u32;
    loop {
        let (status, _) = probe.get("/json").await;
        attempts += 1;
        if status.contains("200") {
            println!(
                "[{:>6} ms] fast request -> {status} after {} ms and {attempts} attempt(s)",
                probe.now_ms(),
                recovery.elapsed().as_millis()
            );
            break;
        }
        assert!(
            recovery.elapsed() < Duration::from_secs(30),
            "pool never recovered; last status {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    probe.report("after recovery");

    let after = probe.snapshot();
    println!(
        "generation {} -> {}, respawns {} -> {}, failures {} -> {}",
        before.workers[0].generation,
        after.workers[0].generation,
        before.total_respawns,
        after.total_respawns,
        before.workers[0].consecutive_failures,
        after.workers[0].consecutive_failures
    );
    probe.stop().await;
}

/// E2: back-to-back slow requests on a single-worker pool.
///
/// The question is whether a slot that only ever sees slow traffic walks the
/// whole backoff ladder into the cooldown band, and what a client sees at each
/// step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn e2_repeated_slow_requests_walk_the_ladder() {
    let probe = Probe::start(1, 8).await;
    probe.report("before");

    for round in 1..=13_u32 {
        // A request sent while the replacement is still bootstrapping is
        // refused instantly and never reaches PHP, so it cannot cost the slot
        // a failure. Waiting for READY is what makes this the worst case
        // rather than an accidentally harmless one.
        let waited = probe.wait_for_ready(Duration::from_secs(360)).await;
        let (status, elapsed) = probe.get("/delay?ms=6000").await;
        let snapshot = probe.snapshot();
        println!(
            "[{:>6} ms] round {round:>2}: waited {:>6} ms for READY, {status} after {:>5} ms | ready={}/{} gen={} fail={}",
            probe.now_ms(),
            waited.as_millis(),
            elapsed.as_millis(),
            snapshot.ready_workers,
            snapshot.target_workers,
            snapshot.workers[0].generation,
            snapshot.workers[0].consecutive_failures
        );
    }
    probe.report("after slow traffic");

    // Does an ordinary request still work, and how long does it wait?
    let clock = Instant::now();
    let (status, elapsed) = probe.get("/json").await;
    println!(
        "[{:>6} ms] fast request after ladder -> {status} in {} ms",
        probe.now_ms(),
        elapsed.as_millis()
    );
    let _ = clock;
    probe.report("after one fast request");
    probe.stop().await;
}

/// E3: concurrent slow requests on a multi-worker pool, with ordinary traffic
/// running beside them.
///
/// This is the availability question the review raised: whether slow requests
/// can take every worker away from fast requests at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn e3_concurrent_slow_requests_against_fast_traffic() {
    let probe = Probe::start(4, 16).await;
    probe.report("before");
    let probe = Arc::new(probe);

    let mut slow = Vec::new();
    for index in 0..4_u32 {
        let probe = Arc::clone(&probe);
        slow.push(tokio::spawn(async move {
            let (status, elapsed) = probe.get("/delay?ms=6000").await;
            (index, status, elapsed)
        }));
    }

    // Sample ordinary traffic while all four workers are occupied.
    let mut fast_results: Vec<(u128, String, u128)> = Vec::new();
    for _ in 0..14_u32 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let at = probe.now_ms();
        let (status, elapsed) = probe.get("/json").await;
        fast_results.push((at, status, elapsed.as_millis()));
        probe.report("during slow traffic");
    }

    for handle in slow {
        let (index, status, elapsed) = handle.await.expect("slow request task");
        println!(
            "slow request {index}: {status} after {} ms",
            elapsed.as_millis()
        );
    }
    println!("--- ordinary /json requests sampled during the slow window ---");
    for (at, status, elapsed) in &fast_results {
        println!("  at {at:>6} ms: {status} in {elapsed:>5} ms");
    }
    probe.report("after slow traffic");

    let probe = Arc::try_unwrap(probe).unwrap_or_else(|_| panic!("probe still shared"));
    probe.stop().await;
}

/// E4: the realistic mixed workload.
///
/// Continuous ordinary traffic plus one slow request every few seconds. The
/// decisive question is whether successes on healthy generations keep the
/// failure history reset, or whether the failures accumulate anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn e4_mixed_traffic_accumulates_or_resets() {
    let probe = Arc::new(Probe::start(2, 16).await);
    probe.report("before");

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fast_probe = Arc::clone(&probe);
    let fast_stop = Arc::clone(&stop);
    let fast = tokio::spawn(async move {
        let mut served = 0_u32;
        let mut refused = 0_u32;
        let mut other = 0_u32;
        while !fast_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let (status, _) = fast_probe.get("/json").await;
            if status.contains("200") {
                served += 1;
            } else if status.contains("503") {
                refused += 1;
            } else {
                other += 1;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        (served, refused, other)
    });

    for round in 1..=10_u32 {
        let (status, elapsed) = probe.get("/delay?ms=6000").await;
        let snapshot = probe.snapshot();
        let failures: Vec<usize> = snapshot
            .workers
            .iter()
            .map(|worker| worker.consecutive_failures)
            .collect();
        println!(
            "[{:>6} ms] slow {round:>2}: {status} in {:>5} ms | ready={}/{} respawns={} failures={failures:?}",
            probe.now_ms(),
            elapsed.as_millis(),
            snapshot.ready_workers,
            snapshot.target_workers,
            snapshot.total_respawns,
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (served, refused, other) = fast.await.expect("fast traffic task");
    println!("ordinary traffic during the run: 200={served} 503={refused} other={other}");
    probe.report("after mixed traffic");

    let probe = Arc::try_unwrap(probe).unwrap_or_else(|_| panic!("probe still shared"));
    probe.stop().await;
}

/// E7: what one timeout wave costs ordinary traffic.
///
/// Drives a slot into the cooldown band with paced slow requests, then stops
/// the slow traffic completely and samples ordinary requests until they are
/// served again. This separates "a trap the pool cannot leave" from "an
/// expensive but self-healing state", which is the difference that decides
/// whether a policy change is warranted at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn e7_cost_of_one_timeout_wave_to_ordinary_traffic() {
    let probe = Probe::start(1, 8).await;
    probe.report("before");

    let mut round = 0_u32;
    loop {
        probe.wait_for_ready(Duration::from_secs(360)).await;
        round += 1;
        let (status, _) = probe.get("/delay?ms=6000").await;
        let failures = probe.snapshot().workers[0].consecutive_failures;
        println!(
            "[{:>6} ms] slow {round:>2}: {status} | fail={failures}",
            probe.now_ms()
        );
        // This probe predates the accepted execution-timeout policy. A slow
        // application transaction now increments its dedicated counter and
        // deliberately does not enter the infrastructure backoff ladder, so
        // the old open-ended loop could never reach `fail > 10`. Keep the
        // historical probe runnable under `cargo test -- --ignored` without
        // pretending it still measures a reachable policy.
        if round == 2 && failures == 0 {
            assert_eq!(probe.snapshot().total_execution_timeouts, 2);
            println!(
                "E7 not applicable: execution timeouts do not enter the infrastructure cooldown ladder"
            );
            probe.stop().await;
            return;
        }
        if failures > 10 {
            break;
        }
    }
    probe.report("cooldown band reached");

    // Slow traffic stops here. Nothing else touches the pool except ordinary
    // requests, which is what an operator's users would still be sending.
    println!("--- slow traffic stopped; ordinary /json only from here ---");
    let outage = Instant::now();
    let mut refused = 0_u32;
    let served_at = loop {
        let (status, _) = probe.get("/json").await;
        if status.contains("200") {
            break outage.elapsed();
        }
        refused += 1;
        if refused.is_multiple_of(10) {
            println!(
                "[{:>6} ms] ordinary traffic still refused after {} ms ({refused} attempts)",
                probe.now_ms(),
                outage.elapsed().as_millis()
            );
        }
        assert!(
            outage.elapsed() < Duration::from_secs(400),
            "ordinary traffic never recovered: {status}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    println!(
        "[{:>6} ms] ordinary traffic served again after {} ms and {refused} refusal(s)",
        probe.now_ms(),
        served_at.as_millis()
    );
    probe.report("ordinary traffic restored");

    // And how long before the slot is no longer primed for the next wave.
    let heal = Instant::now();
    loop {
        let (status, _) = probe.get("/json").await;
        assert!(
            status.contains("200"),
            "ordinary traffic degraded again: {status}"
        );
        if probe.snapshot().workers[0].consecutive_failures == 0 {
            println!(
                "[{:>6} ms] failure history cleared {} ms after traffic resumed",
                probe.now_ms(),
                heal.elapsed().as_millis()
            );
            break;
        }
        assert!(
            heal.elapsed() < Duration::from_secs(60),
            "failure history never cleared"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    probe.report("fully healed");
    probe.stop().await;
}

/// E6: does ordinary traffic clear the failure history a timeout wave left
/// behind?
///
/// E5 showed a recovered pool still carrying `fail=7`. The stability reset
/// needs a generation that has been READY for 15 s and served at least one
/// request, so this measures whether normal traffic actually reaches that and
/// how long the pool stays primed for the next wave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn e6_ordinary_traffic_clears_the_failure_history() {
    let probe = Probe::start(1, 8).await;
    probe.report("before");

    for round in 1..=5_u32 {
        probe.wait_for_ready(Duration::from_secs(60)).await;
        let (status, _) = probe.get("/delay?ms=6000").await;
        println!(
            "[{:>6} ms] slow {round}: {status} | fail={}",
            probe.now_ms(),
            probe.snapshot().workers[0].consecutive_failures
        );
    }
    probe.report("timeout wave done");

    probe.wait_for_ready(Duration::from_secs(60)).await;
    let cleared = Instant::now();
    let mut reported = 0_u128;
    loop {
        let (status, _) = probe.get("/json").await;
        assert!(
            status.contains("200"),
            "ordinary traffic degraded: {status}"
        );
        let snapshot = probe.snapshot();
        let elapsed = cleared.elapsed().as_millis();
        if elapsed / 2_000 > reported {
            reported = elapsed / 2_000;
            println!(
                "[{:>6} ms] ordinary traffic for {elapsed:>6} ms | gen={} fail={}",
                probe.now_ms(),
                snapshot.workers[0].generation,
                snapshot.workers[0].consecutive_failures
            );
        }
        if snapshot.workers[0].consecutive_failures == 0 {
            println!(
                "[{:>6} ms] failure history cleared after {elapsed} ms of ordinary traffic",
                probe.now_ms()
            );
            break;
        }
        assert!(
            cleared.elapsed() < Duration::from_secs(45),
            "failure history never cleared under ordinary traffic"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    probe.report("after ordinary traffic");
    probe.stop().await;
}

/// E5: recovery once slow traffic stops.
///
/// Drives a two-worker pool as hard as E2 does, then stops and watches whether
/// the pool returns to target by itself and how long that takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn e5_recovery_after_slow_traffic_stops() {
    let probe = Arc::new(Probe::start(2, 16).await);
    probe.report("before");

    // Saturate both slots with slow requests, repeatedly, so neither slot can
    // reach the stability reset.
    for round in 1..=7_u32 {
        // Without this the burst lands while both replacements are still
        // bootstrapping, is refused instantly, and never costs a failure.
        let waited = probe.wait_for_target(Duration::from_secs(360)).await;
        let mut slow = Vec::new();
        for _ in 0..2_u32 {
            let probe = Arc::clone(&probe);
            slow.push(tokio::spawn(
                async move { probe.get("/delay?ms=6000").await },
            ));
        }
        for handle in slow {
            let _ = handle.await;
        }
        let snapshot = probe.snapshot();
        let failures: Vec<usize> = snapshot
            .workers
            .iter()
            .map(|worker| worker.consecutive_failures)
            .collect();
        println!(
            "[{:>6} ms] burst {round}: waited {:>6} ms, ready={}/{} respawns={} failures={failures:?}",
            probe.now_ms(),
            waited.as_millis(),
            snapshot.ready_workers,
            snapshot.target_workers,
            snapshot.total_respawns
        );
    }
    probe.report("slow traffic stopped");

    let recovery = Instant::now();
    loop {
        let snapshot = probe.snapshot();
        if snapshot.ready_workers == snapshot.target_workers {
            println!(
                "[{:>6} ms] pool back to target after {} ms",
                probe.now_ms(),
                recovery.elapsed().as_millis()
            );
            break;
        }
        assert!(
            recovery.elapsed() < Duration::from_secs(400),
            "pool never returned to target: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (status, elapsed) = probe.get("/json").await;
    println!(
        "[{:>6} ms] fast request after recovery -> {status} in {} ms",
        probe.now_ms(),
        elapsed.as_millis()
    );
    probe.report("after recovery");

    let probe = Arc::try_unwrap(probe).unwrap_or_else(|_| panic!("probe still shared"));
    probe.stop().await;
}

/// F1: what a short `request_timeout` costs, which is what the floor must bound.
///
/// Exceeding the deadline kills the worker, so `request_timeout` sets a slot's
/// worst-case respawn rate at `1 / (request_timeout + bootstrap)`. This drives
/// a slot continuously at several candidate values and measures the respawn
/// rate and the server's own CPU cost, so the floor comes from data instead of
/// taste. Run with `--nocapture`; it prints one row per candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn f1_respawn_cost_at_candidate_timeout_floors() {
    println!("timeout_s | respawns | window_s | respawns/min/slot | cpu_s | cpu_per_respawn_ms");
    for seconds in [1_u64, 2, 5] {
        let probe = Probe::start_with_request_timeout(1, 8, seconds).await;
        let cpu_before = process_cpu_seconds();
        let window = Instant::now();
        let mut respawns = 0_u64;
        // Drive continuously for a fixed window, always waiting for READY so
        // every request actually reaches PHP and costs a generation.
        while window.elapsed() < Duration::from_secs(20) {
            probe.wait_for_ready(Duration::from_secs(60)).await;
            let _ = probe
                .get(&format!("/delay?ms={}", seconds * 1000 + 2000))
                .await;
            respawns = probe.snapshot().total_respawns;
        }
        let elapsed = window.elapsed().as_secs_f64();
        let cpu = process_cpu_seconds() - cpu_before;
        println!(
            "{seconds:>9} | {respawns:>8} | {elapsed:>8.1} | {:>17.1} | {cpu:>5.2} | {:>18.1}",
            f64::from(u32::try_from(respawns).unwrap_or(u32::MAX)) / elapsed * 60.0,
            if respawns == 0 {
                0.0
            } else {
                cpu / f64::from(u32::try_from(respawns).unwrap_or(1)) * 1000.0
            }
        );
        probe.stop().await;
    }
}

/// Total CPU seconds this process and its children have consumed.
///
/// The PHP children are spawned by this process, so `Get-Process` on the test
/// process alone would miss them. Measuring the whole tree is what makes the
/// respawn cost visible.
#[cfg(windows)]
fn process_cpu_seconds() -> f64 {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Name php,timeout_probe* -ErrorAction SilentlyContinue | Measure-Object -Property CPU -Sum).Sum",
        ])
        .output()
        .expect("powershell should run");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

/// F2: how much of the deadline the protocol itself can consume.
///
/// `request_timeout` covers the REQUEST frame write, PHP execution and the
/// RESPONSE frame read. A floor below the time needed to move the protocol's
/// own largest legal payload would let the deadline fire on Hamster's I/O and
/// be reported as an application timeout, which is exactly the misclassification
/// this slice exists to prevent. This measures the worst legal round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn f2_worst_legal_payload_round_trip() {
    let probe = Probe::start(1, 8).await;
    // `/large` echoes the request body, so a 1 MiB body is the largest legal
    // request and produces the largest response this route can return.
    let body = vec![b'x'; 1024 * 1024];
    let mut samples = Vec::new();
    for _ in 0..12 {
        let clock = Instant::now();
        let response = post_at(probe.address, "/large", &body).await;
        let elapsed = clock.elapsed();
        assert!(
            status_line(&response).contains("200"),
            "large echo should succeed: {}",
            status_line(&response)
        );
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "1 MiB round trip over the protocol: median {:.1} ms, worst {:.1} ms across {} samples",
        samples[samples.len() / 2],
        samples[samples.len() - 1],
        samples.len()
    );
    probe.stop().await;
}

async fn post_at(address: SocketAddr, target: &str, body: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("probe client should connect");
    let head = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("probe head should be written");
    stream
        .write_all(body)
        .await
        .expect("probe body should be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("probe response should be read");
    response
}

/// G1: sustained churn with no ladder to stop it.
///
/// Removing the backoff for execution timeouts means a pathological
/// application that times out every request now respawns forever instead of
/// settling into the cooldown band. This drives exactly that worst case on a
/// multi-worker pool and checks the properties that must survive it: bounded
/// respawn rate, no process leak, and no unbounded memory growth.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn g1_every_request_times_out_forever() {
    let probe = Arc::new(Probe::start_with_request_timeout(4, 16, 1).await);
    let rss_before = server_private_mib();
    let window = Instant::now();
    let mut answered = 0_u32;
    let mut unexpected = Vec::new();

    while window.elapsed() < Duration::from_secs(30) {
        let mut wave = Vec::new();
        for _ in 0..4_u32 {
            let probe = Arc::clone(&probe);
            wave.push(tokio::spawn(
                async move { probe.get("/delay?ms=4000").await.0 },
            ));
        }
        for handle in wave {
            let status = handle.await.expect("churn request task");
            answered += 1;
            if !(status.contains("502") || status.contains("503")) {
                unexpected.push(status);
            }
        }
    }

    let snapshot = probe.snapshot();
    let elapsed = window.elapsed().as_secs_f64();
    let rss_after = server_private_mib();
    println!(
        "churn for {elapsed:.1} s: {answered} answered, respawns={}, execution_timeouts={}",
        snapshot.total_respawns, snapshot.total_execution_timeouts
    );
    println!(
        "respawns/min/slot = {:.1}; server private MiB {rss_before:.2} -> {rss_after:.2}",
        f64::from(u32::try_from(snapshot.total_respawns).unwrap_or(u32::MAX)) / elapsed * 60.0
            / 4.0
    );
    let failures: Vec<usize> = snapshot
        .workers
        .iter()
        .map(|worker| worker.consecutive_failures)
        .collect();
    println!("consecutive_failures per slot = {failures:?}");

    assert!(
        unexpected.is_empty(),
        "every request must be answered 502 or 503: {unexpected:?}"
    );
    assert!(
        failures.iter().all(|count| *count == 0),
        "sustained execution timeouts must never charge the slots: {failures:?}"
    );
    assert_eq!(
        snapshot.total_execution_timeouts, snapshot.total_respawns,
        "every respawn in this run should be a timeout replacement"
    );
    probe.report("after sustained churn");

    let probe = Arc::try_unwrap(probe).unwrap_or_else(|_| panic!("probe still shared"));
    probe.stop().await;
}

/// Private working set of this test process, which owns the pool.
#[cfg(windows)]
fn server_private_mib() -> f64 {
    // The pool runs in this process, so read this process directly.
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-Process -Id {}).PrivateMemorySize64",
                std::process::id()
            ),
        ])
        .output()
        .expect("powershell should run");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / (1024.0 * 1024.0)
}

/// Total CPU seconds this process and its children have consumed: this
/// process, the PHP children already reaped (`cutime`/`cstime`), and the ones
/// still running. Replaced workers are counted too, which is the respawn cost
/// this probe measures.
#[cfg(not(windows))]
fn process_cpu_seconds() -> f64 {
    let own = std::process::id();
    let mut ticks = proc_stat_fields(own).map_or(0, |fields| {
        fields[11..=14]
            .iter()
            .filter_map(|field| field.parse::<u64>().ok())
            .sum()
    });
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Some(fields) = proc_stat_fields(pid)
            && fields[1] == own.to_string()
        {
            ticks += fields[11..=12]
                .iter()
                .filter_map(|field| field.parse::<u64>().ok())
                .sum::<u64>();
        }
    }
    ticks as f64 / clock_ticks_per_second()
}

/// The `/proc/<pid>/stat` fields after the command name: state is index 0,
/// ppid 1, utime 11, stime 12, cutime 13 and cstime 14.
#[cfg(not(windows))]
fn proc_stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();
    (fields.len() > 14).then_some(fields)
}

#[cfg(not(windows))]
fn clock_ticks_per_second() -> f64 {
    std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
        .unwrap_or(100.0)
}

/// Private resident memory of this test process, which owns the pool: the
/// anonymous pages, which is what the Windows private working set counts.
#[cfg(not(windows))]
fn server_private_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("RssAnon:"))
                .and_then(|value| {
                    value
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<f64>()
                        .ok()
                })
        })
        .map_or(0.0, |kib| kib / 1024.0)
}
