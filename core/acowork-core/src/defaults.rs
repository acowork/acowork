//! Default constants for Gateway configuration.
//!
//! All crates should reference these constants instead of hardcoding
//! host, port, or URL values for the Gateway HTTP API.

/// Default Gateway HTTP listen port
pub const GATEWAY_HTTP_PORT: u16 = 19876;

/// The Gateway HTTP port this process should call back into.
/// [`GATEWAY_HTTP_PORT`] by default; `ACOWORK_GATEWAY_HTTP_PORT` overrides
/// it so a process-level test can boot a private Gateway on a free port
/// instead of colliding with the well-known one a desktop instance holds.
pub fn gateway_http_port() -> u16 {
    std::env::var("ACOWORK_GATEWAY_HTTP_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(GATEWAY_HTTP_PORT)
}

/// Default Gateway HTTP listen host (localhost only)
pub const GATEWAY_HTTP_HOST: &str = "127.0.0.1";

/// Default Gateway HTTP base URL (composed from host and port)
pub const GATEWAY_HTTP_URL: &str = "http://127.0.0.1:19876";

/// Default maximum port when auto-incrementing on conflict
pub const GATEWAY_HTTP_PORT_MAX: u16 = 19878;

// ── MQTT defaults (ADR-033) ──────────────────────────────────────────

/// Default MQTT broker listen host
pub const GATEWAY_MQTT_HOST: &str = "127.0.0.1";
/// Default MQTT broker listen port
pub const GATEWAY_MQTT_PORT: u16 = 19875;
/// Maximum TCP connections for the embedded MQTT broker
pub const GATEWAY_MQTT_MAX_CONNECTIONS: usize = 100;
/// Maximum MQTT packet size (10 MB)
pub const GATEWAY_MQTT_MAX_PACKET_SIZE: usize = 10 * 1024 * 1024;
/// Client ID for the Gateway's own MQTT publisher
pub const GATEWAY_MQTT_PUBLISHER_CLIENT_ID: &str = "gateway:publisher";
