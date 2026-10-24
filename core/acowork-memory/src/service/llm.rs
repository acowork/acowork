//! Distiller LLM client (ADR-100 decision 7).
//!
//! The consolidation pipeline needs one thing from a model: send a list of
//! messages, get a JSON string back. That is [`crate::consolidation::ConsolidationLlm`],
//! and the Runtime satisfied it with [`acowork_core::providers::traits::Provider`]
//! — the full chat API with streaming, tool calls and per-protocol wire formats,
//! wrapped by an adapter and a retry provider.
//!
//! This service deliberately does **not** carry that machinery. The distiller
//! is a background batch job that makes non-streaming, tool-free calls, so the
//! surface actually required is one `POST /chat/completions`. Speaking the
//! OpenAI-compatible dialect directly keeps the crate free of a dependency on
//! the Runtime's provider factory (which would invert the dependency the
//! moment P3 strips memory out of the Runtime).
//!
//! Consequences, stated plainly:
//! - providers whose Gateway protocol is Anthropic / Google / Ollama-native are
//!   skipped by [`resolve_candidates`] — the pipeline runs on whatever
//!   OpenAI-compatible provider is configured, and says so in the log when
//!   there is none;
//! - the candidate chain is walked exactly like `ProviderLlmAdapter` walked it:
//!   first success wins, every failure is logged with its candidate index, and
//!   the last error is what the caller sees.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use acowork_core::protocol::{CompactModelRef, ProtocolType};

use crate::consolidation::{ConsolidationLlm, LlmMessage, LlmResponse};
use crate::service::resources::GlobalResources;

/// Output-token ceiling for one extraction / merge call.
///
/// Carried over from the Runtime adapter verbatim: the merge reply is one JSON
/// object per episode, and the previous 2048 ceiling truncated a 100-episode
/// batch a quarter of the way through, which failed the parse and lost every
/// episode in the batch.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Temperature of every distiller call — low, because the reply is structured.
/// `f64` so it serializes as exactly `0.1` rather than the `f32` rounding.
const TEMPERATURE: f64 = 0.1;

/// One resolved distiller target: a provider endpoint plus the model to ask
/// that endpoint for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCandidate {
    /// Gateway provider id, kept so the log says which account was called.
    pub provider_id: String,
    /// OpenAI-compatible base URL **without** the `/chat/completions` suffix.
    pub base_url: String,
    /// Vault-decrypted key from the global-resources snapshot. Never persisted
    /// and never written to a log line.
    pub api_key: Option<String>,
    pub model: String,
}

/// Ordered candidate chain behind [`ConsolidationLlm`].
#[derive(Debug, Clone)]
pub struct ChainLlm {
    candidates: Vec<ChatCandidate>,
    max_tokens: u32,
    http: reqwest::Client,
}

impl ChainLlm {
    /// Build a chain over `candidates` (order = priority). An empty chain is
    /// legal to construct — [`ConsolidationLlm::chat`] then reports the same
    /// "no distiller candidates configured" error the Runtime adapter did, so
    /// the caller's failure path is identical.
    pub fn new(candidates: Vec<ChatCandidate>, timeout: Duration) -> Self {
        Self {
            candidates,
            max_tokens: DEFAULT_MAX_TOKENS,
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
        }
    }

    /// How many candidates this chain will walk.
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// The candidate list (ids and models only — keys are not readable back).
    pub fn candidates(&self) -> Vec<(String, String)> {
        self.candidates
            .iter()
            .map(|c| (c.provider_id.clone(), c.model.clone()))
            .collect()
    }

    /// One call against one candidate.
    async fn chat_once(
        &self,
        cand: &ChatCandidate,
        body: serde_json::Value,
    ) -> Result<LlmResponse, String> {
        let url = format!("{}/chat/completions", cand.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &cand.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| format!("send: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            // Surface the provider's own message when it sends one — "model not
            // found" is a different fix from "the endpoint is down".
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("{status}: {}", truncate(&text, 400)));
        }
        let parsed: ChatCompletion = resp
            .json()
            .await
            .map_err(|e| format!("decode response: {e}"))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| "response carried no choices".to_string())?;
        Ok(LlmResponse {
            content: choice.message.content,
            usage_tokens: parsed.usage.map(|u| u.total_tokens),
            finish_reason: choice.finish_reason,
        })
    }
}

#[async_trait::async_trait]
impl ConsolidationLlm for ChainLlm {
    async fn chat(&self, messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String> {
        let mut body = json!({
            "messages": messages
                .iter()
                .map(|m| json!({"role": m.role, "content": m.content}))
                .collect::<Vec<_>>(),
            "temperature": TEMPERATURE,
            "max_tokens": self.max_tokens,
            // Deterministic structured output: the distiller never expects a
            // tool call back, and a model that may call tools may answer with
            // one instead of the JSON it was asked for.
            "tool_choice": "none",
            "stream": false,
        });

        let mut last_err = String::from("no distiller candidates configured");
        for (i, cand) in self.candidates.iter().enumerate() {
            // One field differs per candidate — the model — so the shared body
            // is built once and stamped.
            body["model"] = json!(&cand.model);

            match self.chat_once(cand, body.clone()).await {
                Ok(response) => {
                    if i > 0 {
                        tracing::info!(
                            candidate_index = i,
                            provider_id = %cand.provider_id,
                            model = %cand.model,
                            "distiller LLM call succeeded on a fallback candidate"
                        );
                    }
                    if response.finish_reason.as_deref() == Some("length") {
                        tracing::warn!(
                            candidate_index = i,
                            provider_id = %cand.provider_id,
                            model = %cand.model,
                            max_tokens = self.max_tokens,
                            "distiller LLM reply truncated by max_tokens"
                        );
                    }
                    return Ok(response);
                }
                Err(e) => {
                    tracing::warn!(
                        candidate_index = i,
                        provider_id = %cand.provider_id,
                        model = %cand.model,
                        error = %e,
                        "distiller LLM candidate failed, trying next"
                    );
                    last_err = format!("provider chat failed: {e}");
                }
            }
        }
        Err(last_err)
    }
}

/// Resolve the ordered candidate chain.
///
/// Layers, highest first — the same shape the Runtime used, with the manifest
/// and `agent_config.json` layers replaced by the service's own file and the
/// panel's per-instance override:
///
/// 1. `instance_override` — the per-instance model list the memory panel chose
///    (`PUT /memory/consolidation/status`);
/// 2. `service_models` — `[distiller].models` in `config.toml`;
/// 3. `resources.default_compact_models` — the Gateway's global compact chain;
/// 4. each ready provider's own compact model (or first model), in snapshot
///    order — the "nothing was configured anywhere" fallback, which mirrors
///    the Runtime's "first model in the provider list" convention but keeps
///    more than one candidate alive.
///
/// An empty `provider_id` in a reference means "the first provider that can
/// serve this model" (the Runtime read it as "the agent's active provider"; a
/// service has no active provider, so it resolves against the snapshot).
/// Duplicates by `(provider_id, model_id)` are dropped, and providers this
/// client cannot talk to are skipped with a warning.
pub fn resolve_candidates(
    resources: Option<&GlobalResources>,
    instance_override: &[CompactModelRef],
    service_models: &[CompactModelRef],
) -> Vec<ChatCandidate> {
    let Some(resources) = resources else {
        tracing::debug!("no global-resources snapshot yet — distiller has no candidate");
        return Vec::new();
    };

    let mut out: Vec<ChatCandidate> = Vec::new();

    for m in instance_override {
        push_candidate(&mut out, resources, &m.provider_id, &m.model_id);
    }
    for m in service_models {
        push_candidate(&mut out, resources, &m.provider_id, &m.model_id);
    }
    for m in &resources.default_compact_models {
        push_candidate(&mut out, resources, &m.provider_id, &m.model_id);
    }
    if out.is_empty() {
        for provider in &resources.providers {
            let model = provider
                .compact_model
                .clone()
                .or_else(|| provider.models.first().cloned());
            if let Some(model) = model {
                push_candidate(&mut out, resources, &provider.id, &model);
            }
        }
    }
    out
}

/// Append one `(provider_id, model_id)` pair, dropping duplicates and
/// references this snapshot cannot serve.
fn push_candidate(
    out: &mut Vec<ChatCandidate>,
    resources: &GlobalResources,
    provider_id: &str,
    model_id: &str,
) {
    let Some(cand) = build_candidate(resources, provider_id, model_id) else {
        return;
    };
    if !out
        .iter()
        .any(|c| c.provider_id == cand.provider_id && c.model == cand.model)
    {
        out.push(cand);
    }
}

/// One `(provider_id, model_id)` pair → a callable candidate, or `None` when no
/// ready provider can serve it.
fn build_candidate(
    resources: &GlobalResources,
    provider_id: &str,
    model_id: &str,
) -> Option<ChatCandidate> {
    let matches = |p: &crate::service::resources::ProviderSnapshot| {
        if provider_id.is_empty() {
            // Unqualified reference: any provider that actually lists the
            // model. Falling back to the first provider reproduces the
            // Runtime's "active provider" reading when the id is a leftover.
            p.models.iter().any(|m| m == model_id) || p.compact_model.as_deref() == Some(model_id)
        } else {
            p.id == provider_id
        }
    };
    let provider = resources
        .providers
        .iter()
        .find(|p| matches(p))
        .or_else(|| {
            provider_id
                .is_empty()
                .then(|| resources.providers.first())
                .flatten()
        })?;

    if provider.protocol != ProtocolType::OpenAI {
        tracing::warn!(
            provider_id = %provider.id,
            model_id,
            protocol = ?provider.protocol,
            "distiller candidate skipped: this client speaks OpenAI-compatible \
             chat completions only"
        );
        return None;
    }
    if provider.base_url.trim().is_empty() {
        tracing::warn!(provider_id = %provider.id, "distiller candidate skipped: empty base_url");
        return None;
    }
    Some(ChatCandidate {
        provider_id: provider.id.clone(),
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        model: model_id.to_string(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push('…');
    out
}

// ── Wire shapes ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    /// Empty when the model returned only tool calls — the distiller treats it
    /// as content and the JSON parse upstream fails loudly, which is the
    /// behaviour this has to preserve.
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    total_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::resources::{GlobalResources, ProviderSnapshot};
    use acowork_core::protocol::ProtocolType;
    use axum::Json;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn provider(
        id: &str,
        protocol: ProtocolType,
        base_url: &str,
        models: &[&str],
    ) -> ProviderSnapshot {
        ProviderSnapshot {
            id: id.into(),
            base_url: base_url.into(),
            protocol,
            api_key: Some(format!("sk-{id}")),
            models: models.iter().map(|m| m.to_string()).collect(),
            compact_model: None,
        }
    }

    fn resources() -> GlobalResources {
        GlobalResources {
            providers: vec![
                provider(
                    "volc",
                    ProtocolType::OpenAI,
                    "https://ark.example/api/v3",
                    &["deepseek-v3", "kimi"],
                ),
                provider(
                    "claude",
                    ProtocolType::Anthropic,
                    "https://api.anthropic.com",
                    &["sonnet"],
                ),
                provider(
                    "local",
                    ProtocolType::Ollama,
                    "http://127.0.0.1:11434",
                    &["llama"],
                ),
            ],
            default_compact_models: vec![CompactModelRef {
                provider_id: "volc".into(),
                model_id: "kimi".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn layers_are_ordered_and_deduplicated() {
        let got = resolve_candidates(
            Some(&resources()),
            &[CompactModelRef {
                provider_id: "volc".into(),
                model_id: "deepseek-v3".into(),
            }],
            &[CompactModelRef {
                provider_id: "volc".into(),
                model_id: "deepseek-v3".into(),
            }],
        );
        // Layer 1 wins; the layer-2 duplicate is dropped, and the global
        // compact model still contributes its own pair.
        assert_eq!(
            got.iter()
                .map(|c| (c.provider_id.clone(), c.model.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("volc".to_string(), "deepseek-v3".to_string()),
                ("volc".to_string(), "kimi".to_string())
            ]
        );
        assert_eq!(got[0].api_key.as_deref(), Some("sk-volc"));
    }

    #[test]
    fn non_openai_protocol_candidates_are_skipped() {
        let got = resolve_candidates(
            Some(&resources()),
            &[
                CompactModelRef {
                    provider_id: "claude".into(),
                    model_id: "sonnet".into(),
                },
                CompactModelRef {
                    provider_id: "local".into(),
                    model_id: "llama".into(),
                },
                CompactModelRef {
                    provider_id: "volc".into(),
                    model_id: "kimi".into(),
                },
            ],
            &[],
        );
        assert_eq!(
            got.len(),
            1,
            "only the OpenAI-compatible pair survives: {got:?}"
        );
        assert_eq!(got[0].provider_id, "volc");
    }

    #[test]
    fn an_unqualified_reference_resolves_against_a_provider_that_lists_the_model() {
        let got = resolve_candidates(
            Some(&resources()),
            &[CompactModelRef {
                provider_id: String::new(),
                model_id: "kimi".into(),
            }],
            &[],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].provider_id, "volc", "kimi is only served there");
    }

    #[test]
    fn nothing_configured_anywhere_falls_back_to_each_provider() {
        let mut res = resources();
        res.default_compact_models.clear();
        // Only OpenAI-compatible providers survive, so the fallback chain is
        // one entry here — but it is a working entry, not a dead "default".
        let got = resolve_candidates(Some(&res), &[], &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].model, "deepseek-v3",
            "first model of the first usable provider"
        );
    }

    #[test]
    fn no_snapshot_means_no_candidates() {
        assert!(resolve_candidates(None, &[], &[]).is_empty());
        let llm = ChainLlm::new(vec![], Duration::from_secs(1));
        assert_eq!(llm.candidate_count(), 0);
    }

    #[test]
    fn an_unknown_provider_id_is_dropped_not_substituted() {
        let mut res = resources();
        res.default_compact_models.clear();
        let got = resolve_candidates(
            Some(&res),
            &[CompactModelRef {
                provider_id: "typo".into(),
                model_id: "m".into(),
            }],
            &[],
        );
        // The bad pair is dropped outright — a typo in the panel must not
        // silently retarget a different account — and the chain degrades to the
        // "nothing resolved anywhere" provider fallback.
        assert!(
            !got.iter()
                .any(|c| c.provider_id == "typo" || c.model == "m"),
            "unknown reference leaked into the chain: {got:?}"
        );
        assert_eq!(
            got.iter()
                .map(|c| c.provider_id.clone())
                .collect::<Vec<_>>(),
            vec!["volc".to_string()]
        );
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        // The budget counts bytes, not chars: 4 lands inside the second 3-byte
        // glyph, so the cut walks back to the previous boundary instead of
        // panicking on a sliced char.
        assert_eq!(truncate("中文测试", 4), "中…");
        assert_eq!(truncate("中文测试", 7), "中文…");
        assert_eq!(truncate("short", 100), "short");
    }

    /// The chain must walk: a failing first candidate is retried down the list,
    /// and the body actually sent carries the per-candidate model.
    #[tokio::test]
    async fn chat_walks_candidates_in_order_and_reports_the_winning_body() {
        let calls: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let seen: Arc<tokio::sync::Mutex<Vec<Value>>> = Arc::new(tokio::sync::Mutex::new(vec![]));
        let counter = calls.clone();
        let recorder = seen.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |Json(body): Json<Value>| {
                let counter = counter.clone();
                let recorder = recorder.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    recorder.lock().await.push(body.clone());
                    if n == 0 {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": {"message": "upstream exploded"}})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({
                            "choices": [{
                                "message": {"role": "assistant", "content": "[{\"subject\":\"user\"}]"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"total_tokens": 123}
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}/v1");

        let llm = ChainLlm::new(
            vec![
                ChatCandidate {
                    provider_id: "bad".into(),
                    base_url: base.clone(),
                    api_key: Some("k1".into()),
                    model: "model-a".into(),
                },
                ChatCandidate {
                    provider_id: "good".into(),
                    base_url: base.clone(),
                    api_key: Some("k2".into()),
                    model: "model-b".into(),
                },
            ],
            Duration::from_secs(5),
        );
        let got = llm
            .chat(vec![
                LlmMessage {
                    role: "system".into(),
                    content: "extract".into(),
                },
                LlmMessage {
                    role: "user".into(),
                    content: "I like rust".into(),
                },
            ])
            .await
            .expect("second candidate should carry the run");
        assert_eq!(got.content, "[{\"subject\":\"user\"}]");
        assert_eq!(got.usage_tokens, Some(123));
        assert_eq!(got.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "first failure must not abort the chain"
        );

        let bodies = seen.lock().await;
        assert_eq!(bodies[0]["model"], "model-a");
        assert_eq!(
            bodies[1]["model"], "model-b",
            "each candidate sends its own model"
        );
        assert_eq!(bodies[1]["temperature"], 0.1);
        assert_eq!(bodies[1]["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(bodies[1]["stream"], false);
        assert_eq!(bodies[1]["messages"][0]["role"], "system");
        assert_eq!(bodies[1]["messages"][1]["content"], "I like rust");
    }

    /// Every candidate failing must surface the **last** provider message, the
    /// way `ProviderLlmAdapter` surfaced it — that string is what the panel
    /// shows under `last_run.error`.
    #[tokio::test]
    async fn all_candidates_failing_reports_the_last_error() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"message": "model not found"}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let llm = ChainLlm::new(
            vec![ChatCandidate {
                provider_id: "p".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: None,
                model: "m".into(),
            }],
            Duration::from_secs(5),
        );
        let err = llm.chat(vec![]).await.unwrap_err();
        assert!(
            err.contains("model not found"),
            "provider message lost: {err}"
        );

        // An empty chain keeps the built-in wording the Runtime used.
        let empty = ChainLlm::new(vec![], Duration::from_secs(1));
        assert_eq!(
            empty.chat(vec![]).await.unwrap_err(),
            "no distiller candidates configured"
        );
    }

    #[tokio::test]
    async fn a_truncated_reply_is_flagged_not_swallowed() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                Json(json!({
                    "choices": [{"message": {"content": "[{\"subject\":"}, "finish_reason": "length"}]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let llm = ChainLlm::new(
            vec![ChatCandidate {
                provider_id: "p".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: None,
                model: "m".into(),
            }],
            Duration::from_secs(5),
        );
        let got = llm.chat(vec![]).await.unwrap();
        assert!(
            got.truncated(),
            "finish_reason=length must survive the decode"
        );
    }

    #[tokio::test]
    async fn a_response_without_choices_is_an_error() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async { Json(json!({"choices": []})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let llm = ChainLlm::new(
            vec![ChatCandidate {
                provider_id: "p".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: None,
                model: "m".into(),
            }],
            Duration::from_secs(5),
        );
        assert!(llm.chat(vec![]).await.unwrap_err().contains("no choices"));
    }
}
