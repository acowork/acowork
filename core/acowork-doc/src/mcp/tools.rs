//! MCP 工具分发（设计 §6 全量 8 工具）。
//!
//! 每个工具：解析参数 → 身份校验（写工具需可信 runtime instance_id
//! (UUID, ADR-073)，匿名只读，设计 §9）→ 经 [`crate::state::DocState`]
//! 调 **service trait** →
//! 返回精简 JSON（不向 LLM 暴露内部存储细节）。
//!
//! ## 寻址
//!
//! 设计用人类可读 path（`项目A/纪要.md`），而库内部用 `dir_id` + 文件名
//! 寻址（D1 决策：目录物理名即 `dir-{hex}`，同级名唯一）。本模块的
//! `resolve_*` 把 path 逐级解析为内部 id —— 解析只依赖 service trait
//! （`DirectoryService::list_tree` / `DocumentService::list`），不触碰
//! `store`/`types`。

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{DocError, Result};
use crate::path::validate_request_id;
use crate::service::directory::{CreateDirectoryInput, DirectoryService};
use crate::service::document::{CreateDocumentInput, DocumentService};
use crate::service::request::{RequestService, SubmitRequestInput};
use crate::service::search::SearchService;
use crate::state::DocState;
use crate::types::{DocMeta, ImportSource, RequestStatus, ROOT_DIR_ID};

// ── 身份与参数辅助 ──────────────────────────────────────────────────────

/// 要求调用方具备可信身份（匿名 → Forbidden）。所有写工具先过此关。
fn require_actor(actor: Option<&str>) -> Result<&str> {
    actor.ok_or_else(|| {
        DocError::Forbidden(actor.map(str::to_string))
    })
}

/// 宽松参数解析：缺省字段走 `default`；未知字段忽略；类型错误给清晰信息。
fn parse_args<T: for<'de> Deserialize<'de>>(name: &str, args: Value) -> Result<T> {
    serde_json::from_value(args)
        .map_err(|e| DocError::BadRequest(format!("invalid arguments for {name}: {e}")))
}

// ── path ↔ dir_id / doc 解析（设计 §6 寻址约定）─────────────────────────

/// 目录 path（`""` | `"/"` | `"研发"` | `"研发/文档库"`）→ `dir_id`。
async fn resolve_dir(dirs: &dyn DirectoryService, path: &str) -> Result<String> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return Ok(ROOT_DIR_ID.to_string());
    }
    let mut current = ROOT_DIR_ID.to_string();
    for seg in path.split('/') {
        let tree = dirs.list_tree(&current).await?;
        let next = tree
            .dirs
            .iter()
            .find(|d| !d.deleted && d.name == seg)
            .map(|d| d.dir_id.clone())
            .ok_or_else(|| DocError::DirNotFound(format!("directory path segment: {seg}")))?;
        current = next;
    }
    Ok(current)
}

/// 文档引用：`doc_id`（`doc-` 前缀）或 path（`"纪要.md"` / `"研发/纪要.md"`）
/// → `(dir_id, doc_id)`。
async fn resolve_doc_ref(
    dirs: &dyn DirectoryService,
    docs: &dyn DocumentService,
    reference: &str,
) -> Result<(String, String)> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(DocError::BadRequest("ref must not be empty".into()));
    }
    if reference.starts_with("doc-") {
        let meta = docs.read(reference).await?.meta;
        return Ok((meta.doc_id.clone(), meta.doc_id));
    }
    // path：最后一段是文件名（.md），前面是目录链
    let (dir_path, file) = match reference.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", reference),
    };
    let stem = file
        .strip_suffix(".md")
        .ok_or_else(|| {
            DocError::BadRequest(format!(
                "document path must end in .md, got: {reference}"
            ))
        })?;
    if stem.is_empty() || stem.contains('/') {
        return Err(DocError::BadRequest(format!("invalid document path: {reference}")));
    }
    let dir_id = resolve_dir(dirs, dir_path).await?;
    let metas = docs.list(&dir_id).await?;
    let meta = metas
        .iter()
        .find(|m| m.name == stem)
        .ok_or_else(|| DocError::DocNotFound(reference.to_string()))?;
    Ok((dir_id, meta.doc_id.clone()))
}

/// 目录 path 缺省解析（`""` → root）。
async fn resolve_dir_or_root(dirs: &dyn DirectoryService, path: &str) -> Result<String> {
    resolve_dir(dirs, path).await
}

// ── 只读工具 ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ListArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    100
}

/// `doc_list` — 目录树逐级浏览（含 query 过滤 + 分页）。
async fn doc_list(state: &DocState, args: Value) -> Result<Value> {
    let a: ListArgs = parse_args("doc_list", args)?;
    let limit = a.limit.clamp(1, 500);
    let dir_id = resolve_dir_or_root(state.dirs.as_ref(), &a.path).await?;
    let tree = state.dirs.list_tree(&dir_id).await?;

    let mut entries: Vec<Value> = Vec::new();
    for d in tree.dirs {
        if d.deleted {
            continue;
        }
        if !a.query.is_empty() && !d.name.to_lowercase().contains(&a.query.to_lowercase()) {
            continue;
        }
        entries.push(json!({
            "kind": "dir",
            "dir_id": d.dir_id,
            "name": d.name,
        }));
    }
    for f in tree.files {
        if f.deleted {
            continue;
        }
        if !a.query.is_empty() && !f.name.to_lowercase().contains(&a.query.to_lowercase()) {
            continue;
        }
        entries.push(json!({
            "kind": "doc",
            "doc_id": f.doc_id,
            "name": f.name,
            "version": f.version,
            "updated_at": f.updated_at,
        }));
    }
    let total = entries.len();
    let items = entries
        .into_iter()
        .skip(a.offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(json!({ "path": if dir_id == ROOT_DIR_ID { "" } else { &a.path }, "total": total, "items": items }))
}

#[derive(Deserialize)]
struct DocRefArgs {
    #[serde(rename = "ref")]
    reference: String,
}

/// `doc_read` — 读取文档（Markdown 原文 + version + 元数据）。
async fn doc_read(state: &DocState, args: Value) -> Result<Value> {
    let a: DocRefArgs = parse_args("doc_read", args)?;
    let (_dir, doc_id) =
        resolve_doc_ref(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;
    let read = state.docs.read(&doc_id).await?;
    doc_to_value(read.meta, Some(read.content), read.path, None)
}

/// `doc_pull` — doc_read + base_version + 建议缓存路径（§5.5）。
///
/// 注意：doc 进程与 Agent 工作区可能不在同一信任边界（远程 Agent），
/// 服务端**不落盘**；返回内容 + base_version + 建议缓存相对路径
/// （`.acowork/tmp/docs/{doc_id}.md`），由调用方（Runtime / Agent 文件工具）
/// 写入其工作区。这是对设计 §5.5「服务端落盘」的适配 —— 落盘职责在
/// Agent 侧文件能力。
async fn doc_pull(state: &DocState, args: Value) -> Result<Value> {
    let a: DocRefArgs = parse_args("doc_pull", args)?;
    let (_dir, doc_id) =
        resolve_doc_ref(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;
    let read = state.docs.read(&doc_id).await?;
    let base_version = read.meta.version;
    let cache_path = format!(".acowork/tmp/docs/{}.md", read.meta.doc_id);
    let mut v = doc_to_value(read.meta, Some(read.content), read.path, Some(&cache_path))?;
    // §5.5：pull 的版本号即后续 doc_submit_update 的 base_version。
    v["base_version"] = Value::from(base_version);
    Ok(v)
}

/// 共享的文档响应序列化（doc_read / doc_pull 复用）。
fn doc_to_value(
    meta: DocMeta,
    content: Option<String>,
    path: String,
    cache_path: Option<&str>,
) -> Result<Value> {
    let mut v = json!({
        "doc_id": meta.doc_id,
        "name": meta.name,
        "path": path,
        "version": meta.version,
        "created_at": meta.created_at,
        "updated_at": meta.updated_at,
    });
    if let Some(c) = content {
        v["content"] = Value::String(c);
    }
    if let Some(cp) = cache_path {
        v["cache_path"] = Value::String(cp.to_string());
    }
    if let Some(imp) = meta.import {
        // ADR-073: wire key is `instance_id` (UUID carried over `X-MCP-Actor`
        // by the Gateway catalog). The legacy `agent_id` key was removed
        // because a runtime instance — not a package id — is what the
        // import badge should correlate to.
        v["import"] = json!({ "instance_id": imp.instance_id, "workspace_path": imp.workspace_path });
    }
    Ok(v)
}

#[derive(Deserialize)]
struct SearchArgs {
    keyword: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    20
}

/// `doc_search` — 跨目录关键字检索。
async fn doc_search(state: &DocState, args: Value) -> Result<Value> {
    let a: SearchArgs = parse_args("doc_search", args)?;
    let hits = state.search.search(&a.keyword, a.limit.clamp(1, 100)).await?;
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            json!({
                "doc_id": h.doc_id,
                "name": h.name,
                "path": h.path,
                "snippet": h.snippet,
                "score": h.score,
            })
        })
        .collect();
    Ok(json!({ "keyword": a.keyword, "hits": hits }))
}

// ── 写工具（需身份）──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct MkdirArgs {
    path: String,
}

/// `doc_mkdir` — 创建子目录（父须已存在，逐级创建）。
async fn doc_mkdir(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: MkdirArgs = parse_args("doc_mkdir", args)?;
    let path = a.path.trim().trim_matches('/');
    let (parent_path, name) = match path.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", path),
    };
    if name.is_empty() {
        return Err(DocError::BadRequest(
            "doc_mkdir requires a non-empty `path`".into(),
        ));
    }
    let parent_dir_id = resolve_dir_or_root(state.dirs.as_ref(), parent_path).await?;
    let meta = state
        .dirs
        .create(CreateDirectoryInput {
            parent_dir_id,
            name: name.to_string(),
            // Provenance only — MCP agent calls are not ACL-enforced in v1
            // (see `service::acl` module docs); the creator stamp keeps
            // "who made this" answerable.
            creator: Some(actor.to_string()),
        })
        .await?;
    Ok(json!({
        "dir_id": meta.dir_id,
        "name": meta.name,
        "path": path,
    }))
}

#[derive(Deserialize)]
struct AddArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    title: Option<String>,
    content: String,
    #[serde(default)]
    source_workspace: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
}

/// `doc_add` — add-to-doc 快照导入（新文档直接生效，不审核）。
async fn doc_add(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: AddArgs = parse_args("doc_add", args)?;
    let dir_id = resolve_dir_or_root(state.dirs.as_ref(), &a.path).await?;
    // title 缺省 → source_path 文件 stem；都缺 → 400
    let title = match a.title {
        Some(t) if !t.trim().is_empty() => t,
        _ => match a.source_path.as_deref().and_then(stem_of_path) {
            Some(s) => s,
            None => {
                return Err(DocError::BadRequest(
                    "doc_add requires a `title` when `source_path` is omitted".into(),
                ));
            }
        },
    };
    let workspace_path = match (&a.source_workspace, &a.source_path) {
        (Some(w), Some(p)) => format!("{w}:{p}"),
        (Some(w), None) => w.clone(),
        (None, Some(p)) => p.clone(),
        (None, None) => String::new(),
    };
    let import = ImportSource {
        // ADR-073: `actor` is the runtime instance identity (UUID) — the
        // value substituted into the Gateway's `{instance_id}` MCP template
        // for doc. Storing it as `instance_id` keeps the wire format
        // consistent with `UpdateRequest::submitted_by` and the agentStore
        // key on desktop.
        instance_id: actor.to_string(),
        workspace_path,
    };
    let meta = state
        .docs
        .create(CreateDocumentInput {
            parent_dir_id: dir_id,
            title,
            content: a.content,
            import: Some(import),
            creator: Some(actor.to_string()),
        })
        .await?;
    let path = state.docs.path_of(&meta.doc_id).await?;
    Ok(json!({
        "doc_id": meta.doc_id,
        "name": meta.name,
        "path": path,
        "version": meta.version,
    }))
}

/// 从源路径取文件 stem（`C:\a\report.md` → `report`；无扩展名 → 文件名）。
fn stem_of_path(p: &str) -> Option<String> {
    let base = p.rsplit(['/', '\\']).next().unwrap_or(p);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    if stem.is_empty() { None } else { Some(stem.to_string()) }
}

#[derive(Deserialize)]
struct SubmitUpdateArgs {
    #[serde(rename = "ref")]
    reference: String,
    content: String,
    base_version: u64,
}

/// `doc_submit_update` — PR 式更新请求（不直接写库）。
async fn doc_submit_update(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: SubmitUpdateArgs = parse_args("doc_submit_update", args)?;
    let (_dir, doc_id) =
        resolve_doc_ref(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;
    let req = state
        .requests
        .submit(SubmitRequestInput {
            doc_id,
            base_version: a.base_version,
            content: a.content,
            submitted_by: actor.to_string(),
        })
        .await?;
    Ok(json!({
        "request_id": req.request_id,
        "doc_id": req.doc_id,
        "path": req.path,
        "base_version": req.base_version,
        "status": "pending",
    }))
}

#[derive(Deserialize)]
struct CheckRequestArgs {
    request_id: String,
}

/// `doc_check_request` — 查询审核状态（只读，匿名可查）。
async fn doc_check_request(state: &DocState, args: Value) -> Result<Value> {
    let a: CheckRequestArgs = parse_args("doc_check_request", args)?;
    validate_request_id(&a.request_id)?;
    let req = state.requests.get(&a.request_id).await?;
    let status = match req.status {
        RequestStatus::Pending => "pending",
        RequestStatus::Approved => "approved",
        RequestStatus::Rejected => "rejected",
        RequestStatus::Expired => "expired",
    };
    let mut v = json!({
        "request_id": req.request_id,
        "doc_id": req.doc_id,
        "status": status,
    });
    if let Some(note) = &req.review_note {
        v["review_note"] = Value::String(note.clone());
    }
    if let Some(by) = &req.reviewed_by {
        v["reviewed_by"] = Value::String(by.clone());
    }
    if let Some(at) = req.reviewed_at {
        v["reviewed_at"] = Value::String(at.to_rfc3339());
    }
    Ok(v)
}

// ── dispatch ─────────────────────────────────────────────────────────────

/// 按工具名分发；写工具先过 `require_actor`（设计 §9）。
pub async fn dispatch(state: &DocState, actor: Option<&str>, name: &str, args: Value) -> Result<Value> {
    match name {
        // ── 只读（匿名允许）─────────────────────────────────────────
        "doc_list" => doc_list(state, args).await,
        "doc_read" => doc_read(state, args).await,
        "doc_pull" => doc_pull(state, args).await,
        "doc_search" => doc_search(state, args).await,
        "doc_check_request" => doc_check_request(state, args).await,

        // ── 写（需身份）────────────────────────────────────────────
        "doc_mkdir" => {
            let actor = require_actor(actor)?;
            doc_mkdir(state, actor, args).await
        }
        "doc_add" => {
            let actor = require_actor(actor)?;
            doc_add(state, actor, args).await
        }
        "doc_submit_update" => {
            let actor = require_actor(actor)?;
            doc_submit_update(state, actor, args).await
        }

        // 未知工具 → 协议级 method-not-found（tools/call 已校验存在，
        // 防御未知分支）
        other => Err(DocError::BadRequest(format!("unknown tool: {other}"))),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocConfig;
    use tempfile::TempDir;

    // ADR-073: `actor` strings here stand in for the runtime instance
    // identity (UUID). The doc server doesn't enforce UUID format — the
    // Gateway doc_proxy is the trust boundary — but using a UUID-shaped
    // string in tests ensures any future `instance_id` format check in
    // the doc crate (defense-in-depth) won't have to be loosened just
    // for tests.
    const AGENT: &str = "inst-0000-0000-0000-0000-000000000001";

    async fn setup() -> (TempDir, DocState) {
        let tmp = TempDir::new().unwrap();
        let cfg = DocConfig {
            data_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let state = DocState::new(cfg).await.unwrap();
        (tmp, state)
    }

    /// 成功调用的 content（错误则 panic 并打印）。
    async fn ok(state: &DocState, actor: Option<&str>, name: &str, args: Value) -> Value {
        dispatch(state, actor, name, args).await.unwrap_or_else(|e| {
            panic!("tool {name} should succeed, got: {e:?}");
        })
    }

    /// 期望失败：断言 `DocError` 匹配。
    async fn err(
        state: &DocState,
        actor: Option<&str>,
        name: &str,
        args: Value,
    ) -> DocError {
        dispatch(state, actor, name, args).await.expect_err("expected error")
    }

    #[tokio::test]
    async fn mkdir_nested_and_add_read_roundtrip() {
        let (_tmp, state) = setup().await;
        // mkdir 逐级
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发" })).await;
        assert_eq!(v["name"], "研发");
        assert!(v["dir_id"].as_str().unwrap().starts_with("dir-"));
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发/文档库" })).await;
        assert_eq!(v["name"], "文档库");

        // add 进子目录
        let v = ok(
            &state,
            Some(AGENT),
            "doc_add",
            json!({
                "path": "研发/文档库",
                "title": "设计纪要",
                "content": "# v1 设计纪要",
                "source_workspace": "ws-main",
                "source_path": "notes/design.md",
            }),
        )
        .await;
        assert_eq!(v["version"], 1);
        assert_eq!(v["name"], "设计纪要");
        let doc_id = v["doc_id"].as_str().unwrap().to_string();

        // read by doc_id
        let by_id = ok(&state, None, "doc_read", json!({ "ref": doc_id })).await;
        assert_eq!(by_id["content"], "# v1 设计纪要");
        // ADR-073: the wire field is `instance_id` (UUID), populated from
        // the `X-MCP-Actor` header — which here is `AGENT` itself, used as
        // the actor token. The badge downstream resolves this UUID via the
        // agentStore to the human display name.
        assert_eq!(by_id["import"]["instance_id"], AGENT);
        assert_eq!(by_id["import"]["workspace_path"], "ws-main:notes/design.md");
        // Regression guard: the legacy `agent_id` field name must not
        // reappear in the wire JSON. A regression that re-introduces it
        // would silently break desktop display-name resolution.
        assert!(
            by_id["import"].get("agent_id").is_none(),
            "wire JSON must not contain legacy `agent_id` key: {by_id}"
        );

        // read by path —— 寻址一致
        let by_path = ok(
            &state,
            None,
            "doc_read",
            json!({ "ref": "研发/文档库/设计纪要.md" }),
        )
        .await;
        assert_eq!(by_path["doc_id"], doc_id);

        // list 含 dir + doc + query 过滤
        let root = ok(&state, None, "doc_list", json!({})).await;
        assert_eq!(root["total"], 1);
        assert_eq!(root["items"][0]["kind"], "dir");
        assert_eq!(root["items"][0]["name"], "研发");

        let lib = ok(&state, None, "doc_list", json!({ "path": "研发/文档库" })).await;
        assert_eq!(lib["total"], 1);
        assert_eq!(lib["items"][0]["kind"], "doc");
        assert_eq!(lib["items"][0]["doc_id"], doc_id);

        let filtered = ok(
            &state,
            None,
            "doc_list",
            json!({ "path": "研发/文档库", "query": "设计" }),
        )
        .await;
        assert_eq!(filtered["total"], 1);
        let none = ok(
            &state,
            None,
            "doc_list",
            json!({ "path": "研发/文档库", "query": "zzz" }),
        )
        .await;
        assert_eq!(none["total"], 0);
    }

    #[tokio::test]
    async fn add_duplicate_name_conflicts_and_missing_parent_fails() {
        let (_tmp, state) = setup().await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "组A" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "path": "组A", "title": "周报", "content": "a" }),
        )
        .await;

        // 同名 → NameConflict
        let e = err(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "path": "组A", "title": "周报", "content": "b" }),
        )
        .await;
        assert!(matches!(e, DocError::NameConflict(_)), "got {e:?}");

        // 父目录不存在 → DirNotFound
        let e = err(
            &state,
            Some(AGENT),
            "doc_mkdir",
            json!({ "path": "幽灵/子组" }),
        )
        .await;
        assert!(matches!(e, DocError::DirNotFound(_)), "got {e:?}");

        // 无 title 无 source_path → BadRequest；有 source_path → stem 当 title
        let e = err(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "content": "x" }),
        )
        .await;
        assert!(matches!(e, DocError::BadRequest(_)), "got {e:?}");
        let v = ok(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "content": "x", "source_path": "/home/a/report.md" }),
        )
        .await;
        assert_eq!(v["name"], "report");
    }

    #[tokio::test]
    async fn submit_update_and_check_request_flow() {
        let (_tmp, state) = setup().await;
        let v = ok(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "title": "会议纪要", "content": "# v1" }),
        )
        .await;
        let doc_id = v["doc_id"].as_str().unwrap().to_string();

        // 提交 base=1 → pending
        let v = ok(
            &state,
            Some(AGENT),
            "doc_submit_update",
            json!({ "ref": doc_id, "content": "# v2", "base_version": 1 }),
        )
        .await;
        assert_eq!(v["status"], "pending");
        let request_id = v["request_id"].as_str().unwrap().to_string();

        // check → pending；人类 approve 后 → approved + note
        let v = ok(&state, None, "doc_check_request", json!({ "request_id": request_id })).await;
        assert_eq!(v["status"], "pending");
        state
            .requests
            .approve(&request_id, "human:zhang", Some("ok"))
            .await
            .unwrap();
        let v = ok(&state, None, "doc_check_request", json!({ "request_id": request_id })).await;
        assert_eq!(v["status"], "approved");
        assert_eq!(v["review_note"], "ok");
        assert_eq!(v["reviewed_by"], "human:zhang");

        // stale base 提交 → VersionConflict
        let e = err(
            &state,
            Some(AGENT),
            "doc_submit_update",
            json!({ "ref": doc_id, "content": "x", "base_version": 1 }),
        )
        .await;
        assert!(matches!(e, DocError::VersionConflict { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn search_and_pull_tools() {
        let (_tmp, state) = setup().await;
        ok(
            &state,
            Some(AGENT),
            "doc_add",
            json!({ "title": "发布方案", "content": "回滚策略与灰度发布" }),
        )
        .await;

        let v = ok(&state, None, "doc_search", json!({ "keyword": "回滚" })).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "发布方案");

        // pull 返回内容 + base_version + 建议缓存路径（不落盘）
        let v = ok(&state, None, "doc_pull", json!({ "ref": "发布方案.md" })).await;
        assert_eq!(v["version"], 1);
        assert_eq!(v["content"], "回滚策略与灰度发布");
        assert!(v["cache_path"].as_str().unwrap().ends_with(".md"));
        assert!(!v["cache_path"].as_str().unwrap().contains(".."), "cache path must be safe");
    }

    #[tokio::test]
    async fn anonymous_writes_rejected_reads_allowed() {
        let (_tmp, state) = setup().await;
        // 匿名创建文档 → Forbidden
        let e = err(&state, None, "doc_add", json!({ "title": "x", "content": "y" })).await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");
        // 匿名 submit_update → Forbidden
        let e = err(
            &state,
            None,
            "doc_submit_update",
            json!({ "ref": "doc-ffffffffffff", "content": "x", "base_version": 1 }),
        )
        .await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");
        // 匿名只读仍可用（空库 list）
        let v = ok(&state, None, "doc_list", json!({})).await;
        assert_eq!(v["total"], 0);
    }

    #[tokio::test]
    async fn resolve_dir_rejects_absolute_and_traversal() {
        let (_tmp, state) = setup().await;
        // `..` 段在目录链解析中查无此名 → 失败（绝不向上逃逸）
        let e = err(&state, Some(AGENT), "doc_mkdir", json!({ "path": "../escape" })).await;
        assert!(
            matches!(e, DocError::DirNotFound(_) | DocError::BadRequest(_)),
            "got {e:?}"
        );
        // 混合穿越同样被拦
        let e = err(&state, Some(AGENT), "doc_mkdir", json!({ "path": "组A/../../x" })).await;
        assert!(matches!(e, DocError::DirNotFound(_)), "got {e:?}");
        // 前导 / 被安全地当作相对根路径处理（不逃逸到文件系统根）
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "/abs" })).await;
        assert_eq!(v["name"], "abs");
        let tree = state.dirs.list_tree("root").await.unwrap();
        assert!(tree.dirs.iter().any(|d| d.name == "abs"));
    }
}


