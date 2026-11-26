# Multi-Language LSP Project Root Discovery

## 1. Background

### 1.1 Problem

Opening TypeScript source files (e.g. `MarkdownPreviewView.tsx`) via the File Tab in acowork-desktop results in numerous "Cannot find module" red squiggles, while `tsc --noEmit` compiles without errors.

### 1.2 Root Cause

acowork is a Rust + TypeScript monorepo with no `tsconfig.json` at the workspace root (e.g. `/Users/nicholas/projects/AgentCowork`). The LSP relay passes this monorepo root as `rootUri` to `typescript-language-server`, which falls back to a **default project** using TypeScript's default module resolution (without `moduleResolution: "bundler"`). This causes resolution failures for packages like `react-markdown` and `remark-gfm` that declare types via `exports` fields.

### 1.3 Scope

The problem affects all languages declared in `lsp_servers.json`, not just TypeScript:

| Language | LSP Server | rootUri Dependency | Current Behavior |
|----------|------------|-------------------|-----------------|
| TypeScript | typescript-language-server | **Strong** | No tsconfig at root → default project → module resolution fails |
| Rust | rust-analyzer | Weak | Walks up from file path to find Cargo.toml; works fine |
| Python | pyright / pylsp | Medium | Finds pyproject.toml walking up; works fine |
| Go | gopls | **Strong** | rootUri must contain go.mod or have an ancestor with one |
| C/C++ | clangd | **Strong** | Requires compile_commands.json under rootUri |
| Java | jdtls | **Strong** | Requires pom.xml / build.gradle under rootUri |
| Kotlin | kotlin-language-server | **Strong** | Same as Java |
| JSON/YAML/HTML/CSS/Markdown | respective LSPs | None | Rootless; monorepo root works fine |

### 1.4 VSCode's Approach

VSCode also sets `rootUri` to the workspace root on `initialize`, but tsserver walks up from the opened file's directory on `textDocument/didOpen` to find a `tsconfig.json`, creates an independent TypeScript Project with that config's `compilerOptions`. The VSCode TypeScript extension also actively scans all `tsconfig.json` files in the workspace and registers them as independent projects.

acowork's LSP relay currently only passes one rootUri and does not use the file path from `didOpen` for secondary discovery.

## 2. Design Goals

1. **Full language coverage**: All languages declared in `lsp_servers.json` are covered
2. **Automatic discovery**: Derive project root from file path + language automatically, no manual config
3. **Minimal changes**: Leverage existing LSP Pool key isolation; do not modify pool layer
4. **Declarative extension**: Adding a new language requires only one line in config
5. **Dual-path support**: Both WebSocket relay path (editor interaction) and Codebase RPC path (Bot Runtime code intelligence)

## 3. Core Concepts: Project Root vs Workspace Root

Two independent concepts:

| Concept | Purpose | Source |
|---------|---------|--------|
| **Workspace Root** | Build absolute file paths (`buildAbsoluteUri`) | `treeRoots[agentId:workspaceId]`, i.e. the monorepo root |
| **Project Root** | LSP connection params (`rootUri`, pool key, working directory) | Auto-discovered from file path + language marker files |

Both derived from "file path + language" without user intervention.

## 4. Language Marker File Table

Each language declares a set of **marker files** (root markers). The LSP relay checks for these as it walks up from the file's directory. The first directory containing a marker is the Project Root.

| Language | Marker Files | Notes |
|----------|-------------|-------|
| TypeScript | `tsconfig.json`, `jsconfig.json` | Must precisely locate dir containing `moduleResolution` |
| Rust | `Cargo.toml` | rust-analyzer handles workspace boundaries itself; nearest wins |
| Python | `pyproject.toml`, `setup.py`, `setup.cfg`, `requirements.txt` | Nearest |
| Go | `go.mod` | Must be at rootUri or ancestor |
| C/C++ | `compile_commands.json`, `CMakeLists.txt`, `Makefile`, `.clangd` | compile_commands.json prioritized |
| Java | `pom.xml`, `build.gradle`, `build.gradle.kts`, `settings.gradle` | jdtls needs precise project root |
| Kotlin | `build.gradle.kts`, `settings.gradle.kts` | Same as Java |
| JSON | — | Rootless, use workspace root directly |
| YAML | — | Rootless |
| HTML | — | Rootless |
| CSS | — | Rootless |
| Markdown | — | Rootless |

Empty = rootless language, no discovery logic, use workspace root directly.

## 5. Discovery Algorithm

```
Input: filePath (absolute), language, workspaceRoot (monorepo root, upper bound)
Output: projectRoot (absolute)

1. markers = root_markers[language]
2. If markers is empty → return workspaceRoot
3. currentDir = dirname(filePath)
4. Loop: walk currentDir up from file directory
     a. For each marker, check if currentDir/marker exists
     b. First match → return currentDir
     c. currentDir == workspaceRoot → also check once, then stop
5. Not found → return workspaceRoot (fallback)
```

### Example

Opening `apps/acowork-desktop/src/components/editor/MarkdownPreviewView.tsx`:

```
workspaceRoot = /Users/nicholas/projects/AgentCowork
filePath      = /Users/nicholas/projects/AgentCowork/apps/acowork-desktop/src/components/editor/MarkdownPreviewView.tsx

Walk:
  .../apps/acowork-desktop/src/components/editor/  → tsconfig.json? no
  .../apps/acowork-desktop/src/components/         → tsconfig.json? no
  .../apps/acowork-desktop/src/                    → tsconfig.json? no
  .../apps/acowork-desktop/                        → tsconfig.json? ✅

projectRoot = /Users/nicholas/projects/AgentCowork/apps/acowork-desktop
```

## 6. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│  Config Layer: lsp_servers.json with extended root_markers field   │
│                                                                     │
│  "typescript": {                                                    │
│    "candidates": [...],                                             │
│    "args": ["--stdio"],                                             │
│    "root_markers": ["tsconfig.json", "jsconfig.json"],             │
│    ...                                                              │
│  }                                                                  │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
              ┌────────────────┴────────────────┐
              ▼                                  ▼
┌──────────────────────────┐    ┌──────────────────────────────────┐
│  Frontend (TypeScript)    │    │  Relay Layer (Rust)               │
│                          │    │                                  │
│  lspProjectRoot.ts (NEW) │    │  project_root.rs (NEW)            │
│  discoverProjectRoot()   │    │  discover_project_root()          │
│                          │    │                                  │
│  Called in:              │    │  Called in:                       │
│  connectLanguage() of    │    │  codebase_rpc handler:            │
│  useLspClientPool        │    │  extract file URI from LSP params │
│  read relPath from       │    │  → walk up for marker files       │
│  Monaco model            │    │                                  │
│                          │    │                                  │
│  useLspClientPool (MOD)  │   │  codebase.rs (MOD)                │
│  pass projectRoot to     │    │  replace req.workspace_root with  │
│  buildLspWsUrl +         │    │  project_root                     │
│  workspaceFolder         │    │                                  │
│  keep workspaceRoot for  │    │                                  │
│  buildAbsoluteUri        │    │                                  │
└──────────────────────────┘    └──────────────────────────────────┘
```

## 7. Implementation Details

### 7.1 Config Layer: Extend `lsp_servers.json`

Add `root_markers` field to each language entry in `assets/lsp_servers.json`:

```json
{
  "typescript": {
    "candidates": ["typescript-language-server", "typescript-language-server.cmd"],
    "args": ["--stdio"],
    "root_markers": ["tsconfig.json", "jsconfig.json"],
    "install_hint": "npm install -g typescript-language-server typescript",
    "install_script": "typescript",
    "description": "TypeScript/JavaScript language server"
  },
  "rust": {
    "candidates": ["rust-analyzer"],
    "args": [],
    "root_markers": ["Cargo.toml"],
    "install_hint": "rustup component add rust-analyzer",
    "install_script": "rust",
    "description": "Rust language server (defaults to stdio, no --stdio flag)"
  }
}
```

`root_markers` is optional; missing = rootless language.

Corresponding Rust config struct (in `config.rs`) adds:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerEntry {
    pub candidates: Vec<String>,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "empty_candidate_args")]
    pub candidate_args: std::collections::HashMap<String, Vec<String>>,
    /// Files that indicate a project root for this language
    /// (e.g. ["tsconfig.json"] for TypeScript, ["Cargo.toml"] for Rust).
    /// Empty or missing = rootless language (use workspace root as-is).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_markers: Vec<String>,
    pub install_hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_script: Option<String>,
    pub description: String,
}
```

### 7.2 Frontend: `lspProjectRoot.ts` (new file)

Location: `apps/acowork-desktop/src/lib/lspProjectRoot.ts`

```typescript
import { exists } from "@tauri-apps/plugin-fs";

/** Language → root marker files. Empty array = rootless language. */
const ROOT_MARKERS: Record<string, string[]> = {
    typescript: ["tsconfig.json", "jsconfig.json"],
    javascript: ["tsconfig.json", "jsconfig.json"],
    rust: ["Cargo.toml"],
    python: ["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
    go: ["go.mod"],
    c: ["compile_commands.json", "CMakeLists.txt", "Makefile", ".clangd"],
    java: ["pom.xml", "build.gradle", "build.gradle.kts", "settings.gradle"],
    kotlin: ["build.gradle.kts", "settings.gradle.kts"],
    // Rootless languages — no markers, use workspace root directly
    json: [],
    yaml: [],
    html: [],
    css: [],
    markdown: [],
};

/**
 * Discover the language-specific project root for a given file.
 *
 * Walks up from the file's directory to the workspace root, checking
 * for language-specific marker files (tsconfig.json, Cargo.toml, etc.).
 * The first directory containing a marker file is returned as the
 * project root. If no marker is found, falls back to the workspace root.
 */
export async function discoverProjectRoot(
    filePath: string,
    language: string,
    workspaceRoot: string,
): Promise<string> {
    const markers = ROOT_MARKERS[language.toLowerCase()];
    if (!markers || markers.length === 0) {
        return workspaceRoot;
    }

    const root = workspaceRoot.replace(/\\/g, "/");
    let dir = filePath.replace(/\\/g, "/");

    // Start from file's directory (strip filename)
    const lastSlash = dir.lastIndexOf("/");
    if (lastSlash > 0) {
        dir = dir.substring(0, lastSlash);
    }

    // Walk up to workspace root (inclusive)
    while (dir.startsWith(root)) {
        for (const marker of markers) {
            const markerPath = `${dir}/${marker}`;
            try {
                if (await exists(markerPath)) {
                    return dir;
                }
            } catch {
                // exists() may fail on permission errors — treat as not found
            }
        }
        if (dir === root) break;

        const parentSlash = dir.lastIndexOf("/");
        if (parentSlash <= 0) break;
        dir = dir.substring(0, parentSlash);
    }

    return workspaceRoot;
}
```

### 7.3 Frontend: Modify `useLspClientPool.ts`

In `connectLanguage()`, discover project root before connecting LSP:

**Key changes:**

1. Import `discoverProjectRoot`
2. Inside `connectLanguage`, get `relPath` for the language from Monaco models
3. Build absolute path: `workspaceRoot + "/" + relPath`
4. Call `discoverProjectRoot(absPath, language, workspaceRoot)` → `projectRoot`
5. Use `projectRoot` instead of `workspaceRoot` for `buildLspWsUrl` and `workspaceFolder`
6. `buildAbsoluteUri` still uses `workspaceRoot` (unchanged)
7. Add `projectRoot` to `paramsKey` so different project roots trigger reconnection

```typescript
// Inside connectLanguage(), before buildLspWsUrl:

let projectRoot = workspaceRoot;
try {
    const monaco = await import("monaco-editor");
    const models = monaco.editor.getModels();
    const firstModel = models.find(m => m.getLanguageId() === language);
    if (firstModel) {
        const relPath = firstModel.uri.path.replace(/^\/+/, "");
        const absPath = `${workspaceRoot.replace(/\\/g, "/")}/${relPath}`;
        projectRoot = await discoverProjectRoot(absPath, language, workspaceRoot);
        console.log("[LSP] pool project root —", language,
            "workspace:", workspaceRoot, "→ project:", projectRoot);
    }
} catch (err) {
    console.warn("[LSP] pool project root discovery failed —", language, err);
}

// paramsKey must include projectRoot, otherwise switching projects won't reconnect
const paramsKey = `${language}|${agentId ?? ""}|${workspaceId ?? ""}|${workspaceRoot ?? ""}|${projectRoot}`;

// Connect LSP relay using projectRoot
const wsUrl = await buildLspWsUrl(language, projectRoot);

// Use projectRoot as Monaco workspaceFolder (determines rootUri)
const rootFolderUri = monaco.Uri.file(projectRoot.replace(/\\/g, "/"));
const clientOptions: LanguageClientOptions = {
    documentSelector: [],
    workspaceFolder: { uri: rootFolderUri, name: "workspace", index: 0 },
    // ... rest unchanged
};

// didOpen absolute URI still uses workspaceRoot (relPath is relative to monorepo root)
const absUri = buildAbsoluteUri(workspaceRoot, model.uri.path.replace(/^\/+/, ""));
```

### 7.4 Relay: `project_root.rs` (new file)

Location: `core/acowork-lsp-relay/src/project_root.rs`

```rust
//! Language-aware project root discovery.
//!
//! Given a file path and language, walks up the directory tree to find
//! the nearest directory containing a language-specific marker file
//! (tsconfig.json, Cargo.toml, go.mod, etc.). Falls back to the
//! workspace root if no marker is found.

use std::path::{Path, PathBuf};

/// Discover the project root for a given file and language.
///
/// Reads `root_markers` from the LSP server config (`lsp_servers.json`).
/// If the language has no root markers (rootless language), returns
/// `workspace_root` as-is.
pub fn discover_project_root(
    file_path: &str,
    language: &str,
    workspace_root: &str,
) -> String {
    let cfg = crate::config::lsp_servers_config();
    let canonical = crate::config::canonical_language(language);
    let entry = match cfg.servers.get(canonical) {
        Some(e) => e,
        None => return workspace_root.to_string(),
    };

    if entry.root_markers.is_empty() {
        return workspace_root.to_string();
    }

    let ws_root = Path::new(workspace_root);
    let file_dir = Path::new(file_path)
        .parent()
        .unwrap_or(ws_root);

    let mut current = file_dir;
    loop {
        for marker in &entry.root_markers {
            if current.join(marker).is_file() {
                return current.to_string_lossy().into_owned();
            }
        }

        if current == ws_root {
            break;
        }

        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    workspace_root.to_string()
}
```

### 7.5 Relay: Modify `codebase.rs`

In the `codebase_rpc` handler, extract file path from LSP params and discover the correct project root:

```rust
pub async fn codebase_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CodebaseRpcRequest>,
) -> Result<Json<CodebaseRpcResponse>, StatusCode> {
    // ... resolve LSP command (unchanged) ...

    // Discover the correct project root from the file path in the
    // LSP request params. Essential for languages that need a specific
    // project root (e.g. TypeScript needs tsconfig.json's directory,
    // not the monorepo root).
    let file_path = extract_file_path_from_params(&req.params);
    let project_root = match file_path {
        Some(path) => crate::project_root::discover_project_root(
            &path,
            &req.language,
            &req.workspace_root,
        ),
        None => req.workspace_root.clone(),
    };

    // Use project_root (not workspace_root) for pool and initialization
    let entry = match state
        .lsp_pool
        .get_or_spawn(&spec.command, &spec.args, &project_root)
        .await
    {
        // ...
    };

    // ... execute_codebase_rpc with project_root ...
}
```

`extract_file_path_from_params` parses `textDocument.uri` from LSP request params:

```rust
/// Extract a file system path from LSP request params.
fn extract_file_path_from_params(params: &serde_json::Value) -> Option<String> {
    let uri = params
        .get("textDocument")?
        .get("uri")?
        .as_str()?;
    let path = uri.strip_prefix("file://")?;
    Some(path.to_string())
}
```

## 8. Data Flow (After Changes)

### 8.1 WebSocket Relay Path

```
User opens MarkdownPreviewView.tsx
  ↓
FileEditorPanel.tsx
  workspaceRoot = treeRoots[agentId:workspaceId]  ← monorepo root
  → useLspClientPool(language=typescript, ..., workspaceRoot)
    ↓
connectLanguage("typescript")
  Get relPath from Monaco model = "apps/acowork-desktop/src/.../file.tsx"
  absPath = workspaceRoot + "/" + relPath
  projectRoot = discoverProjectRoot(absPath, "typescript", workspaceRoot)
             = ".../apps/acowork-desktop"  ← finds tsconfig.json
  ↓
buildLspWsUrl("typescript", projectRoot)
  → ws://relay/lsp/typescript?workspace_root=.../apps/acowork-desktop
  ↓
MonacoLanguageClient workspaceFolder = projectRoot
  → initialize rootUri = file://.../apps/acowork-desktop
  → tsserver finds tsconfig.json → correct moduleResolution: "bundler"
  ↓
buildAbsoluteUri(workspaceRoot, relPath)
  → file://.../AgentCowork/apps/acowork-desktop/src/.../file.tsx  ← correct absolute path
```

### 8.2 Codebase RPC Path

```
Agent Runtime → POST /api/codebase/rpc
  { language: "typescript",
    workspace_root: ".../AgentCowork",       ← monorepo root
    method: "textDocument/definition",
    params: { textDocument: { uri: "file://.../file.tsx" }, position: {...} }
  }
  ↓
codebase_rpc handler
  file_path = extract_file_path_from_params(params)  ← "file://.../file.tsx"
  project_root = discover_project_root(file_path, "typescript", workspace_root)
              = ".../apps/acowork-desktop"
  ↓
pool.get_or_spawn(cmd, args, project_root)  ← use project_root as pool key
  → If project_root already has LSP process → reuse
  → Else → new process, current_dir = project_root
  ↓
ensure_initialized(entry, project_root)
  → rootUri = file://.../apps/acowork-desktop
  → tsserver finds tsconfig.json → correct resolution
```

## 9. Edge Cases

| Scenario | Handling |
|----------|----------|
| Multiple projects for same language (e.g. two tsconfig.json) | Pool key includes project_root; auto-isolated into separate LSP processes |
| File directly under monorepo root (no project marker) | Fallback to workspace root |
| Rust workspace Cargo.toml vs crate Cargo.toml | Nearest Cargo.toml; rust-analyzer handles workspace boundaries |
| Bot Runtime codebase RPC params have no file URI | Fallback to `req.workspace_root` |
| Marker file check fails (permission/IO error) | `exists()` exception treated as not found, continue walking up |
| Rootless languages (JSON/YAML/HTML/CSS/Markdown) | No discovery; use workspace root directly |
| Tauri fs plugin unavailable (non-Tauri env) | `discoverProjectRoot` catches exception, fallback to workspace root |

## 10. Unchanged Components

The following components **do not need modification**:

- **`pool.rs` (LSP process pool)**: Key already includes `workspace_root`; different roots auto-isolate
- **`relay.rs` (WebSocket bidirectional proxy)**: Transparent forwarding, does not care about rootUri content
- **`buildAbsoluteUri()`**: Still uses workspace root to build file paths
- **LSP relay WebSocket endpoint signature**: `?workspace_root=` parameter meaning shifts from "workspace root" to "project root", but the interface is unchanged

## 11. Implementation Order

1. **Config layer**: Extend `lsp_servers.json` + `config.rs` struct (add `root_markers` field)
2. **Relay layer**: Add `project_root.rs` + modify `codebase.rs`
3. **Frontend layer**: Add `lspProjectRoot.ts` + modify `useLspClientPool.ts`
4. **Verify**: Open TypeScript file — confirm red squiggles disappear; open Rust file — confirm unaffected

## 12. Testing Strategy

| Test Item | Method |
|-----------|--------|
| TypeScript project root discovery | Open .tsx files under `apps/acowork-desktop/src/`; confirm no "Cannot find module" |
| Rust unaffected | Open .rs files under `core/acowork-core/src/`; confirm rust-analyzer works normally |
| Multi-project isolation | (If multiple tsconfig.json exist) Open files from different projects; confirm separate LSP processes |
| Rootless languages | Open .json / .yaml files; confirm LSP connects normally |
| Codebase RPC | Bot Runtime sends `textDocument/definition` request; confirm correct results |
| Fallback scenario | Open files directly under monorepo root (no marker); confirm fallback to workspace root |
