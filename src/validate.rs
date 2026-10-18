//! What `--validate` prints: the resources a configuration file compiles to.
//!
//! The 0.8 scope asks for a `validate` CLI operation. Its value is not that it
//! parses TOML — a server start does that too, loudly — but that it does so
//! **without starting anything**: no PHP worker is spawned, no listener is
//! bound, no log file is created, no Job Object is established. An operator
//! can therefore check a replacement configuration on the machine that will
//! run it, while the current server keeps serving.
//!
//! The report is `key=value` lines, like the control protocol, so it can be
//! read by a person and grepped by a script. It lists the resources the file
//! compiles to rather than the full route table: the compiled router answers
//! routing questions and does not enumerate itself, and inventing a second
//! description of the routes here would be a second thing to keep true.

use std::fmt::Write as _;

use crate::config::RuntimeConfig;

/// The report for a configuration that compiled.
pub fn report(config: &RuntimeConfig) -> String {
    let mut out = String::with_capacity(1_024);
    out.push_str("configuration=ok\n");

    if let Some(address) = config.listen_address() {
        let _ = writeln!(out, "listener=http://{address}");
    }
    if let Some(tls) = config.tls() {
        let certificates = tls.certificates();
        let _ = writeln!(
            out,
            "listener=https://{} certificates={} handshake_timeout_seconds={}",
            tls.address(),
            certificates.len(),
            tls.handshake_timeout().as_secs()
        );
        for certificate in certificates.iter() {
            // Expiry is the one fact about a certificate file that an operator
            // most often wants before a deployment, and loading it is the main
            // thing that can fail here.
            let _ = writeln!(
                out,
                "certificate={} names={} not_after={} days_left={}",
                single_line(&certificate.field),
                single_line(&certificate.names),
                single_line(&certificate.not_after),
                certificate.days_left
            );
            if let Some(note) = certificate.note {
                let _ = writeln!(
                    out,
                    "certificate={} note={}",
                    single_line(&certificate.field),
                    note
                );
            }
        }
    }

    let limits = config.http_limits();
    let _ = writeln!(
        out,
        "max_connections={} header_read_timeout_seconds={} body_read_timeout_seconds={} write_progress_timeout_seconds={} max_body_memory_bytes={} http1_max_buffer_size_bytes={}",
        limits.max_connections(),
        limits.header_read_timeout().as_secs(),
        limits.body_read_timeout().as_secs(),
        limits.write_progress_timeout().as_secs(),
        limits.max_body_memory(),
        limits.http1_max_buffer_size()
    );

    if let Some(name) = config.control_name() {
        let _ = writeln!(out, "control={}", single_line(name));
    }
    if let Some(admin) = config.admin() {
        let _ = writeln!(out, "admin=http://{}", admin.listen());
    }
    if let Some(access_log) = config.access_log() {
        let _ = writeln!(
            out,
            "access_log={} include_query={} queue_capacity={}",
            single_line(&access_log.target_description()),
            access_log.include_query(),
            access_log.queue_capacity()
        );
    }

    for pool in config.php_workers() {
        let _ = writeln!(
            out,
            "php_pool={} workers={} queue_capacity={} request_timeout_seconds={}",
            single_line(pool.label()),
            pool.snapshot().target_workers,
            pool.queue_capacity(),
            pool.request_timeout().as_secs()
        );
    }
    for upstream in config.upstreams() {
        let _ = writeln!(
            out,
            "proxy_upstream={} address={}",
            single_line(upstream.label()),
            upstream.address()
        );
    }
    for backend in config.fastcgi_backends() {
        let servers = backend
            .servers()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            out,
            "fastcgi={} servers={}",
            single_line(backend.label()),
            servers
        );
    }

    // What validation did not do. Stating it is the difference between a
    // report an operator can rely on and one that invites a wrong conclusion.
    out.push_str("not_checked=listener_bind,log_write,worker_startup,backend_reachability\n");
    out
}

/// Keeps a configured name on its own line, as the control protocol does.
fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config;

    fn compiled(source: &str) -> RuntimeConfig {
        config::compile_str(source, Path::new(".")).expect("the test source should compile")
    }

    const HOSTS: &str = "\n\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n";

    #[test]
    fn a_minimal_configuration_reports_its_listener_and_limits() {
        let report = report(&compiled(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"{HOSTS}"
        )));
        assert!(report.starts_with("configuration=ok\n"), "{report}");
        assert!(
            report.contains("listener=http://127.0.0.1:8080\n"),
            "{report}"
        );
        assert!(report.contains("max_connections=1024"), "{report}");
        assert!(
            report.contains("header_read_timeout_seconds=30"),
            "{report}"
        );
        // Absent sections say nothing rather than saying "none".
        assert!(!report.contains("control="), "{report}");
        assert!(!report.contains("admin="), "{report}");
        assert!(!report.contains("access_log="), "{report}");
        assert!(!report.contains("php_pool="), "{report}");
    }

    #[test]
    fn the_report_names_what_validation_did_not_check() {
        // A report that looked complete would invite the conclusion that a
        // validated configuration is a working deployment.
        let report = report(&compiled(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"{HOSTS}"
        )));
        let last = report.lines().last().unwrap();
        assert_eq!(
            last,
            "not_checked=listener_bind,log_write,worker_startup,backend_reachability"
        );
    }

    #[test]
    fn configured_surfaces_each_appear_once_with_their_settings() {
        let report = report(&compiled(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[server.admin]\nlisten = \"127.0.0.1:9090\"\n\n\
             [server.access_log]\npath = \"logs/access.jsonl\"\ninclude_query = true\nqueue_capacity = 32{HOSTS}"
        )));
        assert!(report.contains("admin=http://127.0.0.1:9090\n"), "{report}");
        assert_eq!(report.matches("admin=").count(), 1, "{report}");
        assert!(
            report.contains("include_query=true queue_capacity=32"),
            "{report}"
        );
        assert!(report.contains("access_log="), "{report}");
    }

    #[test]
    fn a_proxy_upstream_reports_the_address_it_resolved_to() {
        let report = report(&compiled(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
             [[hosts.locations]]\npath = \"/\"\nhandler = \"proxy\"\n\n\
             [hosts.locations.proxy]\nupstream = \"http://127.0.0.1:3000\"\n",
        ));
        // The address was resolved once at compile time, so this is the
        // address the running server would use, not the name in the file.
        assert!(
            report.contains("proxy_upstream=localhost/ address=127.0.0.1:3000"),
            "{report}"
        );
    }

    #[test]
    fn a_configured_name_cannot_add_a_line_to_the_report() {
        assert_eq!(single_line("a\nb\rc"), "a b c");
    }
}
