use std::{
    fmt, io,
    net::{IpAddr, SocketAddr},
};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::transport::Scheme;

pub(crate) const PROTOCOL_VERSION: u16 = 3;
pub(crate) const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;
pub(crate) const MAX_HEADERS: usize = 128;

const READY: u8 = 1;
const REQUEST: u8 = 2;
const RESPONSE: u8 = 3;
const SHUTDOWN: u8 = 4;
const SHUTDOWN_ACK: u8 = 5;
const HTTP_10: u8 = 1;
const HTTP_11: u8 = 2;
const SCHEME_HTTP: u8 = 1;
const SCHEME_HTTPS: u8 = 2;
const IPV4: u8 = 4;
const IPV6: u8 = 6;

#[derive(Debug)]
pub(crate) enum ProtocolError {
    Io(io::Error),
    Invalid(String),
    FrameTooLarge(usize),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "protocol I/O: {error}"),
            Self::Invalid(message) => write!(formatter, "invalid protocol: {message}"),
            Self::FrameTooLarge(length) => write!(
                formatter,
                "protocol frame is {length} bytes; maximum is {MAX_FRAME_LEN}"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Invalid(_) | Self::FrameTooLarge(_) => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReadyFrame {
    pub pid: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkerRequest {
    pub http_version: WorkerHttpVersion,
    /// The connection's trusted scheme, never derived from a request header.
    pub scheme: Scheme,
    pub peer_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub method: Vec<u8>,
    pub uri: Vec<u8>,
    pub path: Vec<u8>,
    pub query: Vec<u8>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerHttpVersion {
    Http10,
    Http11,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkerResponse {
    pub request_id: u64,
    pub status: u16,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
}

/// A frame write that failed, and how much of it reached the pipe first.
///
/// `transferred` is what makes the never-retry invariant decidable. A failure
/// at zero transferred bytes proves the peer cannot have seen any part of this
/// frame, so the job it carried never started. Any other count leaves open the
/// possibility that PHP read the frame and began work, and those are never
/// retried.
#[derive(Debug)]
pub(crate) struct FrameWriteError {
    pub source: ProtocolError,
    pub transferred: usize,
}

impl fmt::Display for FrameWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} after {} byte(s) written",
            self.source, self.transferred
        )
    }
}

pub(crate) async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    write_frame_tracked(writer, payload)
        .await
        .map_err(|error| error.source)
}

/// Writes a frame, reporting how many bytes were handed to the writer when it
/// fails. `write_all` cannot answer that question, so the length prefix and the
/// payload are written through one loop that counts its own progress.
pub(crate) async fn write_frame_tracked<W>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), FrameWriteError>
where
    W: AsyncWrite + Unpin,
{
    let too_large = |length: usize| FrameWriteError {
        source: ProtocolError::FrameTooLarge(length),
        transferred: 0,
    };
    if payload.len() > MAX_FRAME_LEN {
        return Err(too_large(payload.len()));
    }
    let length = u32::try_from(payload.len()).map_err(|_| too_large(payload.len()))?;

    let mut transferred = 0_usize;
    for mut chunk in [&length.to_be_bytes()[..], payload] {
        while !chunk.is_empty() {
            match writer.write(chunk).await {
                Ok(0) => {
                    return Err(FrameWriteError {
                        source: ProtocolError::Io(io::Error::from(io::ErrorKind::WriteZero)),
                        transferred,
                    });
                }
                Ok(written) => {
                    transferred += written;
                    chunk = &chunk[written..];
                }
                Err(error) => {
                    return Err(FrameWriteError {
                        source: ProtocolError::Io(error),
                        transferred,
                    });
                }
            }
        }
    }
    writer.flush().await.map_err(|error| FrameWriteError {
        source: ProtocolError::Io(error),
        transferred,
    })?;
    Ok(())
}

pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        return Err(ProtocolError::Invalid("empty frame".to_owned()));
    }
    if length > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

pub(crate) fn decode_ready(payload: &[u8]) -> Result<ReadyFrame, ProtocolError> {
    let mut decoder = Decoder::new(payload);
    decoder.expect_kind(READY, "READY")?;
    let version = decoder.u16()?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::Invalid(format!(
            "unsupported READY protocol version {version}"
        )));
    }
    let pid = decoder.u32()?;
    if pid == 0 {
        return Err(ProtocolError::Invalid("READY PID is zero".to_owned()));
    }
    decoder.finish()?;
    Ok(ReadyFrame { pid })
}

pub(crate) fn encode_request(
    request_id: u64,
    request: &WorkerRequest,
) -> Result<Vec<u8>, ProtocolError> {
    if request.headers.len() > MAX_HEADERS {
        return Err(ProtocolError::Invalid(format!(
            "request has {} headers; maximum is {MAX_HEADERS}",
            request.headers.len()
        )));
    }
    let mut payload = Vec::new();
    payload.push(REQUEST);
    push_u64(&mut payload, request_id);
    payload.push(match request.http_version {
        WorkerHttpVersion::Http10 => HTTP_10,
        WorkerHttpVersion::Http11 => HTTP_11,
    });
    payload.push(match request.scheme {
        Scheme::Http => SCHEME_HTTP,
        Scheme::Https => SCHEME_HTTPS,
    });
    push_endpoint(&mut payload, request.peer_addr);
    push_endpoint(&mut payload, request.local_addr);
    push_bytes_u16(&mut payload, &request.method, "method")?;
    push_bytes_u32(&mut payload, &request.uri, "URI")?;
    push_bytes_u32(&mut payload, &request.path, "path")?;
    push_bytes_u32(&mut payload, &request.query, "query")?;
    push_u16(
        &mut payload,
        u16::try_from(request.headers.len())
            .map_err(|_| ProtocolError::Invalid("too many request headers".to_owned()))?,
    );
    for (name, value) in &request.headers {
        push_bytes_u16(&mut payload, name, "header name")?;
        push_bytes_u32(&mut payload, value, "header value")?;
    }
    push_bytes_u32(&mut payload, &request.body, "body")?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    Ok(payload)
}

fn push_endpoint(output: &mut Vec<u8>, endpoint: SocketAddr) {
    match endpoint.ip() {
        IpAddr::V4(address) => {
            output.push(IPV4);
            output.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            output.push(IPV6);
            output.extend_from_slice(&address.octets());
        }
    }
    push_u16(output, endpoint.port());
}

pub(crate) fn decode_response(payload: &[u8]) -> Result<WorkerResponse, ProtocolError> {
    let mut decoder = Decoder::new(payload);
    decoder.expect_kind(RESPONSE, "RESPONSE")?;
    let request_id = decoder.u64()?;
    let status = decoder.u16()?;
    let header_count = decoder.u16()? as usize;
    if header_count > MAX_HEADERS {
        return Err(ProtocolError::Invalid(format!(
            "response has {header_count} headers; maximum is {MAX_HEADERS}"
        )));
    }
    let mut headers = Vec::with_capacity(header_count);
    for _ in 0..header_count {
        headers.push((decoder.bytes_u16()?.to_vec(), decoder.bytes_u32()?.to_vec()));
    }
    let body = decoder.bytes_u32()?.to_vec();
    decoder.finish()?;
    Ok(WorkerResponse {
        request_id,
        status,
        headers,
        body,
    })
}

pub(crate) fn encode_shutdown() -> [u8; 1] {
    [SHUTDOWN]
}

pub(crate) fn decode_shutdown_ack(payload: &[u8]) -> Result<(), ProtocolError> {
    let mut decoder = Decoder::new(payload);
    decoder.expect_kind(SHUTDOWN_ACK, "SHUTDOWN_ACK")?;
    decoder.finish()
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_bytes_u16(output: &mut Vec<u8>, value: &[u8], field: &str) -> Result<(), ProtocolError> {
    let length = u16::try_from(value.len()).map_err(|_| {
        ProtocolError::Invalid(format!("{field} length {} exceeds u16", value.len()))
    })?;
    push_u16(output, length);
    output.extend_from_slice(value);
    Ok(())
}

fn push_bytes_u32(output: &mut Vec<u8>, value: &[u8], field: &str) -> Result<(), ProtocolError> {
    let length = u32::try_from(value.len()).map_err(|_| {
        ProtocolError::Invalid(format!("{field} length {} exceeds u32", value.len()))
    })?;
    push_u32(output, length);
    output.extend_from_slice(value);
    Ok(())
}

struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    const fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }

    fn expect_kind(&mut self, expected: u8, name: &str) -> Result<(), ProtocolError> {
        let actual = self.bytes(1)?[0];
        if actual != expected {
            return Err(ProtocolError::Invalid(format!(
                "expected {name} frame kind {expected}, received {actual}"
            )));
        }
        Ok(())
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn bytes_u16(&mut self) -> Result<&'a [u8], ProtocolError> {
        let length = self.u16()? as usize;
        self.bytes(length)
    }

    fn bytes_u32(&mut self) -> Result<&'a [u8], ProtocolError> {
        let length = self.u32()? as usize;
        self.bytes(length)
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.position.checked_add(length).ok_or_else(|| {
            ProtocolError::Invalid("field length overflows address space".to_owned())
        })?;
        let value = self.input.get(self.position..end).ok_or_else(|| {
            ProtocolError::Invalid(format!(
                "truncated payload at byte {} while reading {length} bytes",
                self.position
            ))
        })?;
        self.position = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| ProtocolError::Invalid("invalid integer width".to_owned()))
    }

    fn finish(self) -> Result<(), ProtocolError> {
        if self.position == self.input.len() {
            Ok(())
        } else {
            Err(ProtocolError::Invalid(format!(
                "{} trailing payload bytes",
                self.input.len() - self.position
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn decode_endpoint(decoder: &mut Decoder<'_>) -> SocketAddr {
        let family = decoder.bytes(1).unwrap()[0];
        let address = match family {
            IPV4 => IpAddr::V4(Ipv4Addr::from(decoder.array::<4>().unwrap())),
            IPV6 => IpAddr::V6(Ipv6Addr::from(decoder.array::<16>().unwrap())),
            _ => panic!("unexpected endpoint family"),
        };
        SocketAddr::new(address, decoder.u16().unwrap())
    }

    #[test]
    fn request_encoding_is_binary_safe_and_preserves_repeated_headers() {
        let request = WorkerRequest {
            http_version: WorkerHttpVersion::Http10,
            scheme: Scheme::Https,
            peer_addr: "127.0.0.2:43210".parse().unwrap(),
            local_addr: "[2001:db8::1]:8080".parse().unwrap(),
            method: b"POST".to_vec(),
            uri: b"/echo?q=a%0Ab".to_vec(),
            path: b"/echo".to_vec(),
            query: b"q=a%0Ab".to_vec(),
            headers: vec![
                (b"x-repeat".to_vec(), b"one".to_vec()),
                (b"x-repeat".to_vec(), b"two\0\nthree".to_vec()),
            ],
            body: vec![0, 10, 13, 255],
        };
        let encoded = encode_request(7, &request).unwrap();
        let mut decoder = Decoder::new(&encoded);
        decoder.expect_kind(REQUEST, "REQUEST").unwrap();
        assert_eq!(decoder.u64().unwrap(), 7);
        assert_eq!(decoder.bytes(1).unwrap(), [HTTP_10]);
        assert_eq!(decoder.bytes(1).unwrap(), [SCHEME_HTTPS]);
        assert_eq!(decode_endpoint(&mut decoder), request.peer_addr);
        assert_eq!(decode_endpoint(&mut decoder), request.local_addr);
        assert_eq!(decoder.bytes_u16().unwrap(), request.method);
        assert_eq!(decoder.bytes_u32().unwrap(), request.uri);
        assert_eq!(decoder.bytes_u32().unwrap(), request.path);
        assert_eq!(decoder.bytes_u32().unwrap(), request.query);
        let header_count = decoder.u16().unwrap();
        assert_eq!(header_count, 2);
        for expected in &request.headers {
            assert_eq!(decoder.bytes_u16().unwrap(), expected.0);
            assert_eq!(decoder.bytes_u32().unwrap(), expected.1);
        }
        assert_eq!(decoder.bytes_u32().unwrap(), request.body);
        decoder.finish().unwrap();
        assert!(encoded.windows(4).any(|bytes| bytes == [0, 10, 13, 255]));
    }

    #[test]
    fn ready_rejects_every_version_except_v3() {
        // Version 2 is refused like any other: a v2 worker would read the
        // scheme byte as an address family and fail mid-request instead.
        for version in [0_u16, 1, 2, 4, u16::MAX] {
            let mut ready = vec![READY];
            push_u16(&mut ready, version);
            push_u32(&mut ready, 123);
            let error = decode_ready(&ready).expect_err("version mismatch must fail");
            assert!(error.to_string().contains(&format!("version {version}")));
        }

        let mut ready = vec![READY];
        push_u16(&mut ready, PROTOCOL_VERSION);
        push_u32(&mut ready, 123);
        assert_eq!(decode_ready(&ready).unwrap().pid, 123);
    }

    #[test]
    fn response_decoder_rejects_truncation_and_trailing_bytes() {
        let mut response = vec![RESPONSE];
        push_u64(&mut response, 1);
        push_u16(&mut response, 200);
        push_u16(&mut response, 0);
        push_u32(&mut response, 4);
        response.extend_from_slice(b"abc");
        assert!(decode_response(&response).is_err());

        response.push(b'd');
        response.push(0);
        assert!(decode_response(&response).is_err());
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversized_and_truncated_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(ProtocolError::FrameTooLarge(_))
        ));

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&5_u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"ab").await.unwrap();
        drop(writer);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(ProtocolError::Io(_))
        ));
    }

    /// A writer that accepts a fixed number of bytes and then fails, so a
    /// partial frame write can be observed without a real pipe.
    struct FailingWriter {
        accept: usize,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            buffer: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            if self.accept == 0 {
                return std::task::Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
            }
            let written = buffer.len().min(self.accept);
            self.accept -= written;
            std::task::Poll::Ready(Ok(written))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn frame_write_reports_whether_any_byte_reached_the_peer() {
        // Nothing was transferred, so the peer provably never saw this frame.
        let error = write_frame_tracked(&mut FailingWriter { accept: 0 }, b"payload")
            .await
            .expect_err("a writer that refuses everything must fail");
        assert_eq!(error.transferred, 0);

        // The length prefix went out, so the peer may already have started
        // reading. This is exactly the case that must never be retried.
        let error = write_frame_tracked(&mut FailingWriter { accept: 4 }, b"payload")
            .await
            .expect_err("a writer that fails mid-frame must fail");
        assert_eq!(error.transferred, 4);

        let error = write_frame_tracked(&mut FailingWriter { accept: 6 }, b"payload")
            .await
            .expect_err("a writer that fails inside the payload must fail");
        assert_eq!(error.transferred, 6);

        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_frame_tracked(&mut writer, b"payload").await.unwrap();
        assert_eq!(read_frame(&mut reader).await.unwrap(), b"payload");
    }
}
