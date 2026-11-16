//! Gateway Service API handler implementations
//!
//! Contains handler functions for processing Gateway Service API requests.
//! These handlers are transport-agnostic: ADR-033 collapsed the legacy
//! gRPC dispatch (`grpc/dispatch.rs`) and the HTTP route handlers into
//! the same function surface, so any future transport (CLI, IPC, …)
//! can call them directly without going through a per-protocol bridge.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway::state::GatewayState;
use acowork_core::protocol::GatewayResponse;

/// Shared state type: Arc<RwLock<GatewayState>> for concurrent read/write access.
/// RwLock chosen because handlers are predominantly read-heavy (key lookup,
/// budget query) with occasional writes (install/uninstall).
pub type SharedState = Arc<RwLock<GatewayState>>;

// ── Handler implementations ─────────────────────────────────────────────────

pub async fn handle_key_release(
    provider: &str,
    agent_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    // Read-only access to GatewayState
    let state_guard = state.read().await;
    match state_guard.vault.get_key(provider) {
        Ok(api_key) => {
            tracing::info!("KeyRelease for agent={}, provider={}", agent_id, provider);
            GatewayResponse::KeyReleaseResult {
                api_key: Some(api_key),
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                "KeyRelease failed for agent={}, provider={}: {}",
                agent_id,
                provider,
                e
            );
            GatewayResponse::KeyReleaseResult {
                api_key: None,
                error: Some(e.to_string()),
            }
        }
    }
}

/// S4.3.3: Budget query handler — returns real remaining budget
pub async fn handle_budget_query(provider: &str, state: &SharedState) -> GatewayResponse {
    let guard = state.read().await;
    if let Some(tracker) = guard.budget_tracker() {
        let remaining = tracker.remaining_tokens(provider);
        let remaining_cost = tracker.remaining_cost_usd(provider);
        tracing::info!(
            "BudgetQuery: provider={} remaining_tokens={} remaining_cost={}",
            provider,
            remaining,
            remaining_cost
        );
        GatewayResponse::BudgetInfo {
            remaining_tokens: remaining,
            remaining_cost_usd: remaining_cost,
        }
    } else {
        // No budget tracker configured — return unlimited
        GatewayResponse::BudgetInfo {
            remaining_tokens: u64::MAX,
            remaining_cost_usd: f64::MAX,
        }
    }
}

/// S4.3.2: Usage report handler — updates cumulative usage
pub async fn handle_usage_report(
    report: acowork_core::budget::UsageReport,
    state: &SharedState,
) -> GatewayResponse {
    tracing::info!(
        "UsageReport: agent={} provider={} tokens={} cost={:.4}",
        report.agent_id,
        report.provider,
        report.tokens_used,
        report.cost_usd
    );

    let mut guard = state.write().await;
    if let Some(tracker) = guard.budget_tracker_mut() {
        tracker.record_usage(
            &report.agent_id,
            &report.provider,
            report.tokens_used,
            report.cost_usd,
        );
    }

    GatewayResponse::UsageReportAck {}
}

/// S4.4.2: Rate acquire handler — token bucket allocation
pub async fn handle_rate_acquire(provider: &str, state: &SharedState) -> GatewayResponse {
    let mut guard = state.write().await;
    if let Some(limiter) = guard.rate_limiter_mut() {
        let result = limiter.try_acquire_for(provider, "default");
        tracing::info!(
            "RateAcquire: provider={} granted={} retry_after={:?}",
            provider,
            result.granted,
            result.retry_after_ms
        );
        GatewayResponse::RateToken {
            granted: result.granted,
            retry_after_ms: result.retry_after_ms,
        }
    } else {
        // No rate limiter configured — always grant
        GatewayResponse::RateToken {
            granted: true,
            retry_after_ms: None,
        }
    }
}

/// Handle CapabilityQuery request from Runtime.
///
/// S4.2.4: Returns the capability registry for the requested agent
/// or all agents if no filter is specified.
pub async fn handle_capability_query(
    agent_id: Option<&str>,
    state: &SharedState,
) -> GatewayResponse {
    let guard = state.read().await;
    let overview = guard.capability_registry.overview();

    match agent_id {
        Some(id) => {
            // Filter to specific agent
            let mut filtered = std::collections::HashMap::new();
            if let Some(actions) = overview.by_agent.get(id) {
                filtered.insert(id.to_string(), actions.clone());
            }
            tracing::info!("CapabilityQuery: agent={:?}, found={}", id, filtered.len());
            GatewayResponse::CapabilityOverview {
                capabilities: filtered,
            }
        }
        None => {
            tracing::info!(
                "CapabilityQuery: all agents, count={}",
                overview.by_agent.len()
            );
            GatewayResponse::CapabilityOverview {
                capabilities: overview.by_agent,
            }
        }
    }
}

// ── Cron handlers (S3.4) ──────────────────────────────────────────────────

pub async fn handle_cron_register(
    agent_id: &str,
    schedule: &str,
    action: &str,
    params: &serde_json::Value,
    state: &SharedState,
) -> GatewayResponse {
    let (cron_id, store_clone) = {
        let mut guard = state.write().await;
        match guard
            .cron_scheduler
            .register(agent_id, schedule, action, params.clone())
        {
            Ok(id) => {
                let store = guard.cron_store.clone();
                (id, store)
            }
            Err(e) => {
                tracing::warn!(
                    "Cron register failed: agent={} schedule={} error={}",
                    agent_id,
                    schedule,
                    e
                );
                return GatewayResponse::CronRegisterResult {
                    cron_id: None,
                    error: Some(e),
                };
            }
        }
    };

    // P1-9 fix: Use spawn_blocking for file I/O in CronStore
    if let Some(store) = store_clone {
        let entry = crate::cron::StoredCronEntry {
            id: cron_id.clone(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params: serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string()),
            timezone: None,
            retry_count: 0,
            retry_interval_secs: 60,
            max_runs: None,
            run_count: 0,
            expires_at: None,
        };
        let cron_id_clone = cron_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.insert(&entry) {
                tracing::warn!("Failed to persist cron entry {}: {}", cron_id_clone, e);
            }
        })
        .await;
    }

    tracing::info!(
        "Cron registered via gRPC: agent={} cron_id={} schedule={} action={}",
        agent_id,
        cron_id,
        schedule,
        action
    );
    GatewayResponse::CronRegisterResult {
        cron_id: Some(cron_id),
        error: None,
    }
}

pub async fn handle_cron_unregister(cron_id: &str, state: &SharedState) -> GatewayResponse {
    let (removed, store_clone) = {
        let mut guard = state.write().await;
        let removed = guard.cron_scheduler.unregister(cron_id);
        let store = guard.cron_store.clone();
        (removed, store)
    };

    // P1-9 fix: Use spawn_blocking for file I/O in CronStore
    if removed && let Some(store) = store_clone {
        let cron_id_clone = cron_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.delete(&cron_id_clone) {
                tracing::warn!(
                    "Failed to delete cron entry {} from store: {}",
                    cron_id_clone,
                    e
                );
            }
        })
        .await;
    }

    tracing::info!("Cron unregister: cron_id={} removed={}", cron_id, removed);
    GatewayResponse::CronUnregisterResult { removed }
}

pub async fn handle_cron_list(
    agent_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    let guard = state.read().await;
    let entries = guard
        .cron_scheduler
        .entries_for_agent(agent_id)
        .into_iter()
        .map(|e| acowork_core::protocol::CronEntryInfo {
            id: e.id.clone(),
            agent_id: e.agent_id.clone(),
            schedule: e.schedule.clone(),
            action: e.action.clone(),
            params: e.params.clone(),
            timezone: e.timezone.clone(),
            retry_count: e.retry_count,
            retry_interval_secs: e.retry_interval_secs,
            max_runs: e.max_runs,
            run_count: e.run_count,
            expires_at: e.expires_at,
        })
        .collect();

    GatewayResponse::CronListResult { entries }
}

/// Resolved LLM configuration for an Agent.
///
/// Returned by `resolve_llm_config_for_agent`, replaces the previous
/// 6-tuple with named fields for readability and maintainability.
pub struct ResolvedLlmConfig {
    pub provider: String,
    pub model: Option<String>,
    pub api_key: String,
    pub base_url: Option<String>,
    pub models: Vec<String>,
    /// Capabilities for ALL models in this provider, so Runtime can
    /// look them up when switching models without a Gateway roundtrip.
    pub all_model_capabilities: Vec<acowork_core::protocol::ProviderModelEntry>,
    pub compact_model: Option<String>,
}

/// Resolve the LLM configuration to deliver to an Agent.
///
/// Config (base_url, models, capabilities, compact_model) comes from
/// provider_list.json (resource_cache). API key comes from Vault.
///
/// Priority:
/// 1. Gateway config `default_provider` + `default_model`
/// 2. First provider in provider_list.json
/// 3. None (Agent has no provider configured)
///
/// Model resolution order (within the chosen provider):
/// 1. Gateway config `default_model` (explicit user choice)
/// 2. Provider config's models[0]
/// 3. None — Agent Runtime uses the first model from the provider list
pub async fn resolve_llm_config_for_agent(
    _agent_id: &str,
    state: &SharedState,
) -> Option<ResolvedLlmConfig> {
    let state_guard = state.read().await;

    // Try default_provider from Gateway config first
    let default_provider = state_guard
        .config
        .as_ref()
        .and_then(|c| c.default_provider.as_deref());

    // Try default_model from Gateway config
    let config_default_model = state_guard
        .config
        .as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Determine which provider to use
    let provider_name = if let Some(name) = default_provider {
        Some(name.to_string())
    } else {
        // Fall back to first provider in resource cache
        state_guard
            .resource_cache
            .provider_list
            .providers
            .first()
            .map(|p| p.id.clone())
    };

    let provider_name = match provider_name {
        Some(name) => name,
        None => {
            tracing::info!("No provider configured, cannot deliver LLM config");
            return None;
        }
    };

    // Look up provider config from resource_cache (provider_list.json).
    let provider_config = state_guard
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider_name)
        .cloned();

    let provider_config = match provider_config {
        Some(cfg) => cfg,
        None => {
            tracing::warn!(
                "Provider '{}' not found in resource cache",
                provider_name
            );
            return None;
        }
    };

    let models: Vec<String> = provider_config
        .models
        .iter()
        .map(|m| m.id.clone())
        .collect();

    // Model resolution: gateway config default_model > provider's models[0]
    let model = config_default_model
        .map(|m| m.to_string())
        .or_else(|| provider_config.models.first().map(|m| m.id.clone()));

    // Get API key from Vault.
    let api_key = state_guard
        .vault
        .get_key(&provider_name)
        .ok()
        .unwrap_or_default();

    Some(ResolvedLlmConfig {
        provider: provider_name,
        model,
        api_key,
        base_url: if provider_config.base_url.is_empty() {
            None
        } else {
            Some(provider_config.base_url)
        },
        models,
        all_model_capabilities: provider_config.models,
        compact_model: provider_config.compact_model,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = crate::test_support::unique_temp_dir(format!(
            "acowork-test-ipc-state-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn test_shared_state(name: &str) -> SharedState {
        let dir = temp_vault_dir(name);
        Arc::new(RwLock::new(GatewayState::new(&dir)))
    }

    // ── Unit tests for handlers (async, with state) ──────────────────────

    #[tokio::test]
    async fn test_handle_budget_query() {
        let state = test_shared_state("budget-query");
        let response = handle_budget_query("openai", &state).await;
        if let GatewayResponse::BudgetInfo {
            remaining_tokens, ..
        } = response
        {
            // No budget tracker configured → unlimited
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo");
        }
    }

    #[tokio::test]
    async fn test_handle_rate_acquire() {
        let state = test_shared_state("rate-acquire");
        let response = handle_rate_acquire("openai", &state).await;
        if let GatewayResponse::RateToken {
            granted,
            retry_after_ms,
        } = response
        {
            // No rate limiter configured → always grant
            assert!(granted);
            assert!(retry_after_ms.is_none());
        } else {
            panic!("Expected RateToken");
        }
    }

    // ── Permission-related tests removed (old dual-authorization layer deleted) ──
    // See: be7bd1c "权限体系重构 — 删除双授权层 + Shell 命令风险审批机制"

    #[tokio::test]
    async fn test_handle_usage_report() {
        let state = test_shared_state("usage-report");
        let report = acowork_core::budget::UsageReport {
            agent_id: "com.example.weather".to_string(),
            provider: "openai".to_string(),
            tokens_used: 150,
            cost_usd: 0.01,
            timestamp: chrono::Utc::now(),
            error: None,
        };
        let response = handle_usage_report(report, &state).await;
        assert!(matches!(response, GatewayResponse::UsageReportAck {}));
    }

    // ── Integration tests (no longer using legacy IPC transport) ─────

    #[tokio::test]
    async fn test_gateway_state_concurrent_access() {
        let dir = temp_vault_dir("concurrent_rw");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

        let mut handles = Vec::new();

        // Concurrent reads (should not block each other with RwLock)
        for _ in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let guard = state.read().await;
                assert!(guard.installed_agents.is_empty());
            }));
        }

        // Concurrent writes
        for i in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let mut guard = state.write().await;
                let toml_str = r#"
                    agent_id = "com.test"
                    version = "1.0.0"
                    name = "Test"
                    description = "test"
                    author = "test"
                    runtime_version = "0.1.0"
                    [llm]
                    provider = "openai"
                    model = "gpt-4"
                "#;
                let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
                guard.add_installed(crate::gateway::state::AgentInfo {
                    instance_id: format!("com.test.{}", i),
                    agent_id: format!("com.test.{}", i),
                    version: "1.0.0".to_string(),
                    name: format!("Test Agent {}", i),
                    install_path: "/tmp/test".to_string(),
                    manifest,
                    node_id: "local".to_string(),
                });
            }));
        }

        // All tasks should complete without deadlock
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all writes succeeded
        {
            let guard = state.read().await;
            assert_eq!(guard.installed_agents.len(), 5);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn test_capability_broadcast_to_sessions() {
        let (capability_tx, mut cap_rx1) = tokio::sync::broadcast::channel::<GatewayResponse>(64);
        let mut cap_rx2 = capability_tx.subscribe();

        // Simulate an install event — broadcast CapabilityUpdate
        let update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec!["query".to_string(), "forecast".to_string()],
            removed: false,
        };
        capability_tx.send(update.clone()).unwrap();

        // Both subscribers should receive the update
        let msg1 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx1.recv())
            .await
            .expect("Timeout waiting for broadcast on subscriber 1")
            .expect("Channel closed");

        let msg2 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx2.recv())
            .await
            .expect("Timeout waiting for broadcast on subscriber 2")
            .expect("Channel closed");

        match (&msg1, &msg2) {
            (
                GatewayResponse::CapabilityUpdate {
                    agent_id,
                    actions,
                    removed,
                },
                GatewayResponse::CapabilityUpdate { .. },
            ) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(actions.len(), 2);
                assert!(!removed);
            }
            _ => panic!("Expected CapabilityUpdate, got {:?} and {:?}", msg1, msg2),
        }

        // Simulate an uninstall event
        let remove_update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec![],
            removed: true,
        };
        capability_tx.send(remove_update.clone()).unwrap();

        let msg3 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx1.recv())
            .await
            .expect("Timeout waiting for uninstall broadcast")
            .expect("Channel closed");

        match &msg3 {
            GatewayResponse::CapabilityUpdate {
                agent_id,
                actions,
                removed,
            } => {
                assert_eq!(agent_id, "com.example.weather");
                assert!(actions.is_empty());
                assert!(*removed);
            }
            _ => panic!("Expected CapabilityUpdate (removed), got {:?}", msg3),
        }
    }
}
