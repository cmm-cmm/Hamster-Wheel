//! The `fastcgi` handler against real `php-cgi.exe` endpoints, started here the
//! way an operator would start them (`php-cgi -b 127.0.0.1:<port>`), serving
//! `fixtures/php-cgi`, a classic SAPI application: superglobals,
//! `move_uploaded_file()`, `header()`, `setcookie()`, native sessions,
//! `flush()` and `exit`.
//!
//! Ignored: needs the repository's `php/8.5/php-cgi.exe`.

mod common;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use hamster_wheel::{config, fastcgi::FastCgiBackend, server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// A loopback port outside the dynamic range that is free now.
fn free_port() -> SocketAddr {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let _ = NEXT.compare_exchange(
        0,
        (std::process::id() % 9_000) as u16 + 1,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    loop {
        let port = 30_000 + NEXT.fetch_add(1, Ordering::AcqRel) % 9_000;
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        if std::net::TcpListener::bind(address).is_ok() {
            return address;
        }
    }
}

/// `php-cgi` endpoints the test manages as an operator would.
struct PhpCgi {
    addresses: Vec<SocketAddr>,
    children: Vec<Option<Child>>,
    sessions: PathBuf,
}

impl PhpCgi {
    fn start(count: usize) -> Self {
        common::end_children_with_this_process();
        let sessions = std::env::temp_dir().join(format!(
            "hamster-cgi-sessions-{}-{}",
            std::process::id(),
            free_port().port()
        ));
        std::fs::create_dir_all(&sessions).unwrap();
        let mut endpoints = Self {
            addresses: Vec::new(),
            children: Vec::new(),
            sessions,
        };
        for _ in 0..count {
            let address = free_port();
            endpoints.addresses.push(address);
            endpoints.children.push(None);
            endpoints.launch(endpoints.addresses.len() - 1);
        }
        endpoints
    }

    fn launch(&mut self, index: usize) {
        let address = self.addresses[index];
        let child = Command::new(repository().join("php/8.5/php-cgi.exe"))
            .arg("-b")
            .arg(address.to_string())
            .args([
                "-d",
                "display_errors=0",
                "-d",
                "log_errors=1",
                "-d",
                "error_log=",
            ])
            .arg("-d")
            .arg(format!("session.save_path={}", self.sessions.display()))
            // php-cgi exits after this many requests; 0 means never.
            .env("PHP_FCGI_MAX_REQUESTS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("php-cgi.exe starts");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(address).is_err() {
            assert!(
                Instant::now() < deadline,
                "php-cgi never listened on {address}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.children[index] = Some(child);
    }

    fn kill(&mut self, index: usize) {
        if let Some(mut child) = self.children[index].take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for PhpCgi {
    fn drop(&mut self) {
        for index in 0..self.children.len() {
            self.kill(index);
        }
        let _ = std::fs::remove_dir_all(&self.sessions);
    }
}

struct Location<'a> {
    host: &'a str,
    root: PathBuf,
    servers: &'a [SocketAddr],
    extra: &'a str,
    /// Path prefixes answered 404 instead of reaching the root, as an
    /// application's own `.htaccess` deny rules would under Apache.
    deny: &'a [&'a str],
}

struct AppServer {
    address: SocketAddr,
    runtime: Arc<config::RuntimeConfig>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl AppServer {
    async fn start(locations: &[Location<'_>]) -> Self {
        let mut source = "[server]\nlisten = \"127.0.0.1:0\"\n".to_owned();
        for location in locations {
            let servers = location
                .servers
                .iter()
                .map(|address| format!("\"{address}\""))
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str(&format!("\n[[hosts]]\nname = \"{}\"\n", location.host));
            for prefix in location.deny {
                source.push_str(&format!(
                    "\n[[hosts.locations]]\npath = \"{prefix}\"\nhandler = \"not_found\"\n"
                ));
            }
            source.push_str(&format!(
                "\n[[hosts.locations]]\npath = \"/\"\nhandler = \"fastcgi\"\nroot = {}\n\n[hosts.locations.fastcgi]\nservers = [{servers}]\n{}\n",
                toml_literal(&location.root),
                location.extra
            ));
        }
        let runtime =
            Arc::new(config::compile_str(&source, &repository()).expect("config compiles"));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = runtime.router_arc();
        let limits = runtime.http_limits();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, limits, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Self {
            address,
            runtime,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn backend(&self, index: usize) -> &Arc<FastCgiBackend> {
        &self.runtime.fastcgi_backends()[index]
    }

    async fn stop(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        tokio::time::timeout(Duration::from_secs(30), &mut self.task)
            .await
            .expect("the server stops")
            .unwrap()
            .unwrap();
    }
}

fn fixture_root() -> PathBuf {
    repository().join("fixtures/php-cgi")
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
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

    fn fields(&self) -> HashMap<String, String> {
        self.text()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }
}

/// One request per connection; a small cookie jar per client.
struct Client {
    address: SocketAddr,
    host: String,
    cookies: Vec<(String, String)>,
}

impl Client {
    fn new(address: SocketAddr, host: &str) -> Self {
        Self {
            address,
            host: host.to_owned(),
            cookies: Vec::new(),
        }
    }

    async fn get(&mut self, target: &str) -> Reply {
        self.send("GET", target, &[], &[]).await
    }

    async fn send(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.host
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
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() || method == "POST" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        request.extend_from_slice(body);
        let reply = exchange(self.address, &request).await;
        for value in reply.all("set-cookie") {
            if let Some((name, cookie)) = value
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
            {
                self.cookies.retain(|(existing, _)| existing != name);
                self.cookies.push((name.to_owned(), cookie.to_owned()));
            }
        }
        reply
    }
}

async fn exchange(address: SocketAddr, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .expect("a reply within a minute")
        .unwrap();
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Reply {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {} bytes", bytes.len()));
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let mut body = bytes[end + 4..].to_vec();
    if headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked")
    {
        let mut raw = body.as_slice();
        let mut decoded = Vec::new();
        loop {
            let line = raw.windows(2).position(|window| window == b"\r\n").unwrap();
            let size =
                usize::from_str_radix(std::str::from_utf8(&raw[..line]).unwrap(), 16).unwrap();
            raw = &raw[line + 2..];
            if size == 0 {
                break;
            }
            decoded.extend_from_slice(&raw[..size]);
            raw = &raw[size + 2..];
        }
        body = decoded;
    }
    Reply {
        status,
        headers,
        body,
    }
}

fn fnv1a64(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

const HOST: &str = "cgi.test";

/// What a SAPI hands a classic script: superglobals from the query, a form, a
/// multipart upload that `move_uploaded_file()` accepts, cookies, a repeated
/// header joined as CGI joins it, PATH_INFO, and no HTTP_PROXY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn a_classic_script_sees_the_request_through_superglobals_and_moves_its_upload() {
    let cgi = PhpCgi::start(1);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "",
        deny: &[],
    }])
    .await;
    let mut client = Client::new(server.address, HOST);

    let get = client
        .send(
            "GET",
            "/some/route?a=1&list[]=x&list[]=y",
            &[
                ("Cookie", "theme=dark; note=two%20words"),
                ("X-Repeat", "one"),
                ("X-Repeat", "two"),
                ("Proxy", "http://attacker.test"),
            ],
            &[],
        )
        .await;
    assert_eq!(get.status, 200, "{}", get.text());
    let fields = get.fields();
    let field = |key: &str| fields.get(key).map(String::as_str);
    assert_eq!(field("script"), Some("index.php"));
    assert_eq!(field("sapi"), Some("cgi-fcgi"));
    assert_eq!(
        field("server.REQUEST_URI"),
        Some("/some/route?a=1&list[]=x&list[]=y")
    );
    assert_eq!(field("server.SCRIPT_NAME"), Some("/index.php"));
    assert_eq!(field("get.a"), Some("1"));
    assert_eq!(
        (field("get.list.0"), field("get.list.1")),
        (Some("x"), Some("y"))
    );
    assert_eq!(field("cookie.note"), Some("two words"));
    assert_eq!(field("server.HTTP_X_REPEAT"), Some("one, two"));
    assert_eq!(field("server.HTTP_PROXY"), Some("(unset)"));
    assert_eq!(field("server.REMOTE_ADDR"), Some("127.0.0.1"));
    assert_eq!(field("server.SERVER_NAME"), Some(HOST));

    let form = client
        .send(
            "POST",
            "/?via=form",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"title=hello&tags%5B%5D=a&tags%5B%5D=b&text=%C3%A9",
        )
        .await
        .fields();
    assert_eq!(form.get("post.title").map(String::as_str), Some("hello"));
    assert_eq!(form.get("post.tags.1").map(String::as_str), Some("b"));
    assert_eq!(form.get("post.text").map(String::as_str), Some("é"));

    let binary: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let mut body = b"--b0undary\r\nContent-Disposition: form-data; name=\"caption\"\r\n\r\nhi\r\n--b0undary\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"data.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
    body.extend_from_slice(&binary);
    body.extend_from_slice(b"\r\n--b0undary--\r\n");
    let upload = client
        .send(
            "POST",
            "/",
            &[("Content-Type", "multipart/form-data; boundary=b0undary")],
            &body,
        )
        .await
        .fields();
    let field = |key: &str| upload.get(key).map(String::as_str);
    assert_eq!(field("post.caption"), Some("hi"));
    assert_eq!(field("files.doc.name"), Some("data.bin"));
    assert_eq!(field("files.doc.size"), Some("300000"));
    assert_eq!(
        field("files.doc.uploaded"),
        Some("yes"),
        "a real SAPI upload"
    );
    assert_eq!(field("files.doc.moved"), Some("yes"));
    assert_eq!(field("files.doc.fnv1a64"), Some(fnv1a64(&binary).as_str()));

    let path_info = client.get("/index.php/extra/path?q=1").await.fields();
    assert_eq!(
        path_info.get("server.PATH_INFO").map(String::as_str),
        Some("/extra/path")
    );
    assert_eq!(
        path_info.get("php_self").map(String::as_str),
        Some("/index.php/extra/path")
    );
    server.stop().await;
}

/// `http_response_code()`, `setcookie()`, repeated `header()`, a redirect by
/// `Location`, and native sessions whose cookie PHP itself sets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn status_headers_cookies_redirects_and_native_sessions_come_from_php() {
    let cgi = PhpCgi::start(2);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "",
        deny: &[],
    }])
    .await;
    let mut client = Client::new(server.address, HOST);

    let cookies = client.get("/features.php?do=cookies").await;
    assert_eq!(cookies.status, 201);
    assert_eq!(cookies.text(), "cookies");
    let set = cookies.all("set-cookie");
    assert_eq!(set.len(), 2, "{set:?}");
    assert!(set[0].starts_with("first=1"));
    assert!(set[1].starts_with("second=two%20words") && set[1].contains("SameSite=Lax"));
    assert_eq!(cookies.all("x-multi"), ["a", "b"]);

    let redirect = client.get("/features.php?do=redirect").await;
    assert_eq!(redirect.status, 302);
    assert_eq!(redirect.all("location"), ["/target?from=redirect"]);

    // Sessions live in files both php-cgi processes share, so they follow the
    // cookie whichever endpoint serves the request.
    let mut alice = Client::new(server.address, HOST);
    let mut bob = Client::new(server.address, HOST);
    let first = alice.get("/features.php?do=session&name=alice").await;
    assert!(
        first
            .all("set-cookie")
            .iter()
            .any(|cookie| cookie.starts_with("PHPSESSID="))
    );
    bob.get("/features.php?do=session&name=bob").await;
    let mut pids = std::collections::HashSet::new();
    for visit in 2..=8 {
        let (a, b) = tokio::join!(
            alice.get("/features.php?do=session"),
            bob.get("/features.php?do=session")
        );
        let (a, b) = (a.fields(), b.fields());
        assert_eq!(a["visits"], visit.to_string());
        assert_eq!(a["name"], "alice");
        assert_eq!(b["name"], "bob", "sessions never cross clients");
        pids.insert(a["pid"].clone());
        pids.insert(b["pid"].clone());
    }
    assert_eq!(pids.len(), 2, "both endpoints served the sessions");
    server.stop().await;
}

/// Output flushed by the script reaches the client before the script ends,
/// and a response is not bounded by any Hamster frame size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn flushed_output_streams_and_large_responses_are_not_bounded() {
    let cgi = PhpCgi::start(1);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "",
        deny: &[],
    }])
    .await;

    let started = Instant::now();
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /features.php?do=stream HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut first_chunk_at = None;
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..read]);
        if first_chunk_at.is_none() && received.windows(8).any(|window| window == b"chunk 0\n") {
            first_chunk_at = Some(started.elapsed());
        }
    }
    let total = started.elapsed();
    let first = first_chunk_at.expect("the first chunk arrived");
    assert!(
        first + Duration::from_millis(600) < total,
        "the first chunk ({first:?}) arrived long before the end ({total:?})"
    );
    assert!(String::from_utf8_lossy(&received).contains("chunk 3\n"));

    let bytes = 20 * 1024 * 1024 + 17;
    let big = Client::new(server.address, HOST)
        .get(&format!("/features.php?do=big&bytes={bytes}"))
        .await;
    assert_eq!(big.status, 200);
    assert_eq!(big.body.len(), bytes);
    assert!(big.body.starts_with(b"0123456789abcdef"));
    server.stop().await;
}

/// A fatal error is PHP's own 500 and `exit` keeps what was printed; the
/// endpoint keeps serving. A missing script is 404, a static file is served by
/// Hamster, and no spelling of a script's name returns its source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn errors_exit_static_files_and_script_sources_behave_as_under_a_web_server() {
    let cgi = PhpCgi::start(1);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "",
        deny: &[],
    }])
    .await;
    let mut client = Client::new(server.address, HOST);

    let fatal = client.get("/features.php?do=fatal").await;
    assert_eq!(fatal.status, 500, "{}", fatal.text());
    let exit = client.get("/features.php?do=exit").await;
    assert_eq!((exit.status, exit.text().as_str()), (200, "before exit"));
    assert_eq!(client.get("/features.php?do=sleep").await.status, 200);

    assert_eq!(client.get("/nothing-here.php").await.status, 404);
    let css = client.get("/assets/site.css").await;
    assert_eq!(css.status, 200);
    assert!(css.all("content-type")[0].starts_with("text/css"));
    for path in ["/features.php.", "/FEATURES.PHP?do=exit", "/index.php%20"] {
        let reply = client.get(path).await;
        assert!(
            !reply.text().contains("<?php"),
            "{path} returned PHP source: {}",
            reply.text()
        );
    }
    assert_eq!(server.backend(0).snapshot().backend_failures, 0);
    server.stop().await;
}

/// Each endpoint serves one request at a time; two endpoints serve two. When
/// one endpoint dies Hamster moves new requests to the other; when all are
/// down it answers 502 at once; an endpoint the operator restarts is used
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn concurrency_follows_the_endpoints_and_a_dead_endpoint_is_skipped() {
    let mut cgi = PhpCgi::start(2);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "connect_timeout = 10",
        deny: &[],
    }])
    .await;
    let address = server.address;
    let slow = |ms: u64| async move {
        Client::new(address, HOST)
            .get(&format!("/features.php?do=sleep&ms={ms}"))
            .await
    };

    let started = Instant::now();
    let (a, b) = tokio::join!(slow(600), slow(600));
    let two = started.elapsed();
    assert_eq!((a.status, b.status), (200, 200));
    assert_ne!(a.text(), b.text(), "two endpoints, two processes");
    assert!(
        two < Duration::from_millis(1_100),
        "ran in parallel: {two:?}"
    );
    let started = Instant::now();
    let (a, b, c) = tokio::join!(slow(600), slow(600), slow(600));
    assert!(
        [a.status, b.status, c.status]
            .iter()
            .all(|status| *status == 200)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1_150),
        "the third waited for a slot"
    );

    cgi.kill(0);
    for _ in 0..4 {
        assert_eq!(slow(0).await.status, 200, "the surviving endpoint serves");
    }
    assert!(server.backend(0).snapshot().connect_failures >= 1);
    cgi.kill(1);
    let refused = slow(0).await;
    assert_eq!(refused.status, 502);
    cgi.launch(0);
    assert_eq!(
        slow(0).await.status,
        200,
        "a restarted endpoint is used again"
    );
    assert_eq!(server.backend(0).snapshot().active_exchanges, 0);
    server.stop().await;
}

/// A script that outlives `response_timeout` is answered 504 and its slot is
/// released; php-cgi itself keeps running the script, so the endpoint is busy
/// until it finishes (Hamster cannot abort PHP through FastCGI).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn a_script_slower_than_the_response_timeout_is_a_gateway_timeout() {
    let cgi = PhpCgi::start(1);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "response_timeout = 1",
        deny: &[],
    }])
    .await;
    let mut client = Client::new(server.address, HOST);
    let started = Instant::now();
    let slow = client.get("/features.php?do=sleep&ms=3000").await;
    assert_eq!(slow.status, 504);
    assert!(started.elapsed() < Duration::from_millis(2_000));
    let snapshot = server.backend(0).snapshot();
    assert_eq!((snapshot.timeouts, snapshot.active_exchanges), (1, 0));
    // php-cgi is still running the abandoned script; a request sent now would
    // wait behind it and time out too.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let after = client.get("/features.php?do=sleep").await;
    assert_eq!(
        after.status, 200,
        "served once php-cgi finished the slow script"
    );
    server.stop().await;
}

/// Two hosts, two applications, two sets of endpoints: each host runs its own
/// root, and losing one host's endpoints does not touch the other host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn hosts_keep_their_own_roots_and_endpoints() {
    let other_root = std::env::temp_dir().join(format!("hamster-cgi-other-{}", std::process::id()));
    std::fs::create_dir_all(&other_root).unwrap();
    std::fs::write(
        other_root.join("index.php"),
        "<?php echo 'other app ', basename(__DIR__);",
    )
    .unwrap();
    let mut first = PhpCgi::start(1);
    let second = PhpCgi::start(1);
    let server = AppServer::start(&[
        Location {
            host: "one.test",
            root: fixture_root(),
            servers: &first.addresses,
            extra: "",
            deny: &[],
        },
        Location {
            host: "two.test",
            root: other_root.clone(),
            servers: &second.addresses,
            extra: "",
            deny: &[],
        },
    ])
    .await;
    let mut one = Client::new(server.address, "one.test");
    let mut two = Client::new(server.address, "two.test");
    assert_eq!(
        one.get("/")
            .await
            .fields()
            .get("script")
            .map(String::as_str),
        Some("index.php")
    );
    assert!(two.get("/").await.text().starts_with("other app"));
    assert_eq!(
        two.get("/features.php").await.status,
        404,
        "the other root has no such script"
    );

    first.kill(0);
    assert_eq!(one.get("/").await.status, 502);
    for _ in 0..3 {
        assert!(two.get("/").await.text().starts_with("other app"));
    }
    server.stop().await;
    let _ = std::fs::remove_dir_all(other_root);
}

/// Shutdown lets a script that is already running finish and deliver its
/// response before the server stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires repository php/8.5/php-cgi.exe"]
async fn shutdown_drains_a_running_script() {
    let cgi = PhpCgi::start(1);
    let server = AppServer::start(&[Location {
        host: HOST,
        root: fixture_root(),
        servers: &cgi.addresses,
        extra: "",
        deny: &[],
    }])
    .await;
    let address = server.address;
    let running = tokio::spawn(async move {
        Client::new(address, HOST)
            .get("/features.php?do=sleep&ms=800")
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.stop().await;
    let reply = running.await.unwrap();
    assert_eq!(reply.status, 200);
    assert!(reply.text().starts_with("slept"));
}

/// A 1x1 PNG, small enough to inline and valid enough for DokuWiki's checks.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn form_value(page: &str, name: &str) -> String {
    page.split(&format!("name=\"{name}\""))
        .nth(1)
        .and_then(|rest| rest.split('>').next())
        .and_then(|input| input.split("value=\"").nth(1))
        .and_then(|value| value.split('"').next())
        .unwrap_or_default()
        .to_owned()
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

/// DokuWiki 2026-07-14c, the release tarball unmodified, through two php-cgi
/// endpoints: pages, its static assets, its native session cookie, an edit
/// saved through its form and read back, and a media upload it stores with
/// move_uploaded_file() and serves again. Its data and configuration
/// directories are closed with `not_found` locations, as its `.htaccess`
/// files close them under Apache; without them `/data/pages/*.txt` is served.
/// Needs HAMSTER_PHP_APPS (fixtures/php-apps/prepare_php_apps.ps1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires HAMSTER_PHP_APPS and repository php/8.5/php-cgi.exe"]
async fn dokuwiki_runs_unmodified_through_fastcgi() {
    let apps = PathBuf::from(std::env::var_os("HAMSTER_PHP_APPS").expect(
        "HAMSTER_PHP_APPS must name the workspace built by fixtures/php-apps/prepare_php_apps.ps1",
    ));
    let wiki = apps.join("dokuwiki");
    assert!(
        wiki.join("doku.php").is_file(),
        "the workspace has no dokuwiki"
    );
    let cgi = PhpCgi::start(2);
    let deny = ["/data", "/conf", "/bin", "/inc", "/vendor"];
    let server = AppServer::start(&[Location {
        host: "wiki.test",
        root: wiki.clone(),
        servers: &cgi.addresses,
        extra: "index = \"doku.php\"",
        deny: &deny,
    }])
    .await;
    let mut editor = Client::new(server.address, "wiki.test");

    let home = editor.get("/").await;
    assert_eq!(home.status, 200);
    assert!(home.text().contains("DokuWiki"), "{}", home.text());
    assert!(
        editor.cookies.iter().any(|(name, _)| name == "DokuWiki"),
        "DokuWiki's own session cookie reaches the client: {:?}",
        home.all("set-cookie")
    );
    let logo = editor.get("/lib/tpl/dokuwiki/images/logo.png").await;
    assert_eq!(logo.status, 200);
    assert_eq!(logo.all("content-type"), ["image/png"]);
    for private in [
        "/data/pages/wiki/dokuwiki.txt",
        "/conf/dokuwiki.php",
        "/inc/init.php",
    ] {
        assert_eq!(editor.get(private).await.status, 404, "{private}");
    }

    let unique = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let page = format!("hamster{unique}");
    let form = editor.get(&format!("/doku.php?id={page}&do=edit")).await;
    assert_eq!(form.status, 200);
    let form = form.text();
    let text = format!("Written through Hamster FastCGI {unique}");
    let body = [
        ("id", page.as_str()),
        ("sectok", &form_value(&form, "sectok")),
        ("date", &form_value(&form, "date")),
        ("prefix", "."),
        ("suffix", ""),
        ("changecheck", &form_value(&form, "changecheck")),
        ("target", "section"),
        ("wikitext", &text),
        ("summary", "hamster test"),
        ("do[save]", "1"),
    ]
    .iter()
    .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    let saved = editor
        .send(
            "POST",
            "/doku.php",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            body.as_bytes(),
        )
        .await;
    assert!(
        matches!(saved.status, 302 | 303),
        "{}: {}",
        saved.status,
        saved.text()
    );
    let reader = Client::new(server.address, "wiki.test")
        .get(&format!("/doku.php?id={page}"))
        .await;
    assert!(
        reader.text().contains(&text),
        "another client reads the saved page"
    );

    let media = editor
        .get("/doku.php?do=media&ns=hamster&tab_files=upload")
        .await
        .text();
    let name = format!("upload{unique}.png");
    let mut upload = Vec::new();
    for (field, value) in [
        ("sectok", form_value(&media, "sectok")),
        ("ns", "hamster".to_owned()),
        ("mediaid", name.clone()),
        ("ow", "1".to_owned()),
    ] {
        upload.extend_from_slice(
            format!(
                "--b0undary\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    upload.extend_from_slice(format!("--b0undary\r\nContent-Disposition: form-data; name=\"upload\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n").as_bytes());
    upload.extend_from_slice(PNG);
    upload.extend_from_slice(b"\r\n--b0undary--\r\n");
    let uploaded = editor
        .send(
            "POST",
            "/doku.php?do=media&ns=hamster&tab_files=files",
            &[("Content-Type", "multipart/form-data; boundary=b0undary")],
            &upload,
        )
        .await;
    assert!(
        uploaded.status < 400,
        "{}: {}",
        uploaded.status,
        uploaded.text()
    );
    let fetched = editor
        .get(&format!("/lib/exe/fetch.php?media=hamster:{name}"))
        .await;
    assert_eq!(fetched.status, 200, "{}", fetched.text());
    assert_eq!(
        fetched.body, PNG,
        "the uploaded file is served back unchanged"
    );
    server.stop().await;
    let _ = std::fs::remove_file(wiki.join("data/media/hamster").join(&name));
}
