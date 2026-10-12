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

/// The command semantics and their wire form.
///
/// This is the server half of the exchange, so it exists only where the
/// transport does. A target without the named pipe still needs `Command` and
/// `validate_name` above, because configuration and argument parsing use them
/// on every platform.
#[cfg(windows)]
mod protocol {
    use std::{
        fmt::Write as _,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::Command;
    use crate::{access_log::AccessLog, config::RuntimeConfig};

    /// Deadline for one control exchange, from connect to the written response.
    ///
    /// A client that connects and sends nothing must not hold a pipe instance, and
    /// must never delay shutdown.
    pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Longest request line accepted, including its newline.
    pub(crate) const MAX_REQUEST_BYTES: usize = 256;

    /// Control connections handled at once.
    ///
    /// The commands are administrative and rare. A small ceiling bounds the work a
    /// client with pipe access can queue without rejecting the one request an
    /// operator actually makes.
    pub(crate) const MAX_CONCURRENT_EXCHANGES: usize = 4;
    /// Refusals wait briefly for the peer to read their answer. Bound those
    /// waits separately so sustained overload cannot accumulate one task and
    /// pipe handle per client for the full exchange deadline.
    pub(crate) const MAX_CONCURRENT_REFUSALS: usize = 8;

    /// Security descriptor for the control pipe: LocalSystem and the owner only.
    pub(crate) const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)";

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
            // Every answer ends with a blank line, which is the frame a client
            // stops reading at. Detail blocks are built both with and without a
            // trailing newline, so the terminator is normalised here rather
            // than trusted from each caller.
            let mut body = format!("OK\n{}", details.trim_end_matches('\n'));
            body.push_str("\n\n");
            Self {
                body,
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
        draining: &AtomicBool,
        access_log: Option<&AccessLog>,
    ) -> Response {
        match command {
            Command::Ping => Response::ok(format!("pid={}", std::process::id())),
            Command::Status => Response::ok(status_details(config, started, draining, access_log)),
            Command::Stop => Response {
                // Answered before the shutdown is triggered, so the client learns
                // the request was accepted even though the pipe is about to close.
                body: format!("OK\npid={}\nstopping=true\n\n", std::process::id()),
                stop_requested: true,
            },
            Command::ReloadCertificates => reload_certificates(config).await,
        }
    }

    fn status_details(
        config: &Arc<RuntimeConfig>,
        started: Instant,
        draining: &AtomicBool,
        access_log: Option<&AccessLog>,
    ) -> String {
        let mut details = String::new();
        let _ = writeln!(details, "pid={}", std::process::id());
        let _ = writeln!(
            details,
            "uptime_seconds={:.3}",
            started.elapsed().as_secs_f64()
        );
        // The endpoint keeps answering while connections drain, so without
        // this a health poll during a stop would be told the server is still
        // serving. A supervisor that restarts on an unhealthy answer must be
        // able to tell "stopping on purpose" from "broken".
        let _ = writeln!(
            details,
            "state={}",
            if draining.load(Ordering::Acquire) {
                "draining"
            } else {
                "serving"
            }
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
        match crate::metrics::readiness(config, draining) {
            Ok(()) => {
                let _ = writeln!(details, "ready=true");
            }
            // The same answer the administrative `/ready` gives, for an
            // operator who has a pipe but no HTTP client.
            Err(reason) => {
                let _ = writeln!(details, "ready=false reason={}", single_line(&reason));
            }
        }
        if let Some(admin) = config.admin() {
            let _ = writeln!(details, "admin=http://{}", admin.listen());
        }
        if let Some(settings) = config.access_log() {
            // A log an operator cannot see the health of is a log they have to
            // trust. `dropped` is the only honest answer to "is this log
            // complete?", so it is reported beside where the records go.
            let (queued, written, dropped, failed) = access_log.map_or((0, 0, 0, 0), |log| {
                (log.queued(), log.written(), log.dropped(), log.failed())
            });
            let _ = writeln!(
                details,
                "access_log={} queued={queued} written={written} dropped={dropped} failed={failed}",
                single_line(&settings.target_description())
            );
        }
        let _ = writeln!(
            details,
            "max_connections={}",
            config.http_limits().max_connections()
        );
        for pool in config.php_workers() {
            let snapshot = pool.snapshot();
            // The exact worker PIDs come from the same snapshot as the
            // counts, so they cannot disagree with them. An operator tracking
            // or reaping a worker tree must not have to match process names.
            let pids = snapshot
                .workers
                .iter()
                .filter_map(|worker| worker.pid)
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(
                details,
                "php_pool={} target={} live={} ready={} pids={}",
                single_line(pool.label()),
                snapshot.target_workers,
                snapshot.live_workers,
                snapshot.ready_workers,
                pids
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

    #[cfg(test)]
    mod tests {
        use super::*;

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
        fn an_ok_answer_is_framed_whether_or_not_its_details_end_with_a_newline() {
            // A client stops reading at the blank line, so an answer without
            // one leaves it waiting for the server to close while the server
            // waits for the client: both would only be freed by a deadline.
            for details in ["pid=1", "pid=1\nlistener=http://127.0.0.1:1\n"] {
                let response = Response::ok(details.to_owned());
                assert!(response.body.starts_with("OK\n"), "{details:?}");
                assert!(response.body.ends_with("\n\n"), "{details:?}");
                assert!(!response.body.ends_with("\n\n\n"), "{details:?}");
                assert!(!response.stop_requested);
            }
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
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
        sync::{Semaphore, mpsc, oneshot},
        task::JoinSet,
        time::timeout_at,
    };

    use super::{
        Command,
        protocol::{
            EXCHANGE_TIMEOUT, MAX_CONCURRENT_EXCHANGES, MAX_CONCURRENT_REFUSALS, MAX_REQUEST_BYTES,
            PIPE_SDDL, Response, execute, pipe_path,
        },
    };
    use crate::{access_log::AccessLog, config::RuntimeConfig};

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
            draining: Arc<AtomicBool>,
            access_log: Option<Arc<AccessLog>>,
            stop_tx: mpsc::Sender<()>,
            mut shutdown: oneshot::Receiver<()>,
        ) {
            let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_EXCHANGES));
            let refusal_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_REFUSALS));
            // Tasks belong to this listener, including refusal waits. Reap
            // completed tasks on each accept and abort/join the rest on stop.
            let mut exchanges = JoinSet::new();
            let mut renewal_reported = false;
            loop {
                while let Some(result) = exchanges.try_join_next() {
                    if let Err(error) = result {
                        errln!("control exchange task failed: {error}");
                    }
                }
                // The waiting instance is created here rather than right after
                // an accept, so a creation that cannot succeed yet is retried
                // instead of ending the endpoint for the rest of this process.
                // Windows refuses it while every instance is occupied, which a
                // burst of control connections reaches legitimately.
                if self.server.is_none() {
                    match create_instance(&self.path, &self.descriptor, false) {
                        Ok(next) => {
                            self.server = Some(next);
                            renewal_reported = false;
                        }
                        Err(error) => {
                            if !renewal_reported {
                                errln!("control endpoint cannot accept for now: {error}");
                                renewal_reported = true;
                            }
                            tokio::select! {
                                _ = &mut shutdown => break,
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                            continue;
                        }
                    }
                }
                let waiting = self
                    .server
                    .as_mut()
                    .expect("an instance was just ensured above");
                let accepted = tokio::select! {
                    _ = &mut shutdown => break,
                    accepted = waiting.connect() => accepted,
                };
                if let Err(error) = accepted {
                    // ERROR_NO_DATA means the client connected and closed
                    // before ConnectNamedPipe completed. Replace that instance
                    // rather than losing the endpoint for the process lifetime.
                    if error.raw_os_error() != Some(232) {
                        errln!("control endpoint accept failed: {error}");
                        break;
                    }
                    self.server.take();
                    continue;
                }
                let connected = self
                    .server
                    .take()
                    .expect("the waiting instance is the connected one");
                // Restore the waiting instance before doing anything with the
                // connection, so a client that arrives during the handoff is
                // not refused by Windows with a bare `ERROR_PIPE_BUSY`. A
                // failure here is not fatal: the top of the loop retries.
                if let Ok(next) = create_instance(&self.path, &self.descriptor, false) {
                    self.server = Some(next);
                    renewal_reported = false;
                }
                let permit = match Arc::clone(&limit).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        // Over the ceiling: refuse this one instead of queueing
                        // work a client could keep adding to. The refusal is
                        // written off the accept loop, because a client that
                        // never reads it would otherwise hold every later
                        // connection behind its own write deadline. Once the
                        // refusal budget is full, close immediately: a bare
                        // pipe error is preferable to unbounded server tasks.
                        if let Ok(permit) = Arc::clone(&refusal_limit).try_acquire_owned() {
                            exchanges.spawn(async move {
                                refuse(connected).await;
                                drop(permit);
                            });
                        }
                        continue;
                    }
                };
                let config = Arc::clone(&config);
                let draining = Arc::clone(&draining);
                let access_log = access_log.clone();
                let stop_tx = stop_tx.clone();
                exchanges.spawn(async move {
                    handle(
                        connected,
                        &config,
                        started,
                        &draining,
                        access_log.as_deref(),
                        &stop_tx,
                    )
                    .await;
                    drop(permit);
                });
            }
            // Close admission first; every remaining task is administrative
            // pipe work, not an application transaction. No exchange survives
            // the listener or retains a RuntimeConfig after it returns.
            self.server.take();
            exchanges.abort_all();
            while exchanges.join_next().await.is_some() {}
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
            // Exchange and refusal semaphores bound live tasks/handles. Keep
            // the OS instance ceiling above both so a short burst can receive
            // an explicit refusal before the fallback pipe error.
            .max_instances(254);
        // SAFETY: the attributes and the descriptor they point at outlive this
        // call, and the descriptor is owned by the listener for as long as any
        // instance is created from it.
        unsafe {
            options
                .create_with_security_attributes_raw(path, (&raw mut attributes).cast::<c_void>())
        }
    }

    /// Answers one over-the-ceiling connection and releases its instance.
    ///
    /// The refusal is flushed and the pipe is shut down before the handle is
    /// dropped: closing a named pipe with unread bytes in it can discard them,
    /// which would leave the client at end of stream with nothing to report.
    async fn refuse(mut pipe: NamedPipeServer) {
        const REFUSAL: &[u8] = b"ERR busy\nmessage=too many control connections\n\n";
        let deadline = tokio::time::Instant::now() + EXCHANGE_TIMEOUT;
        if !matches!(
            timeout_at(deadline, pipe.write_all(REFUSAL)).await,
            Ok(Ok(()))
        ) {
            return;
        }
        await_peer_close(pipe, deadline).await;
    }

    /// Reads one answer and stops at its terminating blank line.
    ///
    /// A client that read to end of stream would wait for the server to close
    /// while the server waits for the client to close, so each side would only
    /// be released by its deadline. The blank line that ends every answer is
    /// the frame, so the client can stop reading and close first.
    async fn read_framed(
        pipe: &mut tokio::net::windows::named_pipe::NamedPipeClient,
    ) -> io::Result<String> {
        const MAX_RESPONSE_BYTES: usize = 64 * 1024;
        let mut response = Vec::new();
        let mut chunk = [0_u8; 512];
        loop {
            let read = pipe.read(&mut chunk).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control endpoint closed before a complete answer",
                ));
            }
            response.extend_from_slice(&chunk[..read]);
            if response.len() > MAX_RESPONSE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control answer exceeded its size limit",
                ));
            }
            if response.ends_with(b"\n\n") {
                break;
            }
        }
        let response = String::from_utf8(response).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the control endpoint answered with invalid UTF-8",
            )
        })?;
        let status = response.lines().next().unwrap_or_default();
        if status != "OK"
            && !status.strip_prefix("ERR ").is_some_and(|code| {
                !code.is_empty()
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control answer has an invalid status line",
            ));
        }
        Ok(response)
    }

    /// Waits for the client to close before this end does.
    ///
    /// Windows can discard bytes a client has not read yet when the server
    /// handle closes first, which would turn an answered request into a bare
    /// end of stream. A client reads the whole answer and then closes, so its
    /// end of stream is the proof that the answer arrived. The wait is bounded
    /// because a client that never closes must not hold a pipe instance.
    async fn await_peer_close(mut pipe: NamedPipeServer, deadline: tokio::time::Instant) {
        let mut sink = [0_u8; 256];
        let _ = timeout_at(deadline, async {
            while let Ok(count) = pipe.read(&mut sink).await {
                if count == 0 {
                    break;
                }
            }
        })
        .await;
    }

    async fn handle(
        mut pipe: NamedPipeServer,
        config: &Arc<RuntimeConfig>,
        started: Instant,
        draining: &AtomicBool,
        access_log: Option<&AccessLog>,
        stop_tx: &mpsc::Sender<()>,
    ) {
        let deadline = tokio::time::Instant::now() + EXCHANGE_TIMEOUT;
        let exchange = timeout_at(deadline, async {
            let mut request = Vec::with_capacity(MAX_REQUEST_BYTES);
            let mut byte = [0_u8; 1];
            loop {
                match pipe.read(&mut byte).await {
                    // EOF does not complete a request line. A client that
                    // cancels before its newline must not execute a command.
                    Ok(0) => return Ok(None),
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
                Some(command) => execute(command, config, started, draining, access_log).await,
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
        if matches!(
            timeout_at(deadline, pipe.write_all(response.body.as_bytes())).await,
            Ok(Ok(()))
        ) {
            await_peer_close(pipe, deadline).await;
        }
        // A complete stop was accepted even if its client vanished before
        // reading the acknowledgement. Notification is idempotent and bounded.
        if stop_requested {
            let _ = stop_tx.try_send(());
        }
    }

    /// Sends one command to a running server and returns its raw response.
    ///
    /// Every pipe instance can be occupied while other control connections are
    /// in flight, and Windows then refuses the open with `ERROR_PIPE_BUSY`
    /// rather than queueing it. An operator asked for one command, so this
    /// waits for an instance within the same deadline the server answers in
    /// instead of reporting a raw OS error.
    pub async fn request(name: &str, command: Command) -> io::Result<String> {
        const ERROR_PIPE_BUSY: i32 = 231;
        let path = pipe_path(name);
        let deadline = tokio::time::Instant::now() + EXCHANGE_TIMEOUT;
        loop {
            let mut pipe = loop {
                match ClientOptions::new().open(&path) {
                    Ok(pipe) => break pipe,
                    Err(error)
                        if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let response = timeout_at(deadline, async {
                pipe.write_all(format!("{}\n", command.name()).as_bytes())
                    .await?;
                pipe.flush().await?;
                let response = read_framed(&mut pipe).await?;
                Ok::<_, io::Error>(response)
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control request timed out"))??;
            // A refusal means other control connections hold every slot, not
            // that the command was wrong. Retrying inside the same budget is
            // what makes a burst of connections survivable for the operator.
            if response.lines().next() == Some("ERR busy") && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            return Ok(response);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::time::timeout;

        fn endpoint_name(tag: &str) -> String {
            format!(
                "audit-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )
        }

        fn test_config() -> Arc<RuntimeConfig> {
            Arc::new(crate::config::compile_str(
                "[server]\nlisten = '127.0.0.1:8080'\n[[hosts]]\nname = 'localhost'\nlocations = [{path = '/', handler = 'not_found'}]",
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
            ).unwrap())
        }

        #[tokio::test]
        async fn disconnect_before_accept_preserves_the_endpoint() {
            let name = endpoint_name("early-close");
            let listener = ControlListener::bind(&name).unwrap();
            drop(ClientOptions::new().open(pipe_path(&name)).unwrap());
            let (stop_tx, _stop_rx) = mpsc::channel(1);
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(listener.serve(
                test_config(),
                Instant::now(),
                Arc::new(AtomicBool::new(false)),
                None,
                stop_tx,
                shutdown_rx,
            ));
            let answer = request(&name, Command::Ping).await;
            let _ = shutdown_tx.send(());
            task.await.unwrap();
            assert!(answer.unwrap().starts_with("OK\n"));
        }

        #[tokio::test]
        async fn a_partial_stop_at_eof_is_not_executed() {
            let name = endpoint_name("partial-eof");
            let listener = ControlListener::bind(&name).unwrap();
            let (stop_tx, mut stop_rx) = mpsc::channel(1);
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(listener.serve(
                test_config(),
                Instant::now(),
                Arc::new(AtomicBool::new(false)),
                None,
                stop_tx,
                shutdown_rx,
            ));
            let mut client = ClientOptions::new().open(pipe_path(&name)).unwrap();
            client.write_all(b"stop").await.unwrap();
            drop(client);
            assert!(
                request(&name, Command::Ping)
                    .await
                    .unwrap()
                    .starts_with("OK\n")
            );
            assert!(
                stop_rx.try_recv().is_err(),
                "an incomplete command was executed"
            );
            let _ = shutdown_tx.send(());
            task.await.unwrap();
        }

        #[tokio::test]
        async fn explicit_shutdown_joins_exchanges_and_releases_the_config() {
            let name = endpoint_name("join");
            let listener = ControlListener::bind(&name).unwrap();
            let config = test_config();
            let (stop_tx, _stop_rx) = mpsc::channel(1);
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(listener.serve(
                Arc::clone(&config),
                Instant::now(),
                Arc::new(AtomicBool::new(false)),
                None,
                stop_tx,
                shutdown_rx,
            ));
            let mut client = ClientOptions::new().open(pipe_path(&name)).unwrap();
            timeout(Duration::from_secs(1), async {
                while Arc::strong_count(&config) < 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            shutdown_tx.send(()).unwrap();
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Arc::strong_count(&config), 1);
            let mut byte = [0; 1];
            assert!(!matches!(client.read(&mut byte).await, Ok(count) if count > 0));
        }

        #[tokio::test]
        async fn exchange_uses_one_deadline_including_waiting_for_peer_close() {
            let name = endpoint_name("deadline");
            let listener = ControlListener::bind(&name).unwrap();
            let config = test_config();
            let (stop_tx, _stop_rx) = mpsc::channel(1);
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(listener.serve(
                Arc::clone(&config),
                Instant::now(),
                Arc::new(AtomicBool::new(false)),
                None,
                stop_tx,
                shutdown_rx,
            ));
            let mut client = ClientOptions::new().open(pipe_path(&name)).unwrap();
            timeout(Duration::from_secs(1), async {
                while Arc::strong_count(&config) < 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let started = Instant::now();
            tokio::time::sleep(Duration::from_secs(2)).await;
            client.write_all(b"ping\n").await.unwrap();
            assert!(read_framed(&mut client).await.unwrap().starts_with("OK\n"));
            // Sending another byte must not release the slot prematurely.
            client.write_all(b"x").await.unwrap();
            let mut byte = [0; 1];
            let closed = timeout(Duration::from_secs(4), client.read(&mut byte)).await;
            let _ = shutdown_tx.send(());
            task.await.unwrap();
            assert!(
                closed.is_ok(),
                "each phase incorrectly received a fresh timeout budget"
            );
            assert!(!matches!(closed.unwrap(), Ok(count) if count > 0));
            assert!(
                started.elapsed() >= Duration::from_millis(4500),
                "extra data was mistaken for peer close"
            );
        }

        #[tokio::test]
        async fn aborting_the_listener_releases_its_inflight_clients() {
            let name = endpoint_name("ownership");
            let listener = ControlListener::bind(&name).unwrap();
            let config = Arc::new(crate::config::compile_str(
                "[server]\nlisten = '127.0.0.1:8080'\n[[hosts]]\nname = 'localhost'\nlocations = [{path = '/', handler = 'not_found'}]",
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
            ).unwrap());
            let (stop_tx, _stop_rx) = mpsc::channel(1);
            let (_shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(listener.serve(
                Arc::clone(&config),
                Instant::now(),
                Arc::new(AtomicBool::new(false)),
                None,
                stop_tx,
                shutdown_rx,
            ));
            let mut client = ClientOptions::new().open(pipe_path(&name)).unwrap();
            timeout(Duration::from_secs(1), async {
                while Arc::strong_count(&config) < 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            task.abort();
            let _ = task.await;
            let mut byte = [0_u8; 1];
            let closed = timeout(Duration::from_millis(500), client.read(&mut byte)).await;
            assert!(
                closed.is_ok(),
                "a detached exchange kept its pipe after the listener exited"
            );
            assert!(!matches!(closed.unwrap(), Ok(count) if count > 0));
        }

        #[tokio::test]
        async fn a_framed_answer_with_an_invalid_status_is_not_success() {
            let name = endpoint_name("status-line");
            let mut pipe = ServerOptions::new().create(pipe_path(&name)).unwrap();
            let peer = tokio::spawn(async move {
                pipe.connect().await.unwrap();
                let mut byte = [0_u8; 1];
                while pipe.read(&mut byte).await.unwrap() > 0 && byte[0] != b'\n' {}
                pipe.write_all(b"NOT_OK\n\n").await.unwrap();
                let _ = pipe.read(&mut byte).await;
            });
            let response = request(&name, Command::Ping).await;
            peer.await.unwrap();
            assert!(
                response.is_err(),
                "an invalid status was treated as success: {response:?}"
            );
        }
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
}
