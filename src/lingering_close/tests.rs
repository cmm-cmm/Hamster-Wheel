//! Each lingering rule on a real loopback socket pair: the wrapper is the
//! server side, the test drives the peer.

use std::time::{Duration, Instant};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use super::{LINGER_IDLE, LingeringClose};

async fn pair(timeout: Duration) -> (LingeringClose<TcpStream>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (LingeringClose::new(server, timeout), peer)
}

/// Runs the wrapper's shutdown and reports how long it took.
async fn close(server: &mut LingeringClose<TcpStream>) -> Duration {
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(20), server.shutdown())
        .await
        .expect("shutdown is bounded")
        .expect("shutdown");
    started.elapsed()
}

#[tokio::test]
async fn closes_at_once_without_a_declared_body_or_waiting_bytes() {
    let (mut server, _peer) = pair(Duration::from_secs(10)).await;
    let took = close(&mut server).await;
    assert!(took < Duration::from_millis(200), "lingered {took:?}");
}

#[tokio::test]
async fn does_not_linger_once_the_server_is_stopping() {
    let (mut server, _peer) = pair(Duration::from_secs(10)).await;
    server.signals().request_declared_body(true);
    server.signals().server_stopping();
    let took = close(&mut server).await;
    assert!(took < Duration::from_millis(200), "lingered {took:?}");
}

#[tokio::test]
async fn a_declared_body_is_drained_until_the_client_closes() {
    let (mut server, mut peer) = pair(Duration::from_secs(10)).await;
    server.signals().request_declared_body(true);
    let sender = tokio::spawn(async move {
        // Keeps sending after the server's FIN, then closes. Every write must
        // succeed: the server is still reading, so nothing resets.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            peer.write_all(&[b'x'; 64 * 1024]).await?;
        }
        let mut rest = Vec::new();
        peer.read_to_end(&mut rest).await?;
        drop(peer);
        std::io::Result::Ok(rest.len())
    });
    let took = close(&mut server).await;
    assert_eq!(sender.await.unwrap().expect("no reset while sending"), 0);
    assert!(
        took >= Duration::from_millis(400) && took < LINGER_IDLE + Duration::from_millis(500),
        "drained for {took:?}"
    );
}

#[tokio::test]
async fn a_client_that_stops_sending_is_dropped_after_the_idle_bound() {
    let (mut server, _peer) = pair(Duration::from_secs(10)).await;
    server.signals().request_declared_body(true);
    let took = close(&mut server).await;
    assert!(
        took >= LINGER_IDLE - Duration::from_millis(50) && took < LINGER_IDLE * 2,
        "lingered {took:?} for an idle bound of {LINGER_IDLE:?}"
    );
}

#[tokio::test]
async fn a_client_that_keeps_sending_is_cut_at_the_total_bound() {
    let total = Duration::from_millis(400);
    let (mut server, mut peer) = pair(total).await;
    server.signals().request_declared_body(true);
    let sender = tokio::spawn(async move {
        while peer.write_all(&[b'x'; 1024]).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let took = close(&mut server).await;
    sender.abort();
    assert!(
        took >= total - Duration::from_millis(50) && took < total + Duration::from_millis(400),
        "lingered {took:?} for a total bound of {total:?}"
    );
}
