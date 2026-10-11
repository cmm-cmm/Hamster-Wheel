//! TLS termination for the HTTPS listener.
//!
//! TLS is a property of a listener, not of a handler. Everything here happens
//! before HTTP exists on a connection: certificates are loaded and checked
//! while configuration compiles or a replacement set reloads. An accepted
//! socket either completes a
//! handshake inside its deadline or is dropped without an HTTP response. After
//! the handshake the connection enters the same HTTP serving path as
//! cleartext, carrying only a trusted `Scheme::Https`; no handler sees a
//! rustls type.
//!
//! Certificate selection uses SNI as a certificate identity hint only. The
//! effective HTTP authority still selects the virtual host, exactly as on the
//! cleartext listener, but a TLS connection is authoritative only for the
//! names configured on the certificate it presented (`CertificateScope`);
//! the router answers any other authority with `421 Misdirected Request`.

use std::{
    collections::HashMap,
    fmt, fs,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self, RootCertStore, ServerConfig,
        client::{verify_server_cert_signed_by_trust_anchor, verify_server_name},
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
        server::{ClientHello, ParsedCertificate, ResolvesServerCert},
        sign::CertifiedKey,
    },
    server::TlsStream,
};

use crate::router::valid_hostname;

#[cfg(test)]
pub(crate) mod test_support;
mod x509;

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

/// A configured certificate name: an exact DNS name, `*.` plus a parent
/// domain matching exactly one extra label, or an IP address.
///
/// DNS names select the certificate by SNI and authorize requests for that
/// host. An IP address only authorizes requests whose Host is that literal:
/// clients never send an IP address as SNI, so it cannot select anything.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TlsName {
    Exact(Box<str>),
    Wildcard(Box<str>),
    Address(IpAddr),
}

impl TlsName {
    pub(crate) fn parse(name: &str) -> Result<Self, &'static str> {
        let lowered = name.to_ascii_lowercase();
        let (wildcard, host) = match lowered.strip_prefix("*.") {
            Some(parent) => (true, parent),
            None => (false, lowered.as_str()),
        };
        if let Ok(address) = host.parse::<IpAddr>() {
            if wildcard {
                return Err("is a wildcard over an IP address");
            }
            return Ok(Self::Address(address));
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
            Self::Address(address) => address.to_string(),
        }
    }
}

impl fmt::Display for TlsName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => formatter.write_str(name),
            Self::Wildcard(parent) => write!(formatter, "*.{parent}"),
            Self::Address(address) => write!(formatter, "{address}"),
        }
    }
}

/// The hosts one certificate entry is authoritative for: exactly its
/// configured `names`, each already checked against the leaf's subject
/// alternative names at startup.
///
/// RFC 9110 section 4.3.3: over TLS, an origin is authoritative for a host
/// only when the certificate it presented is valid for that host. A request
/// for any other authority on this connection is misdirected (section
/// 15.5.20), whichever virtual host would have matched it.
#[derive(Debug, Default)]
pub(crate) struct CertificateScope {
    exact: Box<[Box<str>]>,
    wildcard: Box<[Box<str>]>,
    addresses: Box<[IpAddr]>,
}

impl CertificateScope {
    fn new(names: &[TlsName]) -> Self {
        let mut exact = Vec::new();
        let mut wildcard = Vec::new();
        let mut addresses = Vec::new();
        for name in names {
            match name {
                TlsName::Exact(name) => exact.push(name.clone()),
                TlsName::Wildcard(parent) => wildcard.push(parent.clone()),
                TlsName::Address(address) => addresses.push(*address),
            }
        }
        Self {
            exact: exact.into(),
            wildcard: wildcard.into(),
            addresses: addresses.into(),
        }
    }

    /// Whether a request for `hostname`, the effective authority without its
    /// port, may be served on a connection that presented this certificate.
    /// The router has already validated the hostname's syntax.
    pub(crate) fn covers(&self, hostname: &str) -> bool {
        if let Ok(address) = hostname.parse::<IpAddr>() {
            return self.addresses.contains(&address);
        }
        if self
            .exact
            .iter()
            .any(|name| name.eq_ignore_ascii_case(hostname))
        {
            return true;
        }
        hostname.split_once('.').is_some_and(|(_, parent)| {
            self.wildcard
                .iter()
                .any(|name| name.eq_ignore_ascii_case(parent))
        })
    }

    /// A scope covering the given names, for tests that route without TLS.
    #[cfg(test)]
    pub(crate) fn for_names(names: &[&str]) -> Self {
        Self::new(
            &names
                .iter()
                .map(|name| TlsName::parse(name).expect("valid test name"))
                .collect::<Vec<_>>(),
        )
    }
}

/// One validated certificate entry. Nothing has been read from disk yet.
#[derive(Clone)]
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

/// A compiled HTTPS listener with one atomically replaced certificate set.
pub struct TlsListener {
    address: SocketAddr,
    source: Box<[CertificateSource]>,
    provider: Arc<CryptoProvider>,
    current: RwLock<Arc<CertificateSet>>,
    reload_gate: Arc<Mutex<()>>,
    handshake_timeout: Duration,
    stats: HandshakeStats,
    #[cfg(test)]
    handshakes_started: AtomicU64,
}

/// One generation of keys, SNI selection, Host scopes and rustls settings.
struct CertificateSet {
    acceptor: TlsAcceptor,
    resolver: Arc<CertificateResolver>,
    reports: Arc<[CertificateReport]>,
}

/// What startup prints about one certificate entry.
#[derive(Clone, Debug)]
pub struct CertificateReport {
    pub field: String,
    pub names: String,
    pub not_after: String,
    /// Whole days until `not_after`, at startup.
    pub days_left: i64,
    /// A property the server cannot decide, stated rather than guessed.
    pub note: Option<&'static str>,
}

impl fmt::Debug for TlsListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsListener")
            .field("address", &self.address)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("certificates", &self.certificates().len())
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
        let current = compile_set(&source.certificates, Arc::clone(&provider))?;
        Ok(Self {
            address: source.listen,
            source: source.certificates.into_boxed_slice(),
            provider,
            current: RwLock::new(Arc::new(current)),
            reload_gate: Arc::new(Mutex::new(())),
            handshake_timeout: source.handshake_timeout,
            stats: HandshakeStats::new(),
            #[cfg(test)]
            handshakes_started: AtomicU64::new(0),
        })
    }

    /// Re-read only configured certificate and key paths. Compilation occurs
    /// outside the snapshot lock; failure leaves the serving set untouched.
    pub async fn reload_certificates(&self) -> Result<Arc<[CertificateReport]>, String> {
        let gate = Arc::clone(&self.reload_gate).lock_owned().await;
        let source = self.source.to_vec();
        let provider = Arc::clone(&self.provider);
        // Cancellation cannot stop a queued/running blocking filesystem job.
        // That job retains serialization until it finishes; on success keep
        // the guard through adoption too, preventing out-of-order commits.
        let (replacement, _gate) = tokio::task::spawn_blocking(move || {
            compile_set(&source, provider).map(|replacement| (replacement, gate))
        })
        .await
        .map_err(|error| format!("TLS certificate reload task failed: {error}"))??;
        let reports = Arc::clone(&replacement.reports);
        let previous = {
            let mut current = self.current.write().expect("TLS snapshot lock");
            std::mem::replace(&mut *current, Arc::new(replacement))
        };
        drop(previous);
        Ok(reports)
    }

    fn snapshot(&self) -> Arc<CertificateSet> {
        Arc::clone(&self.current.read().expect("TLS snapshot lock"))
    }

    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    pub const fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    pub fn certificates(&self) -> Arc<[CertificateReport]> {
        Arc::clone(&self.snapshot().reports)
    }

    #[cfg(test)]
    pub(crate) fn handshakes_started(&self) -> u64 {
        self.handshakes_started.load(Ordering::Relaxed)
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

    /// Each handshake holds one generation until its scope has been selected.
    pub(crate) async fn handshake<IO>(
        &self,
        io: IO,
        deadline: tokio::time::Instant,
    ) -> Option<(TlsStream<IO>, Arc<CertificateScope>)>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let snapshot = self.snapshot();
        #[cfg(test)]
        self.handshakes_started.fetch_add(1, Ordering::Relaxed);
        match tokio::time::timeout_at(deadline, snapshot.acceptor.accept(io)).await {
            Ok(Ok(stream)) => {
                let scope = snapshot
                    .resolver
                    .select(stream.get_ref().1.server_name())
                    .map(|entry| Arc::clone(&entry.scope))?;
                Some((stream, scope))
            }
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

fn compile_set(
    certificates: &[CertificateSource],
    provider: Arc<CryptoProvider>,
) -> Result<CertificateSet, String> {
    let now = unix_now();
    let mut resolver = CertificateResolver::default();
    let mut reports = Vec::with_capacity(certificates.len());
    for certificate in certificates {
        let (key, report) = load_certified_key(certificate, &provider, now)?;
        let entry = Arc::new(Entry {
            key,
            scope: Arc::new(CertificateScope::new(&certificate.names)),
        });
        for name in &certificate.names {
            let (table, lookup) = match name {
                TlsName::Exact(name) => (&mut resolver.exact, name),
                TlsName::Wildcard(parent) => (&mut resolver.wildcard, parent),
                TlsName::Address(_) => continue,
            };
            // Validation already rejected duplicates; this keeps the
            // resolver honest if that ever changes.
            if table.insert(lookup.clone(), Arc::clone(&entry)).is_some() {
                return Err(format!(
                    "{}.names {name} is configured twice",
                    certificate.field
                ));
            }
        }
        if certificate.default {
            resolver.default = Some(entry);
        }
        reports.push(report);
    }
    let resolver = Arc::new(resolver);
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("server.tls: {error}"))?
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
    config.alpn_protocols = vec![HTTP_1_1.to_vec()];
    Ok(CertificateSet {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        resolver,
        reports: reports.into(),
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn load_certified_key(
    source: &CertificateSource,
    provider: &CryptoProvider,
    now: i64,
) -> Result<(Arc<CertifiedKey>, CertificateReport), String> {
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
    x509::check_leaf_server_auth(&certified.cert[0])
        .map_err(|error| format!("{field}.cert {cert_path}: {error}"))?;
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

    // What the server can decide on its own: every certificate it will send
    // is inside its validity period now, and the chain links. Whether the
    // client trusts the last issuer is the client's decision.
    let summaries = certified
        .cert
        .iter()
        .map(|der| x509::summarize(der))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!("{field}.cert {cert_path}: a certificate cannot be read: {error}")
        })?;
    let last = summaries.len() - 1;
    for (index, summary) in summaries.iter().enumerate() {
        // A trailing self-signed root is ignored by clients, which use their
        // own copy of it, so its dates decide nothing.
        if index > 0 && index == last && summary.self_issued {
            continue;
        }
        let which = if index == 0 {
            "the leaf certificate".to_owned()
        } else {
            format!("chain certificate {}", index + 1)
        };
        if now < summary.not_before.0 {
            return Err(format!(
                "{field}.cert {cert_path}: {which} is not valid until {}",
                summary.not_before
            ));
        }
        if now > summary.not_after.0 {
            return Err(format!(
                "{field}.cert {cert_path}: {which} expired on {}",
                summary.not_after
            ));
        }
    }
    let mut note = None;
    if last > 0 {
        x509::check_issuer_ca(&certified.cert[last])
            .map_err(|error| format!("{field}.cert {cert_path}: {error}"))?;
        // Treat the last certificate in the file as the anchor: webpki checks
        // signatures and the constraints below it. Its own CA bit was checked
        // above because webpki does not apply intermediate rules to anchors.
        let mut anchor = RootCertStore::empty();
        anchor
            .add(certified.cert[last].clone())
            .map_err(|error| format!("{field}.cert {cert_path}: last certificate: {error}"))?;
        verify_server_cert_signed_by_trust_anchor(
            &leaf,
            &anchor,
            &certified.cert[1..last],
            UnixTime::since_unix_epoch(Duration::from_secs(now.max(0).unsigned_abs())),
            provider.signature_verification_algorithms.all,
        )
        .map_err(|error| {
            format!(
                "{field}.cert {cert_path}: the chain does not link the leaf to the last certificate in the file: {error}"
            )
        })?;
    } else if !summaries[0].self_issued {
        note = Some(
            "the file holds only the leaf and not its issuer; clients that do not already have the issuing CA certificate will reject it",
        );
    }
    let not_after = summaries[0].not_after;
    let report = CertificateReport {
        field: field.clone(),
        names: source
            .names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        not_after: not_after.to_string(),
        days_left: (not_after.0 - now).div_euclid(86_400),
        note,
    };
    Ok((Arc::new(certified), report))
}

/// A static SNI map built at startup. Lookup is two hash probes at most and
/// never touches the filesystem, because rustls calls it synchronously in the
/// middle of a handshake.
#[derive(Debug, Default)]
struct CertificateResolver {
    exact: HashMap<Box<str>, Arc<Entry>>,
    /// Keyed by the parent domain: `*.example.net` is stored as
    /// `example.net` and matches exactly one additional label.
    wildcard: HashMap<Box<str>, Arc<Entry>>,
    /// Used when the client sends no SNI or a name nothing else matches.
    /// Without one, such a handshake fails.
    default: Option<Arc<Entry>>,
}

#[derive(Debug)]
struct Entry {
    key: Arc<CertifiedKey>,
    scope: Arc<CertificateScope>,
}

impl CertificateResolver {
    fn lookup(&self, server_name: &str) -> Option<&Arc<Entry>> {
        let name = server_name.to_ascii_lowercase();
        let name = name.strip_suffix('.').unwrap_or(&name);
        if let Some(entry) = self.exact.get(name) {
            return Some(entry);
        }
        let (_, parent) = name.split_once('.')?;
        self.wildcard.get(parent)
    }

    /// The single selection rule, shared by the handshake and by the scope
    /// lookup after it, so the two cannot disagree.
    fn select(&self, server_name: Option<&str>) -> Option<&Arc<Entry>> {
        server_name
            .and_then(|name| self.lookup(name))
            .or(self.default.as_ref())
    }
}

impl ResolvesServerCert for CertificateResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.select(client_hello.server_name())
            .map(|entry| Arc::clone(&entry.key))
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
        errln!(
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
    fn cancelled_reload_keeps_its_blocking_work_serialized() {
        let pki = test_support::Pki::new("cancel-reload");
        let leaf = pki.leaf("server", &["localhost"]);
        let tls = test_support::listener(
            "127.0.0.1:0".parse().unwrap(),
            Duration::from_secs(5),
            vec![test_support::source(&leaf, &["localhost"], true)],
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        runtime.block_on(async {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            started_rx.await.unwrap();
            let reload_tls = Arc::clone(&tls);
            let reload = tokio::spawn(async move { reload_tls.reload_certificates().await });
            tokio::time::timeout(Duration::from_secs(2), async {
                while tls.reload_gate.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            reload.abort();
            let _ = reload.await;
            let work_still_serialized = tls.reload_gate.try_lock().is_err();
            // Always release the blocking thread before asserting, including
            // the pre-fix failure case; runtime shutdown must not hang.
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tls.reload_certificates().await.unwrap();
            assert!(
                work_still_serialized,
                "cancelled reload released its gate while certificate work was still queued"
            );
        });
    }

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
        assert_eq!(
            TlsName::parse("127.0.0.1"),
            Ok(TlsName::Address([127, 0, 0, 1].into()))
        );
        for invalid in [
            "",
            "*",
            "*.com",
            "a.*.example.net",
            "*example.net",
            "**.example.net",
            "*.127.0.0.1",
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
        let entry = |name: &str| {
            let generated = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
            let key =
                PrivateKeyDer::from_pem_slice(generated.signing_key.serialize_pem().as_bytes())
                    .unwrap();
            Arc::new(Entry {
                key: Arc::new(
                    CertifiedKey::from_der(vec![generated.cert.der().clone()], key, &provider)
                        .unwrap(),
                ),
                scope: Arc::new(CertificateScope::for_names(&[name])),
            })
        };
        let exact = entry("api.example.net");
        let wildcard = entry("*.example.net");
        let fallback = entry("fallback.test");
        let resolver = CertificateResolver {
            exact: HashMap::from([("api.example.net".into(), Arc::clone(&exact))]),
            wildcard: HashMap::from([("example.net".into(), Arc::clone(&wildcard))]),
            default: Some(Arc::clone(&fallback)),
        };
        let same = |found: Option<&Arc<Entry>>, expected: &Arc<Entry>| {
            found.is_some_and(|found| Arc::ptr_eq(found, expected))
        };
        assert!(same(resolver.lookup("API.example.net"), &exact));
        assert!(same(resolver.lookup("www.example.net"), &wildcard));
        assert!(same(resolver.lookup("www.example.net."), &wildcard));
        assert!(resolver.lookup("example.net").is_none());
        assert!(resolver.lookup("a.b.example.net").is_none());
        assert!(resolver.lookup("example.org").is_none());
        assert!(same(resolver.select(Some("example.org")), &fallback));
        assert!(same(resolver.select(None), &fallback));
        assert!(same(resolver.select(Some("api.example.net")), &exact));
    }

    #[test]
    fn a_scope_covers_exactly_its_configured_names() {
        let scope =
            CertificateScope::for_names(&["a.test", "*.b.test", "127.0.0.1", "2001:db8::1"]);
        for covered in ["a.test", "A.TEST", "x.b.test", "127.0.0.1"] {
            assert!(scope.covers(covered), "{covered}");
        }
        for uncovered in [
            "b.test",
            "y.x.b.test",
            "c.test",
            "127.0.0.2",
            "localhost",
            "test",
        ] {
            assert!(!scope.covers(uncovered), "{uncovered}");
        }
        assert!(!CertificateScope::default().covers("a.test"));
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
