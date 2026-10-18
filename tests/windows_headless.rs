#![cfg(windows)]

//! Process-level headless rehearsal. This does not install a Windows Service.

use std::{
    ffi::c_void,
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use tokio_rustls::{
    TlsConnector,
    rustls::{
        self, ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName},
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn FreeConsole() -> i32;
    fn AttachConsole(pid: u32) -> i32;
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("hamster-headless-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct OwnedChild(Child);

impl OwnedChild {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn stop_abnormally(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn reserve_address() -> SocketAddr {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "headless readiness timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

fn request(address: SocketAddr, host: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn spawn_hamster(config: &Path, stdout: &Path, stderr: &Path) -> OwnedChild {
    OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_hamster-wheel"))
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(File::create(stdout).unwrap())
            .stderr(File::create(stderr).unwrap())
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .unwrap(),
    )
}

fn wait_for_worker_pid(log: &Path) -> u32 {
    let mut pid = None;
    wait_until(|| {
        pid = fs::read_to_string(log).ok().and_then(|output| {
            output.lines().find_map(|line| {
                line.strip_prefix("PHP worker pool ")?
                    .split_once(" worker ")?
                    .1
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
        });
        pid.is_some()
    });
    pid.unwrap()
}

#[test]
#[ignore = "helper: inspect whether a DETACHED_PROCESS server has a console"]
fn console_probe_entry() {
    let Ok(target) = std::env::var("HAMSTER_HEADLESS_TARGET") else {
        return;
    };
    // The test executable may inherit its harness console even when
    // CREATE_NO_WINDOW is requested. Detach this helper only before probing.
    unsafe { FreeConsole() };
    let attached = unsafe { AttachConsole(target.parse().unwrap()) };
    assert_eq!(attached, 0, "headless server unexpectedly has a console");
    let error = std::io::Error::last_os_error();
    assert_eq!(
        error.raw_os_error(),
        Some(6),
        "AttachConsole should report ERROR_INVALID_HANDLE for a console-free target"
    );
}

#[test]
#[ignore = "requires repository php/8.5/php.exe and php-cgi.exe"]
fn headless_crash_restart_keeps_operator_endpoint_separate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let php = root.join("php/8.5/php.exe");
    let cgi = root.join("php/8.5/php-cgi.exe");
    assert!(
        php.is_file() && cgi.is_file(),
        "local PHP distribution required"
    );
    let scratch = Scratch::new();
    let backend = reserve_address();
    let listen = reserve_address();
    let mut endpoint = OwnedChild(
        Command::new(&cgi)
            .arg("-b")
            .arg(backend.to_string())
            .args(["-d", "display_errors=0"])
            .env("PHP_FCGI_MAX_REQUESTS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(scratch.file("cgi.stderr")).unwrap())
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .unwrap(),
    );
    wait_until(|| TcpStream::connect(backend).is_ok());

    let config = scratch.file("hamster.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = '{listen}'\n\n[[hosts]]\nname = 'worker.test'\n[[hosts.locations]]\npath = '/'\nhandler = 'php_worker'\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = 1\n\n[[hosts]]\nname = 'cgi.test'\n[[hosts.locations]]\npath = '/'\nhandler = 'fastcgi'\nroot = {}\n[hosts.locations.fastcgi]\nservers = ['{backend}']\n",
            quoted(&php),
            quoted(&root.join("fixtures/php-worker/worker.php")),
            quoted(&root.join("fixtures/php-cgi")),
        ),
    )
    .unwrap();

    let first_log = scratch.file("first.stdout");
    let first_err = scratch.file("first.stderr");
    let mut first = spawn_hamster(&config, &first_log, &first_err);
    wait_until(|| {
        fs::read_to_string(&first_log)
            .unwrap_or_default()
            .contains("listening on")
    });
    let worker_pid = wait_for_worker_pid(&first_log);
    let helper = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "console_probe_entry"])
        .env("HAMSTER_HEADLESS_TARGET", first.id().to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(
        helper.status.success(),
        "console probe stdout={} stderr={}",
        String::from_utf8_lossy(&helper.stdout),
        String::from_utf8_lossy(&helper.stderr)
    );
    assert!(request(listen, "worker.test").contains("Hello from PHP"));
    assert!(request(listen, "cgi.test").contains("sapi=cgi-fcgi"));

    let worker_handle = unsafe { OpenProcess(SYNCHRONIZE, 0, worker_pid) };
    assert!(!worker_handle.is_null(), "open worker before parent crash");
    assert_eq!(
        unsafe { WaitForSingleObject(worker_handle, 0) },
        WAIT_TIMEOUT
    );
    let first_pid = first.id();
    first.stop_abnormally();
    let started = Instant::now();
    let result = unsafe { WaitForSingleObject(worker_handle, 5_000) };
    unsafe { CloseHandle(worker_handle) };
    assert_eq!(result, WAIT_OBJECT_0, "worker survived Hamster crash");
    assert!(
        endpoint.0.try_wait().unwrap().is_none(),
        "external php-cgi unexpectedly exited"
    );
    eprintln!(
        "headless evidence: hamster_pid={first_pid} worker_pid={worker_pid} cgi_pid={} mechanism=TerminateProcess worker_reclaim_ms={}",
        endpoint.id(),
        started.elapsed().as_millis()
    );

    let second_log = scratch.file("second.stdout");
    let second_err = scratch.file("second.stderr");
    let mut second = spawn_hamster(&config, &second_log, &second_err);
    wait_until(|| {
        fs::read_to_string(&second_log)
            .unwrap_or_default()
            .contains("listening on")
    });
    let replacement_pid = wait_for_worker_pid(&second_log);
    assert_ne!(worker_pid, replacement_pid);
    assert!(request(listen, "worker.test").contains("Hello from PHP"));
    assert!(request(listen, "cgi.test").contains("sapi=cgi-fcgi"));
    assert!(fs::read_to_string(&first_err).unwrap().is_empty());
    assert!(fs::read_to_string(&second_err).unwrap().is_empty());
    eprintln!(
        "headless evidence: restarted_hamster_pid={} replacement_worker_pid={} cgi_pid={} stdout_captured=true",
        second.id(),
        replacement_pid,
        endpoint.id()
    );
    let replacement_handle = unsafe { OpenProcess(SYNCHRONIZE, 0, replacement_pid) };
    assert!(!replacement_handle.is_null());
    assert_eq!(
        unsafe { WaitForSingleObject(replacement_handle, 0) },
        WAIT_TIMEOUT
    );
    second.stop_abnormally();
    assert_eq!(
        unsafe { WaitForSingleObject(replacement_handle, 5_000) },
        WAIT_OBJECT_0
    );
    unsafe { CloseHandle(replacement_handle) };
    endpoint.stop_abnormally();
}

#[tokio::test]
async fn detached_process_keeps_loaded_tls_certificate_until_reload_trigger() {
    let scratch = Scratch::new();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let cert_path = scratch.file("active.pem");
    let key_path = scratch.file("active.key");
    let replacement_key = KeyPair::generate().unwrap();
    let replacement = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&replacement_key, &issuer)
        .unwrap();
    let initial_key = KeyPair::generate().unwrap();
    let initial = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&initial_key, &issuer)
        .unwrap();
    assert_ne!(initial.der(), replacement.der());
    fs::write(&cert_path, format!("{}{}", initial.pem(), issuer.pem())).unwrap();
    fs::write(&key_path, initial_key.serialize_pem()).unwrap();
    let address = reserve_address();
    let config = scratch.file("tls.toml");
    fs::write(
        &config,
        format!(
            "[server.tls]\nlisten = '{address}'\n[[server.tls.certificates]]\ncert = {}\nkey = {}\nnames = ['localhost']\ndefault = true\n[[hosts]]\nname = 'localhost'\nlocations = [{{ path = '/', handler = 'not_found' }}]\n",
            quoted(&cert_path),
            quoted(&key_path)
        ),
    )
    .unwrap();
    let log = scratch.file("tls.stdout");
    let err = scratch.file("tls.stderr");
    let server = spawn_hamster(&config, &log, &err);
    wait_until(|| {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on https://")
    });

    // A new client config on every connection rules out TLS resumption as an
    // explanation for seeing the previously loaded certificate.
    let connector = || {
        let mut roots = RootCertStore::empty();
        roots.add(issuer.der().clone()).unwrap();
        let client = ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        TlsConnector::from(std::sync::Arc::new(client))
    };
    async fn served_leaf(connector: &TlsConnector, address: SocketAddr) -> CertificateDer<'static> {
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let stream = connector
            .connect(ServerName::try_from("localhost").unwrap(), socket)
            .await
            .unwrap();
        stream.get_ref().1.peer_certificates().unwrap()[0].clone()
    }
    assert_eq!(
        served_leaf(&connector(), address).await,
        initial.der().clone()
    );
    fs::write(&cert_path, format!("{}{}", replacement.pem(), issuer.pem())).unwrap();
    fs::write(&key_path, replacement_key.serialize_pem()).unwrap();
    assert_eq!(
        served_leaf(&connector(), address).await,
        initial.der().clone()
    );
    assert_ne!(
        served_leaf(&connector(), address).await,
        replacement.der().clone()
    );
    assert!(
        !fs::read_to_string(&log)
            .unwrap()
            .contains("TLS certificate reload succeeded")
    );
    eprintln!(
        "headless TLS evidence: hamster_pid={} certificate_changed_on_disk=true new_handshake_kept_loaded_certificate=true",
        server.id()
    );
}
