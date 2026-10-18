//! Independent PHP applications behind Hamster, each in its own pool, alone and
//! side by side.
//!
//! The applications differ in how they are built and how they reach Hamster:
//!
//! | Application | Origin | Adapter |
//! |---|---|---|
//! | symfony/symfony-demo 3.1.0 | external, unmodified | `symfony_worker.php` (HttpKernel) |
//! | slim/slim-skeleton 4.5.0 | external, unmodified | `psr15_worker.php` + `entries/slim_skeleton.php` |
//! | `fixtures/php-apps/native` | simulated, this repository | speaks the protocol itself |
//! | Laravel stand-in (optional) | simulated, `prepare_scratch_app.ps1` | `laravel_worker.php` (Octane) |
//!
//! Every test is ignored: it needs `HAMSTER_PHP_APPS`, the workspace built by
//! `fixtures/php-apps/prepare_php_apps.ps1`, and the repository PHP. Setting
//! `HAMSTER_LARAVEL_TEST_PATH` adds the Laravel stand-in to the side-by-side
//! test. See `doc/PHP_APPLICATION_COMPATIBILITY.md`.

mod support;

use support::PHP_RELATIVE_PATH;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use hamster_wheel::{
    config::{self, PhpPoolsError},
    php_worker::{PhpWorkerPool, PoolSnapshot},
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

const SYMFONY: &str = "symfony.test";
const SLIM: &str = "slim.test";
const NATIVE: &str = "native.test";
const PROBE: &str = "probe.test";
const LARAVEL: &str = "laravel.test";
const POST_SLUG: &str = "lorem-ipsum-dolor-sit-amet-consectetur-adipiscing-elit";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace() -> PathBuf {
    let path = PathBuf::from(std::env::var_os("HAMSTER_PHP_APPS").expect(
        "HAMSTER_PHP_APPS must name the workspace built by fixtures/php-apps/prepare_php_apps.ps1",
    ));
    for required in ["php.ini", "symfony-demo", "slim-skeleton", "native"] {
        assert!(
            path.join(required).exists(),
            "HAMSTER_PHP_APPS is missing {required}"
        );
    }
    path
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[derive(Clone)]
enum App {
    Symfony,
    Slim,
    Native,
    NativeSessionProbe,
    Laravel(PathBuf),
}

struct Pool {
    host: &'static str,
    app: App,
    workers: usize,
}

fn host_section(apps: &Path, pool: &Pool) -> String {
    let php = repository().join(PHP_RELATIVE_PATH);
    let adapters = repository().join("fixtures/php-worker");
    let ini = apps.join("php.ini");
    let (script, mut env, static_root) = match &pool.app {
        App::Symfony => (
            adapters.join("symfony_worker.php"),
            vec![
                ("PHPRC", toml_literal(&ini)),
                (
                    "HAMSTER_SYMFONY_PATH",
                    toml_literal(&apps.join("symfony-demo")),
                ),
                ("APP_ENV", "'prod'".to_owned()),
                ("APP_DEBUG", "'0'".to_owned()),
            ],
            Some(apps.join("symfony-demo/public")),
        ),
        App::Slim => (
            adapters.join("psr15_worker.php"),
            vec![
                ("PHPRC", toml_literal(&ini)),
                (
                    "HAMSTER_PSR15_ENTRY",
                    toml_literal(&adapters.join("entries/slim_skeleton.php")),
                ),
                (
                    "HAMSTER_APP_PATH",
                    toml_literal(&apps.join("slim-skeleton")),
                ),
            ],
            None,
        ),
        App::NativeSessionProbe => (
            adapters.join("psr15_worker.php"),
            vec![
                ("PHPRC", toml_literal(&ini)),
                (
                    "HAMSTER_PSR15_ENTRY",
                    toml_literal(&adapters.join("entries/native_session_probe.php")),
                ),
                (
                    "HAMSTER_APP_PATH",
                    toml_literal(&apps.join("slim-skeleton")),
                ),
            ],
            None,
        ),
        App::Native => (
            apps.join("native/worker.php"),
            vec![
                ("PHPRC", toml_literal(&ini)),
                ("HAMSTER_ADAPTER_DIR", toml_literal(&adapters)),
            ],
            None,
        ),
        App::Laravel(path) => (
            adapters.join("laravel_worker.php"),
            vec![
                ("PHPRC", toml_literal(&path.join("php.ini"))),
                ("HAMSTER_LARAVEL_PATH", toml_literal(path)),
                ("HAMSTER_PMS_PROBE", "'1'".to_owned()),
            ],
            None,
        ),
    };
    env.sort();
    let mut section = format!("\n[[hosts]]\nname = \"{}\"\n", pool.host);
    if let Some(root) = static_root {
        // Hamster serves the compiled assets itself, beside the application.
        section.push_str(&format!(
            "\n[[hosts.locations]]\npath = \"/assets\"\nhandler = \"static\"\nroot = {}\n",
            toml_literal(&root)
        ));
    }
    section.push_str(&format!(
        "\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {}\nqueue_capacity = 32\nrequest_timeout = 30\n\n[hosts.locations.php.env]\n",
        toml_literal(&php),
        toml_literal(&script),
        pool.workers
    ));
    for (name, value) in env {
        section.push_str(&format!("{name} = {value}\n"));
    }
    section
}

struct AppServer {
    address: SocketAddr,
    runtime: Arc<config::RuntimeConfig>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl AppServer {
    async fn start(pools: &[Pool]) -> Self {
        let apps = workspace();
        let mut source = "[server]\nlisten = \"127.0.0.1:0\"\n".to_owned();
        for pool in pools {
            source.push_str(&host_section(&apps, pool));
        }
        let runtime = Arc::new(
            config::compile_str(&source, &repository())
                .unwrap_or_else(|error| panic!("configuration must compile: {error}\n{source}")),
        );
        runtime
            .start_workers()
            .await
            .unwrap_or_else(|error| panic!("every application pool must start: {error}"));
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let router = runtime.router_arc();
        let limits = runtime.http_limits();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, limits, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Self {
            address,
            runtime,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    fn pool(&self, host: &str) -> &Arc<PhpWorkerPool> {
        let label = format!("{host}/");
        self.runtime
            .php_workers()
            .iter()
            .find(|pool| pool.label() == label)
            .unwrap_or_else(|| panic!("no pool {label}"))
    }

    fn client(&self, host: &str) -> Client {
        let headers = if host == NATIVE {
            vec![("Accept".to_owned(), "text/plain".to_owned())]
        } else {
            Vec::new()
        };
        Client {
            address: self.address,
            authority: format!("{host}:{}", self.address.port()),
            cookies: HashMap::new(),
            headers,
        }
    }

    /// Stops accepting, drains connections, then shuts every pool down.
    async fn stop(mut self) -> Result<(), PhpPoolsError> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        let _ = (&mut self.task).await;
        self.runtime.shutdown_workers().await
    }
}

#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The native application's flat `key=value` rendering (`Accept: text/plain`).
    fn fields(&self) -> HashMap<String, String> {
        assert!(
            self.header("content-type")
                .is_some_and(|value| value.starts_with("text/plain")),
            "{self:?}"
        );
        self.text()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }
}

/// A minimal HTTP/1.1 client with a cookie jar, one connection per request.
/// The authority always carries the port, as a browser's does; Symfony's
/// same-origin CSRF check compares it with the Origin header.
struct Client {
    address: SocketAddr,
    authority: String,
    cookies: HashMap<String, String>,
    /// Sent with every request.
    headers: Vec<(String, String)>,
}

impl Client {
    async fn get(&mut self, path: &str) -> Response {
        self.send("GET", path, &[], &[]).await
    }

    async fn post_form(&mut self, path: &str, fields: &[(&str, &str)]) -> Response {
        let body = fields
            .iter()
            .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let origin = format!("http://{}", self.authority);
        self.send(
            "POST",
            path,
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Origin", &origin),
            ],
            body.as_bytes(),
        )
        .await
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.authority
        );
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            head.push_str(&format!("Cookie: {jar}\r\n"));
        }
        for (name, value) in headers
            .iter()
            .copied()
            .chain(self.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        let declared = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
        if !declared && (!body.is_empty() || method == "POST") {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        let response = exchange(self.address, &raw).await;
        for value in response.all("set-cookie") {
            let pair = value.split(';').next().unwrap_or_default();
            if let Some((name, cookie)) = pair.split_once('=') {
                let expired = value.to_ascii_lowercase().contains("max-age=0");
                if expired || cookie == "deleted" {
                    self.cookies.remove(name.trim());
                } else {
                    self.cookies
                        .insert(name.trim().to_owned(), cookie.trim().to_owned());
                }
            }
        }
        response
    }
}

async fn exchange(address: SocketAddr, raw: &[u8]) -> Response {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(raw).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .expect("a response within sixty seconds")
        .unwrap();
    parse_response(&bytes)
}

fn parse_response(bytes: &[u8]) -> Response {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response head in {} bytes", bytes.len()));
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let mut body = bytes[end + 4..].to_vec();
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = dechunk(&body);
    }
    Response {
        status,
        headers,
        body,
    }
}

fn dechunk(mut bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let line = bytes.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&bytes[..line])
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .unwrap();
        bytes = &bytes[line + 2..];
        if size == 0 {
            return body;
        }
        body.extend_from_slice(&bytes[..size]);
        bytes = &bytes[size + 2..];
    }
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn wait_for(pool: &PhpWorkerPool, predicate: impl Fn(&PoolSnapshot) -> bool) -> PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = pool.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "pool did not reach the expected state: {snapshot:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn pids(snapshot: &PoolSnapshot) -> Vec<u32> {
    snapshot
        .workers
        .iter()
        .filter_map(|worker| worker.pid)
        .collect()
}

/// A real application's full request cycle: front-controller routing, a static
/// asset served by Hamster beside it, a form login with session cookie and
/// same-origin CSRF, an authorization decision, and a form POST that writes to
/// the application's database and is visible on the next read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn symfony_demo_logs_in_comments_and_enforces_access_through_its_own_session() {
    let server = AppServer::start(&[Pool {
        host: SYMFONY,
        app: App::Symfony,
        workers: 2,
    }])
    .await;
    let mut visitor = server.client(SYMFONY);

    let blog = visitor.get("/en/blog/").await;
    assert_eq!(blog.status, 200, "{}", blog.text());
    assert!(
        blog.text().contains(POST_SLUG),
        "the blog index lists posts"
    );
    let asset = blog
        .text()
        .split('"')
        .find(|piece| piece.starts_with("/assets/") && piece.ends_with(".css"))
        .map(str::to_owned)
        .expect("the page links a compiled stylesheet");
    let stylesheet = visitor.get(&asset).await;
    assert_eq!(stylesheet.status, 200, "{asset}");
    assert!(
        stylesheet
            .header("content-type")
            .is_some_and(|value| value.starts_with("text/css")),
        "{:?}",
        stylesheet.headers
    );

    let missing = visitor.get("/en/blog/posts/no-such-post").await;
    assert_eq!(missing.status, 404, "the application answers its own 404");

    let admin = visitor.get("/en/admin/post/").await;
    assert_eq!(admin.status, 302);
    assert!(
        admin
            .header("location")
            .is_some_and(|location| location.ends_with("/en/login")),
        "redirects are built from the effective authority: {:?}",
        admin.header("location")
    );

    let login_page = visitor.get("/en/login").await;
    assert_eq!(login_page.status, 200);
    let token = login_page
        .text()
        .split("name=\"_csrf_token\"")
        .nth(1)
        .and_then(|rest| rest.split("value=\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("login form has a CSRF field")
        .to_owned();
    let login = visitor
        .post_form(
            "/en/login",
            &[
                ("_username", "john_user"),
                ("_password", "kitten"),
                ("_csrf_token", &token),
            ],
        )
        .await;
    assert_eq!(login.status, 302, "{}", login.text());
    assert!(
        !login
            .header("location")
            .is_some_and(|location| location.ends_with("/en/login")),
        "a successful login does not return to the form: {:?}",
        login.headers
    );
    assert!(
        visitor.cookies.contains_key("PHPSESSID"),
        "the session cookie reaches the client"
    );

    // john_user is authenticated but not an administrator.
    assert_eq!(visitor.get("/en/admin/post/").await.status, 403);

    let comment = format!(
        "Written through Hamster at {:?}",
        std::time::SystemTime::now()
    );
    let posted = visitor
        .post_form(
            &format!("/en/blog/comment/{POST_SLUG}/new"),
            &[
                ("comment[content]", &comment),
                ("comment[_token]", "csrf-token"),
            ],
        )
        .await;
    assert!(
        matches!(posted.status, 302 | 303),
        "{}: {}",
        posted.status,
        posted.text()
    );
    let post = visitor.get(&format!("/en/blog/posts/{POST_SLUG}")).await;
    assert_eq!(post.status, 200);
    assert!(
        post.text().contains(&comment),
        "the comment written by one request is read by the next"
    );

    let mut stranger = server.client(SYMFONY);
    assert_eq!(
        stranger.get("/en/admin/post/").await.status,
        302,
        "another client is not logged in"
    );
    assert!(server.stop().await.is_ok());
}

/// One worker, alternating an administrator and an anonymous client. Symfony
/// resets its services between requests; Hamster must neither mix their
/// cookies nor replace the worker to achieve that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn symfony_demo_keeps_clients_apart_on_one_persistent_worker() {
    let server = AppServer::start(&[Pool {
        host: SYMFONY,
        app: App::Symfony,
        workers: 1,
    }])
    .await;
    let before = server.pool(SYMFONY).snapshot();
    let mut admin = server.client(SYMFONY);
    let mut anonymous = server.client(SYMFONY);
    let login = admin
        .post_form(
            "/en/login",
            &[
                ("_username", "jane_admin"),
                ("_password", "kitten"),
                ("_csrf_token", "csrf-token"),
            ],
        )
        .await;
    assert_eq!(login.status, 302, "{}", login.text());

    for round in 0..10 {
        assert_eq!(
            admin.get("/en/admin/post/").await.status,
            200,
            "round {round}: the administrator stays logged in"
        );
        let refused = anonymous.get("/en/admin/post/").await;
        assert_eq!(
            refused.status, 302,
            "round {round}: the anonymous client never inherits the session"
        );
        assert!(anonymous.cookies.get("PHPSESSID") != admin.cookies.get("PHPSESSID"));
    }
    let after = server.pool(SYMFONY).snapshot();
    assert_eq!(
        pids(&after),
        pids(&before),
        "one process served every request"
    );
    assert_eq!(after.workers[0].generation, before.workers[0].generation);
    assert!(after.workers[0].request_count >= 21);
    assert!(server.stop().await.is_ok());
}

/// A PSR-15 application: routing, the application's JSON error handler, and
/// the headers its own emitter would add, reproduced by the entry file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn slim_skeleton_routes_and_reports_errors_through_its_own_handlers() {
    let server = AppServer::start(&[Pool {
        host: SLIM,
        app: App::Slim,
        workers: 1,
    }])
    .await;
    let mut client = server.client(SLIM);

    let home = client
        .send("GET", "/", &[("Origin", "http://app.example")], &[])
        .await;
    assert_eq!(home.status, 200);
    assert_eq!(home.text(), "Hello world!");
    assert_eq!(
        home.header("access-control-allow-origin"),
        Some("http://app.example")
    );
    assert_eq!(
        home.all("cache-control"),
        [
            "no-store, no-cache, must-revalidate, max-age=0",
            "post-check=0, pre-check=0"
        ],
        "repeated response headers survive"
    );

    let users = client.get("/users").await;
    assert_eq!(users.status, 200);
    assert_eq!(users.header("content-type"), Some("application/json"));
    assert_eq!(users.text().matches("\"username\"").count(), 5);
    let user = client.get("/users/1").await;
    assert!(
        user.text().contains("\"username\": \"bill.gates\""),
        "{}",
        user.text()
    );

    let missing = client.get("/users/99").await;
    assert_eq!(missing.status, 404);
    assert!(
        missing.text().contains("\"type\": \"RESOURCE_NOT_FOUND\""),
        "{}",
        missing.text()
    );

    let preflight = client.send("OPTIONS", "/users", &[], &[]).await;
    assert_eq!(preflight.status, 200);
    assert!(server.stop().await.is_ok());
}

/// PHP's own session module works under the CLI SAPI but cannot send its
/// cookie, and an open session would otherwise be found already started by the
/// next request from any client. The PSR-15 adapter closes it after every
/// request, so two cookieless clients each start from nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn a_native_php_session_never_reaches_another_request() {
    let server = AppServer::start(&[Pool {
        host: PROBE,
        app: App::NativeSessionProbe,
        workers: 1,
    }])
    .await;
    for client in 0..3 {
        let response = server.client(PROBE).get("/").await;
        assert_eq!(response.status, 200, "{}", response.text());
        assert_eq!(
            response.text(),
            "{\"visits\":1}",
            "client {client} saw another client's session"
        );
        assert!(response.header("set-cookie").is_none());
    }
    assert!(server.stop().await.is_ok());
}

/// A form part: field name, optional (filename, content type), content.
type Part<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a [u8]);

/// Runs one of the adapters' standalone PHP check scripts against a real
/// library from the workspace and returns its stdout.
async fn adapter_checks(script: &str, autoload: &Path) -> String {
    let apps = workspace();
    let output = tokio::process::Command::new(repository().join(PHP_RELATIVE_PATH))
        .arg("-c")
        .arg(apps.join("php.ini"))
        // The Slim skeleton's PHP-DI emits deprecations on PHP 8.5.
        .args(["-d", "error_reporting=24575"])
        .arg(repository().join("fixtures/php-worker").join(script))
        .arg(autoload)
        .output()
        .await
        .expect("php runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{script} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// The HttpFoundation mapping against Symfony Demo's own symfony/http-foundation:
/// URI, scheme, port, client address, query, form, cookies, a repeated header
/// joined as CGI joins it, an upload that is valid and movable, upload cleanup,
/// headers Hamster owns, both cookies of a response, streamed and file bodies.
#[tokio::test]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn the_httpfoundation_mapping_holds_against_symfony_itself() {
    let autoload = workspace().join("symfony-demo/vendor/autoload.php");
    let stdout = adapter_checks("httpfoundation_test.php", &autoload).await;
    assert!(stdout.contains("28 checks passed"), "{stdout}");
}

/// The PSR-7 mapping against slim/psr7: URI parts, protocol version, query,
/// cookies, repeated headers both as PSR-7 values and as CGI server
/// parameters, parsed bodies, nested and empty uploads that move under the CLI
/// SAPI, response header flattening, and the native session guard.
#[tokio::test]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn the_psr7_mapping_holds_against_slim_psr7() {
    let autoload = workspace().join("slim-skeleton/vendor/autoload.php");
    let stdout = adapter_checks("psr7_test.php", &autoload).await;
    assert!(stdout.contains("30 checks passed"), "{stdout}");
}

fn multipart(boundary: &str, parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, file, content) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file {
            Some((filename, content_type)) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// PHP's `hash('fnv1a64', ...)`, so digests can be compared without a crate.
fn fnv1a64(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// What a SAPI would have parsed, reconstructed for a framework-free
/// application: form and multipart bodies with nested names and binary files,
/// cookies, repeated request headers, repeated response headers, headers
/// Hamster owns, the body limits in both directions, and application output
/// that must never reach the protocol pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn native_application_receives_sapi_equivalent_requests_and_bounded_responses() {
    let server = AppServer::start(&[Pool {
        host: NATIVE,
        app: App::Native,
        workers: 2,
    }])
    .await;
    let mut client = server.client(NATIVE);

    let binary: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let body = multipart(
        "hamster-boundary",
        &[
            ("title", None, b"hello"),
            ("tags[]", None, b"x"),
            ("tags[]", None, b"y"),
            (
                "doc",
                Some(("data.bin", "application/octet-stream")),
                &binary,
            ),
            (
                "nested[inner]",
                Some(("inner.txt", "text/plain")),
                b"inside",
            ),
            ("empty", Some(("", "application/octet-stream")), b""),
        ],
    );
    let echoed = client
        .send(
            "POST",
            "/echo?q=1&list[]=a",
            &[
                (
                    "Content-Type",
                    "multipart/form-data; boundary=hamster-boundary",
                ),
                ("Cookie", "theme=dark; note=two%20words"),
                ("X-Repeat", "one"),
                ("X-Repeat", "two"),
            ],
            &body,
        )
        .await;
    assert_eq!(echoed.status, 200, "{}", echoed.text());
    let fields = echoed.fields();
    let field = |key: &str| fields.get(key).map(String::as_str);
    assert_eq!(field("method"), Some("POST"));
    assert_eq!(field("query.list.0"), Some("a"));
    assert_eq!(field("fields.title"), Some("hello"));
    assert_eq!(
        (field("fields.tags.0"), field("fields.tags.1")),
        (Some("x"), Some("y"))
    );
    assert_eq!(field("cookies.note"), Some("two words"));
    assert_eq!(field("server_name"), Some(NATIVE));
    let repeated: Vec<_> = (0..)
        .map_while(|index| field(&format!("headers.{index}.0")).map(|name| (index, name)))
        .filter(|(_, name)| *name == "x-repeat")
        .map(|(index, _)| field(&format!("headers.{index}.1")).unwrap())
        .collect();
    assert_eq!(
        repeated,
        ["one", "two"],
        "repeated request headers arrive in order"
    );
    assert_eq!(field("files.0.field"), Some("doc"));
    assert_eq!(field("files.0.name"), Some("data.bin"));
    assert_eq!(
        field("files.0.size"),
        Some(binary.len().to_string().as_str())
    );
    assert_eq!(field("files.0.fnv1a64"), Some(fnv1a64(&binary).as_str()));
    assert_eq!(field("files.1.field"), Some("nested[inner]"));
    assert_eq!(field("files.1.fnv1a64"), Some(fnv1a64(b"inside").as_str()));
    assert_eq!(
        field("files.2.error"),
        Some("4"),
        "an empty file input is UPLOAD_ERR_NO_FILE"
    );
    assert_eq!(field("files.3.field"), None);
    assert_eq!(field("body_fnv1a64"), Some(fnv1a64(&body).as_str()));

    let form = client
        .post_form("/echo", &[("a[b][]", "1"), ("a[b][]", "2"), ("c", "é")])
        .await
        .fields();
    assert_eq!(form.get("fields.a.b.0").map(String::as_str), Some("1"));
    assert_eq!(form.get("fields.a.b.1").map(String::as_str), Some("2"));
    assert_eq!(form.get("fields.c").map(String::as_str), Some("é"));

    let cookies = client.get("/cookies").await;
    assert_eq!(
        cookies.all("set-cookie"),
        [
            "first=1; Path=/; HttpOnly",
            "second=two%20words; Path=/; SameSite=Lax"
        ]
    );
    assert_eq!(cookies.all("x-multi"), ["a", "b"]);
    assert_eq!(
        cookies.header("content-length"),
        Some("11"),
        "Hamster, not the application, owns Content-Length"
    );

    let pids_before = pids(&server.pool(NATIVE).snapshot());
    let seven = client.get("/bytes?n=7340032").await;
    assert_eq!(seven.status, 200);
    assert_eq!(seven.body.len(), 7_340_032);
    assert!(
        seven
            .body
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == index as u8)
    );
    let nine = client.get("/bytes?n=9437184").await;
    assert_eq!(nine.status, 500);
    assert_eq!(nine.text(), "Response Too Large");
    assert_eq!(
        pids(&server.pool(NATIVE).snapshot()),
        pids_before,
        "an oversized response costs the request, not the worker"
    );

    let head = client.send("HEAD", "/bytes?n=1000", &[], &[]).await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("1000"));
    assert!(head.body.is_empty());

    // Declared over the 1 MiB protocol limit: refused before any body byte is
    // read or the worker is involved. The client waits for 100-continue, as
    // curl does for large bodies; one that sends the body regardless can see
    // a reset instead of the 413 (see doc/PHP_APPLICATION_COMPATIBILITY.md).
    let refused = client
        .send(
            "POST",
            "/echo",
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", "1048577"),
                ("Expect", "100-continue"),
            ],
            &[],
        )
        .await;
    assert_eq!(refused.status, 413, "{}", refused.text());

    let output = client.get("/stdout").await;
    assert_eq!(
        output.status, 200,
        "application output never corrupts a frame"
    );
    let outcome = output.fields();
    for safe in ["echo", "print", "php://output", "php://stdout"] {
        assert_eq!(
            outcome.get(safe).map(String::as_str),
            Some("ok"),
            "{safe}: {outcome:?}"
        );
    }
    assert!(
        outcome
            .get("STDOUT constant")
            .is_some_and(|message| message.starts_with("TypeError")),
        "the closed STDOUT constant is a known worker limitation: {outcome:?}"
    );
    assert_eq!(output.header("x-discarded-output"), Some("27"));
    assert_eq!(
        pids(&server.pool(NATIVE).snapshot()),
        pids_before,
        "nothing above replaced a worker"
    );
    assert!(server.stop().await.is_ok());
}

/// Application sessions shared by every worker of a pool: the cookie set by
/// one worker is honoured by the other, and two clients never see each
/// other's data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn native_application_sessions_follow_the_client_across_workers() {
    let server = AppServer::start(&[Pool {
        host: NATIVE,
        app: App::Native,
        workers: 2,
    }])
    .await;
    let mut alice = server.client(NATIVE);
    let mut bob = server.client(NATIVE);
    let first = alice.get("/session?name=alice").await;
    assert!(
        first
            .header("set-cookie")
            .is_some_and(|value| value.starts_with("nsid="))
    );
    bob.get("/session?name=bob").await;

    // Two clients at once, each request holding its worker for 100 ms, so each
    // round occupies both workers. Continue until Alice's session has been
    // served by both processes: it lives in the application's shared store
    // and must follow her cookie whichever worker the dispatcher picks.
    let mut alice_pids = std::collections::HashSet::new();
    let mut visits = 1;
    for _ in 0..40 {
        let (visit, other) =
            tokio::join!(alice.get("/session?hold=100"), bob.get("/session?hold=100"));
        let (visit, other) = (visit.fields(), other.fields());
        visits += 1;
        assert_eq!(visit["name"], "alice");
        assert_eq!(visit["visits"], visits.to_string());
        assert_eq!(other["name"], "bob");
        assert_ne!(
            visit["pid"], other["pid"],
            "concurrent requests use both workers"
        );
        alice_pids.insert(visit["pid"].clone());
        if alice_pids.len() == 2 {
            break;
        }
    }
    assert_eq!(
        alice_pids.len(),
        2,
        "one session was served by both workers"
    );
    assert!(server.stop().await.is_ok());
}

/// Failures owned by the application stay inside it: a handled exception is
/// the application's 500 and costs nothing; `exit` and memory exhaustion end
/// the worker, the request is answered 502 without being retried, and the
/// slot is replaced while the other worker keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn native_application_failures_cost_only_the_failing_worker() {
    let server = AppServer::start(&[Pool {
        host: NATIVE,
        app: App::Native,
        workers: 2,
    }])
    .await;
    let pool = server.pool(NATIVE);
    let mut client = server.client(NATIVE);

    let handled = client.get("/error").await;
    assert_eq!(handled.status, 500);
    assert_eq!(handled.text(), "application error: handled failure");
    assert_eq!(pool.snapshot().total_respawns, 0);

    for (path, respawns) in [("/exit", 1), ("/oom", 2)] {
        let failed = client.get(path).await;
        assert_eq!(failed.status, 502, "{path}: {}", failed.text());
        let snapshot = wait_for(pool, |snapshot| {
            snapshot.total_respawns >= respawns && snapshot.live_workers == 2
        })
        .await;
        assert_eq!(snapshot.total_respawns, respawns, "{path}");
        let state = client.get("/state").await;
        assert_eq!(state.status, 200, "{path}: the pool serves again");
    }
    assert!(server.stop().await.is_ok());
}

/// Every application at once, each in its own pool, under concurrent mixed
/// traffic while one pool's workers are killed repeatedly by its own requests.
/// The other pools must answer every request and never replace a worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn applications_share_one_server_and_a_crashing_pool_disturbs_no_other() {
    let mut pools = vec![
        Pool {
            host: SYMFONY,
            app: App::Symfony,
            workers: 2,
        },
        Pool {
            host: SLIM,
            app: App::Slim,
            workers: 2,
        },
        Pool {
            host: NATIVE,
            app: App::Native,
            workers: 2,
        },
    ];
    let laravel = std::env::var_os("HAMSTER_LARAVEL_TEST_PATH").map(PathBuf::from);
    if let Some(path) = &laravel {
        pools.push(Pool {
            host: LARAVEL,
            app: App::Laravel(path.clone()),
            workers: 2,
        });
    } else {
        eprintln!("HAMSTER_LARAVEL_TEST_PATH is not set; running without the Laravel stand-in");
    }
    let server = Arc::new(AppServer::start(&pools).await);
    let stable: Vec<&str> = pools
        .iter()
        .map(|pool| pool.host)
        .filter(|host| *host != NATIVE)
        .collect();
    let before: HashMap<&str, PoolSnapshot> = stable
        .iter()
        .map(|host| (*host, server.pool(host).snapshot()))
        .collect();

    // Symfony compiles templates on first use; warm each of its workers so the
    // measured traffic is steady-state rather than first-request latency.
    let warm: Vec<_> = (0..2)
        .map(|_| {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.client(SYMFONY).get("/en/blog/").await.status })
        })
        .collect();
    for task in warm {
        assert_eq!(task.await.unwrap(), 200);
    }

    let chaos = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let mut statuses = Vec::new();
            for _ in 0..8 {
                statuses.push(server.client(NATIVE).get("/exit").await.status);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            statuses
        })
    };
    let mut clients = Vec::new();
    for client in 0..8 {
        let server = Arc::clone(&server);
        let hosts = stable.clone();
        clients.push(tokio::spawn(async move {
            let mut results = Vec::new();
            for round in 0..24 {
                let host = match (client + round) % (hosts.len() + 1) {
                    index if index < hosts.len() => hosts[index],
                    _ => NATIVE,
                };
                let (path, marker) = match host {
                    SYMFONY => ("/en/blog/", POST_SLUG),
                    SLIM => ("/users/1", "bill.gates"),
                    LARAVEL => ("/up", ""),
                    _ => ("/state", "pid="),
                };
                let response = server.client(host).get(path).await;
                results.push((host, response.status, response.text().contains(marker)));
            }
            results
        }));
    }
    let mut native_statuses = Vec::new();
    for client in clients {
        for (host, status, marked) in client.await.unwrap() {
            if host == NATIVE {
                native_statuses.push(status);
            } else {
                assert_eq!(status, 200, "{host} failed while another pool crashed");
                assert!(marked, "{host} answered with another application's content");
            }
        }
    }
    let chaos = chaos.await.unwrap();
    assert!(
        chaos.iter().all(|status| *status == 502 || *status == 503),
        "{chaos:?}"
    );
    assert!(
        native_statuses
            .iter()
            .all(|status| *status == 200 || *status == 503),
        "the crashing pool refuses or serves, never mis-serves: {native_statuses:?}"
    );
    wait_for(server.pool(NATIVE), |snapshot| snapshot.live_workers == 2).await;
    assert!(server.pool(NATIVE).snapshot().total_respawns >= 1);
    for host in &stable {
        let after = server.pool(host).snapshot();
        assert_eq!(after.total_respawns, 0, "{host} replaced a worker");
        assert_eq!(pids(&after), pids(&before[host]), "{host} changed process");
    }
    let server = Arc::try_unwrap(server).ok().expect("all clients finished");
    assert!(server.stop().await.is_ok());
}

/// Every adapter answers SHUTDOWN and exits by itself: each worker's process
/// exit code is 0 rather than the 1 of the server's kill fallback.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php.exe"]
async fn every_application_worker_stops_through_its_adapter_on_shutdown() {
    let mut pools = vec![
        Pool {
            host: SYMFONY,
            app: App::Symfony,
            workers: 2,
        },
        Pool {
            host: SLIM,
            app: App::Slim,
            workers: 2,
        },
        Pool {
            host: NATIVE,
            app: App::Native,
            workers: 2,
        },
    ];
    if let Some(path) = std::env::var_os("HAMSTER_LARAVEL_TEST_PATH") {
        pools.push(Pool {
            host: LARAVEL,
            app: App::Laravel(PathBuf::from(path)),
            workers: 1,
        });
    }
    let server = AppServer::start(&pools).await;
    // One request each, so shutdown meets workers that have served traffic.
    assert_eq!(server.client(SYMFONY).get("/en/blog/").await.status, 200);
    assert_eq!(server.client(SLIM).get("/").await.status, 200);
    assert_eq!(server.client(NATIVE).get("/").await.status, 200);
    let workers: Vec<(String, windows::Process)> = server
        .runtime
        .php_workers()
        .iter()
        .flat_map(|pool| {
            pids(&pool.snapshot())
                .into_iter()
                .map(|pid| (pool.label().to_owned(), windows::Process::open(pid)))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        workers.len(),
        pools.iter().map(|pool| pool.workers).sum::<usize>()
    );

    let result = server.stop().await;
    assert!(result.is_ok(), "{result:?}");
    for (label, process) in &workers {
        assert_eq!(
            process.exit_code(),
            0,
            "{label} worker {} was killed",
            process.pid
        );
    }
}

#[cfg(windows)]
mod windows {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
        fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(handle: isize, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    /// A process handle opened while the process runs; unlike its PID, it
    /// keeps naming this process after exit.
    pub struct Process {
        pub pid: u32,
        handle: isize,
    }

    impl Process {
        pub fn open(pid: u32) -> Self {
            const SYNCHRONIZE: u32 = 0x0010_0000;
            const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
            // SAFETY: plain Win32 call; a null result is checked below.
            let handle =
                unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            assert_ne!(handle, 0, "open worker {pid}");
            Self { pid, handle }
        }

        pub fn exit_code(&self) -> u32 {
            // SAFETY: the handle is owned by self and open.
            let waited = unsafe { WaitForSingleObject(self.handle, 5_000) };
            assert_eq!(waited, 0, "worker {} is still running", self.pid);
            let mut code = 0;
            // SAFETY: as above; `code` outlives the call.
            assert_ne!(unsafe { GetExitCodeProcess(self.handle, &mut code) }, 0);
            code
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed once.
            unsafe { CloseHandle(self.handle) };
        }
    }
}
