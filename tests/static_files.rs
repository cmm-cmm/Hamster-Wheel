use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use hamster_wheel::{
    config::{HttpLimits, SERVER_NAME},
    router::HostRouter,
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hamster-wheel-static-test-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("test document root should be created");
        Self { path }
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).expect("test document root should be removed");
    }
}

struct TestServer {
    address: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(document_root: PathBuf) -> Self {
        let document_root = Arc::new(
            tokio::fs::canonicalize(document_root)
                .await
                .expect("test document root should canonicalize"),
        );
        let router = Arc::new(
            HostRouter::default_static(document_root).expect("test static router should compile"),
        );
        Self::start_router(router).await
    }

    async fn start_router(router: Arc<HostRouter>) -> Self {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, router, HttpLimits::default(), async {
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

    async fn stop(mut self) {
        self.shutdown_tx
            .take()
            .expect("shutdown sender should exist")
            .send(())
            .expect("server should still be running");
        self.task
            .await
            .expect("server task should finish")
            .expect("server should shut down cleanly");
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
}

#[tokio::test]
async fn filesystem_handlers_do_not_serve_php_source_or_dotfiles() {
    let root = TestRoot::new();
    std::fs::write(root.path.join("index.html"), "public").unwrap();
    std::fs::write(root.path.join("config.php"), "<?php secret();").unwrap();
    std::fs::write(root.path.join("code.phtml"), "<?php secret();").unwrap();
    std::fs::write(root.path.join(".env"), "PRIVATE_PASSWORD").unwrap();
    std::fs::create_dir(root.path.join(".well-known")).unwrap();
    std::fs::write(root.path.join(".well-known/check.txt"), "public-check").unwrap();
    let server = TestServer::start(root.path.clone()).await;
    for path in ["/config.php", "/code.phtml", "/.env", "/%2eenv"] {
        for method in ["GET", "HEAD"] {
            let response = server
                .request(&format!(
                    "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                ))
                .await;
            assert!(
                response.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{method} {path}: {response}"
            );
            assert!(!response.contains("secret"));
            assert!(!response.contains("PRIVATE_PASSWORD"));
        }
    }
    let public = server
        .request(
            "GET /.well-known/check.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(public.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(public.ends_with("public-check"));
    server.stop().await;
}

#[cfg(windows)]
#[tokio::test]
async fn host_denials_cover_uri_aliases_and_final_file_location() {
    let root = TestRoot::new();
    std::fs::create_dir(root.path.join("data")).unwrap();
    std::fs::create_dir(root.path.join("public")).unwrap();
    std::fs::write(root.path.join("data/secret.txt"), "PRIVATE_DATA").unwrap();
    std::fs::write(root.path.join("public/composer.json"), "PRIVATE_CONFIG").unwrap();
    std::fs::write(root.path.join("public/ok.txt"), "public").unwrap();
    let alias = root.path.join("public").join("alias");
    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&alias)
        .arg(root.path.join("data"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let source = format!(
        "[server]\nlisten = '127.0.0.1:0'\n[[hosts]]\nname = 'localhost'\ndeny_paths = ['/data']\ndeny_names = ['composer.json']\n[[hosts.locations]]\npath = '/public'\nhandler = 'static'\nroot = '{}'\n[[hosts.locations]]\npath = '/'\nhandler = 'fastcgi'\nroot = '{}'\n[hosts.locations.fastcgi]\nservers = ['127.0.0.1:39001']\n",
        root.path.display(),
        root.path.display()
    );
    let runtime = hamster_wheel::config::compile_str(&source, &root.path).unwrap();
    let server = TestServer::start_router(runtime.router_arc()).await;
    for path in [
        "/data/secret.txt",
        "/DATA/secret.txt",
        "/%64ata/secret.txt",
        "/public/alias/secret.txt",
        "/public/composer.json",
    ] {
        let response = server
            .request(&format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ))
            .await;
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{path}: {response}"
        );
        assert!(!response.contains("PRIVATE_"));
    }
    let public = server
        .request("GET /public/ok.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(public.starts_with("HTTP/1.1 200 OK\r\n"));
    server.stop().await;
    std::fs::remove_dir(&alias).unwrap();
}

#[tokio::test]
async fn serves_index_mime_and_head() {
    let root = TestRoot::new();
    std::fs::write(root.path.join("index.html"), b"Hello, Hamster Wheel!")
        .expect("index fixture should be written");
    std::fs::create_dir(root.path.join("assets")).expect("asset directory should be created");
    std::fs::write(root.path.join("assets/app.js"), b"export {};")
        .expect("nested fixture should be written");
    std::fs::write(root.path.join("style.css"), b"body{}").expect("CSS fixture should be written");
    let large_body = "x".repeat(256 * 1_024);
    std::fs::write(root.path.join("large.txt"), &large_body)
        .expect("large fixture should be written");
    let server = TestServer::start(root.path.clone()).await;

    let index = server
        .request("GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(index.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(index.contains(&format!("server: {SERVER_NAME}\r\n")));
    assert!(index.contains("content-type: text/html; charset=utf-8\r\n"));
    assert!(index.ends_with("\r\n\r\nHello, Hamster Wheel!"));

    let head = server
        .request("HEAD /style.css HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(head.contains("content-type: text/css; charset=utf-8\r\n"));
    assert!(head.contains("content-length: 6\r\n"));
    assert!(head.ends_with("\r\n\r\n"));

    let nested = server
        .request("GET /assets/app.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(nested.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(nested.contains("content-type: text/javascript; charset=utf-8\r\n"));
    assert!(nested.ends_with("\r\n\r\nexport {};"));

    let large = server
        .request("GET /large.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(large.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(large.contains("content-length: 262144\r\n"));
    assert!(large.ends_with(&format!("\r\n\r\n{large_body}")));

    server.stop().await;
}

#[tokio::test]
async fn rejects_traversal_and_reports_missing_files() {
    let parent = TestRoot::new();
    let root = parent.path.join("public");
    std::fs::create_dir(&root).expect("document root should be created");
    std::fs::write(parent.path.join("secret.txt"), b"secret")
        .expect("outside fixture should be written");
    let server = TestServer::start(root).await;

    let traversal = server
        .request("GET /%2e%2e/secret.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(traversal.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(!traversal.contains("secret\r\n"));

    let encoded_backslash = server
        .request("GET /safe%5csecret.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(encoded_backslash.starts_with("HTTP/1.1 403 Forbidden\r\n"));

    let encoded_nul = server
        .request("GET /safe%00secret.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(encoded_nul.starts_with("HTTP/1.1 403 Forbidden\r\n"));

    #[cfg(windows)]
    {
        let ads = server
            .request(
                "GET /secret.txt:$DATA HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await;
        assert!(ads.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }

    let missing = server
        .request("GET /missing.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(missing.starts_with("HTTP/1.1 404 Not Found\r\n"));

    let method = server
        .request(
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(method.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
    assert!(method.contains("allow: GET, HEAD\r\n"));

    server.stop().await;
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_symlink_escape_when_windows_allows_symlink_creation() {
    use std::os::windows::fs::symlink_file;

    let parent = TestRoot::new();
    let root = parent.path.join("public");
    std::fs::create_dir(&root).expect("document root should be created");
    let outside = parent.path.join("outside.txt");
    std::fs::write(&outside, b"outside").expect("outside fixture should be written");
    if let Err(error) = symlink_file(&outside, root.join("link.txt")) {
        eprintln!("skipping symlink assertion: Windows denied symlink creation: {error}");
        return;
    }
    let server = TestServer::start(root).await;

    let response = server
        .request("GET /link.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));

    server.stop().await;
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_junction_escape_when_windows_allows_junction_creation() {
    use std::process::Command;

    let parent = TestRoot::new();
    let root = parent.path.join("public");
    let outside = parent.path.join("outside");
    let junction = root.join("outside-link");
    std::fs::create_dir(&root).expect("document root should be created");
    std::fs::create_dir(&outside).expect("outside directory should be created");
    std::fs::write(outside.join("secret.txt"), b"junction-secret")
        .expect("outside fixture should be written");

    let output = Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .expect("cmd.exe should run");
    if !output.status.success() {
        eprintln!(
            "skipping junction assertion: mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let server = TestServer::start(root).await;

    let response = server
        .request(
            "GET /outside-link/secret.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(!response.contains("junction-secret"));

    server.stop().await;
    std::fs::remove_dir(&junction).expect("junction should be removed without traversing it");
}

#[tokio::test]
async fn keeps_http_1_connection_alive() {
    let root = TestRoot::new();
    std::fs::write(root.path.join("index.html"), b"Hello, Hamster Wheel!")
        .expect("index fixture should be written");
    let server = TestServer::start(root.path.clone()).await;

    let responses = server
        .request(
            "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n\
             GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(responses.matches("HTTP/1.1 200 OK").count(), 2);
    assert_eq!(responses.matches("Hello, Hamster Wheel!").count(), 2);

    server.stop().await;
}

#[tokio::test]
async fn answers_sequential_keep_alive_requests_without_delayed_ack_stalls() {
    const BODY: &[u8] = b"Hello, Hamster Wheel!";
    const EXCHANGES: usize = 20;
    let root = TestRoot::new();
    std::fs::write(root.path.join("index.html"), BODY).expect("index fixture should be written");
    let server = TestServer::start(root.path.clone()).await;

    // A file response is written as a head and then a body. Without
    // TCP_NODELAY on the accepted socket, Nagle holds the body until the
    // client acknowledges the head, and a client waiting for the rest of the
    // response delays that acknowledgement (40 ms on Linux), so every
    // keep-alive exchange after the first few costs a delayed-ACK period.
    let mut stream = TcpStream::connect(server.address)
        .await
        .expect("client should connect");
    let mut elapsed = Vec::with_capacity(EXCHANGES);
    for _ in 0..EXCHANGES {
        let started = std::time::Instant::now();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("request should be written");
        let mut response = Vec::new();
        let mut buffer = [0; 1024];
        while !response.ends_with(BODY) {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("response should be read");
            assert_ne!(read, 0, "server closed a keep-alive connection");
            response.extend_from_slice(&buffer[..read]);
        }
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        elapsed.push(started.elapsed());
    }
    elapsed.sort();
    let median = elapsed[EXCHANGES / 2];
    assert!(
        median < std::time::Duration::from_millis(20),
        "median keep-alive exchange took {median:?}"
    );

    server.stop().await;
}
