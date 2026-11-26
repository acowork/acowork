# 多语言 LSP 项目根目录发现机制

## 1. 问题背景

### 1.1 现象

在 acowork-desktop 中通过 File Tab 打开 TypeScript 源码文件（如 `MarkdownPreviewView.tsx`），LSP 报大量 "Cannot find module" 红线，但 `tsc --noEmit` 编译无错误。

### 1.2 根因

acowork 是 Rust + TypeScript 融合的 monorepo，workspace root（如 `/Users/nicholas/projects/AgentCowork`）下没有 `tsconfig.json`。LSP relay 将这个 monorepo root 作为 `rootUri` 传给 `typescript-language-server`，tsserver 在 rootUri 目录找不到 `tsconfig.json`，便创建一个**默认项目**，使用 TypeScript 默认的模块解析规则（不含 `moduleResolution: "bundler"`），导致 `react-markdown`、`remark-gfm` 等通过 `exports` 字段声明类型的包解析失败。

### 1.3 影响范围

不仅是 TypeScript，所有在 `lsp_servers.json` 中声明的语言都面临类似问题：

| 语言 | LSP Server | rootUri 依赖 | 当前表现 |
|------|------------|-------------|---------|
| TypeScript | typescript-language-server | **强依赖** | rootUri 无 tsconfig → 默认项目 → 模块解析失败 |
| Rust | rust-analyzer | 弱依赖 | 从文件路径向上找 Cargo.toml，基本可工作 |
| Python | pyright / pylsp | 中等 | 从文件路径向上找 pyproject.toml，基本可工作 |
| Go | gopls | **强依赖** | rootUri 需包含 go.mod 或其祖先有 |
| C/C++ | clangd | **强依赖** | 需要 compile_commands.json 在 rootUri 子目录 |
| Java | jdtls | **强依赖** | 需要 pom.xml / build.gradle 在 rootUri |
| Kotlin | kotlin-language-server | **强依赖** | 同 Java |
| JSON/YAML/HTML/CSS/Markdown | 各自 LSP | 无依赖 | rootless，monorepo root 可工作 |

### 1.4 VSCode 的做法

VSCode 在 `initialize` 时也将 `rootUri` 设为 workspace root，但 tsserver 在收到 `textDocument/didOpen` 时会**从文件目录向上查找 `tsconfig.json`**，找到后创建独立的 TypeScript Project，使用该 tsconfig 的 `compilerOptions`。VSCode 的 TypeScript 扩展还会主动扫描 workspace 内所有 tsconfig.json 并注册为独立项目。

acowork 的 LSP relay 当前只传了一个 rootUri，没有利用 `didOpen` 的文件路径做二次发现。

## 2. 设计目标

1. **全语言覆盖**：`lsp_servers.json` 中声明的所有语言都纳入方案
2. **自动发现**：根据文件路径 + 语言自动推导项目根，无需用户手动配置
3. **最小改动**：利用现有 LSP Pool 的 key 隔离机制，不改 pool 层
4. **声明式扩展**：新增语言只需在配置中加一行标记文件
5. **双路径覆盖**：WebSocket relay 路径（编辑器交互）和 Codebase RPC 路径（Bot Runtime 代码智能）都支持

## 3. 核心概念：Project Root vs Workspace Root

引入两个独立的概念：

| 概念 | 用途 | 来源 |
|------|------|------|
| **Workspace Root** | 拼接文件绝对路径（`buildAbsoluteUri`） | `treeRoots[agentId:workspaceId]`，即 monorepo root |
| **Project Root** | LSP 连接参数（`rootUri`、pool key、工作目录） | 从文件路径 + 语言标记文件自动发现 |

两者通过「文件路径 + 语言」推导，不需要用户介入。

## 4. 语言标记文件表

每种语言声明一组「标记文件」（root markers），LSP relay 在文件目录向上遍历时检查这些文件是否存在。第一个包含标记文件的目录即为 Project Root。

| 语言 | 标记文件 | 说明 |
|------|---------|------|
| TypeScript | `tsconfig.json`, `jsconfig.json` | 必须精确定位到含 `moduleResolution` 的目录 |
| Rust | `Cargo.toml` | rust-analyzer 自身能处理 workspace 边界，取最近的即可 |
| Python | `pyproject.toml`, `setup.py`, `setup.cfg`, `requirements.txt` | 取最近的 |
| Go | `go.mod` | 必须在 rootUri 或其祖先 |
| C/C++ | `compile_commands.json`, `CMakeLists.txt`, `Makefile`, `.clangd` | compile_commands.json 优先 |
| Java | `pom.xml`, `build.gradle`, `build.gradle.kts`, `settings.gradle` | jdtls 需要精确的项目根 |
| Kotlin | `build.gradle.kts`, `settings.gradle.kts` | 同 Java |
| JSON | （无） | rootless，直接用 workspace root |
| YAML | （无） | rootless |
| HTML | （无） | rootless |
| CSS | （无） | rootless |
| Markdown | （无） | rootless |

标记为空的 = rootless 语言，不执行发现逻辑，直接使用 workspace root。

## 5. 发现算法

```
输入：filePath (绝对路径), language, workspaceRoot (monorepo root, 上界)
输出：projectRoot (绝对路径)

1. markers = root_markers[language]
2. 若 markers 为空 → 返回 workspaceRoot
3. currentDir = dirname(filePath)
4. 循环：currentDir 从文件目录逐级向上
     a. 对每个 marker，检查 currentDir/marker 是否存在
     b. 找到第一个 → 返回 currentDir
     c. currentDir == workspaceRoot 时也检查一次，然后终止
5. 未找到 → 返回 workspaceRoot (fallback)
```

### 示例

打开 `apps/acowork-desktop/src/components/editor/MarkdownPreviewView.tsx`：

```
workspaceRoot = /Users/nicholas/projects/AgentCowork
filePath      = /Users/nicholas/projects/AgentCowork/apps/acowork-desktop/src/components/editor/MarkdownPreviewView.tsx

遍历：
  .../apps/acowork-desktop/src/components/editor/  → tsconfig.json? 否
  .../apps/acowork-desktop/src/components/         → tsconfig.json? 否
  .../apps/acowork-desktop/src/                    → tsconfig.json? 否
  .../apps/acowork-desktop/                        → tsconfig.json? ✅

projectRoot = /Users/nicholas/projects/AgentCowork/apps/acowork-desktop
```

## 6. 架构总览

```
┌─────────────────────────────────────────────────────────────────────┐
│  配置层：lsp_servers.json 扩展 root_markers 字段                    │
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
│  前端层 (TypeScript)      │    │  Relay 层 (Rust)                  │
│                          │    │                                  │
│  lspProjectRoot.ts (NEW) │    │  project_root.rs (NEW)            │
│  discoverProjectRoot()   │    │  discover_project_root()          │
│                          │    │                                  │
│  调用时机：               │    │  调用时机：                       │
│  useLspClientPool 的      │    │  codebase_rpc handler 中          │
│  connectLanguage() 中     │    │  从 LSP params 提取 file URI      │
│  从 Monaco model 获取     │    │  → 向上找标记文件                 │
│  relPath → 向上找标记     │    │                                  │
│                          │    │                                  │
│  useLspClientPool (MODIFY)│   │  codebase.rs (MODIFY)             │
│  传入 projectRoot 给      │    │  用 project_root 替换             │
│  buildLspWsUrl +          │    │  req.workspace_root              │
│  workspaceFolder          │    │                                  │
│  保留 workspaceRoot 给    │    │                                  │
│  buildAbsoluteUri         │    │                                  │
└──────────────────────────┘    └──────────────────────────────────┘
```

## 7. 实现细节

### 7.1 配置层：扩展 `lsp_servers.json`

在 `assets/lsp_servers.json` 的每个语言条目中新增 `root_markers` 字段：

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

`root_markers` 为可选字段，缺失时视为 rootless 语言。

对应的 Rust 配置结构体（`config.rs`）需新增字段：

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

### 7.2 前端层：`lspProjectRoot.ts`（新文件）

位置：`apps/acowork-desktop/src/lib/lspProjectRoot.ts`

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
 *
 * @param filePath    Absolute path of the file being opened
 * @param language    Language id (e.g. "typescript", "rust")
 * @param workspaceRoot  Monorepo root (upper bound for the search)
 * @returns Project root directory (absolute path)
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

    // Normalize separators to forward slash
    const root = workspaceRoot.replace(/\\/g, "/");
    let dir = filePath.replace(/\\/g, "/");

    // Start from the file's directory (strip filename)
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

### 7.3 前端层：修改 `useLspClientPool.ts`

在 `connectLanguage()` 函数中，连接 LSP 前先发现 project root：

**改动要点：**

1. 导入 `discoverProjectRoot`
2. 在 `connectLanguage` 内部，从 Monaco models 获取该语言的 `relPath`
3. 拼接绝对路径：`workspaceRoot + "/" + relPath`
4. 调用 `discoverProjectRoot(absPath, language, workspaceRoot)` 得到 `projectRoot`
5. 用 `projectRoot` 替代 `workspaceRoot` 传给 `buildLspWsUrl` 和 `workspaceFolder`
6. `buildAbsoluteUri` 仍用 `workspaceRoot`（不变）
7. `paramsKey` 加入 `projectRoot`，确保不同 project root 触发重连

```typescript
// 在 connectLanguage() 内部，buildLspWsUrl 调用之前：

// Discover language-specific project root from the first open model.
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
    // Fallback: use workspace root
}

// paramsKey 必须包含 projectRoot，否则切换项目时不会重连
const paramsKey = `${language}|${agentId ?? ""}|${workspaceId ?? ""}|${workspaceRoot ?? ""}|${projectRoot}`;

// 用 projectRoot 连接 LSP relay
const wsUrl = await buildLspWsUrl(language, projectRoot);

// 用 projectRoot 作为 Monaco workspaceFolder（决定 rootUri）
const rootFolderUri = monaco.Uri.file(projectRoot.replace(/\\/g, "/"));
const clientOptions: LanguageClientOptions = {
    documentSelector: [],
    workspaceFolder: { uri: rootFolderUri, name: "workspace", index: 0 },
    // ... 其余不变
};

// didOpen 的绝对 URI 仍用 workspaceRoot（relPath 是相对于 monorepo root 的）
const absUri = buildAbsoluteUri(workspaceRoot, model.uri.path.replace(/^\/+/, ""));
```

### 7.4 Relay 层：`project_root.rs`（新文件）

位置：`core/acowork-lsp-relay/src/project_root.rs`

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

### 7.5 Relay 层：修改 `codebase.rs`

在 `codebase_rpc` handler 中，从 LSP params 提取文件路径，发现正确的 project root：

```rust
pub async fn codebase_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CodebaseRpcRequest>,
) -> Result<Json<CodebaseRpcResponse>, StatusCode> {
    // ... resolve LSP command (unchanged) ...

    // Discover the correct project root from the file path in the
    // LSP request params. This is essential for languages that need
    // a specific project root (e.g. TypeScript needs the directory
    // containing tsconfig.json, not the monorepo root).
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

`extract_file_path_from_params` 从 LSP 请求参数中提取文件路径。LSP `textDocument/*` 方法的参数中包含 `textDocument.uri` 字段（如 `file:///Users/.../file.tsx`），从中解析出文件系统路径。

```rust
/// Extract a file system path from LSP request params.
///
/// LSP textDocument methods include a `textDocument.uri` field
/// (e.g. "file:///Users/foo/project/src/file.ts"). This function
/// parses the URI and returns the file system path.
fn extract_file_path_from_params(params: &serde_json::Value) -> Option<String> {
    let uri = params
        .get("textDocument")?
        .get("uri")?
        .as_str()?;

    // Convert file:// URI to filesystem path
    let path = uri.strip_prefix("file://")?;
    // On Windows: file:///C:/... → C:/...
    // On Unix:    file:///Users/... → /Users/...
    Some(path.to_string())
}
```

## 8. 数据流（改动后）

### 8.1 WebSocket relay 路径

```
用户打开 MarkdownPreviewView.tsx
  ↓
FileEditorPanel.tsx
  workspaceRoot = treeRoots[agentId:workspaceId]  ← monorepo root
  → useLspClientPool(language=typescript, ..., workspaceRoot)
    ↓
connectLanguage("typescript")
  从 Monaco model 获取 relPath = "apps/acowork-desktop/src/.../file.tsx"
  absPath = workspaceRoot + "/" + relPath
  projectRoot = discoverProjectRoot(absPath, "typescript", workspaceRoot)
             = ".../apps/acowork-desktop"  ← 发现 tsconfig.json
  ↓
buildLspWsUrl("typescript", projectRoot)
  → ws://relay/lsp/typescript?workspace_root=.../apps/acowork-desktop
  ↓
MonacoLanguageClient workspaceFolder = projectRoot
  → initialize rootUri = file://.../apps/acowork-desktop
  → tsserver 找到 tsconfig.json → 正确的 moduleResolution: "bundler"
  ↓
buildAbsoluteUri(workspaceRoot, relPath)
  → file://.../AgentCowork/apps/acowork-desktop/src/.../file.tsx  ← 正确的绝对路径
```

### 8.2 Codebase RPC 路径

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
pool.get_or_spawn(cmd, args, project_root)  ← 用 project_root 做 pool key
  → 若该 project_root 已有 LSP 进程 → 复用
  → 否则 → 新进程，current_dir = project_root
  ↓
ensure_initialized(entry, project_root)
  → rootUri = file://.../apps/acowork-desktop
  → tsserver 找到 tsconfig.json → 正确解析
```

## 9. 边界情况

| 场景 | 处理方式 |
|------|---------|
| 同一语言多个项目（如两个 tsconfig.json） | Pool key 包含 project_root，自动隔离为独立 LSP 进程 |
| 文件在 monorepo root 直接下（无项目标记） | fallback 到 workspace root |
| Rust workspace Cargo.toml vs crate Cargo.toml | 取最近的 Cargo.toml；rust-analyzer 自身能处理 workspace 边界 |
| Bot Runtime codebase RPC 的 params 中无 file URI | fallback 到 `req.workspace_root` |
| 标记文件检查失败（权限/IO 错误） | `exists()` 异常视为不存在，继续向上查找 |
| rootless 语言（JSON/YAML/HTML/CSS/Markdown） | 不执行发现，直接用 workspace root |
| Tauri fs plugin 不可用（非 Tauri 环境） | `discoverProjectRoot` 捕获异常，fallback 到 workspace root |

## 10. 不变的部分

以下组件**不需要修改**：

- **`pool.rs`（LSP 进程池）**：key 已包含 `workspace_root`，不同 root 自动隔离
- **`relay.rs`（WebSocket 双向代理）**：透明转发，不关心 rootUri 内容
- **`buildAbsoluteUri()`**：仍用 workspace root 拼接文件路径
- **LSP relay 的 WebSocket 端点签名**：`?workspace_root=` 参数含义从「workspace root」变为「project root」，但接口不变

## 11. 实施顺序

1. **配置层**：扩展 `lsp_servers.json` + `config.rs` 结构体（加 `root_markers` 字段）
2. **Relay 层**：新增 `project_root.rs` + 修改 `codebase.rs`
3. **前端层**：新增 `lspProjectRoot.ts` + 修改 `useLspClientPool.ts`
4. **验证**：打开 TypeScript 文件确认红线消失；打开 Rust 文件确认不受影响

## 12. 测试策略

| 测试项 | 方法 |
|--------|------|
| TypeScript 项目根发现 | 打开 `apps/acowork-desktop/src/` 下的 .tsx 文件，确认 LSP 不再报 "Cannot find module" |
| Rust 不受影响 | 打开 `core/acowork-core/src/` 下的 .rs 文件，确认 rust-analyzer 正常工作 |
| 多项目隔离 | （若存在多个 tsconfig.json）分别打开不同项目的文件，确认各自使用独立的 LSP 进程 |
| rootless 语言 | 打开 .json / .yaml 文件，确认 LSP 正常连接 |
| Codebase RPC | Bot Runtime 发起 `textDocument/definition` 请求，确认返回正确结果 |
| fallback 场景 | 打开 monorepo root 下的文件（无标记文件），确认 fallback 到 workspace root |
