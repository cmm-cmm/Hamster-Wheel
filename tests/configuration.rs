use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use hamster_wheel::{
    config::{self, ConfigError},
    router::{Handler, HandlerKind},
    server,
};
use hyper::{HeaderMap, header::HOST};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hamster-wheel-config-test-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).expect("temporary directory should be removed");
    }
}

struct TestServer {
    address: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(runtime: config::RuntimeConfig) -> Self {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let router = runtime.router_arc();
        let http_limits = runtime.http_limits();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, http_limits, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Self {
            address,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    async fn request(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(self.address)
            .await
            .expect("client should connect");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("request should be written");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response should be read");
        response
    }

    async fn stop(mut self) {
        self.shutdown_tx
            .take()
            .expect("shutdown sender should exist")
            .send(())
            .expect("server should still be running");
        self.task
            .await
            .expect("server task should finish")
            .expect("server should stop cleanly");
    }
}

fn literal_path(path: &Path) -> String {
    format!("'{0}'", path.to_string_lossy().replace('\'', "''"))
}

fn config_with_root(root: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"example.test\"\naliases = [\"alias.test\"]\n\n[[hosts.locations]]\npath = \"/api\"\nhandler = \"not_found\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = {root}\n"
    )
}

fn not_found_config(hosts: &str) -> String {
    format!("[server]\nlisten = \"127.0.0.1:8080\"\n{hosts}")
}

/// A php_worker location whose `env` table is whatever the caller supplies.
/// The executable never has to exist: env is rejected during validation, which
/// runs before any path is canonicalized.
fn php_worker_config_with_env(env: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\nworkers = 1\n\n[hosts.locations.php.env]\n{env}"
    )
}

fn headers(host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, host.parse().unwrap());
    headers
}

#[test]
fn compiles_minimal_multiple_hosts_aliases_and_locations() {
    let source = not_found_config(
        "\n[[hosts]]\nname = \"one.test\"\naliases = [\"one-alias.test\"]\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n\n[[hosts]]\nname = \"two.test\"\n\n[[hosts.locations]]\npath = \"/nested\"\nhandler = \"not_found\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n",
    );
    let runtime = config::compile_str(&source, Path::new(".")).unwrap();
    assert_eq!(
        runtime
            .router()
            .route(&headers("ONE-ALIAS.TEST:80"), "/")
            .unwrap()
            .kind(),
        HandlerKind::Simple
    );
    assert!(
        runtime
            .router()
            .route(&headers("two.test"), "/nested/x")
            .is_ok()
    );
}

#[test]
fn no_arguments_selects_and_loads_the_development_config() {
    let selection = config::parse_arguments(Vec::<std::ffi::OsString>::new()).unwrap();
    let runtime = config::load(selection).unwrap();
    assert_eq!(
        runtime.listen_address(),
        Some("127.0.0.1:8080".parse().unwrap())
    );
    assert_eq!(
        runtime
            .router()
            .route(&headers("LOCALHOST:8080"), "/")
            .unwrap()
            .kind(),
        HandlerKind::Static
    );
}

#[test]
fn resolves_relative_root_from_config_directory_and_accepts_absolute_root() {
    let temporary = TestDirectory::new();
    let site = temporary.path.join("site");
    let public = site.join("public");
    std::fs::create_dir_all(&public).unwrap();
    std::fs::write(public.join("index.html"), "relative").unwrap();
    let relative_source = config_with_root("\"public\"");
    let config_path = site.join("hamster.toml");
    std::fs::write(&config_path, relative_source).unwrap();

    let relative = config::load_file(&config_path).unwrap();
    assert_eq!(
        relative
            .router()
            .route(&headers("example.test"), "/")
            .unwrap()
            .kind(),
        HandlerKind::Static
    );
    let absolute = config::compile_str(
        &config_with_root(&literal_path(&public)),
        Path::new("deliberately-unused"),
    )
    .unwrap();
    assert_eq!(
        absolute
            .router()
            .route(&headers("alias.test"), "/")
            .unwrap()
            .kind(),
        HandlerKind::Static
    );
}

#[tokio::test]
async fn serves_compiled_static_route_preserves_routing_and_keep_alive() {
    let temporary = TestDirectory::new();
    let public = temporary.path.join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::write(public.join("index.html"), "configured-static").unwrap();
    let runtime =
        config::compile_str(&config_with_root(&literal_path(&public)), &temporary.path).unwrap();
    let server = TestServer::start(runtime).await;

    let responses = server
        .request(
            "GET / HTTP/1.1\r\nHost: EXAMPLE.TEST:8080\r\n\r\n\
             GET /api/items HTTP/1.1\r\nHost: alias.test\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(responses.matches("HTTP/1.1 200 OK").count(), 1);
    assert_eq!(responses.matches("HTTP/1.1 404 Not Found").count(), 1);
    assert!(responses.contains("configured-staticHTTP/1.1 404 Not Found"));
    server.stop().await;
}

#[tokio::test]
async fn compiled_static_route_keeps_traversal_protection() {
    let temporary = TestDirectory::new();
    let public = temporary.path.join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::write(temporary.path.join("secret.txt"), "secret").unwrap();
    let runtime =
        config::compile_str(&config_with_root(&literal_path(&public)), &temporary.path).unwrap();
    let server = TestServer::start(runtime).await;
    let response = server
        .request(
            "GET /%2e%2e/secret.txt HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(!response.contains("\r\n\r\nsecret"));
    server.stop().await;
}

#[test]
fn rejects_unusable_php_worker_environment_names() {
    // An empty name or one carrying "=" cannot be expressed as a process
    // environment entry, so it is refused at the configuration boundary rather
    // than producing a worker that silently lacks the variable.
    for env in ["\"\" = \"value\"\n", "\"A=B\" = \"value\"\n"] {
        let error = config::compile_str(&php_worker_config_with_env(env), Path::new("."));
        assert!(
            matches!(error, Err(ConfigError::Validation(_))),
            "expected a validation error for env {env:?}"
        );
    }
}

#[test]
fn accepts_a_php_worker_environment_table() {
    // Valid env passes validation; compilation then fails later on the missing
    // executable, which is what proves env itself was not the objection.
    let error = config::compile_str(
        &php_worker_config_with_env("HAMSTER_LARAVEL_PATH = \"/srv/app\"\n"),
        Path::new("."),
    );
    match error {
        Err(ConfigError::Validation(message)) => {
            assert!(
                !message.contains("env"),
                "valid env was rejected: {message}"
            );
        }
        Err(_) => {}
        Ok(_) => panic!("a missing executable should not compile"),
    }
}

#[test]
fn rejects_malformed_toml_and_invalid_listen() {
    assert!(matches!(
        config::compile_str("[server", Path::new(".")),
        Err(ConfigError::Parse { .. })
    ));
    let invalid = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n",
    )
    .replace("127.0.0.1:8080", "localhost:8080");
    assert!(matches!(
        config::compile_str(&invalid, Path::new(".")),
        Err(ConfigError::Validation(_))
    ));
}

#[test]
fn rejects_duplicate_primary_alias_and_alias_primary_collisions() {
    for hosts in [
        "\n[[hosts]]\nname = \"same.test\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n\n[[hosts]]\nname = \"SAME.TEST\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n",
        "\n[[hosts]]\nname = \"one.test\"\naliases = [\"alias.test\", \"ALIAS.TEST\"]\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n",
        "\n[[hosts]]\nname = \"one.test\"\naliases = [\"two.test\"]\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n\n[[hosts]]\nname = \"TWO.TEST\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n",
    ] {
        let error = match config::compile_str(&not_found_config(hosts), Path::new(".")) {
            Ok(_) => panic!("hostname collision should fail"),
            Err(error) => error,
        };
        assert!(matches!(error, ConfigError::Validation(_)));
        assert!(error.to_string().contains("collides case-insensitively"));
    }
}

#[test]
fn rejects_empty_invalid_and_port_bearing_hostnames() {
    for hostname in ["", "-bad.test", "example.test:8080"] {
        let hosts = format!(
            "\n[[hosts]]\nname = \"{hostname}\"\nlocations = [{{ path = \"/\", handler = \"not_found\" }}]\n"
        );
        assert!(matches!(
            config::compile_str(&not_found_config(&hosts), Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
    }
}

#[test]
fn rejects_invalid_location_duplicate_location_and_unknown_handler() {
    for location in [
        "{ path = \"api\", handler = \"not_found\" }",
        "{ path = \"/\", handler = \"proxy\" }",
    ] {
        let source = not_found_config(&format!(
            "\n[[hosts]]\nname = \"localhost\"\nlocations = [{location}]\n"
        ));
        assert!(matches!(
            config::compile_str(&source, Path::new(".")),
            Err(ConfigError::Validation(_))
        ));
    }
    let duplicate = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/api\", handler = \"not_found\" }, { path = \"/api/\", handler = \"not_found\" }]\n",
    );
    assert!(matches!(
        config::compile_str(&duplicate, Path::new(".")),
        Err(ConfigError::Validation(_))
    ));
}

#[test]
fn rejects_missing_nonexistent_and_file_static_roots() {
    let temporary = TestDirectory::new();
    let file = temporary.path.join("file.txt");
    std::fs::write(&file, "not a directory").unwrap();
    let missing_field = config_with_root("\"placeholder\"").replace("root = \"placeholder\"\n", "");
    assert!(matches!(
        config::compile_str(&missing_field, &temporary.path),
        Err(ConfigError::Validation(_))
    ));
    assert!(matches!(
        config::compile_str(&config_with_root("\"missing\""), &temporary.path),
        Err(ConfigError::Compile(_))
    ));
    let error = match config::compile_str(&config_with_root(&literal_path(&file)), &temporary.path)
    {
        Ok(_) => panic!("file root should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, ConfigError::Compile(_)));
    assert!(error.to_string().contains("is not a directory"));
}

#[test]
fn compiles_one_php_worker_with_config_relative_files() {
    let temporary = TestDirectory::new();
    std::fs::write(temporary.path.join("php.exe"), "fixture executable").unwrap();
    std::fs::write(temporary.path.join("worker.php"), "<?php").unwrap();
    let source = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/php\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n",
    );
    let runtime = config::compile_str(&source, &temporary.path).unwrap();
    assert_eq!(
        runtime
            .router()
            .route(&headers("localhost"), "/php/state")
            .unwrap()
            .kind(),
        HandlerKind::PhpWorker
    );
    assert!(runtime.php_worker().is_some());
    let pool = runtime.php_worker().unwrap();
    assert_eq!(pool.worker_count(), 1);
    assert_eq!(pool.queue_capacity(), 8);
}

#[test]
fn validates_php_pool_worker_and_queue_bounds() {
    let temporary = TestDirectory::new();
    std::fs::write(temporary.path.join("php.exe"), "fixture executable").unwrap();
    std::fs::write(temporary.path.join("worker.php"), "<?php").unwrap();
    let base = "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/php\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n";

    for field in [
        "workers = 0",
        "workers = 65",
        "queue_capacity = 0",
        "queue_capacity = 129",
        "max_requests = 0",
        "max_worker_age = 0",
        // Below the floor the deadline could expire on Hamster's own protocol
        // I/O rather than on application execution.
        "request_timeout = 0",
        "request_timeout = 3601",
        "startup_timeout = 0",
        "startup_timeout = 301",
    ] {
        let source = not_found_config(&format!("{base}{field}\n"));
        assert!(matches!(
            config::compile_str(&source, &temporary.path),
            Err(ConfigError::Validation(_))
        ));
    }

    let source = not_found_config(&format!("{base}workers = 4\nqueue_capacity = 32\n"));
    let runtime = config::compile_str(&source, &temporary.path).unwrap();
    let pool = runtime.php_worker().unwrap();
    assert_eq!(pool.worker_count(), 4);
    assert_eq!(pool.queue_capacity(), 32);
    assert_eq!(pool.max_requests(), None);
    assert_eq!(pool.max_worker_age(), None);
    // Omitting it must keep the historical five seconds, so making the value
    // configurable does not silently change an existing deployment.
    assert_eq!(
        pool.request_timeout(),
        std::time::Duration::from_secs(5),
        "an unset request_timeout keeps the compatibility default"
    );
    assert_eq!(
        pool.startup_timeout(),
        std::time::Duration::from_secs(5),
        "an unset startup_timeout keeps the historical READY budget"
    );

    let source = not_found_config(&format!("{base}startup_timeout = 120\n"));
    let runtime = config::compile_str(&source, &temporary.path).unwrap();
    assert_eq!(
        runtime.php_worker().unwrap().startup_timeout(),
        std::time::Duration::from_secs(120)
    );

    let source = not_found_config(&format!(
        "{base}max_requests = 1000\nmax_worker_age = 3600\n"
    ));
    let runtime = config::compile_str(&source, &temporary.path).unwrap();
    let pool = runtime.php_worker().unwrap();
    assert_eq!(pool.max_requests(), Some(1000));
    assert_eq!(
        pool.max_worker_age(),
        Some(std::time::Duration::from_secs(3600))
    );

    let source = not_found_config(&format!("{base}workers = 64\n"));
    let runtime = config::compile_str(&source, &temporary.path).unwrap();
    assert_eq!(runtime.php_worker().unwrap().queue_capacity(), 128);
}

#[test]
fn rejects_missing_and_invalid_php_worker_configuration() {
    let temporary = TestDirectory::new();
    std::fs::write(temporary.path.join("php.exe"), "fixture executable").unwrap();
    std::fs::write(temporary.path.join("worker.php"), "<?php").unwrap();
    let missing_block = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/\", handler = \"php_worker\" }]\n",
    );
    assert!(matches!(
        config::compile_str(&missing_block, &temporary.path),
        Err(ConfigError::Validation(_))
    ));

    for (executable, script) in [
        ("missing.exe", "worker.php"),
        ("php.exe", "missing.php"),
        (".", "worker.php"),
        ("php.exe", "."),
    ] {
        let source = not_found_config(&format!(
            "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = \"{executable}\"\nscript = \"{script}\"\n"
        ));
        assert!(matches!(
            config::compile_str(&source, &temporary.path),
            Err(ConfigError::Compile(_))
        ));
    }
}

#[test]
fn compiles_multiple_php_worker_locations_as_distinct_pools() {
    let temporary = TestDirectory::new();
    std::fs::write(temporary.path.join("php.exe"), "fixture executable").unwrap();
    std::fs::write(temporary.path.join("worker.php"), "<?php").unwrap();
    let multiple = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/one\"\nhandler = \"php_worker\"\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n\n[[hosts.locations]]\npath = \"/two\"\nhandler = \"php_worker\"\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n",
    );
    let runtime = config::compile_str(&multiple, &temporary.path).unwrap();
    assert_eq!(runtime.php_workers().len(), 2);
    assert_eq!(runtime.php_workers()[0].label(), "localhost/one");
    assert_eq!(runtime.php_workers()[1].label(), "localhost/two");
    let Handler::PhpWorker { pool: first_route } = runtime
        .router()
        .route(&headers("localhost"), "/one/request")
        .unwrap()
    else {
        panic!("/one did not compile to a PHP pool");
    };
    let Handler::PhpWorker { pool: second_route } = runtime
        .router()
        .route(&headers("localhost"), "/two/request")
        .unwrap()
    else {
        panic!("/two did not compile to a PHP pool");
    };
    assert!(Arc::ptr_eq(first_route, &runtime.php_workers()[0]));
    assert!(Arc::ptr_eq(second_route, &runtime.php_workers()[1]));
    assert!(!Arc::ptr_eq(first_route, second_route));
}

#[test]
fn explicitly_selected_file_errors_do_not_fall_back() {
    let temporary = TestDirectory::new();
    let result = config::load(config::ConfigSelection::File(
        temporary.path.join("missing.toml"),
    ));
    assert!(matches!(result, Err(ConfigError::Io { .. })));

    let malformed = temporary.path.join("malformed.toml");
    std::fs::write(&malformed, "[server").unwrap();
    assert!(matches!(
        config::load(config::ConfigSelection::File(malformed)),
        Err(ConfigError::Parse { .. })
    ));

    let invalid = temporary.path.join("invalid.toml");
    std::fs::write(
        &invalid,
        not_found_config(
            "\n[[hosts]]\nname = \"bad host\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n",
        ),
    )
    .unwrap();
    assert!(matches!(
        config::load(config::ConfigSelection::File(invalid)),
        Err(ConfigError::Validation(_))
    ));
}

fn proxy_config(table: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/api\"\nhandler = \"proxy\"\n\n[hosts.locations.proxy]\n{table}\n"
    )
}

fn validation_message(source: &str) -> String {
    match config::compile_str(source, Path::new(".")) {
        Err(ConfigError::Validation(message)) => message,
        Err(other) => panic!("expected a validation error, got {other}"),
        Ok(_) => panic!("expected a validation error, got a compiled config"),
    }
}

#[test]
fn compiles_a_proxy_location_into_its_own_upstream() {
    let runtime = config::compile_str(
        &proxy_config(
            "upstream = \"http://127.0.0.1:3000\"\nconnect_timeout = 2\nresponse_header_timeout = 10\nprogress_timeout = 20\nidle_timeout = 3\nmax_idle = 0\npreserve_host = false",
        ),
        Path::new("."),
    )
    .unwrap();
    let upstreams = runtime.upstreams();
    assert_eq!(upstreams.len(), 1);
    assert_eq!(upstreams[0].label(), "localhost/api");
    assert_eq!(
        upstreams[0].address(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000)
    );
    assert_eq!(
        runtime
            .router()
            .route(&headers("localhost"), "/api/users")
            .map(Handler::kind),
        Ok(HandlerKind::Proxy)
    );

    // The defaults compile too, and port 80 is implied.
    let runtime = config::compile_str(
        &proxy_config("upstream = \"http://127.0.0.1\""),
        Path::new("."),
    )
    .unwrap();
    assert_eq!(runtime.upstreams()[0].address().port(), 80);
}

#[test]
fn rejects_proxy_upstreams_v1_cannot_honor() {
    for (upstream, reason) in [
        ("https://127.0.0.1:3000", "TLS upstreams are not supported"),
        ("127.0.0.1:3000", "absolute http://"),
        ("ftp://127.0.0.1:3000", "absolute http://"),
        ("http://127.0.0.1:3000/v1", "must not have a path"),
        (
            "http://127.0.0.1:3000/?a=b",
            "must not have a path or query",
        ),
        ("http://user:pass@127.0.0.1:3000", "user information"),
        ("http://127.0.0.1:0", "port 0"),
        ("http://127.0.0.1:bogus", "invalid port"),
        ("http://127.0.0.1:", "invalid port"),
        ("http://127.0.0.1:65536", "invalid port"),
        ("not a url", "is not a valid URL"),
    ] {
        let message = validation_message(&proxy_config(&format!("upstream = \"{upstream}\"")));
        assert!(message.contains(reason), "{upstream}: {message}");
        assert!(message.contains("locations[0].proxy.upstream"), "{message}");
    }
}

#[test]
fn rejects_out_of_range_proxy_limits() {
    for (field, value) in [
        ("connect_timeout", "0"),
        ("response_header_timeout", "3601"),
        ("progress_timeout", "0"),
        ("idle_timeout", "0"),
        ("max_idle", "4097"),
    ] {
        let message = validation_message(&proxy_config(&format!(
            "upstream = \"http://127.0.0.1:3000\"\n{field} = {value}"
        )));
        assert!(message.contains(&format!("proxy.{field}")), "{message}");
    }
}

#[test]
fn a_proxy_table_belongs_only_to_a_proxy_location() {
    let table = "\n[hosts.locations.proxy]\nupstream = \"http://127.0.0.1:3000\"\n";
    let static_with_proxy = format!(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n{table}"
    );
    assert!(validation_message(&static_with_proxy).contains(".proxy is not valid"));
    let missing = "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"proxy\"\n";
    assert!(validation_message(missing).contains(".proxy is required"));
    assert!(matches!(
        config::compile_str(
            &proxy_config("upstream = \"http://127.0.0.1:3000\"\npath_prefix = \"/v1\""),
            Path::new(".")
        ),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn an_unresolvable_upstream_fails_compilation() {
    // `.invalid` is reserved and never resolves, so this fails before any
    // listener could be bound.
    match config::compile_str(
        &proxy_config("upstream = \"http://upstream.invalid:3000\""),
        Path::new("."),
    ) {
        Err(ConfigError::Compile(message)) => {
            assert!(message.contains("cannot be resolved"), "{message}");
        }
        Err(other) => panic!("expected a compile error, got {other}"),
        Ok(_) => panic!("an unresolvable upstream compiled"),
    }
}

#[test]
fn a_later_invalid_proxy_location_aborts_the_whole_compile() {
    let config = format!(
        "{}\n[[hosts.locations]]\npath = \"/broken\"\nhandler = \"proxy\"\n\n[hosts.locations.proxy]\nupstream = \"http://upstream.invalid:3000\"\n",
        proxy_config("upstream = \"http://127.0.0.1:3000\"")
    );
    match config::compile_str(&config, Path::new(".")) {
        Err(ConfigError::Compile(message)) => {
            assert!(message.contains("locations[1].proxy.upstream"), "{message}");
            assert!(message.contains("upstream.invalid"), "{message}");
        }
        Err(other) => panic!("expected a compile error, got {other}"),
        Ok(_) => panic!("partial proxy configuration compiled"),
    }
}

#[tokio::test]
async fn a_compiled_proxy_location_forwards_to_its_upstream() {
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let origin = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1_024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).await.unwrap();
            request.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nupstream")
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    });
    let runtime = config::compile_str(
        &format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/api\"\nhandler = \"proxy\"\n\n[hosts.locations.proxy]\nupstream = \"http://{upstream_address}\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"not_found\"\n"
        ),
        Path::new("."),
    )
    .unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(server::serve_with_router(
        listener,
        runtime.router_arc(),
        runtime.http_limits(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            b"GET /api/items?page=2 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("upstream"), "{response}");
    let request = origin.await.unwrap();
    assert!(
        request.starts_with("GET /api/items?page=2 HTTP/1.1\r\n"),
        "the location prefix must be forwarded unchanged: {request}"
    );

    let _ = shutdown.send(());
    server.await.unwrap().unwrap();
}

/// Writes a self-signed certificate for `names` as `<file>.crt` and
/// `<file>.key` and returns the key PEM so a test can check it never leaks.
fn write_certificate(directory: &Path, file: &str, names: &[&str]) -> String {
    let generated = rcgen::generate_simple_self_signed(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("generate certificate");
    let key = generated.signing_key.serialize_pem();
    std::fs::write(directory.join(format!("{file}.crt")), generated.cert.pem())
        .expect("write certificate");
    std::fs::write(directory.join(format!("{file}.key")), &key).expect("write key");
    key
}

/// `[server.tls]` with the given certificate tables; certificate paths are
/// relative, so they resolve against the configuration directory.
fn tls_config(listen: Option<&str>, tls_extra: &str, certificates: &str) -> String {
    let listen = listen.map_or_else(String::new, |listen| format!("listen = \"{listen}\"\n"));
    format!(
        "[server]\n{listen}\n[server.tls]\nlisten = \"127.0.0.1:8443\"\n{tls_extra}\n{certificates}\n[[hosts]]\nname = \"localhost\"\nlocations = [{{ path = \"/\", handler = \"not_found\" }}]\n"
    )
}

fn certificate_table(file: &str, names: &str, extra: &str) -> String {
    format!(
        "[[server.tls.certificates]]\ncert = \"{file}.crt\"\nkey = \"{file}.key\"\nnames = {names}\n{extra}\n"
    )
}

#[test]
fn compiles_a_tls_listener_with_or_without_a_cleartext_listener() {
    let directory = TestDirectory::new();
    write_certificate(&directory.path, "site", &["localhost", "*.example.test"]);
    let certificates = certificate_table(
        "site",
        "[\"localhost\", \"*.example.test\"]",
        "default = true",
    );

    let both = config::compile_str(
        &tls_config(
            Some("127.0.0.1:8080"),
            "handshake_timeout = 7",
            &certificates,
        ),
        &directory.path,
    )
    .expect("TLS plus cleartext should compile");
    assert_eq!(
        both.listen_address(),
        Some("127.0.0.1:8080".parse().unwrap())
    );
    let tls = both.tls().expect("TLS listener");
    assert_eq!(tls.address(), "127.0.0.1:8443".parse().unwrap());
    assert_eq!(tls.handshake_timeout(), std::time::Duration::from_secs(7));
    assert_eq!(tls.certificates().len(), 1);

    let only = config::compile_str(&tls_config(None, "", &certificates), &directory.path)
        .expect("a TLS-only server should compile");
    assert!(only.listen_address().is_none());
    assert_eq!(
        only.tls().unwrap().handshake_timeout(),
        std::time::Duration::from_secs(10),
        "the documented default"
    );
}

#[test]
fn rejects_tls_configuration_that_cannot_be_served() {
    let directory = TestDirectory::new();
    let base = directory.path.as_path();
    let key_pem = write_certificate(base, "site", &["localhost"]);
    write_certificate(base, "other", &["other.test"]);
    write_certificate(base, "explicit", &["a.example.test"]);
    std::fs::write(
        base.join("garbage.crt"),
        "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    std::fs::write(base.join("empty.crt"), "no PEM here\n").unwrap();
    std::fs::write(
        base.join("encrypted.key"),
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIBvTBXBgkqhkiG9w0BBQ0wSjApBgkqhkiG9w0BBQwwHAQI\n-----END ENCRYPTED PRIVATE KEY-----\n",
    )
    .unwrap();
    let site = |names: &str, extra: &str| certificate_table("site", names, extra);

    for (why, source, expected) in [
        (
            "no listener at all",
            "[server]\n[[hosts]]\nname = \"localhost\"\nlocations = [{ path = \"/\", handler = \"not_found\" }]\n".to_owned(),
            "server.listen or server.tls must be configured",
        ),
        (
            "overlapping listeners",
            tls_config(Some("0.0.0.0:8443"), "", &site("[\"localhost\"]", "")),
            "overlap",
        ),
        (
            "non-numeric TLS listen",
            tls_config(None, "", &site("[\"localhost\"]", "")).replace("127.0.0.1:8443", "localhost:8443"),
            "server.tls.listen",
        ),
        (
            "zero handshake timeout",
            tls_config(None, "handshake_timeout = 0", &site("[\"localhost\"]", "")),
            "server.tls.handshake_timeout must be between 1 and 60 seconds",
        ),
        (
            "excessive handshake timeout",
            tls_config(None, "handshake_timeout = 61", &site("[\"localhost\"]", "")),
            "server.tls.handshake_timeout",
        ),
        (
            "no certificates",
            tls_config(None, "certificates = []", ""),
            "at least one certificate",
        ),
        (
            "no names",
            tls_config(None, "", &site("[]", "")),
            "names must contain at least one name",
        ),
        (
            "an IP address name the certificate does not cover",
            tls_config(None, "", &site("[\"127.0.0.1\"]", "")),
            "names 127.0.0.1 is not covered",
        ),
        (
            "a wildcard over an IP address",
            tls_config(None, "", &site("[\"*.127.0.0.1\"]", "")),
            "is a wildcard over an IP address",
        ),
        (
            "a top-level wildcard",
            tls_config(None, "", &site("[\"*.test\"]", "")),
            "wildcard over a top-level domain",
        ),
        (
            "a mid-label wildcard",
            tls_config(None, "", &site("[\"a.*.test\"]", "")),
            "is not a DNS name",
        ),
        (
            "a name configured twice",
            tls_config(
                None,
                "",
                &format!(
                    "{}{}",
                    site("[\"localhost\"]", ""),
                    certificate_table("other", "[\"LOCALHOST\"]", "")
                ),
            ),
            "duplicates server.tls.certificates[0].names[0]",
        ),
        (
            "two defaults",
            tls_config(
                None,
                "",
                &format!(
                    "{}{}",
                    site("[\"localhost\"]", "default = true"),
                    certificate_table("other", "[\"other.test\"]", "default = true")
                ),
            ),
            "at most one certificate is the default",
        ),
        (
            "an unknown field",
            tls_config(None, "", &site("[\"localhost\"]", "password = \"x\"")),
            "unknown field",
        ),
        (
            "a missing certificate file",
            tls_config(None, "", &certificate_table("missing", "[\"localhost\"]", "")),
            "server.tls.certificates[0].cert",
        ),
        (
            "malformed certificate PEM",
            tls_config(
                None,
                "",
                "[[server.tls.certificates]]\ncert = \"garbage.crt\"\nkey = \"site.key\"\nnames = [\"localhost\"]\n",
            ),
            "is not valid PEM",
        ),
        (
            "a certificate file without a certificate",
            tls_config(
                None,
                "",
                "[[server.tls.certificates]]\ncert = \"empty.crt\"\nkey = \"site.key\"\nnames = [\"localhost\"]\n",
            ),
            "contains no CERTIFICATE block",
        ),
        (
            "a key file without a key",
            tls_config(
                None,
                "",
                "[[server.tls.certificates]]\ncert = \"site.crt\"\nkey = \"site.crt\"\nnames = [\"localhost\"]\n",
            ),
            "contains no readable PKCS#8, PKCS#1 or SEC1 private key",
        ),
        (
            "an encrypted key",
            tls_config(
                None,
                "",
                "[[server.tls.certificates]]\ncert = \"site.crt\"\nkey = \"encrypted.key\"\nnames = [\"localhost\"]\n",
            ),
            "is encrypted",
        ),
        (
            "a key that does not match the certificate",
            tls_config(
                None,
                "",
                "[[server.tls.certificates]]\ncert = \"site.crt\"\nkey = \"other.key\"\nnames = [\"localhost\"]\n",
            ),
            "cannot be used together",
        ),
        (
            "a name the certificate does not cover",
            tls_config(None, "", &site("[\"localhost\", \"www.localhost\"]", "")),
            "names www.localhost is not covered",
        ),
        (
            "a wildcard over explicit names only",
            tls_config(None, "", &certificate_table("explicit", "[\"*.example.test\"]", "")),
            "names *.example.test is not covered",
        ),
    ] {
        let message = match config::compile_str(&source, base) {
            Err(ConfigError::Parse { source, .. }) => source.to_string(),
            Err(ConfigError::Compile(message) | ConfigError::Validation(message)) => message,
            Err(other) => panic!("{why}: unexpected error {other}"),
            Ok(_) => panic!("{why}: must not compile"),
        };
        assert!(message.contains(expected), "{why}: {message}");
        // Nothing from a private key belongs in a diagnostic.
        let key_body = key_pem.lines().nth(1).expect("key body line");
        assert!(!message.contains(key_body), "{why} leaked key material");
    }
}

/// A CA (or intermediate) that signs `localhost` leaves, with explicit dates.
struct DatedCa {
    issuer: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
}

fn dated_ca_params(common_name: &str, not_before: i32, not_after: i32) -> rcgen::CertificateParams {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, common_name);
    params.not_before = rcgen::date_time_ymd(not_before, 1, 1);
    params.not_after = rcgen::date_time_ymd(not_after, 1, 1);
    params
}

impl DatedCa {
    fn root(common_name: &str, not_before: i32, not_after: i32) -> Self {
        Self {
            issuer: rcgen::CertifiedIssuer::self_signed(
                dated_ca_params(common_name, not_before, not_after),
                rcgen::KeyPair::generate().unwrap(),
            )
            .unwrap(),
        }
    }

    fn intermediate(&self, common_name: &str, not_before: i32, not_after: i32) -> Self {
        Self {
            issuer: rcgen::CertifiedIssuer::signed_by(
                dated_ca_params(common_name, not_before, not_after),
                rcgen::KeyPair::generate().unwrap(),
                &self.issuer,
            )
            .unwrap(),
        }
    }

    /// Writes `<file>.key` and returns the leaf PEM for `localhost`.
    fn leaf(&self, directory: &Path, file: &str, not_before: i32, not_after: i32) -> String {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(not_before, 1, 1);
        params.not_after = rcgen::date_time_ymd(not_after, 1, 1);
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, &self.issuer).unwrap();
        std::fs::write(directory.join(format!("{file}.key")), key.serialize_pem()).unwrap();
        certificate.pem()
    }

    fn pem(&self) -> String {
        self.issuer.pem()
    }
}

/// Writes `<file>.crt` from `chain` and compiles a TLS-only config using it
/// and `<file>.key`.
fn compile_tls(
    directory: &Path,
    file: &str,
    chain: &[String],
) -> Result<config::RuntimeConfig, String> {
    std::fs::write(directory.join(format!("{file}.crt")), chain.concat()).unwrap();
    let source = tls_config(None, "", &certificate_table(file, "[\"localhost\"]", ""));
    config::compile_str(&source, directory).map_err(|error| match error {
        ConfigError::Compile(message) | ConfigError::Validation(message) => message,
        other => panic!("unexpected error kind: {other}"),
    })
}

#[test]
fn a_certificate_outside_its_validity_period_fails_startup() {
    let directory = TestDirectory::new();
    let base = directory.path.as_path();
    let ca = DatedCa::root("Hamster Dated CA", 2000, 2090);

    let expired = ca.leaf(base, "expired", 2010, 2011);
    let message = compile_tls(base, "expired", &[expired, ca.pem()])
        .err()
        .expect("must not compile");
    assert!(
        message.contains("the leaf certificate expired on 2011-01-01 00:00:00 UTC"),
        "{message}"
    );

    let future = ca.leaf(base, "future", 2080, 2085);
    let message = compile_tls(base, "future", &[future, ca.pem()])
        .err()
        .expect("must not compile");
    assert!(
        message.contains("the leaf certificate is not valid until 2080-01-01 00:00:00 UTC"),
        "{message}"
    );

    // A self-signed leaf alone is judged by its own dates too.
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    params.not_before = rcgen::date_time_ymd(2011, 1, 1);
    params.not_after = rcgen::date_time_ymd(2012, 1, 1);
    let key = rcgen::KeyPair::generate().unwrap();
    let self_signed = params.self_signed(&key).unwrap();
    std::fs::write(base.join("selfsigned.key"), key.serialize_pem()).unwrap();
    let message = compile_tls(base, "selfsigned", &[self_signed.pem()])
        .err()
        .expect("must not compile");
    assert!(message.contains("expired on 2012-01-01"), "{message}");

    // An expired intermediate fails even though the leaf is current.
    let stale = ca.intermediate("Hamster Stale Intermediate", 2000, 2012);
    let leaf = stale.leaf(base, "stale", 2001, 2090);
    let message = compile_tls(base, "stale", &[leaf, stale.pem(), ca.pem()])
        .err()
        .expect("must not compile");
    assert!(
        message.contains("chain certificate 2 expired on 2012-01-01"),
        "{message}"
    );

    // A trailing self-signed root is not date-checked: clients use their
    // own copy of a root, so a bundled expired one decides nothing.
    let old_root = DatedCa::root("Hamster Old Root", 2000, 2012);
    let intermediate = old_root.intermediate("Hamster Current Intermediate", 2000, 2090);
    let leaf = intermediate.leaf(base, "oldroot", 2001, 2090);
    compile_tls(base, "oldroot", &[leaf, intermediate.pem(), old_root.pem()])
        .expect("an expired trailing root is not the server's to judge");
}

#[test]
fn a_chain_that_does_not_link_fails_startup_and_a_bare_leaf_is_reported() {
    let directory = TestDirectory::new();
    let base = directory.path.as_path();
    let ca = DatedCa::root("Hamster Chain CA", 2000, 2090);
    let other = DatedCa::root("Hamster Other CA", 2000, 2090);
    let intermediate = ca.intermediate("Hamster Intermediate", 2000, 2090);
    let leaf = intermediate.leaf(base, "linked", 2001, 2090);
    for file in ["noroot", "wrong", "gap", "bare"] {
        std::fs::copy(base.join("linked.key"), base.join(format!("{file}.key"))).unwrap();
    }

    // Linked: leaf, intermediate, root; and the usual form without the root.
    let runtime = compile_tls(
        base,
        "linked",
        &[leaf.clone(), intermediate.pem(), ca.pem()],
    )
    .expect("a linked chain compiles");
    let report = &runtime.tls().unwrap().certificates()[0];
    assert!(report.note.is_none());
    assert_eq!(report.not_after, "2090-01-01 00:00:00 UTC");
    assert!(report.days_left > 20_000, "{}", report.days_left);
    compile_tls(base, "noroot", &[leaf.clone(), intermediate.pem()]).expect("root omitted");

    // An unrelated issuer after the leaf.
    let message = compile_tls(base, "wrong", &[leaf.clone(), other.pem()])
        .err()
        .expect("must not compile");
    assert!(
        message.contains("the chain does not link the leaf to the last certificate"),
        "{message}"
    );

    // The intermediate missing between leaf and root.
    let message = compile_tls(base, "gap", &[leaf.clone(), ca.pem()])
        .err()
        .expect("must not compile");
    assert!(message.contains("does not link"), "{message}");

    // A leaf alone whose issuer is not in the file: whether clients hold
    // that issuer is theirs to know, so the server starts and says so.
    let runtime = compile_tls(base, "bare", &[leaf]).expect("a bare leaf still compiles");
    let note = runtime.tls().unwrap().certificates()[0]
        .note
        .expect("a bare leaf is reported");
    assert!(note.contains("only the leaf"), "{note}");

    // A self-signed leaf alone has no missing issuer.
    write_certificate(base, "self", &["localhost"]);
    let source = tls_config(None, "", &certificate_table("self", "[\"localhost\"]", ""));
    let runtime = config::compile_str(&source, base).expect("self-signed");
    assert!(runtime.tls().unwrap().certificates()[0].note.is_none());
}

#[test]
fn a_bare_leaf_with_client_auth_only_eku_fails_startup() {
    use rcgen::ExtendedKeyUsagePurpose;

    let directory = TestDirectory::new();
    let base = directory.path.as_path();
    let ca = DatedCa::root("Hamster EKU CA", 2000, 2090);
    for (file, usages, bundled, accepted) in [
        (
            "client",
            vec![ExtendedKeyUsagePurpose::ClientAuth],
            false,
            false,
        ),
        (
            "client-chain",
            vec![ExtendedKeyUsagePurpose::ClientAuth],
            true,
            false,
        ),
        (
            "server",
            vec![ExtendedKeyUsagePurpose::ServerAuth],
            false,
            true,
        ),
        (
            "both",
            vec![
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::ServerAuth,
            ],
            false,
            true,
        ),
        ("absent", vec![], false, true),
    ] {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2090, 1, 1);
        params.extended_key_usages = usages;
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &ca.issuer).unwrap();
        std::fs::write(base.join(format!("{file}.key")), key.serialize_pem()).unwrap();
        let chain = if bundled {
            vec![cert.pem(), ca.pem()]
        } else {
            vec![cert.pem()]
        };
        let result = compile_tls(base, file, &chain);
        if accepted {
            let runtime = result.expect(file);
            assert!(runtime.tls().unwrap().certificates()[0].note.is_some());
        } else {
            let error = result.err().expect("client-auth-only leaf must fail");
            assert!(
                error.contains("extended key usage does not permit server authentication"),
                "{error}"
            );
        }
    }
}

#[test]
fn a_non_ca_issuer_at_the_end_of_a_chain_fails_startup() {
    let directory = TestDirectory::new();
    let base = directory.path.as_path();
    let params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    let issuer =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    std::fs::write(base.join("nonca.key"), key.serialize_pem()).unwrap();
    let error = compile_tls(base, "nonca", &[leaf.pem(), issuer.pem()])
        .err()
        .expect("a non-CA issuer must not validate a chain");
    assert!(error.contains("not a CA"), "{error}");
}

/// A `fastcgi` location: its root and servers are required, its addresses are
/// numeric, unique and have ports, its index is a `.php` path inside the root,
/// its timeouts are bounded, and its block is refused on any other handler.
#[test]
fn validates_and_compiles_fastcgi_locations() {
    let temporary = TestDirectory::new();
    std::fs::create_dir(temporary.path.join("public")).unwrap();
    let location = |fields: &str, block: &str| {
        not_found_config(&format!(
            "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"fastcgi\"\n{fields}\n{block}"
        ))
    };
    let block = |fields: &str| {
        format!("[hosts.locations.fastcgi]\nservers = [\"127.0.0.1:9000\"]\n{fields}\n")
    };

    let runtime = config::compile_str(&location("root = \"public\"", &block("")), &temporary.path)
        .expect("a minimal fastcgi location compiles");
    assert_eq!(
        runtime
            .router()
            .route(&headers("localhost"), "/index.php")
            .unwrap()
            .kind(),
        HandlerKind::FastCgi
    );
    let backend = &runtime.fastcgi_backends()[0];
    assert_eq!(backend.label(), "localhost/");
    assert_eq!(backend.servers(), ["127.0.0.1:9000".parse().unwrap()]);

    let full = block(
        "concurrency = 4\nindex = \"app/front.php\"\nconnect_timeout = 2\nresponse_timeout = 120\nkeep_connections = false\nresolve_cache = 60",
    );
    config::compile_str(
        &location("root = \"public\"", &block("resolve_cache = 0")),
        &temporary.path,
    )
    .expect("a zero resolve_cache turns the cache off");
    config::compile_str(&location("root = \"public\"", &full), &temporary.path)
        .expect("every field in range compiles");

    for (fields, block_fields, servers) in [
        ("", "", None),
        ("root = \"public\"", "", Some("")),
        ("root = \"public\"", "", Some("\"localhost:9000\"")),
        ("root = \"public\"", "", Some("\"127.0.0.1:0\"")),
        (
            "root = \"public\"",
            "",
            Some("\"127.0.0.1:9000\", \"127.0.0.1:9000\""),
        ),
        ("root = \"public\"", "concurrency = 0", None),
        ("root = \"public\"", "concurrency = 257", None),
        ("root = \"public\"", "index = \"../index.php\"", None),
        ("root = \"public\"", "index = \"/index.php\"", None),
        ("root = \"public\"", "index = \"index.html\"", None),
        ("root = \"public\"", "index = \"c:index.php\"", None),
        ("root = \"public\"", "connect_timeout = 0", None),
        ("root = \"public\"", "response_timeout = 3601", None),
        ("root = \"public\"", "resolve_cache = 61", None),
        (
            "root = \"public\"\nphp = { executable = \"php.exe\", script = \"w.php\" }",
            "",
            None,
        ),
    ] {
        let block = match servers {
            Some(servers) => {
                format!("[hosts.locations.fastcgi]\nservers = [{servers}]\n{block_fields}\n")
            }
            None => block(block_fields),
        };
        let result = config::compile_str(&location(fields, &block), &temporary.path);
        assert!(
            matches!(result, Err(ConfigError::Validation(_))),
            "{fields} / {block}: {:?}",
            result.err()
        );
    }
    // The block is required, and not accepted on another handler.
    let missing = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"fastcgi\"\nroot = \"public\"\n",
    );
    assert!(matches!(
        config::compile_str(&missing, &temporary.path),
        Err(ConfigError::Validation(_))
    ));
    let misplaced = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"public\"\n\n[hosts.locations.fastcgi]\nservers = [\"127.0.0.1:9000\"]\n",
    );
    assert!(matches!(
        config::compile_str(&misplaced, &temporary.path),
        Err(ConfigError::Validation(_))
    ));
    // A root that does not exist is a compile error, before anything binds.
    let absent = location("root = \"absent\"", &block(""));
    assert!(matches!(
        config::compile_str(&absent, &temporary.path),
        Err(ConfigError::Compile(_))
    ));
}

#[test]
fn validates_host_file_denials_before_runtime_start() {
    let temporary = TestDirectory::new();
    std::fs::create_dir(temporary.path.join("public")).unwrap();
    let source = |rules: &str| {
        not_found_config(&format!(
            "\n[[hosts]]\nname = 'localhost'\n{rules}\n[[hosts.locations]]\npath = '/'\nhandler = 'static'\nroot = 'public'\n"
        ))
    };
    let valid = source("deny_paths = ['/data', '/conf/private']\ndeny_names = ['composer.json']");
    let runtime = config::compile_str(&valid, &temporary.path).unwrap();
    for path in [
        "/data/a.txt",
        "/DATA/a.txt",
        "/%64ata/a.txt",
        "/x/composer.json",
    ] {
        assert!(
            runtime.router().route(&headers("localhost"), path).is_err(),
            "{path} must not reach a handler"
        );
    }
    assert!(
        runtime
            .router()
            .route(&headers("localhost"), "/database/a.txt")
            .is_ok()
    );
    for rules in [
        "deny_paths = ['/']",
        "deny_paths = ['data']",
        "deny_paths = ['/data/']",
        "deny_paths = ['/data//pages']",
        "deny_paths = ['/data/../conf']",
        "deny_paths = ['/data', '/DATA']",
        "deny_names = ['foo/bar']",
        "deny_names = ['composer.json', 'COMPOSER.JSON']",
        "deny_names = ['file%2eenv']",
    ] {
        assert!(
            matches!(
                config::compile_str(&source(rules), &temporary.path),
                Err(ConfigError::Validation(_))
            ),
            "invalid access rule should fail startup: {rules}"
        );
    }
}
