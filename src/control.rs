//! A local control endpoint for a Hamster that has no console.
//!
//! Windows delivers graceful stop and certificate reload through console
//! control events, and a server started with `DETACHED_PROCESS`, as a Service,
//! or under most supervisors has no console to receive them. Such a server
//! could previously only be stopped by `TerminateProcess`, which skips worker
//! shutdown, and its certificates could not be reloaded at all.
//!
//! # Shape
//!
//! One request per connection, answered, then closed. Herdr's socket API
//! (newline-delimited JSON with a request id echoed in each response) solves
//! correlation for an API with dozens of methods and long-lived event
//! subscriptions; this endpoint has four commands and no events, so a request
//! per connection removes correlation, read loops and idle-connection
//! bookkeeping instead of encoding them. It also needs no JSON dependency and
//! keeps the wire readable from PowerShell.
//!
//! Requests are one line: a command name and nothing else. A response is a
//! status line, `OK` or `ERR <code>`, then zero or more `key=value` detail
//! lines, then a blank line. Values never contain a newline.
//!
//! # Access control
//!
//! The pipe's DACL is set when the pipe is created, which is the only point at
//! which Windows lets it be set, and is protected so nothing is inherited:
//! `D:P(A;;GA;;;SY)(A;;GA;;;OW)` grants full access to `LocalSystem` and to the
//! pipe's owner, which is the account the server runs as, and grants nothing to
//! anybody else. `first_pipe_instance` makes creation fail if the name is
//! already taken, so a second process cannot serve the same name beside this
//! one, and remote clients are rejected.
//!
//! The endpoint is off unless `[server.control]` names it. It is a control
//! surface, so it exists only when an operator asks for one.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::config::RuntimeConfig;

/// Deadline for one control exchange, from connect to the written response.
///
/// A client that connects and sends nothing must not hold a pipe instance, and
/// must never delay shutdown.
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest request line accepted, including its newline.
const MAX_REQUEST_BYTES: usize = 256;

/// Control connections handled at once.
///
/// The commands are administrative and rare. A small ceiling bounds the work a
/// client with pipe access can queue without rejecting the one request an
/// operator actually makes.
const MAX_CONCURRENT_EXCHANGES: usize = 4;

/// Security descriptor for the control pipe: LocalSystem and the owner only.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Ping,
    Status,
    Stop,
    ReloadCertificates,
}

impl Command {
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "ping" => Some(Self::Ping),
            "status" => Some(Self::Status),
            "stop" => Some(Self::Stop),
            "reload-certificates" => Some(Self::ReloadCertificates),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Status => "status",
            Self::Stop => "stop",
            Self::ReloadCertificates => "reload-certificates",
        }
    }

    pub const ALL: [Self; 4] = [
        Self::Ping,
        Self::Status,
        Self::Stop,
        Self::ReloadCertificates,
    ];
}

/// A pipe name is part of a filesystem-like namespace, so it is restricted to
/// characters that cannot change which object is opened.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("server.control.name must not be empty".to_owned());
    }
    if name.len() > 64 {
        return Err("server.control.name must be at most 64 characters".to_owned());
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(
            "server.control.name may contain only ASCII letters, digits, '-', '_' and '.'"
                .to_owned(),
        );
    }
    Ok(())
}

/// The full pipe path for a configured name.
pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\hamster-wheel-control-{name}")
}

/// What a command produced, ready to be written to a client.
pub(crate) struct Response {
    pub(crate) body: String,
    pub(crate) stop_requested: bool,
}

impl Response {
    fn ok(details: String) -> Self {
        Self {
            body: format!("OK\n{details}\n"),
            stop_requested: false,
        }
    }

    fn error(code: &str, message: &str) -> Self {
        Self {
            body: format!("ERR {code}\nmessage={}\n\n", single_line(message)),
            stop_requested: false,
        }
    }

    pub(crate) fn unknown_command() -> Self {
        let names = Command::ALL
            .iter()
            .map(|command| command.name())
            .collect::<Vec<_>>()
            .join(", ");
        Self::error("unknown_command", &format!("known commands: {names}"))
    }

    pub(crate) fn request_too_long() -> Self {
        Self::error(
            "request_too_long",
            &format!("a request line may be at most {MAX_REQUEST_BYTES} bytes"),
        )
    }
}

/// Detail values are single-line by construction, so a value can never be read
/// as another key or as the end of the response.
fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// Runs one command against the live server.
pub(crate) async fn execute(
    command: Command,
    config: &Arc<RuntimeConfig>,
    started: Instant,
) -> Response {
    match command {
        Command::Ping => Response::ok(format!("pid={}", std::process::id())),
        Command::Status => Response::ok(status_details(config, started)),
        Command::Stop => Response {
            // Answered before the shutdown is triggered, so the client learns
            // the request was accepted even though the pipe is about to close.
            body: format!("OK\npid={}\nstopping=true\n\n", std::process::id()),
            stop_requested: true,
        },
        Command::ReloadCertificates => reload_certificates(config).await,
    }
}

fn status_details(config: &Arc<RuntimeConfig>, started: Instant) -> String {
    let mut details = String::new();
    let _ = writeln!(details, "pid={}", std::process::id());
    let _ = writeln!(
        details,
        "uptime_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    if let Some(address) = config.listen_address() {
        let _ = writeln!(details, "listener=http://{address}");
    }
    if let Some(tls) = config.tls() {
        let _ = writeln!(details, "listener=https://{}", tls.address());
        let certificates = tls.certificates();
        let _ = writeln!(details, "certificates={}", certificates.len());
        for certificate in certificates.iter() {
            let _ = writeln!(
                details,
                "certificate={} names={} not_after={} days_left={}",
                single_line(&certificate.field),
                single_line(&certificate.names),
                single_line(&certificate.not_after),
                certificate.days_left
            );
        }
        let _ = writeln!(
            details,
            "handshakes_timed_out={}",
            tls.handshakes_timed_out()
        );
    }
    let _ = writeln!(
        details,
        "max_connections={}",
        config.http_limits().max_connections()
    );
    for pool in config.php_workers() {
        let snapshot = pool.snapshot();
        let _ = writeln!(
            details,
            "php_pool={} target={} live={} ready={}",
            single_line(pool.label()),
            snapshot.target_workers,
            snapshot.live_workers,
            snapshot.ready_workers
        );
    }
    for upstream in config.upstreams() {
        let _ = writeln!(
            details,
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
            details,
            "fastcgi={} servers={servers}",
            single_line(backend.label())
        );
    }
    details.pop();
    details
}

async fn reload_certificates(config: &Arc<RuntimeConfig>) -> Response {
    let Some(tls) = config.tls() else {
        return Response::error(
            "no_tls_listener",
            "this server has no [server.tls] listener to reload",
        );
    };
    // The whole replacement set is loaded and verified before one atomic swap;
    // a failure leaves the previously loaded set serving. This endpoint reports
    // that outcome, it does not implement it.
    match tls.reload_certificates().await {
        Ok(reports) => {
            let mut details = format!("certificates={}\n", reports.len());
            for report in reports.iter() {
                let _ = writeln!(
                    details,
                    "certificate={} names={} not_after={} days_left={}",
                    single_line(&report.field),
                    single_line(&report.names),
                    single_line(&report.not_after),
                    report.days_left
                );
                if let Some(note) = report.note {
                    let _ = writeln!(
                        details,
                        "note={} {}",
                        single_line(&report.field),
                        single_line(note)
                    );
                }
            }
            details.pop();
            Response::ok(details)
        }
        Err(error) => Response {
            body: format!(
                "ERR reload_failed\nmessage={}\nactive_set=previous\n\n",
                single_line(&error)
            ),
            stop_requested: false,
        },
    }
}

#[cfg(windows)]
pub use windows::{ControlListener, request};

#[cfg(windows)]
mod windows {
    use std::{
        ffi::{OsStr, c_void},
        io,
        os::windows::ffi::OsStrExt,
        ptr,
        sync::Arc,
        time::Instant,
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
        sync::{Semaphore, mpsc},
        time::timeout,
    };

    use super::{
        Command, EXCHANGE_TIMEOUT, MAX_CONCURRENT_EXCHANGES, MAX_REQUEST_BYTES, PIPE_SDDL,
        Response, execute, pipe_path,
    };
    use crate::config::RuntimeConfig;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            revision: u32,
            security_descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// Owns a converted security descriptor for as long as pipes are created
    /// with it. `LocalFree` is the documented release for the converted buffer.
    struct OwnedSecurityDescriptor(*mut c_void);

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: the pointer came from the conversion below and is freed
            // once, when this value is dropped.
            unsafe { LocalFree(self.0) };
        }
    }

    // SAFETY: the pointer is only read by `CreateNamedPipe` through a
    // `SECURITY_ATTRIBUTES` built from it, and is never mutated after
    // conversion.
    unsafe impl Send for OwnedSecurityDescriptor {}
    unsafe impl Sync for OwnedSecurityDescriptor {}

    fn security_descriptor() -> io::Result<OwnedSecurityDescriptor> {
        const SDDL_REVISION_1: u32 = 1;
        let sddl: Vec<u16> = OsStr::new(PIPE_SDDL)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and outlives the call; the output
        // pointer is written only on success.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedSecurityDescriptor(descriptor))
    }

    /// An accepted control endpoint and the stop requests it produces.
    pub struct ControlListener {
        path: String,
        /// Always `Some` between accepts; taken while a connected instance is
        /// handed to its handler and a replacement is created.
        server: Option<NamedPipeServer>,
        descriptor: OwnedSecurityDescriptor,
    }

    impl ControlListener {
        /// Creates the first pipe instance, which is what reserves the name.
        ///
        /// Binding before the server starts serving means a name already in
        /// use is reported at startup rather than discovered later.
        pub fn bind(name: &str) -> io::Result<Self> {
            let path = pipe_path(name);
            let descriptor = security_descriptor()?;
            let server = create_instance(&path, &descriptor, true)?;
            Ok(Self {
                path,
                server: Some(server),
                descriptor,
            })
        }

        pub fn path(&self) -> &str {
            &self.path
        }

        /// Serves control requests until the returned stop signal is used or
        /// this future is dropped.
        ///
        /// A `stop` command is reported through `stop_tx` after its response
        /// has been written, so the caller owns shutdown ordering.
        pub async fn serve(
            mut self,
            config: Arc<RuntimeConfig>,
            started: Instant,
            stop_tx: mpsc::Sender<()>,
        ) {
            let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_EXCHANGES));
            loop {
                let waiting = self
                    .server
                    .as_mut()
                    .expect("a control instance is always waiting between accepts");
                if let Err(error) = waiting.connect().await {
                    errln!("control endpoint accept failed: {error}");
                    return;
                }
                let connected = self
                    .server
                    .take()
                    .expect("the waiting instance is the connected one");
                match create_instance(&self.path, &self.descriptor, false) {
                    Ok(next) => self.server = Some(next),
                    Err(error) => {
                        // Without a further instance no client can connect
                        // again. Answer this one, then stop accepting rather
                        // than spin on the same failure.
                        errln!("control endpoint cannot be renewed: {error}");
                        handle(connected, &config, started, &stop_tx).await;
                        return;
                    }
                }
                let permit = match Arc::clone(&limit).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        // Over the ceiling: refuse this one instead of queueing
                        // work a client could keep adding to.
                        let mut connected = connected;
                        let _ = timeout(
                            EXCHANGE_TIMEOUT,
                            connected
                                .write_all(b"ERR busy\nmessage=too many control connections\n\n"),
                        )
                        .await;
                        continue;
                    }
                };
                let config = Arc::clone(&config);
                let stop_tx = stop_tx.clone();
                tokio::spawn(async move {
                    handle(connected, &config, started, &stop_tx).await;
                    drop(permit);
                });
            }
        }
    }

    fn create_instance(
        path: &str,
        descriptor: &OwnedSecurityDescriptor,
        first: bool,
    ) -> io::Result<NamedPipeServer> {
        let mut attributes = SecurityAttributes {
            length: u32::try_from(std::mem::size_of::<SecurityAttributes>()).unwrap_or(u32::MAX),
            security_descriptor: descriptor.0,
            inherit_handle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(MAX_CONCURRENT_EXCHANGES + 1);
        // SAFETY: the attributes and the descriptor they point at outlive this
        // call, and the descriptor is owned by the listener for as long as any
        // instance is created from it.
        unsafe {
            options
                .create_with_security_attributes_raw(path, (&raw mut attributes).cast::<c_void>())
        }
    }

    async fn handle(
        mut pipe: NamedPipeServer,
        config: &Arc<RuntimeConfig>,
        started: Instant,
        stop_tx: &mpsc::Sender<()>,
    ) {
        let exchange = timeout(EXCHANGE_TIMEOUT, async {
            let mut request = Vec::with_capacity(MAX_REQUEST_BYTES);
            let mut byte = [0_u8; 1];
            loop {
                match pipe.read(&mut byte).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if byte[0] == b'\n' {
                            break;
                        }
                        if request.len() == MAX_REQUEST_BYTES {
                            return Ok::<_, std::io::Error>(Some(Response::request_too_long()));
                        }
                        request.push(byte[0]);
                    }
                    Err(error) => return Err(error),
                }
            }
            let line = String::from_utf8_lossy(&request).into_owned();
            let response = match Command::parse(&line) {
                Some(command) => execute(command, config, started).await,
                None => Response::unknown_command(),
            };
            Ok(Some(response))
        })
        .await;

        let response = match exchange {
            Ok(Ok(Some(response))) => response,
            // A client that disconnects or stalls is not an event worth a line
            // in the log: it is remote behaviour the operator cannot act on.
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return,
        };
        let stop_requested = response.stop_requested;
        if timeout(EXCHANGE_TIMEOUT, pipe.write_all(response.body.as_bytes()))
            .await
            .is_err()
        {
            return;
        }
        let _ = pipe.flush().await;
        // The response is on the wire before shutdown begins, so a `stop`
        // client always sees its acknowledgement.
        let _ = pipe.shutdown().await;
        if stop_requested {
            let _ = stop_tx.send(()).await;
        }
    }

    /// Sends one command to a running server and returns its raw response.
    pub async fn request(name: &str, command: Command) -> io::Result<String> {
        let path = pipe_path(name);
        let mut pipe = ClientOptions::new().open(&path)?;
        timeout(EXCHANGE_TIMEOUT, async {
            pipe.write_all(format!("{}\n", command.name()).as_bytes())
                .await?;
            pipe.flush().await?;
            let mut response = String::new();
            pipe.read_to_string(&mut response).await?;
            Ok::<_, io::Error>(response)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control request timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_documented_command_and_rejects_others() {
        for command in Command::ALL {
            assert_eq!(Command::parse(command.name()), Some(command));
            // Operators type these by hand through a shell.
            assert_eq!(
                Command::parse(&format!(" {}\r\n", command.name())),
                Some(command)
            );
        }
        for unknown in ["", "sto", "stop now", "STATUS", "reload", "ping;status"] {
            assert_eq!(Command::parse(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn rejects_pipe_names_that_could_change_the_opened_object() {
        for name in ["", "a/b", r"a\b", "a b", "name!", &"x".repeat(65)] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        for name in ["default", "edge-1", "edge_1", "edge.1", &"x".repeat(64)] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn pipe_path_is_local_and_namespaced() {
        assert_eq!(
            pipe_path("default"),
            r"\\.\pipe\hamster-wheel-control-default"
        );
    }

    #[test]
    fn responses_end_with_a_blank_line_and_never_leak_a_newline_into_a_value() {
        let response = Response::error("reload_failed", "line one\nline two\rline three");
        assert!(response.body.starts_with("ERR reload_failed\n"));
        assert!(response.body.ends_with("\n\n"));
        assert_eq!(response.body.matches('\n').count(), 3);
        assert!(response.body.contains("line one line two line three"));
        assert!(!response.stop_requested);
    }

    #[test]
    fn unknown_command_names_the_commands_that_exist() {
        let response = Response::unknown_command();
        assert!(response.body.starts_with("ERR unknown_command\n"));
        for command in Command::ALL {
            assert!(response.body.contains(command.name()), "{}", command.name());
        }
    }
}
