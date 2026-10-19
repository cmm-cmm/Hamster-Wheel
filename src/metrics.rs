//! Bounded counters and their Prometheus text exposition.
//!
//! Two rules shape this module. The first is that cardinality is bounded by
//! the configuration, never by traffic: labels come from pool, upstream,
//! FastCGI and certificate names an operator wrote down, plus a fixed status
//! class and a fixed set of duration buckets. Nothing here is labelled by
//! request path, by peer or by exact status code, so a server cannot be made
//! to grow its own metric set by being asked for odd URLs.
//!
//! The second is that the request path only touches relaxed atomics. Reading
//! and formatting happens on the admin listener's task, from snapshots the
//! subsystems already keep for `--control status`.
//!
//! The text format is written by hand, like the access log's JSON, rather
//! than through a client library: the exposition is eleven lines of shape and
//! a dependency would own a hot-path type for it.

use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::{access_log::AccessLog, config::RuntimeConfig};

/// Upper bounds of the request-duration histogram, in seconds.
///
/// Fixed at compile time: a configurable histogram is a configurable metric
/// cardinality. These cover a static file hit through a timed-out backend.
const DURATION_BUCKETS_SECONDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Status classes counted separately: `1xx` through `5xx`.
const STATUS_CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

/// HTTP-level counters Hamster did not already keep somewhere else.
///
/// Everything about pools, upstreams, FastCGI endpoints and TLS is read from
/// those subsystems' own snapshots instead of being counted twice.
#[derive(Debug, Default)]
pub struct ServerMetrics {
    connections_total: AtomicU64,
    connections_active: AtomicU64,
    requests_total: AtomicU64,
    requests_by_class: [AtomicU64; STATUS_CLASSES.len()],
    requests_incomplete: AtomicU64,
    response_bytes_total: AtomicU64,
    duration_micros_total: AtomicU64,
    /// One counter per bucket, plus one for everything above the last bound.
    duration_buckets: [AtomicU64; DURATION_BUCKETS_SECONDS.len() + 1],
}

impl ServerMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One accepted connection has begun being served.
    pub(crate) fn connection_opened(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
        self.connections_active.fetch_add(1, Ordering::Relaxed);
    }

    /// Its task has ended, for any reason.
    pub(crate) fn connection_closed(&self) {
        self.connections_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// One request has finished, successfully or not.
    ///
    /// Called from the same place the access log's record is produced, so the
    /// two can never disagree about a request's status, size or outcome.
    pub(crate) fn request_finished(
        &self,
        status: u16,
        bytes: u64,
        complete: bool,
        total_micros: u64,
    ) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        if let Some(class) = status_class_index(status) {
            self.requests_by_class[class].fetch_add(1, Ordering::Relaxed);
        }
        if !complete {
            self.requests_incomplete.fetch_add(1, Ordering::Relaxed);
        }
        self.response_bytes_total
            .fetch_add(bytes, Ordering::Relaxed);
        self.duration_micros_total
            .fetch_add(total_micros, Ordering::Relaxed);
        self.duration_buckets[duration_bucket_index(total_micros)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn connections_active(&self) -> u64 {
        self.connections_active.load(Ordering::Relaxed)
    }

    pub fn requests_total(&self) -> u64 {
        self.requests_total.load(Ordering::Relaxed)
    }
}

/// `1xx`..`5xx` as an index, or `None` for a status outside those classes.
fn status_class_index(status: u16) -> Option<usize> {
    match status {
        100..=199 => Some(0),
        200..=299 => Some(1),
        300..=399 => Some(2),
        400..=499 => Some(3),
        500..=599 => Some(4),
        _ => None,
    }
}

/// The bucket a duration falls in; the last index is everything above the
/// final bound.
fn duration_bucket_index(total_micros: u64) -> usize {
    let seconds = total_micros as f64 / 1_000_000.0;
    DURATION_BUCKETS_SECONDS
        .iter()
        .position(|bound| seconds <= *bound)
        .unwrap_or(DURATION_BUCKETS_SECONDS.len())
}

/// Whether this server should be sent application traffic.
///
/// A draining server is deliberately not ready: a load balancer that keeps
/// sending to it would be filling a connection queue that is closing. A pool
/// with no live worker cannot serve its locations, so it is not ready either,
/// while a pool below its target but still serving is degraded rather than
/// unready.
///
/// FastCGI endpoints and proxy upstreams are deliberately **not** part of
/// readiness. Hamster does not own, start or probe them, and reporting
/// readiness for a process it does not supervise would be a claim it cannot
/// support. `doc/WINDOWS_OPERATIONS.md` says the same about startup.
pub(crate) fn readiness(config: &RuntimeConfig, draining: &AtomicBool) -> Result<(), String> {
    if draining.load(Ordering::Acquire) {
        return Err("draining".to_owned());
    }
    for pool in config.php_workers() {
        if pool.snapshot().live_workers == 0 {
            return Err(format!("php_pool {} has no live worker", pool.label()));
        }
    }
    Ok(())
}

/// Everything the exposition reads, gathered once per scrape.
pub(crate) struct MetricsView<'a> {
    /// The generation being reported. Owned rather than borrowed because it is
    /// read from the generations holder for each scrape.
    pub(crate) config: Arc<RuntimeConfig>,
    pub(crate) started: Instant,
    pub(crate) draining: &'a AtomicBool,
    pub(crate) access_log: Option<&'a AccessLog>,
    pub(crate) metrics: &'a ServerMetrics,
}

impl MetricsView<'_> {
    pub(crate) fn readiness(&self) -> Result<(), String> {
        readiness(&self.config, self.draining)
    }

    /// The whole exposition, in Prometheus text format.
    pub(crate) fn render(&self) -> String {
        let mut out = String::with_capacity(4 * 1_024);
        let ready = self.readiness().is_ok();

        gauge(&mut out, "hamster_up", "1 while the process can answer", 1);
        gauge(
            &mut out,
            "hamster_ready",
            "1 when this server should be sent application traffic",
            u64::from(ready),
        );
        gauge(
            &mut out,
            "hamster_config_generation",
            "Which compilation of the configuration is serving new connections",
            self.config.generation(),
        );
        gauge(
            &mut out,
            "hamster_draining",
            "1 once a graceful stop has been triggered",
            u64::from(self.draining.load(Ordering::Acquire)),
        );
        let _ = writeln!(out, "# HELP hamster_uptime_seconds Seconds since startup");
        let _ = writeln!(out, "# TYPE hamster_uptime_seconds gauge");
        let _ = writeln!(
            out,
            "hamster_uptime_seconds {:.3}",
            self.started.elapsed().as_secs_f64()
        );

        self.render_http(&mut out);
        self.render_pools(&mut out);
        self.render_upstreams(&mut out);
        self.render_fastcgi(&mut out);
        self.render_tls(&mut out);
        self.render_access_log(&mut out);
        out
    }

    fn render_http(&self, out: &mut String) {
        let metrics = self.metrics;
        counter(
            out,
            "hamster_connections_total",
            "Connections accepted since startup",
            metrics.connections_total.load(Ordering::Relaxed),
        );
        gauge(
            out,
            "hamster_connections_active",
            "Connections being served now",
            metrics.connections_active.load(Ordering::Relaxed),
        );
        gauge(
            out,
            "hamster_connections_limit",
            "Configured ceiling on connections served at once",
            self.config.http_limits().max_connections() as u64,
        );
        series(
            out,
            "hamster_requests_total",
            "Requests finished, by status class",
            "counter",
            STATUS_CLASSES.iter().enumerate().map(|(index, class)| {
                (
                    format!("status=\"{class}\""),
                    metrics.requests_by_class[index].load(Ordering::Relaxed),
                )
            }),
        );
        counter(
            out,
            "hamster_requests_incomplete_total",
            "Responses the client did not receive in full",
            metrics.requests_incomplete.load(Ordering::Relaxed),
        );
        counter(
            out,
            "hamster_response_bytes_total",
            "Response body bytes written to clients",
            metrics.response_bytes_total.load(Ordering::Relaxed),
        );

        let _ = writeln!(
            out,
            "# HELP hamster_request_duration_seconds Time from request start to the last body byte"
        );
        let _ = writeln!(out, "# TYPE hamster_request_duration_seconds histogram");
        let mut cumulative = 0_u64;
        for (index, bound) in DURATION_BUCKETS_SECONDS.iter().enumerate() {
            cumulative += metrics.duration_buckets[index].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "hamster_request_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
            );
        }
        cumulative +=
            metrics.duration_buckets[DURATION_BUCKETS_SECONDS.len()].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "hamster_request_duration_seconds_bucket{{le=\"+Inf\"}} {cumulative}"
        );
        let _ = writeln!(
            out,
            "hamster_request_duration_seconds_sum {:.6}",
            metrics.duration_micros_total.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );
        let _ = writeln!(out, "hamster_request_duration_seconds_count {cumulative}");
    }

    fn render_pools(&self, out: &mut String) {
        let pools = self.config.php_workers();
        if pools.is_empty() {
            return;
        }
        let _ = writeln!(
            out,
            "# HELP hamster_php_pool_workers Workers in a pool, by what the number means"
        );
        let _ = writeln!(out, "# TYPE hamster_php_pool_workers gauge");
        for pool in pools {
            let snapshot = pool.snapshot();
            let label = escape_label(pool.label());
            for (state, value) in [
                ("target", snapshot.target_workers),
                ("live", snapshot.live_workers),
                ("idle_ready", snapshot.ready_workers),
            ] {
                let _ = writeln!(
                    out,
                    "hamster_php_pool_workers{{pool=\"{label}\",state=\"{state}\"}} {value}"
                );
            }
        }
        series(
            out,
            "hamster_php_pool_respawns_total",
            "Workers replaced since startup",
            "counter",
            pools.iter().map(|pool| {
                (
                    format!("pool=\"{}\"", escape_label(pool.label())),
                    pool.snapshot().total_respawns,
                )
            }),
        );
        series(
            out,
            "hamster_php_pool_execution_timeouts_total",
            "Application transactions killed by the execution timeout",
            "counter",
            pools.iter().map(|pool| {
                (
                    format!("pool=\"{}\"", escape_label(pool.label())),
                    pool.snapshot().total_execution_timeouts,
                )
            }),
        );
    }

    fn render_upstreams(&self, out: &mut String) {
        let upstreams = self.config.upstreams();
        if upstreams.is_empty() {
            return;
        }
        for (name, help, kind, pick) in UPSTREAM_SERIES {
            series(
                out,
                name,
                help,
                kind,
                upstreams.iter().map(|upstream| {
                    (
                        format!("upstream=\"{}\"", escape_label(upstream.label())),
                        pick(&upstream.snapshot()),
                    )
                }),
            );
        }
    }

    fn render_fastcgi(&self, out: &mut String) {
        let backends = self.config.fastcgi_backends();
        if backends.is_empty() {
            return;
        }
        for (name, help, kind, pick) in FASTCGI_SERIES {
            series(
                out,
                name,
                help,
                kind,
                backends.iter().map(|backend| {
                    (
                        format!("endpoint=\"{}\"", escape_label(backend.label())),
                        pick(&backend.snapshot()),
                    )
                }),
            );
        }
    }

    fn render_tls(&self, out: &mut String) {
        let Some(tls) = self.config.tls() else {
            return;
        };
        let certificates = tls.certificates();
        let _ = writeln!(
            out,
            "# HELP hamster_tls_certificate_expiry_days Whole days until a loaded certificate expires"
        );
        let _ = writeln!(out, "# TYPE hamster_tls_certificate_expiry_days gauge");
        for certificate in certificates.iter() {
            let _ = writeln!(
                out,
                "hamster_tls_certificate_expiry_days{{field=\"{}\"}} {}",
                escape_label(&certificate.field),
                certificate.days_left
            );
        }
        // There is deliberately no "handshakes started" counter: that
        // instrument exists only in test builds, and adding it to the shipped
        // build would put one more atomic on every handshake. The three
        // outcome counters below are the ones an operator acts on.
        counter(
            out,
            "hamster_tls_handshakes_timed_out_total",
            "TLS handshakes that passed their deadline",
            tls.handshakes_timed_out(),
        );
        counter(
            out,
            "hamster_tls_handshakes_rejected_total",
            "TLS handshakes refused",
            tls.handshakes_rejected(),
        );
        counter(
            out,
            "hamster_tls_handshakes_abandoned_total",
            "TLS handshakes abandoned by the peer",
            tls.handshakes_abandoned(),
        );
    }

    fn render_access_log(&self, out: &mut String) {
        let Some(log) = self.access_log else {
            return;
        };
        series(
            out,
            "hamster_access_log_records_total",
            "Access log records by what happened to them",
            "counter",
            [
                ("result=\"written\"".to_owned(), log.written()),
                ("result=\"dropped\"".to_owned(), log.dropped()),
                ("result=\"failed\"".to_owned(), log.failed()),
            ],
        );
        gauge(
            out,
            "hamster_access_log_queued",
            "Records waiting to be written",
            log.queued() as u64,
        );
    }
}

/// Upstream series, kept as data so the metric list is one place.
type UpstreamPick = fn(&crate::proxy::UpstreamSnapshot) -> u64;
const UPSTREAM_SERIES: &[(&str, &str, &str, UpstreamPick)] = &[
    (
        "hamster_proxy_active_exchanges",
        "Proxy exchanges in flight",
        "gauge",
        |snapshot| snapshot.active_exchanges as u64,
    ),
    (
        "hamster_proxy_idle_connections",
        "Pooled upstream connections held idle",
        "gauge",
        |snapshot| snapshot.idle_connections as u64,
    ),
    (
        "hamster_proxy_connections_opened_total",
        "Upstream connections opened",
        "counter",
        |snapshot| snapshot.connections_opened,
    ),
    (
        "hamster_proxy_connections_reused_total",
        "Upstream connections taken from the idle pool",
        "counter",
        |snapshot| snapshot.connections_reused,
    ),
    (
        "hamster_proxy_safe_retries_total",
        "Requests re-sent after Hyper proved they never started",
        "counter",
        |snapshot| snapshot.safe_retries,
    ),
    (
        "hamster_proxy_connect_failures_total",
        "Upstream connection attempts that failed",
        "counter",
        |snapshot| snapshot.connect_failures,
    ),
    (
        "hamster_proxy_upstream_timeouts_total",
        "Connect, response-head and progress timeouts",
        "counter",
        |snapshot| snapshot.upstream_timeouts,
    ),
    (
        "hamster_proxy_upstream_failures_total",
        "Invalid or missing upstream responses",
        "counter",
        |snapshot| snapshot.upstream_failures,
    ),
];

type FastCgiPick = fn(&crate::fastcgi::FastCgiSnapshot) -> u64;
const FASTCGI_SERIES: &[(&str, &str, &str, FastCgiPick)] = &[
    (
        "hamster_fastcgi_active_exchanges",
        "FastCGI exchanges in flight",
        "gauge",
        |snapshot| snapshot.active_exchanges as u64,
    ),
    (
        "hamster_fastcgi_scripts_total",
        "Scripts executed through FastCGI",
        "counter",
        |snapshot| snapshot.scripts,
    ),
    (
        "hamster_fastcgi_static_files_total",
        "Static files served from a FastCGI location",
        "counter",
        |snapshot| snapshot.static_files,
    ),
    (
        "hamster_fastcgi_connect_failures_total",
        "FastCGI connection attempts that failed",
        "counter",
        |snapshot| snapshot.connect_failures,
    ),
    (
        "hamster_fastcgi_busy_refusals_total",
        "Requests refused because every slot stayed busy",
        "counter",
        |snapshot| snapshot.busy_refusals,
    ),
    (
        "hamster_fastcgi_timeouts_total",
        "FastCGI exchanges that passed a deadline",
        "counter",
        |snapshot| snapshot.timeouts,
    ),
    (
        "hamster_fastcgi_backend_failures_total",
        "Missing, malformed or truncated FastCGI responses",
        "counter",
        |snapshot| snapshot.backend_failures,
    ),
    (
        "hamster_fastcgi_connections_opened_total",
        "TCP connections opened to FastCGI servers",
        "counter",
        |snapshot| snapshot.connections_opened,
    ),
    (
        "hamster_fastcgi_connections_reused_total",
        "FastCGI exchanges that ran on a kept connection",
        "counter",
        |snapshot| snapshot.connections_reused,
    ),
];

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

/// One metric with a label per configured resource.
fn series(
    out: &mut String,
    name: &str,
    help: &str,
    kind: &str,
    values: impl IntoIterator<Item = (String, u64)>,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, value) in values {
        let _ = writeln!(out, "{name}{{{labels}}} {value}");
    }
}

/// Escapes a configured name for a label value.
///
/// Labels come from the configuration rather than from traffic, but a
/// configured host or pool name still must not be able to end a label or a
/// line.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            ordinary => escaped.push(ordinary),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_outside_the_classes_is_counted_once_but_not_classified() {
        let metrics = ServerMetrics::default();
        metrics.request_finished(200, 10, true, 1_000);
        metrics.request_finished(503, 0, true, 1_000);
        metrics.request_finished(999, 0, true, 1_000);
        assert_eq!(metrics.requests_total(), 3);
        assert_eq!(metrics.requests_by_class[1].load(Ordering::Relaxed), 1);
        assert_eq!(metrics.requests_by_class[4].load(Ordering::Relaxed), 1);
        assert_eq!(status_class_index(999), None);
        assert_eq!(status_class_index(99), None);
    }

    #[test]
    fn a_duration_falls_in_the_bucket_at_or_below_its_bound() {
        // Boundaries are inclusive, as Prometheus `le` means.
        assert_eq!(duration_bucket_index(0), 0);
        assert_eq!(duration_bucket_index(1_000), 0);
        assert_eq!(duration_bucket_index(1_001), 1);
        assert_eq!(duration_bucket_index(10_000), 2);
        assert_eq!(
            duration_bucket_index(10_000_000),
            DURATION_BUCKETS_SECONDS.len() - 1
        );
        // Above the last bound, in the overflow counter rather than lost.
        assert_eq!(
            duration_bucket_index(60_000_000),
            DURATION_BUCKETS_SECONDS.len()
        );
    }

    #[test]
    fn an_incomplete_response_is_counted_as_one_request_and_one_incompletion() {
        let metrics = ServerMetrics::default();
        metrics.request_finished(200, 5, false, 2_000);
        assert_eq!(metrics.requests_total(), 1);
        assert_eq!(metrics.requests_incomplete.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.response_bytes_total.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn a_connection_gauge_returns_to_zero() {
        let metrics = ServerMetrics::default();
        metrics.connection_opened();
        metrics.connection_opened();
        assert_eq!(metrics.connections_active(), 2);
        metrics.connection_closed();
        metrics.connection_closed();
        assert_eq!(metrics.connections_active(), 0);
        assert_eq!(metrics.connections_total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_configured_name_cannot_end_a_label_or_a_line() {
        assert_eq!(escape_label("pool\"name"), "pool\\\"name");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("line\nbreak"), "line\\nbreak");
        assert_eq!(escape_label("localhost/php"), "localhost/php");
    }
}
