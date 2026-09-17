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

use crate::body_budget::BodyBudget;
use crate::php_worker::{
    DEFAULT_REQUEST_TIMEOUT, MAX_QUEUE_CAPACITY, MAX_REQUEST_BODY, MAX_REQUEST_TIMEOUT,
    MAX_WORKERS, MIN_REQUEST_TIMEOUT, PhpWorkerConfig, PhpWorkerError, PhpWorkerPool,
    PhpWorkerPoolConfig, WorkerStart,
};
use crate::router::{
    Handler, HostRouter, LocationRouter, RouterBuildError, VirtualHost, normalize_location,
    valid_hostname,
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
    listen: String,
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
    locations: Vec<RawLocation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocation {
    path: String,
    handler: String,
    root: Option<PathBuf>,
    php: Option<RawPhpWorker>,
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
    listen: SocketAddr,
    http_limits: HttpLimits,
    router: Arc<HostRouter>,
    php_pools: Vec<Arc<PhpWorkerPool>>,
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
    pub const fn listen_address(&self) -> SocketAddr {
        self.listen
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

    pub async fn start_workers(&self) -> Result<Vec<WorkerStart>, PhpWorkerError> {
        let mut starts = Vec::new();
        for pool in &self.php_pools {
            starts.extend(pool.start().await?);
        }
        Ok(starts)
    }

    pub async fn shutdown_workers(&self) -> Result<(), PhpWorkerError> {
        for pool in &self.php_pools {
            pool.shutdown().await?;
        }
        Ok(())
    }

    pub fn php_worker(&self) -> Option<&Arc<PhpWorkerPool>> {
        self.php_pools.first()
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
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Arguments(_) | Self::Validation(_) | Self::Compile(_) => None,
        }
    }
}

pub fn parse_arguments<I, S>(arguments: I) -> Result<ConfigSelection, ConfigError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    match arguments.as_slice() {
        [] => Ok(ConfigSelection::BuiltIn),
        [flag, path] if flag == "--config" && !path.is_empty() => {
            Ok(ConfigSelection::File(PathBuf::from(path)))
        }
        [flag] if flag == "--config" => Err(ConfigError::Arguments(
            "--config requires a file path".to_owned(),
        )),
        _ => Err(ConfigError::Arguments(
            "usage: hamster-wheel [--config PATH]".to_owned(),
        )),
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
    )
}

pub fn compile_str(source: &str, base_directory: &Path) -> Result<RuntimeConfig, ConfigError> {
    compile_str_named(source, base_directory, "<memory>")
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
            listen: DEFAULT_LISTEN.to_owned(),
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
            locations: vec![
                RawLocation {
                    path: "/api".to_owned(),
                    handler: "not_found".to_owned(),
                    root: None,
                    php: None,
                },
                RawLocation {
                    path: "/".to_owned(),
                    handler: "static".to_owned(),
                    root: Some(PathBuf::from(DEFAULT_DOCUMENT_ROOT)),
                    php: None,
                },
            ],
        }],
    }
}

fn compile_str_named(
    source: &str,
    base_directory: &Path,
    source_name: &str,
) -> Result<RuntimeConfig, ConfigError> {
    let raw = parse_raw(source, source_name)?;
    compile_raw(raw, base_directory)
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
    let validated = validate_raw(raw, base_directory)?;
    compile_validated(validated)
}

pub(crate) struct ValidatedConfig {
    listen: SocketAddr,
    http_limits: HttpLimits,
    hosts: Vec<ValidatedHost>,
}

struct ValidatedHost {
    name: String,
    aliases: Vec<String>,
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
        max_requests: Option<u64>,
        max_worker_age: Option<Duration>,
        env: BTreeMap<String, String>,
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
    let listen = raw.server.listen.parse::<SocketAddr>().map_err(|error| {
        ConfigError::Validation(format!(
            "server.listen {:?} is not a numeric IP socket address: {error}",
            raw.server.listen
        ))
    })?;
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
    if raw.hosts.is_empty() {
        return Err(ConfigError::Validation(
            "hosts must contain at least one virtual host".to_owned(),
        ));
    }

    let mut names = HashMap::<String, HostnameRole>::new();
    let mut hosts = Vec::with_capacity(raw.hosts.len());
    let mut php_worker_count = 0_usize;
    for (host_index, host) in raw.hosts.into_iter().enumerate() {
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
            let handler = match location.handler.as_str() {
                "static" => {
                    if location.php.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.php is not valid for handler \"static\""
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
                    ValidatedHandler::NotFound
                }
                "php_worker" => {
                    if location.root.is_some() {
                        return Err(ConfigError::Validation(format!(
                            "{context}.root is not valid for handler \"php_worker\""
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
                    php_worker_count += 1;
                    if php_worker_count > 1 {
                        return Err(ConfigError::Validation(format!(
                            "{context}: milestone 0.3.0 permits exactly one php_worker location"
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
                        max_requests: php.max_requests,
                        max_worker_age: php.max_worker_age.map(Duration::from_secs),
                        env: php.env,
                    }
                }
                handler => {
                    return Err(ConfigError::Validation(format!(
                        "{context}.handler {handler:?} is unknown; expected \"static\", \"not_found\", or \"php_worker\""
                    )));
                }
            };
            locations.push(ValidatedLocation { path, handler });
        }

        hosts.push(ValidatedHost {
            name: host.name,
            aliases: host.aliases,
            locations,
        });
    }
    Ok(ValidatedConfig {
        listen,
        http_limits,
        hosts,
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
    let mut virtual_hosts = Vec::new();
    let mut php_pools = Vec::new();
    // One budget for the whole process. Every PHP pool shares it, because the
    // resource it protects is this process's memory, not a pool's queue.
    let body_budget = BodyBudget::new(validated.http_limits.max_body_memory());
    for (host_index, host) in validated.hosts.into_iter().enumerate() {
        let mut locations = Vec::with_capacity(host.locations.len());
        for (location_index, location) in host.locations.into_iter().enumerate() {
            let handler = match location.handler {
                ValidatedHandler::Static { resolved_root } => {
                    let canonical_root = fs::canonicalize(&resolved_root).map_err(|error| {
                        ConfigError::Compile(format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].root {} cannot be canonicalized: {error}",
                            host.name,
                            resolved_root.display()
                        ))
                    })?;
                    let metadata = fs::metadata(&canonical_root).map_err(|error| {
                        ConfigError::Compile(format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].root {} cannot be inspected: {error}",
                            host.name,
                            canonical_root.display()
                        ))
                    })?;
                    if !metadata.is_dir() {
                        return Err(ConfigError::Compile(format!(
                            "hosts[{host_index}] ({:?}).locations[{location_index}].root {} is not a directory",
                            host.name,
                            canonical_root.display()
                        )));
                    }
                    Handler::static_files(Arc::new(canonical_root))
                }
                ValidatedHandler::NotFound => Handler::not_found(),
                ValidatedHandler::PhpWorker {
                    executable,
                    script,
                    workers,
                    queue_capacity,
                    request_timeout,
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
                    let pool = Arc::new(PhpWorkerPool::new(PhpWorkerPoolConfig {
                        worker: PhpWorkerConfig {
                            executable,
                            script,
                            env,
                        },
                        label,
                        workers,
                        queue_capacity,
                        request_timeout,
                        max_requests,
                        max_worker_age,
                        body_budget: Arc::clone(&body_budget),
                    }));
                    php_pools.push(Arc::clone(&pool));
                    Handler::php_worker(pool)
                }
            };
            locations.push((location.path, handler));
        }
        let location_refs = locations
            .iter()
            .map(|(path, handler)| (path.as_str(), handler.clone()))
            .collect();
        let location_router = Arc::new(
            LocationRouter::new(location_refs, Handler::not_found())
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
    Ok(RuntimeConfig {
        listen: validated.listen,
        http_limits: validated.http_limits,
        router: Arc::new(router),
        php_pools,
    })
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
        assert_eq!(runtime.listen_address(), "127.0.0.1:8080".parse().unwrap());
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
