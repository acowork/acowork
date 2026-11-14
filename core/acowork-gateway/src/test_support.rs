//! Loopback port allocation for the Gateway's in-tree tests.
//!
//! Every test that starts a broker, an HTTP listener, or a supervised service
//! needs a port. These used to be hand-picked constants (`18976..18979`,
//! `27974..27977`, with `+10`/`+20` offsets per test) kept distinct by a
//! comment asking the next author to look at the others. That does not work,
//! for two reasons:
//!
//! - A fixed port can be *permanently* unavailable on some machines: Windows
//!   reserves large ranges for Hyper-V/WSL ("excluded port ranges"), and a
//!   developer may have a service on it. The test then fails on that machine
//!   and passes everywhere else, which reads like a product bug.
//! - Two tests in one binary run in parallel; the constants only stay disjoint
//!   as long as nobody adds a third file.
//!
//! Probing an ephemeral port and releasing it before the real bind leaves a
//! small race window (another process could take it in between) — the same
//! trade the e2e suites already make, and it fails rarely and randomly rather
//! than deterministically on a configured machine.
#![allow(dead_code)] // shared helpers; not every consumer uses every one of them

use std::net::TcpListener;

/// A loopback port that is free right now.
pub(crate) fn free_port() -> u16 {
    free_ports(1)[0]
}

/// `n` *distinct* loopback ports that are free right now.
///
/// Distinctness is checked rather than assumed: the OS is free to hand back the
/// same ephemeral port after the probe socket is dropped, and a broker that
/// binds its main and its remote listener to the same port does not fail in a
/// way that points back here.
pub(crate) fn free_ports(n: usize) -> Vec<u16> {
    let mut ports = Vec::with_capacity(n);
    while ports.len() < n {
        let listener = TcpListener::bind("127.0.0.1:0")
            .expect("bind an ephemeral loopback port for the test");
        let port = listener.local_addr().expect("local addr").port();
        if !ports.contains(&port) {
            ports.push(port);
        }
        // Dropping the listener is what frees the port for the real bind.
        drop(listener);
    }
    ports
}

/// A `reqwest` client for talking to a loopback endpoint the test started.
///
/// An ambient proxy (`HTTP_PROXY`, set by Clash/v2rayN and by most corporate
/// machines) cannot reach the listener, and its error becomes the test's
/// verdict: "the service never came up" while it came up fine. Loopback means
/// *this machine* — never route it through a third party.
pub(crate) fn loopback_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build a loopback HTTP client")
}
