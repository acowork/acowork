//! Messages-API parameter corrections, end to end at the real HTTP boundary.
//!
//! The 2026-11-03 report: a custom provider (`custom-loostone`) with
//! `claude-haiku-5-5` answered a plain Anthropic request with two 400s —
//! `` `temperature` is deprecated for this model `` and
//! `"thinking.type.enabled" is not supported for this model. Use
//! "thinking.type.adaptive"`. Nothing corrected them, so the model was
//! unusable over the Anthropic protocol while the OpenAI-compatible path
//! (which has a fallback ladder) worked.
//!
//! This proves the production path — `AnthropicProvider::chat →
//! send_with_compat → CompatCache` — against a scripted in-test server that
//! rejects a parameter exactly the way the real endpoint did:
//!
//! 1. The cold path adjusts one rejected parameter per retry and succeeds.
//! 2. A single success is only a candidate; three confirmations promote it.
//! 3. The promoted trim takes the single-request fast path.
//! 4. It is persisted to its own file (`provider_compat_anthropic.json`),
//!    so a restart does not pay the probing cost again.

#![cfg(test)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acowork_core::providers::traits::{ChatMessage, ChatRequest, Provider};
use acowork_core::providers::ReasoningEffort;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::anthropic_compat::{AnthropicCompatCache, cache_path, load, trim_key};

const MODEL: &str = "claude-haiku-5-5";
const PROVIDER_ID: &str = "custom-loostone";

const OK_BODY: &str =
    r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;
const TEMPERATURE_400: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"`temperature` is deprecated for this model."}}"#;
const THINKING_400: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"\"thinking.type.enabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior."}}"#;

/// A scripted Messages endpoint: it rejects whatever the request still
/// carries. Unlike a fixed script queue this stays correct no matter how many
/// requests a call needs, which is exactly what makes the retry count
/// observable.
struct MockMessages {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockMessages {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock messages endpoint");
        let addr = listener.local_addr().expect("local addr");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let bodies_t = bodies.clone();

        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let bodies_c = bodies_t.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match sock.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                head.push(byte[0]);
                                if head.ends_with(b"\r\n\r\n") || head.len() > 64 * 1024 {
                                    break;
                                }
                            }
                        }
                    }
                    let head_str = String::from_utf8_lossy(&head);
                    let mut content_len = 0usize;
                    for line in head_str.split("\r\n") {
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_len];
                    let mut read = 0usize;
                    while read < content_len {
                        match sock.read(&mut body[read..]).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => read += n,
                        }
                    }
                    let body = String::from_utf8_lossy(&body).into_owned();
                    bodies_c.lock().unwrap().push(body.clone());

                    // Reject in the order the real endpoint did.
                    let (status, reason, resp_body) = if body.contains("\"temperature\"") {
                        (400, "Bad Request", TEMPERATURE_400)
                    } else if body.contains("\"type\":\"enabled\"") {
                        (400, "Bad Request", THINKING_400)
                    } else {
                        (200, "OK", OK_BODY)
                    };
                    let out = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        Self {
            addr,
            bodies,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.handle.abort();
    }
}

/// A request with `temperature` and **no** explicit reasoning effort — the
/// shape that produced the first 400 (thinking off ⇒ `temperature` is sent).
fn chat_request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hello from e2e"),
        ],
        temperature: Some(0.7),
        max_tokens: Some(16_000),
        tools: None,
        reasoning_effort: None,
        thinking_mode: None,
    }
}

/// The same request with thinking on — the shape that produced the second 400
/// (extended `{ type: "enabled" }` is refused by the model).
fn chat_request_with_thinking() -> ChatRequest {
    ChatRequest {
        reasoning_effort: Some(ReasoningEffort::High),
        ..chat_request()
    }
}

#[tokio::test]
async fn rejected_parameters_are_corrected_confirmed_and_persisted() {
    let mock = MockMessages::start().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let cache: Arc<AnthropicCompatCache> = load(dir.path());

    let mut provider = AnthropicProvider::with_base_url(Some(&mock.base_url()), Some("k"));
    provider.set_provider_id(PROVIDER_ID.to_string());
    provider.set_compat_cache(cache.clone());

    let request = chat_request();
    let key = trim_key(PROVIDER_ID, MODEL);

    // ── Call 1: cold path, one correction per retry ────────────────────
    let resp = provider
        .chat(request.clone())
        .await
        .expect("the cold path must drop the rejected parameter and succeed");
    assert_eq!(resp.content, "ok");

    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "expected: original, then without temperature");
    assert!(
        bodies[0].contains("\"temperature\""),
        "first request goes out exactly as asked: {}",
        bodies[0]
    );
    assert!(
        !bodies[1].contains("\"temperature\""),
        "the deprecated field must be gone on the retry: {}",
        bodies[1]
    );

    // One lucky success is a candidate, not a fact.
    assert!(
        cache.get(&key).is_none(),
        "a single success must not be promoted to durable"
    );

    // ── Calls 2 and 3: same evidence, now durable ──────────────────────
    for _ in 0..2 {
        provider.chat(request.clone()).await.expect("repeat call");
    }
    let confirmed = cache.get(&key).expect("three confirmations promote the trim");
    assert!(confirmed.omit_temperature && !confirmed.adaptive_thinking);

    // ── Fast path: exactly one HTTP request ───────────────────────────
    let before = mock.bodies().len();
    provider.chat(request.clone()).await.expect("fast-path call");
    assert_eq!(
        mock.bodies().len(),
        before + 1,
        "a confirmed trim must take the single-request fast path"
    );

    // ── Persisted in its own file ─────────────────────────────────────
    //
    // Poll for the KEY, not for readability: the file is written whole, and
    // an earlier persist (the one that carried the still-unconfirmed
    // candidate) already left a readable `{"version":3,"entries":{}}`
    // behind. Breaking on "some file exists" therefore sampled that stale
    // snapshot and failed under machine load, while passing whenever the
    // confirmed write happened to win the race.
    let path = cache_path(dir.path().to_str().expect("utf-8 path"));
    let mut persisted = String::new();
    for _ in 0..40 {
        if let Ok(raw) = std::fs::read_to_string(&path)
            && raw.contains(&key)
        {
            persisted = raw;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        persisted.contains(&key),
        "the confirmed trim must survive a restart ({}): {persisted}",
        path.display()
    );

    mock.stop().await;
}

/// The other 400: extended thinking is refused, the adaptive shape is
/// accepted — and the adaptive request never carries a `temperature`
/// (Anthropic forbids it alongside thinking).
#[tokio::test]
async fn extended_thinking_is_replaced_by_the_adaptive_shape() {
    let mock = MockMessages::start().await;
    let mut provider = AnthropicProvider::with_base_url(Some(&mock.base_url()), Some("k"));
    provider.set_provider_id(PROVIDER_ID.to_string());

    let resp = provider
        .chat(chat_request_with_thinking())
        .await
        .expect("the adaptive shape must be accepted");
    assert_eq!(resp.content, "ok");

    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "expected: enabled thinking, then adaptive");
    assert!(
        bodies[0].contains("\"type\":\"enabled\"") && !bodies[0].contains("\"temperature\""),
        "extended thinking goes out first, without temperature: {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("\"adaptive\"") && bodies[1].contains("\"effort\""),
        "the retry must use the adaptive shape with an effort level: {}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains("\"temperature\""),
        "adaptive thinking and temperature never travel together: {}",
        bodies[1]
    );

    mock.stop().await;
}
