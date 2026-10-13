//! Outbound MQTT wake-up signals (QoS 1, fire-and-forget).
//!
//! Two topics, one shape: this process owns data that other processes render,
//! so after a write lands it publishes a *nudge* and the reader re-pulls over
//! HTTP. Never the other way round — a payload copied from the store would be
//! a second truth to keep in sync.
//!
//! 1. `acowork/user/profiles/changed` ([`notify_profiles_changed`], ADR-084
//!    §决策 4b): payload = the new profile-list version. The Gateway listens,
//!    re-pulls `GET /internal/user-profiles` and republishes its global
//!    resources — the `last_user_profile` Runtime sees (ADR-042).
//! 2. `acowork/users/{uid}/chat/changed` ([`notify_chat_changed`], design doc
//!    25 §8): **empty payload**. Every device holding that account's inbox
//!    re-reads `GET /api/users/{uid}/chats`. Content-free because the embedded
//!    broker has no subscription ACL, so a payload naming the peer would
//!    publish who talks to whom.
//!
//! Delivery contract (same shape as `acowork-doc`'s tree-change publisher):
//! - QoS 1, **non-retained**. A lost signal is healed by the next mutation
//!   and by the reader's own startup / foreground pull, so neither can wedge.
//! - Publisher is a **global singleton** so the store code can emit without
//!   threading a handle through `AppState`. Unit tests never call [`init`],
//!   so the publish path is a no-op there; the signals it would have sent are
//!   recorded instead (see [`test_signals`]) so handlers can assert on them.
//! - Publish failure is fire-and-forget: warn and drop. A business write
//!   (create user, rename, change avatar, send a chat message) must never
//!   fail because the broker is down.
//!
//! Reuse: [`MqttClient`] from `acowork-mqtt-session` (ADR-065) gives us the
//! whole poll loop — reconnect, backoff, wake recovery — for free; this
//! process only ever publishes, never subscribes.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use rumqttc::QoS;
use tracing::warn;

use acowork_core::mqtt_proto::{user_chat_changed_topic, USER_PROFILES_CHANGED_TOPIC};
use acowork_mqtt_session::{MqttClient, MqttClientConfig, MqttClientError, MqttClientHandler};

/// Broker client id. The shared prefix `user:service` is matched by
/// the Gateway broker allowlist (ADR-084 §决策 4b, `starts_with`).
/// The per-process suffix avoids `Duplicate client_id, dropping previous`
/// on internal reconnect — rumqttd drops the previous session and the
/// publisher's reconnect loop then races itself; a unique suffix makes
/// each reconnect a fresh session and the broker stops issuing
/// `Duplicate`. Stable across restarts of the same process is not
/// required (the broker auth path treats all `user:service:*` ids
/// identically and the rest of the runtime never subscribes to this
/// publisher).
fn client_id() -> String {
    format!("user:service:{}", std::process::id())
}

static PUBLISHER: OnceLock<Arc<UserMqttPublisher>> = OnceLock::new();

/// Initialise the global publisher. Called once from `main`; a broker that
/// is not up yet is fine — [`MqttClient::connect`] only spawns the poll task
/// (auto-reconnect inside), it does not wait for CONNACK.
///
/// `password` is the Gateway publisher token when `mqtt.auth_enabled` is on
/// (ADR-084 §决策 4b); `None` connects without credentials.
///
/// Never returns an error that should kill the user service: without the
/// signal the Gateway still picks the new profiles up on its next restart.
pub async fn init(host: &str, port: u16, password: Option<String>) {
    match UserMqttPublisher::connect(host.to_string(), port, password).await {
        Ok(publisher) => {
            let _ = PUBLISHER.set(Arc::new(publisher));
            tracing::info!(host, port, "user MQTT publisher ready");
        }
        Err(e) => {
            warn!(host, port, error = %e,
                "user MQTT publisher init failed — profile-change signals disabled \
                 (Gateway picks changes up on its next pull)");
        }
    }
}

/// Fire-and-forget profile-change notification. No-op when the publisher was
/// never initialised (unit tests) or the broker is unreachable.
pub fn notify_profiles_changed(version: u64) {
    // Service-layer writers are sync (`rebuild_and_save_user_profile_cache`),
    // and a sync unit test has no reactor — where the publisher is
    // uninitialised anyway, so there is nothing to publish.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(publish_profiles_changed(version));
}

async fn publish_profiles_changed(version: u64) {
    let Some(publisher) = PUBLISHER.get() else {
        return;
    };
    publisher.publish_profiles_changed(version).await;
}

/// Fire-and-forget chat wake-up for ONE account's inbox.
///
/// `send_message` calls this twice: the recipient's devices have an inbox to
/// refresh, and the sender's *other* devices do too (the device that posted
/// the message already holds it — that is why the response is the stored
/// record rather than a refetch). `mark_read` calls it once so a badge clears
/// everywhere at once, on every device.
///
/// Takes and records only the account id: the topic is the whole message, so
/// there is nothing here that could leak which thread changed.
pub fn notify_chat_changed(user_id: &str) {
    #[cfg(test)]
    record_signal(user_id);
    // Same guard as `notify_profiles_changed`: without a reactor there is no
    // task to spawn, and in a unit test there is no publisher either.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let user_id = user_id.to_string();
    tokio::spawn(async move { publish_chat_changed(&user_id).await });
}

async fn publish_chat_changed(user_id: &str) {
    let Some(publisher) = PUBLISHER.get() else {
        return;
    };
    publisher.publish_chat_changed(user_id).await;
}

/// Topics [`notify_chat_changed`] would have published, newest last. The
/// publisher is a process-wide singleton that unit tests never initialise, so
/// without this the only way to observe "a send wakes both mailboxes" would be
/// a live broker in the test.
///
/// Tests take a length snapshot before the request and assert on the window
/// after it: the sink is process-wide and cargo runs tests on several threads,
/// so a same-thread call is the only ordering available.
#[cfg(test)]
pub fn test_signals() -> Vec<String> {
    SIGNALS.lock().expect("signal test sink").clone()
}

#[cfg(test)]
fn record_signal(user_id: &str) {
    let mut sink = SIGNALS.lock().expect("signal test sink");
    sink.push(user_chat_changed_topic(user_id));
}

#[cfg(test)]
static SIGNALS: std::sync::Mutex<Vec<String>> = const { std::sync::Mutex::new(Vec::new()) };

/// Publisher: a `MqttClient` with a no-op handler (publish-only).
struct UserMqttPublisher {
    client: MqttClient<NoopHandler>,
}

impl UserMqttPublisher {
    async fn connect(
        host: String,
        port: u16,
        password: Option<String>,
    ) -> Result<Self, MqttClientError> {
        let mut config = MqttClientConfig::new(client_id(), host, port);
        // With `mqtt.auth_enabled` on, the broker only admits
        // `user:service:*` (any per-process id with this prefix) with
        // the Gateway's publisher token; the supervisor forwards it
        // at spawn time (ADR-084 §决策 4b).
        config.credentials = password.map(|p| (client_id(), p));
        let client = MqttClient::connect(config, NoopHandler, None).await?;
        Ok(Self { client })
    }

    /// Publish the profile-list version that a mutation just produced.
    ///
    /// The version is the whole payload: the Gateway logs it and only pulls
    /// when it differs from what it caches, so a duplicate signal costs
    /// nothing.
    async fn publish_profiles_changed(&self, version: u64) {
        if let Err(e) = self
            .client
            .publish_raw(
                USER_PROFILES_CHANGED_TOPIC,
                version_payload(version),
                QoS::AtLeastOnce,
                false,
            )
            .await
        {
            warn!(topic = USER_PROFILES_CHANGED_TOPIC, version, error = %e,
                "user: failed to publish profile-changed signal");
        }
    }

    /// Publish one chat wake-up for a single account.
    ///
    /// The payload is empty **by design** (see the topic's contract in
    /// `acowork_core::mqtt_proto::user_chat_changed_topic`): the embedded
    /// broker has no subscription ACL, so anything naming the peer or the
    /// thread would be readable by every other account. The subscriber's own
    /// HTTP read is what fetches the truth.
    async fn publish_chat_changed(&self, user_id: &str) {
        let topic = user_chat_changed_topic(user_id);
        if let Err(e) = self
            .client
            .publish_raw(&topic, Vec::new(), QoS::AtLeastOnce, false)
            .await
        {
            warn!(topic, error = %e, "user: failed to publish chat-changed signal");
        }
    }
}

/// Publish-only handler: no subscriptions, nothing to do on ConnAck /
/// disconnect / error beyond the defaults.
#[derive(Default)]
struct NoopHandler;

#[async_trait]
impl MqttClientHandler for NoopHandler {}

/// Plain ASCII decimal — see the topic docs: the payload is a hint the
/// Gateway string-compares, not a parsed envelope.
fn version_payload(version: u64) -> Vec<u8> {
    version.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The topic is a wire contract with the Gateway's subscription list; a
    /// rename on one side only would silently stop profile refresh.
    #[test]
    fn topic_matches_the_shared_contract() {
        assert_eq!(
            USER_PROFILES_CHANGED_TOPIC,
            "acowork/user/profiles/changed"
        );
    }

    /// The version is serialised as plain ASCII digits — the Gateway parses
    /// the payload on its side and a JSON wrapper would break it.
    #[test]
    fn version_payload_is_plain_ascii() {
        assert_eq!(version_payload(42), b"42");
        assert_eq!(version_payload(0), b"0");
    }

    /// The chat topic is a wire contract with the two clients that subscribe
    /// it (`apps/acowork-mobile/src/lib/presence.ts` and the Desktop MQTT
    /// client). A one-sided rename would stop chat push silently.
    #[test]
    fn chat_topic_is_keyed_by_account_alone() {
        assert_eq!(
            user_chat_changed_topic("22063a06-0d2f-4f0e-9b1a-7d5f1c2e3a4b"),
            "acowork/users/22063a06-0d2f-4f0e-9b1a-7d5f1c2e3a4b/chat/changed"
        );
    }

    /// What a send emits is a topic and nothing else: no thread id, no body.
    /// `chat_id` is the two account ids joined by `__`, so its appearance in
    /// a signal would publish the contact graph to every account that can
    /// subscribe `acowork/users/#` (the broker has no subscription ACL).
    #[test]
    fn notify_emits_an_account_topic_and_no_thread_identity() {
        notify_chat_changed("alice");
        let signals = test_signals();
        assert!(
            signals
                .iter()
                .any(|t| t == "acowork/users/alice/chat/changed"),
            "the wake-up must name the account it belongs to"
        );
        assert!(
            signals.iter().all(|t| !t.contains("__")),
            "a `chat_id` in a signal topic would leak which pair changed"
        );
    }
}
