#![cfg(windows)]

//! Process-level rehearsal of the console-free control endpoint.
//!
//! Every test here starts a real `DETACHED_PROCESS` server, which is the state
//! a Service or supervisor leaves Hamster in: no console, so no Ctrl+C and no
//! Ctrl+Break. The endpoint is the only graceful stop and the only certificate
//! reload such a server has, so it is exercised through the shipped binary
//! rather than in-process.

use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
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
        ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName},
    },
};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const ERROR_ACCESS_DENIED: i32 = 5;

const TOKEN_DUPLICATE: u32 = 0x0002;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_IMPERSONATE: u32 = 0x0004;
/// `WinWorldSid`: the Everyone group.
const WIN_WORLD_SID: i32 = 1;

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn GetCurrentProcess() -> *mut c_void;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn CreateRestrictedToken(
        existing: *mut c_void,
        flags: u32,
        disable_sid_count: u32,
        disable_sids: *const SidAndAttributes,
        delete_privilege_count: u32,
        delete_privileges: *const c_void,
        restrict_sid_count: u32,
        restrict_sids: *const SidAndAttributes,
        new_token: *mut *mut c_void,
    ) -> i32;
    fn ImpersonateLoggedOnUser(token: *mut c_void) -> i32;
    fn RevertToSelf() -> i32;
    fn CreateWellKnownSid(
        kind: i32,
        domain: *const c_void,
        sid: *mut c_void,
        size: *mut u32,
    ) -> i32;
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hamster-control-{tag}-{}-{stamp}",
            std::process::id()
        ));
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

    fn exited(&mut self) -> Option<i32> {
        self.0.try_wait().unwrap().and_then(|status| status.code())
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

/// A unique endpoint name per test, because a pipe name is machine-global.
fn control_name(tag: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test-{tag}-{}-{stamp}", std::process::id())
}

fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\hamster-wheel-control-{name}")
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

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_detached(config: &Path, stdout: &Path, stderr: &Path) -> OwnedChild {
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

/// Runs the shipped control client, which is how an operator reaches a
/// console-free server.
fn control(config: &Path, command: &str) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"))
        .arg("--config")
        .arg(config)
        .arg("--control")
        .arg(command)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// One raw exchange, for protocol-level cases the client cannot express.
fn raw_exchange(name: &str, request: &str) -> String {
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path(name))
        .unwrap();
    pipe.write_all(request.as_bytes()).unwrap();
    pipe.flush().unwrap();
    let mut response = String::new();
    pipe.read_to_string(&mut response).unwrap();
    response
}

/// Not ignored, and deliberately cheap: wiring the control stop into the
/// server's shutdown future once made a server with no `[server.control]`
/// section stop the instant it began serving, because nothing held the
/// trigger's sender open. Only a started server shows that.
#[test]
fn a_server_without_a_control_endpoint_keeps_serving() {
    let scratch = Scratch::new("no-endpoint");
    let address = reserve_address();
    let config = scratch.file("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = \"{address}\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
             [[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n"
        ),
    )
    .unwrap();
    let log = scratch.file("server.stdout");
    let mut server = spawn_detached(&config, &log, &scratch.file("server.stderr"));
    wait_until("the listener", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on")
    });
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        server.exited(),
        None,
        "a server with no control endpoint stopped on its own: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(
        std::net::TcpStream::connect(address).is_ok(),
        "the listener is gone"
    );
    let recorded = fs::read_to_string(&log).unwrap();
    assert!(
        !recorded.contains("Control endpoint"),
        "no endpoint was configured: {recorded}"
    );
    assert!(!recorded.contains("stopped"), "{recorded}");
}

fn wait_for_exit(pid: u32, within: Duration) -> bool {
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // Already reaped by the time the handle was requested.
        return true;
    }
    let result = unsafe { WaitForSingleObject(handle, within.as_millis() as u32) };
    unsafe { CloseHandle(handle) };
    result == WAIT_OBJECT_0
}

fn php_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe")
}

fn worker_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php")
}

fn worker_pid(log: &Path) -> u32 {
    let mut pid = None;
    wait_until("a worker PID in the log", || {
        let text = fs::read_to_string(log).unwrap_or_default();
        pid = text
            .lines()
            .find_map(|line| line.strip_prefix("PHP worker pool "))
            .and_then(|line| line.split_whitespace().nth(2))
            .and_then(|value| value.parse::<u32>().ok());
        pid.is_some()
    });
    pid.unwrap()
}

#[test]
#[ignore = "starts a detached server and one PHP worker; requires php/8.5/php.exe"]
fn status_and_stop_reach_a_server_with_no_console() {
    let scratch = Scratch::new("stop");
    let name = control_name("stop");
    let address = reserve_address();
    let config = scratch.file("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = \"{address}\"\n\n[server.control]\nname = \"{name}\"\n\n\
             [[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\n\
             handler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\n\
             workers = 1\n",
            quoted(&php_executable()),
            quoted(&worker_script())
        ),
    )
    .unwrap();
    let log = scratch.file("server.stdout");
    let errors = scratch.file("server.stderr");
    let mut server = spawn_detached(&config, &log, &errors);
    wait_until("the listener", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on")
    });
    let server_pid = server.id();
    let worker = worker_pid(&log);

    let (ok, status) = control(&config, "status");
    assert!(ok, "status failed: {status}");
    assert!(status.starts_with("OK\n"), "{status}");
    assert!(status.contains(&format!("pid={server_pid}")), "{status}");
    assert!(
        status.contains(&format!("listener=http://{address}")),
        "{status}"
    );
    assert!(
        status.contains("php_pool=") && status.contains("ready=1"),
        "{status}"
    );

    let (ok, stopped) = control(&config, "stop");
    assert!(ok, "stop failed: {stopped}");
    assert!(stopped.contains("stopping=true"), "{stopped}");

    // Graceful stop, not TerminateProcess: the server exits by itself and its
    // worker is shut down rather than reclaimed by the job object.
    assert!(
        wait_for_exit(server_pid, Duration::from_secs(10)),
        "server did not exit after a control stop"
    );
    assert_eq!(server.exited(), Some(0));
    assert!(
        wait_for_exit(worker, Duration::from_secs(10)),
        "PHP worker survived a control stop"
    );
    let log = fs::read_to_string(&log).unwrap();
    assert!(log.contains("Control endpoint requested shutdown"), "{log}");
    assert!(log.contains("Hamster Wheel stopped"), "{log}");
    assert_eq!(fs::read_to_string(&errors).unwrap(), "");
    eprintln!(
        "control evidence: detached server pid={server_pid} worker={worker} stop=graceful exit=0"
    );
}

#[test]
#[ignore = "starts two detached servers that contend for one pipe name"]
fn a_second_server_cannot_take_an_endpoint_name_already_in_use() {
    let scratch = Scratch::new("name");
    let name = control_name("name");
    let first_config = scratch.file("first.toml");
    let second_config = scratch.file("second.toml");
    for (path, address) in [
        (&first_config, reserve_address()),
        (&second_config, reserve_address()),
    ] {
        fs::write(
            path,
            format!(
                "[server]\nlisten = \"{address}\"\n\n[server.control]\nname = \"{name}\"\n\n\
                 [[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\n\
                 handler = \"not_found\"\n"
            ),
        )
        .unwrap();
    }
    let first_log = scratch.file("first.stdout");
    let _first = spawn_detached(&first_config, &first_log, &scratch.file("first.stderr"));
    wait_until("the first listener", || {
        fs::read_to_string(&first_log)
            .unwrap_or_default()
            .contains("listening on")
    });

    let second_errors = scratch.file("second.stderr");
    let mut second = spawn_detached(
        &second_config,
        &scratch.file("second.stdout"),
        &second_errors,
    );
    wait_until("the second server to exit", || second.exited().is_some());
    assert_eq!(second.exited(), Some(1));
    let reported = fs::read_to_string(&second_errors).unwrap();
    assert!(
        reported.contains("hamster-wheel:"),
        "a name clash must be reported: {reported}"
    );
    // The first server still owns the endpoint.
    let (ok, status) = control(&first_config, "ping");
    assert!(ok, "{status}");
    eprintln!("control evidence: duplicate endpoint name refused at startup: {reported:?}");
}

#[test]
#[ignore = "starts a detached server and impersonates a restricted token"]
fn the_endpoint_denies_a_token_restricted_to_everyone() {
    let scratch = Scratch::new("acl");
    let name = control_name("acl");
    let address = reserve_address();
    let config = scratch.file("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = \"{address}\"\n\n[server.control]\nname = \"{name}\"\n\n\
             [[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\n\
             handler = \"not_found\"\n"
        ),
    )
    .unwrap();
    let log = scratch.file("server.stdout");
    let _server = spawn_detached(&config, &log, &scratch.file("server.stderr"));
    wait_until("the listener", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on")
    });

    // The owner can open it.
    assert!(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_path(&name))
            .is_ok()
    );

    // A thread whose effective access is checked against Everyone alone must
    // not: the DACL grants LocalSystem and the owner only. A restricting SID
    // forces a second access check against that SID set, which is how this
    // proves the DACL rather than re-reading it.
    let mut process_token = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE,
                &mut process_token,
            )
        },
        0,
        "cannot open this process's token"
    );
    let mut everyone = [0_u8; 68];
    let mut everyone_size = everyone.len() as u32;
    assert_ne!(
        unsafe {
            CreateWellKnownSid(
                WIN_WORLD_SID,
                std::ptr::null(),
                everyone.as_mut_ptr().cast::<c_void>(),
                &mut everyone_size,
            )
        },
        0
    );
    let restricting = SidAndAttributes {
        sid: everyone.as_mut_ptr().cast::<c_void>(),
        attributes: 0,
    };
    let mut restricted = std::ptr::null_mut();
    let created = unsafe {
        CreateRestrictedToken(
            process_token,
            0,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            1,
            &restricting,
            &mut restricted,
        )
    };
    assert_ne!(created, 0, "cannot build a restricted token");
    let impersonated = unsafe { ImpersonateLoggedOnUser(restricted) };
    let denied = if impersonated != 0 {
        let attempt = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_path(&name));
        let outcome = attempt.err().and_then(|error| error.raw_os_error());
        assert_ne!(unsafe { RevertToSelf() }, 0, "impersonation must be undone");
        outcome
    } else {
        unsafe { CloseHandle(restricted) };
        unsafe { CloseHandle(process_token) };
        panic!("cannot impersonate a restricted token derived from this process");
    };
    unsafe { CloseHandle(restricted) };
    unsafe { CloseHandle(process_token) };
    assert_eq!(
        denied,
        Some(ERROR_ACCESS_DENIED),
        "a restricted token must be refused by the pipe DACL"
    );

    // The owner still has access after the impersonation was reverted.
    let (ok, status) = control(&config, "ping");
    assert!(ok, "{status}");
    eprintln!("control evidence: restricted-token open refused with ERROR_ACCESS_DENIED (5)");
}

#[test]
#[ignore = "starts a detached server and holds a control connection open"]
fn a_silent_client_is_released_and_never_delays_shutdown() {
    let scratch = Scratch::new("silent");
    let name = control_name("silent");
    let address = reserve_address();
    let config = scratch.file("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = \"{address}\"\n\n[server.control]\nname = \"{name}\"\n\n\
             [[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\n\
             handler = \"not_found\"\n"
        ),
    )
    .unwrap();
    let log = scratch.file("server.stdout");
    let mut server = spawn_detached(&config, &log, &scratch.file("server.stderr"));
    wait_until("the listener", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on")
    });
    let server_pid = server.id();

    // Connected, never sends a request line.
    let mut silent = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path(&name))
        .unwrap();

    // Another client is served while the silent one is still connected.
    let (ok, status) = control(&config, "status");
    assert!(ok, "{status}");

    // An unknown request is refused rather than closing without an answer.
    assert!(raw_exchange(&name, "explode\n").starts_with("ERR unknown_command\n"));

    // The exchange deadline releases the silent connection by itself.
    let released = Instant::now();
    let mut sink = Vec::new();
    silent.read_to_end(&mut sink).unwrap_or_default();
    let waited = released.elapsed();
    assert!(
        waited < Duration::from_secs(15),
        "a silent control client was never released: {waited:?}"
    );
    assert!(sink.is_empty(), "a silent client must not be answered");

    // Shutdown is not held by control state.
    let (ok, stopped) = control(&config, "stop");
    assert!(ok, "{stopped}");
    assert!(
        wait_for_exit(server_pid, Duration::from_secs(10)),
        "a control stop did not end the server"
    );
    assert_eq!(server.exited(), Some(0));
    eprintln!(
        "control evidence: silent client released after {waited:?}; stop still graceful (exit 0)"
    );
}

#[tokio::test]
#[ignore = "starts a detached TLS server and replaces its certificate files"]
async fn reload_certificates_swaps_the_served_leaf_and_keeps_the_old_set_on_failure() {
    let scratch = Scratch::new("reload");
    let name = control_name("reload");
    let mut authority = CertificateParams::new(Vec::<String>::new()).unwrap();
    authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = CertifiedIssuer::self_signed(authority, KeyPair::generate().unwrap()).unwrap();
    let first_key = KeyPair::generate().unwrap();
    let first = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&first_key, &issuer)
        .unwrap();
    let second_key = KeyPair::generate().unwrap();
    let second = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&second_key, &issuer)
        .unwrap();
    assert_ne!(first.der(), second.der());

    let certificate = scratch.file("leaf.pem");
    let key = scratch.file("leaf.key");
    fs::write(&certificate, format!("{}{}", first.pem(), issuer.pem())).unwrap();
    fs::write(&key, first_key.serialize_pem()).unwrap();
    let address = reserve_address();
    let config = scratch.file("tls.toml");
    fs::write(
        &config,
        format!(
            "[server.tls]\nlisten = \"{address}\"\n\n[[server.tls.certificates]]\n\
             cert = {}\nkey = {}\nnames = [\"localhost\"]\ndefault = true\n\n\
             [server.control]\nname = \"{name}\"\n\n[[hosts]]\nname = \"localhost\"\n\
             locations = [{{ path = \"/\", handler = \"not_found\" }}]\n",
            quoted(&certificate),
            quoted(&key)
        ),
    )
    .unwrap();
    let log = scratch.file("server.stdout");
    let mut server = spawn_detached(&config, &log, &scratch.file("server.stderr"));
    wait_until("the TLS listener", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("listening on")
    });

    let mut roots = RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    let connector = TlsConnector::from(std::sync::Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    assert_eq!(served_leaf(&connector, address).await, *first.der());

    // A valid replacement is adopted.
    fs::write(&certificate, format!("{}{}", second.pem(), issuer.pem())).unwrap();
    fs::write(&key, second_key.serialize_pem()).unwrap();
    let (ok, reloaded) = control(&config, "reload-certificates");
    assert!(ok, "reload failed: {reloaded}");
    assert!(reloaded.starts_with("OK\n"), "{reloaded}");
    assert!(reloaded.contains("certificates=1"), "{reloaded}");
    assert_eq!(served_leaf(&connector, address).await, *second.der());

    // A key that does not match the certificate must leave the adopted set
    // serving, and must say so.
    fs::write(&key, first_key.serialize_pem()).unwrap();
    let (ok, refused) = control(&config, "reload-certificates");
    assert!(!ok, "a mismatched key must fail: {refused}");
    assert!(refused.starts_with("ERR reload_failed\n"), "{refused}");
    assert!(refused.contains("active_set=previous"), "{refused}");
    assert_eq!(
        served_leaf(&connector, address).await,
        *second.der(),
        "a failed reload must not disturb the serving set"
    );

    let (ok, stopped) = control(&config, "stop");
    assert!(ok, "{stopped}");
    let server_pid = server.id();
    assert!(wait_for_exit(server_pid, Duration::from_secs(10)));
    assert_eq!(server.exited(), Some(0));
    eprintln!("control evidence: reload adopted a new leaf, then rolled back and kept serving it");
}

async fn served_leaf(connector: &TlsConnector, address: SocketAddr) -> CertificateDer<'static> {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let session = connector.connect(name, stream).await.unwrap();
    let (_, connection) = session.get_ref();
    connection.peer_certificates().unwrap()[0]
        .clone()
        .into_owned()
}
