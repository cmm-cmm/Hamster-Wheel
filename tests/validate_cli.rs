//! `--validate` through the shipped binary.
//!
//! The point of the operation is that it compiles a configuration without
//! starting anything, so the test that matters most takes the port the
//! configuration names and binds it afterwards: if validation had bound it,
//! that bind would fail.

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// A directory this test owns, removed unless the test failed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/validate-cli-tests")
            .join(format!("{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// An address nothing is listening on, released before it is used.
fn reserve_address() -> SocketAddr {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap()
}

/// Runs the binary and returns (success, stdout, stderr).
fn validate(arguments: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"))
        .args(arguments)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn minimal(address: SocketAddr) -> String {
    format!(
        "[server]\nlisten = \"{address}\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
         [[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n"
    )
}

#[test]
fn a_valid_configuration_reports_what_it_would_serve_and_exits_zero() {
    let scratch = Scratch::new("ok");
    let address = reserve_address();
    let config = scratch.write("hamster.toml", &minimal(address));

    let (ok, report, errors) = validate(&["--config", config.to_str().unwrap(), "--validate"]);
    assert!(ok, "validation failed: {errors}");
    assert!(report.starts_with("configuration=ok\n"), "{report}");
    assert!(
        report.contains(&format!("listener=http://{address}")),
        "{report}"
    );
    assert!(report.contains("max_connections=1024"), "{report}");
    assert!(
        report.contains("not_checked=listener_bind,log_write,worker_startup,backend_reachability"),
        "{report}"
    );
    assert_eq!(errors, "", "a successful validation says nothing on stderr");
}

#[test]
fn validation_binds_no_listener() {
    let scratch = Scratch::new("unbound");
    let address = reserve_address();
    let config = scratch.write("hamster.toml", &minimal(address));

    let (ok, report, errors) = validate(&["--config", config.to_str().unwrap(), "--validate"]);
    assert!(ok, "{errors}");
    assert!(
        report.contains(&format!("listener=http://{address}")),
        "{report}"
    );

    // The configured address is still free, which is the whole claim of the
    // operation: an operator can check a replacement configuration while the
    // current server keeps serving.
    let taken = TcpListener::bind(address);
    assert!(
        taken.is_ok(),
        "validation left something bound on {address}: {:?}",
        taken.err()
    );
}

#[test]
fn an_invalid_configuration_is_reported_on_stderr_with_exit_one() {
    let scratch = Scratch::new("invalid");
    let config = scratch.write(
        "hamster.toml",
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
         [[hosts.locations]]\npath = \"/\"\nhandler = \"nonsense\"\n",
    );

    let (ok, report, errors) = validate(&["--config", config.to_str().unwrap(), "--validate"]);
    assert!(!ok, "an invalid handler must not validate: {report}");
    assert!(errors.contains("hamster-wheel:"), "{errors}");
    assert!(errors.contains("nonsense"), "{errors}");
    // Nothing is reported as ok when nothing compiled.
    assert!(!report.contains("configuration=ok"), "{report}");
}

#[test]
fn a_certificate_that_cannot_be_loaded_fails_validation() {
    let scratch = Scratch::new("tls");
    let missing = scratch.0.join("absent.pem");
    let config = scratch.write(
        "hamster.toml",
        &format!(
            "[server.tls]\nlisten = \"127.0.0.1:8443\"\n\n[[server.tls.certificates]]\n\
             cert = '{}'\nkey = '{}'\nnames = [\"localhost\"]\ndefault = true\n\n\
             [[hosts]]\nname = \"localhost\"\n\
             locations = [{{ path = \"/\", handler = \"not_found\" }}]\n",
            missing.display(),
            missing.display()
        ),
    );

    let (ok, _report, errors) = validate(&["--config", config.to_str().unwrap(), "--validate"]);
    assert!(
        !ok,
        "a certificate that does not exist must fail validation: {errors}"
    );
    assert!(errors.contains("hamster-wheel:"), "{errors}");
}

#[test]
fn the_built_in_configuration_validates() {
    // No `--config`, which is the development default a new operator meets
    // first.
    let (ok, report, errors) = validate(&["--validate"]);
    assert!(ok, "{errors}");
    assert!(
        report.contains("listener=http://127.0.0.1:8080"),
        "{report}"
    );
}

#[test]
fn validate_is_rejected_beside_a_control_command() {
    let (ok, _report, errors) = validate(&["--validate", "--control", "status"]);
    assert!(!ok);
    assert!(errors.contains("usage: hamster-wheel"), "{errors}");
}

#[test]
#[ignore = "validates a PHP pool configuration; requires php/8.5/php.exe"]
fn a_worker_pool_validates_without_starting_a_worker() {
    let scratch = Scratch::new("pool");
    let address = reserve_address();
    let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/php-worker/worker.php");
    let config = scratch.write(
        "hamster.toml",
        &format!(
            "[server]\nlisten = \"{address}\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
             [[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n\
             [hosts.locations.php]\nexecutable = '{}'\nscript = '{}'\nworkers = 4\n",
            executable.display(),
            script.display()
        ),
    );

    let (ok, report, errors) = validate(&["--config", config.to_str().unwrap(), "--validate"]);
    assert!(ok, "{errors}");
    assert!(report.contains("php_pool=localhost/ workers=4"), "{report}");
    // A started pool reports every worker reaching READY. None may appear.
    assert!(
        !report.contains("ready after"),
        "validation started a worker: {report}"
    );
    assert_eq!(errors, "");
    // And the listener the pool would have served on is still free.
    assert!(TcpListener::bind(address).is_ok());
}
