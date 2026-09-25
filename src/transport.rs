use std::net::SocketAddr;

/// Socket identity established by the server for an accepted connection.
///
/// This is deliberately distinct from request headers and from any future
/// trusted-proxy-derived client identity. Both addresses describe the
/// immediate TCP connection and remain constant for its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransportInfo {
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
}

impl TransportInfo {
    pub(crate) const fn new(peer_addr: SocketAddr, local_addr: SocketAddr) -> Self {
        Self {
            peer_addr,
            local_addr,
        }
    }

    pub(crate) const fn peer_addr(self) -> SocketAddr {
        self.peer_addr
    }

    pub(crate) const fn local_addr(self) -> SocketAddr {
        self.local_addr
    }
}

/// The scheme a client used to reach Hamster, as established by the listener
/// that accepted the connection.
///
/// `Https` means Hamster itself completed a TLS handshake on this connection.
/// Nothing a client sends, such as `X-Forwarded-Proto` or an absolute-form
/// `https://` request target, can produce it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// What the server knows about a connection, independent of any request on
/// it: the socket identity and the trusted scheme. Handlers receive this and
/// nothing transport-specific beyond it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionInfo {
    transport: TransportInfo,
    scheme: Scheme,
}

impl ConnectionInfo {
    pub(crate) const fn new(transport: TransportInfo, scheme: Scheme) -> Self {
        Self { transport, scheme }
    }

    pub(crate) const fn transport(self) -> TransportInfo {
        self.transport
    }

    pub(crate) const fn scheme(self) -> Scheme {
        self.scheme
    }
}
