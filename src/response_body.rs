use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, SizeHint};
use tokio::{
    fs::File,
    io::{AsyncRead, ReadBuf},
};

use crate::proxy::ProxyBody;

/// Maximum application buffer produced for one static-file body frame.
///
/// Tokio's file implementation also has an internal buffer. The same limit is
/// applied to it when constructing `FileBody`, so a nominally small Hamster
/// frame cannot conceal Tokio's 2 MiB default buffer per open file.
pub(crate) const STATIC_FILE_CHUNK_SIZE: usize = 128 * 1_024;

/// The concrete response shapes Hamster serves today.
///
/// Keeping this as an enum makes the existing buffered PHP and small-response
/// path explicit while allowing static files to stream. This is not a backend
/// extension point; new variants require the same ownership and resource
/// review as this file variant.
///
/// `Proxy` is boxed so the reverse proxy's lease and deadline do not widen the
/// enum that every static and PHP response is moved through.
#[derive(Debug)]
pub(crate) enum ResponseBody {
    Buffered(Full<Bytes>),
    File(FileBody),
    Proxy(Box<ProxyBody>),
}

impl ResponseBody {
    pub(crate) fn buffered(bytes: Bytes) -> Self {
        Self::Buffered(Full::new(bytes))
    }

    pub(crate) fn proxy(body: ProxyBody) -> Self {
        Self::Proxy(Box::new(body))
    }

    pub(crate) fn file(mut file: File, length: u64) -> Self {
        file.set_max_buf_size(STATIC_FILE_CHUNK_SIZE);
        Self::File(FileBody {
            file,
            remaining: length,
            buffer: Vec::new(),
            finished: length == 0,
        })
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::Buffered(body) => match Pin::new(body).poll_frame(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
                Poll::Ready(Some(Err(error))) => match error {},
                Poll::Ready(None) => Poll::Ready(None),
            },
            Self::File(body) => Pin::new(body).poll_frame(cx),
            Self::Proxy(body) => Pin::new(body.as_mut()).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Buffered(body) => body.is_end_stream(),
            Self::File(body) => body.is_end_stream(),
            Self::Proxy(body) => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Buffered(body) => body.size_hint(),
            Self::File(body) => body.size_hint(),
            Self::Proxy(body) => body.size_hint(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct FileBody {
    file: File,
    remaining: u64,
    /// Storage for the read currently being polled. It must survive a Pending
    /// result even though Tokio's file implementation performs the OS read on
    /// its blocking pool on Windows.
    buffer: Vec<u8>,
    finished: bool,
}

impl FileBody {
    fn fail(&mut self, error: io::Error) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.buffer.clear();
        self.remaining = 0;
        self.finished = true;
        Poll::Ready(Some(Err(error)))
    }
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }

        let read_length = usize::try_from(self.remaining.min(STATIC_FILE_CHUNK_SIZE as u64))
            .expect("a chunk length always fits usize");
        self.buffer.resize(read_length, 0);

        let mut buffer = std::mem::take(&mut self.buffer);
        let mut read_buffer = ReadBuf::new(&mut buffer);
        match Pin::new(&mut self.file).poll_read(cx, &mut read_buffer) {
            Poll::Pending => {
                self.buffer = buffer;
                Poll::Pending
            }
            Poll::Ready(Err(error)) => {
                self.buffer = buffer;
                self.fail(error)
            }
            Poll::Ready(Ok(())) => {
                let read = read_buffer.filled().len();
                if read == 0 {
                    self.buffer = buffer;
                    let missing = self.remaining;
                    return self.fail(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("static file ended with {missing} response bytes remaining"),
                    ));
                }

                buffer.truncate(read);
                self.remaining -= read as u64;
                self.finished = self.remaining == 0;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(buffer)))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        async fn with_bytes(bytes: &[u8]) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hamster-wheel-response-body-{}-{id}",
                std::process::id()
            ));
            let mut file = File::create(&path).await.expect("create body fixture");
            file.write_all(bytes).await.expect("write body fixture");
            file.flush().await.expect("flush body fixture");
            drop(file);
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            std::fs::remove_file(&self.path).expect("remove body fixture");
        }
    }

    #[tokio::test]
    async fn file_body_streams_exact_bounded_frames() {
        let expected = vec![b'x'; STATIC_FILE_CHUNK_SIZE * 2 + 17];
        let fixture = TempFile::with_bytes(&expected).await;
        let file = File::open(&fixture.path).await.expect("open body fixture");
        let mut body = ResponseBody::file(file, expected.len() as u64);
        let mut actual = Vec::new();
        let mut frame_lengths = Vec::new();

        assert_eq!(body.size_hint().exact(), Some(expected.len() as u64));
        while let Some(frame) = body.frame().await {
            let data = frame
                .expect("file read should succeed")
                .into_data()
                .expect("file bodies only contain data");
            frame_lengths.push(data.len());
            actual.extend_from_slice(&data);
        }

        assert_eq!(actual, expected);
        assert_eq!(
            frame_lengths,
            [STATIC_FILE_CHUNK_SIZE, STATIC_FILE_CHUNK_SIZE, 17]
        );
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
    }

    #[tokio::test]
    async fn early_eof_fails_once_and_then_ends() {
        let fixture = TempFile::with_bytes(b"short").await;
        let file = File::open(&fixture.path).await.expect("open body fixture");
        let mut body = ResponseBody::file(file, 10);

        let first = body
            .frame()
            .await
            .expect("first frame")
            .expect("first read succeeds")
            .into_data()
            .expect("data frame");
        assert_eq!(first, Bytes::from_static(b"short"));

        let error = body
            .frame()
            .await
            .expect("error frame")
            .expect_err("short file must not become a valid short response");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(body.is_end_stream());
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn dropping_body_releases_the_open_file() {
        let fixture = TempFile::with_bytes(b"owned").await;
        let file = File::open(&fixture.path).await.expect("open body fixture");
        let body = ResponseBody::file(file, 5);
        drop(body);

        // This assertion is strongest on Windows, where a leaked or retained
        // handle commonly prevents replacement. It is still a useful ownership
        // check on Unix and mirrors what disconnect does by dropping the body.
        let replacement = fixture.path.with_extension("replacement");
        std::fs::rename(&fixture.path, &replacement).expect("body drop releases file ownership");
        std::fs::rename(&replacement, &fixture.path).expect("restore fixture name");
    }
}
