//! TLS termination for the HTTPS listener.
//!
//! TLS is a property of a listener, not of a handler. Everything here happens
//! before HTTP exists on a connection: certificates are loaded and checked
//! while configuration compiles, and an accepted socket either completes a
//! handshake inside its deadline or is dropped without an HTTP response. After
//! the handshake the connection enters the same HTTP serving path as
//! cleartext, carrying only a trusted `Scheme::Https`; no handler sees a
//! rustls type.
//!
//! Certificate selection uses SNI as a certificate identity hint only. The
//! effective HTTP authority still selects the virtual host, exactly as on the
//! cleartext listener.

use std::{
    collections::HashMap,
    fmt, fs,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self, ServerConfig,
        client::verify_server_name,
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
        server::{ClientHello, ParsedCertificate, ResolvesServerCert},
        sign::CertifiedKey,
    },
    server::TlsStream,
};

use crate::router::valid_hostname;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// The only application protocol this listener speaks. A client that offers
/// ALPN without it is refused by rustls; a client that offers no ALPN at all
/// gets HTTP/1.1, which is the only thing it could have meant.
const HTTP_1_1: &[u8] = b"http/1.1";

/// A handshake failure is peer-controlled, so it is reported at a rate the
/// peer cannot influence, exactly like the body budget's refusals.
const HANDSHAKE_NOTICE_PERIOD_MS: u64 = 60_000;

/// A configured wildcard is checked against the certificate with a name that
/// only a matching wildcard SAN covers. A certificate listing a few explicit
/// subdomains does not serve every subdomain, and must not pass as if it did.
const WILDCARD_PROBE_LABEL: &str = "hamster-wildcard-check";

/// A configured certificate name: an exact DNS name, or `*.` plus a parent
/// domain matching exactly one extra label.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TlsName {
    Exact(Box<str>),
    Wildcard(Box<str>),
}

impl TlsName {
    pub(crate) fn parse(name: &str) -> Result<Self, &'static str> {
        let lowered = name.to_ascii_lowercase();
        let (wildcard, host) = match lowered.strip_prefix("*.") {
            Some(parent) => (true, parent),
            None => (false, lowered.as_str()),
        };
        if host.parse::<IpAddr>().is_ok() {
            return Err("is an IP address; clients never send one as SNI");
        }
        if !valid_hostname(host) || host.contains('*') {
            return Err("is not a DNS name or a single leading \"*.\" wildcard");
        }
        if wildcard && !host.contains('.') {
            return Err("is a wildcard over a top-level domain");
        }
        Ok(if wildcard {
            Self::Wildcard(host.into())
        } else {
            Self::Exact(host.into())
        })
    }

    fn probe(&self) -> String {
        match self {
            Self::Exact(name) => name.to_string(),
            Self::Wildcard(parent) => format!("{WILDCARD_PROBE_LABEL}.{parent}"),
        }
    }
}

impl fmt::Display for TlsName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => formatter.write_str(name),
            Self::Wildcard(parent) => write!(formatter, "*.{parent}"),
        }
    }
}

/// One validated certificate entry. Nothing has been read from disk yet.
pub(crate) struct CertificateSource {
    pub(crate) field: String,
    pub(crate) cert: PathBuf,
    pub(crate) key: PathBuf,
    pub(crate) names: Vec<TlsName>,
    pub(crate) default: bool,
}

/// The validated `[server.tls]` table.
pub(crate) struct TlsSource {
    pub(crate) listen: SocketAddr,
    pub(crate) handshake_timeout: Duration,
    pub(crate) certificates: Vec<CertificateSource>,
}

/// A compiled HTTPS listener: address, immutable rustls configuration and the
/// handshake deadline. Built once at startup and shared by every connection.
pub struct TlsListener {
    address: SocketAddr,
    acceptor: TlsAcceptor,
    handshake_timeout: Duration,
    certificates: usize,
    stats: HandshakeStats,
}

impl fmt::Debug for TlsListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsListener")
            .field("address", &self.address)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("certificates", &self.certificates)
            .finish_non_exhaustive()
    }
}

impl TlsListener {
    /// Loads every certificate and key and builds the rustls configuration.
    ///
    /// Any failure here is a configuration failure: the server has not started
    /// a worker or bound a socket yet, so nothing is served with a partial TLS
    /// setup. Errors name the configuration field and path, never key bytes.
    pub(crate) fn compile(source: TlsSource) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut resolver = CertificateResolver::default();
        for certificate in &source.certificates {
            let key = load_certified_key(certificate, &provider)?;
            for name in &certificate.names {
                let (table, lookup) = match name {
                    TlsName::Exact(name) => (&mut resolver.exact, name),
                    TlsName::Wildcard(parent) => (&mut resolver.wildcard, parent),
                };
                // Validation already rejected duplicates; this keeps the
                // resolver honest if that ever changes.
                if table.insert(lookup.clone(), Arc::clone(&key)).is_some() {
                    return Err(format!(
                        "{}.names {name} is configured twice",
                        certificate.field
                    ));
                }
            }
            if certificate.default {
                resolver.default = Some(key);
            }
        }
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("server.tls: {error}"))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        config.alpn_protocols = vec![HTTP_1_1.to_vec()];
        Ok(Self {
            address: source.listen,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshake_timeout: source.handshake_timeout,
            certificates: source.certificates.len(),
            stats: HandshakeStats::new(),
        })
    }

    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    pub const fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    pub const fn certificates(&self) -> usize {
        self.certificates
    }

    /// Handshakes that did not finish before the total deadline, or whose
    /// socket write made no progress for the write-progress interval.
    pub fn handshakes_timed_out(&self) -> u64 {
        self.stats.timed_out.load(Ordering::Relaxed)
    }

    /// Handshakes rustls refused: malformed or non-TLS bytes, no common
    /// version, cipher or ALPN protocol, or no certificate for the SNI name.
    pub fn handshakes_rejected(&self) -> u64 {
        self.stats.rejected.load(Ordering::Relaxed)
    }

    /// Handshakes the peer closed or reset. Browsers do this routinely with
    /// speculative connections, so it is counted but never reported.
    pub fn handshakes_abandoned(&self) -> u64 {
        self.stats.abandoned.load(Ordering::Relaxed)
    }

    /// Runs one handshake to completion or to `deadline`, whichever is first.
    ///
    /// The deadline is total, not an inactivity timer: a peer that trickles
    /// one byte at a time cannot keep a handshake, and its admission slot,
    /// alive. The caller owns cancellation for shutdown; dropping this future
    /// drops the socket and all rustls state, and no task is left behind.
    pub(crate) async fn handshake<IO>(
        &self,
        io: IO,
        deadline: tokio::time::Instant,
    ) -> Option<TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        match tokio::time::timeout_at(deadline, self.acceptor.accept(io)).await {
            Ok(Ok(stream)) => Some(stream),
            Ok(Err(error)) => {
                self.stats.record(&self.address, &error);
                None
            }
            Err(_) => {
                self.stats
                    .record_timeout(&self.address, self.handshake_timeout);
                None
            }
        }
    }
}

fn load_certified_key(
    source: &CertificateSource,
    provider: &CryptoProvider,
) -> Result<Arc<CertifiedKey>, String> {
    let field = &source.field;
    let cert_path = source.cert.display();
    let key_path = source.key.display();
    let cert_pem = fs::read(&source.cert)
        .map_err(|error| format!("{field}.cert {cert_path} cannot be read: {error}"))?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{field}.cert {cert_path} is not valid PEM: {error}"))?;
    if chain.is_empty() {
        return Err(format!(
            "{field}.cert {cert_path} contains no CERTIFICATE block"
        ));
    }

    let key_pem = fs::read(&source.key)
        .map_err(|error| format!("{field}.key {key_path} cannot be read: {error}"))?;
    // The PEM error for a key file is never displayed: it can quote a line of
    // the file, and nothing from a private key belongs in a diagnostic.
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| {
        if key_pem
            .windows(b"ENCRYPTED".len())
            .any(|window| window == b"ENCRYPTED")
        {
            format!("{field}.key {key_path} is encrypted; an unencrypted key is required")
        } else {
            format!(
                "{field}.key {key_path} contains no readable PKCS#8, PKCS#1 or SEC1 private key"
            )
        }
    })?;
    drop(key_pem);

    let certified = CertifiedKey::from_der(chain, key, provider).map_err(|error| {
        format!(
            "{field}: certificate {cert_path} and key {key_path} cannot be used together: {error}"
        )
    })?;
    let leaf = certified
        .end_entity_cert()
        .and_then(ParsedCertificate::try_from)
        .map_err(|error| {
            format!("{field}.cert {cert_path}: leaf certificate cannot be parsed: {error}")
        })?;
    for name in &source.names {
        let probe = name.probe();
        let covered = ServerName::try_from(probe.as_str())
            .map_err(|error| error.to_string())
            .and_then(|server_name| {
                verify_server_name(&leaf, &server_name).map_err(|error| error.to_string())
            });
        if let Err(error) = covered {
            return Err(format!(
                "{field}.names {name} is not covered by certificate {cert_path}: {error}"
            ));
        }
    }
    Ok(Arc::new(certified))
}

/// A static SNI map built at startup. Lookup is two hash probes at most and
/// never touches the filesystem, because rustls calls it synchronously in the
/// middle of a handshake.
#[derive(Debug, Default)]
struct CertificateResolver {
    exact: HashMap<Box<str>, Arc<CertifiedKey>>,
    /// Keyed by the parent domain: `*.example.net` is stored as
    /// `example.net` and matches exactly one additional label.
    wildcard: HashMap<Box<str>, Arc<CertifiedKey>>,
    /// Used when the client sends no SNI or a name nothing else matches.
    /// Without one, such a handshake fails.
    default: Option<Arc<CertifiedKey>>,
}

impl CertificateResolver {
    fn lookup(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let name = server_name.to_ascii_lowercase();
        let name = name.strip_suffix('.').unwrap_or(&name);
        if let Some(key) = self.exact.get(name) {
            return Some(Arc::clone(key));
        }
        let (_, parent) = name.split_once('.')?;
        self.wildcard.get(parent).map(Arc::clone)
    }
}

impl ResolvesServerCert for CertificateResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        client_hello
            .server_name()
            .and_then(|name| self.lookup(name))
            .or_else(|| self.default.clone())
    }
}

#[derive(Debug)]
struct HandshakeStats {
    timed_out: AtomicU64,
    rejected: AtomicU64,
    abandoned: AtomicU64,
    /// Milliseconds since `started` at which the next failure may be reported.
    next_notice_ms: AtomicU64,
    started: Instant,
}

impl HandshakeStats {
    fn new() -> Self {
        Self {
            timed_out: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            abandoned: AtomicU64::new(0),
            next_notice_ms: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn record(&self, address: &SocketAddr, error: &std::io::Error) {
        match error.kind() {
            // The write-progress deadline under TLS: the peer stopped reading
            // the handshake. Same class as the total deadline.
            std::io::ErrorKind::TimedOut => {
                self.timed_out.fetch_add(1, Ordering::Relaxed);
                self.report(address, format_args!("{error}"));
            }
            // tokio-rustls wraps every rustls protocol error as InvalidData.
            std::io::ErrorKind::InvalidData => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                self.report(address, format_args!("{error}"));
            }
            _ => {
                self.abandoned.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn record_timeout(&self, address: &SocketAddr, timeout: Duration) {
        self.timed_out.fetch_add(1, Ordering::Relaxed);
        self.report(
            address,
            format_args!(
                "handshake not complete after {:.3} s",
                timeout.as_secs_f64()
            ),
        );
    }

    fn report(&self, address: &SocketAddr, last: fmt::Arguments<'_>) {
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.claim_notice(elapsed_ms) {
            return;
        }
        eprintln!(
            "TLS listener {address}: {} handshake(s) rejected, {} timed out, {} abandoned since start; last: {last}; reported at most once per {} s",
            self.rejected.load(Ordering::Relaxed),
            self.timed_out.load(Ordering::Relaxed),
            self.abandoned.load(Ordering::Relaxed),
            HANDSHAKE_NOTICE_PERIOD_MS / 1_000
        );
    }

    fn claim_notice(&self, now_ms: u64) -> bool {
        let due = self.next_notice_ms.load(Ordering::Relaxed);
        if due == u64::MAX || now_ms < due {
            return false;
        }
        self.next_notice_ms
            .compare_exchange(
                due,
                now_ms.saturating_add(HANDSHAKE_NOTICE_PERIOD_MS),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_and_single_label_wildcard_names() {
        assert_eq!(
            TlsName::parse("Example.COM"),
            Ok(TlsName::Exact("example.com".into()))
        );
        assert_eq!(
            TlsName::parse("*.example.net"),
            Ok(TlsName::Wildcard("example.net".into()))
        );
        for invalid in [
            "",
            "*",
            "*.com",
            "a.*.example.net",
            "*example.net",
            "**.example.net",
            "127.0.0.1",
            "::1",
            "example.com.",
            "exa_mple.com",
            "example.com:443",
        ] {
            assert!(TlsName::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_wildcard_matches_exactly_one_label_and_exact_names_win() {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let key = |name: &str| {
            let generated = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
            let key =
                PrivateKeyDer::from_pem_slice(generated.signing_key.serialize_pem().as_bytes())
                    .unwrap();
            Arc::new(
                CertifiedKey::from_der(vec![generated.cert.der().clone()], key, &provider).unwrap(),
            )
        };
        let exact = key("api.example.net");
        let wildcard = key("*.example.net");
        let resolver = CertificateResolver {
            exact: HashMap::from([("api.example.net".into(), Arc::clone(&exact))]),
            wildcard: HashMap::from([("example.net".into(), Arc::clone(&wildcard))]),
            default: None,
        };
        let same = |found: Option<Arc<CertifiedKey>>, expected: &Arc<CertifiedKey>| {
            found.is_some_and(|found| Arc::ptr_eq(&found, expected))
        };
        assert!(same(resolver.lookup("API.example.net"), &exact));
        assert!(same(resolver.lookup("www.example.net"), &wildcard));
        assert!(same(resolver.lookup("www.example.net."), &wildcard));
        assert!(resolver.lookup("example.net").is_none());
        assert!(resolver.lookup("a.b.example.net").is_none());
        assert!(resolver.lookup("example.org").is_none());
    }

    #[test]
    fn handshake_failures_are_reported_at_most_once_per_window() {
        let stats = HandshakeStats::new();
        assert!(stats.claim_notice(0));
        assert!(!stats.claim_notice(1));
        assert!(!stats.claim_notice(HANDSHAKE_NOTICE_PERIOD_MS - 1));
        assert!(stats.claim_notice(HANDSHAKE_NOTICE_PERIOD_MS));
        stats.next_notice_ms.store(u64::MAX, Ordering::Relaxed);
        assert!(!stats.claim_notice(u64::MAX));
    }
}
