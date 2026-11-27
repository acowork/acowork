//! Disk-watermark watcher — ADR-123 D5/D7.
//!
//! The Gateway samples the **volume** that `data_dir` lives on (free/total
//! bytes, i.e. a `statfs`-class query) and, when the used share crosses the
//! configured threshold, publishes a retained state message on
//! `acowork/desktop/storage`. The Desktop pops a dialog; the user clicks it and
//! lands on the cache-management card. That is the entire action — no write
//! refusal, no automatic cleanup (ADR-123 §1.4), and no per-store quota
//! (ADR-122 Q2).
//!
//! ## Why this does not violate the fs red line
//!
//! AGENTS.md / ADR-009 forbid the Gateway from reading a **service's** data
//! directory. Asking the filesystem "how much free space does this volume
//! have" reads no service directory at all — it is a machine-level fact about
//! the volume the Gateway itself is installed on. ADR-123 §1.3 red line 1 names
//! this as the one other exception besides the Gateway's own directory.
//!
//! ## Why retained
//!
//! The payload is a *state*, not an event: a Desktop that starts while the disk
//! is already 95 % full must learn that on subscribe, without waiting up to a
//! tick. The broker keeps the last value, so the retained publish is the whole
//! catch-up mechanism.
//!
//! ## Debounce (ADR-123 §6 Q2)
//!
//! - **Crossing** (alarm flips either way) → publish immediately.
//! - **Staying above** → republish at most once per `alarm_cooldown_secs`, so a
//!   user who dismissed the dialog is re-nudged hourly instead of never, and a
//!   disk hovering on the threshold cannot storm them.
//! - **Staying below** → publish nothing; the retained "all clear" is already
//!   the broker's last word.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::task::JoinHandle;

use acowork_core::storage::DiskWatermark;

use crate::config::CacheConfig;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// Retained watermark state topic. Lives under `acowork/desktop/` for the same
/// reason the inventory signal does: every Runtime decodes `acowork/global/*`
/// as a protobuf `DataEnvelope`, and a JSON payload there would log a decode
/// failure in each of them (see `mqtt::inventory_notifier`).
pub const TOPIC_STORAGE_WATERMARK: &str = "acowork/desktop/storage";

/// The wire payload: the watermark plus what the Desktop needs to treat each
/// alarmed episode as one dialog rather than one per republish.
#[derive(Debug, Clone, Serialize)]
pub struct StorageWatermarkEvent {
    #[serde(flatten)]
    pub watermark: DiskWatermark,
    /// Unix seconds when the alarm turned on; `None` while it is off. The
    /// Desktop remembers the value it dismissed, so the hourly republish of a
    /// still-full disk does not re-open a dialog the user already closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm_since: Option<i64>,
    /// Unix seconds of this sample.
    pub checked_at: i64,
}

/// Sample the volume that holds `probe` (a path that already exists).
///
/// `total == 0` (unreadable volume) yields a non-alarming watermark — see
/// [`DiskWatermark::new`]; the next tick retries.
pub fn sample_watermark(probe: &Path, threshold_percent: f64) -> DiskWatermark {
    let total = fs2::total_space(probe).unwrap_or(0);
    let available = fs2::available_space(probe).unwrap_or(0);
    DiskWatermark::new(total, available, threshold_percent)
}

/// Start the periodic watcher.
///
/// `client == None` (MQTT not up in this mode) leaves the watcher unspawned:
/// there is no other channel to alarm on, and the report endpoint still answers
/// the on-demand sample.
pub fn spawn_watcher(
    data_dir: std::path::PathBuf,
    cfg: CacheConfig,
    client: Option<GatewayMqttClient>,
) -> Option<JoinHandle<()>> {
    let client = client?;
    let interval = Duration::from_secs(cfg.disk_check_interval_secs.max(5));
    let cooldown = Duration::from_secs(cfg.alarm_cooldown_secs);
    let threshold = cfg.disk_alarm_percent;

    Some(tokio::spawn(async move {
        let mut alarming = false;
        // `None` = "never published"; the first sample always publishes so the
        // broker's retained value is never a stale one from a previous run.
        let mut last_publish: Option<SystemTime> = None;
        let mut alarm_since: Option<i64> = None;

        loop {
            let watermark = sample_watermark(&data_dir, threshold);
            let now_s = unix_now();

            let due = if watermark.alarm != alarming {
                true
            } else if watermark.alarm {
                last_publish
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .map(|elapsed| elapsed >= cooldown)
                    .unwrap_or(true)
            } else {
                // Below the threshold and it was already below: the retained
                // all-clear stands.
                last_publish.is_none()
            };

            if due {
                if watermark.alarm && !alarming {
                    alarm_since = Some(now_s);
                }
                if !watermark.alarm {
                    alarm_since = None;
                }
                let event = StorageWatermarkEvent {
                    watermark,
                    alarm_since,
                    checked_at: now_s,
                };
                match serde_json::to_vec(&event) {
                    Ok(payload) => {
                        match client
                            .publish_raw(TOPIC_STORAGE_WATERMARK, payload, MqttQoS::AtLeastOnce, true)
                            .await
                        {
                            Ok(()) => {
                                last_publish = Some(SystemTime::now());
                                if watermark.alarm != alarming {
                                    tracing::warn!(
                                        used_percent = watermark.used_percent,
                                        available_bytes = watermark.available_bytes,
                                        threshold = threshold,
                                        "disk watermark {}",
                                        if watermark.alarm { "tripped" } else { "cleared" }
                                    );
                                }
                                alarming = watermark.alarm;
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                "disk watermark publish failed; retrying next tick"
                            ),
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "disk watermark payload failed to encode"),
                }
            }

            tokio::time::sleep(interval).await;
        }
    }))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample answers from a real path without touching any service tree,
    /// and a real volume always reports a non-zero total.
    #[test]
    fn sampling_an_existing_path_reports_a_volume() {
        let probe = std::env::temp_dir();
        let w = sample_watermark(&probe, 90.0);
        assert!(w.total_bytes > 0);
        assert!(w.available_bytes <= w.total_bytes);
        assert!((0.0..=100.0).contains(&w.used_percent));
    }
}