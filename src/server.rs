use std::{fmt, future::Future, io, sync::Arc};

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

use crate::{config::RuntimeConfig, php_worker::PhpWorkerError, router::HostRouter};

#[derive(Debug)]
pub enum ServerError {
    Io(io::Error),
    Worker(PhpWorkerError),
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "server I/O: {error}"),
            Self::Worker(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Worker(error) => Some(error),
        }
    }
}

impl From<io::Error> for ServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PhpWorkerError> for ServerError {
    fn from(error: PhpWorkerError) -> Self {
        Self::Worker(error)
    }
}

pub async fn run(config: Arc<RuntimeConfig>) -> Result<(), ServerError> {
    for worker in config.start_workers().await? {
        println!(
            "PHP worker {} ready after {:.3} ms",
            worker.pid,
            worker.elapsed.as_secs_f64() * 1_000.0
        );
    }
    let address = config.listen_address();
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = config.shutdown_workers().await;
            return Err(ServerError::Io(error));
        }
    };
    let shutdown = match shutdown_signal() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            let _ = config.shutdown_workers().await;
            return Err(ServerError::Io(error));
        }
    };
    println!("Hamster Wheel listening on http://{address}");

    let router = config.router_arc();
    let serve_result = serve_with_router(listener, router, shutdown).await;
    let shutdown_result = config.shutdown_workers().await;
    serve_result?;
    shutdown_result?;
    Ok(())
}

pub async fn serve_with_router<F>(
    listener: TcpListener,
    router: Arc<HostRouter>,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                let shutdown_rx = shutdown_rx.clone();
                let router = Arc::clone(&router);

                connections.spawn(async move {
                    serve_connection(stream, router, shutdown_rx).await;
                });
            }
            join_result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = join_result {
                    eprintln!("connection task failed: {error}");
                }
            }
            () = &mut shutdown => break,
        }
    }

    drop(listener);
    let _ = shutdown_tx.send(true);

    while let Some(join_result) = connections.join_next().await {
        if let Err(error) = join_result {
            eprintln!("connection task failed during shutdown: {error}");
        }
    }

    println!("Hamster Wheel stopped");
    Ok(())
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    router: Arc<HostRouter>,
    mut shutdown: watch::Receiver<bool>,
) {
    let service = hyper::service::service_fn(move |request| {
        let router = Arc::clone(&router);
        async move { router.serve(request).await }
    });
    let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);

    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result {
                eprintln!("failed to serve connection: {error}");
            }
        }
        changed = shutdown.changed() => {
            if changed.is_ok() && *shutdown.borrow() {
                connection.as_mut().graceful_shutdown();
                if let Err(error) = connection.await {
                    eprintln!("failed to gracefully close connection: {error}");
                }
            }
        }
    }
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}

#[cfg(not(windows))]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    Ok(async move {
        let _ = signal.recv().await;
    })
}
