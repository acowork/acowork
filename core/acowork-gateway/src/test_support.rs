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

/// A temp directory unique to this call, for a test that needs a real
/// directory on disk.
///
/// The tests used to build these names by hand — `acowork-test-{kind}-{id}`
/// or `{pid}-{id}` — and then `create_dir_all` them. That is not unique: two
/// tests that use the same id (or the same helper called twice with the same
/// id) resolve to the same path, and one test's `remove_dir_all` then deletes
/// the directory another test has open. On Windows the victim fails with
/// "Cannot create a file when that file already exists", which surfaces as an
/// unrelated test failing at random — roughly one run in six — and points at
/// the wrong file entirely.
///
/// The name carries the process id (so two concurrent test binaries do not
/// collide) and a per-process sequence number (so two calls inside one
/// binary do not collide). The caller owns the directory; nothing removes it
/// automatically, which keeps the current "read the artefacts afterwards"
/// debugging habit working.
pub(crate) fn unique_temp_dir(label: impl AsRef<str>) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "acowork-test-{}-{}-{seq}",
        label.as_ref(),
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::unique_temp_dir;

    /// The whole point of the helper: two calls with the same label must not
    /// resolve to the same directory.
    ///
    /// The tests that built these names by hand used the *subject* of the
    /// test as the discriminator (the node id, the agent id, the instance id),
    /// so every test about `gpu-1` shared one directory. One test's
    /// `remove_dir_all` then deleted the directory another test had open, and
    /// the victim failed at random with "Cannot create a file when that file
    /// already exists" — pointing at the wrong file entirely.
    #[test]
    fn unique_temp_dir_never_repeats_a_path() {
        let a = unique_temp_dir("same-label");
        let b = unique_temp_dir("same-label");
        assert_ne!(a, b, "two calls with the same label collided");
    }

    /// The label still shows up in the path, so a stray directory on disk is
    /// traceable back to the test that made it.
    #[test]
    fn unique_temp_dir_keeps_the_label() {
        let dir = unique_temp_dir("acl");
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains("acl"), "label missing from {name}");
    }
}
