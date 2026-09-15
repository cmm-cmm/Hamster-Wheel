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
