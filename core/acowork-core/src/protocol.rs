//! Gateway Service API message definitions (contract layer, transport-agnostic)
//!
//! Defines the protocol between Agent Runtime and Gateway.
//! All messages are JSON-serializable.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::budget::UsageReport;

/// Default connection role for backward compatibility
fn default_connection_role() -> String {
    "main".to_string()
}

/// Default value for boolean fields that should default to true
fn default_true() -> bool {
    true
}

/// Default max output tokens limit (16K = 1024×16) — the last-resort floor
/// so a model always has output space when neither models.dev nor the
/// Gateway config says anything.
fn default_max_output_tokens_limit() -> u64 {
    16_384
}

/// Cost information for a model (per million tokens)
///
/// Used by BudgetGuard for cost-aware token budgeting.
/// Values are in USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCostInfo {
    /// Input cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    /// Output cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
}

/// Modality information for a model
///
/// Describes what input/output formats the model supports.
/// Used for future multimodal routing decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelModalities {
    /// Input modalities (e.g. "text", "image", "audio", "video")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Output modalities (e.g. "text", "image")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

/// Model capabilities info (queried from models.dev / offline data)
///
/// Populated by Gateway when delivering LLM config to Agent Runtime.
/// The Runtime uses this to adapt max_tokens, budget tracking, and
/// other parameters without hardcoding model limits in manifests.
///
/// Design principle: carry as much models.dev data as possible to
/// avoid future protocol changes. All new fields are optional with
/// serde defaults for backward compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilitiesInfo {
    // ── Limit (core, always populated from models.dev) ──
    /// Context window size (total tokens: input + output)
    pub context_window: u64,
    /// Maximum output tokens the model can generate
    pub max_output_tokens: u64,
    /// Maximum input tokens (optional, from models.dev limit.input).
    /// When available, usable context = max_input_tokens - reserved.
    /// When absent, usable context = context_window - max_output_tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,

    // ── Capability flags ──
    /// Whether the model supports tool/function calling
    #[serde(default = "default_true")]
    pub supports_tool_calling: bool,
    /// Whether the model supports reasoning/thinking (e.g. o1, deepseek-reasoner)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Whether the model supports file attachments (multimodal input)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_attachment: Option<bool>,
    /// Whether the model supports temperature parameter
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,

    // ── Cost (for budget tracking) ──
    /// Pricing information (USD per 1M tokens)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostInfo>,

    // ── Modalities (for future multimodal support) ──
    /// Supported input/output modalities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelModalities>,

    // ── Metadata (for display and routing) ──
    /// Model display name (e.g. "GPT-4o", "Claude Sonnet 4")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Model family (e.g. "gpt", "claude", "qwen")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Knowledge cutoff date (e.g. "2025-04", "2024-10")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,

    // ── Reasoning configuration ──
    /// Default reasoning effort level for this model (user-configured).
    /// Values: "off", "low", "medium", "high", "max".
    /// When `None`, the model's built-in default is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    /// Anthropic thinking mode: "extended" (budget_tokens) or "adaptive".
    /// When `None`, the provider auto-detects based on model capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<String>,
}

impl ModelCapabilitiesInfo {
    /// Output-space reserve derived from this model's caps and the user's
    /// `max_output_tokens_limit`.
    ///
    /// - models.dev provides output → cap it by limit (if limit > 0),
    ///   otherwise use raw value (user disabled the cap).
    /// - models.dev missing, limit > 0 → use limit as default reserve.
    /// - both missing/0 → fall back to the system default (16K) so the
    ///   model always has output space.
    pub fn output_reserve(&self, max_output_tokens_limit: u64) -> u64 {
        if self.max_output_tokens > 0 {
            if max_output_tokens_limit > 0 {
                self.max_output_tokens.min(max_output_tokens_limit)
            } else {
                self.max_output_tokens
            }
        } else if max_output_tokens_limit > 0 {
            max_output_tokens_limit
        } else {
            default_max_output_tokens_limit()
        }
    }

    /// Effective input token budget when **no** user-imposed context cap is
    /// in effect.
    ///
    /// Derivation:
    /// 1. `max_input_tokens` provided — authoritative, use directly.
    /// 2. `max_input_tokens` missing — `context_window - output_reserve`.
    ///
    /// `max_output_tokens_limit` is the global cap (default 16K, configurable
    /// by user). Set to 0 to disable capping models.dev values — but when
    /// models.dev also provides nothing, the system default (16K) is used
    /// as a safety floor so the model always has output space.
    pub fn effective_input_budget(&self, max_output_tokens_limit: u64) -> u64 {
        if let Some(max_input) = self.max_input_tokens {
            return max_input;
        }
        self.context_window
            .saturating_sub(self.output_reserve(max_output_tokens_limit))
    }

    /// Input budget when the user imposes a **total context cap**
    /// (`Some(cap)`, `Some(0)`/`None` = no cap).
    ///
    /// The cap bounds the model window first, then the output reserve is
    /// subtracted **inside** the cap. Without this ordering a cap below
    /// `window − reserve` silently swallowed the output reservation and the
    /// input side could consume the whole cap (a 250K cap + 32K output
    /// reserve behaved like a 250K *input* budget instead of 218K — the
    /// mismatch reported in the 2026-09-06 compaction-threshold review).
    ///
    /// A provider-declared `max_input_tokens` still binds when it is smaller
    /// than the capped window (it is an input-only allowance and is NOT
    /// double-subtracted against the output reserve).
    pub fn input_budget_with_cap(
        &self,
        context_cap: Option<u64>,
        max_output_tokens_limit: u64,
    ) -> u64 {
        match context_cap {
            Some(0) | None => self.effective_input_budget(max_output_tokens_limit),
            Some(cap) => {
                let window = self.context_window;
                let total = window.min(cap);
                let provider_input = self
                    .max_input_tokens
                    .map(|mi| mi.min(window))
                    .unwrap_or(window);
                provider_input
                    .min(total)
                    .saturating_sub(self.output_reserve(max_output_tokens_limit))
            }
        }
    }
}

/// ADR-056: Cross-provider reference to a (provider_id, model_id) pair.
///
/// Used for global selections that span providers, such as the user's
/// chosen default compact model. Mirrors the on-disk
/// `agent_provider.json` top-level `default_compact_model` object and
/// the `AvailableProviders.default_compact_model` proto field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CompactModelRef {
    /// Provider identifier (matches `ProviderListItem.id`).
    pub provider_id: String,
    /// Model identifier within that provider (matches `ProviderModelEntry.id`).
    pub model_id: String,
}

/// Provider list entry — delivered by Gateway to Runtime via AgentHelloResult.
///
/// Contains all metadata needed to construct a Provider instance
/// (base_url, protocol_type, models with capabilities).
/// API keys are NOT included — see ProviderKeyEntry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderListItem {
    /// Provider identifier (e.g. "alibaba-cn", "openai")
    pub id: String,
    /// API base URL
    pub base_url: String,
    /// LLM protocol type
    pub protocol_type: ProtocolType,
    /// Available models for this provider with full capabilities
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ProviderModelEntry>,
    /// Compact model for LLM summarization / context compression (ADR-010).
    /// When set, the Runtime uses this model for context summarization instead
    /// of the main chat model. Set by the user in frontend Provider Settings.
    /// None = fall back to the session's current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a user-defined custom provider (not listed in models.dev).
    /// Custom providers always use OpenAI-compatible protocol.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
}

/// Individual model entry within a provider's model list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelEntry {
    /// Model identifier (e.g. "gpt-4o", "qwen-plus")
    pub id: String,
    /// Resolved model capabilities from models.dev offline data
    pub capabilities: ModelCapabilitiesInfo,
    /// Gateway-level max output tokens limit for this model
    #[serde(default = "default_max_output_tokens_limit")]
    pub max_output_tokens_limit: u64,
}

/// MCP server list entry — delivered by Gateway to Runtime via AgentHelloResult.
///
/// Describes an installed MCP server that the Runtime can connect to.
/// API keys/tokens are NOT included — see McpKeyEntry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpListItem {
    /// Server identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Transport type
    #[serde(default)]
    pub transport: McpTransportDef,
    /// Server URL (for HTTP/SSE transports)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Command path (for stdio transport)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Command arguments
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// HTTP headers
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Tool timeout override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u64>,
}

/// Provider key entry — delivered by Gateway to Runtime via AgentHelloResult.
///
/// One entry per `(provider, account)` pair: when a provider has
/// multiple accounts the Gateway sends one row per account, all with
/// their own `account_id`. Runtime addresses each account by its UUID;
/// `provider_id` alone is ambiguous for multi-account providers.
///
/// Always delivered in full on every AgentHello (no version check).
/// Runtime stores this ONLY in memory, never persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderKeyEntry {
    /// Provider identifier
    pub provider_id: String,
    /// Stable account UUID. Use this together with `provider_id` to
    /// address a specific account; matches the `account_id` returned
    /// by `GET /api/providers`.
    pub account_id: String,
    /// Decrypted API key
    pub api_key: String,
}

/// MCP key entry — delivered by Gateway to Runtime via AgentHelloResult.
///
/// Always delivered in full on every AgentHello (no version check).
/// Runtime stores this ONLY in memory, never persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpKeyEntry {
    /// MCP server identifier
    pub mcp_id: String,
    /// API key or access token (optional, some MCP servers don't require auth)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Per-agent provider configuration — persisted to agent_provider.json.
///
/// Contains the Gateway-pushed provider list (with models and capabilities).
/// API keys are NEVER stored here — they are delivered inline via MQTT
/// `AvailableProviders.api_key` and held only in memory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentProviderConfig {
    /// Gateway-provided provider list (from acowork/global/providers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderListItem>,
    /// Monotonic version for diff sync (mirrors AvailableProviders.version).
    pub version: u64,
    /// ADR-056: Ordered list of global compact-model candidates — picked by
    /// the user in the Harness from across all configured providers. The
    /// Runtime tries them in order; when all fail it falls back to
    /// `provider.compact_model` and the chat model. Empty = no global override.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_compact_models: Vec<CompactModelRef>,
    /// Legacy single-value form of `default_compact_models` (pre-list
    /// `agent_provider.json` / `provider_list.json`). Read-on-migrate only:
    /// `migrate_legacy_defaults()` folds it into the list and clears it;
    /// never serialized back out.
    #[serde(default, skip_serializing, alias = "default_compact_model")]
    #[allow(deprecated)]
    pub default_compact_model_legacy: Option<CompactModelRef>,
}

impl AgentProviderConfig {
    /// Fold the legacy single `default_compact_model` into the ordered list.
    /// Idempotent; call once after loading from disk / proto.
    pub fn migrate_legacy_defaults(&mut self) {
        if let Some(legacy) = self.default_compact_model_legacy.take()
            && !self.default_compact_models.iter().any(|r| {
                r.provider_id == legacy.provider_id && r.model_id == legacy.model_id
            })
        {
            self.default_compact_models.insert(0, legacy);
        }
    }
}


/// ── Embedding Model types ──
/// Pooling strategy for embedding models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PoolingStrategy {
    /// Use [CLS] token output (BGE models).
    #[default]
    Cls,
    /// Mean pooling over token embeddings weighted by attention_mask (MiniLM).
    Mean,
    /// Use last token output (causal LMs).
    LastToken,
}

/// A file or selection attached to a chat message from the Desktop App.
///
/// The frontend sends an array of these via WebSocket (`attached_items`).
/// ADR-046: this unified enum replaces the prior `document_ids` +
/// `attached_context` fields. The Gateway forwards them through
/// `params_json`, and the Runtime persists each as a system entry
/// (via `AttachmentMeta`) and injects file-path hints into the user
/// message for LLM tool access.
///
/// The frontend is responsible for resolving the absolute path before
/// sending — the Runtime uses `abs_path` directly without path joining.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachedItem {
    /// User-uploaded document (PDF/DOCX/PPTX/XLSX).
    #[serde(rename_all = "camelCase")]
    FileUpload {
        document_id: String,
        filename: String,
        format: String,
        size_bytes: u64,
        /// Frontend-generated client ID (the `clientId` from
        /// `AttachedItem` on the wire). When `Some`, the Runtime writes
        /// the JSONL attachment system entry with this exact ID so the
        /// optimistic overlay in the desktop can be cleared via ID
        /// deduplication. When `None` (legacy / non-optimistic
        /// callers), the Runtime generates a fresh UUID per item.
        ///
        /// `#[serde(default)]` keeps the field optional for backward
        /// compatibility with desktop clients that don't generate
        /// client IDs yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// User-uploaded image (PNG/JPG).
    #[serde(rename_all = "camelCase")]
    ImageUpload {
        document_id: String,
        filename: String,
        format: String,
        size_bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// User-attached workspace file (read-only reference, not copied).
    #[serde(rename_all = "camelCase")]
    AttachedFile {
        abs_path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// User-attached workspace selection with explicit line range.
    #[serde(rename_all = "camelCase")]
    AttachedSelection {
        abs_path: String,
        name: String,
        start_line: u32,
        end_line: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// User-attached workspace folder. Directory contents are NOT copied;
    /// the LLM is expected to walk the path on demand via its own tools.
    #[serde(rename_all = "camelCase")]
    AttachedFolder {
        abs_path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
}

/// Metadata of one file attached to a durable chat message (design 27 §4.2.1).
///
/// The Gateway produces this from the **stored row** — the user service
/// re-reads name / mime / size from the session's `files/` dir when it appends
/// the message, so this is the authoritative description of what the user
/// attached, never a client claim.
///
/// Consumed by the Chat Router in two places that must agree byte for byte:
/// the history projection (a row already in the transcript) and the current
/// turn's own view (the message being routed). Bytes are deliberately absent —
/// the Runtime pulls those over its own attachment route, and only when it
/// actually needs to hand them to an execution session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatAttachmentMeta {
    /// Per-field defaults: a row written by an older (or buggier) producer
    /// must degrade to an incomplete description of the file, never drop the
    /// whole message from the router's history window.
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size: u64,
}

/// Delivery-params key carrying this turn's [`ChatAttachmentMeta`] list.
///
/// It lives here, next to the type, because the key crosses a process
/// boundary that the compiler cannot check: the Gateway writes it and the
/// Chat Router reads it. Runtime-side the reader is deliberately tolerant
/// (a wrong shape costs the files, not the turn), which means a renamed key
/// on one side degrades **silently** to "this turn carried nothing" — the
/// exact blindness the field exists to remove. One constant, two sides.
pub const CHAT_ATTACHMENTS_PARAM: &str = "chat_attachments";

/// Legacy — replaced by `AttachedItem` (ADR-046). Retained for
/// deserialization of old `attached_context` payloads during transition.
/// Will be removed after all frontend clients ship `attached_items`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedContextItem {
    /// Absolute path to the file on the filesystem (e.g. "/home/user/project/src/main.rs")
    pub abs_path: String,
    /// Context type: "file", "directory", or "selection"
    #[serde(rename = "type")]
    pub context_type: String,
    /// Start line (1-based) for selection type, None for whole file
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// End line (1-based) for selection type, None for whole file
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
}

/// Embedding model entry in embedding_models.json.
///
/// Describes a downloadable embedding model with ONNX runtime metadata.
/// Shared between Gateway, acowork-embed, and Desktop App.
///
/// Note: `acowork-embed` carries a parallel definition in
/// `registry::EmbeddingModelEntry`. Keep both in sync — Gateway uses
/// this type to **write** `embedding_models.json`, so dropping a field
/// here would silently strip it from disk on the next save (see
/// bge-m3 `onnx_output_kind` round-trip).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelEntry {
    /// Model identifier (e.g. "bge-small-zh-v1.5")
    pub id: String,
    /// Display name (e.g. "BGE Small Chinese")
    pub name: String,
    /// Human-readable description
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Embedding vector dimension
    pub dimension: usize,
    /// Maximum input token length
    pub max_tokens: usize,
    /// Download size in MB
    pub size_mb: u64,
    /// Supported language codes (e.g. ["zh", "en"])
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// HuggingFace repository (e.g. "onnx-community/bge-small-zh-v1.5-ONNX")
    pub hf_repo: String,
    /// Pooling strategy for this model
    #[serde(default)]
    pub pooling_strategy: PoolingStrategy,
    /// ONNX output shape category — drives the inference glue in
    /// `acowork-embed::model`. `already_pooled` for ONNX exporters
    /// (e.g. `aapot/bge-m3-onnx`) that bake pooling into the graph
    /// and emit `[batch, dim]`; `hidden_states` (default) for raw
    /// `last_hidden_state `[batch, seq_len, dim]`.
    #[serde(default)]
    pub onnx_output_kind: OnnxOutputKind,
    /// Path within the HF repo to the ONNX model file (e.g. "onnx/model.onnx")
    pub onnx_file: String,
    /// Path within the HF repo to the tokenizer (e.g. "tokenizer.json")
    pub tokenizer_file: String,
    /// ONNX model variants (e.g. {"fp32": "onnx/model.onnx", "fp16": "onnx/model_fp16.onnx"})
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onnx_variants: Option<std::collections::HashMap<String, String>>,
    /// External ONNX data files per variant (e.g. {"fp32": ["onnx/model.onnx_data"]}).
    /// MUST stay in sync with `acowork-embed` — dropping this on a gateway
    /// rewrite silently breaks download/load of external-data models.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub external_data_files: std::collections::HashMap<String, Vec<String>>,
    /// Whether the model is bundled with the installation
    #[serde(default)]
    pub bundled: bool,
    /// Whether this is the recommended default model
    #[serde(default)]
    pub recommended: bool,
}

/// ONNX output shape category — mirror of `acowork-embed::registry::OnnxOutputKind`.
/// Duplicated here because the gateway uses this protocol type to **serialize**
/// `embedding_models.json` to disk; the embed crate keeps its own copy for
/// standalone operation. Keep the two definitions byte-identical.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnnxOutputKind {
    /// Raw `last_hidden_state` `[batch, seq_len, hidden_dim]`.
    #[default]
    HiddenStates,
    /// Model already pools (and often normalizes) internally: `[batch, dim]`.
    AlreadyPooled,
}

/// Versioned embedding model list persisted to disk.
///
/// Follows the same pattern as ProviderListFile, McpListFile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelsFile {
    /// Monotonic version counter — bumped on every change
    pub version: u64,
    /// All known embedding model entries
    pub models: Vec<EmbeddingModelEntry>,
}

// ── Cloud Embedding Provider types (参考 LLM ProviderListFile 风格) ──────

/// A single model definition inside a cloud embedding provider catalog.
///
/// Distinct from [`EmbeddingModelEntry`] which describes LOCAL ONNX models
/// (with hf_repo / onnx_file / pooling_strategy). Cloud models only need
/// dimensions + context — there is no file to download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingProviderModelDef {
    /// Model identifier as expected by the provider API
    /// (e.g. "doubao-embedding", "text-embedding-v3", "BAAI/bge-m3")
    pub id: String,
    /// Human-readable display name
    #[serde(default)]
    pub name: String,
    /// Embedding vector dimension — REQUIRED. Switching models with a
    /// different dimension than the current memory store triggers the
    /// existing embedding dimension migration flow.
    pub dimensions: u32,
    /// Maximum input token length (context window)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Supported input modalities (e.g. ["text"])
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding_modalities: Vec<String>,
}

/// A cloud embedding provider catalog entry (mirrors the shape of
/// `offline_embedding_providers.json`). Independent from the LLM
/// [`ProviderListItem`] — embedding providers do not need
/// `tool_call`, `reasoning`, `cost.matrix`, `knowledge`, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingProviderDef {
    /// Stable provider identifier (e.g. "volcengine", "dashscope")
    pub id: String,
    /// Human-readable display name (e.g. "字节火山方舟")
    #[serde(default)]
    pub name: String,
    /// OpenAI-compatible base URL (e.g. "https://ark.cn-beijing.volces.com/api/v3")
    pub api: String,
    /// Protocol marker — always "openai-compatible" for now
    #[serde(default = "default_embedding_protocol")]
    pub protocol: String,
    /// Environment variable names that hold the API key (informational;
    /// the actual key is stored encrypted in Vault)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Optional documentation URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Models offered by this provider, keyed by model id
    pub models: std::collections::HashMap<String, EmbeddingProviderModelDef>,
    /// True if this entry was added by the user (persisted to
    /// `data_dir/user_embedding_providers.json`). False for bundled
    /// catalog entries from `offline_embedding_providers.json`.
    /// Set by the merged loader; defaults to false on deserialize so
    /// legacy catalog files keep working.
    #[serde(default)]
    pub custom: bool,
}

fn default_embedding_protocol() -> String {
    "openai-compatible".to_string()
}

/// Versioned cloud embedding provider catalog loaded from
/// `assets/offline_embedding_providers.json` at startup.
///
/// Independent from [`EmbeddingModelsFile`] (LOCAL ONNX models). Both
/// lists coexist — the UI shows them as two distinct sections.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmbeddingProviderCatalog {
    /// Monotonic version counter
    pub version: u64,
    /// All known cloud embedding providers, keyed by provider id
    pub providers: std::collections::HashMap<String, EmbeddingProviderDef>,
}

/// One user-added cloud embedding provider, persisted to
/// `data_dir/user_embedding_providers.json`.
///
/// Mirrors the catalog entry shape but stripped of fields the user is
/// unlikely to fill in correctly (`env`, `doc`). The user MUST provide
/// `api` (OpenAI-compatible base URL) and at least one model entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEmbeddingProviderEntry {
    /// Stable provider id (e.g. "custom-my-proxy"). Must be unique
    /// across user-added entries AND must not collide with any id in
    /// the bundled [`EmbeddingProviderCatalog`].
    pub id: String,
    /// Human-readable display name (e.g. "My GPT Proxy")
    #[serde(default)]
    pub name: String,
    /// OpenAI-compatible base URL (e.g. "https://api.example.com/v1")
    pub api: String,
    /// Models offered by this provider, keyed by model id. Each model
    /// MUST have `dimensions > 0`.
    pub models: std::collections::HashMap<String, EmbeddingProviderModelDef>,
}

/// Top-level wrapper for `data_dir/user_embedding_providers.json`.
///
/// Versioned for future schema migrations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserEmbeddingProviderList {
    pub version: u64,
    pub providers: Vec<UserEmbeddingProviderEntry>,
}

/// The user's currently-active cloud embedding selection.
///
/// Persisted to `data_dir/active_embedding_provider.json` (user-writable).
/// `None` (file missing) means the user has not selected a cloud
/// embedding provider — Runtime continues to use the local ONNX service.
///
/// When set, the Gateway publishes this selection via `AgentHello`
/// (see [`crate::protocol::HelloPayload::embed_provider_id`] and
/// [`crate::protocol::HelloPayload::embed_api_key`]) so Runtime can
/// construct a `RemoteEmbeddingProvider` against the cloud endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActiveEmbeddingProvider {
    /// Provider id (e.g. "volcengine"). Must match a key in the catalog.
    pub provider_id: String,
    /// Model id within the provider (e.g. "doubao-embedding"). Must
    /// match a model id under that provider in the catalog.
    pub model_id: String,
    /// Embedding dimension of the selected model. Copied from the
    /// catalog at selection time so Runtime doesn't need the catalog
    /// to construct the provider.
    pub dimension: u32,
    /// Base URL copied from the catalog at selection time. Frozen so
    /// a later catalog change cannot silently redirect Runtime's
    /// outbound traffic.
    pub base_url: String,
    /// Whether an API key is currently stored in Vault for this
    /// provider. `false` means Runtime should treat the provider as
    /// unavailable (the key is never sent over the wire in this case).
    #[serde(default)]
    pub has_api_key: bool,
    /// When this selection was made (ISO 8601, UTC)
    #[serde(default)]
    pub selected_at: String,
}

/// ── User Identity types ──
/// A single user's identity profile.
///
/// Persisted in `user_profiles.json` in Gateway's data directory.
/// Each profile is keyed by a UUID `user_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique user identifier (UUID v4)
    pub user_id: String,
    /// Display name — what the user wants to be called
    pub display_name: String,
    /// Preferred language (BCP 47, e.g. "zh-CN", "en-US")
    pub language: String,
    /// Timezone (IANA, e.g. "Asia/Shanghai", "UTC")
    pub timezone: String,
    /// City (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// Country (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Occupation / domain (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,
    /// Custom avatar path (relative to Gateway data_dir, e.g. "assets/avatar-01.png")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Builtin avatar icon ID (e.g. "icon-05")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    /// Communication style preference (optional)
    /// e.g. "concise", "detailed", "casual"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communication_style: Option<String>,
    /// Free-form extension fields (optional)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, String>,
    /// When this profile was created (ISO 8601)
    pub created_at: String,
    /// When this profile was last updated (ISO 8601)
    pub updated_at: String,
    /// Whether this user is currently the active / online user.
    /// Only the active user's profile is pushed to Runtime.
    #[serde(default)]
    pub is_active: bool,
}

/// Versioned user profile list persisted to disk.
///
/// Follows the same pattern as ProviderListFile, McpListFile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfileListFile {
    /// Monotonic version counter — bumped on every create/update/delete
    pub version: u64,
    /// All known user profiles (historical + current)
    pub users: Vec<UserProfile>,
}

/// Context usage info reported by Runtime to Gateway after each LLM call.
/// Forwarded to Desktop App via WebSocket for UI display.
///
/// Per-turn fields (`input_tokens`, `output_tokens`, `total_tokens`) reflect the
/// most recent LLM call only. Cumulative session fields
/// (`total_input_tokens`, `total_output_tokens`) accumulate across all LLM
/// calls in the session — they are sourced from `SessionTokens` and are
/// `None` until the first LLM call has been recorded.
///
/// Cumulative agent fields (`agent_total_input_tokens`,
/// `agent_total_output_tokens`) aggregate across **every LLM call made by
/// this Runtime process for this agent** — they are sourced from
/// [`crate::protocol`]'s agent-scoped counters and are populated on every
/// push (live data source). The same figures also ride along in the
/// `GET /api/agents/:id/sessions` response as a fallback for the case
/// where the Runtime has just started and no LLM call has happened yet
/// (in which case the counter is bootstrapped from the on-disk scan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsageInfo {
    /// Context window limit (from model capabilities)
    pub context_window: u64,
    /// Current input tokens used (prompt_tokens from API response, last turn)
    pub input_tokens: u64,
    /// Current output tokens generated (completion_tokens, last turn)
    pub output_tokens: u64,
    /// Total tokens of the last turn (input + output)
    pub total_tokens: u64,
    /// Max input tokens (from models.dev limit.input, if available)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Usable context space (context_window - max_output_tokens, or max_input_tokens - reserved)
    pub usable_context: u64,
    /// Usage percentage (0-100)
    pub usage_percent: u8,
    /// Cumulative input tokens across all LLM calls in this session.
    /// `None` until the first LLM call has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    /// Cumulative output tokens across all LLM calls in this session.
    /// `None` until the first LLM call has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    /// ADR-028: cumulative input tokens across every LLM call made by this
    /// Runtime process for this agent (live data source from AgentCore).
    /// `None` if the Runtime is older than ADR-028 and never sets it; the
    /// session-list response carries the fallback copy in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_input_tokens: Option<u64>,
    /// ADR-028: cumulative output tokens across every LLM call made by this
    /// Runtime process for this agent. See [`Self::agent_total_input_tokens`]
    /// for semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_output_tokens: Option<u64>,
    // ── ADR-066: prompt cache tokens (Provider-reported) ──────────────
    /// Prompt tokens served from cache on the most recent LLM call
    /// (OpenAI `cached_tokens` / Anthropic `cache_read_input_tokens`).
    /// `None` if the Provider does not report cache hits, or the call
    /// is too old to be relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written to cache on the most recent LLM call
    /// (Anthropic `cache_creation_input_tokens`; OpenAI has no concept
    /// → always `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Cumulative cache read tokens across all session LLM calls.
    /// `None` until the first LLM call has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cache_read_tokens: Option<u64>,
    /// Cumulative cache write tokens across all session LLM calls.
    /// `None` until the first LLM call has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cache_write_tokens: Option<u64>,
    /// ADR-066: cumulative cache read tokens across every LLM call
    /// made by this Runtime process for this agent (live data source
    /// from `AgentCore`).  Mirrors [`Self::agent_total_input_tokens`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_cache_read_tokens: Option<u64>,
    /// ADR-066: cumulative cache write tokens across every LLM call
    /// made by this Runtime process for this agent.  Mirrors
    /// [`Self::agent_total_cache_read_tokens`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_cache_write_tokens: Option<u64>,
    /// ADR-067: per-section byte sizes of the most recent `ChatRequest`
    /// assembled by `ContextBuilder::build()`. Empty/`None` until the
    /// first LLM call after enabling this ADR's wiring in
    /// `process_llm_response_usage`. Powers the input-box context-usage
    /// popup's 5-category breakdown (system / tools / messages /
    /// connectors / skills) **without** requiring DevMode to be on —
    /// `DebugObserverSlot::Production` makes `on_context_built` a no-op,
    /// but the per-section byte sizes are now computed in a separate,
    /// cheap path that runs unconditionally.
    ///
    /// Only `key` + `size_bytes` are exposed here; full per-section
    /// metadata (token estimate, SHA-256 hash, request params, lazy
    /// content) stays on the DevMode-only `DebugContextBuiltEvent`
    /// path. `skip_serializing_if = "Option::is_none"` keeps the wire
    /// shape compatible with pre-ADR-067 Runtimes that never populate
    /// the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<ContextUsageSection>>,
    /// 1-based per-session LLM-call counter (monotonic for the session's
    /// lifetime, does NOT reset when the user clicks Continue after
    /// `max_iterations`). Per-session metadata persisted in the session's
    /// meta.json — a resumed/historical session shows the same count and
    /// a Continue keeps accumulating. Populated only on `ContextUsage`
    /// events tied to a real LLM response; absent on context-window
    /// config changes / post-compaction pushes that don't reflect a new
    /// call. `None` on Runtimes pre-dating this field — the frontend
    /// falls back to assistant-message count in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    /// Session-level mean output speed in tokens/second.
    ///
    /// Per call it is `completion_tokens / streaming_seconds`, timed from
    /// the first output token; the session figure is the unweighted mean
    /// over the calls that produced a speed measurement (calls whose
    /// generation window was too short to be credible are skipped, so this
    /// can average fewer calls than [`Self::avg_ttft_ms`]). `None` until the
    /// first usable measurement (and on pre-existing sessions) — the
    /// frontend renders "—" rather than a fake 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_tps: Option<f64>,
    /// Session-level mean time-to-first-token in milliseconds — the wait
    /// from "LLM call started" to the first output event. Sampled
    /// independently of [`Self::avg_tps`], so the two means can cover
    /// different numbers of calls. Same `None` semantics as
    /// [`Self::avg_tps`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ttft_ms: Option<f64>,
}

/// One entry in [`ContextUsageInfo::sections`].
///
/// Mirrors the key + size_bytes pair of [`crate::mqtt_proto::SectionMeta`]
/// (proto `mqtt_payload.proto:1168`) but trimmed to the two fields the
/// runtime observability path actually needs. The Debug Panel reads the
/// full [`crate::mqtt_proto::SectionMeta`] from `DebugContextBuiltEvent`
/// separately; this struct intentionally does not re-export
/// `token_estimate` or `hash` to keep the always-on emission path light
/// (no SHA-256, no per-section token counting).
///
/// Section keys (`system_prompt`, `identity_context`, `workspace_context`,
/// `retrieved_memory`, `skill_instructions`, `environment`,
/// `workspace_prompt_file`, `tool_definitions`, `connector_tool_definitions`,
/// `messages`, `ambiguous_confirmation_hint`, `abstention_prompt`)
/// match the keys surfaced by `compute_section_sizes` in
/// `acowork-runtime/src/agent/context.rs` (ADR-067). `tool_definitions` are
/// Runtime built-ins; `connector_tool_definitions` are MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsageSection {
    /// Section key, e.g. `"system_prompt"`, `"messages"`. Stable across
    /// Runtime versions; the frontend (`contextUsageBreakdown.ts`)
    /// groups by this string into the 5 popup categories.
    pub key: String,
    /// Byte size of the assembled section content — exact (UTF-8
    /// `.len()`), not a heuristic. The frontend scales this by the
    /// billed `usage_percent` to compute per-row percentages.
    pub size_bytes: u64,
}

/// LLM API protocol type, derived from models.dev npm field.
///
/// Used by Gateway to tell Runtime which protocol adapter to use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    /// Anthropic Messages API (used by providers with npm: @ai-sdk/anthropic)
    Anthropic,
    /// Google Gemini API (used by providers with npm: @ai-sdk/google)
    Google,
    /// Ollama native API
    Ollama,
    /// OpenAI-compatible Chat Completions API (default for all other providers)
    #[default]
    #[serde(alias = "openai-compatible")]
    OpenAI,
}

/// Convert MQTT protobuf `LlmProtocol` (as i32) to domain `ProtocolType`.
///
/// Reverse of `map_protocol_type` in the Gateway's
/// `mqtt/global_resources_publisher.rs`. Used by the Runtime when
/// converting `ProviderRef` (protobuf) to `ProviderListItem` (domain)
/// so that non-OpenAI providers (Anthropic, Google, Ollama) retain
/// their correct protocol type through the MQTT sync path.
pub fn llm_protocol_to_protocol_type(proto: i32) -> ProtocolType {
    match crate::mqtt_proto::LlmProtocol::try_from(proto) {
        Ok(crate::mqtt_proto::LlmProtocol::Anthropic) => ProtocolType::Anthropic,
        Ok(crate::mqtt_proto::LlmProtocol::Google) => ProtocolType::Google,
        Ok(crate::mqtt_proto::LlmProtocol::Ollama) => ProtocolType::Ollama,
        // Unspecified or unknown -> default to OpenAI-compatible
        _ => ProtocolType::OpenAI,
    }
}

impl std::str::FromStr for ProtocolType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(ProtocolType::Anthropic),
            "google" | "gemini" => Ok(ProtocolType::Google),
            "ollama" => Ok(ProtocolType::Ollama),
            "openai" | "openai-compatible" => Ok(ProtocolType::OpenAI),
            _ => Err(format!("Unknown protocol type: {}", s)),
        }
    }
}

/// Gateway Service API request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayRequest {
    /// Request an API key for a specific provider
    KeyRelease { provider: String },
    /// Send an Intent to another Agent
    IntentSend {
        target: String,
        action: String,
        params: Value,
        #[serde(rename = "async")]
        async_: bool,
    },
    /// Query remaining budget for a provider
    BudgetQuery { provider: String },
    /// Report token usage
    UsageReport(UsageReport),
    /// Acquire a rate limit token
    RateAcquire { provider: String },
    /// Query capabilities for a specific agent or all agents
    CapabilityQuery {
        /// Optional agent ID filter (None = all agents)
        agent_id: Option<String>,
    },
    /// Register a cron entry (S3.4, S5.8 enhanced)
    CronRegister {
        /// Agent ID that owns this cron entry
        agent_id: String,
        /// Cron schedule expression (5-field)
        schedule: String,
        /// Action to fire when the schedule triggers
        action: String,
        /// Params to include in the IntentReceived
        params: Value,
        /// Timezone for schedule interpretation (None = UTC, Some("local") = system local)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// Max retry count on failure (0 = no retry, default 0)
        #[serde(default)]
        retry_count: u32,
        /// Retry backoff interval in seconds (default 60)
        #[serde(default = "default_retry_interval")]
        retry_interval_secs: u64,
        /// Max total executions (None = unlimited)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_runs: Option<u32>,
        /// Expiry timestamp in Unix millis (None = never expires)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
    },
    /// Unregister a cron entry (S3.4)
    CronUnregister {
        /// Cron entry ID to remove
        cron_id: String,
    },
    /// List cron entries for the calling agent (S3.4)
    CronList {},
    /// Runtime reports context usage to Gateway (after each LLM call)
    ContextUsageReport {
        agent_id: String,
        context: ContextUsageInfo,
        session_id: String,
    },
    /// Agent registration — first message sent after gRPC connection
    /// Runtime sends this to identify itself to the Gateway
    AgentHello {
        /// The agent's reverse-domain identifier (package identity)
        agent_id: String,
        /// ADR-073: instance identity (UUID v4). The Runtime receives it
        /// from the Node control plane at spawn time (`--agent-instance-id`)
        /// and reports it here; the Gateway uses it to bind the running
        /// registry entry.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        instance_id: String,
        /// ADR-073: current location (node hosting this Runtime). `"local"`
        /// for standalone / Gateway-spawned runtimes.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        node_id: String,
        /// The agent's version
        version: String,
        /// Connection role — "main" for the primary gRPC connection,
        /// "chunk-relay" for the streaming chunk relay connection.
        /// The Gateway uses this to route IntentReceived only to "main" connections.
        /// Defaults to "main" when absent (backward compatible).
        #[serde(default = "default_connection_role")]
        connection_role: String,
        /// Runtime's cached provider list version (0 = never synced)
        #[serde(default)]
        provider_list_version: u64,
        /// Runtime's cached MCP server list version (0 = never synced)
        #[serde(default)]
        mcp_list_version: u64,
        /// Runtime's cached user profile version (0 = never synced)
        #[serde(default)]
        user_profile_version: u64,
        /// ADR-017: Runtime's current avatar config from agent_config.json.
        /// Gateway uses this to sync its avatar cache on agent restart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_avatar: Option<String>,
    },
    /// List sessions request (S1.14)
    ///
    /// Runtime sends this to Gateway to request a list of
    /// conversation sessions. Gateway responds with SessionList.
    ListSessions,
    /// Get session messages request (S1.14)
    ///
    /// Runtime sends this to Gateway to request paginated messages
    /// for a specific session. Gateway responds with SessionMessages.
    GetSessionMessages {
        /// Session identifier to query
        session_id: String,
        /// Cursor for pagination (message ID of the last seen message)
        #[serde(skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        /// Maximum number of messages to return
        limit: u32,
        /// Pagination direction: "forward" or "backward"
        direction: String,
    },
    /// Create session request (S1.14)
    ///
    /// Runtime sends this to Gateway to signal that a new
    /// conversation session has been created. Gateway responds
    /// with SessionCreated.
    CreateSession,
    /// Delete session request
    ///
    /// Gateway sends this to Runtime to delete a conversation
    /// session. Runtime deletes the JSONL file and responds
    /// with SessionDeleted.
    DeleteSession {
        /// Session identifier to delete
        session_id: String,
    },
    /// Config snapshot response (Runtime → Gateway)
    ///
    /// Sent by Runtime in response to GatewayResponse::QueryConfig.
    /// Carries the current per-agent configuration stored in
    /// workspace/config/agent_config.json and agent_model.json.
    ConfigSnapshot {
        /// Correlating request ID from QueryConfig
        request_id: String,
        /// Current model name (from workspace/config/agent_model.json)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Current provider name (from workspace/config/agent_model.json)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Max output tokens override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u64>,
        /// Max iterations override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
        /// Temperature override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        /// System prompt override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt_override: Option<String>,
        /// Shell approval threshold
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_approval_threshold: Option<String>,
        /// ADR-017: Avatar config from agent_config.json
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_avatar: Option<String>,
        /// ADR-024: max sessions limit (from agent_config.json)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_sessions: Option<usize>,
        /// ADR-026: Per-agent context window cap in tokens (0 = no limit).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        /// Per-agent approval timeout in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_timeout_secs: Option<u64>,
        /// ADR-029: Builtin tools enabled list (JSON-serialized Vec<String>).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_tools_enabled_json: Option<String>,
        /// ADR-029: Full builtin tools list with enabled flags (JSON-serialized Vec<AgentToolEntry>).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_tools_all_json: Option<String>,
    },
    /// Update workspace config snapshot (Runtime → Gateway).
    ///
    /// Sent by Runtime after AgentHello to populate Gateway's in-memory cache
    /// with the current workspace config so that the Gateway HTTP API can serve
    /// list_workspaces and handle CRUD requests without persisting workspace data.
    /// Gateway caches this in RunningAgentInfo and uses it for HTTP responses;
    /// it is NOT persisted to disk (Gateway is pure pass-through for workspace config).
    UpdateWorkspaceConfig {
        /// Full workspace config JSON (same format as .agent_workspaces.json)
        config_json: String,
    },
}

/// Gateway Service API response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum GatewayResponse {
    /// AgentHello response — confirms registration and delivers all
    /// handshake-time configuration in a single atomic message.
    ///
    /// Bundles LLM config, workspace context, and runtime overrides
    /// so the Runtime does not need to selectively read from the shared
    /// push channel during startup (eliminating the message-loss race).
    AgentHelloResult {
        /// Whether the registration was successful
        success: bool,
        /// Error message if registration failed
        error: Option<String>,

        // ── Global Resource Lists (version-driven diff sync) ──
        /// Provider list with full models + capabilities.
        /// Only included when provider_list_version in AgentHello < Gateway's current version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_list: Option<Vec<ProviderListItem>>,
        /// Gateway's current provider list version
        #[serde(default)]
        provider_list_version: u64,

        /// MCP server list.
        /// Only included when mcp_list_version in AgentHello < Gateway's current version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_list: Option<Vec<McpListItem>>,
        /// Gateway's current MCP list version
        #[serde(default)]
        mcp_list_version: u64,

        // ── Key Vaults (always delivered in full, Runtime memory-only) ──
        /// Provider API keys — NEVER persisted to workspace disk by Runtime.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_key_vault: Vec<ProviderKeyEntry>,

        /// MCP server keys/tokens — NEVER persisted to workspace disk by Runtime.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mcp_key_vault: Vec<McpKeyEntry>,

        // ── User Identity ──
        /// Active user profile. Only included when user_profile_version in
        /// AgentHello request is stale. None when no active user exists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_identity: Option<UserProfile>,
        /// Gateway's current user profile list version
        #[serde(default)]
        user_profile_version: u64,

        // ── Embedding Service ──
        /// Embedding service endpoint URL for ONNX local inference.
        /// Runtime uses this as the primary embedding provider.
        /// Example: "http://127.0.0.1:18080/v1"
        /// None when the embedding service is not running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed_endpoint: Option<String>,
        /// Active embedding model ID (e.g. "bge-small-zh-v1.5").
        /// None when the embedding service is not running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed_model_id: Option<String>,
        /// Embedding dimension of the active model (e.g. 512).
        /// None when the embedding service is not running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed_dimension: Option<usize>,

        // ── Cloud Embedding Provider (optional, 参考 LLM provider 模式) ──
        /// Active cloud embedding provider id (e.g. "volcengine",
        /// "dashscope", "siliconflow"). `None` when the user has not
        /// selected a cloud provider — Runtime continues to use the
        /// local ONNX service via `embed_endpoint`.
        ///
        /// When present, Runtime should construct a
        /// `RemoteEmbeddingProvider` against `embed_endpoint` (which
        /// will carry the cloud base URL instead of the local one)
        /// and authenticate with `embed_api_key`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed_provider_id: Option<String>,
        /// Decrypted API key for `embed_provider_id`. NEVER persisted to
        /// disk by Runtime — same security contract as the chat
        /// provider keys in the same message.
        ///
        /// Empty when no key is configured (Runtime should treat the
        /// provider as unavailable in that case).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed_api_key: Option<String>,

        // ── LSP Relay ──
        /// LSP Relay HTTP endpoint (e.g. "http://127.0.0.1:19878").
        /// None when the LSP Relay is not running.
        /// Used by the codebase tool to connect directly to the relay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lsp_relay_endpoint: Option<String>,
    },
    /// API key release result
    KeyReleaseResult {
        /// The released API key on success
        api_key: Option<String>,
        /// Error message on failure (e.g. "unauthenticated session", vault error)
        error: Option<String>,
    },
    /// Intent delivery confirmation
    IntentDelivered { message_id: String },
    /// Intent received from another Agent
    IntentReceived {
        from: String,
        action: String,
        params: Value,
        /// Skill command selected by the user (e.g. "/commit", "/review-pr").
        /// When present, the Runtime knows the user explicitly chose a skill.
        /// None for normal chat messages or non-skill intents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// Budget information
    BudgetInfo {
        remaining_tokens: u64,
        remaining_cost_usd: f64,
    },
    /// Usage report acknowledgment
    UsageReportAck {},
    /// Context usage report acknowledgment
    ContextUsageAck {},
    /// Rate limit token
    RateToken {
        granted: bool,
        retry_after_ms: Option<u64>,
    },
    /// Provider list update (Gateway → Runtime, hot-push)
    ///
    /// Replaces the old LLMConfigDelivery with a full-list push of all
    /// providers + models + capabilities. Sent when a provider is added,
    /// removed, or has its model list or API key changed.
    ProviderListUpdate {
        /// Full provider list (replaces any previous state)
        provider_list: Vec<ProviderListItem>,
        /// Monotonic version for diff-sync at next AgentHello
        provider_list_version: u64,
        /// Full provider key vault (in-memory only, never persisted)
        provider_key_vault: Vec<ProviderKeyEntry>,
    },
    /// User profile update (Gateway → Runtime, hot push)
    ///
    /// Pushed to all running agents when the user profile is created,
    /// updated, or when the active user is switched.
    UserProfileUpdate {
        /// Updated active user profile (None = no active user)
        user_identity: Option<UserProfile>,
        /// New version
        version: u64,
    },
    /// Capability overview (handshake step ⑤ and CapabilityQuery response)
    CapabilityOverview {
        /// Map of agent_id → list of action names
        capabilities: std::collections::HashMap<String, Vec<String>>,
    },
    /// Capability update (incremental push on install/uninstall/update)
    CapabilityUpdate {
        /// Agent that was updated
        agent_id: String,
        /// New/updated actions
        actions: Vec<String>,
        /// Whether this is a removal
        removed: bool,
    },
    /// Cron registration result (S3.4)
    CronRegisterResult {
        /// Cron entry ID on success
        cron_id: Option<String>,
        /// Error message on failure
        error: Option<String>,
    },
    /// Cron unregistration result (S3.4)
    CronUnregisterResult {
        /// Whether the entry was found and removed
        removed: bool,
    },
    /// Cron list result (S3.4)
    CronListResult {
        /// List of cron entries
        entries: Vec<CronEntryInfo>,
    },
    /// Workspace config update (Gateway → Runtime, push)
    ///
    /// Pushes the full workspace config JSON to the Agent Runtime when
    /// the user modifies workspace directories via the HTTP API.
    /// The Runtime persists this to .agent_workspaces.json, reloads its
    /// WorkspaceResolver, and self-formats the LLM context text.
    /// Gateway does NOT persist workspace config — it is a pure pass-through.
    WorkspaceConfigUpdate {
        /// Full workspace config JSON (same format as .agent_workspaces.json)
        config_json: String,
    },
    /// Set the current workspace for a specific session (Gateway → Runtime).
    ///
    /// Unlike WorkspaceConfigUpdate (which pushes the full list),
    /// this targets a single session's working directory selection.
    /// `workspace_id` of "__agent_home__" means the agent's install directory.
    SetSessionWorkspace {
        /// Target session ID
        session_id: String,
        /// Workspace ID to activate, or "__agent_home__" for agent home
        workspace_id: String,
    },
    /// Iteration limit reached — agent loop paused, awaiting user decision.
    ///
    /// The Runtime pushes this when `iteration >= max_iterations`.
    /// The Gateway relays it to the Desktop App so the user can choose
    /// to continue (which resets the iteration counter) or stop.
    IterationLimitPaused {
        /// Current iteration count when the limit was hit
        iteration: u32,
        /// Configured max_iterations limit
        max_iterations: u32,
        /// Human-readable message
        message: String,
    },
    /// Session list result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::ListSessions.
    /// Carries the list of session summaries.
    SessionList {
        /// List of session info DTOs
        sessions: Vec<SessionInfoDto>,
    },
    /// Session messages result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::GetSessionMessages.
    /// Carries a paginated page of conversation messages.
    SessionMessages {
        /// Messages in the current page
        messages: Vec<ConversationEntryDto>,
        /// Cursor for the next page (message ID)
        cursor: Option<String>,
        /// Whether more messages exist beyond this page
        has_more: bool,
    },
    /// Session created result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::CreateSession.
    SessionCreated {
        /// The newly created session identifier
        session_id: String,
    },
    /// Session deleted result
    ///
    /// Sent by Runtime in response to GatewayRequest::DeleteSession.
    SessionDeleted {
        /// Whether the session was successfully deleted
        success: bool,
        /// Error message if deletion failed
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Log level update (Gateway → Runtime, push)
    ///
    /// Gateway pushes a new log level when the user changes it in Settings.
    /// The Runtime applies the change to its tracing subscriber via reload::Handle.
    LogLevelUpdate {
        /// New log level string (e.g. "trace", "debug", "info", "warn", "error")
        log_level: String,
    },
    /// Log rotation request (Gateway → Runtime, push)
    ///
    /// Gateway pushes this when the user triggers log cleanup in Settings.
    /// The Runtime must:
    ///   1. Delete all *.log files in its workspace/logs/ directory
    ///   2. Force-rotate to create a fresh log file for subsequent writes
    LogRotate,
    /// Log file count update (Gateway → Runtime, push)
    ///
    /// Gateway pushes the new maximum log file count when the user changes
    /// it in Settings. The Runtime updates its SizeRollingFileAppender
    /// and immediately enforces the limit by deleting the oldest files.
    LogFileCountUpdate {
        /// New maximum number of log files to keep (0 = unlimited)
        log_file_count: u64,
    },
    /// Runtime configuration update (Gateway → Runtime, push)
    ///
    /// Gateway pushes per-agent config overrides to the Runtime.
    /// Sent at two times:
    ///   A) After AgentHello handshake (initial config delivery)
    ///   B) When the user updates config via PUT /api/agents/{id}/config
    ///
    /// All fields are optional — None means "keep current value".
    RuntimeConfigUpdate {
        /// Max output tokens per request (0 = use global default)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u64>,
        /// Max LLM iterations per run (0 = use global default).
        /// Controls the total number of LLM turns in a single Agent loop.
        /// When exceeded, the Runtime pushes `IterationLimitPaused`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
        /// LLM temperature override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        /// System prompt override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt_override: Option<String>,
        /// Shell command approval threshold.
        /// Controls which risk levels require user confirmation before execution.
        /// "low" | "medium" (default) | "high" | "auto_approve" (legacy "never" accepted)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_approval_threshold: Option<String>,
        /// MCP server configurations.
        /// Some(vec![]) means no MCP servers; None means keep current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_servers: Option<Vec<McpServerConfigDef>>,
        /// Model name override (e.g. "gpt-4o", "claude-sonnet-4-20250514").
        /// When set, the Runtime switches to this model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Provider name override (e.g. "openai", "anthropic").
        /// When set together with `model`, the Runtime switches provider and model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// ADR-017: Custom avatar path override.
        /// Some("path") = set, Some("") = clear, None = don't change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<String>,
        /// ADR-017: Builtin avatar icon ID override.
        /// Some("icon-05") = set, Some("") = clear, None = don't change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_avatar: Option<String>,
        /// ADR-024: max sessions limit per-agent.
        /// Some(n) = set to n, None = don't change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_sessions: Option<usize>,
        /// ADR-026: Per-agent context window cap in tokens (0 = no limit).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        /// Per-agent approval timeout in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_timeout_secs: Option<u64>,
        /// ADR-029: Builtin tools enabled set.
        /// Some(vec![]) means all builtin tools disabled.
        /// None means don't change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builtin_tools_enabled: Option<Vec<String>>,
    },
    /// Query config request (Gateway → Runtime)
    ///
    /// Gateway sends this to the Runtime to query the current per-agent
    /// configuration stored in workspace/config/. The Runtime responds
    /// with GatewayRequest::ConfigSnapshot.
    QueryConfig {
        /// Request ID for correlating the response
        request_id: String,
    },
    /// Unknown or unrecognized message from Gateway.
    ///
    /// Returned when proto_to_gateway_response encounters an empty payload
    /// or an unrecognized variant. This is distinct from normal business
    /// messages so the agent loop can log and discard it without confusing
    /// it with a legitimate UsageReportAck or other response.
    Unknown {},

    /// Enable debug mode on a running agent (Gateway → Runtime, push).
    ///
    /// Gateway pushes this when the user clicks "Restart in Debug" on a
    /// running agent. The Runtime fires urgent_interrupt to cancel any
    /// in-flight tools and LLM calls, registers the Debug HTTP routes and
    /// spawns the MQTT debug events publisher (ADR-048), and injects the
    /// DebugController and notify handles into the shared AgentCore. If
    /// the agent loop is idle, the interrupt step is skipped and debug
    /// mode is initialized directly.
    EnableDebugMode {
        /// Debug Protocol port hint (allocated by Gateway; no longer bound
        /// as a listener after ADR-048, kept for API stability).
        debug_port: u32,
    },
    /// Start embedding dimension migration (Gateway → Runtime).
    ///
    /// Sent by Gateway when the user confirms migration for a specific agent.
    /// The Runtime must re-embed all memory nodes and rebuild HNSW indexes.
    /// Progress is reported via GatewayRequest::MigrationProgress.
    MigrationStart {
        /// Unique request ID for correlating progress/completion messages
        request_id: String,
        /// Embedding service endpoint URL
        embed_endpoint: String,
        /// Active embedding model ID
        embed_model_id: String,
        /// Embedding dimension of the new model
        embed_dimension: usize,
    },
    /// Sidecar endpoint update (Gateway → Runtime, push).
    ///
    /// Pushed whenever a Gateway-managed sidecar (`lsp_relay`, `embed`,
    /// future sidecars) transitions to ready, changes its endpoint, or
    /// becomes unavailable. The Runtime reacts by:
    ///   - `lsp_relay`: registering or disabling the `codebase` builtin tool
    ///   - `embed`:     rebuilding the `FallbackEmbeddingProvider` chain
    ///
    /// Empty `endpoint` signals "sidecar is down" — the Runtime should
    /// disable dependent features rather than try to connect.
    ///
    /// This message is the canonical (and only) channel for sidecar state
    /// updates. As of ADR-030 C4, the legacy `RuntimeConfigUpdate.embed_config_json`
    /// JSON field and the `EmbeddingConfigUpdate` variant have been removed.
    SidecarEndpointUpdate {
        /// Which sidecar this update is for.
        sidecar: SidecarKind,
        /// HTTP URL the Runtime should use. Empty string = sidecar unavailable.
        endpoint: String,
        /// Sidecar-specific metadata. Schema depends on `sidecar`:
        ///   - `LspRelay`: `""` (no extra fields today)
        ///   - `Embed`:    `{"model_id":"bge-small-zh-v1.5","dimension":512}`
        ///
        /// Empty string if no metadata applies.
        spec_json: String,
    },
}

/// Identifies a Gateway-managed sidecar process. The Runtime uses this to
/// route a `SidecarEndpointUpdate` to the correct subsystem.
///
/// Mirrors the `SidecarKind` enum in `gateway_ipc.proto`. Adding a new
/// sidecar requires updating both this enum and the proto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarKind {
    /// Unspecified — reserved for forward-compat. Should not appear in
    /// production traffic; treated as "unknown" by the Runtime.
    Unspecified,
    /// `acowork-lsp-relay` — provides JSON-RPC LSP relay used by the
    /// Runtime's `codebase` builtin tool.
    LspRelay,
    /// `acowork-embed` — local ONNX embedding HTTP service. The Runtime
    /// builds a `FallbackEmbeddingProvider` chain from the active model
    /// id and dimension provided in the push payload.
    Embed,
}

/// Deployment scope of a sidecar (ADR-055 §6.7 Sidecar Scope model).
///
/// Determines where the sidecar process is hosted and which retained
/// MQTT topic carries its endpoint:
/// - `Global` — one instance on the Gateway machine, endpoint distributed
///   via the `acowork/global/*` retained topics (e.g. embed).
/// - `NodeLocal` — one instance per Node (it must run on the same machine
///   as the agent workspace, e.g. `root_uri = file://{workspace_root}`),
///   endpoint distributed via `acowork/nodes/{node_id}/lsps` retained
///   (e.g. LSP relay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarScope {
    /// Hosted by the Gateway machine.
    Global,
    /// Hosted by each Node machine, one per node.
    NodeLocal,
}

impl SidecarKind {
    /// Canonical string identifier used in the proto and over the wire
    /// (the proto enum value). Stable across versions; do not rename.
    pub fn as_str(&self) -> &'static str {
        match self {
            SidecarKind::Unspecified => "unspecified",
            SidecarKind::LspRelay => "lsp_relay",
            SidecarKind::Embed => "embed",
        }
    }

    /// Deployment scope of this sidecar (ADR-055 §6.7).
    ///
    /// `Unspecified` falls back to `Global` for forward-compat: unknown
    /// sidecars were historically Gateway-managed.
    pub fn scope(&self) -> SidecarScope {
        match self {
            SidecarKind::Unspecified => SidecarScope::Global,
            SidecarKind::LspRelay => SidecarScope::NodeLocal,
            SidecarKind::Embed => SidecarScope::Global,
        }
    }
}

impl std::str::FromStr for SidecarKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unspecified" => Ok(SidecarKind::Unspecified),
            "lsp_relay" => Ok(SidecarKind::LspRelay),
            "embed" => Ok(SidecarKind::Embed),
            other => Err(format!("Unknown SidecarKind: {other}")),
        }
    }
}

/// MCP server configuration definition (transport-agnostic, shared between Gateway and Runtime).
///
/// This is the wire format for MCP server configs. Both Gateway and Runtime
/// convert to/from their own internal representations as needed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfigDef {
    pub name: String,
    /// Human-readable alias shown in the UI. Purely cosmetic — agents and the
    /// wire protocol always reference a server by `name` (the immutable
    /// identity key). Lets a machine-generated name like
    /// `market-cmgjmcp00075722` be relabeled "CMJ Market" without a re-key.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub transport: McpTransportDef,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// Optional install context (ADR-072). Present for preset-added servers;
    /// absent (None) for user-custom servers. Since ADR-090 the install
    /// command is executed by the Runtime (Agent process) on first enable,
    /// so this field is delivered to the Agent via `McpRef.install_spec_json`.
    #[serde(default)]
    pub install: Option<McpInstallSpec>,
    /// Secret names this server REQUIRES to be usable (ADR-090 D4), e.g.
    /// `["BRAVE_API_KEY"]`. Populated from the frontend preset at "add
    /// server" time and persisted in the Gateway catalog. Gateway's
    /// `build_available_mcps` skips pushing an entry whose `required_secrets`
    /// is non-empty while any required name is missing/empty across BOTH
    /// `env` and `headers` (stdio keys live in env, custom HTTP keys live in
    /// headers — same lookup convention as `extract_api_key_from_mcp_config`).
    /// Empty vec => usable without any secret.
    #[serde(default)]
    pub required_secrets: Vec<String>,
}

/// MCP transport type (wire format).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportDef {
    #[default]
    Stdio,
    Http,
    Sse,
}

/// MCP package distribution kind (ADR-072) — decides install command derivation
/// and dependency detection strategy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    /// npm / npx ecosystem (`npx -y <spec>`).
    Npm,
    /// PyPI ecosystem (`uvx` / `pipx` / `pip <spec>`).
    Pypi,
    /// Cargo ecosystem (`cargo install <crate>`).
    Cargo,
    /// Go toolchain (`go install <pkg>@latest`).
    Go,
    /// Container image (`docker run -i --rm <image>`).
    Docker,
    /// Direct binary download (URL → extract → PATH).
    Binary,
    /// Escape hatch: preset ships an explicit install script (.ps1/.sh).
    Script,
}

/// PyPI runner selection (ADR-072) — affects install and spawn derivation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PypiRunner {
    /// `uvx --from <spec> <entry_point>` — isolated ephemeral env.
    Uvx,
    /// `pipx install <spec>` — persistent env, command on PATH.
    Pipx,
    /// `pip install <spec>` — system/user site-packages.
    Pip,
}

/// Full spawn override (command + args) for MCP servers that cannot be derived.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecOverride {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Declarative MCP install spec (ADR-072) — three orthogonal axes:
/// package manager (kind) × package id (spec) × spawn shape (entry_point/spawn_args).
///
/// The installer framework derives install & spawn commands from this spec,
/// so presets declare facts instead of writing procedural scripts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPackageSpec {
    /// Distribution kind — decides install command derivation & dependency probe.
    pub kind: PackageKind,
    /// Package id: npm name / pypi name / cargo crate / docker image / binary URL.
    pub spec: String,
    /// PyPI-only: uvx / pipx / pip runner.
    #[serde(default)]
    pub runner: Option<PypiRunner>,
    /// Spawn command name override (defaults to `spec`).
    /// e.g. docling: package "docling-mcp" → entry_point "docling-mcp-server".
    #[serde(default)]
    pub entry_point: Option<String>,
    /// Extra args appended to the spawn command.
    /// e.g. docling needs ["--transport", "stdio"].
    #[serde(default)]
    pub spawn_args: Vec<String>,
    /// Full spawn override (takes precedence over derivation + entry_point).
    #[serde(default)]
    pub exec_override: Option<ExecOverride>,
    /// Script kind only: filename (without extension) under `assets/mcp_install/`.
    #[serde(default)]
    pub install_script: Option<String>,
    /// Extra HTTP MCP probe ports (ADR-072 decision 5), appended to the default
    /// fallback list when stdio fails with "invalid JSON-RPC response" and the
    /// server likely runs in HTTP mode. e.g. docling defaults to 8000.
    #[serde(default)]
    pub http_probe_ports: Vec<u16>,
}

/// Install lifecycle state — drives the frontend Install/Repair button.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstallState {
    /// No install record / unknown.
    #[default]
    Unknown,
    /// Runtime dependencies present, server not yet installed.
    Detected,
    /// Install in progress.
    Installing,
    /// Successfully installed & health-checked.
    Installed,
    /// Install failed (frontend shows stderr + guidance).
    Failed,
}

/// Optional install context attached to a catalog entry (ADR-072).
/// Present for preset-added servers; absent for user-custom servers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpInstallSpec {
    pub package: McpPackageSpec,
    #[serde(default)]
    pub state: InstallState,
}

/// Session info DTO for gRPC responses (S1.14)
///
/// Carries session metadata from Runtime to Gateway
/// so the HTTP API can return session lists without
/// directly reading JSONL files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfoDto {
    /// Session identifier (e.g. "20260503_143022_a1b2c3")
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Number of messages in the session
    pub message_count: u32,
    /// Optional session title
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the session metadata was recovered from a corrupted first line
    #[serde(default)]
    pub corrupted: bool,
    /// Current session lifecycle status (ADR-014). None if status is unknown
    /// (e.g. session loaded from disk, not currently active in memory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatusDto>,
    /// Per-session workspace selection persisted in JSONL metadata.
    /// None or "__agent_home__" means the agent's home directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Per-session model selection (ADR-012), from JSONL metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-session provider selection (ADR-012), from JSONL metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// DTO for session lifecycle status (ADR-014).
///
/// Mirrors `SessionStatus` from acowork-runtime but is defined in
/// acowork-core so Gateway can use it without depending on runtime.
///
/// ADR-049: `Streaming` is split into 3 sub-states (`LlmAwaitingFirstChunk`,
/// `LlmStreaming`, `ToolExecuting`) so the frontend can derive the processing
/// phase directly from session status without composing from data parameters.
/// This DTO must stay 1:1 in sync with the runtime enum (Gateway deserializes
/// `GET /api/agents/{id}/sessions/{sid}/state` into this type).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum SessionStatusDto {
    /// Session is idle — no LLM call in progress.
    Idle,
    /// LLM HTTP request has been sent; waiting for the first content chunk
    /// (TTFT phase).
    LlmAwaitingFirstChunk,
    /// LLM is producing reasoning/thinking content. Entered when the first
    /// ReasoningContent stream event arrives, before any visible Content.
    /// Promotes to LlmStreaming when the first visible content chunk arrives.
    Thinking,
    /// LLM is actively streaming content. `message_id` matches the streaming
    /// message, if available.
    LlmStreaming { message_id: Option<String> },
    /// Tool calls have been dispatched to the tool registry; waiting for
    /// their results.
    ToolExecuting,
    /// A tool requires user approval before execution.
    WaitingApproval { request_id: String },
    /// Iteration limit reached, debug pause, or 429 retry wait — awaiting user decision.
    Paused {
        iteration: Option<u32>,
        max_iterations: Option<u32>,
        /// 429 retry wait info. `None` for non-retry pauses.
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_info: Option<RetryPauseInfoDto>,
        /// Why the session paused. `None` for 429 retry waits (retry_info
        /// already disambiguates them). Mirrors `PauseReason` from runtime.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<PauseReasonDto>,
        /// Human-readable pause message. Mirrors the runtime field.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Reason a session entered `Paused` (DTO mirror of runtime `PauseReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReasonDto {
    /// Iteration limit reached — awaiting a user decision to continue or stop.
    IterationLimit,
    /// Loop detector triggered — awaiting a user decision to continue or stop.
    LoopDetected,
    /// Debugger paused the session (DevMode debug protocol).
    Debug,
}

/// 429 rate-limit retry pause information (DTO).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPauseInfoDto {
    /// Wait duration in milliseconds
    pub wait_ms: u64,
    /// Current retry attempt (1-based)
    pub attempt: u32,
    /// Maximum retry attempts
    pub max_attempts: u32,
    /// Name of the provider that was rate-limited
    pub provider: String,
}

/// Conversation entry DTO for gRPC responses (S1.14)
///
/// Carries a single message from Runtime to Gateway
/// for paginated message queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntryDto {
    /// Unique message ID
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// Message role: "user" | "assistant" | "think" | "tool_call" | "tool_result" | "system"
    pub role: String,
    /// Full message content
    pub content: String,
    /// Optional metadata (e.g. tool_call_id, tool_name, or `CompactionEventMeta`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Entry kind. `None` or `"message"` denotes a regular role-based message.
    /// `"compaction"` denotes an LLM-driven compaction summary event whose
    /// `content` carries the summary text and `metadata` carries
    /// `CompactionEventMeta`. Mirrors `ConversationEntry.kind` from the
    /// JSONL v2 schema and is transparently propagated to the frontend so
    /// the UI can render compaction events as folded summary cards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Cron entry info (for gRPC responses, S5.8 enhanced)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntryInfo {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent ID that owns this entry
    pub agent_id: String,
    /// Cron schedule expression
    pub schedule: String,
    /// Action to fire
    pub action: String,
    /// Params for the IntentReceived
    pub params: Value,
    /// Timezone for schedule interpretation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Max retry count on failure
    #[serde(default)]
    pub retry_count: u32,
    /// Retry backoff interval in seconds
    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,
    /// Max total executions (None = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
    /// Current execution count
    #[serde(default)]
    pub run_count: u32,
    /// Expiry timestamp in Unix millis
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// Default retry interval: 60 seconds
fn default_retry_interval() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_request_serialize_key_release() {
        let req = GatewayRequest::KeyRelease {
            provider: "openai".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"KeyRelease\""));
        assert!(json.contains("\"provider\":\"openai\""));
    }

    #[test]
    fn test_gateway_request_roundtrip() {
        let req = GatewayRequest::IntentSend {
            target: "com.example.calendar".into(),
            action: "schedule".into(),
            params: serde_json::json!({"time": "10:00"}),
            async_: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: GatewayRequest = serde_json::from_str(&json).unwrap();
        if let GatewayRequest::IntentSend { target, action, .. } = parsed {
            assert_eq!(target, "com.example.calendar");
            assert_eq!(action, "schedule");
        } else {
            panic!("Expected IntentSend variant");
        }
    }

    #[test]
    fn test_gateway_response_roundtrip() {
        let resp = GatewayResponse::BudgetInfo {
            remaining_tokens: 50000,
            remaining_cost_usd: 1.5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::BudgetInfo {
            remaining_tokens, ..
        } = parsed
        {
            assert_eq!(remaining_tokens, 50000);
        } else {
            panic!("Expected BudgetInfo variant");
        }
    }

    #[test]
    fn test_intent_received_without_command() {
        let resp = GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "chat_message".to_string(),
            params: serde_json::json!({"content": "hello"}),
            command: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        // command should be skipped when None
        assert!(!json.contains("command"));
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::IntentReceived {
            from,
            action,
            command,
            ..
        } = parsed
        {
            assert_eq!(from, "http-api");
            assert_eq!(action, "chat_message");
            assert!(command.is_none());
        } else {
            panic!("Expected IntentReceived variant");
        }
    }

    #[test]
    fn test_intent_received_with_command() {
        let resp = GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "chat_message".to_string(),
            params: serde_json::json!({"content": "hello"}),
            command: Some("/commit".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("command"));
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::IntentReceived {
            from,
            action,
            command,
            ..
        } = parsed
        {
            assert_eq!(from, "http-api");
            assert_eq!(action, "chat_message");
            assert_eq!(command, Some("/commit".to_string()));
        } else {
            panic!("Expected IntentReceived variant");
        }
    }

    #[test]
    fn test_intent_received_backward_compatible() {
        // Old JSON without command field should deserialize with command=None
        let json = r#"{"type":"IntentReceived","from":"http-api","action":"chat_message","params":{"content":"hello"}}"#;
        let parsed: GatewayResponse = serde_json::from_str(json).unwrap();
        if let GatewayResponse::IntentReceived { command, .. } = parsed {
            assert!(command.is_none());
        } else {
            panic!("Expected IntentReceived variant");
        }
    }

    #[test]
    fn test_attached_context_item_roundtrip() {
        // Frontend sends camelCase absPath; runtime uses snake_case abs_path.
        let item = AttachedContextItem {
            abs_path: "/workspace/src/main.rs".to_string(),
            context_type: "selection".to_string(),
            start_line: Some(10),
            end_line: Some(20),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"absPath\":"));
        assert!(!json.contains("\"abs_path\":"));

        let parsed: AttachedContextItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.abs_path, item.abs_path);
        assert_eq!(parsed.context_type, item.context_type);
        assert_eq!(parsed.start_line, item.start_line);
        assert_eq!(parsed.end_line, item.end_line);

        // Also verify deserialization from the frontend payload shape.
        let frontend_json = r#"{"absPath":"D:\\project\\foo.rs","type":"file","startLine":5,"endLine":15}"#;
        let parsed: AttachedContextItem = serde_json::from_str(frontend_json).unwrap();
        assert_eq!(parsed.abs_path, "D:\\project\\foo.rs");
        assert_eq!(parsed.context_type, "file");
        assert_eq!(parsed.start_line, Some(5));
        assert_eq!(parsed.end_line, Some(15));
    }

    // ── AttachedItem (ADR-046) ──────────────────────────────────────────

    /// Frontend sends camelCase fields; the `type` tag is snake_case
    /// (e.g. `"file_upload"`).
    #[test]
    fn test_attached_item_file_upload_roundtrip() {
        let item = AttachedItem::FileUpload {
            document_id: "doc-abc".into(),
            filename: "report.pdf".into(),
            format: "pdf".into(),
            size_bytes: 12345,
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"file_upload\""));
        assert!(json.contains("\"documentId\":"));
        assert!(json.contains("\"sizeBytes\":12345"));
        // Round-trip
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_attached_item_image_upload_with_dimensions() {
        let item = AttachedItem::ImageUpload {
            document_id: "doc-img".into(),
            filename: "photo.png".into(),
            format: "png".into(),
            size_bytes: 987654,
            width: Some(1920),
            height: Some(1080),
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"image_upload\""));
        assert!(json.contains("\"width\":1920"));
        assert!(json.contains("\"height\":1080"));
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_attached_item_image_upload_omits_dimensions() {
        let item = AttachedItem::ImageUpload {
            document_id: "doc-img2".into(),
            filename: "x.png".into(),
            format: "png".into(),
            size_bytes: 1,
            width: None,
            height: None,
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("width"));
        assert!(!json.contains("height"));
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_attached_item_attached_file_roundtrip() {
        let item = AttachedItem::AttachedFile {
            abs_path: "/workspace/foo.rs".into(),
            name: "foo.rs".into(),
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"attached_file\""));
        assert!(json.contains("\"absPath\":"));
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_attached_item_attached_selection_roundtrip() {
        let item = AttachedItem::AttachedSelection {
            abs_path: "/workspace/bar.rs".into(),
            name: "bar.rs".into(),
            start_line: 10,
            end_line: 25,
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"attached_selection\""));
        assert!(json.contains("\"startLine\":10"));
        assert!(json.contains("\"endLine\":25"));
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_attached_item_attached_folder_roundtrip() {
        let item = AttachedItem::AttachedFolder {
            abs_path: "/workspace/src".into(),
            name: "src".into(),
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"attached_folder\""));
        assert!(json.contains("\"absPath\":"));
        let parsed: AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    // ── SidecarKind wire compatibility ───────────────────────────────────

    /// The proto enum value is the on-the-wire identifier and must stay
    /// stable across versions. Adding new sidecars appends a new variant
    /// at the next free i32, never renames or reorders existing ones.
    #[test]
    fn test_sidecar_kind_as_str_is_stable() {
        assert_eq!(SidecarKind::Unspecified.as_str(), "unspecified");
        assert_eq!(SidecarKind::LspRelay.as_str(), "lsp_relay");
        assert_eq!(SidecarKind::Embed.as_str(), "embed");
    }

    #[test]
    fn test_sidecar_kind_from_str_roundtrip() {
        for k in [
            SidecarKind::Unspecified,
            SidecarKind::LspRelay,
            SidecarKind::Embed,
        ] {
            let s = k.as_str();
            let parsed: SidecarKind = s.parse().expect("must parse");
            assert_eq!(parsed, k);
        }
    }

    #[test]
    fn test_sidecar_kind_unknown_string_is_rejected() {
        let bad: Result<SidecarKind, _> = "code_index".parse();
        assert!(bad.is_err(), "unknown sidecar must be rejected");
    }

    /// SidecarScope model (ADR-055 §6.7): LSP relay is node-local (one per
    /// node, must share the machine with the agent workspace); embed stays
    /// global on the Gateway machine. Unspecified falls back to Global.
    #[test]
    fn test_sidecar_kind_scope() {
        assert_eq!(SidecarKind::LspRelay.scope(), SidecarScope::NodeLocal);
        assert_eq!(SidecarKind::Embed.scope(), SidecarScope::Global);
        assert_eq!(SidecarKind::Unspecified.scope(), SidecarScope::Global);
    }

    /// `SidecarEndpointUpdate` payload survives JSON roundtrip. The empty
    /// `endpoint` field is the "sidecar unavailable" signal and must be
    /// preserved (not collapsed to None).
    #[test]
    fn test_sidecar_endpoint_update_roundtrip() {
        let resp = GatewayResponse::SidecarEndpointUpdate {
            sidecar: SidecarKind::LspRelay,
            endpoint: "http://127.0.0.1:19878".to_string(),
            spec_json: String::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            GatewayResponse::SidecarEndpointUpdate {
                sidecar,
                endpoint,
                spec_json,
            } => {
                assert_eq!(sidecar, SidecarKind::LspRelay);
                assert_eq!(endpoint, "http://127.0.0.1:19878");
                assert!(spec_json.is_empty());
            }
            _ => panic!("Expected SidecarEndpointUpdate variant"),
        }
    }

    /// Empty `endpoint` (sidecar unavailable) must round-trip cleanly. This
    /// is the disable signal the Runtime uses to turn off sidecar-dependent
    /// features.
    #[test]
    fn test_sidecar_endpoint_update_unavailable_signal() {
        let resp = GatewayResponse::SidecarEndpointUpdate {
            sidecar: SidecarKind::Embed,
            endpoint: String::new(),
            spec_json: r#"{"model_id":"","dimension":0}"#.to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            GatewayResponse::SidecarEndpointUpdate {
                sidecar,
                endpoint,
                spec_json,
            } => {
                assert_eq!(sidecar, SidecarKind::Embed);
                assert!(endpoint.is_empty(), "empty endpoint must be preserved");
                assert!(spec_json.contains("model_id"));
            }
            _ => panic!("Expected SidecarEndpointUpdate variant"),
        }
    }

    #[test]
    fn test_paused_dto_serializes_reason_and_message() {
        let paused = SessionStatusDto::Paused {
            iteration: Some(3),
            max_iterations: Some(5),
            retry_info: None,
            reason: Some(PauseReasonDto::IterationLimit),
            message: Some("Iteration limit reached (3/5). Click Continue to proceed.".into()),
        };
        let json = serde_json::to_string(&paused).unwrap();
        let parsed: SessionStatusDto = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionStatusDto::Paused {
                iteration,
                max_iterations,
                retry_info,
                reason,
                message,
            } => {
                assert_eq!(iteration, Some(3));
                assert_eq!(max_iterations, Some(5));
                assert!(retry_info.is_none());
                assert_eq!(reason, Some(PauseReasonDto::IterationLimit));
                assert!(message.as_deref().unwrap().contains("Iteration limit reached"));
            }
            _ => panic!("Expected Paused variant"),
        }
    }

    #[test]
    fn test_paused_dto_retry_skips_reason_and_message() {
        // 429 retry pause: reason/message are None and must be omitted from
        // the JSON so the frontend only sees retry_info.
        let paused = SessionStatusDto::Paused {
            iteration: None,
            max_iterations: None,
            retry_info: Some(RetryPauseInfoDto {
                wait_ms: 300_000,
                attempt: 1,
                max_attempts: 3,
                provider: "mock-provider".into(),
            }),
            reason: None,
            message: None,
        };
        let json = serde_json::to_string(&paused).unwrap();
        assert!(json.contains("retry_info"));
        assert!(!json.contains("reason"));
        assert!(!json.contains("message"));
        let parsed: SessionStatusDto = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionStatusDto::Paused {
                retry_info,
                reason,
                message,
                ..
            } => {
                assert!(retry_info.is_some());
                assert!(reason.is_none());
                assert!(message.is_none());
            }
            _ => panic!("Expected Paused variant"),
        }
    }

    // ── ADR-066: ContextUsageInfo cache token fields ─────────────────────

    /// All six cache-token fields round-trip cleanly through JSON.
    /// This is the wire-format contract between Runtime push (ContextUpdate)
    /// and Desktop ResultsPanel.
    #[test]
    fn test_context_usage_info_cache_roundtrip_all_fields() {
        let info = ContextUsageInfo {
            context_window: 200_000,
            input_tokens: 1000,
            output_tokens: 200,
            total_tokens: 1200,
            max_input_tokens: Some(183_616),
            usable_context: 183_616,
            usage_percent: 1,
            total_input_tokens: Some(4500),
            total_output_tokens: Some(650),
            agent_total_input_tokens: Some(4500),
            agent_total_output_tokens: Some(650),
            cache_read_tokens: Some(1600),
            cache_write_tokens: Some(100),
            total_cache_read_tokens: Some(1600),
            total_cache_write_tokens: Some(100),
            agent_total_cache_read_tokens: Some(1600),
            agent_total_cache_write_tokens: Some(100),
            sections: None,
            iteration: Some(42),
            avg_tps: Some(48.25),
            avg_ttft_ms: Some(512.0),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: ContextUsageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cache_read_tokens, Some(1600));
        assert_eq!(parsed.cache_write_tokens, Some(100));
        assert_eq!(parsed.total_cache_read_tokens, Some(1600));
        assert_eq!(parsed.total_cache_write_tokens, Some(100));
        assert_eq!(parsed.agent_total_cache_read_tokens, Some(1600));
        assert_eq!(parsed.agent_total_cache_write_tokens, Some(100));
        // Non-cache fields also preserved.
        assert_eq!(parsed.input_tokens, 1000);
        assert_eq!(parsed.usage_percent, 1);
        // Iteration field roundtrips.
        assert_eq!(parsed.iteration, Some(42));
    }

    /// Cache fields are `skip_serializing_if = "Option::is_none"`. When
    /// every cache field is `None` (e.g. the Provider does not report
    /// cache, or the call is too old to be relevant), they MUST be
    /// omitted from the JSON — this is what the panel uses to render "—".
    #[test]
    fn test_context_usage_info_cache_none_omitted_from_json() {
        let info = ContextUsageInfo {
            context_window: 128_000,
            input_tokens: 500,
            output_tokens: 50,
            total_tokens: 550,
            max_input_tokens: None,
            usable_context: 111_616,
            usage_percent: 0,
            total_input_tokens: Some(500),
            total_output_tokens: Some(50),
            agent_total_input_tokens: Some(500),
            agent_total_output_tokens: Some(50),
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_cache_read_tokens: None,
            total_cache_write_tokens: None,
            agent_total_cache_read_tokens: None,
            agent_total_cache_write_tokens: None,
            sections: None,
            iteration: None,
            avg_tps: None,
            avg_ttft_ms: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        // Cache fields absent → no `"cache_..."` substring in the JSON.
        assert!(!json.contains("cache_read_tokens"), "field omitted");
        assert!(!json.contains("cache_write_tokens"), "field omitted");
        assert!(!json.contains("total_cache_read_tokens"), "field omitted");
        assert!(!json.contains("total_cache_write_tokens"), "field omitted");
        assert!(!json.contains("agent_total_cache_read_tokens"), "field omitted");
        assert!(!json.contains("agent_total_cache_write_tokens"), "field omitted");
        // iteration is also `skip_serializing_if = Option::is_none`.
        assert!(!json.contains("iteration"), "field omitted");
    }

    /// Backward compatibility: an older Runtime (ADR-066 not yet
    /// implemented) sends a JSON without the six cache fields. The
    /// frontend MUST still parse the payload and treat the cache fields
    /// as `None` — this is what prevents the "—" / "0% cached" UI states
    /// from being silently wiped by a downgrade.
    #[test]
    fn test_context_usage_info_cache_backward_compatible_legacy_payload() {
        // Pre-ADR-066 wire format: only input/output fields, no cache_*
        let legacy_json = r#"{
            "context_window": 200000,
            "input_tokens": 1000,
            "output_tokens": 200,
            "total_tokens": 1200,
            "usable_context": 183616,
            "usage_percent": 1,
            "total_input_tokens": 4500,
            "total_output_tokens": 650,
            "agent_total_input_tokens": 4500,
            "agent_total_output_tokens": 650
        }"#;
        let parsed: ContextUsageInfo = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.cache_read_tokens, None);
        assert_eq!(parsed.cache_write_tokens, None);
        assert_eq!(parsed.total_cache_read_tokens, None);
        assert_eq!(parsed.total_cache_write_tokens, None);
        assert_eq!(parsed.agent_total_cache_read_tokens, None);
        assert_eq!(parsed.agent_total_cache_write_tokens, None);
        // Non-cache fields are intact.
        assert_eq!(parsed.context_window, 200_000);
        assert_eq!(parsed.total_input_tokens, Some(4500));
    }
}
