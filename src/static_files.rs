use std::{
    convert::Infallible,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{ALLOW, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, SERVER},
};

use crate::{
    access::{AccessPolicy, is_php_source},
    config::SERVER_NAME,
    response_body::ResponseBody,
};

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
    /// The opened handle's final location could not be determined, so its
    /// containment is unknown and it must not be served.
    #[cfg(windows)]
    FinalPath(io::Error),
    Io(io::Error),
}

struct ResolvedFile {
    /// Location of the served object. On Windows this is the final resolved
    /// location of the opened handle, which is also what containment checked.
    path: PathBuf,
    file: tokio::fs::File,
    length: u64,
}

pub async fn serve(
    request: Request<Incoming>,
    document_root: Arc<PathBuf>,
    access: &AccessPolicy,
) -> Result<Response<ResponseBody>, Infallible> {
    let is_head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !is_head {
        let mut response = text_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return Ok(response);
    }

    Ok(respond(is_head, &document_root, request.uri().path(), access).await)
}

/// GET and HEAD share one resolution, so they reach the same containment
/// decision for the same object. HEAD drops the validated handle after its
/// metadata has produced the headers; GET streams that same handle.
async fn respond(
    is_head: bool,
    document_root: &Path,
    uri_path: &str,
    access: &AccessPolicy,
) -> Response<ResponseBody> {
    match resolve_file(document_root, uri_path).await {
        Ok(file)
            if access.denies_final_path(document_root, &file.path) || is_php_source(&file.path) =>
        {
            error_response(FileError::NotFound)
        }
        Ok(file) if is_head => file_response(
            &file.path,
            ResponseBody::buffered(Bytes::new()),
            file.length,
        ),
        Ok(file) => file_response(
            &file.path,
            ResponseBody::file(file.file, file.length),
            file.length,
        ),
        Err(error) => error_response(error),
    }
}

fn relative_request_path(uri_path: &str) -> Result<PathBuf, FileError> {
    normalize_path(uri_path).map_err(|error| match error {
        PathError::InvalidEncoding => FileError::BadRequest,
        PathError::Traversal => FileError::Forbidden,
    })
}

/// Windows opened-handle containment.
///
/// The candidate is opened once. Containment is decided from the final
/// resolved location of that handle, and the same handle supplies the metadata
/// and the response body. Nothing reopens a pathname after validation, so a
/// namespace change after the open cannot substitute a different object.
///
/// The document root comes from `fs::canonicalize`, which on Windows is itself
/// `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)`; both sides of the comparison
/// therefore share one representation and the filesystem's stored name casing.
#[cfg(windows)]
async fn resolve_file(document_root: &Path, uri_path: &str) -> Result<ResolvedFile, FileError> {
    let relative = relative_request_path(uri_path)?;
    let document_root = document_root.to_path_buf();
    let (path, file, length) =
        tokio::task::spawn_blocking(move || open_contained(&document_root, relative))
            .await
            .map_err(|error| FileError::Io(io::Error::other(error)))??;
    Ok(ResolvedFile {
        path,
        file: tokio::fs::File::from_std(file),
        length,
    })
}

#[cfg(windows)]
fn open_contained(
    document_root: &Path,
    relative: PathBuf,
) -> Result<(PathBuf, std::fs::File, u64), FileError> {
    let mut candidate = document_root.join(relative);
    // Index selection only: this lookup decides which name to open and is not
    // a containment check. A directory cannot be opened without
    // FILE_FLAG_BACKUP_SEMANTICS, which also changes access checks when the
    // backup privilege is enabled, so the served handle is never opened that
    // way. If the namespace changes after this lookup, the object that is
    // opened is still validated below.
    if std::fs::metadata(&candidate)
        .map_err(map_io_error)?
        .is_dir()
    {
        candidate.push(INDEX_FILE);
    }

    let file = std::fs::File::open(&candidate).map_err(map_io_error)?;
    #[cfg(test)]
    test_seam::reached(test_seam::Checkpoint::Opened, &candidate);

    let final_location = final_path_of(&file).map_err(FileError::FinalPath)?;
    // Component-wise: `C:\www` does not contain `C:\www-other`, and a
    // representation mismatch fails closed rather than open.
    if !final_location.starts_with(document_root) {
        return Err(FileError::Forbidden);
    }
    #[cfg(test)]
    test_seam::reached(test_seam::Checkpoint::Validated, &candidate);

    let metadata = file.metadata().map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(FileError::NotFound);
    }
    Ok((final_location, file, metadata.len()))
}

/// `GetFinalPathNameByHandleW` with `FILE_NAME_NORMALIZED | VOLUME_NAME_DOS`,
/// the same query `std::fs::canonicalize` performs on Windows.
#[cfg(windows)]
fn final_path_of(file: &std::fs::File) -> io::Result<PathBuf> {
    use std::{
        ffi::OsString,
        os::windows::{ffi::OsStringExt, io::AsRawHandle},
    };

    const FILE_NAME_NORMALIZED_VOLUME_NAME_DOS: u32 = 0;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: std::os::windows::io::RawHandle,
            path: *mut u16,
            path_length: u32,
            flags: u32,
        ) -> u32;
    }

    let mut buffer = vec![0_u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the handle is owned by `file` and outlives this call, and
        // `buffer` is writable for `capacity` UTF-16 units.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED_VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        // A result shorter than the buffer is the path without its NUL; a
        // larger one is the required size including the NUL.
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length, 0);
    }
}

/// Non-Windows targets keep the pathname model: containment is checked on the
/// canonical pathname, and the file is opened afterwards by that pathname.
/// This does not provide the opened-handle guarantee of the Windows path and
/// is not qualified against concurrent namespace changes.
#[cfg(not(windows))]
async fn resolve_file(document_root: &Path, uri_path: &str) -> Result<ResolvedFile, FileError> {
    let relative = relative_request_path(uri_path)?;
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

    let file = tokio::fs::File::open(&canonical)
        .await
        .map_err(map_io_error)?;
    let metadata = file.metadata().await.map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(FileError::NotFound);
    }

    Ok(ResolvedFile {
        path: canonical,
        file,
        length: metadata.len(),
    })
}

pub(crate) fn normalize_path(uri_path: &str) -> Result<PathBuf, PathError> {
    let mut normalized: PathBuf = decode_segments(uri_path)?.into_iter().collect();
    if normalized.as_os_str().is_empty() {
        normalized.push(INDEX_FILE);
    }

    Ok(normalized)
}

/// The decoded, validated segments of a request path, in order. Empty and
/// `.` segments are dropped; `..`, encoded separators, NUL and (on Windows)
/// drive or stream syntax are refused, so the segments can only name
/// something beneath whatever root they are joined to.
pub(crate) fn decode_segments(uri_path: &str) -> Result<Vec<String>, PathError> {
    if !uri_path.starts_with('/') {
        return Err(PathError::InvalidEncoding);
    }

    let mut segments = Vec::new();
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

        segments.push(segment);
    }

    Ok(segments)
}

/// What a relative path names beneath a document root, decided on the object
/// actually opened, as `serve` decides it.
pub(crate) enum Lookup {
    /// A regular file whose final location is inside the root. `path` is that
    /// final location, so a name the filesystem rewrote (letter case, trailing
    /// dots or spaces on Windows) is reported as the object it really is.
    File {
        path: PathBuf,
        file: std::fs::File,
        length: u64,
    },
    Directory,
    Missing,
    /// Resolves outside the root, through a link or junction.
    Outside,
}

/// Looks up `relative` beneath `document_root` without the directory index
/// rule. Blocking; call it from a blocking task.
#[cfg(windows)]
pub(crate) fn lookup_within(document_root: &Path, relative: &Path) -> io::Result<Lookup> {
    let candidate = document_root.join(relative);
    let metadata = match std::fs::metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if is_missing(&error) => return Ok(Lookup::Missing),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        return Ok(Lookup::Directory);
    }
    let file = match std::fs::File::open(&candidate) {
        Ok(file) => file,
        Err(error) if is_missing(&error) => return Ok(Lookup::Missing),
        Err(error) => return Err(error),
    };
    let path = final_path_of(&file)?;
    if !path.starts_with(document_root) {
        return Ok(Lookup::Outside);
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Lookup::Missing);
    }
    Ok(Lookup::File {
        path,
        length: metadata.len(),
        file,
    })
}

/// Pathname model, as `resolve_file` on these targets; not qualified against
/// concurrent namespace changes.
#[cfg(not(windows))]
pub(crate) fn lookup_within(document_root: &Path, relative: &Path) -> io::Result<Lookup> {
    let candidate = document_root.join(relative);
    let metadata = match std::fs::metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if is_missing(&error) => return Ok(Lookup::Missing),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        return Ok(Lookup::Directory);
    }
    let path = std::fs::canonicalize(&candidate)?;
    if !path.starts_with(document_root) {
        return Ok(Lookup::Outside);
    }
    let file = std::fs::File::open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Lookup::Missing);
    }
    Ok(Lookup::File {
        path,
        length: metadata.len(),
        file,
    })
}

fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidFilename
    ) || error.raw_os_error() == Some(123)
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
    body: ResponseBody,
    content_length: u64,
) -> Response<ResponseBody> {
    let mut response = Response::new(body);
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

fn error_response(error: FileError) -> Response<ResponseBody> {
    match error {
        FileError::BadRequest => text_response(StatusCode::BAD_REQUEST, "Bad Request"),
        FileError::Forbidden => text_response(StatusCode::FORBIDDEN, "Forbidden"),
        FileError::NotFound => text_response(StatusCode::NOT_FOUND, "Not Found"),
        #[cfg(windows)]
        FileError::FinalPath(error) => {
            errln!("cannot determine the final location of an opened static file: {error}");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
        FileError::Io(error) => {
            errln!("failed to serve static file: {error}");
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

fn text_response(status: StatusCode, message: &'static str) -> Response<ResponseBody> {
    if status.is_client_error() || status.is_server_error() {
        let mut response = error_text_response(status, message);
        response
            .extensions_mut()
            .insert(crate::access_log::ErrorReason(message));
        return response;
    }
    error_text_response(status, message)
}

fn error_text_response(status: StatusCode, message: &'static str) -> Response<ResponseBody> {
    let mut response = Response::new(ResponseBody::buffered(Bytes::from_static(
        message.as_bytes(),
    )));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    insert_common_headers(&mut response);
    response
}

fn insert_common_headers(response: &mut Response<ResponseBody>) {
    response
        .headers_mut()
        .insert(SERVER, HeaderValue::from_static(SERVER_NAME));
}

#[cfg(feature = "profiling")]
pub(crate) fn constant_response() -> Response<ResponseBody> {
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

/// Test-only ordering control for the Windows opened-handle path. It does not
/// exist outside unit-test builds of this crate.
#[cfg(all(test, windows))]
mod test_seam {
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Checkpoint {
        /// The candidate handle is open; its final location is not yet known.
        Opened,
        /// The handle passed containment; metadata and the body are next.
        Validated,
    }

    type Action = Box<dyn FnOnce() + Send>;

    static PENDING: Mutex<Vec<(PathBuf, Checkpoint, Action)>> = Mutex::new(Vec::new());

    /// Runs `action` once, on the resolving thread, when a request for a
    /// candidate under `scope` reaches `checkpoint`. Scopes keep concurrently
    /// running tests from triggering each other's actions.
    pub(super) fn arm(
        scope: &Path,
        checkpoint: Checkpoint,
        action: impl FnOnce() + Send + 'static,
    ) {
        PENDING
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((scope.to_path_buf(), checkpoint, Box::new(action)));
    }

    pub(super) fn reached(checkpoint: Checkpoint, candidate: &Path) {
        let action = {
            let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
            pending
                .iter()
                .position(|(scope, at, _)| *at == checkpoint && candidate.starts_with(scope))
                .map(|index| pending.remove(index).2)
        };
        if let Some(action) = action {
            action();
        }
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

#[cfg(all(test, windows))]
mod windows_containment_tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use bytes::Bytes;
    use http_body_util::BodyExt;
    use hyper::{StatusCode, header::CONTENT_LENGTH};

    use super::{
        respond,
        test_seam::{self, Checkpoint},
    };

    const INSIDE: &[u8] = b"inside-original";
    const SECRET: &[u8] = b"OUTSIDE-SECRET-BYTES-THAT-MUST-NEVER-BE-SERVED";

    /// A document root and a sibling `outside` directory. Junctions are removed
    /// as links before the tree is deleted so cleanup never traverses them.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "hamster-static-{name}-{}-{unique}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(base.join("www")).unwrap();
            fs::create_dir_all(base.join("outside")).unwrap();
            // The production root is canonical, so the fixture root is too.
            let root = fs::canonicalize(base.join("www")).unwrap();
            let outside = fs::canonicalize(base.join("outside")).unwrap();
            Self {
                base,
                root,
                outside,
            }
        }

        fn junction(&self, link: &Path, target: &Path) {
            let output = Command::new("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "mklink /J failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            remove_links(&self.base);
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn remove_links(directory: &Path) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || is_junction(&metadata) {
                let _ = fs::remove_dir(&path);
            } else if metadata.is_dir() {
                remove_links(&path);
            }
        }
    }

    fn is_junction(metadata: &fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    struct Fetched {
        status: StatusCode,
        content_length: Option<u64>,
        body: Bytes,
    }

    async fn fetch(is_head: bool, root: &Path, uri_path: &str) -> Fetched {
        let response = respond(
            is_head,
            root,
            uri_path,
            &crate::access::AccessPolicy::default(),
        )
        .await;
        let status = response.status();
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .map(|value| value.to_str().unwrap().parse().unwrap());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Fetched {
            status,
            content_length,
            body,
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn assert_not_disclosed(fetched: &Fetched, fixture: &Fixture) {
        assert!(
            !contains(&fetched.body, SECRET),
            "outside bytes were served"
        );
        let outside = fixture.outside.to_string_lossy();
        assert!(
            !contains(&fetched.body, outside.as_bytes()),
            "the outside location was disclosed"
        );
        let unprefixed = outside.trim_start_matches(r"\\?\");
        assert!(!contains(&fetched.body, unprefixed.as_bytes()));
    }

    /// Arms `action` and returns a flag proving it actually ran, so a test
    /// cannot pass vacuously because its namespace change never happened.
    fn arm(
        fixture: &Fixture,
        checkpoint: Checkpoint,
        action: impl FnOnce() + Send + 'static,
    ) -> Arc<AtomicBool> {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        test_seam::arm(&fixture.root, checkpoint, move || {
            action();
            flag.store(true, Ordering::Release);
        });
        fired
    }

    #[tokio::test]
    async fn get_and_head_serve_an_inside_file_from_its_opened_handle() {
        let fixture = Fixture::new("inside");
        fs::write(fixture.root.join("a.txt"), INSIDE).unwrap();

        let get = fetch(false, &fixture.root, "/a.txt").await;
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(get.body, INSIDE);
        assert_eq!(get.content_length, Some(INSIDE.len() as u64));

        let head = fetch(true, &fixture.root, "/a.txt").await;
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(head.content_length, Some(INSIDE.len() as u64));
        assert!(head.body.is_empty());
    }

    #[tokio::test]
    async fn serves_a_junction_that_resolves_inside_the_root() {
        let fixture = Fixture::new("junction-inside");
        fs::create_dir(fixture.root.join("real")).unwrap();
        fs::write(fixture.root.join("real").join("page.txt"), INSIDE).unwrap();
        fixture.junction(&fixture.root.join("alias"), &fixture.root.join("real"));

        let get = fetch(false, &fixture.root, "/alias/page.txt").await;
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(get.body, INSIDE);
    }

    #[tokio::test]
    async fn rejects_a_junction_that_resolves_outside_the_root() {
        let fixture = Fixture::new("junction-outside");
        fs::write(fixture.outside.join("secret.txt"), SECRET).unwrap();
        fixture.junction(&fixture.root.join("escape"), &fixture.outside);

        for is_head in [false, true] {
            let fetched = fetch(is_head, &fixture.root, "/escape/secret.txt").await;
            assert_eq!(fetched.status, StatusCode::FORBIDDEN, "head={is_head}");
            assert_ne!(fetched.content_length, Some(SECRET.len() as u64));
            assert_not_disclosed(&fetched, &fixture);
        }
    }

    #[tokio::test]
    async fn get_and_head_agree_on_containment() {
        let fixture = Fixture::new("agreement");
        fs::write(fixture.root.join("a.txt"), INSIDE).unwrap();
        fs::create_dir(fixture.root.join("real")).unwrap();
        fs::write(fixture.root.join("real").join("page.txt"), INSIDE).unwrap();
        fixture.junction(&fixture.root.join("alias"), &fixture.root.join("real"));
        fs::write(fixture.outside.join("secret.txt"), SECRET).unwrap();
        fixture.junction(&fixture.root.join("escape"), &fixture.outside);
        // A sibling sharing the root's name as a string prefix: `www-other`
        // must not count as inside `www`.
        let sibling = fixture.base.join("www-other");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("secret.txt"), SECRET).unwrap();
        fixture.junction(&fixture.root.join("sibling"), &sibling);

        for (path, expected) in [
            ("/a.txt", StatusCode::OK),
            ("/alias/page.txt", StatusCode::OK),
            ("/escape/secret.txt", StatusCode::FORBIDDEN),
            ("/sibling/secret.txt", StatusCode::FORBIDDEN),
            ("/missing.txt", StatusCode::NOT_FOUND),
        ] {
            let get = fetch(false, &fixture.root, path).await;
            let head = fetch(true, &fixture.root, path).await;
            assert_eq!(get.status, expected, "GET {path}");
            assert_eq!(head.status, get.status, "HEAD and GET disagree on {path}");
            assert!(head.body.is_empty() || head.status != StatusCode::OK);
            if get.status == StatusCode::OK {
                assert_eq!(head.content_length, Some(get.body.len() as u64), "{path}");
            } else {
                assert_not_disclosed(&get, &fixture);
                assert_not_disclosed(&head, &fixture);
            }
        }
    }

    #[tokio::test]
    async fn replacing_the_file_after_open_serves_the_opened_object() {
        let fixture = Fixture::new("replace-file");
        fs::write(fixture.root.join("a.txt"), INSIDE).unwrap();
        let root = fixture.root.clone();
        let fired = arm(&fixture, Checkpoint::Opened, move || {
            fs::rename(root.join("a.txt"), root.join("a.old")).unwrap();
            fs::write(root.join("a.txt"), SECRET).unwrap();
        });

        let get = fetch(false, &fixture.root, "/a.txt").await;
        assert!(fired.load(Ordering::Acquire), "namespace change must run");
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(get.body, INSIDE);
        assert_not_disclosed(&get, &fixture);
    }

    #[tokio::test]
    async fn retargeting_a_junction_after_open_serves_the_opened_object() {
        let fixture = Fixture::new("retarget-junction");
        fs::create_dir(fixture.root.join("real")).unwrap();
        fs::write(fixture.root.join("real").join("page.txt"), INSIDE).unwrap();
        fs::write(fixture.outside.join("page.txt"), SECRET).unwrap();
        fixture.junction(&fixture.root.join("alias"), &fixture.root.join("real"));

        let alias = fixture.root.join("alias");
        let outside = fixture.outside.clone();
        let fired = arm(&fixture, Checkpoint::Opened, move || {
            fs::remove_dir(&alias).unwrap();
            let output = Command::new("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&alias)
                .arg(&outside)
                .output()
                .unwrap();
            assert!(output.status.success());
        });

        let get = fetch(false, &fixture.root, "/alias/page.txt").await;
        assert!(fired.load(Ordering::Acquire), "namespace change must run");
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(get.body, INSIDE);
        assert_not_disclosed(&get, &fixture);
    }

    #[tokio::test]
    async fn replacing_an_intermediate_directory_after_open_cannot_redirect() {
        let fixture = Fixture::new("replace-intermediate");
        fs::create_dir(fixture.root.join("dir")).unwrap();
        fs::write(fixture.root.join("dir").join("f.txt"), INSIDE).unwrap();
        fs::write(fixture.outside.join("f.txt"), SECRET).unwrap();

        let root = fixture.root.clone();
        let outside = fixture.outside.clone();
        let replaced = Arc::new(AtomicBool::new(false));
        let replaced_flag = Arc::clone(&replaced);
        let fired = arm(&fixture, Checkpoint::Opened, move || {
            // NTFS refuses to rename a directory while a file below it is
            // open, which is itself part of the protection. Either way the
            // opened object must be the one served.
            if fs::rename(root.join("dir"), root.join("dir.old")).is_ok() {
                let output = Command::new("cmd")
                    .args(["/D", "/C", "mklink", "/J"])
                    .arg(root.join("dir"))
                    .arg(&outside)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                replaced_flag.store(true, Ordering::Release);
            }
        });

        let get = fetch(false, &fixture.root, "/dir/f.txt").await;
        assert!(fired.load(Ordering::Acquire), "namespace change must run");
        eprintln!(
            "intermediate directory replaced after open: {}",
            replaced.load(Ordering::Acquire)
        );
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(get.body, INSIDE);
        assert_not_disclosed(&get, &fixture);
    }

    /// Falsification: an implementation that validates one handle and then
    /// reopens the pathname would serve the replacement placed here, after
    /// validation. The replacement also has a different length, so HEAD proves
    /// its metadata came from the validated handle too.
    #[tokio::test]
    async fn namespace_change_after_validation_does_not_change_the_served_object() {
        for is_head in [false, true] {
            let fixture = Fixture::new(if is_head {
                "after-validation-head"
            } else {
                "after-validation-get"
            });
            fs::write(fixture.root.join("a.txt"), INSIDE).unwrap();
            let root = fixture.root.clone();
            let fired = arm(&fixture, Checkpoint::Validated, move || {
                fs::rename(root.join("a.txt"), root.join("a.old")).unwrap();
                fs::write(root.join("a.txt"), SECRET).unwrap();
            });

            let fetched = fetch(is_head, &fixture.root, "/a.txt").await;
            assert!(fired.load(Ordering::Acquire), "namespace change must run");
            assert_eq!(fetched.status, StatusCode::OK);
            assert_eq!(fetched.content_length, Some(INSIDE.len() as u64));
            if is_head {
                assert!(fetched.body.is_empty());
            } else {
                assert_eq!(fetched.body, INSIDE);
            }
            assert_not_disclosed(&fetched, &fixture);
        }
    }
}
