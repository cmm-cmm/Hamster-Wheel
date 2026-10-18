//! The administrative listener: health, readiness and metrics.
//!
//! The 0.8 scope requires that this surface "must not silently occupy an
//! application route or be publicly exposed by default", and that is the whole
//! shape of this module. It is a **separate listener** with its own address,
//! its own small connection bound and its own three fixed routes. It never
//! sees the application's `HostRouter`, so no configuration can make
//! `/metrics` shadow an application path, and no application host can be asked
//! for it. It does not exist unless `[server.admin]` names an address, and the
//! address is the operator's: bind it to loopback or to a management
//! interface, never to the public one.
//!
//! There is no TLS and no authentication here. That is a deliberate boundary,
//! not an omission to be worked around: anyone who can reach this socket can
//! read the server's operational state, so reaching it must be a privilege the
//! network already grants. `doc/OBSERVABILITY_0_8.md` says so in the runbook
//! terms.
//!
//! The listener keeps answering while the application listeners drain, which
//! is the point of `hamster_draining` and of a 503 from `/ready`: a load
//! balancer must be able to see that this server is going away.

use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Method, Request, Response, StatusCode,
    header::{CACHE_CONTROL, CONTENT_TYPE, SERVER},
    server::conn::http1,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};

use crate::{
    access_log::AccessLog,
    config::SERVER_NAME,
    metrics::{MetricsView, ServerMetrics},
    server::{Generations, should_report_connection_error},
};

/// Connections this listener serves at once.
///
/// A scrape and a health check are small and rare. The bound exists so that
/// whatever can reach this socket cannot use it to occupy the process.
const MAX_ADMIN_CONNECTIONS: usize = 16;

/// How long an administrative client may take to send its request head.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What `[server.admin]` compiles to.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    listen: SocketAddr,
}

impl Settings {
    pub(crate) const fn new(listen: SocketAddr) -> Self {
        Self { listen }
    }

    pub const fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Whether this address is reachable from outside the machine.
    ///
    /// Used for the startup warning: binding the administrative surface to a
    /// routable address is a decision an operator must have made on purpose.
    pub fn is_public(&self) -> bool {
        !self.listen.ip().is_loopback()
    }
}

/// Everything the three routes read.
pub struct AdminState {
    pub generations: Arc<Generations>,
    pub started: Instant,
    pub draining: Arc<AtomicBool>,
    pub access_log: Option<Arc<AccessLog>>,
    pub metrics: Arc<ServerMetrics>,
}

impl AdminState {
    fn view(&self) -> MetricsView<'_> {
        MetricsView {
            config: self.generations.current(),
            started: self.started,
            draining: &self.draining,
            access_log: self.access_log.as_deref(),
            metrics: &self.metrics,
        }
    }
}

/// Binds the administrative address.
///
/// Separate from serving so that a bound address can be reported, and so an
/// address already in use fails startup rather than silently leaving the
/// surface absent.
pub async fn bind(settings: Settings) -> io::Result<TcpListener> {
    TcpListener::bind(settings.listen()).await
}

/// Serves health, readiness and metrics until `shutdown` is signalled.
pub async fn serve(
    listener: TcpListener,
    state: Arc<AdminState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if connections.len() < MAX_ADMIN_CONNECTIONS => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let state = Arc::clone(&state);
                        connections.spawn(serve_connection(stream, state));
                    }
                    Err(error) => {
                        errln!("admin listener accept failed: {error}");
                        return;
                    }
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    errln!("admin connection task failed: {error}");
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    // The administrative surface closes last and is never drained for its own
    // sake: a scrape in flight is not work the operator needs completed.
    connections.shutdown().await;
}

async fn serve_connection(stream: TcpStream, state: Arc<AdminState>) {
    let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
        let state = Arc::clone(&state);
        async move { Ok::<_, Infallible>(answer(&request, &state)) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    if let Err(error) = builder
        .serve_connection(TokioIo::new(stream), service)
        .await
        && should_report_connection_error(&error)
    {
        // A scraper that disconnects mid-answer is ordinary peer behaviour and
        // is filtered by the same rule the application listener uses.
        errln!("admin connection failed: {error}");
    }
}

/// The three routes, and nothing else.
fn answer<B>(request: &Request<B>, state: &AdminState) -> Response<Full<Bytes>> {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return text(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n".to_owned(),
        );
    }
    match request.uri().path() {
        "/health" => text(StatusCode::OK, "ok\n".to_owned()),
        "/ready" => match state.view().readiness() {
            Ok(()) => text(StatusCode::OK, "ready\n".to_owned()),
            // 503 is what a load balancer acts on, and the reason is what an
            // operator reads in a terminal.
            Err(reason) => text(StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")),
        },
        "/metrics" => {
            let mut response = text(StatusCode::OK, state.view().render());
            response.headers_mut().insert(
                CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            );
            response
        }
        _ => text(StatusCode::NOT_FOUND, "not found\n".to_owned()),
    }
}

fn text(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    // Health and readiness answers are about this instant.
    headers.insert(
        CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    headers.insert(SERVER, hyper::header::HeaderValue::from_static(SERVER_NAME));
    response
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        sync::atomic::{AtomicBool, Ordering},
    };

    use super::*;
    use crate::config;

    fn state(draining: bool) -> AdminState {
        let source = "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\
                      locations = [{ path = \"/\", handler = \"not_found\" }]\n";
        AdminState {
            generations: Generations::new(Arc::new(
                config::compile_str(source, std::path::Path::new(".")).unwrap(),
            )),
            started: Instant::now(),
            draining: Arc::new(AtomicBool::new(draining)),
            access_log: None,
            metrics: ServerMetrics::new(),
        }
    }

    fn request(method: Method, path: &str) -> Request<()> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(())
            .unwrap()
    }

    async fn body_of(response: Response<Full<Bytes>>) -> String {
        use http_body_util::BodyExt as _;
        let collected = response.into_body().collect().await.unwrap();
        String::from_utf8(collected.to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn only_the_three_documented_paths_answer() {
        let state = state(false);
        assert_eq!(
            answer(&request(Method::GET, "/health"), &state).status(),
            StatusCode::OK
        );
        assert_eq!(
            answer(&request(Method::GET, "/ready"), &state).status(),
            StatusCode::OK
        );
        assert_eq!(
            answer(&request(Method::GET, "/metrics"), &state).status(),
            StatusCode::OK
        );
        for path in ["/", "/index.html", "/metrics/", "/health/x", "/admin"] {
            assert_eq!(
                answer(&request(Method::GET, path), &state).status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn a_write_method_is_refused_before_a_path_is_considered() {
        let state = state(false);
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let response = answer(&request(method.clone(), "/metrics"), &state);
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method}"
            );
        }
        // HEAD is how a health checker asks without a body.
        assert_eq!(
            answer(&request(Method::HEAD, "/health"), &state).status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_draining_server_is_alive_but_not_ready() {
        let state = state(true);
        assert_eq!(
            answer(&request(Method::GET, "/health"), &state).status(),
            StatusCode::OK,
            "a draining server is still alive"
        );
        let ready = answer(&request(Method::GET, "/ready"), &state);
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_of(ready).await, "draining\n");
    }

    #[tokio::test]
    async fn the_exposition_reports_readiness_and_the_fixed_histogram() {
        let state = state(false);
        state.metrics.request_finished(200, 120, true, 1_500);
        state.metrics.request_finished(502, 0, true, 3_000_000);
        state.metrics.request_finished(200, 7, false, 900);
        let body = body_of(answer(&request(Method::GET, "/metrics"), &state)).await;

        for line in [
            "hamster_up 1",
            "hamster_ready 1",
            "hamster_draining 0",
            "hamster_requests_total{status=\"2xx\"} 2",
            "hamster_requests_total{status=\"5xx\"} 1",
            "hamster_requests_incomplete_total 1",
            "hamster_response_bytes_total 127",
            "hamster_request_duration_seconds_count 3",
        ] {
            assert!(body.contains(line), "{line} missing from:\n{body}");
        }
        // Buckets are cumulative and end at +Inf with every observation.
        assert!(body.contains("hamster_request_duration_seconds_bucket{le=\"0.001\"} 1"));
        assert!(body.contains("hamster_request_duration_seconds_bucket{le=\"0.005\"} 2"));
        assert!(body.contains("hamster_request_duration_seconds_bucket{le=\"+Inf\"} 3"));
        // Every metric is declared before it is used.
        for line in body.lines().filter(|line| !line.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            let base = name
                .trim_end_matches("_bucket")
                .trim_end_matches("_count")
                .trim_end_matches("_sum");
            assert!(
                body.contains(&format!("# TYPE {base} ")),
                "{name} has no TYPE line"
            );
        }
        // Nothing is labelled by anything a client could choose.
        assert!(!body.contains("path="), "{body}");
        assert!(!body.contains("peer="), "{body}");
    }

    #[test]
    fn a_loopback_address_is_not_reported_as_public() {
        assert!(!Settings::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 9_090))).is_public());
        assert!(Settings::new(SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 9_090))).is_public());
    }

    #[test]
    fn readiness_ignores_backends_hamster_does_not_own() {
        // No PHP pool is configured here, so readiness is about this process
        // and nothing else; a FastCGI or proxy endpoint being down must not
        // make a healthy Hamster report itself unready.
        let state = state(false);
        assert!(state.view().readiness().is_ok());
        state.draining.store(true, Ordering::Release);
        assert_eq!(state.view().readiness().unwrap_err(), "draining");
    }
}
