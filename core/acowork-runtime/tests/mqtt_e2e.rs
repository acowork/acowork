//! MQTT protocol E2E tests (ADR-033).
//!
//! Tests each protocol layer independently:
//! 1. Broker config building
//! 2. ControlCommand protobuf encoding/decoding
//! 3. control_handler::parse_control_payload
//! 4. GatewayMqttClient control publish (requires broker)
//!
//! Note: rumqttd 0.14's Broker::start() panics inside tokio runtime.
//! Broker startup tests run in a separate OS thread via start_broker.

use acowork_core::mqtt_proto::{self, DataEnvelope, data_envelope::Payload};
use acowork_gateway::mqtt::broker::build_broker_config;
use acowork_runtime::mqtt::control_handler;
use prost::Message;

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Broker config building (pure function, no runtime needed)
// ═══════════════════════════════════════════════════════════════════════

// NOTE (Sept 2026): the only MQTT control-plane command now is
// `Intent` — `ActiveHeartbeat` was retired alongside auto-sleep.
// Earlier tests that exercised `ControlCommand::ActiveHeartbeat` and
// the matching `ControlAction::ActiveHeartbeat` arm have been removed.

#[test]
fn test_build_broker_config() {
    let config = build_broker_config("127.0.0.1", 19875, None);
    let v4 = config.v4.as_ref().expect("v4 servers must be configured");
    let server = v4.get("acowork").expect("server 'acowork' must exist");
    assert_eq!(server.listen.to_string(), "127.0.0.1:19875");
    assert_eq!(config.router.max_connections, 100);
}

#[test]
fn test_build_broker_config_custom_port() {
    let config = build_broker_config("0.0.0.0", 32100, None);
    let v4 = config.v4.as_ref().expect("v4 servers must be configured");
    let server = v4.get("acowork").unwrap();
    assert_eq!(server.listen.to_string(), "0.0.0.0:32100");
}

// ═══════════════════════════════════════════════════════════════════════
// control_handler parsing
// ═══════════════════════════════════════════════════════════════════════

/// Garbage bytes must not panic and must not produce an action. This is the
/// only remaining parser-level assertion that is not about a specific
/// command: with the control plane reduced to just `Intent`
/// (auto-sleep removed), there is nothing else to parse.
#[test]
fn test_parse_invalid_payload() {
    let action = control_handler::parse_control_payload("test", b"not valid protobuf");
    assert!(action.is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: AvailableProviders serialization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_available_providers_roundtrip() {
    let providers = mqtt_proto::AvailableProviders {
        version: 42,
        // ADR-056: no global default compact model in this fixture.
        #[allow(deprecated)]
        default_compact_model: None,
        default_compact_models: vec![],
        providers: vec![mqtt_proto::ProviderRef {
            id: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol_type: mqtt_proto::LlmProtocol::Openai.into(),
            compact_model: String::new(),
            custom: false,
            models: vec![],
            api_key: String::new(),
            account_id: String::new(),
        }],
    };

    let env = DataEnvelope {
        version: 1,
        payload: Some(Payload::AvailableProviders(providers)),
    };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();

    match decoded.payload {
        Some(Payload::AvailableProviders(p)) => {
            assert_eq!(p.version, 42);
            assert_eq!(p.providers.len(), 1);
            assert_eq!(p.providers[0].id, "openai");
        }
        _ => panic!("Expected AvailableProviders"),
    }
}

// ═════════════════════════════════════════════════════════════════════════
// ADR-038: Session lifecycle explicit model — open acks
//
// The `open_session` *command* is HTTP-only now (ADR-076 §决策 4); only the
// `SessionOpened` / `SessionNotOpened` events it triggers still live on MQTT.
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn test_session_opened_event_roundtrip() {
    let evt = mqtt_proto::SessionOpened {
        session_id: "sess-001".into(),
        status: "resumed_from_disk".into(),
        model: "gpt-4o".into(),
        provider: "openai".into(),
        last_active_at: "2026-07-17T12:34:56Z".into(),
    };
    let env = DataEnvelope {
        version: 1,
        payload: Some(Payload::SessionOpened(evt)),
    };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::SessionOpened(s)) => {
            assert_eq!(s.session_id, "sess-001");
            assert_eq!(s.status, "resumed_from_disk");
            assert_eq!(s.model, "gpt-4o");
            assert_eq!(s.provider, "openai");
            assert_eq!(s.last_active_at, "2026-07-17T12:34:56Z");
        }
        _ => panic!("Expected SessionOpened event"),
    }
}

#[test]
fn test_session_not_opened_event_roundtrip() {
    let evt = mqtt_proto::SessionNotOpened {
        session_id: "sess-closed-002".into(),
        attempted_command: "chat_message".into(),
        reason: "session_closed".into(),
    };
    let env = DataEnvelope {
        version: 1,
        payload: Some(Payload::SessionNotOpened(evt)),
    };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::SessionNotOpened(s)) => {
            assert_eq!(s.session_id, "sess-closed-002");
            assert_eq!(s.attempted_command, "chat_message");
            assert_eq!(s.reason, "session_closed");
        }
        _ => panic!("Expected SessionNotOpened event"),
    }
}

#[test]
fn test_session_lifecycle_state_machine_enum() {
    // Pure type-level smoke test: verifies SessionLifecycleState variants
    // exist and compare correctly. Runtime semantics are covered by
    // SessionManager integration tests.
    use acowork_runtime::agent::session::{SessionLifecycleState as S, SessionOpenOutcome as O};
    assert_eq!(S::NotFound, S::NotFound);
    assert_eq!(S::Closed, S::Closed);
    assert_eq!(S::Active, S::Active);
    assert_ne!(S::Active, S::Closed);
    assert_eq!(O::AlreadyActive, O::AlreadyActive);
    assert_eq!(O::ResumedFromDisk, O::ResumedFromDisk);
    assert_ne!(O::AlreadyActive, O::ResumedFromDisk);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-046: ChatMessage shape after image-pipeline merge
// ═══════════════════════════════════════════════════════════════════════
//
// `phase9_chat_message_rich_fields_via_params_json` (above) verifies that
// the wire-level `params_json` survives encode → decode → dispatch. That
// covers the bytes path. This test covers the *shape* path that the LLM
// actually consumes: when the desktop sends only `attached_items` (no
// inline `content_parts` — the ADR-046 default), the Runtime's
// `derive_image_parts` + `merge_content_parts` pipeline must produce a
// `ChatMessage` carrying `ContentPart::ImageUrl` entries with valid data
// URLs. If this test fails, the LLM would receive a plain text message
// and have no way to see the picture — exactly the user-reported bug.

#[tokio::test]
async fn adr046_image_pipeline_produces_multimodal_chat_message_shape() {
    use acowork_core::protocol::AttachedItem;
    use acowork_core::providers::traits::{ChatMessage as CoreChatMessage, ContentPart};
    use acowork_runtime::agent::attachment_to_image::{
        derive_image_parts, merge_content_parts, ImageDelivery,
    };
    use std::sync::Arc;

    // Stub AttachmentService: returns the fake bytes for `img-1` /
    // `img-2` and errors otherwise. Mirrors the shape used by
    // `attachment_to_image.rs` unit tests, lifted to a top-level
    // integration test so the public API surface stays pinned.
    struct FakeAttachment;
    #[async_trait::async_trait]
    impl acowork_runtime::usecases::AttachmentService for FakeAttachment {
        async fn upload_file(
            &self,
            _params: acowork_runtime::usecases::attachment::UploadFileParams,
        ) -> Result<
            acowork_runtime::usecases::attachment::UploadedFileResponse,
            acowork_runtime::usecases::attachment::AttachmentError,
        > {
            unimplemented!()
        }
        async fn read_file(
            &self,
            document_id: &str,
        ) -> Result<Vec<u8>, acowork_runtime::usecases::attachment::AttachmentError> {
            match document_id {
                "img-1" => Ok(b"\x89PNG\r\n\x1a\nfake-png".to_vec()),
                "img-2" => Ok(b"\xff\xd8\xff\xe0fake-jpg".to_vec()),
                other => Err(
                    acowork_runtime::usecases::attachment::AttachmentError::NotFound(
                        other.to_string(),
                    ),
                ),
            }
        }
    }
    let svc: Arc<dyn acowork_runtime::usecases::AttachmentService> = Arc::new(FakeAttachment);

    // Simulated desktop chat_send payload: user typed "see these:" and
    // attached two images (no inline content_parts — this is the
    // ADR-046 default that triggers the bug if the pipeline is missing).
    let user_text = "see these:";
    let attached_items = vec![
        AttachedItem::ImageUpload {
            document_id: "img-1".into(),
            filename: "a.png".into(),
            format: "png".into(),
            size_bytes: 9,
            width: Some(640),
            height: Some(480),
            client_id: None,
        },
        AttachedItem::ImageUpload {
            document_id: "img-2".into(),
            filename: "b.jpg".into(),
            format: "jpg".into(),
            size_bytes: 12,
            width: None,
            height: None,
            client_id: None,
        },
    ];
    let frontend_content_parts: Option<Vec<ContentPart>> = None;

    // Run the same derivation SessionTask does:
    let derived = derive_image_parts(Some(&svc), &attached_items)
        .await
        .expect("derive_image_parts succeeds");
    // Both images are small and on the whitelist, so both must be delivered —
    // the prompt hint is built from these verdicts, and a `Skipped` here would
    // have told the model the pixels never arrived.
    assert_eq!(
        derived.verdict("img-1"),
        Some(&ImageDelivery::Delivered)
    );
    assert_eq!(
        derived.verdict("img-2"),
        Some(&ImageDelivery::Delivered)
    );
    let merged = merge_content_parts(frontend_content_parts, derived.parts);

    // Build the exact ChatMessage shape the agent loop will pass to the
    // LLM. `user_multimodal` takes (text_for_logging, parts).
    let msg = CoreChatMessage::user_multimodal(user_text, merged.expect("merged must be Some"));

    // ── Assertions ──
    let parts = msg
        .content_parts
        .as_ref()
        .expect("user_multimodal must populate content_parts");
    assert_eq!(parts.len(), 2, "expected exactly 2 image parts");

    // Order must match attached_items order.
    match &parts[0] {
        ContentPart::ImageUrl { image_url } => {
            assert!(
                image_url.url.starts_with("data:image/png;base64,"),
                "first part must be PNG, got prefix {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
            assert_eq!(image_url.width, Some(640));
            assert_eq!(image_url.height, Some(480));
        }
        other => panic!("expected ImageUrl for img-1, got {other:?}"),
    }
    match &parts[1] {
        ContentPart::ImageUrl { image_url } => {
            assert!(
                image_url.url.starts_with("data:image/jpeg;base64,"),
                "second part must be JPEG, got prefix {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
            assert_eq!(image_url.width, None);
            assert_eq!(image_url.height, None);
        }
        other => panic!("expected ImageUrl for img-2, got {other:?}"),
    }

    // Regression guard against the `build_data_url` `;`-bug: the URL
    // MUST contain ";base64," (not "base64," without the semicolon).
    // This is the exact failure mode that made the LLM reject the data
    // URI as malformed even when the rest of the pipeline worked.
    for (i, part) in parts.iter().enumerate() {
        if let ContentPart::ImageUrl { image_url } = part {
            assert!(
                image_url.url.contains(";base64,"),
                "part[{i}] data URL must use ';base64,' (RFC 2397); got {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
        }
    }
}

// (ActiveHeartbeat tests removed along with the auto-sleep subsystem —
// see mqtt_payload.proto `ControlCommand` doc. Only `Intent` is left on
// the control channel.)
