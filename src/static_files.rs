use std::{convert::Infallible, io, path::PathBuf, sync::Arc};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{ALLOW, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, SERVER},
};

use crate::config::SERVER_NAME;

const INDEX_FILE: &str = "index.html";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathError {
    InvalidEncoding,
    Traversal,
}

#[derive(Debug)]
enum FileError {
    BadRequest,
    Forbidden,
    NotFound,
    Io(io::Error),
}

struct ResolvedFile {
    path: PathBuf,
    length: u64,
}

pub async fn serve(
    request: Request<Incoming>,
    document_root: Arc<PathBuf>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let is_head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !is_head {
        let mut response = text_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return Ok(response);
    }

    let response = match resolve_file(&document_root, request.uri().path()).await {
        Ok(file) if is_head => file_response(&file.path, Bytes::new(), file.length),
        Ok(file) => match tokio::fs::read(&file.path).await {
            Ok(contents) => {
                let content_length = contents.len() as u64;
                file_response(&file.path, Bytes::from(contents), content_length)
            }
            Err(error) => error_response(map_io_error(error)),
        },
        Err(error) => error_response(error),
    };

    Ok(response)
}

async fn resolve_file(document_root: &PathBuf, uri_path: &str) -> Result<ResolvedFile, FileError> {
    let relative = normalize_path(uri_path).map_err(|error| match error {
        PathError::InvalidEncoding => FileError::BadRequest,
        PathError::Traversal => FileError::Forbidden,
    })?;
    let mut candidate = document_root.join(relative);

    let metadata = tokio::fs::metadata(&candidate)
        .await
        .map_err(map_io_error)?;
    if metadata.is_dir() {
        candidate.push(INDEX_FILE);
    }

    let canonical = tokio::fs::canonicalize(candidate)
        .await
        .map_err(map_io_error)?;
    if !canonical.starts_with(document_root) {
        return Err(FileError::Forbidden);
    }

    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(FileError::NotFound);
    }

    Ok(ResolvedFile {
        path: canonical,
        length: metadata.len(),
    })
}

pub(crate) fn normalize_path(uri_path: &str) -> Result<PathBuf, PathError> {
    if !uri_path.starts_with('/') {
        return Err(PathError::InvalidEncoding);
    }

    let mut normalized = PathBuf::new();
    for encoded_segment in uri_path.split('/') {
        if encoded_segment.is_empty() {
            continue;
        }

        let segment = percent_decode(encoded_segment)?;
        if segment == "." {
            continue;
        }
        if segment == ".."
            || segment.contains('/')
            || segment.contains('\\')
            || segment.contains('\0')
        {
            return Err(PathError::Traversal);
        }
        #[cfg(windows)]
        if segment.contains(':') {
            return Err(PathError::Traversal);
        }

        normalized.push(segment);
    }

    if normalized.as_os_str().is_empty() {
        normalized.push(INDEX_FILE);
    }

    Ok(normalized)
}

fn percent_decode(input: &str) -> Result<String, PathError> {
    let input = input.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err(PathError::InvalidEncoding);
            }
            let high = hex_value(input[index + 1]).ok_or(PathError::InvalidEncoding)?;
            let low = hex_value(input[index + 2]).ok_or(PathError::InvalidEncoding)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).map_err(|_| PathError::InvalidEncoding)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn file_response(
    path: &std::path::Path,
    body: Bytes,
    content_length: u64,
) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime_type(path)));
    insert_common_headers(&mut response);
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string())
            .expect("a file length is a valid header value"),
    );
    response
}

fn error_response(error: FileError) -> Response<Full<Bytes>> {
    match error {
        FileError::BadRequest => text_response(StatusCode::BAD_REQUEST, "Bad Request"),
        FileError::Forbidden => text_response(StatusCode::FORBIDDEN, "Forbidden"),
        FileError::NotFound => text_response(StatusCode::NOT_FOUND, "Not Found"),
        FileError::Io(error) => {
            eprintln!("failed to serve static file: {error}");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

fn map_io_error(error: io::Error) -> FileError {
    match error.kind() {
        io::ErrorKind::NotFound => FileError::NotFound,
        io::ErrorKind::PermissionDenied => FileError::Forbidden,
        _ => FileError::Io(error),
    }
}

fn text_response(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(message.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    insert_common_headers(&mut response);
    response
}

fn insert_common_headers(response: &mut Response<Full<Bytes>>) {
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static(SERVER_NAME));
}

#[cfg(feature = "profiling")]
pub(crate) fn constant_response() -> Response<Full<Bytes>> {
    text_response(StatusCode::OK, "Hello, Hamster Wheel!")
}

fn mime_type(path: &std::path::Path) -> &'static str {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return "application/octet-stream";
    };

    if extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm") {
        "text/html; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("css") {
        "text/css; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("js") || extension.eq_ignore_ascii_case("mjs") {
        "text/javascript; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("json") {
        "application/json"
    } else if extension.eq_ignore_ascii_case("txt") {
        "text/plain; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else if extension.eq_ignore_ascii_case("wasm") {
        "application/wasm"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn normalizes_uri_segments() {
        assert_eq!(normalize_path("/").unwrap(), PathBuf::from("index.html"));
        assert_eq!(
            normalize_path("//assets/./hello%20world.txt").unwrap(),
            PathBuf::from("assets").join("hello world.txt")
        );
    }

    #[test]
    fn rejects_path_traversal() {
        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/safe/%2E%2E/secret.txt",
            "/%2e%2e%5csecret.txt",
            "/safe%2f..%2fsecret.txt",
            "/safe\\secret.txt",
            "/safe%5csecret.txt",
            "/safe\0secret.txt",
            "/safe%00secret.txt",
        ] {
            assert_eq!(normalize_path(path), Err(PathError::Traversal), "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_drive_and_ads_syntax() {
        for path in [
            "/C:/Windows/win.ini",
            "/file.txt:stream",
            "/file.txt::$DATA",
            "/file.txt%3Astream",
        ] {
            assert_eq!(normalize_path(path), Err(PathError::Traversal), "{path}");
        }
    }

    #[test]
    fn rejects_invalid_percent_encoding_and_utf8() {
        for path in ["/%", "/%2", "/%GG", "/%FF"] {
            assert_eq!(
                normalize_path(path),
                Err(PathError::InvalidEncoding),
                "{path}"
            );
        }
    }

    #[test]
    fn maps_basic_mime_types_case_insensitively() {
        assert_eq!(
            mime_type(Path::new("index.HTML")),
            "text/html; charset=utf-8"
        );
        assert_eq!(mime_type(Path::new("style.css")), "text/css; charset=utf-8");
        assert_eq!(
            mime_type(Path::new("app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_type(Path::new("image.png")), "image/png");
        assert_eq!(
            mime_type(Path::new("data.unknown")),
            "application/octet-stream"
        );
    }
}
