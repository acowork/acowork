//! Cron scheduler for time-based Intent triggers
//!
//! S4.5: Allows Agents to register cron schedules. When a schedule fires,
//! the Gateway pushes an IntentReceived to the registered Agent.
//!
//! Supports simplified 5-field cron expressions: `min hour day month weekday`
//!
//! Example schedules:
//! - `0 * * * *`     — every hour at minute 0
//! - `*/15 * * * *`  — every 15 minutes
//! - `0 9 * * 1-5`   — weekdays at 9:00 AM
//! - `0 0 1 * *`     — first day of every month at midnight

pub mod store;

use chrono::{Datelike, Timelike};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::mqtt::GatewayMqttClient;
pub use store::{CronStore, CronStoreError, StoredCronEntry};

/// A registered cron entry (S5.8 enhanced)
#[derive(Debug, Clone)]
pub struct CronEntry {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent that owns this cron entry
    pub agent_id: String,
    /// Cron schedule expression (5-field)
    pub schedule: String,
    /// Action to fire when the schedule triggers
    pub action: String,
    /// Params to include in the IntentReceived
    pub params: serde_json::Value,
    /// Timezone for schedule interpretation (None = UTC)
    pub timezone: Option<String>,
    /// Max retry count on failure (0 = no retry)
    pub retry_count: u32,
    /// Retry backoff interval in seconds (default 60)
    pub retry_interval_secs: u64,
    /// Max total executions (None = unlimited)
    pub max_runs: Option<u32>,
    /// Current execution count
    pub run_count: u32,
    /// Expiry timestamp in Unix millis (None = never expires)
    pub expires_at: Option<i64>,
    /// Parsed schedule fields
    parsed: CronFields,
}

/// Parsed cron fields (min, hour, day, month, weekday)
#[derive(Debug, Clone)]
struct CronFields {
    minutes: Vec<u8>,
    hours: Vec<u8>,
    days: Vec<u8>,
    months: Vec<u8>,
    weekdays: Vec<u8>,
}

/// Cron scheduler — manages cron entries and fires triggers
#[derive(Debug, Clone, Default)]
pub struct CronScheduler {
    /// Cron entries by ID
    entries: HashMap<String, CronEntry>,
    /// Next ID counter
    next_id: u64,
}

impl CronScheduler {
    /// Create a new empty scheduler
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    /// Register a new cron entry with default settings.
    ///
    /// Convenience wrapper around `register_full` that uses defaults for
    /// timezone (UTC), retry (disabled), and no expiry/max_runs.
    pub fn register(
        &mut self,
        agent_id: &str,
        schedule: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<String, String> {
        self.register_full(agent_id, schedule, action, params, None, 0, 60, None, None)
    }

    /// Register a new cron entry with full options (S5.8 enhanced).
    ///
    /// Returns the cron entry ID on success, or an error message if the
    /// schedule expression is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn register_full(
        &mut self,
        agent_id: &str,
        schedule: &str,
        action: &str,
        params: serde_json::Value,
        timezone: Option<String>,
        retry_count: u32,
        retry_interval_secs: u64,
        max_runs: Option<u32>,
        expires_at: Option<i64>,
    ) -> Result<String, String> {
        let parsed = parse_cron(schedule)?;
        let id = format!("cron-{}", self.next_id);
        self.next_id += 1;

        let entry = CronEntry {
            id: id.clone(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params,
            timezone,
            retry_count,
            retry_interval_secs,
            max_runs,
            run_count: 0,
            expires_at,
            parsed,
        };

        tracing::info!(
            "Cron registered: id={} agent={} schedule={} action={}",
            id,
            agent_id,
            schedule,
            action
        );
        self.entries.insert(id.clone(), entry);
        Ok(id)
    }

    /// Unregister a cron entry by ID
    pub fn unregister(&mut self, cron_id: &str) -> bool {
        if let Some(entry) = self.entries.remove(cron_id) {
            tracing::info!("Cron unregistered: id={} agent={}", cron_id, entry.agent_id);
            true
        } else {
            false
        }
    }

    /// Unregister all cron entries for an agent (called on agent stop)
    pub fn unregister_agent(&mut self, agent_id: &str) -> usize {
        let ids_to_remove: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect();

        let count = ids_to_remove.len();
        for id in ids_to_remove {
            self.entries.remove(&id);
        }
        if count > 0 {
            tracing::info!(
                "Cron: unregistered {} entries for agent {}",
                count,
                agent_id
            );
        }
        count
    }

    /// Check which cron entries should fire at the given time
    ///
    /// Returns a list of (agent_id, action, params) tuples for entries
    /// whose schedule matches the given time.
    pub fn check(
        &self,
        time: &chrono::DateTime<chrono::Utc>,
    ) -> Vec<(&str, &str, &serde_json::Value)> {
        let minute = time.minute() as u8;
        let hour = time.hour() as u8;
        let day = time.day() as u8;
        let month = time.month() as u8;
        let weekday = time.weekday().num_days_from_sunday() as u8; // 0=Sun, 6=Sat

        self.entries
            .values()
            .filter(|e| {
                let p = &e.parsed;
                p.minutes.contains(&minute)
                    && p.hours.contains(&hour)
                    && p.days.contains(&day)
                    && p.months.contains(&month)
                    && p.weekdays.contains(&weekday)
            })
            .map(|e| (e.agent_id.as_str(), e.action.as_str(), &e.params))
            .collect()
    }

    /// Get the number of registered entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the scheduler is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries for a specific agent
    pub fn entries_for_agent(&self, agent_id: &str) -> Vec<&CronEntry> {
        self.entries
            .values()
            .filter(|e| e.agent_id == agent_id)
            .collect()
    }

    /// Load entries from a CronStore (used on Gateway restart)
    pub fn load_from_store(&mut self, store: &CronStore) -> Result<(), String> {
        let stored = store
            .list_all()
            .map_err(|e| format!("Failed to load cron entries: {}", e))?;
        for entry in stored {
            if let Ok(parsed) = parse_cron(&entry.schedule) {
                let cron_entry = CronEntry {
                    id: entry.id.clone(),
                    agent_id: entry.agent_id.clone(),
                    schedule: entry.schedule.clone(),
                    action: entry.action.clone(),
                    params: serde_json::from_str(&entry.params).unwrap_or(serde_json::json!({})),
                    timezone: entry.timezone.clone(),
                    retry_count: entry.retry_count,
                    retry_interval_secs: entry.retry_interval_secs,
                    max_runs: entry.max_runs,
                    run_count: entry.run_count,
                    expires_at: entry.expires_at,
                    parsed,
                };
                self.entries.insert(entry.id.clone(), cron_entry);

                // Update next_id counter to avoid ID collisions
                if let Some(num) = entry.id.strip_prefix("cron-")
                    && let Ok(n) = num.parse::<u64>()
                    && n >= self.next_id
                {
                    self.next_id = n + 1;
                }
            } else {
                tracing::warn!(
                    "Skipping cron entry with invalid schedule: id={} schedule={}",
                    entry.id,
                    entry.schedule
                );
            }
        }
        if !self.entries.is_empty() {
            tracing::info!("Loaded {} cron entries from store", self.entries.len());
        }
        Ok(())
    }
}

// ── Cron expression parser ──────────────────────────────────────────────────

/// Parse a 5-field cron expression into CronFields
fn parse_cron(expr: &str) -> Result<CronFields, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "Cron expression must have 5 fields (min hour day month weekday), got {}: '{}'",
            fields.len(),
            expr
        ));
    }

    Ok(CronFields {
        minutes: parse_field(fields[0], 0, 59, "minute")?,
        hours: parse_field(fields[1], 0, 23, "hour")?,
        days: parse_field(fields[2], 1, 31, "day")?,
        months: parse_field(fields[3], 1, 12, "month")?,
        weekdays: parse_field(fields[4], 0, 6, "weekday")?,
    })
}

/// Parse a single cron field (supports *, ranges, steps, and lists)
fn parse_field(field: &str, min: u8, max: u8, name: &str) -> Result<Vec<u8>, String> {
    let mut values = Vec::new();

    for part in field.split(',') {
        if part.contains('/') {
            // Step syntax: start/step or */step
            let parts: Vec<&str> = part.split('/').collect();
            if parts.len() != 2 {
                return Err(format!("Invalid step syntax in {} field: '{}'", name, part));
            }
            let step: u8 = parts[1]
                .parse()
                .map_err(|_| format!("Invalid step value in {} field: '{}'", name, parts[1]))?;
            if step == 0 {
                return Err(format!("Step value must be > 0 in {} field", name));
            }

            let start = if parts[0] == "*" {
                min
            } else {
                parts[0]
                    .parse()
                    .map_err(|_| format!("Invalid start value in {} field: '{}'", name, parts[0]))?
            };

            for v in (start..=max).step_by(step as usize) {
                if v >= min && v <= max && !values.contains(&v) {
                    values.push(v);
                }
            }
        } else if part.contains('-') {
            // Range syntax: start-end
            let parts: Vec<&str> = part.split('-').collect();
            if parts.len() != 2 {
                return Err(format!(
                    "Invalid range syntax in {} field: '{}'",
                    name, part
                ));
            }
            let start: u8 = parts[0]
                .parse()
                .map_err(|_| format!("Invalid range start in {} field: '{}'", name, parts[0]))?;
            let end: u8 = parts[1]
                .parse()
                .map_err(|_| format!("Invalid range end in {} field: '{}'", name, parts[1]))?;
            for v in start..=end {
                if v >= min && v <= max && !values.contains(&v) {
                    values.push(v);
                }
            }
        } else if part == "*" {
            // Wildcard: all values
            for v in min..=max {
                values.push(v);
            }
        } else {
            // Single value
            let v: u8 = part
                .parse()
                .map_err(|_| format!("Invalid value in {} field: '{}'", name, part))?;
            if v < min || v > max {
                return Err(format!(
                    "Value {} out of range [{},{}] in {} field",
                    v, min, max, name
                ));
            }
            values.push(v);
        }
    }

    values.sort();
    Ok(values)
}

/// Register the cron triggers declared in an agent's manifest (S3.3).
///
/// ADR-055 Phase 2b.3: extracted from `package_manager/install.rs` (the
/// node no longer registers cron — cron is a Gateway global-resource
/// concern, §6.5). Called once the install-completed inventory arrives.
///
/// ADR-073: `instance_id` is the INSTANCE identity, not the package id.
/// The scheduler loop resolves a firing trigger with
/// [`GatewayState::resolve_installed_key`](crate::gateway::state::GatewayState::resolve_installed_key),
/// which only knows instance identities — a package-id trigger key would
/// register successfully and then never fire.
pub fn register_agent_cron_triggers(
    state: &mut crate::gateway::state::GatewayState,
    instance_id: &str,
    manifest: &acowork_core::AgentManifest,
) {
    let cron_triggers = manifest.cron_triggers();
    for trigger in cron_triggers {
        let Some(schedule) = &trigger.schedule else {
            continue;
        };
        let action = trigger.action.as_deref().unwrap_or("cron_trigger");
        let params = trigger.params.clone().unwrap_or(serde_json::json!({}));
        match state
            .cron_scheduler
            .register(instance_id, schedule, action, params.clone())
        {
            Ok(cron_id) => {
                tracing::info!(
                    "Registered cron trigger: instance={} cron_id={} schedule={}",
                    instance_id,
                    cron_id,
                    schedule
                );
                if let Some(store) = &state.cron_store {
                    let entry = StoredCronEntry {
                        id: cron_id.clone(),
                        agent_id: instance_id.to_string(),
                        schedule: schedule.clone(),
                        action: action.to_string(),
                        params: serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()),
                        timezone: None,
                        retry_count: 0,
                        retry_interval_secs: 60,
                        max_runs: None,
                        run_count: 0,
                        expires_at: None,
                    };
                    if let Err(e) = store.insert(&entry) {
                        tracing::warn!("Failed to persist cron entry {}: {}", cron_id, e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Invalid cron schedule in manifest for instance {}: schedule={} error={}",
                    instance_id,
                    schedule,
                    e
                );
            }
        }
    }
}

/// Run the cron scheduler loop as a background task.
///
/// Checks every minute for entries that should fire and pushes an
/// IntentReceived to the target Agent.
///
/// D2 (tech-debt plan): cron no longer hand-rolls "start then publish".
/// It reuses the single Gateway delivery entry
/// ([`crate::intent::delivery::wake_and_deliver`]), so the online fact
/// is the retained `AgentStatus` (ADR-085) and the cold-start race
/// (spawn reply OK but Runtime not yet subscribed) is closed for
/// scheduled triggers exactly as it is for cross-agent Intents.
pub async fn run_cron_scheduler(
    scheduler: Arc<Mutex<CronScheduler>>,
    mqtt_client: Option<Arc<GatewayMqttClient>>,
    gateway_state: crate::handlers::server::SharedState,
    node_control: Option<crate::mqtt::node_control::NodeControlClient>,
    agent_registry: crate::mqtt::agent_registry::SharedAgentRegistry,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    // Skip the first immediate tick
    interval.tick().await;

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();

        let triggers: Vec<(String, String, serde_json::Value)> = {
            let sched = scheduler.lock().await;
            sched
                .check(&now)
                .into_iter()
                .map(|(agent_id, action, params)| {
                    (agent_id.to_string(), action.to_string(), params.clone())
                })
                .collect()
        };

        for (trigger_id, action, params) in triggers {
            tracing::info!("Cron fired: trigger={} action={}", trigger_id, action);
            let Some(mqtt) = &mqtt_client else {
                tracing::warn!(
                    "Cron trigger skipped: MQTT client not available for trigger={} action={}",
                    trigger_id,
                    action
                );
                continue;
            };

            // ADR-073: the trigger key is the INSTANCE identity; the
            // delivery entry resolves + validates it against the install
            // table and fails loudly (observable error) otherwise.
            let req = crate::intent::delivery::DeliveryRequest {
                message_id: format!("cron-{}-{}", trigger_id, now.timestamp_millis()),
                conversation_id: String::new(),
                from_instance: format!("cron:{}", trigger_id),
                from_session_id: String::new(),
                target_instance: trigger_id.clone(),
                action: action.clone(),
                params_json: serde_json::to_string(&params).unwrap_or_default(),
                source_chat_session_id: String::new(),
                repo: String::new(),
                kind: String::new(),
                in_reply_to: String::new(),
                status: String::new(),
            };

            let mailbox = {
                let gw = gateway_state.read().await;
                gw.intent_mailbox.clone()
            };
            let ctx = crate::intent::delivery::DeliveryContext {
                state: &gateway_state,
                registry: &agent_registry,
                node_control: node_control.as_ref(),
                mqtt: Some(mqtt.as_ref()),
                mailbox: mailbox.as_deref(),
            };
            if let Err(e) = crate::intent::delivery::wake_and_deliver(
                &ctx,
                &req,
                acowork_core::timeout_config::constants::INTENT_DEFAULT,
            )
            .await
            {
                tracing::warn!(
                    "Cron trigger delivery failed: instance={} trigger={} action={} error={}",
                    trigger_id,
                    trigger_id,
                    action,
                    e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wildcard() {
        let fields = parse_cron("* * * * *").unwrap();
        assert_eq!(fields.minutes.len(), 60);
        assert_eq!(fields.hours.len(), 24);
        assert_eq!(fields.days.len(), 31);
        assert_eq!(fields.months.len(), 12);
        assert_eq!(fields.weekdays.len(), 7);
    }

    #[test]
    fn test_parse_specific_values() {
        let fields = parse_cron("0 9 * * 1-5").unwrap();
        assert_eq!(fields.minutes, vec![0]);
        assert_eq!(fields.hours, vec![9]);
        assert_eq!(fields.weekdays, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_parse_step() {
        let fields = parse_cron("*/15 * * * *").unwrap();
        assert_eq!(fields.minutes, vec![0, 15, 30, 45]);
    }

    #[test]
    fn test_parse_list() {
        let fields = parse_cron("0,30 9,17 * * *").unwrap();
        assert_eq!(fields.minutes, vec![0, 30]);
        assert_eq!(fields.hours, vec![9, 17]);
    }

    #[test]
    fn test_parse_invalid_field_count() {
        let result = parse_cron("* * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("5 fields"));
    }

    #[test]
    fn test_parse_out_of_range() {
        let result = parse_cron("60 * * * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn test_scheduler_register() {
        let mut scheduler = CronScheduler::new();
        let id = scheduler
            .register(
                "com.example.weather",
                "0 * * * *",
                "hourly_check",
                serde_json::json!({}),
            )
            .unwrap();
        assert!(id.starts_with("cron-"));
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn test_scheduler_unregister() {
        let mut scheduler = CronScheduler::new();
        let id = scheduler
            .register(
                "com.example.weather",
                "0 * * * *",
                "hourly_check",
                serde_json::json!({}),
            )
            .unwrap();
        assert!(scheduler.unregister(&id));
        assert!(scheduler.is_empty());
    }

    #[test]
    fn test_scheduler_unregister_agent() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register(
                "com.example.weather",
                "0 * * * *",
                "hourly_check",
                serde_json::json!({}),
            )
            .unwrap();
        scheduler
            .register(
                "com.example.weather",
                "0 9 * * *",
                "morning_check",
                serde_json::json!({}),
            )
            .unwrap();
        scheduler
            .register(
                "com.example.calendar",
                "0 0 * * *",
                "daily_check",
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(scheduler.len(), 3);

        let count = scheduler.unregister_agent("com.example.weather");
        assert_eq!(count, 2);
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn test_scheduler_check() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register(
                "com.example.weather",
                "30 9 * * *",
                "morning_report",
                serde_json::json!({"type": "daily"}),
            )
            .unwrap();

        // 9:30 AM on any day should match
        let time = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches = scheduler.check(&time);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "com.example.weather");
        assert_eq!(matches[0].1, "morning_report");

        // 9:31 AM should NOT match
        let time2 = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:31:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches2 = scheduler.check(&time2);
        assert!(matches2.is_empty());
    }

    #[test]
    fn test_scheduler_check_step() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register(
                "com.example.monitor",
                "*/15 * * * *",
                "health_check",
                serde_json::json!({}),
            )
            .unwrap();

        // Should match at minute 0, 15, 30, 45
        for minute in [0, 15, 30, 45] {
            let time =
                chrono::DateTime::parse_from_rfc3339(&format!("2026-04-24T09:{:02}:00Z", minute))
                    .unwrap()
                    .with_timezone(&chrono::Utc);
            let matches = scheduler.check(&time);
            assert_eq!(matches.len(), 1, "Should match at minute {}", minute);
        }

        // Should NOT match at minute 7
        let time = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:07:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches = scheduler.check(&time);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_entries_for_agent() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register(
                "com.example.weather",
                "0 * * * *",
                "hourly",
                serde_json::json!({}),
            )
            .unwrap();
        scheduler
            .register(
                "com.example.weather",
                "0 9 * * *",
                "morning",
                serde_json::json!({}),
            )
            .unwrap();
        scheduler
            .register(
                "com.example.calendar",
                "0 0 * * *",
                "daily",
                serde_json::json!({}),
            )
            .unwrap();

        let weather_entries = scheduler.entries_for_agent("com.example.weather");
        assert_eq!(weather_entries.len(), 2);

        let calendar_entries = scheduler.entries_for_agent("com.example.calendar");
        assert_eq!(calendar_entries.len(), 1);
    }

    /// ADR-073: manifest-declared triggers are registered under the
    /// INSTANCE identity, not the package id.
    ///
    /// The scheduler loop resolves a firing trigger with
    /// `GatewayState::resolve_installed_key`, which only knows instance
    /// identities — a package-id key registers successfully and then
    /// never fires (silently, as an error log). Pin the key here so the
    /// registration site cannot drift back to the package id.
    #[test]
    fn manifest_triggers_are_keyed_by_instance_identity() {
        const INSTANCE: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
        const TOML: &str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"

            [[triggers]]
            type = "cron"
            schedule = "0 * * * *"
            action = "refresh"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(TOML).unwrap();

        let mut state = crate::gateway::state::GatewayState::new("/tmp/acowork-cron-key-test");
        state.add_installed(crate::gateway::state::AgentInfo {
            instance_id: INSTANCE.to_string(),
            agent_id: manifest.agent_id.clone(),
            version: "1.0.0".to_string(),
            name: "Weather Agent".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest: manifest.clone(),
            node_id: "local".to_string(),
        });

        register_agent_cron_triggers(&mut state, INSTANCE, &manifest);

        assert_eq!(
            state.cron_scheduler.entries_for_agent(INSTANCE).len(),
            1,
            "the trigger must be keyed by the instance identity"
        );
        assert!(
            state
                .cron_scheduler
                .entries_for_agent("com.example.weather")
                .is_empty(),
            "a package-id key would never resolve on the fire path"
        );
        assert_eq!(
            state.resolve_installed_key(INSTANCE).as_deref(),
            Some(INSTANCE)
        );
        assert!(state.resolve_installed_key("com.example.weather").is_none());
    }
}
