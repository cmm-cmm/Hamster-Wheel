use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fmt, fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::Deserialize;

use crate::access::AccessPolicy;
use crate::body_budget::BodyBudget;
use crate::fastcgi::{
    DEFAULT_CONNECT_TIMEOUT as FASTCGI_DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_INDEX as FASTCGI_DEFAULT_INDEX,
    DEFAULT_RESPONSE_TIMEOUT as FASTCGI_DEFAULT_RESPONSE_TIMEOUT, FastCgiBackend, FastCgiConfig,
    MAX_CONCURRENCY as FASTCGI_MAX_CONCURRENCY, MAX_SERVERS as FASTCGI_MAX_SERVERS,
};
use crate::php_worker::{
    DEFAULT_REQUEST_TIMEOUT, DEFAULT_STARTUP_TIMEOUT, MAX_QUEUE_CAPACITY, MAX_REQUEST_BODY,
    MAX_REQUEST_TIMEOUT, MAX_STARTUP_TIMEOUT, MAX_WORKERS, MIN_REQUEST_TIMEOUT,
    MIN_STARTUP_TIMEOUT, PhpWorkerConfig, PhpWorkerError, PhpWorkerPool, PhpWorkerPoolConfig,
    WorkerStart,
};
use crate::proxy::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_IDLE, DEFAULT_PROGRESS_TIMEOUT,
    DEFAULT_RESPONSE_HEADER_TIMEOUT, MAX_IDLE_LIMIT, ProxyConfig, Upstream,
};
use crate::router::{
    Handler, HostRouter, LocationRouter, RouterBuildError, VirtualHost, normalize_location,
    valid_hostname,
};
use crate::tls::{
    CertificateSource, DEFAULT_HANDSHAKE_TIMEOUT, MAX_HANDSHAKE_TIMEOUT, TlsListener, TlsName,
    TlsSource,
};

pub const SERVER_NAME: &str = concat!("HamsterWheel/", env!("CARGO_PKG_VERSION"));

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_DOCUMENT_ROOT: &str = "public";
const DEFAULT_MAX_CONNECTIONS: usize = 1_024;
const MAX_CONNECTIONS: usize = 65_536;
const DEFAULT_HEADER_READ_TIMEOUT_SECONDS: u64 = 30;
const MAX_HEADER_READ_TIMEOUT_SECONDS: u64 = 3_600;
const DEFAULT_BODY_READ_TIMEOUT_SECONDS: u64 = 30;
const MAX_BODY_READ_TIMEOUT_SECONDS: u64 = 3_600;
const DEFAULT_WRITE_PROGRESS_TIMEOUT_SECONDS: u64 = 30;
const MAX_WRITE_PROGRESS_TIMEOUT_SECONDS: u64 = 3_600;
/// Request-body bytes Hamster may have buffered process-wide at one time.
///
/// Zero disables the budget and restores the pre-budget path exactly, where
/// the only aggregate bound is `max_connections * MAX_REQUEST_BODY`.
const DEFAULT_MAX_BODY_MEMORY: u64 = 64 * 1_024 * 1_024;
/// Anything below one maximum body would make some legal requests impossible
/// no matter how idle the server is, so the floor is that body limit.
const MIN_MAX_BODY_MEMORY: u64 = MAX_REQUEST_BODY as u64;
const MAX_MAX_BODY_MEMORY: u64 = 64 * 1_024 * 1_024 * 1_024;
const MIN_HTTP1_MAX_BUFFER_SIZE: usize = 8 * 1_024;
const DEFAULT_HTTP1_MAX_BUFFER_SIZE: usize = 64 * 1_024;
const MAX_HTTP1_MAX_BUFFER_SIZE: usize = 1024 * 1_024;
const MAX_PROXY_TIMEOUT_SECONDS: u64 = 3_600;

#[cfg(feature = "profiling")]
pub fn profiling_document_root() -> PathBuf {
    PathBuf::from(DEFAULT_DOCUMENT_ROOT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    server: RawServer,
    hosts: Vec<RawHost>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    /// The cleartext HTTP listener. Optional only when `tls` is configured.
    listen: Option<String>,
    tls: Option<RawTls>,
    #[serde(default = "default_max_connections")]
    max_connections: usize,
    #[serde(default = "default_header_read_timeout")]
    header_read_timeout: u64,
    #[serde(default = "default_body_read_timeout")]
    body_read_timeout: u64,
    #[serde(default = "default_write_progress_timeout")]
    write_progress_timeout: u64,
    #[serde(default = "default_max_body_memory")]
    max_body_memory: u64,
    #[serde(default = "default_http1_max_buffer_size")]
    http1_max_buffer_size: usize,
    control: Option<RawControl>,
    access_log: Option<RawAccessLog>,
    admin: Option<RawAdmin>,
}

/// The administrative listener. Absent means health, readiness and metrics are
/// not served at all, which is the default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdmin {
    listen: String,
}

/// Per-request logging. Absent means no queue, no writer and no per-request
/// work: a server that was not asked to log does not pay for logging.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccessLog {
    /// Log file, resolved beside this configuration file. Absent means stdout.
    path: Option<String>,
    /// Size at which the file is rotated. Absent or 0 means never.
    rotate_bytes: Option<u64>,
    /// Archives kept beside the active file.
    keep: Option<usize>,
    /// Query strings are omitted by default because they carry credentials in
    /// real deployments. See `src/access_log.rs` for what is replaced.
    #[serde(default)]
    include_query: bool,
    #[serde(default = "default_access_log_queue_capacity")]
    queue_capacity: usize,
}

const fn default_access_log_queue_capacity() -> usize {
    crate::access_log::DEFAULT_QUEUE_CAPACITY
}

/// The local control endpoint. Absent means no endpoint exists at all.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControl {
    name: String,
}

/// Just enough of a configuration file to find the control endpoint.
///
/// A control client must not compile the configuration: loading certificates,
/// canonicalizing roots and building pools can fail for reasons that have
/// nothing to do with the request, and a certificate that no longer loads is
/// exactly when `reload-certificates` is needed. Unknown fields are ignored
/// here on purpose, so a file the running server accepted stays readable.
#[derive(Debug, Deserialize)]
struct ControlProbe {
    server: ControlProbeServer,
}

#[derive(Debug, Deserialize)]
struct ControlProbeServer {
    control: Option<RawControl>,
}

/// Reads the configured control endpoint name without compiling anything else.
pub fn control_endpoint(selection: &ConfigSelection) -> Result<Option<String>, ConfigError> {
    let ConfigSelection::File(path) = selection else {
        // The built-in configuration has no control endpoint to name.
        return Ok(None);
    };
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        context: format!("cannot read config file {}", path.display()),
        source,
    })?;
    let probe: ControlProbe = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        source_name: path.display().to_string(),
        source,
    })?;
    match probe.server.control {
        None => Ok(None),
        Some(control) => {
            crate::control::validate_name(&control.name).map_err(ConfigError::Validation)?;
            Ok(Some(control.name))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    listen: String,
    handshake_timeout: Option<u64>,
    certificates: Vec<RawCertificate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCertificate {
    cert: PathBuf,
    key: PathBuf,
    names: Vec<String>,
    #[serde(default)]
    default: bool,
}

const fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}

const fn default_header_read_timeout() -> u64 {
    DEFAULT_HEADER_READ_TIMEOUT_SECONDS
}

const fn default_body_read_timeout() -> u64 {
    DEFAULT_BODY_READ_TIMEOUT_SECONDS
}

const fn default_write_progress_timeout() -> u64 {
    DEFAULT_WRITE_PROGRESS_TIMEOUT_SECONDS
}

const fn default_max_body_memory() -> u64 {
    DEFAULT_MAX_BODY_MEMORY
}

const fn default_http1_max_buffer_size() -> usize {
    DEFAULT_HTTP1_MAX_BUFFER_SIZE
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    deny_paths: Vec<String>,
    #[serde(default)]
    deny_names: Vec<String>,
    locations: Vec<RawLocation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocation {
    path: String,
    handler: String,
    root: Option<PathBuf>,
    php: Option<RawPhpWorker>,
    proxy: Option<RawProxy>,
    fastcgi: Option<RawFastCgi>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFastCgi {
    servers: Vec<String>,
    concurrency: Option<usize>,
    index: Option<String>,
    connect_timeout: Option<u64>,
    response_timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProxy {
    upstream: String,
    connect_timeout: Option<u64>,
    response_header_timeout: Option<u64>,
    progress_timeout: Option<u64>,
    idle_timeout: Option<u64>,
    max_idle: Option<usize>,
    preserve_host: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhpWorker {
    executable: PathBuf,
    script: PathBuf,
    #[serde(default = "default_php_workers")]
    workers: usize,
    queue_capacity: Option<usize>,
    request_timeout: Option<u64>,
    startup_timeout: Option<u64>,
    max_requests: Option<u64>,
    max_worker_age: Option<u64>,
    /// Extra environment for the worker process. The server never interprets
    /// these; they let a worker script be told where its application lives
    /// without the server needing to know what kind of application it is.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

const fn default_php_workers() -> usize {
    1
}

pub struct RuntimeConfig {
    /// Which compilation of the configuration this is, counting from 1.
    ///
    /// A reload publishes the next generation; everything that reports about a
    /// request or about the server names the generation it belongs to, so two
    /// reports can be placed in the same configuration without guessing.
    generation: u64,
    /// Where this generation was compiled from, so a reload can read it again.
    source: ConfigSelection,
    /// What a reload may not change in place, and how carried resources are
    /// recognised. See `ReloadIdentity`.
    identity: ReloadIdentity,
    listen: Option<SocketAddr>,
    tls: Option<Arc<TlsListener>>,
    http_limits: HttpLimits,
    control: Option<String>,
    access_log: Option<crate::access_log::Settings>,
    admin: Option<crate::admin::Settings>,
    router: Arc<HostRouter>,
    php_pools: Vec<Arc<PhpWorkerPool>>,
    upstreams: Vec<Arc<Upstream>>,
    fastcgi_backends: Vec<Arc<FastCgiBackend>>,
}

#[derive(Debug)]
pub struct PhpPoolFailure {
    label: Arc<str>,
    source: PhpWorkerError,
}

impl PhpPoolFailure {
    fn new(pool: &PhpWorkerPool, source: PhpWorkerError) -> Self {
        Self {
            label: Arc::from(pool.label()),
            source,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub const fn error(&self) -> &PhpWorkerError {
        &self.source
    }
}

impl fmt::Display for PhpPoolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "PHP worker pool {:?}: {}",
            self.label, self.source
        )
    }
}

impl std::error::Error for PhpPoolFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// One primary pool failure plus failures encountered while converging the
/// remaining pools. The primary error always stays first and remains the
/// error source; cleanup evidence must never replace the initiating failure.
#[derive(Debug)]
pub struct PhpPoolsError {
    primary: PhpPoolFailure,
    additional: Vec<PhpPoolFailure>,
}

impl PhpPoolsError {
    fn new(primary: PhpPoolFailure, additional: Vec<PhpPoolFailure>) -> Self {
        Self {
            primary,
            additional,
        }
    }

    pub const fn primary(&self) -> &PhpPoolFailure {
        &self.primary
    }

    pub fn additional(&self) -> &[PhpPoolFailure] {
        &self.additional
    }

    pub(crate) fn append(&mut self, mut other: Self) {
        self.additional.push(other.primary);
        self.additional.append(&mut other.additional);
    }
}

impl fmt::Display for PhpPoolsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.primary)?;
        for failure in &self.additional {
            write!(formatter, "; additional cleanup failure: {failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PhpPoolsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    max_connections: usize,
    header_read_timeout: Duration,
    body_read_timeout: Duration,
    write_progress_timeout: Duration,
    max_body_memory: usize,
    http1_max_buffer_size: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            header_read_timeout: Duration::from_secs(DEFAULT_HEADER_READ_TIMEOUT_SECONDS),
            body_read_timeout: Duration::from_secs(DEFAULT_BODY_READ_TIMEOUT_SECONDS),
            write_progress_timeout: Duration::from_secs(DEFAULT_WRITE_PROGRESS_TIMEOUT_SECONDS),
            max_body_memory: DEFAULT_MAX_BODY_MEMORY as usize,
            http1_max_buffer_size: DEFAULT_HTTP1_MAX_BUFFER_SIZE,
        }
    }
}

impl HttpLimits {
    pub const fn max_connections(self) -> usize {
        self.max_connections
    }

    pub const fn header_read_timeout(self) -> Duration {
        self.header_read_timeout
    }

    pub const fn body_read_timeout(self) -> Duration {
        self.body_read_timeout
    }

    pub const fn write_progress_timeout(self) -> Duration {
        self.write_progress_timeout
    }

    /// Zero means the budget is disabled.
    pub const fn max_body_memory(self) -> usize {
        self.max_body_memory
    }

    pub const fn http1_max_buffer_size(self) -> usize {
        self.http1_max_buffer_size
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        max_connections: usize,
        header_read_timeout: Duration,
        body_read_timeout: Duration,
        http1_max_buffer_size: usize,
    ) -> Self {
        Self {
            max_connections,
            header_read_timeout,
            body_read_timeout,
            write_progress_timeout: Duration::from_secs(DEFAULT_WRITE_PROGRESS_TIMEOUT_SECONDS),
            max_body_memory: DEFAULT_MAX_BODY_MEMORY as usize,
            http1_max_buffer_size,
        }
    }

    #[cfg(test)]
    pub(crate) const fn with_write_progress_timeout(mut self, timeout: Duration) -> Self {
        self.write_progress_timeout = timeout;
        self
    }
}

impl RuntimeConfig {
    /// The cleartext listener, when one is configured.
    pub const fn listen_address(&self) -> Option<SocketAddr> {
        self.listen
    }

    /// The configured control endpoint name, when an operator asked for one.
    pub fn control_name(&self) -> Option<&str> {
        self.control.as_deref()
    }

    /// The compiled per-request log settings, when logging was configured.
    pub fn access_log(&self) -> Option<&crate::access_log::Settings> {
        self.access_log.as_ref()
    }

    /// The administrative listener's settings, when one was configured.
    pub fn admin(&self) -> Option<crate::admin::Settings> {
        self.admin
    }

    /// Which compilation of the configuration is serving.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The configuration this generation was compiled from.
    pub fn source(&self) -> &ConfigSelection {
        &self.source
    }

    fn php_worker_pool(&self, label: &str) -> Option<Arc<PhpWorkerPool>> {
        self.php_pools
            .iter()
            .find(|pool| pool.label() == label)
            .map(Arc::clone)
    }

    fn upstream(&self, label: &str) -> Option<Arc<Upstream>> {
        self.upstreams
            .iter()
            .find(|upstream| upstream.label() == label)
            .map(Arc::clone)
    }

    fn fastcgi_backend(&self, label: &str) -> Option<Arc<FastCgiBackend>> {
        self.fastcgi_backends
            .iter()
            .find(|backend| backend.label() == label)
            .map(Arc::clone)
    }

    /// The HTTPS listener, with its certificates already loaded and checked.
    pub fn tls(&self) -> Option<&Arc<TlsListener>> {
        self.tls.as_ref()
    }

    pub fn router(&self) -> &HostRouter {
        self.router.as_ref()
    }

    pub fn router_arc(&self) -> Arc<HostRouter> {
        Arc::clone(&self.router)
    }

    pub const fn http_limits(&self) -> HttpLimits {
        self.http_limits
    }

    pub async fn start_workers(&self) -> Result<Vec<WorkerStart>, PhpPoolsError> {
        #[cfg(windows)]
        if let Some(first_pool) = self.php_pools.first()
            && let Err(error) = crate::windows_child_containment::establish()
        {
            return Err(PhpPoolsError::new(
                PhpPoolFailure::new(
                    first_pool,
                    PhpWorkerError::Startup(format!(
                        "Windows PHP child containment could not be established: {error}"
                    )),
                ),
                Vec::new(),
            ));
        }

        let mut starts = Vec::new();
        for (index, pool) in self.php_pools.iter().enumerate() {
            match pool.start().await {
                Ok(pool_starts) => starts.extend(pool_starts),
                Err(error) => {
                    let primary = PhpPoolFailure::new(pool, error);
                    let mut cleanup_failures = Vec::new();
                    // Roll acquisition back in reverse order. Every earlier
                    // pool is attempted even when one cleanup reports an
                    // error, because each pool owns independent children.
                    for started_pool in self.php_pools[..index].iter().rev() {
                        if let Err(error) = started_pool.shutdown().await {
                            cleanup_failures.push(PhpPoolFailure::new(started_pool, error));
                        }
                    }
                    return Err(PhpPoolsError::new(primary, cleanup_failures));
                }
            }
        }
        Ok(starts)
    }

    pub async fn shutdown_workers(&self) -> Result<(), PhpPoolsError> {
        let mut failures = Vec::new();
        for pool in &self.php_pools {
            if let Err(error) = pool.shutdown().await {
                failures.push(PhpPoolFailure::new(pool, error));
            }
        }
        let mut failures = failures.into_iter();
        match failures.next() {
            Some(primary) => Err(PhpPoolsError::new(primary, failures.collect())),
            None => Ok(()),
        }
    }

    pub fn php_worker(&self) -> Option<&Arc<PhpWorkerPool>> {
        self.php_pools.first()
    }

    /// Every reverse-proxy upstream, in configuration order.
    pub fn upstreams(&self) -> &[Arc<Upstream>] {
        &self.upstreams
    }

    /// Every FastCGI location's backend, in configuration order.
    pub fn fastcgi_backends(&self) -> &[Arc<FastCgiBackend>] {
        &self.fastcgi_backends
    }

    pub fn php_workers(&self) -> &[Arc<PhpWorkerPool>] {
        &self.php_pools
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSelection {
    BuiltIn,
    File(PathBuf),
}

#[derive(Debug)]
pub enum ConfigError {
    Arguments(String),
    Io {
        context: String,
        source: io::Error,
    },
    Parse {
        source_name: String,
        source: toml::de::Error,
    },
    Validation(String),
    Compile(String),
    /// A valid configuration that cannot be published into the running
    /// process. The serving generation is untouched.
    Reload(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(message) => write!(formatter, "configuration arguments: {message}"),
            Self::Io { context, source } => {
                write!(formatter, "configuration I/O ({context}): {source}")
            }
            Self::Parse {
                source_name,
                source,
            } => write!(
                formatter,
                "failed to parse configuration {source_name}: {source}"
            ),
            Self::Validation(message) => write!(formatter, "invalid configuration: {message}"),
            Self::Compile(message) => {
                write!(formatter, "failed to compile configuration: {message}")
            }
            Self::Reload(message) => {
                write!(formatter, "cannot reload in place: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Arguments(_) | Self::Validation(_) | Self::Compile(_) | Self::Reload(_) => None,
        }
    }
}

pub fn parse_arguments<I, S>(arguments: I) -> Result<ConfigSelection, ConfigError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    match parse_invocation(arguments)? {
        Invocation::Serve(selection) => Ok(selection),
        Invocation::Control { .. } => Err(ConfigError::Arguments(
            "--control is not a server invocation".to_owned(),
        )),
        Invocation::Validate(_) => Err(ConfigError::Arguments(
            "--validate is not a server invocation".to_owned(),
        )),
    }
}

/// What the process was asked to be: the server, or a client of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Serve(ConfigSelection),
    /// Compile the configuration, report what it would serve, and start
    /// nothing. See `src/validate.rs`.
    Validate(ConfigSelection),
    /// Send one command to the control endpoint the configuration names.
    Control {
        selection: ConfigSelection,
        command: crate::control::Command,
    },
}

const USAGE: &str = "usage: hamster-wheel [--config PATH] [--control COMMAND | --validate]";

pub fn parse_invocation<I, S>(arguments: I) -> Result<Invocation, ConfigError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    // The control endpoint's name lives in the configuration, so a client
    // invocation reads the same file the server was started with.
    let (selection, rest) = match arguments.as_slice() {
        [flag, path, rest @ ..] if flag == "--config" && !path.is_empty() => {
            (ConfigSelection::File(PathBuf::from(path)), rest)
        }
        [flag] if flag == "--config" => {
            return Err(ConfigError::Arguments(
                "--config requires a file path".to_owned(),
            ));
        }
        rest => (ConfigSelection::BuiltIn, rest),
    };
    match rest {
        [] => Ok(Invocation::Serve(selection)),
        [flag, command] if flag == "--control" => {
            match crate::control::Command::parse(&command.to_string_lossy()) {
                Some(command) => Ok(Invocation::Control { selection, command }),
                None => Err(ConfigError::Arguments(format!(
                    "--control accepts {}",
                    crate::control::Command::ALL
                        .iter()
                        .map(|command| command.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            }
        }
        [flag] if flag == "--control" => Err(ConfigError::Arguments(
            "--control requires a command".to_owned(),
        )),
        [flag] if flag == "--validate" => Ok(Invocation::Validate(selection)),
        _ => Err(ConfigError::Arguments(USAGE.to_owned())),
    }
}

pub fn load(selection: ConfigSelection) -> Result<Arc<RuntimeConfig>, ConfigError> {
    let runtime = match selection {
        ConfigSelection::BuiltIn => load_builtin()?,
        ConfigSelection::File(path) => load_file(&path)?,
    };
    Ok(Arc::new(runtime))
}

pub fn load_file(path: &Path) -> Result<RuntimeConfig, ConfigError> {
    let canonical_path = fs::canonicalize(path).map_err(|source| ConfigError::Io {
        context: format!("cannot resolve config file {}", path.display()),
        source,
    })?;
    let contents = fs::read_to_string(&canonical_path).map_err(|source| ConfigError::Io {
        context: format!("cannot read config file {}", canonical_path.display()),
        source,
    })?;
    let base_directory = canonical_path.parent().ok_or_else(|| {
        ConfigError::Validation(format!(
            "config file {} has no parent directory",
            canonical_path.display()
        ))
    })?;
    compile_str_named(
        &contents,
        base_directory,
        &canonical_path.display().to_string(),
        ConfigSelection::File(canonical_path.clone()),
    )
}

pub fn compile_str(source: &str, base_directory: &Path) -> Result<RuntimeConfig, ConfigError> {
    // An in-memory configuration has no file to re-read, so a reload of the
    // result is refused rather than silently recompiling something else.
    compile_str_named(source, base_directory, "<memory>", ConfigSelection::BuiltIn)
}

fn load_builtin() -> Result<RuntimeConfig, ConfigError> {
    let base_directory = env::current_dir().map_err(|source| ConfigError::Io {
        context: "cannot determine startup working directory for built-in config".to_owned(),
        source,
    })?;
    compile_raw(builtin_raw_config(), &base_directory)
}

fn builtin_raw_config() -> RawConfig {
    RawConfig {
        server: RawServer {
            control: None,
            access_log: None,
            admin: None,
            listen: Some(DEFAULT_LISTEN.to_owned()),
            tls: None,
            max_connections: default_max_connections(),
            header_read_timeout: default_header_read_timeout(),
            body_read_timeout: default_body_read_timeout(),
            write_progress_timeout: default_write_progress_timeout(),
            max_body_memory: default_max_body_memory(),
            http1_max_buffer_size: default_http1_max_buffer_size(),
        },
        hosts: vec![RawHost {
            name: "localhost".to_owned(),
            aliases: vec!["127.0.0.1".to_owned()],
            deny_paths: Vec::new(),
            deny_names: Vec::new(),
            locations: vec![
                RawLocation {
                    path: "/api".to_owned(),
                    handler: "not_found".to_owned(),
                    root: None,
                    php: None,
                    proxy: None,
                    fastcgi: None,
                },
                RawLocation {
                    path: "/".to_owned(),
                    handler: "static".to_owned(),
                    root: Some(PathBuf::from(DEFAULT_DOCUMENT_ROOT)),
                    php: None,
                    proxy: None,
                    fastcgi: None,
                },
            ],
        }],
    }
}

fn compile_str_named(
    source: &str,
    base_directory: &Path,
    source_name: &str,
    selection: ConfigSelection,
) -> Result<RuntimeConfig, ConfigError> {
    let raw = parse_raw(source, source_name)?;
    compile_raw_from(raw, base_directory, selection, None)
}

pub(crate) fn parse_raw(source: &str, source_name: &str) -> Result<RawConfig, ConfigError> {
    toml::from_str(source).map_err(|source| ConfigError::Parse {
        source_name: source_name.to_owned(),
        source,
    })
}

pub(crate) fn compile_raw(
    raw: RawConfig,
    base_directory: &Path,
) -> Result<RuntimeConfig, ConfigError> {
    compile_raw_from(raw, base_directory, ConfigSelection::BuiltIn, None)
}

/// Compiles a configuration, optionally carrying a serving generation.
///
/// With `carry`, every resource that owns operating-system state is taken from
/// that generation instead of being built, and a configuration that disagrees
/// about any of them is refused. Without it, this is an ordinary first
/// compilation.
pub(crate) fn compile_raw_from(
    raw: RawConfig,
    base_directory: &Path,
    source: ConfigSelection,
    carry: Option<&RuntimeConfig>,
) -> Result<RuntimeConfig, ConfigError> {
    let validated = validate_raw(raw, base_directory)?;
    compile_validated_from(validated, source, carry)
}

/// Re-reads and recompiles the configuration a generation was compiled from,
/// carrying that generation's process-owned resources.
///
/// Nothing here touches the running server: the caller publishes the result,
/// or keeps serving the generation it already had.
pub fn reload(previous: &RuntimeConfig) -> Result<RuntimeConfig, ConfigError> {
    let ConfigSelection::File(path) = previous.source() else {
        return Err(ConfigError::Reload(
            "this server was started from the built-in configuration, which has no file to \
             re-read"
                .to_owned(),
        ));
    };
    let canonical_path = fs::canonicalize(path).map_err(|source| ConfigError::Io {
        context: format!("cannot resolve config file {}", path.display()),
        source,
    })?;
    let contents = fs::read_to_string(&canonical_path).map_err(|source| ConfigError::Io {
        context: format!("cannot read config file {}", canonical_path.display()),
        source,
    })?;
    let base_directory = canonical_path.parent().ok_or_else(|| {
        ConfigError::Validation(format!(
            "config file {} has no parent directory",
            canonical_path.display()
        ))
    })?;
    let raw: RawConfig = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        source_name: canonical_path.display().to_string(),
        source,
    })?;
    let selection = ConfigSelection::File(canonical_path.clone());
    compile_raw_from(raw, base_directory, selection, Some(previous))
}

pub(crate) struct ValidatedConfig {
    listen: Option<SocketAddr>,
    tls: Option<TlsSource>,
    http_limits: HttpLimits,
    control: Option<String>,
    access_log: Option<crate::access_log::Settings>,
    admin: Option<crate::admin::Settings>,
    hosts: Vec<ValidatedHost>,
}

struct ValidatedHost {
    name: String,
    aliases: Vec<String>,
    access: Arc<AccessPolicy>,
    locations: Vec<ValidatedLocation>,
}

struct ValidatedLocation {
    path: String,
    handler: ValidatedHandler,
}

enum ValidatedHandler {
    Static {
        resolved_root: PathBuf,
    },
    NotFound,
    PhpWorker {
        executable: PathBuf,
        script: PathBuf,
        workers: usize,
        queue_capacity: usize,
        request_timeout: Duration,
        startup_timeout: Duration,
        max_requests: Option<u64>,
        max_worker_age: Option<Duration>,
        env: BTreeMap<String, String>,
    },
    Proxy {
        upstream_host: String,
        upstream_port: u16,
        authority: String,
        connect_timeout: Duration,
        response_header_timeout: Duration,
        progress_timeout: Duration,
        idle_timeout: Duration,
        max_idle: usize,
        preserve_host: bool,
    },
    FastCgi {
        resolved_root: PathBuf,
        servers: Vec<SocketAddr>,
        concurrency: usize,
        index: String,
        connect_timeout: Duration,
        response_timeout: Duration,
    },
}

#[derive(Clone, Copy)]
enum HostnameRole {
    Primary(usize),
    Alias { host: usize, alias: usize },
}

pub(crate) fn validate_raw(
    raw: RawConfig,
    base_directory: &Path,
) -> Result<ValidatedConfig, ConfigError> {
    let listen = raw
        .server
        .listen
        .as_deref()
        .map(|listen| parse_listen(listen, "server.listen"))
        .transpose()?;
    let tls = raw
        .server
        .tls
        .map(|tls| validate_tls(tls, base_directory))
        .transpose()?;
    if let (Some(listen), Some(tls)) = (listen, &tls)
        && listen.port() == tls.listen.port()
        && (listen.ip() == tls.listen.ip()
            || listen.ip().is_unspecified()
            || tls.listen.ip().is_unspecified())
    {
        return Err(ConfigError::Validation(format!(
            "server.listen {listen} and server.tls.listen {} overlap",
            tls.listen
        )));
    }
    if listen.is_none() && tls.is_none() {
        return Err(ConfigError::Validation(
            "server.listen or server.tls must be configured".to_owned(),
        ));
    }
    if !(1..=MAX_CONNECTIONS).contains(&raw.server.max_connections) {
        return Err(ConfigError::Validation(format!(
            "server.max_connections must be between 1 and {MAX_CONNECTIONS}"
        )));
    }
    if !(1..=MAX_HEADER_READ_TIMEOUT_SECONDS).contains(&raw.server.header_read_timeout) {
        return Err(ConfigError::Validation(format!(
            "server.header_read_timeout must be between 1 and {MAX_HEADER_READ_TIMEOUT_SECONDS} seconds"
        )));
    }
    if !(1..=MAX_BODY_READ_TIMEOUT_SECONDS).contains(&raw.server.body_read_timeout) {
        return Err(ConfigError::Validation(format!(
            "server.body_read_timeout must be between 1 and {MAX_BODY_READ_TIMEOUT_SECONDS} seconds"
        )));
    }
    if !(1..=MAX_WRITE_PROGRESS_TIMEOUT_SECONDS).contains(&raw.server.write_progress_timeout) {
        return Err(ConfigError::Validation(format!(
            "server.write_progress_timeout must be between 1 and {MAX_WRITE_PROGRESS_TIMEOUT_SECONDS} seconds"
        )));
    }
    if raw.server.max_body_memory != 0
        && !(MIN_MAX_BODY_MEMORY..=MAX_MAX_BODY_MEMORY).contains(&raw.server.max_body_memory)
    {
        return Err(ConfigError::Validation(format!(
            "server.max_body_memory must be 0 (disabled) or between {MIN_MAX_BODY_MEMORY} and {MAX_MAX_BODY_MEMORY} bytes"
        )));
    }
    if !(MIN_HTTP1_MAX_BUFFER_SIZE..=MAX_HTTP1_MAX_BUFFER_SIZE)
        .contains(&raw.server.http1_max_buffer_size)
    {
        return Err(ConfigError::Validation(format!(
            "server.http1_max_buffer_size must be between {MIN_HTTP1_MAX_BUFFER_SIZE} and {MAX_HTTP1_MAX_BUFFER_SIZE} bytes"
        )));
    }
    let http_limits = HttpLimits {
        max_connections: raw.server.max_connections,
        header_read_timeout: Duration::from_secs(raw.server.header_read_timeout),
        body_read_timeout: Duration::from_secs(raw.server.body_read_timeout),
        write_progress_timeout: Duration::from_secs(raw.server.write_progress_timeout),
        max_body_memory: raw.server.max_body_memory as usize,
        http1_max_buffer_size: raw.server.http1_max_buffer_size,
    };
    let control = match raw.server.control {
        None => None,
        Some(control) => {
            if !cfg!(windows) {
                return Err(ConfigError::Validation(
                    "server.control is a Windows named pipe and is not available on this platform"
                        .to_owned(),
                ));
            }
            crate::control::validate_name(&control.name).map_err(ConfigError::Validation)?;
            Some(control.name)
        }
    };
    let access_log = match raw.server.access_log {
        None => None,
        Some(settings) => Some(validate_access_log(settings, base_directory)?),
    };
    let admin = match raw.server.admin {
        None => None,
        Some(settings) => Some(validate_admin(settings, listen, tls.as_ref())?),
    };
    if raw.hosts.is_empty() {
        return Err(ConfigError::Validation(
            "hosts must contain at least one virtual host".to_owned(),
        ));
    }

    let mut names = HashMap::<String, HostnameRole>::new();
    let mut hosts = Vec::with_capacity(raw.hosts.len());
    for (host_index, host) in raw.hosts.into_iter().enumerate() {
        let access = Arc::new(
            AccessPolicy::compile(host.deny_paths, host.deny_names).map_err(|error| {
                ConfigError::Validation(format!("hosts[{host_index}] ({:?}): {error}", host.name))
            })?,
        );
        validate_hostname_field(&host.name, &format!("hosts[{host_index}].name"))?;
        register_hostname(
            &mut names,
            &host.name,
            HostnameRole::Primary(host_index),
            &format!("hosts[{host_index}].name"),
        )?;

        for (alias_index, alias) in host.aliases.iter().enumerate() {
            let field = format!("hosts[{host_index}].aliases[{alias_index}]");
            validate_hostname_field(alias, &field)?;
            register_hostname(
                &mut names,
                alias,
                HostnameRole::Alias {
                    host: host_index,
                    alias: alias_index,
                },
                &field,
            )?;
        }

        if host.locations.is_empty() {
            return Err(ConfigError::Validation(format!(
                "hosts[{host_index}] ({:?}).locations must contain at least one location",
                host.name
            )));
        }
        let mut locations = Vec::with_capacity(host.locations.len());
        for (location_index, location) in host.locations.into_iter().enumerate() {
            let context = format!(
                "hosts[{host_index}] ({:?}).locations[{location_index}]",
                host.name
            );
            let path = normalize_location(&location.path).map_err(|error| {
                ConfigError::Validation(format!("{context}.path {:?}: {error}", location.path))
            })?;
            if locations
                .iter()
                .any(|existing: &ValidatedLocation| existing.path == path)
            {
                return Err(ConfigError::Validation(format!(
                    "{context}.path {:?} duplicates normalized location {path:?}",
                    location.path
                )));
            }
            if location.fastcgi.is_some() && location.handler != "fastcgi" {
                return Err(ConfigError::Validation(format!(
                    "{context}.fastcgi is not valid for handler {:?}",
                    location.handler
                )));
            }
            let handler = match location.handler.as_str() {
                "static" => {
                    if location.php.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php is not valid for handler \"static\""
                        )));
                    }
                    if location.proxy.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.proxy is not valid for handler \"static\""
                        )));
                    }
                    let root = location.root.ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "{context}.root is required for handler \"static\""
                        ))
                    })?;
                    if root.as_os_str().is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root must not be empty"
                        )));
                    }
                    let resolved_root = if root.is_absolute() {
                        root
                    } else {
                        base_directory.join(root)
                    };
                    ValidatedHandler::Static { resolved_root }
                }
                "not_found" => {
                    if location.root.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root is not valid for handler \"not_found\""
                        )));
                    }
                    if location.php.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php is not valid for handler \"not_found\""
                        )));
                    }
                    if location.proxy.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.proxy is not valid for handler \"not_found\""
                        )));
                    }
                    ValidatedHandler::NotFound
                }
                "php_worker" => {
                    if location.root.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root is not valid for handler \"php_worker\""
                        )));
                    }
                    if location.proxy.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.proxy is not valid for handler \"php_worker\""
                        )));
                    }
                    let php = location.php.ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "{context}.php is required for handler \"php_worker\""
                        ))
                    })?;
                    if php.executable.as_os_str().is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.executable must not be empty"
                        )));
                    }
                    if php.script.as_os_str().is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.script must not be empty"
                        )));
                    }
                    if !(1..=MAX_WORKERS).contains(&php.workers) {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.workers must be between 1 and {MAX_WORKERS}"
                        )));
                    }
                    let queue_capacity = php
                        .queue_capacity
                        .unwrap_or_else(|| php.workers.saturating_mul(8).min(MAX_QUEUE_CAPACITY));
                    if !(1..=MAX_QUEUE_CAPACITY).contains(&queue_capacity) {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.queue_capacity must be between 1 and {MAX_QUEUE_CAPACITY}"
                        )));
                    }
                    let request_timeout = match php.request_timeout {
                        None => DEFAULT_REQUEST_TIMEOUT,
                        Some(seconds) => {
                            let configured = Duration::from_secs(seconds);
                            if !(MIN_REQUEST_TIMEOUT..=MAX_REQUEST_TIMEOUT).contains(&configured) {
                                return Err(ConfigError::Validation(format!(
                                    "{context}.php.request_timeout must be between {} and {} seconds",
                                    MIN_REQUEST_TIMEOUT.as_secs(),
                                    MAX_REQUEST_TIMEOUT.as_secs()
                                )));
                            }
                            configured
                        }
                    };
                    let startup_timeout = match php.startup_timeout {
                        None => DEFAULT_STARTUP_TIMEOUT,
                        Some(seconds) => {
                            let configured = Duration::from_secs(seconds);
                            if !(MIN_STARTUP_TIMEOUT..=MAX_STARTUP_TIMEOUT).contains(&configured) {
                                return Err(ConfigError::Validation(format!(
                                    "{context}.php.startup_timeout must be between {} and {} seconds",
                                    MIN_STARTUP_TIMEOUT.as_secs(),
                                    MAX_STARTUP_TIMEOUT.as_secs()
                                )));
                            }
                            configured
                        }
                    };
                    if php.max_requests == Some(0) {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.max_requests must be at least 1 when configured"
                        )));
                    }
                    if php.max_worker_age == Some(0) {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php.max_worker_age must be at least 1 second when configured"
                        )));
                    }
                    for name in php.env.keys() {
                        if name.is_empty() {
                            return Err(ConfigError::Validation(format!(
                                "{context}.php.env has an empty variable name"
                            )));
                        }
                        if name.contains('=') || name.contains('\0') {
                            return Err(ConfigError::Validation(format!(
                                "{context}.php.env name {name:?} must not contain \"=\" or NUL"
                            )));
                        }
                    }
                    for (name, value) in &php.env {
                        if value.contains('\0') {
                            return Err(ConfigError::Validation(format!(
                                "{context}.php.env[{name:?}] must not contain NUL"
                            )));
                        }
                    }
                    ValidatedHandler::PhpWorker {
                        executable: resolve_path(base_directory, php.executable),
                        script: resolve_path(base_directory, php.script),
                        workers: php.workers,
                        queue_capacity,
                        request_timeout,
                        startup_timeout,
                        max_requests: php.max_requests,
                        max_worker_age: php.max_worker_age.map(Duration::from_secs),
                        env: php.env,
                    }
                }
                "proxy" => {
                    if location.root.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root is not valid for handler \"proxy\""
                        )));
                    }
                    if location.php.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php is not valid for handler \"proxy\""
                        )));
                    }
                    let proxy = location.proxy.ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "{context}.proxy is required for handler \"proxy\""
                        ))
                    })?;
                    validate_proxy(proxy, &context)?
                }
                "fastcgi" => {
                    if location.php.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php is not valid for handler \"fastcgi\""
                        )));
                    }
                    if location.proxy.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.proxy is not valid for handler \"fastcgi\""
                        )));
                    }
                    let root = location.root.ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "{context}.root is required for handler \"fastcgi\""
                        ))
                    })?;
                    if root.as_os_str().is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root must not be empty"
                        )));
                    }
                    let resolved_root = if root.is_absolute() {
                        root
                    } else {
                        base_directory.join(root)
                    };
                    let fastcgi = location.fastcgi.ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "{context}.fastcgi is required for handler \"fastcgi\""
                        ))
                    })?;
                    validate_fastcgi(fastcgi, resolved_root, &context)?
                }
                handler => {
                    return Err(ConfigError::Validation(format!(
                        "{context}.handler {handler:?} is unknown; expected \"static\", \"not_found\", \"php_worker\", \"proxy\", or \"fastcgi\""
                    )));
                }
            };
            locations.push(ValidatedLocation { path, handler });
        }

        hosts.push(ValidatedHost {
            name: host.name,
            aliases: host.aliases,
            access,
            locations,
        });
    }
    Ok(ValidatedConfig {
        listen,
        tls,
        http_limits,
        control,
        access_log,
        admin,
        hosts,
    })
}

/// Validates a `fastcgi` location without touching the filesystem or the
/// network: the root is canonicalized and the servers are used only at
/// compilation and request time.
fn validate_fastcgi(
    fastcgi: RawFastCgi,
    resolved_root: PathBuf,
    context: &str,
) -> Result<ValidatedHandler, ConfigError> {
    if fastcgi.servers.is_empty() || fastcgi.servers.len() > FASTCGI_MAX_SERVERS {
        return Err(ConfigError::Validation(format!(
            "{context}.fastcgi.servers must list between 1 and {FASTCGI_MAX_SERVERS} addresses"
        )));
    }
    let mut servers = Vec::with_capacity(fastcgi.servers.len());
    for (index, server) in fastcgi.servers.iter().enumerate() {
        let field = format!("{context}.fastcgi.servers[{index}]");
        let address = server.parse::<SocketAddr>().map_err(|error| {
            ConfigError::Validation(format!(
                "{field} {server:?} is not a numeric IP socket address: {error}"
            ))
        })?;
        if address.port() == 0 {
            return Err(ConfigError::Validation(format!(
                "{field} {server:?} must name a port"
            )));
        }
        if servers.contains(&address) {
            return Err(ConfigError::Validation(format!(
                "{field} {server:?} is listed twice"
            )));
        }
        servers.push(address);
    }
    let concurrency = fastcgi.concurrency.unwrap_or(1);
    if !(1..=FASTCGI_MAX_CONCURRENCY).contains(&concurrency) {
        return Err(ConfigError::Validation(format!(
            "{context}.fastcgi.concurrency must be between 1 and {FASTCGI_MAX_CONCURRENCY}"
        )));
    }
    let index = fastcgi
        .index
        .unwrap_or_else(|| FASTCGI_DEFAULT_INDEX.to_owned());
    let plain_segments = !index.is_empty()
        && !index.starts_with('/')
        && index.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.contains(['\\', ':', '\0'])
        });
    let script = index.len() > 4 && index[index.len() - 4..].eq_ignore_ascii_case(".php");
    if !plain_segments || !script {
        return Err(ConfigError::Validation(format!(
            "{context}.fastcgi.index {index:?} must be a relative path to a .php script inside the root"
        )));
    }
    let seconds = |value: Option<u64>, default: Duration, name: &str| match value {
        None => Ok(default),
        Some(value @ 1..=3600) => Ok(Duration::from_secs(value)),
        Some(_) => Err(ConfigError::Validation(format!(
            "{context}.fastcgi.{name} must be between 1 and 3600 seconds"
        ))),
    };
    Ok(ValidatedHandler::FastCgi {
        resolved_root,
        servers,
        concurrency,
        index,
        connect_timeout: seconds(
            fastcgi.connect_timeout,
            FASTCGI_DEFAULT_CONNECT_TIMEOUT,
            "connect_timeout",
        )?,
        response_timeout: seconds(
            fastcgi.response_timeout,
            FASTCGI_DEFAULT_RESPONSE_TIMEOUT,
            "response_timeout",
        )?,
    })
}

fn parse_listen(listen: &str, field: &str) -> Result<SocketAddr, ConfigError> {
    listen.parse::<SocketAddr>().map_err(|error| {
        ConfigError::Validation(format!(
            "{field} {listen:?} is not a numeric IP socket address: {error}"
        ))
    })
}

/// Validates `[server.tls]` without reading any file. Loading and checking
/// the certificates is compilation, which happens before any socket is bound.
fn validate_tls(tls: RawTls, base_directory: &Path) -> Result<TlsSource, ConfigError> {
    let listen = parse_listen(&tls.listen, "server.tls.listen")?;
    let handshake_timeout = match tls.handshake_timeout {
        None => DEFAULT_HANDSHAKE_TIMEOUT,
        Some(seconds) if (1..=MAX_HANDSHAKE_TIMEOUT.as_secs()).contains(&seconds) => {
            Duration::from_secs(seconds)
        }
        Some(_) => {
            return Err(ConfigError::Validation(format!(
                "server.tls.handshake_timeout must be between 1 and {} seconds",
                MAX_HANDSHAKE_TIMEOUT.as_secs()
            )));
        }
    };
    if tls.certificates.is_empty() {
        return Err(ConfigError::Validation(
            "server.tls.certificates must contain at least one certificate".to_owned(),
        ));
    }
    let mut seen = HashMap::<TlsName, String>::new();
    let mut default = None::<String>;
    let mut certificates = Vec::with_capacity(tls.certificates.len());
    for (index, certificate) in tls.certificates.into_iter().enumerate() {
        let field = format!("server.tls.certificates[{index}]");
        for (path, name) in [(&certificate.cert, "cert"), (&certificate.key, "key")] {
            if path.as_os_str().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "{field}.{name} must not be empty"
                )));
            }
        }
        if certificate.names.is_empty() {
            return Err(ConfigError::Validation(format!(
                "{field}.names must contain at least one name"
            )));
        }
        let mut names = Vec::with_capacity(certificate.names.len());
        for (name_index, name) in certificate.names.iter().enumerate() {
            let name_field = format!("{field}.names[{name_index}]");
            let parsed = TlsName::parse(name).map_err(|reason| {
                ConfigError::Validation(format!("{name_field} {name:?} {reason}"))
            })?;
            if let Some(prior) = seen.insert(parsed.clone(), name_field.clone()) {
                return Err(ConfigError::Validation(format!(
                    "{name_field} {name:?} duplicates {prior}"
                )));
            }
            names.push(parsed);
        }
        if certificate.default
            && let Some(prior) = default.replace(field.clone())
        {
            return Err(ConfigError::Validation(format!(
                "{field}.default conflicts with {prior}.default; at most one certificate is the default"
            )));
        }
        certificates.push(CertificateSource {
            cert: resolve_path(base_directory, certificate.cert),
            key: resolve_path(base_directory, certificate.key),
            names,
            default: certificate.default,
            field,
        });
    }
    Ok(TlsSource {
        listen,
        handshake_timeout,
        certificates,
    })
}

fn validate_proxy(proxy: RawProxy, context: &str) -> Result<ValidatedHandler, ConfigError> {
    let field = format!("{context}.proxy.upstream");
    let invalid =
        |reason: &str| ConfigError::Validation(format!("{field} {:?} {reason}", proxy.upstream));
    let uri = proxy
        .upstream
        .parse::<hyper::Uri>()
        .map_err(|error| invalid(&format!("is not a valid URL: {error}")))?;
    match uri.scheme_str() {
        Some("http") => {}
        Some("https") => {
            return Err(invalid(
                "uses https; TLS upstreams are not supported, only http://host:port",
            ));
        }
        _ => return Err(invalid("must be an absolute http:// URL")),
    }
    let authority = uri
        .authority()
        .ok_or_else(|| invalid("has no host"))?
        .clone();
    if authority.as_str().contains('@') {
        return Err(invalid("must not contain user information"));
    }
    // One endpoint, forwarded verbatim: a path would imply prefix rewriting,
    // which v1 deliberately does not do.
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return Err(invalid(
            "must not have a path or query; the request path is forwarded unchanged",
        ));
    }
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err(invalid("has no host"));
    }
    // `http::uri::Authority` accepts an explicit non-numeric or out-of-range
    // port but reports it as `None`. Do not silently connect to port 80 while
    // retaining that invalid authority in the upstream Host header.
    let port = match authority.as_str().strip_prefix(authority.host()) {
        Some("") => 80,
        Some(suffix) if suffix.starts_with(':') => authority
            .port_u16()
            .ok_or_else(|| invalid("has an invalid port"))?,
        _ => return Err(invalid("has an invalid authority")),
    };
    if port == 0 {
        return Err(invalid("must not use port 0"));
    }
    let seconds = |value: Option<u64>, default: Duration, name: &str| match value {
        None => Ok(default),
        Some(seconds) if (1..=MAX_PROXY_TIMEOUT_SECONDS).contains(&seconds) => {
            Ok(Duration::from_secs(seconds))
        }
        Some(_) => Err(ConfigError::Validation(format!(
            "{context}.proxy.{name} must be between 1 and {MAX_PROXY_TIMEOUT_SECONDS} seconds"
        ))),
    };
    let connect_timeout = seconds(
        proxy.connect_timeout,
        DEFAULT_CONNECT_TIMEOUT,
        "connect_timeout",
    )?;
    let response_header_timeout = seconds(
        proxy.response_header_timeout,
        DEFAULT_RESPONSE_HEADER_TIMEOUT,
        "response_header_timeout",
    )?;
    let progress_timeout = seconds(
        proxy.progress_timeout,
        DEFAULT_PROGRESS_TIMEOUT,
        "progress_timeout",
    )?;
    let idle_timeout = seconds(proxy.idle_timeout, DEFAULT_IDLE_TIMEOUT, "idle_timeout")?;
    let max_idle = proxy.max_idle.unwrap_or(DEFAULT_MAX_IDLE);
    if max_idle > MAX_IDLE_LIMIT {
        return Err(ConfigError::Validation(format!(
            "{context}.proxy.max_idle must be between 0 and {MAX_IDLE_LIMIT}"
        )));
    }
    Ok(ValidatedHandler::Proxy {
        upstream_host: host.to_owned(),
        upstream_port: port,
        authority: authority.as_str().to_owned(),
        connect_timeout,
        response_header_timeout,
        progress_timeout,
        idle_timeout,
        max_idle,
        preserve_host: proxy.preserve_host.unwrap_or(true),
    })
}

/// Resolves an upstream once, at startup. The first address wins and is
/// printed at startup; there is no re-resolution and no fallback address.
fn resolve_upstream(host: &str, port: u16, field: &str) -> Result<SocketAddr, ConfigError> {
    use std::net::ToSocketAddrs;
    let mut addresses = (host, port).to_socket_addrs().map_err(|error| {
        ConfigError::Compile(format!("{field} host {host:?} cannot be resolved: {error}"))
    })?;
    addresses.next().ok_or_else(|| {
        ConfigError::Compile(format!("{field} host {host:?} resolved to no address"))
    })
}

fn resolve_path(base_directory: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_directory.join(path)
    }
}

fn validate_hostname_field(hostname: &str, field: &str) -> Result<(), ConfigError> {
    if hostname.contains(':') {
        return Err(ConfigError::Validation(format!(
            "{field} {hostname:?} must not contain a port"
        )));
    }
    if !valid_hostname(hostname) {
        return Err(ConfigError::Validation(format!(
            "{field} {hostname:?} is not a valid hostname"
        )));
    }
    Ok(())
}

fn register_hostname(
    names: &mut HashMap<String, HostnameRole>,
    hostname: &str,
    role: HostnameRole,
    field: &str,
) -> Result<(), ConfigError> {
    let normalized = hostname.to_ascii_lowercase();
    if let Some(existing) = names.insert(normalized, role) {
        let prior = match existing {
            HostnameRole::Primary(host) => format!("hosts[{host}].name"),
            HostnameRole::Alias { host, alias } => {
                format!("hosts[{host}].aliases[{alias}]")
            }
        };
        return Err(ConfigError::Validation(format!(
            "{field} {hostname:?} collides case-insensitively with {prior}"
        )));
    }
    Ok(())
}

pub(crate) fn compile_validated(validated: ValidatedConfig) -> Result<RuntimeConfig, ConfigError> {
    compile_validated_from(validated, ConfigSelection::BuiltIn, None)
}

pub(crate) fn compile_validated_from(
    validated: ValidatedConfig,
    source: ConfigSelection,
    carry: Option<&RuntimeConfig>,
) -> Result<RuntimeConfig, ConfigError> {
    let mut identity = ReloadIdentity::from_validated(&validated);
    // Refused before anything is built, so a configuration that needs a
    // restart costs the serving generation nothing at all.
    if let Some(previous) = carry
        && let Some(reason) = previous.identity.refuse_process_change(&identity)
    {
        return Err(ConfigError::Reload(reason));
    }
    let tls = match carry {
        // The identity above proved the TLS section identical, so the serving
        // listener is carried with its certificate generation and its
        // handshake counters intact. Certificate *contents* are reloaded by
        // the separate transactional certificate reload.
        Some(previous) => previous.tls.clone(),
        None => validated
            .tls
            .map(|source| TlsListener::compile(source).map(Arc::new))
            .transpose()
            .map_err(ConfigError::Compile)?,
    };
    let mut virtual_hosts = Vec::new();
    let mut php_pools = Vec::new();
    let mut upstreams = Vec::new();
    let mut fastcgi_backends = Vec::new();
    // One budget for the whole process. Every PHP pool shares it, because the
    // resource it protects is this process's memory, not a pool's queue.
    let body_budget = BodyBudget::new(validated.http_limits.max_body_memory());
    for (host_index, host) in validated.hosts.into_iter().enumerate() {
        let mut locations = Vec::with_capacity(host.locations.len());
        for (location_index, location) in host.locations.into_iter().enumerate() {
            let handler = match location.handler {
                ValidatedHandler::Static { resolved_root } => {
                    let canonical_root = canonical_directory(
                        &resolved_root,
                        &format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].root",
                            host.name
                        ),
                    )?;
                    Handler::static_files_with_access(
                        Arc::new(canonical_root),
                        Arc::clone(&host.access),
                    )
                }
                ValidatedHandler::FastCgi {
                    resolved_root,
                    servers,
                    concurrency,
                    index,
                    connect_timeout,
                    response_timeout,
                } => {
                    let document_root = canonical_directory(
                        &resolved_root,
                        &format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].root",
                            host.name
                        ),
                    )?;
                    let label = format!("{}{}", host.name, location.path);
                    let fingerprint = format!(
                        "{}|{index:?}|{servers:?}|{concurrency}|{connect_timeout:?}|{response_timeout:?}",
                        document_root.display()
                    );
                    identity.fastcgi.insert(label.clone(), fingerprint.clone());
                    let backend = match carry {
                        Some(previous) => carried(
                            previous.fastcgi_backend(&label),
                            &previous.identity.fastcgi,
                            &label,
                            &fingerprint,
                            "fastcgi endpoint",
                        )?,
                        None => FastCgiBackend::new(FastCgiConfig {
                            label: Arc::from(label.as_str()),
                            document_root,
                            index,
                            servers,
                            concurrency,
                            connect_timeout,
                            response_timeout,
                        }),
                    };
                    fastcgi_backends.push(Arc::clone(&backend));
                    Handler::fastcgi_with_access(backend, Arc::clone(&host.access))
                }
                ValidatedHandler::NotFound => Handler::not_found(),
                ValidatedHandler::PhpWorker {
                    executable,
                    script,
                    workers,
                    queue_capacity,
                    request_timeout,
                    startup_timeout,
                    max_requests,
                    max_worker_age,
                    env,
                } => {
                    let executable = canonical_file(
                        &executable,
                        &format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].php.executable",
                            host.name
                        ),
                    )?;
                    let script = canonical_file(
                        &script,
                        &format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].php.script",
                            host.name
                        ),
                    )?;
                    // Host and location together, so the label stays unique
                    // once more than one pool can exist, and matches strings
                    // the operator already wrote in their configuration.
                    let label: Arc<str> =
                        Arc::from(format!("{}{}", host.name, location.path).as_str());
                    let fingerprint = format!(
                        "{}|{}|{workers}|{queue_capacity}|{request_timeout:?}|{startup_timeout:?}|{max_requests:?}|{max_worker_age:?}|{env:?}",
                        executable.display(),
                        script.display()
                    );
                    identity
                        .pools
                        .insert(label.to_string(), fingerprint.clone());
                    let pool = match carry {
                        Some(previous) => carried(
                            previous.php_worker_pool(&label),
                            &previous.identity.pools,
                            &label,
                            &fingerprint,
                            "php pool",
                        )?,
                        None => Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
                            worker: PhpWorkerConfig {
                                executable,
                                script,
                                env,
                            },
                            label,
                            workers,
                            queue_capacity,
                            request_timeout,
                            startup_timeout,
                            max_requests,
                            max_worker_age,
                            body_budget: Arc::clone(&body_budget),
                        })),
                    };
                    php_pools.push(Arc::clone(&pool));
                    Handler::php_worker(pool)
                }
                ValidatedHandler::Proxy {
                    upstream_host,
                    upstream_port,
                    authority,
                    connect_timeout,
                    response_header_timeout,
                    progress_timeout,
                    idle_timeout,
                    max_idle,
                    preserve_host,
                } => {
                    let field = format!(
                        "hosts[{host_index}] ({:?}).locations[{location_index}].proxy.upstream",
                        host.name
                    );
                    let address = resolve_upstream(&upstream_host, upstream_port, &field)?;
                    let authority =
                        hyper::header::HeaderValue::from_str(&authority).map_err(|error| {
                            ConfigError::Compile(format!("{field} authority: {error}"))
                        })?;
                    let label = format!("{}{}", host.name, location.path);
                    let fingerprint = format!(
                        "{address}|{authority:?}|{connect_timeout:?}|{response_header_timeout:?}|{progress_timeout:?}|{idle_timeout:?}|{max_idle}|{preserve_host}"
                    );
                    identity
                        .upstreams
                        .insert(label.clone(), fingerprint.clone());
                    let upstream = match carry {
                        Some(previous) => carried(
                            previous.upstream(&label),
                            &previous.identity.upstreams,
                            &label,
                            &fingerprint,
                            "proxy upstream",
                        )?,
                        None => Upstream::new(ProxyConfig {
                            label: Arc::from(label.as_str()),
                            address,
                            authority,
                            connect_timeout,
                            response_header_timeout,
                            progress_timeout,
                            idle_timeout,
                            max_idle,
                            preserve_host,
                            max_buf_size: validated.http_limits.http1_max_buffer_size(),
                        }),
                    };
                    upstreams.push(Arc::clone(&upstream));
                    Handler::proxy(upstream)
                }
            };
            locations.push((location.path, handler));
        }
        let location_refs = locations
            .iter()
            .map(|(path, handler)| (path.as_str(), handler.clone()))
            .collect();
        let location_router = Arc::new(
            LocationRouter::with_access(
                location_refs,
                Handler::not_found(),
                Arc::clone(&host.access),
            )
            .map_err(|error| compile_router_error(&host.name, "locations", error))?,
        );
        for hostname in
            std::iter::once(host.name.as_str()).chain(host.aliases.iter().map(String::as_str))
        {
            virtual_hosts.push(
                VirtualHost::new(hostname, Arc::clone(&location_router))
                    .map_err(|error| compile_router_error(&host.name, "hostname", error))?,
            );
        }
    }
    let router = HostRouter::new(virtual_hosts)
        .map_err(|error| ConfigError::Compile(format!("host router: {error}")))?;
    if let Some(previous) = carry {
        // A resource the serving generation owns and the new configuration
        // does not would have to be destroyed, which is the same decision as
        // creating one: it needs a restart, not a reload.
        for (kind, serving, published) in [
            ("php pool", &previous.identity.pools, &identity.pools),
            (
                "proxy upstream",
                &previous.identity.upstreams,
                &identity.upstreams,
            ),
            (
                "fastcgi endpoint",
                &previous.identity.fastcgi,
                &identity.fastcgi,
            ),
        ] {
            if let Some(label) = serving.keys().find(|label| !published.contains_key(*label)) {
                return Err(ConfigError::Reload(format!(
                    "{kind} {label} is no longer configured; a reload cannot retire a running \
                     resource, so restart to apply it"
                )));
            }
        }
    }
    let generation = carry.map_or(1, |previous| previous.generation + 1);
    Ok(RuntimeConfig {
        generation,
        source,
        identity,
        listen: validated.listen,
        tls,
        http_limits: validated.http_limits,
        control: validated.control,
        access_log: validated.access_log,
        admin: validated.admin,
        router: Arc::new(router),
        php_pools,
        upstreams,
        fastcgi_backends,
    })
}

/// Takes a resource from the serving generation, or says why it cannot.
///
/// `existing` is the resource with this label in the serving generation and
/// `serving` its fingerprints. A label that is new, or whose settings differ,
/// means the published generation would have to build something that owns
/// operating-system state.
fn carried<T>(
    existing: Option<T>,
    serving: &BTreeMap<String, String>,
    label: &str,
    fingerprint: &str,
    kind: &str,
) -> Result<T, ConfigError> {
    match (existing, serving.get(label)) {
        (Some(resource), Some(previous)) if previous == fingerprint => Ok(resource),
        (Some(_), _) => Err(ConfigError::Reload(format!(
            "{kind} {label} changed; its settings are held by a running resource, so restart to \
             apply them"
        ))),
        (None, _) => Err(ConfigError::Reload(format!(
            "{kind} {label} is new; a reload cannot start one, so restart to apply it"
        ))),
    }
}

/// What a reload may not change in place, plus a fingerprint per resource.
///
/// A reload must never create or destroy something that owns operating-system
/// state: a listening socket, a named pipe, a log file, a PHP worker process,
/// an upstream connection pool. Those are carried from the serving generation
/// into the published one, unchanged, which is also what keeps warm PHP
/// workers warm across a reload. So every such resource is fingerprinted from
/// the settings it was built with, and a reload whose configuration disagrees
/// with the fingerprint is refused, naming the resource, rather than quietly
/// restarting it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReloadIdentity {
    listen: Option<SocketAddr>,
    tls: Option<String>,
    control: Option<String>,
    admin: Option<SocketAddr>,
    access_log: Option<String>,
    /// The body budget is one allocation shared by every pool, so its size is
    /// fixed for the life of the process.
    max_body_memory: usize,
    /// Buffer size is baked into each upstream's client at construction.
    http1_max_buffer_size: usize,
    pools: BTreeMap<String, String>,
    upstreams: BTreeMap<String, String>,
    fastcgi: BTreeMap<String, String>,
}

impl ReloadIdentity {
    /// The parts that are decided before any host is walked.
    fn from_validated(validated: &ValidatedConfig) -> Self {
        Self {
            listen: validated.listen,
            tls: validated.tls.as_ref().map(fingerprint_tls),
            control: validated.control.clone(),
            admin: validated.admin.map(|admin| admin.listen()),
            access_log: validated.access_log.as_ref().map(fingerprint_access_log),
            max_body_memory: validated.http_limits.max_body_memory(),
            http1_max_buffer_size: validated.http_limits.http1_max_buffer_size(),
            pools: BTreeMap::new(),
            upstreams: BTreeMap::new(),
            fastcgi: BTreeMap::new(),
        }
    }

    /// Why a reload cannot publish this configuration in place, if it cannot.
    ///
    /// Only the process-owned parts are compared here; resources are compared
    /// one at a time as they are reached, so the refusal can name the exact
    /// pool, upstream or endpoint rather than the whole section.
    fn refuse_process_change(&self, next: &Self) -> Option<String> {
        let changed = [
            (self.listen != next.listen, "server.listen"),
            (self.tls != next.tls, "server.tls"),
            (self.control != next.control, "server.control"),
            (self.admin != next.admin, "server.admin"),
            (self.access_log != next.access_log, "server.access_log"),
            (
                self.max_body_memory != next.max_body_memory,
                "server.max_body_memory",
            ),
            (
                self.http1_max_buffer_size != next.http1_max_buffer_size,
                "server.http1_max_buffer_size",
            ),
        ];
        changed
            .into_iter()
            .find_map(|(differs, field)| differs.then_some(field))
            .map(|field| {
                format!(
                    "{field} changed, which owns a socket, a pipe, a file or a client that a reload must not replace; restart to apply it"
                )
            })
    }
}

fn fingerprint_tls(source: &TlsSource) -> String {
    let certificates = source
        .certificates
        .iter()
        .map(|certificate| {
            format!(
                "{}:{}:{}:{}:{}",
                certificate.field,
                certificate.cert.display(),
                certificate.key.display(),
                certificate
                    .names
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                certificate.default
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    format!(
        "{}|{:?}|{certificates}",
        source.listen, source.handshake_timeout
    )
}

fn fingerprint_access_log(settings: &crate::access_log::Settings) -> String {
    format!(
        "{:?}|{}|{}|{:?}|{}",
        settings.path(),
        settings.include_query(),
        settings.queue_capacity(),
        settings.rotate_bytes(),
        settings.keep()
    )
}

/// Validates `[server.admin]`.
///
/// The address is bound in `server::run`, before workers start, so a clash
/// with another process fails startup. What is checked here is a clash an
/// operator can only have meant as a mistake: naming the address an
/// application listener already uses, which would make the administrative
/// surface and the application fight for the same socket.
fn validate_admin(
    raw: RawAdmin,
    listen: Option<SocketAddr>,
    tls: Option<&TlsSource>,
) -> Result<crate::admin::Settings, ConfigError> {
    let address: SocketAddr = raw.listen.parse().map_err(|_| {
        ConfigError::Validation(format!(
            "server.admin.listen must be host:port, got {}",
            raw.listen
        ))
    })?;
    // Port 0 asks the operating system for a free port, so two port-0
    // addresses are not the same socket and must not be read as a clash.
    let clashes = address.port() != 0
        && (Some(address) == listen || tls.map(|tls| tls.listen) == Some(address));
    if clashes {
        return Err(ConfigError::Validation(format!(
            "server.admin.listen {address} is already an application listener"
        )));
    }
    Ok(crate::admin::Settings::new(address))
}

/// Validates `[server.access_log]` without creating or opening anything.
///
/// The file is opened once at startup by `server::run`, which fails closed on
/// a path that cannot be written. Validation here is about values a
/// configuration can get wrong: an unusable queue depth, or a path that names
/// a directory that is already there.
fn validate_access_log(
    raw: RawAccessLog,
    base_directory: &Path,
) -> Result<crate::access_log::Settings, ConfigError> {
    use crate::access_log::{MAX_QUEUE_CAPACITY, MIN_QUEUE_CAPACITY};
    if !(MIN_QUEUE_CAPACITY..=MAX_QUEUE_CAPACITY).contains(&raw.queue_capacity) {
        return Err(ConfigError::Validation(format!(
            "server.access_log.queue_capacity must be between {MIN_QUEUE_CAPACITY} and {MAX_QUEUE_CAPACITY}"
        )));
    }
    let rotate_bytes = match raw.rotate_bytes {
        None | Some(0) => None,
        Some(bytes) if bytes < crate::access_log::MIN_ROTATE_BYTES => {
            return Err(ConfigError::Validation(format!(
                "server.access_log.rotate_bytes must be 0 or at least {}",
                crate::access_log::MIN_ROTATE_BYTES
            )));
        }
        Some(bytes) => Some(bytes),
    };
    let keep = match raw.keep {
        None => crate::access_log::DEFAULT_KEEP,
        Some(keep) if keep > crate::access_log::MAX_KEEP => {
            return Err(ConfigError::Validation(format!(
                "server.access_log.keep must be at most {}",
                crate::access_log::MAX_KEEP
            )));
        }
        Some(keep) => keep,
    };
    if rotate_bytes.is_none() && raw.keep.is_some() {
        // Keeping archives of a file that never rotates is a configuration an
        // operator cannot have meant, and silently ignoring the key would
        // leave them believing in rotation they do not have.
        return Err(ConfigError::Validation(
            "server.access_log.keep needs server.access_log.rotate_bytes".to_owned(),
        ));
    }
    if rotate_bytes.is_some() && raw.path.is_none() {
        return Err(ConfigError::Validation(
            "server.access_log.rotate_bytes needs server.access_log.path; stdout is rotated by whoever owns it"
                .to_owned(),
        ));
    }
    let path = match raw.path {
        None => None,
        Some(path) if path.trim().is_empty() => {
            return Err(ConfigError::Validation(
                "server.access_log.path must not be empty; omit it to log to stdout".to_owned(),
            ));
        }
        Some(path) => {
            let resolved = resolve_path(base_directory, PathBuf::from(path));
            if resolved.is_dir() {
                return Err(ConfigError::Validation(format!(
                    "server.access_log.path {} is a directory",
                    resolved.display()
                )));
            }
            Some(resolved)
        }
    };
    Ok(crate::access_log::Settings::new(
        path,
        raw.include_query,
        raw.queue_capacity,
        rotate_bytes,
        keep,
    ))
}

fn canonical_directory(path: &Path, field: &str) -> Result<PathBuf, ConfigError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        ConfigError::Compile(format!(
            "{field} {} cannot be canonicalized: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        ConfigError::Compile(format!(
            "{field} {} cannot be inspected: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::Compile(format!(
            "{field} {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn canonical_file(path: &Path, field: &str) -> Result<PathBuf, ConfigError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        ConfigError::Compile(format!(
            "{field} {} cannot be canonicalized: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        ConfigError::Compile(format!(
            "{field} {} cannot be inspected: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::Compile(format!(
            "{field} {} is not a file",
            canonical.display()
        )));
    }
    Ok(process_compatible_path(canonical))
}

#[cfg(windows)]
fn process_compatible_path(canonical: PathBuf) -> PathBuf {
    let display = canonical.to_string_lossy();
    if let Some(path) = display.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{path}"))
    } else if let Some(path) = display.strip_prefix(r"\\?\") {
        PathBuf::from(path)
    } else {
        canonical
    }
}

#[cfg(not(windows))]
fn process_compatible_path(canonical: PathBuf) -> PathBuf {
    canonical
}

fn compile_router_error(hostname: &str, field: &str, error: RouterBuildError) -> ConfigError {
    ConfigError::Compile(format!("host {hostname:?} {field}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(root: &Path) -> String {
        format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = {:?}\n",
            root.to_string_lossy()
        )
    }

    #[test]
    fn parses_cli_selection_without_fallback_magic() {
        assert_eq!(
            parse_arguments(Vec::<OsString>::new()).unwrap(),
            ConfigSelection::BuiltIn
        );
        assert_eq!(
            parse_arguments(["--config", "custom.toml"]).unwrap(),
            ConfigSelection::File(PathBuf::from("custom.toml"))
        );
        assert!(parse_arguments(["--config"]).is_err());
        assert!(parse_arguments(["unknown"]).is_err());
        // A control invocation is not a server invocation.
        assert!(parse_arguments(["--config", "custom.toml", "--control", "stop"]).is_err());
    }

    #[test]
    fn parses_control_invocations_beside_the_server_one() {
        assert_eq!(
            parse_invocation(["--config", "custom.toml", "--control", "stop"]).unwrap(),
            Invocation::Control {
                selection: ConfigSelection::File(PathBuf::from("custom.toml")),
                command: crate::control::Command::Stop,
            }
        );
        for command in crate::control::Command::ALL {
            assert_eq!(
                parse_invocation(["--control", command.name()]).unwrap(),
                Invocation::Control {
                    selection: ConfigSelection::BuiltIn,
                    command,
                }
            );
        }
        assert_eq!(
            parse_invocation(Vec::<OsString>::new()).unwrap(),
            Invocation::Serve(ConfigSelection::BuiltIn)
        );
        assert!(parse_invocation(["--control"]).is_err());
        assert!(parse_invocation(["--control", "restart"]).is_err());
        assert!(parse_invocation(["--control", "stop", "extra"]).is_err());
    }

    #[test]
    fn parses_validate_invocations_and_keeps_them_out_of_serving() {
        assert_eq!(
            parse_invocation(["--config", "custom.toml", "--validate"]).unwrap(),
            Invocation::Validate(ConfigSelection::File(PathBuf::from("custom.toml")))
        );
        assert_eq!(
            parse_invocation(["--validate"]).unwrap(),
            Invocation::Validate(ConfigSelection::BuiltIn)
        );
        // Validation is not a server invocation, and not a control one.
        assert!(parse_arguments(["--config", "custom.toml", "--validate"]).is_err());
        assert!(parse_invocation(["--validate", "extra"]).is_err());
        assert!(parse_invocation(["--validate", "--control", "stop"]).is_err());
    }

    #[test]
    fn reads_the_control_endpoint_without_compiling_the_configuration() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let directory = std::env::temp_dir().join(format!(
            "hamster-control-endpoint-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let temporary = Scratch(directory);
        let path = temporary.0.join("hamster.toml");
        // A certificate path that cannot load: the client must still be able to
        // ask the running server to reload, which is the whole point.
        fs::write(
            &path,
            "[server]
listen = \"127.0.0.1:0\"

[server.control]
name = \"edge\"

             [[server.tls.certificates]]
cert = \"missing.pem\"
key = \"missing.key\"
             names = [\"localhost\"]

[[hosts]]
name = \"localhost\"
locations = []
",
        )
        .unwrap();
        let selection = ConfigSelection::File(path);
        assert!(load(selection.clone()).is_err());
        assert_eq!(
            control_endpoint(&selection).unwrap(),
            Some("edge".to_owned())
        );
        assert_eq!(control_endpoint(&ConfigSelection::BuiltIn).unwrap(), None);
    }

    #[test]
    fn rejects_an_unusable_control_endpoint_name() {
        let base = "[server]
listen = \"127.0.0.1:8080\"

[[hosts]]
name = \"localhost\"
                    locations = [{ path = \"/\", handler = \"not_found\" }]

[server.control]
";
        for name in ["", "a b", "pipe/name", "x!"] {
            let source = format!(
                "{base}name = \"{name}\"
"
            );
            assert!(
                matches!(
                    compile_str(&source, Path::new(".")),
                    Err(ConfigError::Validation(_))
                ),
                "{name}"
            );
        }
        // An unknown key under [server.control] is a configuration error.
        assert!(matches!(
            compile_str(
                &format!(
                    "{base}name = \"edge\"
path = \"ignored\"
"
                ),
                Path::new(".")
            ),
            Err(ConfigError::Parse { .. })
        ));
        let runtime = compile_str(
            &format!(
                "{base}name = \"edge\"
"
            ),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(runtime.control_name(), Some("edge"));
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(matches!(
            compile_str("[server", Path::new(".")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_invalid_listen_and_unknown_fields() {
        let invalid = "[server]\nlisten = \"localhost:8080\"\n\n[[hosts]]\nname = \"localhost\"\nlocations = []\n";
        assert!(matches!(
            compile_str(invalid, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
        let unknown = "[server]\nlisten = \"127.0.0.1:8080\"\ntypo = true\n\nhosts = []\n";
        assert!(matches!(
            compile_str(unknown, Path::new(".")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_host_collisions_and_ports() {
        let duplicate = "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"example.test\"\naliases = [\"EXAMPLE.TEST\"]\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n";
        assert!(matches!(
            compile_str(duplicate, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
        let port = duplicate.replace("example.test", "example.test:80");
        assert!(matches!(
            compile_str(&port, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
    }

    /// A configuration file this test owns, so a reload has something to read.
    struct ReloadScratch(PathBuf);

    impl ReloadScratch {
        fn new(tag: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "hamster-reload-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(directory.join("public")).unwrap();
            fs::create_dir_all(directory.join("other")).unwrap();
            Self(directory)
        }

        fn write(&self, body: &str) -> PathBuf {
            let path = self.0.join("hamster.toml");
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for ReloadScratch {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    /// A configuration whose only resources are a listener and static roots.
    fn reloadable(address: &str, root: &str) -> String {
        format!(
            "[server]\nlisten = \"{address}\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
             [[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"{root}\"\n"
        )
    }

    #[test]
    fn a_reload_publishes_the_next_generation_and_keeps_serving_the_old_one_on_refusal() {
        let scratch = ReloadScratch::new("generation");
        let path = scratch.write(&reloadable("127.0.0.1:8080", "public"));
        let first = load_file(&path).unwrap();
        assert_eq!(first.generation(), 1);

        // A change that owns nothing: the published generation is next, and
        // the serving one is untouched because the caller still holds it.
        fs::write(&path, reloadable("127.0.0.1:8080", "other")).unwrap();
        let second = reload(&first).unwrap();
        assert_eq!(second.generation(), 2);
        assert_eq!(first.generation(), 1);
        let third = reload(&second).unwrap();
        assert_eq!(third.generation(), 3);

        // A file that no longer compiles is an ordinary error, and the
        // generation that asked for the reload is still complete.
        fs::write(&path, "[server]\nlisten = \"nonsense\"\n").unwrap();
        assert!(matches!(
            reload(&third),
            Err(ConfigError::Validation(_) | ConfigError::Parse { .. })
        ));
        assert_eq!(third.generation(), 3);
    }

    #[test]
    fn a_reload_cannot_change_anything_that_owns_process_state() {
        let scratch = ReloadScratch::new("refuse");
        let base = format!(
            "{}\n[server.admin]\nlisten = \"127.0.0.1:9090\"\n\n[server.access_log]\npath = \"access.jsonl\"\n",
            reloadable("127.0.0.1:8080", "public")
        );
        let path = scratch.write(&base);
        let serving = load_file(&path).unwrap();

        for (change, replacement, field) in [
            (
                "listener",
                base.replace("127.0.0.1:8080", "127.0.0.1:8081"),
                "server.listen",
            ),
            (
                "admin",
                base.replace("127.0.0.1:9090", "127.0.0.1:9091"),
                "server.admin",
            ),
            (
                "access log",
                base.replace("access.jsonl", "other.jsonl"),
                "server.access_log",
            ),
            (
                "body budget",
                format!("{base}max_body_memory_marker = 0\n").replace(
                    "[server]\nlisten",
                    "[server]\nmax_body_memory = 1048576\nlisten",
                ),
                "server.max_body_memory",
            ),
        ] {
            fs::write(
                &path,
                replacement.replace("max_body_memory_marker = 0\n", ""),
            )
            .unwrap();
            let refusal = reload(&serving);
            match refusal {
                Err(ConfigError::Reload(message)) => assert!(
                    message.contains(field),
                    "{change}: expected {field} in {message}"
                ),
                Err(other) => panic!("{change}: {other}"),
                Ok(_) => panic!("{change} was not refused"),
            }
            // Refused before anything was built, so the serving generation is
            // exactly as it was.
            assert_eq!(serving.generation(), 1);
            // A refusal is read by a person in a terminal, so it stays one
            // sentence on one line: a lost string continuation once left a run
            // of indentation inside this message.
            if let Err(ConfigError::Reload(message)) = reload(&serving) {
                assert!(!message.contains("  "), "{message:?}");
                assert!(!message.contains('\n'), "{message:?}");
            }
        }
    }

    #[test]
    fn a_reload_carries_a_proxy_upstream_and_refuses_to_change_one() {
        let scratch = ReloadScratch::new("upstream");
        let proxy = |upstream: &str, root: &str| {
            format!(
                "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n\
                 [[hosts.locations]]\npath = \"/static\"\nhandler = \"static\"\nroot = \"{root}\"\n\n\
                 [[hosts.locations]]\npath = \"/\"\nhandler = \"proxy\"\n\n\
                 [hosts.locations.proxy]\nupstream = \"{upstream}\"\n"
            )
        };
        let path = scratch.write(&proxy("http://127.0.0.1:3000", "public"));
        let serving = load_file(&path).unwrap();
        let before = Arc::clone(&serving.upstreams()[0]);

        // A static change beside an upstream: the upstream is the same object,
        // so its idle connections and counters survive the reload.
        fs::write(&path, proxy("http://127.0.0.1:3000", "other")).unwrap();
        let published = reload(&serving).unwrap();
        assert!(
            Arc::ptr_eq(&before, &published.upstreams()[0]),
            "the upstream was rebuilt instead of carried"
        );

        // Changing it is refused by name.
        fs::write(&path, proxy("http://127.0.0.1:3001", "public")).unwrap();
        match reload(&serving) {
            Err(ConfigError::Reload(message)) => {
                assert!(message.contains("proxy upstream localhost/"), "{message}");
                assert!(message.contains("changed"), "{message}");
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a changed upstream was not refused"),
        }

        // So is removing it.
        fs::write(&path, reloadable("127.0.0.1:8080", "public")).unwrap();
        match reload(&serving) {
            Err(ConfigError::Reload(message)) => {
                assert!(message.contains("no longer configured"), "{message}");
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a retired upstream was not refused"),
        }

        // And adding one.
        let with_two = format!(
            "{}\n[[hosts.locations]]\npath = \"/api\"\nhandler = \"proxy\"\n\n\
             [hosts.locations.proxy]\nupstream = \"http://127.0.0.1:3002\"\n",
            proxy("http://127.0.0.1:3000", "public")
        );
        fs::write(&path, with_two).unwrap();
        match reload(&serving) {
            Err(ConfigError::Reload(message)) => {
                assert!(message.contains("is new"), "{message}");
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a new upstream was not refused"),
        }
    }

    #[test]
    fn the_built_in_configuration_has_no_file_to_reload() {
        let built_in = load_builtin().unwrap();
        match reload(&built_in) {
            Err(ConfigError::Reload(message)) => {
                assert!(message.contains("built-in"), "{message}");
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("the built-in configuration was reloaded"),
        }
        // An in-memory compilation is in the same position, and must not
        // silently reload something else.
        let in_memory = compile_str(&reloadable("127.0.0.1:8080", "."), Path::new(".")).unwrap();
        assert!(matches!(reload(&in_memory), Err(ConfigError::Reload(_))));
    }

    #[test]
    fn compiles_an_admin_listener_and_rejects_one_that_takes_an_application_address() {
        let hosts = "

[[hosts]]
name = \"localhost\"
locations = [{ path = \"/\", handler = \"not_found\" }]
";

        let without = compile_str(
            &format!(
                "[server]
listen = \"127.0.0.1:8080\"{hosts}"
            ),
            Path::new("."),
        )
        .unwrap();
        assert!(without.admin().is_none(), "admin is off unless configured");

        let configured = compile_str(
            &format!(
                "[server]
listen = \"127.0.0.1:8080\"

[server.admin]
listen = \"127.0.0.1:9090\"{hosts}"
            ),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(
            configured.admin().map(|admin| admin.listen()),
            Some("127.0.0.1:9090".parse().unwrap())
        );

        // The same socket as the application listener is a mistake, not a
        // configuration: it would make the two fight for one address.
        assert!(matches!(
            compile_str(
                &format!(
                    "[server]
listen = \"127.0.0.1:8080\"

[server.admin]
listen = \"127.0.0.1:8080\"{hosts}"
                ),
                Path::new(".")
            ),
            Err(ConfigError::Validation(_))
        ));
        // But port 0 asks for a free port, so two of them are two sockets.
        let both_ephemeral = compile_str(
            &format!(
                "[server]
listen = \"127.0.0.1:0\"

[server.admin]
listen = \"127.0.0.1:0\"{hosts}"
            ),
            Path::new("."),
        )
        .unwrap();
        assert!(both_ephemeral.admin().is_some());

        assert!(matches!(
            compile_str(
                &format!(
                    "[server]
listen = \"127.0.0.1:8080\"

[server.admin]
listen = \"not-an-address\"{hosts}"
                ),
                Path::new(".")
            ),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn compiles_an_access_log_and_rejects_values_that_cannot_work() {
        let base = "[server]
listen = \"127.0.0.1:8080\"

[server.access_log]
";
        let hosts = "

[[hosts]]
name = \"localhost\"
locations = [{ path = \"/\", handler = \"not_found\" }]
";

        // Absent section: no settings at all, so no queue and no writer.
        let without = compile_str(
            &format!(
                "[server]
listen = \"127.0.0.1:8080\"{hosts}"
            ),
            Path::new("."),
        )
        .unwrap();
        assert!(without.access_log().is_none());

        // Present with no path: stdout, query omitted, default queue.
        let stdout = compile_str(&format!("{base}{hosts}"), Path::new(".")).unwrap();
        let settings = stdout.access_log().expect("the section was present");
        assert!(settings.path().is_none());
        assert!(!settings.include_query());
        assert_eq!(
            settings.queue_capacity(),
            crate::access_log::DEFAULT_QUEUE_CAPACITY
        );
        assert_eq!(settings.target_description(), "stdout");

        // A relative path resolves beside the configuration file.
        let relative = compile_str(
            &format!("{base}path = \"logs/access.jsonl\"{hosts}"),
            Path::new("D:/deployment"),
        )
        .unwrap();
        assert_eq!(
            relative.access_log().unwrap().path(),
            Some(
                Path::new("D:/deployment")
                    .join("logs/access.jsonl")
                    .as_path()
            )
        );

        for rejected in [
            "queue_capacity = 0",
            "queue_capacity = 8",
            "queue_capacity = 65537",
            "path = \"\"",
            // A directory is never a log file, and the error belongs in
            // validation rather than in the first write.
            "path = \".\"",
        ] {
            assert!(
                matches!(
                    compile_str(&format!("{base}{rejected}{hosts}"), Path::new(".")),
                    Err(ConfigError::Validation(_))
                ),
                "{rejected} was accepted"
            );
        }
        // An unknown key in the section is still a configuration error.
        assert!(matches!(
            compile_str(&format!("{base}rotate = true{hosts}"), Path::new(".")),
            Err(ConfigError::Parse { .. })
        ));

        // Rotation: on with a size, off without one, and every combination
        // that cannot mean what it says is refused rather than ignored.
        let rotating = compile_str(
            &format!("{base}path = \"logs/access.jsonl\"\nrotate_bytes = 1048576\nkeep = 7{hosts}"),
            Path::new("."),
        )
        .unwrap();
        let settings = rotating.access_log().unwrap();
        assert_eq!(settings.rotate_bytes(), Some(1_048_576));
        assert_eq!(settings.keep(), 7);
        assert_eq!(settings.rotation_description(), "1048576 bytes, 7 kept");

        let plain = compile_str(
            &format!("{base}path = \"logs/access.jsonl\"{hosts}"),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(plain.access_log().unwrap().rotate_bytes(), None);
        assert_eq!(plain.access_log().unwrap().rotation_description(), "off");

        for rejected in [
            // Below the floor, where one batch would rotate repeatedly.
            "path = \"a.jsonl\"\nrotate_bytes = 1024",
            "path = \"a.jsonl\"\nrotate_bytes = 4096\nkeep = 65",
            // Archives of a file that never rotates.
            "path = \"a.jsonl\"\nkeep = 2",
            // Rotation of stdout, which belongs to whoever owns it.
            "rotate_bytes = 8192",
        ] {
            assert!(
                matches!(
                    compile_str(&format!("{base}{rejected}{hosts}"), Path::new(".")),
                    Err(ConfigError::Validation(_))
                ),
                "{rejected} was accepted"
            );
        }
        // Zero is how rotation is turned off explicitly.
        let explicit_off = compile_str(
            &format!("{base}path = \"logs/access.jsonl\"\nrotate_bytes = 0{hosts}"),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(explicit_off.access_log().unwrap().rotate_bytes(), None);
    }

    #[test]
    fn rejects_duplicate_locations_and_invalid_handlers() {
        let duplicate = "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/api\", handler = \"not_found\" }, { path = \"/api/\", handler = \"not_found\" }]\n";
        assert!(matches!(
            compile_str(duplicate, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
        let unknown = duplicate.replace(
            "/api/\", handler = \"not_found",
            "/other\", handler = \"proxy",
        );
        assert!(matches!(
            compile_str(&unknown, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn accepts_existing_absolute_static_root() {
        let current = env::current_dir().unwrap();
        let runtime = compile_str(&minimal(&current), Path::new("unused")).unwrap();
        assert_eq!(
            runtime.listen_address(),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert!(runtime.tls().is_none());
        assert_eq!(runtime.http_limits(), HttpLimits::default());
    }

    #[test]
    fn compiles_explicit_http_limits() {
        let current = env::current_dir().unwrap();
        let source = minimal(&current).replace(
            "listen = \"127.0.0.1:8080\"",
            "listen = \"127.0.0.1:8080\"\nmax_connections = 77\nheader_read_timeout = 12\nbody_read_timeout = 14\nwrite_progress_timeout = 16\nmax_body_memory = 4194304\nhttp1_max_buffer_size = 32768",
        );
        let runtime = compile_str(&source, Path::new("unused")).unwrap();
        let limits = runtime.http_limits();
        assert_eq!(limits.max_connections(), 77);
        assert_eq!(limits.header_read_timeout(), Duration::from_secs(12));
        assert_eq!(limits.body_read_timeout(), Duration::from_secs(14));
        assert_eq!(limits.write_progress_timeout(), Duration::from_secs(16));
        assert_eq!(limits.max_body_memory(), 4 * 1_024 * 1_024);
        assert_eq!(limits.http1_max_buffer_size(), 32_768);
    }

    #[test]
    fn accepts_a_disabled_body_budget() {
        let current = env::current_dir().unwrap();
        let source = minimal(&current).replace(
            "listen = \"127.0.0.1:8080\"",
            "listen = \"127.0.0.1:8080\"
max_body_memory = 0",
        );
        let runtime = compile_str(&source, Path::new("unused")).unwrap();
        assert_eq!(
            runtime.http_limits().max_body_memory(),
            0,
            "zero must stay an explicit opt out, not fall back to the default"
        );
    }

    #[test]
    fn rejects_http_limits_that_cannot_bound_the_server() {
        let current = env::current_dir().unwrap();
        for (field, value) in [
            ("max_connections", "0"),
            ("max_connections", "65537"),
            ("header_read_timeout", "0"),
            ("header_read_timeout", "3601"),
            ("body_read_timeout", "0"),
            ("body_read_timeout", "3601"),
            ("write_progress_timeout", "0"),
            ("write_progress_timeout", "3601"),
            // One byte under a single maximum body would make some legal
            // requests impossible even on a completely idle server.
            ("max_body_memory", "1048575"),
            ("max_body_memory", "68719476737"),
            ("http1_max_buffer_size", "8191"),
            ("http1_max_buffer_size", "1048577"),
        ] {
            let source = minimal(&current).replace(
                "listen = \"127.0.0.1:8080\"",
                &format!("listen = \"127.0.0.1:8080\"\n{field} = {value}"),
            );
            assert!(
                matches!(
                    compile_str(&source, Path::new("unused")),
                    Err(ConfigError::Validation(_))
                ),
                "{field}={value} should be rejected"
            );
        }
    }
}
