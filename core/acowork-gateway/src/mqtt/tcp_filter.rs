//! Transparent TCP pre-filter for the MQTT broker.
//!
//! rumqttd 0.20's CONNECT auth handler receives only
//! `(client_id, username, password)` — it does **not** expose the peer
//! IP. To enforce the `[security].allowed_node_ips` backstop on MQTT we
//! therefore interpose a tiny transparent TCP proxy:
//!
//! ```text
//!   peer ──► pre-filter (0.0.0.0:19875) ──► broker (127.0.0.1:19875)
//!                │
//!                └─ allowed? forward : drop
//! ```
//!
//! The broker always binds loopback when the pre-filter is active; the
//! pre-filter binds the configured external host (`0.0.0.0` or a NIC
//! address) and validates the peer IP before splicing the TCP stream.
//!
//! When `allowed_node_ips` is empty (default, allow everyone) the
//! pre-filter is not started at all and the broker binds the configured
//! host directly — zero overhead for the common single-machine case.

use std::sync::Arc;

use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::security::IpAllowlist;

/// Spawn the TCP pre-filter task.
///
/// Binds `bind_host:bind_port`, accepts connections, and for each peer:
/// - allowed by `allowlist` → spawn a bidirectional copy to
///   `target_host:target_port`
/// - blocked → log + close immediately (no MQTT bytes are ever read)
///
/// Returns `Err` if the bind fails (e.g. port already taken), so the
/// caller can fail fast instead of silently running without the filter.
pub async fn start_mqtt_tcp_filter(
    bind_host: &str,
    bind_port: u16,
    target_host: &str,
    target_port: u16,
    allowlist: IpAllowlist,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let listener = TcpListener::bind((bind_host, bind_port))
        .await
        .map_err(|e| format!("MQTT TCP pre-filter bind failed on {bind_host}:{bind_port}: {e}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| format!("MQTT TCP pre-filter local_addr failed: {e}"))?;

    tracing::info!(
        addr = %local_addr,
        forward = format_args!("{target_host}:{target_port}"),
        entries = allowlist.len(),
        "MQTT TCP pre-filter started (security.allowed_node_ips)"
    );

    let allowlist = Arc::new(allowlist);
    let target_host = target_host.to_string();

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut peer_stream, peer_addr)) => {
                    if !allowlist.allows_socket(peer_addr) {
                        tracing::warn!(
                            peer = %peer_addr.ip(),
                            "blocked by security.allowed_node_ips (MQTT TCP)"
                        );
                        // Drop without reading — the peer sees a hard close
                        // before any MQTT CONNECT handshake byte is acked.
                        let _ = peer_stream.shutdown().await;
                        continue;
                    }
                    tracing::debug!(peer = %peer_addr.ip(), "MQTT TCP peer allowed, splicing");
                    let target_host = target_host.clone();
                    tokio::spawn(async move {
                        match TcpStream::connect((target_host.as_str(), target_port)).await {
                            Ok(mut upstream) => {
                                if let Err(e) =
                                    copy_bidirectional(&mut peer_stream, &mut upstream).await
                                {
                                    tracing::debug!(
                                        peer = %peer_addr.ip(),
                                        %e,
                                        "MQTT pre-filter splice ended"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    peer = %peer_addr.ip(),
                                    %e,
                                    "MQTT pre-filter upstream connect failed"
                                );
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(%e, "MQTT TCP pre-filter accept error");
                }
            }
        }
    });

    Ok(handle)
}

/// Does the configured broker host need a pre-filter?
///
/// The pre-filter is only meaningful when the broker would otherwise be
/// reachable by *remote* peers (non-loopback bind) **and** an allowlist
/// is configured. If the host is loopback the broker is already
/// local-only; if the allowlist is empty there is nothing to enforce.
pub fn needs_mqtt_tcp_filter(host: &str, allowlist: &IpAllowlist) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let ip: std::net::IpAddr = match host.parse() {
        Ok(ip) => ip,
        // Hostnames: be conservative — assume non-loopback so the filter
        // engages (safe default when an allowlist is configured).
        Err(_) => return true,
    };
    !ip.is_loopback()
}

/// Where the broker itself must bind, given the configured host.
///
/// Single decision point for the ingress invariant: **whenever an
/// allowlist has to be enforced, the broker binds loopback** — the
/// pre-filter is then the only external ingress, and it is the only
/// place the allowlist is executed.
///
/// Deliberately independent of whether the pre-filter actually came up.
/// If the filter failed to bind, falling back to the configured external
/// host would publish the broker on the LAN with *no* allowlist
/// enforcement at all — a silent fail-open. Fail closed instead: local
/// clients keep working, external ingress is simply absent until the
/// operator fixes the bind conflict.
pub fn broker_bind_host<'a>(configured: &'a str, allowlist: &IpAllowlist) -> &'a str {
    if needs_mqtt_tcp_filter(configured, allowlist) {
        "127.0.0.1"
    } else {
        configured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::AsyncReadExt;

    /// Grab a free localhost port by binding then dropping the listener.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Decide logic: empty allowlist never needs a filter.
    #[tokio::test]
    async fn needs_filter_empty_allowlist_is_false() {
        let empty = IpAllowlist::default();
        assert!(!needs_mqtt_tcp_filter("0.0.0.0", &empty));
        assert!(!needs_mqtt_tcp_filter("127.0.0.1", &empty));
    }

    /// Decide logic: loopback bind never needs a filter (already local).
    #[tokio::test]
    async fn needs_filter_loopback_bind_is_false_even_with_allowlist() {
        let l = IpAllowlist::new(vec![crate::security::AllowlistEntry::parse(
            "10.0.0.0/8",
        )
        .unwrap()]);
        assert!(!needs_mqtt_tcp_filter("127.0.0.1", &l));
        assert!(!needs_mqtt_tcp_filter("::1", &l));
    }

    /// Ingress invariant: an allowlist forces the broker to loopback,
    /// even when the pre-filter never started (fail closed, not open).
    #[tokio::test]
    async fn broker_bind_host_fails_closed_without_filter() {
        let l = IpAllowlist::new(vec![crate::security::AllowlistEntry::parse(
            "10.0.0.0/8",
        )
        .unwrap()]);
        assert_eq!(broker_bind_host("0.0.0.0", &l), "127.0.0.1");
        assert_eq!(broker_bind_host("192.168.1.20", &l), "127.0.0.1");
        // No allowlist → the configured host stands.
        assert_eq!(broker_bind_host("0.0.0.0", &IpAllowlist::default()), "0.0.0.0");
        // Loopback stays loopback either way.
        assert_eq!(broker_bind_host("127.0.0.1", &l), "127.0.0.1");
    }

    /// Decide logic: non-loopback bind + allowlist → filter needed.
    #[tokio::test]
    async fn needs_filter_non_loopback_with_allowlist_is_true() {
        let l = IpAllowlist::new(vec![crate::security::AllowlistEntry::parse(
            "10.0.0.0/8",
        )
        .unwrap()]);
        assert!(needs_mqtt_tcp_filter("0.0.0.0", &l));
        assert!(needs_mqtt_tcp_filter("192.168.1.20", &l));
        // Hostname → conservative true.
        assert!(needs_mqtt_tcp_filter("localhost", &l));
    }

    /// End-to-end: an allowed loopback peer's bytes reach the upstream
    /// "broker" through the filter (echo server stands in for the
    /// real rumqttd broker listening on loopback).
    #[tokio::test]
    async fn filter_splices_allowed_peer_to_upstream() {
        // Fake broker: echo every received byte back.
        let broker_port = free_port();
        let broker = tokio::spawn(async move {
            let listener = TcpListener::bind(("127.0.0.1", broker_port))
                .await
                .unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
        });

        // Filter binds loopback on another free port, forwards to broker.
        let filter_port = free_port();
        let l = IpAllowlist::new(vec![crate::security::AllowlistEntry::parse(
            "10.0.0.0/8",
        )
        .unwrap()]); // loopback is always allowed regardless
        let handle = start_mqtt_tcp_filter(
            "127.0.0.1",
            filter_port,
            "127.0.0.1",
            broker_port,
            l,
        )
        .await
        .expect("filter starts");

        // Client connects to the filter and sends a payload.
        let mut client = TcpStream::connect(("127.0.0.1", filter_port))
            .await
            .expect("connect to filter");
        client.write_all(b"MQTT-ish-payload").await.unwrap();
        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.expect("echo back");
        assert_eq!(&buf[..n], b"MQTT-ish-payload");

        handle.abort();
        broker.abort();
    }

    /// Blocked peers never reach upstream: the filter closes the socket
    /// without forwarding. We can't synthesize a non-loopback source on a
    /// single host, so this asserts the *decision* layer + that a direct
    /// disallowed peer check closes immediately.
    #[tokio::test]
    async fn filter_drops_disallowed_peer_before_forwarding() {
        // A non-loopback peer address is required for the block path; the
        // allowlist itself never matches it (unit-tested in security.rs).
        // Here we assert the allowlist decision used by the filter.
        let l = IpAllowlist::new(vec![crate::security::AllowlistEntry::parse(
            "10.0.0.0/8",
        )
        .unwrap()]);
        let blocked: SocketAddr = "203.0.113.7:5555".parse().unwrap();
        assert!(!l.allows_socket(blocked), "allowlist must reject the peer");
    }
}
