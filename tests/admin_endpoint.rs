//! The administrative listener, end to end against a real socket.
//!
//! The 0.8 requirement these tests exist for is that this surface "must not
//! silently occupy an application route or be publicly exposed by default", so
//! the first test asks the application listener for the administrative paths
//! and requires it to answer as the application, not as the admin.

use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use hamster_wheel::{
    admin::{self, AdminState},
    config,
    metrics::ServerMetrics,
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

/// An application listener and an administrative one over the same config.
struct Servers {
    application: SocketAddr,
    admin: SocketAddr,
    metrics: Arc<ServerMetrics>,
    draining: Arc<AtomicBool>,
    stop_application: oneshot::Sender<()>,
    stop_admin: tokio::sync::watch::Sender<bool>,
}

const SOURCE: &str = "[server]\nlisten = \"127.0.0.1:0\"\n\n[server.admin]\nlisten = \"127.0.0.1:0\"\n\n\
     [[hosts]]\nname = \"localhost\"\naliases = [\"127.0.0.1\"]\n\n\
     [[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n";

async fn start() -> Servers {
    let runtime = Arc::new(config::compile_str(SOURCE, std::path::Path::new(".")).unwrap());
    let settings = runtime.admin().expect("the configuration asked for admin");
    let metrics = ServerMetrics::new();
    let draining = Arc::new(AtomicBool::new(false));

    // Port 0 in the configuration, so the bound address is read back exactly
    // as the server does it.
    let admin_socket = admin::bind(settings).await.expect("admin bind");
    let admin_address = admin_socket.local_addr().unwrap();
    let state = Arc::new(AdminState {
        config: Arc::clone(&runtime),
        started: Instant::now(),
        draining: Arc::clone(&draining),
        access_log: None,
        metrics: Arc::clone(&metrics),
    });
    let (stop_admin, admin_stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(admin::serve(admin_socket, state, admin_stop_rx));

    let application_socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let application = application_socket.local_addr().unwrap();
    let (stop_application, application_stop_rx) = oneshot::channel::<()>();
    tokio::spawn(server::serve_listeners(
        vec![server::Listener::cleartext(application_socket)],
        runtime.router_arc(),
        runtime.http_limits(),
        None,
        Some(Arc::clone(&metrics)),
        async move {
            let _ = application_stop_rx.await;
        },
    ));

    Servers {
        application,
        admin: admin_address,
        metrics,
        draining,
        stop_application,
        stop_admin,
    }
}

impl Servers {
    fn stop(self) {
        let _ = self.stop_application.send(());
        let _ = self.stop_admin.send(true);
    }
}

/// One request, answered in full, as text.
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

fn status_line(answer: &str) -> &str {
    answer.lines().next().unwrap_or_default()
}

#[tokio::test]
async fn the_application_listener_does_not_serve_the_administrative_paths() {
    let servers = start().await;

    // The application answers these as the application: this configuration
    // routes everything to `not_found`, so a 404 proves the admin surface did
    // not take the path, and the body is never an exposition.
    for path in ["/metrics", "/health", "/ready"] {
        let answer = request(servers.application, &get(path)).await;
        assert!(
            status_line(&answer).starts_with("HTTP/1.1 404"),
            "{path} on the application listener: {answer}"
        );
        assert!(
            !answer.contains("hamster_up"),
            "{path} leaked the exposition on the application listener: {answer}"
        );
    }

    // And the admin listener serves nothing of the application's.
    let answer = request(servers.admin, &get("/")).await;
    assert!(status_line(&answer).starts_with("HTTP/1.1 404"), "{answer}");
    servers.stop();
}

#[tokio::test]
async fn health_readiness_and_metrics_answer_on_the_administrative_listener() {
    let servers = start().await;

    let health = request(servers.admin, &get("/health")).await;
    assert!(status_line(&health).starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.ends_with("ok\n"), "{health}");

    let ready = request(servers.admin, &get("/ready")).await;
    assert!(status_line(&ready).starts_with("HTTP/1.1 200"), "{ready}");
    assert!(ready.ends_with("ready\n"), "{ready}");

    let metrics = request(servers.admin, &get("/metrics")).await;
    assert!(
        status_line(&metrics).starts_with("HTTP/1.1 200"),
        "{metrics}"
    );
    // The content type a scraper requires.
    assert!(
        metrics.contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "{metrics}"
    );
    for line in ["hamster_up 1", "hamster_ready 1", "hamster_draining 0"] {
        assert!(metrics.contains(line), "{line} missing from {metrics}");
    }
    servers.stop();
}

#[tokio::test]
async fn a_draining_server_answers_health_but_refuses_readiness() {
    let servers = start().await;
    servers
        .draining
        .store(true, std::sync::atomic::Ordering::Release);

    let health = request(servers.admin, &get("/health")).await;
    assert!(
        status_line(&health).starts_with("HTTP/1.1 200"),
        "a draining server is still alive: {health}"
    );
    let ready = request(servers.admin, &get("/ready")).await;
    assert!(
        status_line(&ready).starts_with("HTTP/1.1 503"),
        "a draining server must not be sent traffic: {ready}"
    );
    assert!(ready.ends_with("draining\n"), "{ready}");
    let metrics = request(servers.admin, &get("/metrics")).await;
    assert!(metrics.contains("hamster_ready 0"), "{metrics}");
    assert!(metrics.contains("hamster_draining 1"), "{metrics}");
    servers.stop();
}

#[tokio::test]
async fn application_traffic_moves_the_counters_the_exposition_reports() {
    let servers = start().await;

    const REQUESTS: usize = 5;
    for _ in 0..REQUESTS {
        let answer = request(servers.application, &get("/anything")).await;
        assert!(status_line(&answer).starts_with("HTTP/1.1 404"), "{answer}");
    }
    // The counters are read through the exposition, not through the struct, so
    // this is what a scraper would actually have seen.
    let metrics = request(servers.admin, &get("/metrics")).await;
    assert!(
        metrics.contains(&format!(
            "hamster_requests_total{{status=\"4xx\"}} {REQUESTS}"
        )),
        "{metrics}"
    );
    assert!(
        metrics.contains("hamster_requests_total{status=\"5xx\"} 0"),
        "{metrics}"
    );
    assert!(
        metrics.contains(&format!(
            "hamster_request_duration_seconds_count {REQUESTS}"
        )),
        "{metrics}"
    );
    assert!(
        metrics.contains("hamster_requests_incomplete_total 0"),
        "{metrics}"
    );
    // Connections: five were accepted and all have ended.
    assert!(
        metrics.contains("hamster_connections_active 0"),
        "{metrics}"
    );
    assert!(
        metrics.contains(&format!("hamster_connections_total {REQUESTS}")),
        "{metrics}"
    );
    // A scrape is not application traffic and must not be counted as it.
    assert_eq!(servers.metrics.requests_total(), REQUESTS as u64);
    servers.stop();
}

#[tokio::test]
async fn the_administrative_listener_refuses_writes_and_unknown_paths() {
    let servers = start().await;

    let posted = request(
        servers.admin,
        "POST /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status_line(&posted).starts_with("HTTP/1.1 405"), "{posted}");

    for path in ["/", "/metrics/", "/admin", "/healthz"] {
        let answer = request(servers.admin, &get(path)).await;
        assert!(
            status_line(&answer).starts_with("HTTP/1.1 404"),
            "{path}: {answer}"
        );
    }
    servers.stop();
}

#[tokio::test]
async fn a_configuration_without_the_section_has_no_administrative_surface() {
    let source = SOURCE.replace("[server.admin]\nlisten = \"127.0.0.1:0\"\n\n", "");
    let runtime = config::compile_str(&source, std::path::Path::new(".")).unwrap();
    assert!(
        runtime.admin().is_none(),
        "the administrative surface must be off unless configured"
    );
}
