//! Loopback addresses that nothing else in a test run can be handed.
//!
//! Tests that need a port nobody listens on used to bind port 0 and drop the
//! socket. Every other test binds port 0 too, and Windows hands recently freed
//! ephemeral ports out again, so a concurrently running test could be given
//! the same port: the "dead" upstream then accepted connections, or a port
//! checked to be free after a failed startup was found listening. Reproduced
//! with four library test binaries at once: 3 such failures in 40 runs.
//!
//! Ports here come from below the dynamic range (49152-65535 on Windows, the
//! range `bind(0)` and outgoing connections use), starting at a per-process
//! offset, and are checked to be free when chosen.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::atomic::{AtomicU16, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const FIRST: u16 = 20_000;
const COUNT: u16 = 20_000;

/// A loopback address outside the dynamic port range that is free right now.
pub(crate) fn unused_outside_dynamic_range() -> SocketAddr {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    // Different test processes start at different offsets.
    let seed = (u64::from(std::process::id())
        ^ SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::from(elapsed.subsec_nanos())))
        % u64::from(COUNT);
    let _ = NEXT.compare_exchange(0, seed as u16 + 1, Ordering::AcqRel, Ordering::Acquire);
    for _ in 0..COUNT {
        let offset = NEXT.fetch_add(1, Ordering::AcqRel) % COUNT;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, FIRST + offset));
        if TcpListener::bind(address).is_ok() {
            return address;
        }
    }
    panic!(
        "no free loopback port between {FIRST} and {}",
        FIRST + COUNT - 1
    );
}
