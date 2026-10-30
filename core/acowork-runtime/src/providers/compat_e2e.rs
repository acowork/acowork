//! CompatCache v2 end-to-end tests at the REAL HTTP boundary.
//!
//! The 2026-09-05 production incident was: a single lucky `strip_tools`
//! fallback success (masking a `content_integrity` 400) was persisted
//! forever, silently disabling function calling for every later request.
//!
//! These tests prove the v2 guarantees across the actual production call
//! path — `OpenAIProvider::chat → send_with_compat → CompatCache` against a
//! scripted in-test HTTP server (the only simulated part is the provider's
//! wire responses, exactly the trust boundary a unit test cannot cross):
//!
//! 1. Content-integrity 400s are surfaced (never masked, never persisted),
//!    and no fallback request is even attempted (incident regression).
//! 2. A single tools_schema fallback success is only a candidate; three
//!    distinct requests promote it to durable; the next request then takes
//!    the fast path and sends WITHOUT tools (single HTTP hit).
//! 3. TTL lease: an expired durable is re-probed; a re-probe success that
//!    needs the same strip action renews the lease.
//! 4. Fast-path failure (provider behavior changed) invalidates the entry
//!    and the cold path surfaces content errors instead of masking them.
//! 5. A legacy v1 (bare-map) file is discarded and rewritten empty,
//!    so a poisoned `strip_tools` entry cannot survive a restart.
//!
//! Crate-internal because it needs `OpenAIProvider` internals + `CompatCache`
//! (both `pub`, but the module keeps the runtime test tree tidy).

#![cfg(test)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acowork_core::providers::traits::{ChatMessage, ChatRequest, Provider};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::providers::compat::{COMPAT_PROFILE_TTL_SECS, CompatCache};
use crate::providers::openai::OpenAIProvider;

const MODEL: &str = "test-model";

/// A scripted in-test LLM that speaks OpenAI chat-completions JSON.
///
/// Each accepted connection consumes the next `(status, body)` from the
/// script queue and records the raw request body, so tests can assert both
/// "how many requests were sent" and "what fields the request contained"
/// (e.g. that the fast path omitted `tools`).
struct MockLlm {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockLlm {
    async fn start(script: Vec<(u16, &'static str)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock llm");
        let addr = listener.local_addr().expect("local addr");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(
            script
                .into_iter()
                .map(|(s, b)| (s, b.to_string()))
                .collect::<VecDeque<_>>(),
        ));

        let bodies_t = bodies.clone();
        let responses_t = responses.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let bodies_c = bodies_t.clone();
                let responses_c = responses_t.clone();
                tokio::spawn(async move {
                    // Read request head up to \r\n\r\n.
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
                    bodies_c
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body).into_owned());

                    let (status, resp_body) = responses_c
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((500, r#"{"error":"no scripted response left"}"#.to_string()));
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        422 => "Unprocessable Entity",
                        500 => "Internal Server Error",
                        _ => "Error",
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

    /// Raw request bodies in arrival order.
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.handle.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Fixtures
// ═══════════════════════════════════════════════════════════════════════

const OK_BODY: &str = r#"{"choices":[{"message":{"content":"hello-e2e"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
const CONTENT_400: &str = r#"{"error":{"message":"The reasoning_content in the thinking mode must be passed back to the API"}}"#;
const TOOLS_400: &str =
    r#"{"error":{"message":"parallel_tool_calls is not supported by this deployment"}}"#;

/// A chat request that carries a `temperature` and a `tools` payload — the
/// shape where a strip_tools fallback (FB4) can bite.
fn chat_request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hello from e2e"),
        ],
        temperature: Some(0.7),
        max_tokens: None,
        tools: Some(vec![json!({
            "name": "get_weather",
            "description": "weather lookup",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        })]),
        reasoning_effort: None,
        thinking_mode: None,
    }
}

fn provider(server: &MockLlm, cache: Arc<CompatCache>) -> OpenAIProvider {
    let mut p = OpenAIProvider::with_base_url_and_timeouts(
        Some(&server.base_url()),
        Some("sk-e2e"),
        Duration::from_secs(5),
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    p.set_provider_id("e2e-provider".to_string());
    p.set_compat_cache(cache);
    p
}

fn cache_key() -> String {
    // ADR-103 D7.3: keys are shape-qualified; these e2e cases drive the
    // Chat-Completions provider, so they use that shape.
    crate::providers::compat::cache_key(
        "e2e-provider",
        crate::providers::compat::SHAPE_COMPLETIONS,
        MODEL,
    )
}

fn tmp_cache_path(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("provider_compat_{tag}.json"));
    (dir, path)
}

/// Read the on-disk v2 file's durable entries.
fn disk_entries(path: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
}

/// Seed a current-version file with one durable entry (used by TTL /
/// invalidation tests).
fn seed_durable_file(path: &std::path::Path, promoted_at_unix_ts: u64, class: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let file = json!({
        "version": 3,
        "entries": {
            cache_key(): {
                "profile": {
                    "strip_stream_options": false,
                    "strip_reasoning_effort": false,
                    "strip_thinking": false,
                    "strip_temperature": true,
                    "strip_tools": true,
                    "max_tokens_cap": null,
                    "fallback_generation": 4,
                    "last_success_unix_ts": promoted_at_unix_ts,
                },
                "class": class,
                "promoted_at_unix_ts": promoted_at_unix_ts,
            }
        }
    });
    std::fs::write(path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ═══════════════════════════════════════════════════════════════════════
// E2E 1 — content-integrity 400 is surfaced, never masked, never persisted
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn content_integrity_400_is_surfaced_without_fallback_and_not_recorded() {
    // This is the exact 2026-09-05 incident shape. The OLD v1 behavior:
    // FB1-3 fail, FB4 (strip_tools) "succeeds", and that lucky accident is
    // persisted as a durable rule. v2 must do the opposite: raise the error,
    // never attempt the strip_tools loophole, never write anything.
    let (_dir, path) = tmp_cache_path("content");
    let cache = CompatCache::load(path.clone());

    let server = MockLlm::start(vec![(400, CONTENT_400)]).await;
    let p = provider(&server, cache.clone());
    let result = p.chat(chat_request()).await;

    let err = result.expect_err("content-integrity rejection must surface as Err");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("reasoning_content"),
        "error must carry the provider message: {rendered}"
    );

    // Exactly ONE HTTP request: no strip_tools fallback was attempted.
    assert_eq!(
        server.bodies().len(),
        1,
        "content-integrity must not trigger the fallback chain"
    );

    // Nothing learned, nothing persisted: the file must not even exist.
    assert!(
        !path.exists(),
        "content-integrity rejection must never persist anything"
    );
    assert!(cache.get(&cache_key()).is_none());
    server.stop().await;

    // A subsequent healthy request (same cache, still empty) succeeds on the
    // first try — the failed batch did not poison anything.
    let server2 = MockLlm::start(vec![(200, OK_BODY)]).await;
    let p2 = provider(&server2, cache);
    let ok = p2
        .chat(chat_request())
        .await
        .expect("healthy request succeeds");
    assert_eq!(ok.content, "hello-e2e");
    assert_eq!(
        server2.bodies().len(),
        1,
        "no fallback chain for a healthy request"
    );
    server2.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════
// E2E 2 — single success is a candidate; confirmation promotes; fast path
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tools_schema_single_success_not_durable_then_confirmed_and_fast_path_without_tools() {
    let (_dir, path) = tmp_cache_path("promote");
    let cache = CompatCache::load(path.clone());

    // Three cold chats each cost: plain 400 → FB1 400 → FB2 400 → FB3 400 →
    // FB4 (tools stripped) 200. The fourth chat hits the durable fast path.
    let mut script = Vec::new();
    for _ in 0..3 {
        script.extend([
            (400, TOOLS_400),
            (400, TOOLS_400),
            (400, TOOLS_400),
            (400, TOOLS_400),
            (200, OK_BODY),
        ]);
    }
    script.push((200, OK_BODY)); // fast path, single hit
    let server = MockLlm::start(script).await;
    let p = provider(&server, cache.clone());

    // Chat 1 + 2: still candidates only.
    p.chat(chat_request())
        .await
        .expect("chat1 succeeds via FB4");
    assert!(
        cache.get(&cache_key()).is_none(),
        "one success must not be durable"
    );
    assert_eq!(server.bodies().len(), 5);

    p.chat(chat_request())
        .await
        .expect("chat2 succeeds via FB4");
    assert!(
        cache.get(&cache_key()).is_none(),
        "two successes must still be candidates"
    );
    assert_eq!(server.bodies().len(), 10);

    // Chat 3: third distinct confirmation promotes to durable.
    p.chat(chat_request())
        .await
        .expect("chat3 succeeds via FB4");
    let durable = cache
        .get(&cache_key())
        .expect("third confirmation promotes to durable");
    assert!(
        durable.strip_tools,
        "durable learned strip_tools (provider truly rejects tools)"
    );

    // The durable profile is persisted as class=tools_schema (give the
    // spawned persist task a beat to flush).
    tokio::time::sleep(Duration::from_millis(120)).await;
    let entries = disk_entries(&path);
    assert_eq!(
        entries["entries"][cache_key().as_str()]["class"],
        "tools_schema"
    );
    assert_eq!(server.bodies().len(), 15);

    // Chat 4: fast path — exactly ONE HTTP request, tools (and temperature,
    // learned alongside) omitted from the wire body.
    p.chat(chat_request())
        .await
        .expect("fast-path chat succeeds");
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 16, "fast path must be a single HTTP request");
    let fast = &bodies[15];
    assert!(
        !fast.contains("\"tools\""),
        "fast-path request must omit tools: {fast}"
    );
    assert!(
        !fast.contains("\"temperature\""),
        "fast-path request must omit the stripped temperature field: {fast}"
    );
    server.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════
// E2E 3 — TTL lease: expired durable is re-probed; success renews it
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn expired_durable_is_reprobed_and_renewed_by_matching_fallback_success() {
    let (_dir, path) = tmp_cache_path("ttl");
    // Seed an expired durable (promoted long ago) — as if the process had
    // been offline past the lease. get() must treat it as a miss.
    seed_durable_file(
        &path,
        now_ts() - COMPAT_PROFILE_TTL_SECS - 60,
        "tools_schema",
    );
    let cache = CompatCache::load(path.clone());
    assert!(
        cache.get(&cache_key()).is_none(),
        "expired lease = miss (re-probe)"
    );

    // Re-probe: plain request fails with the same class; FB4 again succeeds.
    let server = MockLlm::start(vec![
        (400, TOOLS_400),
        (400, TOOLS_400),
        (400, TOOLS_400),
        (400, TOOLS_400),
        (200, OK_BODY),
    ])
    .await;
    let p = provider(&server, cache.clone());
    p.chat(chat_request())
        .await
        .expect("re-probe chat succeeds");

    let renewed = cache
        .get(&cache_key())
        .expect("matching re-probe success renews the lease");
    assert!(renewed.strip_tools);
    assert_eq!(
        server.bodies().len(),
        5,
        "expired lease went through the full cold chain"
    );
    server.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════
// E2E 4 — fast-path failure invalidates; content errors then surface
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn fast_path_failure_invalidates_and_content_errors_surface_not_masked() {
    let (_dir, path) = tmp_cache_path("invalidate");
    // A fresh (unexpired) durable tools_schema profile — provider "learned"
    // earlier that tools must be stripped.
    seed_durable_file(&path, now_ts(), "tools_schema");
    let cache = CompatCache::load(path.clone());
    assert!(cache.get(&cache_key()).is_some());

    // The provider's behavior changed: even the stripped fast-path request
    // now returns a content-integrity 400. v2 must invalidate the profile and
    // surface the error — NOT keep retrying or masking it.
    let server = MockLlm::start(vec![(400, CONTENT_400), (400, CONTENT_400)]).await;
    let p = provider(&server, cache.clone());
    let err = p
        .chat(chat_request())
        .await
        .expect_err("content error must surface");
    assert!(format!("{err}").contains("reasoning_content"));

    // Request 1 = fast path (profile applied). Request 2 = cold path re-probe
    // of the ORIGINAL request, which also fails with the content error.
    assert_eq!(
        server.bodies().len(),
        2,
        "fast-path failure + one cold re-probe"
    );
    assert!(
        cache.get(&cache_key()).is_none(),
        "failed fast path must invalidate the durable profile"
    );
    // Invalidate persists immediately — allow the spawned task to flush.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let entries = disk_entries(&path);
    assert_eq!(
        entries["entries"].as_object().map(|m| m.len()).unwrap_or(0),
        0
    );
    server.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════
// E2E 5 — legacy v1 bare-map file is discarded, rewritten empty
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn legacy_v1_file_is_discarded_and_next_request_stays_healthy() {
    let (_dir, path) = tmp_cache_path("legacy");
    // The poisoned v1 entry from the actual incident file.
    let v1 = json!({
        "e2e-provider::test-model": {
            "strip_stream_options": false,
            "strip_reasoning_effort": false,
            "strip_thinking": false,
            "strip_temperature": false,
            "strip_tools": true,
            "max_tokens_cap": null,
            "fallback_generation": 4,
            "last_success_unix_ts": now_ts(),
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&v1).unwrap()).unwrap();

    let cache = CompatCache::load(path.clone());
    assert!(
        cache.get(&cache_key()).is_none(),
        "v1 single-sample entries are not trusted"
    );

    // File was proactively rewritten as an empty current-version file.
    let entries = disk_entries(&path);
    assert_eq!(entries["version"], 3);
    assert_eq!(
        entries["entries"].as_object().map(|m| m.len()).unwrap_or(0),
        0
    );

    // The next request goes out plain (tools intact) and succeeds — no
    // strip_tools rule survived the upgrade.
    let server = MockLlm::start(vec![(200, OK_BODY)]).await;
    let p = provider(&server, cache);
    p.chat(chat_request()).await.expect("healthy request");
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("\"tools\""),
        "request must still carry tools"
    );
    server.stop().await;
}
