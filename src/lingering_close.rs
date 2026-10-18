//! Lingering close for a connection that answered before reading its request.
//!
//! Hamster answers some requests without reading their body: a declared body
//! over the PHP limit (413), over the body memory budget (503), and any
//! handler that responds first. Hyper then drains at most one buffer and closes
//! the socket with the rest of the body unread. Closing a TCP socket with
//! unread received data makes the kernel send a reset instead of a FIN, and a
//! client still sending its body -- .NET's HttpClient, measured 10 of 10 times
//! with a 2 MiB upload -- sees "connection reset" and never reads the response
//! it has already been sent. curl reads while it sends and was unaffected.
//!
//! This wrapper sits directly on the socket. Reads and writes pass through
//! untouched. When Hyper shuts the connection down, it sends FIN first and
//! then, if the client may still be sending, keeps reading and discarding
//! until the client closes, stops sending for `LINGER_IDLE`, or the server's
//! `body_read_timeout` passes. The client, having received the response and
//! FIN, finishes sending and reads the response.
//!
//! The total bound is `body_read_timeout` because it is already the time
//! Hamster allows a client to deliver a body it accepts; a refused body gets
//! no longer. There is no byte bound. An earlier 16 MiB bound turned every
//! larger refused upload back into a reset (.NET: 413 in 10 of 10 at 15 MiB,
//! reset in 10 of 10 at 17, 32 and 100 MiB), while discarding 15 MiB took
//! about 7 ms on loopback; time is what bounds the cost of a connection slot.
//!
//! "May still be sending" is decided from HTTP, not only from the socket: the
//! connection's last request declared a body (`CloseSignals`, set by the
//! service for every request), or bytes are already waiting. Waiting bytes
//! alone were measured to be insufficient: Hyper drains one buffer before
//! closing, so at that instant the socket was empty in 3 of 4 .NET uploads and
//! the next segment still drew a reset. A connection whose last request had no
//! body, and nothing waiting, closes at once; so does a client that closes
//! when it sees the FIN, which is what HTTP clients do.
//!
//! Bounds on what lingering can cost: a client that stops sending is dropped
//! after `LINGER_IDLE` (the role of nginx's `lingering_timeout`), so a stalled upload
//! refused with 408 keeps its slot one idle interval longer, not the whole
//! timeout. Once server shutdown has begun, a closing connection does not
//! linger at all: nobody will read the body, and shutdown must not wait for
//! it. A connection already lingering when shutdown begins is still bounded by
//! the graceful drain deadline.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, ready},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep},
};

/// Longest a lingering connection waits for the client's next bytes.
pub(crate) const LINGER_IDLE: Duration = Duration::from_secs(1);

/// What the HTTP layer tells the socket about the connection it is closing.
/// One allocation per connection; one relaxed store per request.
#[derive(Default)]
pub(crate) struct CloseSignals {
    request_body: AtomicBool,
    stopping: AtomicBool,
}

impl CloseSignals {
    /// Whether the connection's latest request declared a body.
    pub(crate) fn request_declared_body(&self, declared: bool) {
        self.request_body.store(declared, Ordering::Release);
    }

    /// Server shutdown has begun; a connection closing now does not linger.
    pub(crate) fn server_stopping(&self) {
        self.stopping.store(true, Ordering::Release);
    }
}

pub(crate) struct LingeringClose<T> {
    inner: T,
    state: Linger,
    signals: Arc<CloseSignals>,
    /// Total linger bound: the server's `body_read_timeout`.
    timeout: Duration,
}

enum Linger {
    Open,
    /// FIN sent and the client may still be sending: drain until a bound.
    /// The timers are allocated only here, never on an ordinary connection.
    Draining {
        deadline: Pin<Box<Sleep>>,
        idle: Pin<Box<Sleep>>,
    },
    Closed,
}

impl Linger {
    fn draining(timeout: Duration) -> Self {
        Self::Draining {
            deadline: Box::pin(tokio::time::sleep(timeout)),
            idle: Box::pin(tokio::time::sleep(LINGER_IDLE)),
        }
    }
}

impl<T> LingeringClose<T> {
    pub(crate) fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            state: Linger::Open,
            signals: Arc::default(),
            timeout,
        }
    }

    pub(crate) fn signals(&self) -> Arc<CloseSignals> {
        Arc::clone(&self.signals)
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for LingeringClose<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

/// One read into a scratch buffer: `Some(n)` bytes, `None` at end or error.
fn discard<T: AsyncRead + Unpin>(inner: &mut T, cx: &mut Context<'_>) -> Poll<Option<usize>> {
    let mut scratch = [0_u8; 8 * 1024];
    let mut buffer = ReadBuf::new(&mut scratch);
    match Pin::new(inner).poll_read(cx, &mut buffer) {
        Poll::Ready(Ok(())) if !buffer.filled().is_empty() => {
            Poll::Ready(Some(buffer.filled().len()))
        }
        Poll::Ready(_) => Poll::Ready(None),
        Poll::Pending => Poll::Pending,
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for LingeringClose<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            match &mut this.state {
                Linger::Open => {
                    ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
                    if this.signals.stopping.load(Ordering::Acquire) {
                        this.state = Linger::Closed;
                        continue;
                    }
                    let body_declared = this.signals.request_body.load(Ordering::Acquire);
                    this.state = match discard(&mut this.inner, cx) {
                        Poll::Ready(Some(_)) => Linger::draining(this.timeout),
                        Poll::Pending if body_declared => Linger::draining(this.timeout),
                        Poll::Ready(None) | Poll::Pending => Linger::Closed,
                    };
                }
                Linger::Draining { deadline, idle } => {
                    if deadline.as_mut().poll(cx).is_ready() || idle.as_mut().poll(cx).is_ready() {
                        this.state = Linger::Closed;
                        continue;
                    }
                    match discard(&mut this.inner, cx) {
                        Poll::Ready(Some(_)) => {
                            idle.as_mut().reset(Instant::now() + LINGER_IDLE);
                        }
                        Poll::Ready(None) => this.state = Linger::Closed,
                        // The socket and both timers are registered.
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Linger::Closed => return Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests;
