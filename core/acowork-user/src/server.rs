//! acowork-user HTTP server entry.
//!
//! Runs as a **standalone process** supervised by the Gateway (ADR-084
//! §决策 5, the PM/doc pattern). [`UserService::serve`] binds a port and
//! serves the full router (user domain + `/health`) behind the identity
//! layer.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;

use crate::config::UserServiceConfig;
use crate::error::ApiError;
use crate::state::AppState;

/// The user service running instance.
pub struct UserService {
    pub config: UserServiceConfig,
    pub state: AppState,
}

impl UserService {
    /// Construct the service (no server started yet).
    ///
    /// The account service is always present (ADR-114): there is no
    /// deployment in which the user service runs without one.
    pub fn new(config: UserServiceConfig, auth: Arc<crate::auth::AuthService>) -> Self {
        Self {
            state: AppState::new(&config, auth),
            config,
        }
    }

    /// Build the full router (user domain + `/health`, behind the identity
    /// layer).
    pub fn router(&self) -> Router {
        crate::http::build_router(&self.state).with_state(self.state.clone())
    }

    /// Serve the full router on `bind`, returning the **actual** bound
    /// address (reported via `--port-file` to the Gateway supervisor).
    ///
    /// Port conflict auto-increments (default 18083 up, max +20), matching
    /// PM/doc. The server runs in a background task.
    ///
    /// # Bind address
    ///
    /// Only loopback is accepted. The service trusts the `X-Auth-*` headers
    /// the Gateway injects (ADR-084 §决策 7); on any other interface a peer
    /// could forge them and impersonate any user. There is no override —
    /// adding one would mean writing a threat model for it first.
    pub async fn serve(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr, ApiError> {
        if !bind.ip().is_loopback() {
            return Err(ApiError::bad_request(&format!(
                "acowork-user must bind loopback (got {}): it trusts Gateway-injected \
                 X-Auth-* headers, which any other host could forge (ADR-084 §决策 7)",
                bind.ip()
            )));
        }

        let host = bind.ip();
        let mut port = bind.port();
        let max_port = port.saturating_add(20);
        let router = self.router();

        loop {
            let addr = SocketAddr::new(host, port);
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    let actual = listener.local_addr().map_err(|e| {
                        ApiError::internal(&format!("failed to read bound address: {e}"))
                    })?;
                    tracing::info!(addr = %actual, "acowork-user server listening (full router)");
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(listener, router).await {
                            tracing::error!(error = %e, "acowork-user server exited with error");
                        }
                    });
                    return Ok(actual);
                }
                Err(_) if port < max_port => {
                    tracing::warn!(port, "acowork-user port occupied — trying next");
                    port += 1;
                }
                Err(e) => {
                    return Err(ApiError::internal(&format!(
                        "failed to bind acowork-user on {addr}: {e}"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The service trusts `X-Auth-*` headers (ADR-084 §决策 7): the only
    //! thing standing between a forged peer and impersonation is
    //! `bind.ip()` being loopback. Pin both branches — the reject path
    //! is the interesting one (a silent loopback failure would defeat the
    //! whole trust model).
    use super::*;

    fn test_config() -> UserServiceConfig {
        // Throwaway data dir under temp keeps the constructor happy without
        // touching the operator's real `~/.acowork/acowork-user`.
        let dir = std::env::temp_dir().join(format!(
            "acowork-user-loopback-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        UserServiceConfig {
            data_dir: dir,
            ..UserServiceConfig::default()
        }
    }

    /// A throwaway account service for the constructor (ADR-114: the
    /// account system is unconditional, so the service cannot be built
    /// without one).
    fn test_auth(config: &UserServiceConfig) -> Arc<crate::auth::AuthService> {
        Arc::new(crate::auth::AuthService::new(&config.data_dir, Default::default(), None).unwrap())
    }

    #[tokio::test]
    async fn serve_rejects_non_loopback_bind() {
        let config = test_config();
        let svc = Arc::new(UserService::new(config.clone(), test_auth(&config)));
        let non_loopback: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let err = svc
            .serve(non_loopback)
            .await
            .expect_err("non-loopback bind must be rejected (ADR-084 §决策 7)");
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("loopback"),
            "rejection must explain the loopback requirement; got: {msg}"
        );
    }

    #[tokio::test]
    async fn serve_accepts_loopback_bind() {
        let config = test_config();
        let svc = Arc::new(UserService::new(config.clone(), test_auth(&config)));
        let actual = svc
            .serve("127.0.0.1:0".parse().unwrap())
            .await
            .expect("loopback bind must succeed");
        assert_eq!(actual.ip().to_string(), "127.0.0.1");
        assert!(actual.port() != 0, "ephemeral port must be resolved");
    }
}

