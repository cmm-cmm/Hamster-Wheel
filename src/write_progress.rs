use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep, sleep_until},
};

/// An inactivity deadline that runs only while an I/O direction is stalled.
///
/// This is the one definition of "no progress" in the server. The first
/// stalled poll arms the deadline, repeated stalled polls keep the original
/// deadline, and any progress disarms it so a later stall receives a fresh
/// interval. The timer is allocated lazily and then reused, so an I/O path that
/// never stalls owns no Tokio timer and one that stalls owns at most one.
///
/// Callers poll their inner I/O first and consult this only on `Pending`, so
/// if progress and the deadline become ready together, progress wins.
pub(crate) struct ProgressDeadline {
    timeout: Duration,
    timer: Option<Pin<Box<Sleep>>>,
    stalled: bool,
}

impl ProgressDeadline {
    pub(crate) const fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            timer: None,
            stalled: false,
        }
    }

    pub(crate) const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Records progress: the current stall, if any, is over.
    pub(crate) fn progress(&mut self) {
        self.stalled = false;
    }

    /// Records that the inner I/O returned `Pending`.
    ///
    /// Returns `Ready` once the stall has lasted the whole interval. The waker
    /// is registered with the timer, so the caller is polled again when the
    /// deadline passes even if the inner I/O never wakes it.
    pub(crate) fn poll_stalled(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.stalled {
            let deadline = Instant::now() + self.timeout;
            match self.timer.as_mut() {
                Some(timer) => timer.as_mut().reset(deadline),
                None => self.timer = Some(Box::pin(sleep_until(deadline))),
            }
            self.stalled = true;
        }

        match self
            .timer
            .as_mut()
            .expect("a stalled direction always has a timer")
            .as_mut()
            .poll(cx)
        {
            Poll::Ready(()) => {
                self.stalled = false;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Applies an inactivity deadline only while a socket write is backpressured.
///
/// A successful write of at least one byte is progress. `Pending` starts the
/// deadline, and repeated `Pending` polls keep the original deadline. Reads,
/// keep-alive idle time, and application execution do not arm this timer.
pub(crate) struct WriteProgressTimeout<T> {
    inner: T,
    deadline: ProgressDeadline,
}

impl<T> WriteProgressTimeout<T> {
    pub(crate) const fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: ProgressDeadline::new(timeout),
        }
    }

    /// The wrapped I/O, for a caller that keeps it beyond this deadline's use.
    pub(crate) fn into_inner(self) -> T {
        self.inner
    }

    fn observe_write(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        match result {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.deadline.progress();
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => match self.deadline.poll_stalled(cx) {
                Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "socket write made no progress for {:.3} seconds",
                        self.deadline.timeout().as_secs_f64()
                    ),
                ))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteProgressTimeout<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteProgressTimeout<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buffer);
        self.observe_write(cx, result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, buffers);
        self.observe_write(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn times_out_only_after_a_write_stops_making_progress() {
        let (writer, _reader) = tokio::io::duplex(1);
        let mut writer = WriteProgressTimeout::new(writer, Duration::from_millis(50));

        let error = writer
            .write_all(b"two bytes")
            .await
            .expect_err("a full peer buffer must eventually time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn slow_continuous_progress_does_not_time_out() {
        let (writer, mut reader) = tokio::io::duplex(1);
        let mut writer = WriteProgressTimeout::new(writer, Duration::from_millis(100));
        let expected = b"slow progress";

        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            let mut byte = [0_u8; 1];
            while received.len() < expected.len() {
                tokio::time::sleep(Duration::from_millis(25)).await;
                reader.read_exact(&mut byte).await.expect("read one byte");
                received.push(byte[0]);
            }
            received
        });

        tokio::time::timeout(Duration::from_secs(1), writer.write_all(expected))
            .await
            .expect("progressing writer should finish before the outer guard")
            .expect("progressing writer should not time out");
        assert_eq!(reader.await.expect("reader task"), expected);
    }

    #[tokio::test]
    async fn read_inactivity_does_not_arm_the_write_timer() {
        let (stream, _peer) = tokio::io::duplex(1);
        let mut stream = WriteProgressTimeout::new(stream, Duration::from_millis(20));
        let mut byte = [0_u8; 1];

        assert!(
            tokio::time::timeout(Duration::from_millis(60), stream.read_exact(&mut byte))
                .await
                .is_err(),
            "the outer guard, not the write-progress policy, owns idle reads"
        );
    }
}
