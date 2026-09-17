//! Probe for the multiple persistent application pools investigation.
//!
//! Investigation instrumentation, not a regression suite. It exists to turn
//! the review's multi-pool concerns into measurements.
//!
//! IT DOES NOT COMPILE-FAIL BUT IT WILL NOT PASS AGAINST THIS TREE. Every test
//! needs the one-`php_worker`-location guard in `config::validate_raw` lifted,
//! which was done temporarily to take these measurements and then restored.
//! Without that, `multi_pool_runtime` panics on a validation error. Results are
//! recorded in `benchmarks/results/multipool_investigation_20260917.md`.
//! All tests are `#[ignore]`d and need `php/8.5/php.exe`.
//!
//! `cargo test --test multipool_probe <name> -- --ignored --nocapture`

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use hamster_wheel::{config, server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// One application: its virtual host, its worker script, and its pool size.
///
/// Each pool gets its own host with its location at `/`, which is both the
/// realistic multi-tenant shape and the only one where the shared fixture's
/// routes work: a pool mounted at `/a` receives `/a/crash` as its path, which
/// the fixture does not recognise, so nothing would actually crash.
struct PoolSpec {
    host: &'static str,
    script: &'static str,
    workers: usize,
}

/// Builds the multi-pool runtime, or `None` when this tree still refuses one.
///
/// Returning `None` rather than panicking keeps these probes from turning into
/// four permanent failures in the ignored suite on a tree where the guard is
/// in place, which is every tree until the owner lifts it.
fn multi_pool_runtime(specs: &[PoolSpec]) -> Option<config::RuntimeConfig> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "multi-pool probe requires repository php/8.5/php.exe"
    );
    let mut source = String::from("[server]\nlisten = \"127.0.0.1:8080\"\n");
    for spec in specs {
        let script = root.join("fixtures/php-worker").join(spec.script);
        source.push_str(&format!(
            "\n[[hosts]]\nname = \"{}\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {}\nqueue_capacity = 8\nrequest_timeout = 5\n",
            spec.host,
            toml_literal(&executable),
            toml_literal(&script),
            spec.workers
        ));
    }
    match config::compile_str(&source, &root) {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            println!("SKIPPED: this tree still permits one php_worker location ({error})");
            None
        }
    }
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// Every `php.exe` whose command line names this repository's fixtures.
///
/// The pools spawn children of this test process, so counting repository PHP
/// processes is what detects an orphan the pool no longer tracks.
fn repository_php_pids() -> Vec<u32> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='php.exe'\" | Where-Object { $_.CommandLine -like '*HamsterWheel*' } | ForEach-Object { $_.ProcessId }",
        ])
        .output()
        .expect("powershell should run");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

async fn get(address: SocketAddr, host: &str, target: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("probe connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
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

/// M1: a second pool that cannot start, after a first pool already started.
///
/// The review's concern was that `start_workers` returns on the first failure
/// and leaves an already-started pool running with nothing to shut it down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe and a tree with the one-pool guard lifted"]
async fn m1_partial_startup_failure() {
    let before = repository_php_pids();
    println!("repository php.exe before: {before:?}");

    let Some(runtime) = multi_pool_runtime(&[
        PoolSpec {
            host: "healthy.test",
            script: "worker.php",
            workers: 2,
        },
        PoolSpec {
            host: "broken.test",
            script: "exit_before_ready.php",
            workers: 1,
        },
    ]) else {
        return;
    };
    let runtime = Arc::new(runtime);

    let error = runtime
        .start_workers()
        .await
        .expect_err("the second pool must fail to start");
    println!("start_workers error: {error}");

    // The moment of interest: startup has failed and returned. Nothing has
    // been shut down, because that is what the caller would now have to do.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let leaked: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!(
        "repository php.exe still alive after the failed startup: {leaked:?} ({} process(es))",
        leaked.len()
    );

    // And whether the caller can still clean up after the fact.
    let cleanup = runtime.shutdown_workers().await;
    println!("shutdown_workers after a failed startup: {cleanup:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!("repository php.exe after cleanup: {after:?}");
}

/// M2: two healthy pools serving side by side, and whether one pool losing
/// every worker disturbs the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe and a tree with the one-pool guard lifted"]
async fn m2_failure_isolation_between_pools() {
    let Some(runtime) = multi_pool_runtime(&[
        PoolSpec {
            host: "a.test",
            script: "worker.php",
            workers: 2,
        },
        PoolSpec {
            host: "b.test",
            script: "worker.php",
            workers: 2,
        },
    ]) else {
        return;
    };
    let runtime = Arc::new(runtime);
    let starts = runtime
        .start_workers()
        .await
        .expect("both pools should start");
    println!("started {} workers across both pools:", starts.len());
    for start in &starts {
        println!("  pool {} pid {}", start.label, start.pid);
    }

    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("probe listener");
    let address = listener.local_addr().expect("probe address");
    let router = runtime.router_arc();
    let limits = runtime.http_limits();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let serving = tokio::spawn(async move {
        server::serve_with_router(listener, router, limits, async {
            let _ = shutdown_rx.await;
        })
        .await
    });

    println!(
        "baseline: a.test -> {}",
        get(address, "a.test", "/json").await
    );
    println!(
        "baseline: b.test -> {}",
        get(address, "b.test", "/json").await
    );

    // Take pool /a apart: crash both of its workers repeatedly while /b is
    // sampled. `/crash` exits mid-transaction, an infrastructure failure.
    let mut a_results = Vec::new();
    let mut b_results = Vec::new();
    for round in 0..6 {
        a_results.push(get(address, "a.test", "/crash?ms=0").await);
        b_results.push(get(address, "b.test", "/json").await);
        if round == 2 {
            println!(
                "  (mid-run) a.test -> {}, b.test -> {}",
                a_results[round], b_results[round]
            );
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    let b_ok = b_results
        .iter()
        .filter(|status| status.contains("200"))
        .count();
    println!(
        "while a.test was being crashed {} times: b.test answered 200 for {} of {} requests",
        a_results.len(),
        b_ok,
        b_results.len()
    );
    println!("  a.test statuses: {a_results:?}");
    println!("  b.test statuses: {b_results:?}");

    // Let /a settle, then confirm both serve again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!(
        "after settling: a.test -> {}",
        get(address, "a.test", "/json").await
    );
    println!(
        "after settling: b.test -> {}",
        get(address, "b.test", "/json").await
    );

    let _ = shutdown_tx.send(());
    let _ = serving.await;
    let stopped = runtime.shutdown_workers().await;
    println!("shutdown_workers: {stopped:?}");
}

/// M3: shutdown-all when one pool's shutdown reports an error.
///
/// The review's concern was that `shutdown_workers` returns on the first
/// error and never reaches the remaining pools.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe and a tree with the one-pool guard lifted"]
async fn m3_shutdown_all_reaches_every_pool() {
    let before = repository_php_pids();
    let Some(runtime) = multi_pool_runtime(&[
        PoolSpec {
            host: "a.test",
            script: "worker.php",
            workers: 2,
        },
        PoolSpec {
            host: "b.test",
            script: "worker.php",
            workers: 2,
        },
    ]) else {
        return;
    };
    let runtime = Arc::new(runtime);
    runtime
        .start_workers()
        .await
        .expect("all pools should start");
    let started: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!("three pools started {} php.exe children", started.len());

    let result = runtime.shutdown_workers().await;
    println!("shutdown_workers: {result:?}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let leaked: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!(
        "php.exe left after shutdown_workers: {leaked:?} ({} of {})",
        leaked.len(),
        started.len()
    );
}

/// M4: shutdown-all when the *first* pool's shutdown reports an error.
///
/// `shutdown_workers` uses `?` inside its loop. If a pool's shutdown returns
/// an error, this measures whether the pools after it are ever reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe and a tree with the one-pool guard lifted"]
async fn m4_shutdown_all_when_an_earlier_pool_fails() {
    let before = repository_php_pids();
    let Some(runtime) = multi_pool_runtime(&[
        // The stubborn pool is deliberately first, so anything after it is
        // only reached if the loop survives an error.
        PoolSpec {
            host: "stubborn.test",
            script: "ignores_shutdown.php",
            workers: 1,
        },
        PoolSpec {
            host: "healthy-a.test",
            script: "worker.php",
            workers: 2,
        },
        PoolSpec {
            host: "healthy-b.test",
            script: "worker.php",
            workers: 2,
        },
    ]) else {
        return;
    };
    let runtime = Arc::new(runtime);
    runtime
        .start_workers()
        .await
        .expect("all pools should start");
    let started: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!(
        "started {} php.exe children across three pools",
        started.len()
    );

    let result = runtime.shutdown_workers().await;
    println!("shutdown_workers returned: {result:?}");

    // Give the healthy pools every chance to have been stopped.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let leaked: Vec<u32> = repository_php_pids()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    println!(
        "php.exe still alive after shutdown_workers: {leaked:?} ({} of {})",
        leaked.len(),
        started.len()
    );

    // Clean up whatever was left, so the probe does not leave processes behind.
    for pid in &leaked {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}
