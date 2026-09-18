//! Permanent regressions for multiple persistent application pools.
//!
//! All tests are `#[ignore]`d because they need `php/8.5/php.exe`.
//!
//! `cargo test --test multipool_probe <name> -- --ignored --nocapture`

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use hamster_wheel::{
    config,
    server::{self, ServerError},
};
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

fn multi_pool_runtime(
    specs: &[PoolSpec],
    pid_record: Option<(&str, &Path)>,
) -> config::RuntimeConfig {
    multi_pool_runtime_at(
        specs,
        pid_record,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
    )
}

fn multi_pool_runtime_at(
    specs: &[PoolSpec],
    pid_record: Option<(&str, &Path)>,
    listen: SocketAddr,
) -> config::RuntimeConfig {
    multi_pool_runtime_with_server(specs, pid_record, listen, "")
}

fn multi_pool_runtime_with_server(
    specs: &[PoolSpec],
    pid_record: Option<(&str, &Path)>,
    listen: SocketAddr,
    server_settings: &str,
) -> config::RuntimeConfig {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let executable = root.join("php/8.5/php.exe");
    assert!(
        executable.is_file(),
        "multi-pool probe requires repository php/8.5/php.exe"
    );
    let mut source = format!("[server]\nlisten = \"{listen}\"\n{server_settings}");
    for spec in specs {
        let script = root.join("fixtures/php-worker").join(spec.script);
        source.push_str(&format!(
            "\n[[hosts]]\nname = \"{}\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {}\nqueue_capacity = 8\nrequest_timeout = 5\n",
            spec.host,
            toml_literal(&executable),
            toml_literal(&script),
            spec.workers
        ));
        if let Some((host, marker)) = pid_record
            && spec.host == host
        {
            source.push_str(&format!(
                "\n[hosts.locations.php.env]\nHAMSTER_TEST_PID_FILE = {}\n",
                toml_literal(marker)
            ));
        }
    }
    config::compile_str(&source, &root).expect("multiple PHP pools should compile")
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn marker_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hamster-wheel-{name}-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn recorded_pids(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .expect("PID marker should exist")
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
            ),
        ])
        .status()
        .expect("powershell should run")
        .success()
}

#[cfg(not(windows))]
fn process_is_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

async fn assert_processes_reaped(pids: &[u32]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let alive: Vec<_> = pids
            .iter()
            .copied()
            .filter(|pid| process_is_alive(*pid))
            .collect();
        if alive.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "PHP processes survived pool cleanup: {alive:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn get(address: SocketAddr, host: &str, target: &str) -> String {
    request(
        address,
        &format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await
}

async fn request(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("probe connect");
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
#[ignore = "requires php/8.5/php.exe"]
async fn m1_partial_startup_failure() {
    let marker = marker_path("multipool-partial-startup");

    let runtime = multi_pool_runtime(
        &[
            PoolSpec {
                host: "healthy.test",
                script: "record_pid_worker.php",
                workers: 2,
            },
            PoolSpec {
                host: "broken.test",
                script: "exit_before_ready.php",
                workers: 1,
            },
        ],
        Some(("healthy.test", &marker)),
    );
    let runtime = Arc::new(runtime);

    let error = runtime
        .start_workers()
        .await
        .expect_err("the second pool must fail to start");
    assert_eq!(error.primary().label(), "broken.test/");
    assert!(
        error.additional().is_empty(),
        "unexpected cleanup error: {error}"
    );
    let pids = recorded_pids(&marker);
    assert_eq!(
        pids.len(),
        2,
        "both earlier workers must have reached READY"
    );
    assert_processes_reaped(&pids).await;
    assert!(
        runtime
            .php_workers()
            .iter()
            .all(|pool| pool.pids().is_empty()),
        "failed startup must leave every pool stopped"
    );
    runtime.shutdown_workers().await.unwrap();
    let _ = tokio::fs::remove_file(marker).await;
}

/// M2: two healthy pools serving side by side, and whether one pool losing
/// every worker disturbs the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn m2_failure_isolation_between_pools() {
    let runtime = multi_pool_runtime(
        &[
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
        ],
        None,
    );
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

    assert!(get(address, "a.test", "/json").await.contains("200"));
    assert!(get(address, "b.test", "/json").await.contains("200"));

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

    assert!(
        a_results
            .iter()
            .all(|status| status.contains("502") || status.contains("503")),
        "crashed application responses: {a_results:?}"
    );
    assert!(
        b_results.iter().all(|status| status.contains("200")),
        "pool A failures affected pool B: {b_results:?}"
    );

    // Let /a settle, then confirm both serve again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(get(address, "a.test", "/json").await.contains("200"));
    assert!(get(address, "b.test", "/json").await.contains("200"));

    let _ = shutdown_tx.send(());
    let _ = serving.await;
    runtime.shutdown_workers().await.unwrap();
}

/// Pool ownership is independent, but request-body memory is intentionally a
/// process resource. A reservation held by A must therefore be visible to B,
/// and disconnecting A must return it for B to use.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn body_budget_is_shared_across_pools_and_released_on_disconnect() {
    let runtime = Arc::new(multi_pool_runtime_with_server(
        &[
            PoolSpec {
                host: "a.test",
                script: "worker.php",
                workers: 1,
            },
            PoolSpec {
                host: "b.test",
                script: "worker.php",
                workers: 1,
            },
        ],
        None,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
        "body_read_timeout = 5\nmax_body_memory = 1048576\n",
    ));
    runtime.start_workers().await.unwrap();
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let router = runtime.router_arc();
    let limits = runtime.http_limits();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let serving = tokio::spawn(async move {
        server::serve_with_router(listener, router, limits, async {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let mut held = TcpStream::connect(address).await.unwrap();
    held.write_all(
        b"POST /echo HTTP/1.1\r\nHost: a.test\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\nx",
    )
    .await
    .unwrap();

    let small_post =
        "POST /echo HTTP/1.1\r\nHost: b.test\r\nContent-Length: 1\r\nConnection: close\r\n\r\ny";
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if request(address, small_post).await.contains("503") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pool A should reserve the shared body budget seen by pool B");
    assert!(
        get(address, "b.test", "/json").await.contains("200"),
        "body pressure in A must not make B's body-free traffic unavailable"
    );

    drop(held);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if request(address, small_post).await.contains("201") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnecting A should release budget for B");

    shutdown_tx.send(()).unwrap();
    serving.await.unwrap().unwrap();
    runtime.shutdown_workers().await.unwrap();
}

/// M3: shutdown-all when one pool's shutdown reports an error.
///
/// The review's concern was that `shutdown_workers` returns on the first
/// error and never reaches the remaining pools.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn m3_shutdown_all_reaches_every_pool() {
    let runtime = multi_pool_runtime(
        &[
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
        ],
        None,
    );
    let runtime = Arc::new(runtime);
    runtime
        .start_workers()
        .await
        .expect("all pools should start");
    let started: Vec<u32> = runtime
        .php_workers()
        .iter()
        .flat_map(|pool| pool.pids())
        .collect();
    assert_eq!(started.len(), 4);
    runtime.shutdown_workers().await.unwrap();
    assert_processes_reaped(&started).await;
}

/// M4: shutdown-all when the *first* pool's shutdown reports an error.
///
/// A pool shutdown error must not prevent the pools after it being reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn m4_shutdown_all_when_an_earlier_pool_fails() {
    let runtime = multi_pool_runtime(
        &[
            // The stubborn pool is deliberately first, so anything after it
            // is reached only if shutdown remains best-effort.
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
        ],
        None,
    );
    let runtime = Arc::new(runtime);
    runtime
        .start_workers()
        .await
        .expect("all pools should start");
    let started: Vec<u32> = runtime
        .php_workers()
        .iter()
        .flat_map(|pool| pool.pids())
        .collect();
    assert_eq!(started.len(), 5);

    let error = runtime
        .shutdown_workers()
        .await
        .expect_err("the stubborn pool must report its forced kill");
    assert_eq!(error.primary().label(), "stubborn.test/");
    assert!(error.additional().is_empty());
    assert_processes_reaped(&started).await;
    assert!(
        runtime
            .php_workers()
            .iter()
            .all(|pool| pool.pids().is_empty()),
        "every pool must reach stopped even when an earlier one fails"
    );
}

/// Aggregation must retain every pool failure, not merely continue cleanup and
/// then report whichever error happened last.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires php/8.5/php.exe"]
async fn shutdown_reports_every_failing_pool_and_reaps_all() {
    let runtime = multi_pool_runtime(
        &[
            PoolSpec {
                host: "stubborn-a.test",
                script: "ignores_shutdown.php",
                workers: 1,
            },
            PoolSpec {
                host: "healthy.test",
                script: "worker.php",
                workers: 1,
            },
            PoolSpec {
                host: "stubborn-b.test",
                script: "ignores_shutdown.php",
                workers: 1,
            },
        ],
        None,
    );
    runtime.start_workers().await.unwrap();
    let pids: Vec<u32> = runtime
        .php_workers()
        .iter()
        .flat_map(|pool| pool.pids())
        .collect();
    assert_eq!(pids.len(), 3);

    let error = runtime.shutdown_workers().await.unwrap_err();
    assert_eq!(error.primary().label(), "stubborn-a.test/");
    assert_eq!(error.additional().len(), 1);
    assert_eq!(error.additional()[0].label(), "stubborn-b.test/");
    assert_processes_reaped(&pids).await;
    assert!(
        runtime
            .php_workers()
            .iter()
            .all(|pool| pool.pids().is_empty())
    );
}

/// The pool that failed startup remains the primary cause even when rollback
/// of an earlier pool also reports a forced kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn startup_preserves_primary_failure_and_reports_cleanup_failure() {
    let marker = marker_path("multipool-startup-cleanup-failure");
    let runtime = multi_pool_runtime(
        &[
            PoolSpec {
                host: "stubborn.test",
                script: "ignores_shutdown.php",
                workers: 1,
            },
            PoolSpec {
                host: "broken.test",
                script: "exit_before_ready.php",
                workers: 1,
            },
        ],
        Some(("stubborn.test", &marker)),
    );

    let error = runtime
        .start_workers()
        .await
        .expect_err("the second pool must fail startup");
    assert_eq!(error.primary().label(), "broken.test/");
    assert_eq!(error.additional().len(), 1);
    assert_eq!(error.additional()[0].label(), "stubborn.test/");
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with("PHP worker pool \"broken.test/\""));
    assert!(diagnostic.contains("additional cleanup failure"));
    assert!(diagnostic.contains("stubborn.test/"));

    let pids = recorded_pids(&marker);
    assert_eq!(pids.len(), 1);
    assert_processes_reaped(&pids).await;
    let _ = tokio::fs::remove_file(marker).await;
}

/// `server::run` must not bind until every application pool has reached READY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn startup_failure_never_accepts_traffic_and_reaps_started_pools() {
    let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let marker = marker_path("multipool-server-startup-failure");
    let runtime = Arc::new(multi_pool_runtime_at(
        &[
            PoolSpec {
                host: "healthy.test",
                script: "record_pid_worker.php",
                workers: 1,
            },
            PoolSpec {
                host: "slow-broken.test",
                script: "slow_ready.php",
                workers: 1,
            },
        ],
        Some(("healthy.test", &marker)),
        address,
    ));

    let serving = tokio::spawn(server::run(Arc::clone(&runtime)));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime.php_workers()[0].snapshot().live_workers == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first pool should be running while the second bootstraps");
    let connect = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(address)).await;
    assert!(
        !matches!(connect, Ok(Ok(_))),
        "server accepted traffic before all pools reached READY"
    );

    let error = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("failed startup should be bounded")
        .expect("server task should not panic")
        .expect_err("slow pool must fail before READY");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("slow-broken.test/"), "{diagnostic}");
    let pids = recorded_pids(&marker);
    assert_eq!(pids.len(), 1);
    assert_processes_reaped(&pids).await;
    assert!(
        runtime
            .php_workers()
            .iter()
            .all(|pool| pool.pids().is_empty())
    );
    let _ = tokio::fs::remove_file(marker).await;
}

/// Failure after all pools start but before server ownership is published must
/// retain the socket error as primary while still reporting pool cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires php/8.5/php.exe"]
async fn bind_failure_remains_primary_when_pool_cleanup_fails() {
    let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = occupied.local_addr().unwrap();
    let marker = marker_path("multipool-bind-cleanup-failure");
    let runtime = Arc::new(multi_pool_runtime_at(
        &[
            PoolSpec {
                host: "stubborn.test",
                script: "ignores_shutdown.php",
                workers: 1,
            },
            PoolSpec {
                host: "healthy.test",
                script: "worker.php",
                workers: 1,
            },
        ],
        Some(("stubborn.test", &marker)),
        address,
    ));

    let error = server::run(Arc::clone(&runtime))
        .await
        .expect_err("the occupied address must reject the server bind");
    match error {
        ServerError::IoWithWorkerCleanup { source, cleanup } => {
            assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
            assert_eq!(cleanup.primary().label(), "stubborn.test/");
            assert!(cleanup.additional().is_empty());
        }
        other => panic!("bind and cleanup failures lost causal structure: {other:?}"),
    }

    let pids = recorded_pids(&marker);
    assert_eq!(pids.len(), 1);
    assert_processes_reaped(&pids).await;
    assert!(
        runtime
            .php_workers()
            .iter()
            .all(|pool| pool.pids().is_empty())
    );
    drop(occupied);
    let _ = tokio::fs::remove_file(marker).await;
}
