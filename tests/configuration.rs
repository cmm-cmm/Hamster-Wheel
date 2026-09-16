use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hamster_wheel::{
    config::{self, ConfigError},
    router::HandlerKind,
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
    assert_eq!(runtime.listen_address(), "127.0.0.1:8080".parse().unwrap());
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
fn rejects_missing_invalid_and_multiple_php_worker_configuration() {
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

    let multiple = not_found_config(
        "\n[[hosts]]\nname = \"localhost\"\n\n[[hosts.locations]]\npath = \"/one\"\nhandler = \"php_worker\"\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n\n[[hosts.locations]]\npath = \"/two\"\nhandler = \"php_worker\"\n[hosts.locations.php]\nexecutable = \"php.exe\"\nscript = \"worker.php\"\n",
    );
    assert!(matches!(
        config::compile_str(&multiple, &temporary.path),
        Err(ConfigError::Validation(_))
    ));
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
