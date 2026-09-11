use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use hamster_wheel::{
    router::{Handler, HostRouter, LocationRouter, VirtualHost},
    server,
};
use hyper::StatusCode;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

struct TestServer {
    address: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(router: HostRouter) -> Self {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            server::serve_with_router(listener, Arc::new(router), async {
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

fn router() -> HostRouter {
    let locations = Arc::new(
        LocationRouter::new(
            vec![
                ("/", Handler::simple(StatusCode::OK, "root")),
                ("/api", Handler::simple(StatusCode::OK, "api")),
                ("/api/admin", Handler::simple(StatusCode::OK, "admin")),
            ],
            Handler::not_found(),
        )
        .expect("locations should compile"),
    );
    HostRouter::new(vec![
        VirtualHost::new("example.test", Arc::clone(&locations)).unwrap(),
        VirtualHost::new("localhost", locations).unwrap(),
    ])
    .expect("hosts should compile")
}

#[tokio::test]
async fn routes_host_case_and_port_and_rejects_unknown_host() {
    let server = TestServer::start(router()).await;

    let known = server
        .request("GET / HTTP/1.1\r\nHost: EXAMPLE.TEST:8080\r\nConnection: close\r\n\r\n")
        .await;
    assert!(known.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(known.ends_with("\r\n\r\nroot"));

    let unknown = server
        .request("GET / HTTP/1.1\r\nHost: unknown.test\r\nConnection: close\r\n\r\n")
        .await;
    assert!(unknown.starts_with("HTTP/1.1 404 Not Found\r\n"));
    assert!(unknown.ends_with("\r\n\r\nUnknown Host"));

    let invalid = server
        .request("GET / HTTP/1.1\r\nHost: example.test:99999\r\nConnection: close\r\n\r\n")
        .await;
    assert!(invalid.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(invalid.ends_with("\r\n\r\nInvalid Host"));

    server.stop().await;
}

#[tokio::test]
async fn uses_longest_location_prefix_and_segment_boundaries() {
    let server = TestServer::start(router()).await;

    for (path, expected) in [
        ("/", "root"),
        ("/api/users", "api"),
        ("/api/admin/users", "admin"),
        ("/apix", "root"),
    ] {
        let response = server
            .request(&format!(
                "GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n"
            ))
            .await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{path}");
        assert!(response.ends_with(&format!("\r\n\r\n{expected}")), "{path}");
    }

    server.stop().await;
}

#[tokio::test]
async fn keeps_connection_alive_through_router() {
    let server = TestServer::start(router()).await;
    let responses = server
        .request(
            "GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n\
             GET /api/admin HTTP/1.1\r\nHost: LOCALHOST:80\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(responses.matches("HTTP/1.1 200 OK").count(), 2);
    assert!(responses.contains("\r\n\r\napiHTTP/1.1 200 OK"));
    assert!(responses.ends_with("\r\n\r\nadmin"));
    server.stop().await;
}

#[tokio::test]
async fn missing_host_is_rejected() {
    let server = TestServer::start(router()).await;
    let response = server
        .request("GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
        .await;
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    server.stop().await;
}
