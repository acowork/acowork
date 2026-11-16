//! Budget persistent storage
//!
//! S4.3.1: Budget persistence — daily/monthly cumulative usage
//! stored as JSON files on disk. Each provider has its own
//! accumulation record with daily and monthly totals.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Usage accumulation record for a single (agent, provider) pair
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageAccumulation {
    /// Agent ID
    pub agent_id: String,
    /// Provider name
    pub provider: String,
    /// Daily token usage (keyed by date string "YYYY-MM-DD")
    pub daily_tokens: HashMap<String, u64>,
    /// Daily cost usage in USD (keyed by date string)
    pub daily_cost_usd: HashMap<String, f64>,
    /// Monthly token usage (keyed by "YYYY-MM")
    pub monthly_tokens: HashMap<String, u64>,
    /// Monthly cost usage in USD (keyed by "YYYY-MM")
    pub monthly_cost_usd: HashMap<String, f64>,
}

impl UsageAccumulation {
    /// Create a new empty accumulation record
    pub fn new(agent_id: &str, provider: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            provider: provider.to_string(),
            daily_tokens: HashMap::new(),
            daily_cost_usd: HashMap::new(),
            monthly_tokens: HashMap::new(),
            monthly_cost_usd: HashMap::new(),
        }
    }

    /// Record usage for the given timestamp
    pub fn record(&mut self, tokens: u64, cost_usd: f64, now: chrono::DateTime<chrono::Utc>) {
        let day_key = now.format("%Y-%m-%d").to_string();
        let month_key = now.format("%Y-%m").to_string();

        *self.daily_tokens.entry(day_key.clone()).or_insert(0) += tokens;
        *self.daily_cost_usd.entry(day_key).or_insert(0.0) += cost_usd;
        *self.monthly_tokens.entry(month_key.clone()).or_insert(0) += tokens;
        *self.monthly_cost_usd.entry(month_key).or_insert(0.0) += cost_usd;
    }

    /// Get today's token usage
    pub fn today_tokens(&self) -> u64 {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.daily_tokens.get(&today).copied().unwrap_or(0)
    }

    /// Get today's cost in USD
    pub fn today_cost_usd(&self) -> f64 {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.daily_cost_usd.get(&today).copied().unwrap_or(0.0)
    }

    /// Get this month's token usage
    pub fn month_tokens(&self) -> u64 {
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        self.monthly_tokens.get(&month).copied().unwrap_or(0)
    }

    /// Get this month's cost in USD
    pub fn month_cost_usd(&self) -> f64 {
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        self.monthly_cost_usd.get(&month).copied().unwrap_or(0.0)
    }
}

/// Budget store — persists usage data to disk
///
/// S4.3.1: Budget persistence layer. Stores cumulative usage
/// per (agent, provider) pair as JSON files.
pub struct BudgetStore {
    /// Base directory for budget data files
    data_dir: PathBuf,
    /// In-memory cache of usage accumulation
    cache: HashMap<String, UsageAccumulation>,
}

impl BudgetStore {
    /// Create a new BudgetStore with the given data directory
    pub fn new(data_dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(data_dir);
        Self {
            data_dir: data_dir.to_path_buf(),
            cache: HashMap::new(),
        }
    }

    /// Create an in-memory BudgetStore (for testing)
    pub fn new_in_memory() -> Self {
        Self {
            data_dir: PathBuf::from(":memory:"),
            cache: HashMap::new(),
        }
    }

    /// Build the cache key for an (agent, provider) pair
    fn cache_key(agent_id: &str, provider: &str) -> String {
        format!("{}:{}", agent_id, provider)
    }

    /// Record usage for an agent+provider pair
    pub fn record_usage(&mut self, agent_id: &str, provider: &str, tokens: u64, cost_usd: f64) {
        let key = Self::cache_key(agent_id, provider);
        let now = chrono::Utc::now();

        let accumulation = self
            .cache
            .entry(key.clone())
            .or_insert_with(|| UsageAccumulation::new(agent_id, provider));

        accumulation.record(tokens, cost_usd, now);

        // Persist to disk (best-effort) — clone to avoid borrow conflict
        let accumulation_clone = accumulation.clone();
        self.save_to_disk(&key, &accumulation_clone);
    }

    /// Get usage accumulation for an agent+provider pair
    pub fn get_accumulation(&self, agent_id: &str, provider: &str) -> Option<&UsageAccumulation> {
        let key = Self::cache_key(agent_id, provider);
        self.cache.get(&key)
    }

    /// Get total tokens used for a provider across all agents (today)
    pub fn total_today_tokens(&self, provider: &str) -> u64 {
        self.cache
            .values()
            .filter(|a| a.provider == provider)
            .map(|a| a.today_tokens())
            .sum()
    }

    /// Get total cost for a provider across all agents (today)
    pub fn total_today_cost_usd(&self, provider: &str) -> f64 {
        self.cache
            .values()
            .filter(|a| a.provider == provider)
            .map(|a| a.today_cost_usd())
            .sum()
    }

    /// Get total tokens used for a provider across all agents (this month)
    pub fn total_month_tokens(&self, provider: &str) -> u64 {
        self.cache
            .values()
            .filter(|a| a.provider == provider)
            .map(|a| a.month_tokens())
            .sum()
    }

    /// Get total cost for a provider across all agents (this month)
    pub fn total_month_cost_usd(&self, provider: &str) -> f64 {
        self.cache
            .values()
            .filter(|a| a.provider == provider)
            .map(|a| a.month_cost_usd())
            .sum()
    }

    /// Save accumulation data to disk (best-effort)
    fn save_to_disk(&self, key: &str, accumulation: &UsageAccumulation) {
        if self.data_dir.to_string_lossy() == ":memory:" {
            return;
        }
        let file_path = self
            .data_dir
            .join(format!("{}.json", key.replace(':', "_")));
        if let Ok(json) = serde_json::to_string_pretty(accumulation) {
            let _ = std::fs::write(&file_path, json);
        }
    }

    /// Load accumulation data from disk
    pub fn load_from_disk(&mut self) {
        if self.data_dir.to_string_lossy() == ":memory:" {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.data_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false)
                    && let Ok(content) = std::fs::read_to_string(&path)
                    && let Ok(accumulation) = serde_json::from_str::<UsageAccumulation>(&content)
                {
                    let key = Self::cache_key(&accumulation.agent_id, &accumulation.provider);
                    self.cache.insert(key, accumulation);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_accumulation_new() {
        let acc = UsageAccumulation::new("com.test", "openai");
        assert_eq!(acc.agent_id, "com.test");
        assert_eq!(acc.provider, "openai");
        assert!(acc.daily_tokens.is_empty());
    }

    #[test]
    fn test_usage_accumulation_record() {
        let mut acc = UsageAccumulation::new("com.test", "openai");
        let now = chrono::Utc::now();
        acc.record(100, 0.5, now);
        acc.record(50, 0.25, now);

        assert_eq!(acc.today_tokens(), 150);
        assert!((acc.today_cost_usd() - 0.75).abs() < 0.001);
        assert_eq!(acc.month_tokens(), 150);
    }

    #[test]
    fn test_budget_store_in_memory() {
        let mut store = BudgetStore::new_in_memory();
        store.record_usage("com.test", "openai", 100, 0.5);
        store.record_usage("com.test", "openai", 50, 0.25);

        let acc = store.get_accumulation("com.test", "openai").unwrap();
        assert_eq!(acc.today_tokens(), 150);

        assert_eq!(store.total_today_tokens("openai"), 150);
        assert_eq!(store.total_today_tokens("anthropic"), 0);
    }

    #[test]
    fn test_budget_store_multiple_agents() {
        let mut store = BudgetStore::new_in_memory();
        store.record_usage("com.test.a", "openai", 100, 0.5);
        store.record_usage("com.test.b", "openai", 200, 1.0);

        assert_eq!(store.total_today_tokens("openai"), 300);
        assert!((store.total_today_cost_usd("openai") - 1.5).abs() < 0.001);
    }

    #[test]
    fn test_budget_store_persistence() {
        let dir = crate::test_support::unique_temp_dir("budget");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write
        {
            let mut store = BudgetStore::new(&dir);
            store.record_usage("com.test", "openai", 100, 0.5);
        }

        // Read back
        {
            let mut store = BudgetStore::new(&dir);
            store.load_from_disk();
            assert_eq!(store.total_today_tokens("openai"), 100);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
