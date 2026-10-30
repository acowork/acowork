//! Capability registry for Intent routing
//!
//! Tracks which Agent INSTANCE provides which capabilities (actions),
//! enabling the IntentRouter to discover target agents for
//! incoming Intent requests.
//!
//! ADR-073: the registry key is the INSTANCE identity — a package
//! installed twice registers two independent capability sets, and
//! intent routing can address either instance. `agent_id` is kept as
//! positional metadata for overview aggregation and display.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Capability key: `{instance_id}:{action}`
pub type CapabilityKey = String;

/// Registered capability entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredCapability {
    /// ADR-073: instance identity (UUID v4) providing this capability.
    pub instance_id: String,
    /// Package identity (from manifest) — display/aggregation only.
    pub agent_id: String,
    /// Action name (e.g., "weather_query", "calendar_schedule")
    pub action: String,
    /// Capability definition
    pub definition: acowork_core::CapabilityDef,
}

/// Capability registry — maps `{instance_id}:{action}` to CapabilityDef
///
/// S4.2.1: Registry data structure (HashMap<agent:action, CapabilityDef>)
/// Capabilities are registered during package installation and
/// removed during uninstallation.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    /// Map of "{instance_id}:{action}" → RegisteredCapability
    capabilities: HashMap<CapabilityKey, RegisteredCapability>,
}

impl CapabilityRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the capability key from instance id and action
    pub fn make_key(instance_id: &str, action: &str) -> CapabilityKey {
        format!("{}:{}", instance_id, action)
    }

    /// Register a capability for an agent instance
    ///
    /// S4.2.2: Called during package installation to register
    /// all capabilities declared in the manifest.
    pub fn register(
        &mut self,
        instance_id: &str,
        agent_id: &str,
        action: &str,
        definition: acowork_core::CapabilityDef,
    ) {
        let key = Self::make_key(instance_id, action);
        tracing::info!(
            instance_id,
            agent_id,
            action,
            "Registering capability: {}",
            key
        );
        self.capabilities.insert(
            key,
            RegisteredCapability {
                instance_id: instance_id.to_string(),
                agent_id: agent_id.to_string(),
                action: action.to_string(),
                definition,
            },
        );
    }

    /// Register all capabilities from an agent manifest
    ///
    /// S4.2.2: Install-time registration
    pub fn register_from_manifest(
        &mut self,
        instance_id: &str,
        agent_id: &str,
        manifest: &acowork_core::AgentManifest,
    ) {
        for (action, def) in &manifest.capabilities {
            self.register(instance_id, agent_id, action, def.clone());
        }
    }

    /// Unregister all capabilities for an agent INSTANCE
    ///
    /// S4.2.3: Called during package uninstallation
    pub fn unregister_instance(&mut self, instance_id: &str) {
        let prefix = format!("{}:", instance_id);
        let keys_to_remove: Vec<String> = self
            .capabilities
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();

        for key in keys_to_remove {
            tracing::info!("Unregistering capability: {}", key);
            self.capabilities.remove(&key);
        }
    }

    /// Look up a capability by instance id and action
    ///
    /// S4.2.4: CapabilityQuery handler support
    pub fn get(&self, instance_id: &str, action: &str) -> Option<&RegisteredCapability> {
        let key = Self::make_key(instance_id, action);
        self.capabilities.get(&key)
    }

    /// Look up a capability by action only (find which instances provide it)
    ///
    /// Deprecated (intent-delivery-tech-debt-plan D8): action-based
    /// discovery was the pre-ADR-073 `IntentRouter` mechanism — Intents
    /// address an **instance** (ADR-073), not "whoever can do X". The
    /// router is gone; nothing calls this. Kept for the capability
    /// overview surface only if a future feature needs reverse lookup;
    /// do NOT wire it into a new delivery path.
    #[deprecated(since = "1.0.0", note = "Intent delivery addresses instances, not actions (ADR-073/D8)")]
    pub fn find_by_action(&self, action: &str) -> Vec<&RegisteredCapability> {
        self.capabilities
            .values()
            .filter(|c| c.action == action)
            .collect()
    }

    /// Get all capabilities for a specific agent package (all instances)
    ///
    /// S4.2.5: capability_overview push support
    pub fn capabilities_for_agent(&self, agent_id: &str) -> Vec<&RegisteredCapability> {
        self.capabilities
            .values()
            .filter(|c| c.agent_id == agent_id)
            .collect()
    }

    /// Get all capabilities for a specific agent INSTANCE
    pub fn capabilities_for_instance(&self, instance_id: &str) -> Vec<&RegisteredCapability> {
        self.capabilities
            .values()
            .filter(|c| c.instance_id == instance_id)
            .collect()
    }

    /// Get all registered capabilities
    pub fn all_capabilities(&self) -> Vec<&RegisteredCapability> {
        self.capabilities.values().collect()
    }

    /// Get count of registered capabilities
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// S2.4: Check if a specific agent instance declares a specific action
    ///
    /// Used by Intent permission validation to verify that the
    /// target agent's capability matches the requested action.
    pub fn has_action(&self, instance_id: &str, action: &str) -> bool {
        let key = Self::make_key(instance_id, action);
        self.capabilities.contains_key(&key)
    }

    /// Get capability overview for handshake step ⑤
    ///
    /// S4.2.5: Returns a summary of all capabilities for the
    /// capability_overview push during AgentHello handshake.
    /// ADR-073: aggregated by PACKAGE so multiple instances of the
    /// same agent do not duplicate the handshake payload.
    pub fn overview(&self) -> CapabilityOverview {
        let mut by_agent: HashMap<String, Vec<String>> = HashMap::new();
        for cap in self.capabilities.values() {
            by_agent
                .entry(cap.agent_id.clone())
                .or_default()
                .push(cap.action.clone());
        }
        CapabilityOverview { by_agent }
    }
}

/// Capability overview for handshake push
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityOverview {
    /// Map of agent_id → list of available actions
    pub by_agent: HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_capability_def(desc: &str) -> acowork_core::CapabilityDef {
        acowork_core::CapabilityDef {
            description: desc.to_string(),
            skills: Vec::new(),
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn test_registry_new() {
        let registry = CapabilityRegistry::new();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "query",
            test_capability_def("Query weather"),
        );

        let cap = registry.get("inst-weather-1", "query").unwrap();
        assert_eq!(cap.instance_id, "inst-weather-1");
        assert_eq!(cap.agent_id, "com.example.weather");
        assert_eq!(cap.action, "query");
        assert_eq!(cap.definition.description, "Query weather");
    }

    #[test]
    fn test_register_from_manifest() {
        let mut registry = CapabilityRegistry::new();
        let toml_str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            [capabilities.query]
            description = "Query weather"
            [capabilities.forecast]
            description = "Weather forecast"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
        registry.register_from_manifest("inst-weather-1", "com.example.weather", &manifest);

        assert_eq!(registry.len(), 2);
        assert!(registry.get("inst-weather-1", "query").is_some());
        assert!(registry.get("inst-weather-1", "forecast").is_some());
    }

    #[test]
    fn test_unregister_instance() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "query",
            test_capability_def("Query"),
        );
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "forecast",
            test_capability_def("Forecast"),
        );
        registry.register(
            "inst-calendar-1",
            "com.example.calendar",
            "schedule",
            test_capability_def("Schedule"),
        );

        assert_eq!(registry.len(), 3);
        registry.unregister_instance("inst-weather-1");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("inst-weather-1", "query").is_none());
        assert!(registry.get("inst-calendar-1", "schedule").is_some());
    }

    #[allow(deprecated)]
    #[test]
    fn test_find_by_action() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "query",
            test_capability_def("Weather query"),
        );
        registry.register(
            "inst-search-1",
            "com.example.search",
            "query",
            test_capability_def("Search query"),
        );

        let results = registry.find_by_action("query");
        assert_eq!(results.len(), 2);
    }

    #[allow(deprecated)]
    #[test]
    fn test_capabilities_for_agent_and_instance() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "query",
            test_capability_def("Query"),
        );
        registry.register(
            "inst-weather-2",
            "com.example.weather",
            "forecast",
            test_capability_def("Forecast"),
        );
        registry.register(
            "inst-calendar-1",
            "com.example.calendar",
            "schedule",
            test_capability_def("Schedule"),
        );

        // Package aggregation spans instances.
        let weather_caps = registry.capabilities_for_agent("com.example.weather");
        assert_eq!(weather_caps.len(), 2);
        // Instance scoping is exact.
        assert_eq!(registry.capabilities_for_instance("inst-weather-1").len(), 1);
        assert_eq!(registry.capabilities_for_instance("inst-weather-2").len(), 1);
    }

    /// ADR-073: the same package installed twice registers INDEPENDENT
    /// capability sets — no last-write-wins on the registry key.
    #[test]
    fn test_same_package_two_instances_do_not_collide() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-a",
            "com.example.weather",
            "query",
            test_capability_def("Instance A query"),
        );
        registry.register(
            "inst-weather-b",
            "com.example.weather",
            "query",
            test_capability_def("Instance B query"),
        );

        assert_eq!(registry.len(), 2, "two instances = two registry entries");
        assert!(registry.get("inst-weather-a", "query").is_some());
        assert!(registry.get("inst-weather-b", "query").is_some());
        assert_eq!(
            registry
                .get("inst-weather-b", "query")
                .unwrap()
                .definition
                .description,
            "Instance B query"
        );
        // Uninstalling one instance must not affect the other.
        registry.unregister_instance("inst-weather-a");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("inst-weather-b", "query").is_some());
    }

    #[test]
    fn test_overview() {
        let mut registry = CapabilityRegistry::new();
        registry.register(
            "inst-weather-1",
            "com.example.weather",
            "query",
            test_capability_def("Query"),
        );
        registry.register(
            "inst-weather-2",
            "com.example.weather",
            "forecast",
            test_capability_def("Forecast"),
        );

        let overview = registry.overview();
        assert_eq!(overview.by_agent.len(), 1);
        assert_eq!(overview.by_agent["com.example.weather"].len(), 2);
    }

    #[test]
    fn test_make_key() {
        assert_eq!(
            CapabilityRegistry::make_key("com.example.weather", "query"),
            "com.example.weather:query"
        );
    }
}
