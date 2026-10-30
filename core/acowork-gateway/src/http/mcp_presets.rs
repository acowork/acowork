//! MCP preset registry — the platform-curated list of installable MCP servers.
//!
//! ## Why this lives in the Gateway, not the Desktop
//!
//! ADR-104 lets a package declare `[[mcp_servers]] preset = "<id>"`, i.e. an
//! *identifier* whose meaning cannot be package-local. If the table lived in
//! the Desktop's TypeScript, the answer to "what does `brave-search` mean?"
//! would exist only in one client: a second client (mobile), a Gateway-side
//! validation pass, or the install pipeline could not resolve it, and a package
//! would be referencing a fact the platform cannot state. So the registry is
//! server-side data with one HTTP projection (`GET /api/mcp-presets`), and the
//! desktop renders what it is given.
//!
//! ## What a preset is (and is not)
//!
//! A preset is a **spawn recipe + secret requirements + install spec** — never
//! an installation. Adding one to the catalog is the user's action; installing
//! the underlying package happens in the Runtime process (ADR-090: the Gateway
//! does not spawn MCP servers, because a Gateway-side spawn lands on whichever
//! machine the Gateway runs on).
//!
//! ## Selection principle (unchanged from the desktop-era table)
//!
//! Only servers that fill a gap the builtin tools do not cover. `file_*`,
//! `shell`, `web_fetch`, `http_request`, `doc_reader` and the memory service
//! are already platform capabilities — listing an MCP server that duplicates
//! them would just be a second, worse path to the same thing.

use axum::{Json, extract::State, routing::get, Router};
use serde::Serialize;
use std::sync::LazyLock;

use crate::http::routes::AppState;
use acowork_core::protocol::{InstallState, McpInstallSpec, McpPackageSpec, McpServerConfigDef, McpTransportDef, PackageKind, PypiRunner};

/// One curated MCP server definition.
///
/// Field semantics match the desktop `McpPresetDef` it replaced, plus
/// `required_secrets`-derived `required_env` (same list, kept under the
/// frontend-facing name so the wire shape stayed stable).
#[derive(Debug, Clone, Serialize)]
pub struct McpPreset {
    /// Stable id. Packages reference `<kind>` via `[[mcp_servers]] preset`,
    /// so this string is an API contract — renaming one breaks published
    /// packages. Add an alias instead.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Grouping label for the catalog picker.
    pub category: &'static str,
    pub transport: McpTransportDef,
    /// stdio spawn command.
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Env vars the user must supply (API keys). Mirrored into
    /// `required_secrets` on the catalog entry — the Gateway filters a push
    /// while any of them is empty (ADR-090 D4).
    pub required_env: &'static [&'static str],
    pub install_hint: &'static str,
    /// lucide-react icon name.
    pub icon: &'static str,
    /// Declarative install spec (ADR-072). Its `state` is always `unknown`
    /// here: the registry describes a package, not an installation.
    pub install_package: McpPackageSpec,
}

impl McpPreset {
    /// Build the catalog entry for this preset, with `origin` set so the
    /// catalog can say where the entry came from.
    pub fn to_catalog_entry(&self, origin: &str) -> McpServerConfigDef {
        McpServerConfigDef {
            name: self.id.to_string(),
            display_name: Some(self.name.to_string()),
            transport: self.transport.clone(),
            url: None,
            command: self.command.to_string(),
            args: self.args.iter().map(|s| s.to_string()).collect(),
            env: Default::default(),
            headers: Default::default(),
            tool_timeout_secs: None,
            install: Some(McpInstallSpec {
                package: self.install_package.clone(),
                state: InstallState::Unknown,
            }),
            required_secrets: self.required_env.iter().map(|s| s.to_string()).collect(),
            origin: Some(origin.to_string()),
        }
    }

    /// Install spec for the wire response (the catalog shape the desktop
    /// already understands).
    fn install_spec(&self) -> McpInstallSpec {
        McpInstallSpec {
            package: self.install_package.clone(),
            state: InstallState::Unknown,
        }
    }
}

/// Wire projection of a preset (camelCase-free — the desktop maps it).
#[derive(Debug, Clone, Serialize)]
pub struct McpPresetResponse {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub transport: McpTransportDef,
    pub command: &'static str,
    pub args: Vec<String>,
    pub required_env: Vec<String>,
    pub install_hint: &'static str,
    pub icon: &'static str,
    pub install: McpInstallSpec,
}

impl From<&McpPreset> for McpPresetResponse {
    fn from(p: &McpPreset) -> Self {
        Self {
            id: p.id,
            name: p.name,
            description: p.description,
            category: p.category,
            transport: p.transport.clone(),
            command: p.command,
            args: p.args.iter().map(|s| s.to_string()).collect(),
            required_env: p.required_env.iter().map(|s| s.to_string()).collect(),
            install_hint: p.install_hint,
            icon: p.icon,
            install: p.install_spec(),
        }
    }
}

/// `GET /api/mcp-presets` response.
#[derive(Debug, Clone, Serialize)]
pub struct McpPresetsResponse {
    pub presets: Vec<McpPresetResponse>,
}

/// Router for the preset registry.
pub fn mcp_preset_routes() -> Router<AppState> {
    Router::new()
        .route("/api/mcp-presets", get(list_presets))
        .route("/api/mcp-presets/{id}", get(get_preset))
}

/// `GET /api/mcp-presets` — the curated list, in table order.
async fn list_presets(State(_state): State<AppState>) -> Json<McpPresetsResponse> {
    Json(McpPresetsResponse {
        presets: MCP_PRESETS.iter().map(McpPresetResponse::from).collect(),
    })
}

/// `GET /api/mcp-presets/{id}` — one preset.
///
/// The list endpoint is enough for the catalog picker, but a package
/// recommendation names a single preset and the Tools tab needs to render that
/// one card without downloading the whole registry. 404 (not an empty body) for
/// an unknown id: a package may reference a preset this platform version does
/// not have, and the caller must be able to tell "removed" from "empty".
async fn get_preset(axum::extract::Path(id): axum::extract::Path<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match preset(&id) {
        Some(p) => Json(McpPresetResponse::from(p)).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown mcp preset '{id}'"),
                "code": "unknown_preset",
            })),
        )
            .into_response(),
    }
}

/// Look up a preset by id (used by package validation and the Runtime-facing
/// code paths that must resolve `[[mcp_servers]] preset = "<id>"`).
pub fn preset(id: &str) -> Option<&'static McpPreset> {
    MCP_PRESETS.iter().find(|p| p.id == id)
}

/// Every preset id — used by the manifest graph validator to reject a
/// declaration that references a preset the platform does not have.
pub fn preset_ids() -> Vec<&'static str> {
    MCP_PRESETS.iter().map(|p| p.id).collect()
}

/// Origin string for a catalog entry installed from a preset.
pub fn preset_origin(id: &str) -> String {
    format!("preset:{id}")
}

/// Origin string for a catalog entry installed on a package's recommendation.
pub fn package_origin(agent_id: &str, recommend_key: &str) -> String {
    format!("agent-package:{agent_id}#{recommend_key}")
}

/// Build an install spec, filling every optional field explicitly.
///
/// `McpPackageSpec` deliberately has no `Default` (its `kind`/`spec` pair
/// carries the meaning), so the registry spells the optional half out once
/// here instead of six times in the table.
fn pkg(kind: PackageKind, spec: &str) -> McpPackageSpec {
    McpPackageSpec {
        kind,
        spec: spec.to_string(),
        runner: None,
        entry_point: None,
        spawn_args: Vec::new(),
        exec_override: None,
        install_script: None,
        http_probe_ports: Vec::new(),
    }
}

fn npm(spec: &str) -> McpPackageSpec {
    pkg(PackageKind::Npm, spec)
}

/// The curated table.
///
/// Order is presentation order in the catalog picker; category groups are
/// rendered by the client.
/// The registry, built once on first use.
///
/// A `static` slice of literals would be nicer, but the install spec carries
/// `String`s and `String::from` is not const — so it is a `LazyLock<Vec<_>>`.
pub static MCP_PRESETS: LazyLock<Vec<McpPreset>> = LazyLock::new(build_presets);

fn build_presets() -> Vec<McpPreset> {
    vec![
    // ── Browser automation ─────────────────────────────────────────────
    McpPreset {
        id: "playwright",
        name: "Playwright",
        description: "Browser automation — navigate pages, take screenshots, fill forms, extract \
                      structured content. Supports Chromium, Firefox, WebKit.",
        category: "browser",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "@playwright/mcp@latest"],
        required_env: &[],
        install_hint: "npx auto-installs. First run downloads browser binaries (~300MB). \
                       No API key required.",
        icon: "Monitor",
        install_package: npm("@playwright/mcp@latest"),
    },
    McpPreset {
        id: "computer-use",
        name: "Computer Use",
        description: "OS-level desktop automation: screenshots, mouse and keyboard control, \
                      clicking UI elements by coordinates. Drives GUI applications that have no \
                      API or MCP integration.",
        category: "browser",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "computer-use-mcp"],
        required_env: &[],
        install_hint: "npx auto-installs. Requires OS-level input-control permissions \
                       (macOS: Accessibility + Screen Recording; Linux: X11/Wayland; \
                       Windows: UAC prompts). No API key required.",
        icon: "MousePointer",
        install_package: npm("computer-use-mcp"),
    },
    // ── Search ─────────────────────────────────────────────────────────
    McpPreset {
        id: "brave-search",
        name: "Brave Search",
        description: "Privacy-first web & local search via the Brave Search API. Good for \
                      general knowledge, news, and factual lookups.",
        category: "search",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-brave-search"],
        required_env: &["BRAVE_API_KEY"],
        install_hint: "Get a free API key at https://brave.com/search/api/ — generous free \
                       tier (2K queries/month).",
        icon: "Search",
        install_package: npm("@modelcontextprotocol/server-brave-search"),
    },
    McpPreset {
        id: "exa-search",
        name: "Exa Search",
        description: "AI-native semantic search engine. Returns clean, structured content \
                      optimized for LLM consumption. Supports neural search, auto-crawl and \
                      content extraction.",
        category: "search",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "exa-mcp-server"],
        required_env: &["EXA_API_KEY"],
        install_hint: "Get an API key at https://exa.ai — free tier includes 1000 \
                       searches/month. Better than keyword search for complex queries.",
        icon: "Globe",
        install_package: npm("exa-mcp-server"),
    },
    McpPreset {
        id: "context7",
        name: "Context7",
        description: "Fetch up-to-date library documentation and code examples. Prevents \
                      hallucinations by grounding responses in real, version-specific docs.",
        category: "search",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "@upstash/context7-mcp@latest"],
        required_env: &[],
        install_hint: "npx auto-installs. No API key required. Covers 20K+ libraries (React, \
                       Next.js, Python, Rust crates, …).",
        icon: "BookOpen",
        install_package: npm("@upstash/context7-mcp@latest"),
    },
    // ── Document processing ────────────────────────────────────────────
    McpPreset {
        id: "docling",
        name: "Docling",
        description: "Advanced document understanding: OCR for image-based PDFs, layout \
                      analysis, table extraction, and legacy Office formats (.doc/.ppt/.xls). \
                      Fills the doc_reader gaps.",
        category: "document",
        transport: McpTransportDef::Stdio,
        command: "uvx",
        args: &[
            "--from",
            "docling-mcp",
            "docling-mcp-server",
            "--transport",
            "stdio",
        ],
        required_env: &[],
        install_hint: "Requires uv (install: `pip install uv`, see https://docs.astral.sh/uv/). \
                       First run downloads ML models (~500MB). No API key — fully local.",
        icon: "FileText",
        install_package: McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "docling-mcp".to_string(),
            runner: Some(PypiRunner::Uvx),
            // The console script is NOT the package name, and the default
            // transport is streamable-http, so stdio must be requested.
            entry_point: Some("docling-mcp-server".to_string()),
            spawn_args: vec!["--transport".to_string(), "stdio".to_string()],
            http_probe_ports: vec![8000],
            exec_override: None,
            install_script: None,
        },
    },
    // ── Knowledge & collaboration ──────────────────────────────────────
    McpPreset {
        id: "notion",
        name: "Notion",
        description: "Read, write and search Notion pages and databases: knowledge bases, \
                      docs, structured data.",
        category: "knowledge",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &["-y", "@notionhq/notion-mcp-server"],
        required_env: &["NOTION_API_KEY"],
        install_hint: "Create an integration at https://www.notion.so/my-integrations — then \
                       share the target pages with that integration.",
        icon: "FileText",
        install_package: npm("@notionhq/notion-mcp-server"),
    },
    // ── Design ─────────────────────────────────────────────────────────
    McpPreset {
        id: "figma",
        name: "Figma",
        description: "Read Figma designs — layouts, styles, components and assets — so an \
                      agent can understand a UI design and generate matching code.",
        category: "design",
        transport: McpTransportDef::Stdio,
        command: "npx",
        args: &[
            "-y",
            "figma-developer-mcp",
            "--figma-api-key=$FIGMA_API_KEY",
            "--stdio",
        ],
        required_env: &["FIGMA_API_KEY"],
        install_hint: "Generate a personal access token at \
                       https://www.figma.com/developers/api#access-tokens — then share the Figma \
                       files with the token owner.",
        icon: "PenTool",
        install_package: McpPackageSpec {
            spawn_args: vec![
                "--figma-api-key=$FIGMA_API_KEY".to_string(),
                "--stdio".to_string(),
            ],
            ..npm("figma-developer-mcp")
        },
    },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_unique_and_kebab() {
        let ids = preset_ids();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "preset ids must be unique");
        for id in ids {
            assert!(!id.is_empty());
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "preset id `{id}` must be a lowercase slug (packages reference it)"
            );
        }
    }

    #[test]
    fn lookup_finds_known_and_rejects_unknown() {
        assert_eq!(preset("brave-search").map(|p| p.name), Some("Brave Search"));
        assert!(preset("no-such-preset").is_none());
    }

    #[test]
    fn every_preset_declares_a_usable_spawn_command() {
        for p in MCP_PRESETS.iter() {
            assert!(!p.command.is_empty(), "{}: command", p.id);
            assert!(
                !p.args.is_empty(),
                "{}: a curated preset must ship its args — an empty arg list usually means the \
                 spawn command is wrong",
                p.id
            );
            assert!(!p.description.is_empty(), "{}: description", p.id);
            assert!(!p.install_hint.is_empty(), "{}: install_hint", p.id);
        }
    }

    #[test]
    fn required_env_mirrors_into_required_secrets() {
        let entry = preset("brave-search").unwrap().to_catalog_entry("preset:brave-search");
        assert_eq!(entry.required_secrets, vec!["BRAVE_API_KEY".to_string()]);
        assert_eq!(entry.env.len(), 0, "a secret is never pre-filled by the registry");
        assert_eq!(entry.origin.as_deref(), Some("preset:brave-search"));
    }

    #[test]
    fn catalog_entry_carries_the_install_spec() {
        let entry = preset("docling").unwrap().to_catalog_entry("preset:docling");
        let install = entry.install.expect("install spec");
        assert_eq!(install.package.kind, PackageKind::Pypi);
        assert_eq!(install.package.entry_point.as_deref(), Some("docling-mcp-server"));
        assert_eq!(install.state, InstallState::Unknown);
    }

    #[test]
    fn package_origin_is_addressable() {
        assert_eq!(
            package_origin("com.acowork.pm", "brave-search"),
            "agent-package:com.acowork.pm#brave-search"
        );
    }

    #[test]
    fn response_projection_carries_what_the_client_needs_to_install() {
        let resp = McpPresetResponse::from(preset("figma").unwrap());
        assert_eq!(resp.id, "figma");
        assert_eq!(resp.required_env, vec!["FIGMA_API_KEY".to_string()]);
        assert_eq!(resp.transport, McpTransportDef::Stdio);
        assert!(resp.command == "npx");
        assert!(resp.install.package.spawn_args.len() == 2);
    }
}