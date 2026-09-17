//! Probe for the Pool Identity + Degradation Visibility investigation.
//!
//! Investigation instrumentation, not a regression suite. It measures what the
//! runtime state already exposes and what an operator can actually conclude
//! from it. All tests are `#[ignore]`d and need `php/8.5/php.exe`.
//!
//! `cargo test --test pool_visibility_probe <name> -- --ignored --nocapture`

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use hamster_wheel::{config, php_worker::PhpWorkerPool, server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn probe_runtime(workers: usize, request_timeout: u64) -> config::RuntimeConfig {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    let script = root.join("fixtures/php-worker/worker.php");
    assert!(
        executable.is_file(),
        "visibility probe requires repository php/8.5/php.exe"
    );
    let source = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {workers}\nqueue_capacity = 32\nrequest_timeout = {request_timeout}\n",
        toml_literal(&executable),
        toml_literal(&script)
    );
    config::compile_str(&source, &root).expect("visibility probe config should compile")
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

async fn get(address: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("probe connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("probe write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("probe read");
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or("<none>")
        .trim_end()
        .to_owned()
}

/// V1: what `ready_workers` reports while the pool is healthy and busy.
///
/// `mark_busy` decrements `ready_workers`, so the counter means "idle and
/// ready" rather than "healthy". If that is so, it is the wrong signal for a
/// degradation detector and the right one is not exposed at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn v1_ready_workers_under_healthy_load() {
    let runtime = Arc::new(probe_runtime(4, 5));
    runtime.start_workers().await.expect("pool should start");
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("probe listener");
    let address = listener.local_addr().expect("probe address");
    let router = runtime.router_arc();
    let limits = runtime.http_limits();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        server::serve_with_router(listener, router, limits, async {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let pool: &Arc<PhpWorkerPool> = runtime.php_worker().unwrap();

    let idle = pool.snapshot();
    println!(
        "idle pool: ready_workers={} of target {}",
        idle.ready_workers, idle.target_workers
    );

    // Sixteen concurrent ordinary requests against four healthy workers, with
    // a sampler watching the counter the whole time.
    let stop = Arc::new(AtomicBool::new(false));
    let lowest = Arc::new(AtomicU64::new(u64::MAX));
    let sampler = {
        let runtime = Arc::clone(&runtime);
        let stop = Arc::clone(&stop);
        let lowest = Arc::clone(&lowest);
        tokio::spawn(async move {
            let pool = runtime.php_worker().unwrap();
            let mut zero_samples = 0_u32;
            let mut samples = 0_u32;
            while !stop.load(Ordering::Relaxed) {
                let snapshot = pool.snapshot();
                let ready = snapshot.ready_workers as u64;
                lowest.fetch_min(ready, Ordering::AcqRel);
                if ready == 0 {
                    zero_samples += 1;
                }
                samples += 1;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            (samples, zero_samples)
        })
    };

    let mut served = 0_u32;
    let load = Instant::now();
    while load.elapsed() < Duration::from_secs(3) {
        let mut wave = Vec::new();
        for _ in 0..16 {
            wave.push(tokio::spawn(async move { address }));
        }
        let mut statuses = Vec::new();
        for handle in wave {
            let address = handle.await.unwrap();
            statuses.push(tokio::spawn(async move { get(address, "/json").await }));
        }
        for handle in statuses {
            let status = handle.await.unwrap();
            assert!(status.contains("200"), "healthy pool must serve: {status}");
            served += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let (samples, zero_samples) = sampler.await.expect("sampler");

    let after = pool.snapshot();
    println!(
        "healthy load: {served} requests served 200, respawns={}, execution_timeouts={}",
        after.total_respawns, after.total_execution_timeouts
    );
    println!(
        "ready_workers while healthy: lowest observed {} of target {}; {zero_samples} of {samples} samples were ZERO",
        lowest.load(Ordering::Acquire),
        after.target_workers
    );
    println!(
        "live workers derived from per-slot pids: {} of {}",
        after.workers.iter().filter(|w| w.pid.is_some()).count(),
        after.target_workers
    );

    let _ = shutdown_tx.send(());
    let _ = server.await;
    runtime.shutdown_workers().await.expect("pool should stop");
}
