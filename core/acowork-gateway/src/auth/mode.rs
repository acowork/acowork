//! Loopback determination and account-system state (ADR-114).
//!
//! ADR-114 retires the `AUTH_MODE` knob that used to couple "how reachable
//! is this Gateway" to "is there an account system". The two are orthogonal
//! facts:
//!
//! - **Reachability** is a property of the *bind address* and decides who
//!   may connect at all. It is a network-topology concern.
//! - **Account system** is a property of the *data* — it always exists, and
//!   what varies is whether the first administrator has been created yet.
//!   That is [`AccountSystemState`], a runtime fact read from the account
//!   store, never a deployment mode.
//!
//! ## Why the bind address is not enough
//!
//! The old code inferred the account regime from the bind host
//! (`resolve_auth_mode(cli, toml, bind_host)`), which meant flipping
//! `[http].host` from `127.0.0.1` to `0.0.0.0` silently changed six
//! unrelated behaviours — login gating, route registration, session
//! filtering, MQTT ACLs, PM actor semantics and the Desktop's UI gate.
//!
//! This module keeps the *one* thing the bind address genuinely answers —
//! "is this listener reachable from outside the box?" — and answers it from
//! the **actual peer address** of each request rather than from a string
//! comparison on the configured host. A peer address is a fact about the
//! connection in front of us; a bind-host string is a fact about the
//! configuration that may be wrong (a `0.0.0.0` bind behind a NAT, a
//! `127.0.0.1` bind inside a container port-forward, a reverse proxy in
//! front).
//!
//! [`is_loopback_host`] is kept as the explicit *listener*-side counterpart,
//! so the distinction is visible in the type/API surface rather than buried
//! in a comment. It is deliberately **not** used to decide whether a request
//! is trusted: the only trust decision goes through [`is_loopback_peer`].

use std::net::{IpAddr, Ipv4Addr};

pub use acowork_core::account::AccountSystemState;

/// Is this **peer address** a loopback address?
///
/// The authoritative loopback test for a *request*: `127.0.0.0/8`, `::1`
/// and the IPv4-mapped form `::ffff:127.0.0.0/8` are the only addresses that
/// provably cannot leave the host. The mapped form is not academic — a
/// dual-stack listener reports a local IPv4 connection as an IPv6 socket
/// address, and `IpAddr::is_loopback()` alone returns `false` for it, which
/// would lock the Desktop on that very machine out of first-boot setup.
///
/// This is deliberately a function of the peer's IP, not of the address the
/// listener is bound to. The two answer different questions:
///
/// | Question | Answered by |
/// |---|---|
/// | "Can someone outside this machine reach the listener?" | the bind host |
/// | "Did *this* request come from this machine?" | the peer address |
///
/// Only the second one may gate trust. A Gateway bound to `0.0.0.0` still
/// receives loopback requests from a Desktop on the same box, and those are
/// exactly the ones that should be allowed to bootstrap an account.
pub fn is_loopback_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // Only the strict `::ffff:a.b.c.d` mapping carries an IPv4 fact.
            // The deprecated IPv4-compatible form (`::a.b.c.d`) is routable
            // and must NOT inherit the embedded address's trust.
            let o = v6.octets();
            let mapped = (o[..10].iter().all(|&b| b == 0) && o[10] == 0xff && o[11] == 0xff)
                .then(|| Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            mapped.is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Is this **bind host** a loopback address?
///
/// Answers "is this listener reachable only from this machine?" — a
/// topology fact, never a trust decision. Trust goes through
/// [`is_loopback_peer`] instead.
///
/// The host may arrive bracketed (`[::1]`) or bare. A literal hostname
/// resolves to loopback only for `localhost`; a LAN DNS name does not.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_matches(|c| c == '[' || c == ']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        // Reuse the peer predicate so the two helpers can never disagree on
        // what "this machine" means (notably for IPv4-mapped IPv6 literals).
        return is_loopback_peer(ip);
    }
    host.eq_ignore_ascii_case("localhost")
}

/// Is this **bind host** reachable from outside the machine?
///
/// The complement of [`is_loopback_host`], but stated positively so the
/// warning sites read as an assertion rather than a negation. The wildcard
/// bind (`0.0.0.0` / `::`), any routable host (LAN IP, DNS name) and IPv6
/// link-local (`fe80::/10`) all count as externally reachable — the safe
/// side of the question.
pub fn is_externally_reachable_host(host: &str) -> bool {
    !is_loopback_host(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_peer_detection() {
        assert!(is_loopback_peer(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_loopback_peer(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 5))));
        assert!(is_loopback_peer(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        assert!(!is_loopback_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))));
        assert!(!is_loopback_peer(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_loopback_peer(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        // IPv4-mapped loopback: `::ffff:127.0.0.1` is still this machine,
        // and a dual-stack listener is exactly where it shows up.
        assert!(is_loopback_peer("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_loopback_peer("::ffff:127.0.0.9".parse().unwrap()));
        // A mapped *routable* address is still remote.
        assert!(!is_loopback_peer("::ffff:192.168.1.20".parse().unwrap()));
        // The deprecated IPv4-compatible form has no loopback guarantee.
        assert!(!is_loopback_peer("::127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn loopback_bind_detection() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.5"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("::ffff:127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));

        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("::"));
        assert!(!is_loopback_host("192.168.1.20"));
        assert!(!is_loopback_host("fe80::1"));
        assert!(!is_loopback_host("gateway.example.com"));
    }

    #[test]
    fn externally_reachable_is_the_complement_of_loopback() {
        assert!(is_externally_reachable_host("0.0.0.0"));
        assert!(is_externally_reachable_host("192.168.1.20"));
        assert!(is_externally_reachable_host("gateway.example.com"));
        assert!(!is_externally_reachable_host("127.0.0.1"));
        assert!(!is_externally_reachable_host("::1"));
        assert!(!is_externally_reachable_host("localhost"));
    }

    /// The regression this module exists for: the bind address must not
    /// decide whether an account system is present. A `0.0.0.0` bind on a
    /// single-user box is still a single-account system, and a `127.0.0.1`
    /// bind on a shared box is still a multi-account one.
    #[test]
    fn bind_address_does_not_influence_the_account_state() {
        // There is no function left that maps a bind host onto an account
        // regime — the type system proves it: `AccountSystemState` is only
        // ever constructed from an account list.
        let list = acowork_core::account::AccountListFile::default();
        assert_eq!(
            AccountSystemState::from_accounts(&list),
            AccountSystemState::Uninitialized
        );
    }
}
