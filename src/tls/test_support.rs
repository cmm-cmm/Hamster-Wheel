//! A throwaway PKI and TLS clients for tests over real sockets.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use tokio::net::TcpStream;
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{
        self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        pki_types::{CertificateDer, ServerName, UnixTime},
    },
};

use super::{CertificateSource, TlsListener, TlsName, TlsSource};

/// A CA plus leaf certificates written as PEM files under a private
/// temporary directory, removed on drop.
pub(crate) struct Pki {
    pub(crate) dir: PathBuf,
    ca: CertificateDer<'static>,
    issuer: CertifiedIssuer<'static, KeyPair>,
}

pub(crate) struct Leaf {
    pub(crate) cert: PathBuf,
    pub(crate) key: PathBuf,
    pub(crate) der: CertificateDer<'static>,
}

impl Pki {
    pub(crate) fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("hamster-wheel-tls-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create PKI directory");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer =
            CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key")).expect("CA");
        Self {
            dir,
            ca: issuer.der().clone(),
            issuer,
        }
    }

    /// Issues a leaf for `names` and writes `<file>.crt` (leaf then CA, a
    /// complete chain) and `<file>.key`.
    pub(crate) fn leaf(&self, file: &str, names: &[&str]) -> Leaf {
        let key = KeyPair::generate().expect("leaf key");
        let params = CertificateParams::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .expect("leaf params");
        let certificate = params.signed_by(&key, &self.issuer).expect("sign leaf");
        let cert = self.dir.join(format!("{file}.crt"));
        let key_path = self.dir.join(format!("{file}.key"));
        std::fs::write(&cert, format!("{}{}", certificate.pem(), self.issuer.pem()))
            .expect("write leaf chain");
        std::fs::write(&key_path, key.serialize_pem()).expect("write leaf key");
        Leaf {
            cert,
            key: key_path,
            der: certificate.der().clone(),
        }
    }

    /// A client that verifies the chain and name against this CA.
    pub(crate) fn client(&self, alpn: &[&[u8]]) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca.clone()).expect("trust test CA");
        let mut config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("client versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
        TlsConnector::from(Arc::new(config))
    }
}

impl Drop for Pki {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn source(leaf: &Leaf, names: &[&str], default: bool) -> CertificateSource {
    CertificateSource {
        field: "test.certificates[0]".to_owned(),
        cert: leaf.cert.clone(),
        key: leaf.key.clone(),
        names: names
            .iter()
            .map(|name| TlsName::parse(name).expect("valid test name"))
            .collect(),
        default,
    }
}

pub(crate) fn listener(
    address: SocketAddr,
    handshake_timeout: Duration,
    certificates: Vec<CertificateSource>,
) -> Arc<TlsListener> {
    Arc::new(
        TlsListener::compile(TlsSource {
            listen: address,
            handshake_timeout,
            certificates,
        })
        .expect("compile test TLS listener"),
    )
}

/// A client that accepts any certificate and records the one it was shown,
/// so a test can assert which certificate SNI selected even when the name
/// would not verify.
pub(crate) fn recording_client(
    sni: bool,
    alpn: &[&[u8]],
) -> (TlsConnector, Arc<Mutex<Option<CertificateDer<'static>>>>) {
    let seen = Arc::new(Mutex::new(None));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("client versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Recording {
            seen: Arc::clone(&seen),
            provider,
        }))
        .with_no_client_auth();
    config.enable_sni = sni;
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    (TlsConnector::from(Arc::new(config)), seen)
}

pub(crate) async fn connect(
    connector: &TlsConnector,
    address: SocketAddr,
    name: &str,
) -> std::io::Result<TlsStream<TcpStream>> {
    let stream = TcpStream::connect(address).await?;
    let name = ServerName::try_from(name.to_owned()).expect("test server name");
    connector.connect(name, stream).await
}

#[derive(Debug)]
struct Recording {
    seen: Arc<Mutex<Option<CertificateDer<'static>>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Recording {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().expect("record certificate") = Some(end_entity.clone().into_owned());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
