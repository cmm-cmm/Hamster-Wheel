use std::{cmp::Reverse, fmt, path::PathBuf, sync::Arc, time::Duration};

use hyper::{
    HeaderMap, Request, StatusCode,
    body::Incoming,
    header::{HOST, HeaderValue},
};

pub use crate::handler::{Handler, HandlerKind};
use crate::{
    handler::{HandlerResponse, simple_response},
    transport::ConnectionInfo,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteError {
    MissingHost,
    InvalidHost,
    UnknownHost,
    InvalidPath,
}

impl RouteError {
    const fn status(self) -> StatusCode {
        match self {
            Self::MissingHost | Self::InvalidHost | Self::InvalidPath => StatusCode::BAD_REQUEST,
            Self::UnknownHost => StatusCode::NOT_FOUND,
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::MissingHost => "Missing Host",
            Self::InvalidHost => "Invalid Host",
            Self::UnknownHost => "Unknown Host",
            Self::InvalidPath => "Invalid Path",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterBuildError {
    InvalidHostname(String),
    DuplicateHostname(String),
    InvalidLocation(String),
    DuplicateLocation(String),
}

impl fmt::Display for RouterBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostname(hostname) => write!(formatter, "invalid hostname: {hostname}"),
            Self::DuplicateHostname(hostname) => {
                write!(formatter, "duplicate hostname: {hostname}")
            }
            Self::InvalidLocation(prefix) => write!(formatter, "invalid location: {prefix}"),
            Self::DuplicateLocation(prefix) => write!(formatter, "duplicate location: {prefix}"),
        }
    }
}

impl std::error::Error for RouterBuildError {}

#[derive(Clone)]
pub struct LocationRouter {
    locations: Vec<Location>,
    fallback: Handler,
}

#[derive(Clone)]
struct Location {
    prefix: Box<str>,
    handler: Handler,
}

impl LocationRouter {
    pub fn new(
        locations: Vec<(&str, Handler)>,
        fallback: Handler,
    ) -> Result<Self, RouterBuildError> {
        let mut compiled = Vec::<Location>::with_capacity(locations.len());
        for (prefix, handler) in locations {
            let prefix = normalize_location(prefix)?;
            if compiled
                .iter()
                .any(|location| location.prefix.as_ref() == prefix)
            {
                return Err(RouterBuildError::DuplicateLocation(prefix));
            }
            compiled.push(Location {
                prefix: prefix.into_boxed_str(),
                handler,
            });
        }
        compiled.sort_unstable_by_key(|location| Reverse(location.prefix.len()));
        Ok(Self {
            locations: compiled,
            fallback,
        })
    }

    pub fn route(&self, path: &str) -> Result<&Handler, RouteError> {
        if !path.starts_with('/') {
            return Err(RouteError::InvalidPath);
        }
        Ok(self
            .locations
            .iter()
            .find(|location| location_matches(&location.prefix, path))
            .map_or(&self.fallback, |location| &location.handler))
    }
}

#[derive(Clone)]
pub struct VirtualHost {
    hostname: Box<str>,
    locations: Arc<LocationRouter>,
}

impl VirtualHost {
    pub fn new(hostname: &str, locations: Arc<LocationRouter>) -> Result<Self, RouterBuildError> {
        if !valid_hostname(hostname) {
            return Err(RouterBuildError::InvalidHostname(hostname.to_owned()));
        }
        Ok(Self {
            hostname: hostname.to_ascii_lowercase().into_boxed_str(),
            locations,
        })
    }
}

pub struct HostRouter {
    hosts: Vec<VirtualHost>,
}

impl HostRouter {
    pub fn new(hosts: Vec<VirtualHost>) -> Result<Self, RouterBuildError> {
        for (index, host) in hosts.iter().enumerate() {
            if hosts[..index]
                .iter()
                .any(|existing| existing.hostname == host.hostname)
            {
                return Err(RouterBuildError::DuplicateHostname(
                    host.hostname.to_string(),
                ));
            }
        }
        Ok(Self { hosts })
    }

    pub fn default_static(document_root: Arc<PathBuf>) -> Result<Self, RouterBuildError> {
        let locations = Arc::new(LocationRouter::new(
            vec![
                ("/api", Handler::not_found()),
                ("/", Handler::static_files(document_root)),
            ],
            Handler::not_found(),
        )?);
        Self::new(vec![
            VirtualHost::new("localhost", Arc::clone(&locations))?,
            VirtualHost::new("127.0.0.1", locations)?,
        ])
    }

    pub fn find_host(&self, headers: &HeaderMap) -> Result<&LocationRouter, RouteError> {
        let hostname = request_hostname(headers)?;
        self.find_hostname(hostname)
    }

    fn find_hostname(&self, hostname: &str) -> Result<&LocationRouter, RouteError> {
        self.hosts
            .iter()
            .find(|host| host.hostname.eq_ignore_ascii_case(hostname))
            .map(|host| host.locations.as_ref())
            .ok_or(RouteError::UnknownHost)
    }

    pub fn route(&self, headers: &HeaderMap, path: &str) -> Result<&Handler, RouteError> {
        self.find_host(headers)?.route(path)
    }

    pub(crate) async fn serve(
        &self,
        mut request: Request<Incoming>,
        connection: ConnectionInfo,
        body_read_timeout: Duration,
    ) -> Result<HandlerResponse, std::convert::Infallible> {
        let handler = match self.route_request(&mut request) {
            Ok(handler) => handler,
            Err(error) => return Ok(simple_response(error.status(), error.message())),
        };
        handler.serve(request, connection, body_read_timeout).await
    }

    fn route_request<B>(&self, request: &mut Request<B>) -> Result<&Handler, RouteError> {
        // HTTP/1.1 still requires exactly one valid Host field for every
        // request. For absolute-form, however, RFC 9112 requires an origin
        // server to ignore that field's identity and use the request-target
        // authority. Replace Host before routing so handlers cannot observe a
        // different authority from the one that selected their virtual host.
        let target_authority = if request.uri().scheme().is_some() {
            Some(
                request
                    .uri()
                    .authority()
                    .cloned()
                    .ok_or(RouteError::InvalidHost)?,
            )
        } else {
            None
        };
        let received_hostname = request_hostname(request.headers())?;

        let locations = if let Some(authority) = target_authority {
            let hostname = parse_authority(authority.as_str())?;
            let host =
                HeaderValue::from_str(authority.as_str()).map_err(|_| RouteError::InvalidHost)?;
            request.headers_mut().insert(HOST, host);
            self.find_hostname(hostname)?
        } else {
            self.find_hostname(received_hostname)?
        };
        locations.route(request.uri().path())
    }

    #[cfg(feature = "profiling")]
    pub fn profiling_handler_kind(
        &self,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<HandlerKind, RouteError> {
        self.route(headers, path).map(Handler::kind)
    }

    #[cfg(feature = "profiling")]
    pub(crate) fn profiling_response(
        &self,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<HandlerResponse, RouteError> {
        self.route(headers, path).map(Handler::profiling_response)
    }
}

fn request_hostname(headers: &HeaderMap) -> Result<&str, RouteError> {
    let mut values = headers.get_all(HOST).iter();
    let value = values.next().ok_or(RouteError::MissingHost)?;
    if values.next().is_some() {
        return Err(RouteError::InvalidHost);
    }
    parse_host(value)
}

fn parse_host(value: &HeaderValue) -> Result<&str, RouteError> {
    let authority = value.to_str().map_err(|_| RouteError::InvalidHost)?;
    parse_authority(authority)
}

fn parse_authority(authority: &str) -> Result<&str, RouteError> {
    let (hostname, port) = match authority.rsplit_once(':') {
        Some((hostname, port)) => (hostname, Some(port)),
        None => (authority, None),
    };
    if !valid_hostname(hostname) {
        return Err(RouteError::InvalidHost);
    }
    if let Some(port) = port
        && (port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().is_err())
    {
        return Err(RouteError::InvalidHost);
    }
    Ok(hostname)
}

pub(crate) fn valid_hostname(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

pub(crate) fn normalize_location(prefix: &str) -> Result<String, RouterBuildError> {
    if !prefix.starts_with('/') || prefix.contains(['?', '#', '\0', '\\']) {
        return Err(RouterBuildError::InvalidLocation(prefix.to_owned()));
    }
    let normalized = prefix.trim_end_matches('/');
    Ok(if normalized.is_empty() {
        "/".to_owned()
    } else {
        normalized.to_owned()
    })
}

fn location_matches(prefix: &str, path: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router() -> HostRouter {
        let locations = Arc::new(
            LocationRouter::new(
                vec![
                    ("/", Handler::simple(StatusCode::OK, "root")),
                    ("/api", Handler::simple(StatusCode::OK, "api")),
                    ("/api/admin", Handler::simple(StatusCode::OK, "admin")),
                ],
                Handler::not_found(),
            )
            .unwrap(),
        );
        HostRouter::new(vec![VirtualHost::new("example.test", locations).unwrap()]).unwrap()
    }

    fn headers(host: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(HOST, HeaderValue::from_str(host).unwrap());
        }
        headers
    }

    #[test]
    fn matches_known_host_case_insensitively_and_ignores_port() {
        let router = test_router();
        for host in ["example.test", "EXAMPLE.TEST", "Example.Test:8080"] {
            assert!(router.find_host(&headers(Some(host))).is_ok(), "{host}");
        }
    }

    #[test]
    fn distinguishes_missing_invalid_and_unknown_hosts() {
        let router = test_router();
        assert!(matches!(
            router.find_host(&headers(None)),
            Err(RouteError::MissingHost)
        ));
        assert!(matches!(
            router.find_host(&headers(Some("example.test:99999"))),
            Err(RouteError::InvalidHost)
        ));
        assert!(matches!(
            router.find_host(&headers(Some("unknown.test"))),
            Err(RouteError::UnknownHost)
        ));

        let mut duplicate = headers(Some("example.test"));
        duplicate.append(HOST, HeaderValue::from_static("example.test"));
        assert!(matches!(
            router.find_host(&duplicate),
            Err(RouteError::InvalidHost)
        ));
    }

    #[test]
    fn uses_longest_segment_prefix() {
        let router = test_router();
        let headers = headers(Some("example.test"));
        assert_eq!(
            router.route(&headers, "/").unwrap().kind(),
            HandlerKind::Simple
        );
        let admin = router.route(&headers, "/api/admin/users").unwrap();
        let api = router.route(&headers, "/api/users").unwrap();
        assert!(matches!(admin, Handler::Simple { body: "admin", .. }));
        assert!(matches!(api, Handler::Simple { body: "api", .. }));
        assert!(matches!(
            router.route(&headers, "/apix").unwrap(),
            Handler::Simple { body: "root", .. }
        ));
    }

    #[test]
    fn rejects_invalid_request_and_configuration_paths() {
        let router = test_router();
        assert!(matches!(
            router.route(&headers(Some("example.test")), "api"),
            Err(RouteError::InvalidPath)
        ));
        assert!(
            LocationRouter::new(vec![("api", Handler::not_found())], Handler::not_found()).is_err()
        );
    }

    #[test]
    fn rejects_duplicate_hosts_and_locations() {
        let locations = Arc::new(LocationRouter::new(Vec::new(), Handler::not_found()).unwrap());
        let first = VirtualHost::new("example.test", Arc::clone(&locations)).unwrap();
        let duplicate = VirtualHost::new("EXAMPLE.TEST", locations).unwrap();
        assert!(HostRouter::new(vec![first, duplicate]).is_err());
        assert!(
            LocationRouter::new(
                vec![
                    ("/api", Handler::not_found()),
                    ("/api/", Handler::not_found())
                ],
                Handler::not_found()
            )
            .is_err()
        );
    }

    #[test]
    fn absolute_form_replaces_host_before_routing_and_downstream_dispatch() {
        let router = test_router();
        let mut request = Request::builder()
            .uri("http://EXAMPLE.TEST:8443/api?value=1")
            .header(HOST, "other.test:9000")
            .body(())
            .unwrap();

        let handler = router.route_request(&mut request).unwrap();

        assert!(matches!(handler, Handler::Simple { body: "api", .. }));
        assert_eq!(request.headers().get_all(HOST).iter().count(), 1);
        assert_eq!(request.headers()[HOST], "EXAMPLE.TEST:8443");
        assert_eq!(request.uri().authority().unwrap(), "EXAMPLE.TEST:8443");
    }

    #[test]
    fn origin_form_keeps_the_validated_host_as_effective_authority() {
        let router = test_router();
        let mut request = Request::builder()
            .uri("/api")
            .header(HOST, "Example.Test:8080")
            .body(())
            .unwrap();

        let handler = router.route_request(&mut request).unwrap();

        assert!(matches!(handler, Handler::Simple { body: "api", .. }));
        assert_eq!(request.headers()[HOST], "Example.Test:8080");
        assert!(request.uri().authority().is_none());
    }

    #[test]
    fn absolute_form_still_requires_one_valid_host_field() {
        let router = test_router();
        let mut missing = Request::builder()
            .uri("http://example.test/api")
            .body(())
            .unwrap();
        assert!(matches!(
            router.route_request(&mut missing),
            Err(RouteError::MissingHost)
        ));

        let mut invalid = Request::builder()
            .uri("http://example.test/api")
            .header(HOST, "bad_host")
            .body(())
            .unwrap();
        assert!(matches!(
            router.route_request(&mut invalid),
            Err(RouteError::InvalidHost)
        ));

        let mut duplicate = Request::builder()
            .uri("http://example.test/api")
            .header(HOST, "example.test")
            .body(())
            .unwrap();
        duplicate
            .headers_mut()
            .append(HOST, HeaderValue::from_static("example.test"));
        assert!(matches!(
            router.route_request(&mut duplicate),
            Err(RouteError::InvalidHost)
        ));
    }

    #[test]
    fn absolute_form_rejects_invalid_and_unknown_target_authority() {
        let router = test_router();
        let mut invalid = Request::builder()
            .uri("http://example.test:99999/api")
            .header(HOST, "example.test")
            .body(())
            .unwrap();
        assert!(matches!(
            router.route_request(&mut invalid),
            Err(RouteError::InvalidHost)
        ));

        let mut unknown = Request::builder()
            .uri("http://unknown.test/api")
            .header(HOST, "example.test")
            .body(())
            .unwrap();
        assert!(matches!(
            router.route_request(&mut unknown),
            Err(RouteError::UnknownHost)
        ));
    }
}
