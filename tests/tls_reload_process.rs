#![cfg(windows)]

//! Exercises the actual console trigger against a separate server process.

mod common;

use std::{
    net::SocketAddr,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Command,
    sync::mpsc,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{
        self, ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName},
    },
};

const GUARD: Duration = Duration::from_secs(10);
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hamster-tls-reload-process-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Leaf {
    cert: PathBuf,
    key: PathBuf,
    der: CertificateDer<'static>,
}

fn issue_leaf(directory: &Path, file: &str, issuer: &CertifiedIssuer<'static, KeyPair>) -> Leaf {
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, issuer)
        .unwrap();
    let cert_path = directory.join(format!("{file}.pem"));
    let key_path = directory.join(format!("{file}.key"));
    std::fs::write(&cert_path, format!("{}{}", cert.pem(), issuer.pem())).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    Leaf {
        cert: cert_path,
        key: key_path,
        der: cert.der().clone(),
    }
}

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('\\', "/"))
}

async fn next_line(
    lines: &mut mpsc::UnboundedReceiver<(&'static str, String)>,
    prefix: &'static str,
    stream: &'static str,
) -> String {
    timeout(GUARD, async {
        while let Some((from, line)) = lines.recv().await {
            if from == stream && line.contains(prefix) {
                return line;
            }
        }
        panic!("server output closed before {prefix}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {prefix}"))
}

async fn connect(connector: &TlsConnector, address: SocketAddr) -> TlsStream<TcpStream> {
    let socket = TcpStream::connect(address).await.unwrap();
    timeout(
        GUARD,
        connector.connect(ServerName::try_from("localhost").unwrap(), socket),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn request(stream: &mut TlsStream<TcpStream>) {
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut received = Vec::new();
    timeout(GUARD, async {
        loop {
            if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let head = std::str::from_utf8(&received[..end + 4]).unwrap();
                assert!(head.starts_with("HTTP/1.1 404 "), "{head}");
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if received.len() >= end + 4 + length {
                    return;
                }
            }
            let mut bytes = [0_u8; 4_096];
            let count = stream.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0, "old TLS connection closed");
            received.extend_from_slice(&bytes[..count]);
        }
    })
    .await
    .unwrap();
}

#[test]
#[ignore = "helper for the Windows process Ctrl+Break test"]
fn ctrl_break_helper_entry() {
    let Ok(target) = std::env::var("HAMSTER_TLS_RELOAD_TARGET") else {
        return;
    };
    let target = target.parse::<u32>().unwrap();
    unsafe extern "system" fn ignore_console_event(_: u32) -> i32 {
        1
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(process_id: u32) -> i32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }
    unsafe {
        FreeConsole();
        assert_ne!(AttachConsole(target), 0, "attach to server console");
        assert_ne!(
            SetConsoleCtrlHandler(Some(ignore_console_event), 1),
            0,
            "ignore Ctrl+Break in helper"
        );
        assert_ne!(GenerateConsoleCtrlEvent(1, 0), 0, "send CTRL_BREAK_EVENT");
    }
    std::thread::sleep(Duration::from_millis(200));
}

async fn send_ctrl_break(pid: u32) {
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("ctrl_break_helper_entry")
        .env("HAMSTER_TLS_RELOAD_TARGET", pid.to_string())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "helper: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires a Windows desktop capable of creating a child console"]
async fn ctrl_break_reloads_a_real_server_process_and_rolls_back_on_error() {
    let scratch = Scratch::new();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let old = issue_leaf(&scratch.0, "old", &issuer);
    let new = issue_leaf(&scratch.0, "new", &issuer);
    let bad_key = KeyPair::generate().unwrap();
    let config_path = scratch.0.join("hamster.toml");
    std::fs::write(&config_path, format!(
        "[server.tls]\nlisten = \"127.0.0.1:0\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\ndefault = true\n\n[[hosts]]\nname = \"localhost\"\nlocations = [{{ path = \"/\", handler = \"not_found\" }}]\n",
        toml_path(&old.cert), toml_path(&old.key),
    )).unwrap();

    common::end_children_with_this_process();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"));
    command
        .arg("--config")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().creation_flags(CREATE_NEW_CONSOLE);
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    let (tx, mut lines) = mpsc::unbounded_channel();
    for (name, pipe) in [
        (
            "stdout",
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        ),
        (
            "stderr",
            Box::new(child.stderr.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        ),
    ] {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx.send((name, line));
            }
        });
    }
    drop(tx);
    let ready = next_line(&mut lines, "listening on https://", "stdout").await;
    let address = ready
        .split("https://")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<SocketAddr>()
        .unwrap();

    let mut roots = RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    let client = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client));
    let mut old_connection = connect(&connector, address).await;
    assert_eq!(
        old_connection.get_ref().1.peer_certificates().unwrap()[0],
        old.der
    );
    request(&mut old_connection).await;

    std::fs::copy(&new.cert, &old.cert).unwrap();
    std::fs::copy(&new.key, &old.key).unwrap();
    send_ctrl_break(pid).await;
    let success = next_line(&mut lines, "TLS certificate reload succeeded", "stdout").await;
    assert!(success.contains("1 certificate(s)"), "{success}");
    let new_connection = connect(&connector, address).await;
    assert_eq!(
        new_connection.get_ref().1.peer_certificates().unwrap()[0],
        new.der
    );
    request(&mut old_connection).await;
    assert_eq!(
        old_connection.get_ref().1.peer_certificates().unwrap()[0],
        old.der
    );

    std::fs::write(&old.key, bad_key.serialize_pem()).unwrap();
    send_ctrl_break(pid).await;
    let failure = next_line(&mut lines, "TLS certificate reload failed", "stderr").await;
    assert!(failure.contains("previous set remains active"), "{failure}");
    assert!(failure.contains("cannot be used together"), "{failure}");
    assert!(
        failure.contains("old.pem") && failure.contains("old.key"),
        "{failure}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "server exited after rejected reload"
    );
    let after_failure = connect(&connector, address).await;
    assert_eq!(
        after_failure.get_ref().1.peer_certificates().unwrap()[0],
        new.der
    );
    request(&mut old_connection).await;

    std::fs::copy(&new.key, &old.key).unwrap();
    std::fs::write(&old.cert, b"not a certificate").unwrap();
    send_ctrl_break(pid).await;
    let bad_certificate = next_line(&mut lines, "TLS certificate reload failed", "stderr").await;
    assert!(
        bad_certificate.contains("previous set remains active"),
        "{bad_certificate}"
    );
    assert!(
        bad_certificate.contains("old.pem")
            && bad_certificate.contains("contains no CERTIFICATE block"),
        "{bad_certificate}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "server exited after bad certificate"
    );
    let after_bad_certificate = connect(&connector, address).await;
    assert_eq!(
        after_bad_certificate
            .get_ref()
            .1
            .peer_certificates()
            .unwrap()[0],
        new.der
    );
    request(&mut old_connection).await;

    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

/// A reload reports on stdout. With stdout closed, that report must not end
/// the reload task: Windows ends a process on a Ctrl+Break nobody handles, so
/// a dead reload task turns the next reload request into process exit.
#[tokio::test]
#[ignore = "requires a Windows desktop capable of creating a child console"]
async fn ctrl_break_reloads_after_stdout_is_closed() {
    let scratch = Scratch::new();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let old = issue_leaf(&scratch.0, "old", &issuer);
    let new = issue_leaf(&scratch.0, "new", &issuer);
    let config_path = scratch.0.join("hamster.toml");
    std::fs::write(&config_path, format!(
        "[server.tls]\nlisten = \"127.0.0.1:0\"\n\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = [\"localhost\"]\ndefault = true\n\n[[hosts]]\nname = \"localhost\"\nlocations = [{{ path = \"/\", handler = \"not_found\" }}]\n",
        toml_path(&old.cert), toml_path(&old.key),
    )).unwrap();

    common::end_children_with_this_process();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"));
    command
        .arg("--config")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().creation_flags(CREATE_NEW_CONSOLE);
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    let stderr = child.stderr.take().unwrap();
    let diagnostics = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut output = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            output.push(line);
        }
        output
    });
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let address = timeout(GUARD, async {
        while let Some(line) = stdout.next_line().await.unwrap() {
            if let Some(rest) = line.split("https://").nth(1) {
                return rest
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<SocketAddr>()
                    .unwrap();
            }
        }
        panic!("server exited before listening");
    })
    .await
    .expect("server should become ready");
    drop(stdout);

    let mut roots = RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    let client = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client));

    std::fs::copy(&new.cert, &old.cert).unwrap();
    std::fs::copy(&new.key, &old.key).unwrap();
    for attempt in 1..=2 {
        send_ctrl_break(pid).await;
        // Nothing can be read from stdout any more; give the reload time.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "server exited with {status:?} after reload {attempt} with stdout closed: {:?}",
                diagnostics.await.unwrap()
            );
        }
    }
    let mut connection = connect(&connector, address).await;
    assert_eq!(
        connection.get_ref().1.peer_certificates().unwrap()[0],
        new.der
    );
    request(&mut connection).await;

    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let diagnostics = diagnostics.await.unwrap();
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}
