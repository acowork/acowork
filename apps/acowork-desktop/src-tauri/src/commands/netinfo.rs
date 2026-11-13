//! Local network interface enumeration for the frontend connectivity
//! guard ([localNetwork.ts](../../../src/lib/connectivity/localNetwork.ts)).
//!
//! The guard needs to answer one question cheaply and offline: "is the
//! persisted Gateway URL host one of THIS machine's own addresses?"
//! That single fact decides whether a Wi-Fi hop / wake-from-sleep turns
//! the URL into a stale black hole (gateway still on this machine) or
//! leaves a genuinely remote gateway untouched. Kept in Rust because
//! the webview cannot enumerate interfaces.

use std::net::Ipv4Addr;

/// Keep only the addresses that identify this machine on a network.
///
/// Pure so the filtering rules stay unit-testable without touching the
/// host's real interfaces. Loopback and unspecified addresses are
/// skipped: the connectivity guard already treats loopback URLs as
/// exempt (it is the preferred destination, never a "foreign" host).
fn filter_local_ipv4(addrs: impl Iterator<Item = Ipv4Addr>) -> Vec<String> {
    addrs
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .map(|ip| ip.to_string())
        .collect()
}

/// This machine's current non-loopback IPv4 addresses.
///
/// Enumeration failures degrade to an empty list: the frontend guard
/// treats "no addresses" as "cannot decide" and simply does nothing.
#[tauri::command]
pub fn get_local_ipv4_addresses() -> Vec<String> {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(addrs) => addrs,
        Err(e) => {
            tracing::warn!(target: "netinfo", "interface enumeration failed: {e}");
            return Vec::new();
        }
    };
    filter_local_ipv4(addrs.into_iter().filter_map(|a| match a.addr {
        if_addrs::IfAddr::V4(v4) => Some(v4.ip),
        if_addrs::IfAddr::V6(_) => None,
    }))
}

/// The IPv4 of the interface that owns the DEFAULT ROUTE — the one address
/// a LAN peer (e.g. a phone pairing over Wi-Fi) can actually reach.
///
/// Why this exists: `get_local_ipv4_addresses` returns interfaces in
/// enumeration order, and virtual adapters (WSL vEthernet, Hyper-V,
/// security-suite virtual NICs) routinely sort FIRST — a phone that
/// dials that address gets nothing. The interface that owns the default
/// route is the one genuinely attached to the LAN (same rule
/// `dev/common/build_core.ps1` uses to pick `--advertise-host`).
///
/// Implementation: the classic UDP "connect trick". Connecting a UDP
/// socket sends no packet — it only makes the OS resolve a route and pick
/// the local source address for it. The sentinel target is a public DNS
/// server, but any routable address works; offline machines still have a
/// default route, so this succeeds without any traffic. Returns `None`
/// when no route exists (fully offline / exotic stack) — callers fall
/// back to the interface list.
#[tauri::command]
pub fn get_default_route_ipv4() -> Option<String> {
    default_route_ipv4()
}

/// Crate-visible: `spawn_gateway` uses this to pin the advertise host of a
/// Desktop-spawned Gateway to the LAN-reachable interface (2026-11-13
/// product decision: the child binds 0.0.0.0 so a phone can pair via QR).
pub(crate) fn default_route_ipv4() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // Any routable sentinel; no packet is ever sent for a UDP connect.
    sock.connect("8.8.8.8:53").ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
            Some(v4.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_drops_loopback_and_unspecified() {
        let got = filter_local_ipv4(
            [
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::new(192, 168, 17, 113),
                Ipv4Addr::UNSPECIFIED,
            ]
            .into_iter(),
        );
        assert_eq!(got, vec!["192.168.17.113".to_string()]);
    }

    #[test]
    fn filter_preserves_multiple_lan_addresses() {
        let got = filter_local_ipv4(
            [
                Ipv4Addr::new(10, 0, 0, 4),
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::new(192, 168, 0, 101),
            ]
            .into_iter(),
        );
        assert_eq!(got, vec!["10.0.0.4".to_string(), "192.168.0.101".to_string()]);
    }
}
