use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{CONTENT_TYPE, HeaderValue, SERVER},
};

#[cfg(test)]
use crate::body_budget::BodyBudget;
use crate::{
    config::SERVER_NAME, php_worker::PhpWorkerPool, static_files, transport::TransportInfo,
};

pub(crate) type HandlerResponse = Response<Full<Bytes>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    Static,
    Simple,
    PhpWorker,
    #[cfg(test)]
    TransportEcho,
    #[cfg(test)]
    BodyEcho,
}

#[derive(Clone)]
pub enum Handler {
    Static {
        document_root: Arc<PathBuf>,
    },
    Simple {
        status: StatusCode,
        body: &'static str,
    },
    PhpWorker {
        pool: Arc<PhpWorkerPool>,
    },
    #[cfg(test)]
    TransportEcho,
    /// Reconstructs a request body through the production PHP body path and
    /// echoes it, without needing a PHP distribution. `hold` keeps the body
    /// budget reservation alive after the response, so a test can observe the
    /// aggregate ceiling over real sockets.
    #[cfg(test)]
    BodyEcho {
        budget: Arc<BodyBudget>,
        hold: Option<Duration>,
    },
}

impl Handler {
    pub fn static_files(document_root: Arc<PathBuf>) -> Self {
        Self::Static { document_root }
    }

    pub const fn simple(status: StatusCode, body: &'static str) -> Self {
        Self::Simple { status, body }
    }

    pub const fn not_found() -> Self {
        Self::simple(StatusCode::NOT_FOUND, "Not Found")
    }

    pub fn php_worker(pool: Arc<PhpWorkerPool>) -> Self {
        Self::PhpWorker { pool }
    }

    #[cfg(test)]
    pub(crate) const fn transport_echo() -> Self {
        Self::TransportEcho
    }

    #[cfg(test)]
    pub(crate) fn body_echo(budget: Arc<BodyBudget>, hold: Option<Duration>) -> Self {
        Self::BodyEcho { budget, hold }
    }

    pub const fn kind(&self) -> HandlerKind {
        match self {
            Self::Static { .. } => HandlerKind::Static,
            Self::Simple { .. } => HandlerKind::Simple,
            Self::PhpWorker { .. } => HandlerKind::PhpWorker,
            #[cfg(test)]
            Self::TransportEcho => HandlerKind::TransportEcho,
            #[cfg(test)]
            Self::BodyEcho { .. } => HandlerKind::BodyEcho,
        }
    }

    pub(crate) async fn serve(
        &self,
        request: Request<Incoming>,
        _transport: TransportInfo,
        body_read_timeout: Duration,
    ) -> Result<HandlerResponse, Infallible> {
        match self {
            Self::Static { document_root } => {
                static_files::serve(request, Arc::clone(document_root)).await
            }
            Self::Simple { status, body } => Ok(simple_response(*status, body)),
            Self::PhpWorker { pool } => Ok(pool.serve_http(request, body_read_timeout).await),
            #[cfg(test)]
            Self::TransportEcho => Ok(transport_response(_transport)),
            #[cfg(test)]
            Self::BodyEcho { budget, hold } => {
                Ok(crate::php_worker::PhpWorkerPool::test_body_echo(
                    request,
                    body_read_timeout,
                    budget,
                    *hold,
                )
                .await)
            }
        }
    }

    #[cfg(feature = "profiling")]
    pub(crate) fn profiling_response(&self) -> HandlerResponse {
        match self {
            Self::Simple { status, body } => simple_response(*status, body),
            Self::Static { .. } => simple_response(StatusCode::OK, "Static"),
            Self::PhpWorker { .. } => simple_response(StatusCode::OK, "PHP Worker"),
            #[cfg(test)]
            Self::TransportEcho => simple_response(StatusCode::OK, "Transport Echo"),
            #[cfg(test)]
            Self::BodyEcho { .. } => simple_response(StatusCode::OK, "Body Echo"),
        }
    }
}

#[cfg(test)]
fn transport_response(transport: TransportInfo) -> HandlerResponse {
    let body = Bytes::from(format!(
        "peer={};local={}",
        transport.peer_addr(),
        transport.local_addr()
    ));
    let mut response = Response::new(Full::new(body));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static(SERVER_NAME));
    response
}

pub(crate) fn simple_response(status: StatusCode, body: &'static str) -> HandlerResponse {
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static(SERVER_NAME));
    response
}
