//! MCP 工具分发（设计 §6 全量 9 工具，文件系统语义）。
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
use crate::limit;
use crate::path::validate_request_id;
use crate::service::directory::{CreateDirectoryInput, DirectoryService};
use crate::service::document::{
    AppendDocumentInput, CreateDocumentInput, DocumentService, UpdateDocumentInput,
};
use crate::service::acl::{AclTarget, ActorCtx};
use crate::types::AclRole;
use crate::service::policy::{gate_owner, gate_write, OwnerGate, WriteGate};
use crate::service::request::{RequestService, SubmitRequestInput};
use crate::service::search::SearchService;
use crate::service::trash::TrashService;
use crate::state::DocState;
use crate::types::{
    DocMeta, ImportSource, RequestStatus, ROOT_DIR_ID, TRASH_ID_PREFIX,
};

// ── 身份与参数辅助 ──────────────────────────────────────────────────────

/// 要求调用方具备可信身份（匿名 → Forbidden）。所有写工具先过此关。
fn require_actor(actor: Option<&str>) -> Result<&str> {
    actor.ok_or_else(|| {
        DocError::Forbidden(actor.map(str::to_string))
    })
}

/// 结构性操作（`doc_move` / `doc_remove` / `doc_rmdir`）的治理闸门。
///
/// 所有权优先（[`gate_owner`](crate::service::policy::gate_owner)）；节点没有
/// 创建人记录时（creator 标记上线前的历史数据、导入的文档）不直接拒绝，而是
/// 交给节点 ACL 裁决——结构性操作没有审核兜底，按所有权一律拒绝会让这类文档
/// 在 Agent 面上永久不可达（既不能改名也不能删），那不是保护而是死锁。
/// ACL 默认公开，显式收紧时才拦人，admin 在 ACL 层内已豁免。
///
/// `target` 由调用方给出（文档传 [`AclTarget::Doc`]，目录传 [`AclTarget::Dir`]）：
/// 闸门规则只依赖创建人与目标坐标，与节点的 kind 无关，所以两者共用一份实现——
/// 这正是 REST 面（`DELETE /api/dirs/:id` → `AclTarget::Dir` + `Edit`）执行的规则。
async fn gate_structural(
    state: &DocState,
    actor: &str,
    creator: Option<&str>,
    target: AclTarget<'_>,
) -> Result<()> {
    match gate_owner(actor, false, creator)? {
        OwnerGate::Owner => Ok(()),
        OwnerGate::Unowned => {
            state
                .acl
                .check(
                    &ActorCtx {
                        id: Some(actor.to_string()),
                        is_admin: false,
                    },
                    target,
                    AclRole::Edit,
                )
                .await
        }
    }
}

/// `edit` 闸门，作用在**目标目录**上——所有“往别人的节点里放东西”的创建操作
/// 都先过这一关。
///
/// 与结构性闸门的分工：结构性操作（移动/删除）动的是节点本身，按**创建人**
/// 裁决（[`gate_structural`]）；创建动的是父目录的**内容**，按目录的
/// **节点 ACL** 裁决。两者都是 REST 面已有的规则，这里只是让 MCP 面追上：
/// `POST /api/dirs`（`api/dirs.rs`）与 `POST /api/docs`（`api/docs.rs`）都对
/// 目标目录做 `AclTarget::Dir` + `Edit`。
///
/// 新增文档改自 / 追加到别人已存在的文档，走的是 `gate_write`（转 `Review`
/// 审核队列）；**创建**没有可审核的对象，所以只剩这道节点闸门——缺了它，
/// agent 就能把内容写进用户显式收紧过的目录，而 REST 面会拒。
async fn gate_create(state: &DocState, actor: &str, parent_dir_id: &str) -> Result<()> {
    state
        .acl
        .check(
            &ActorCtx {
                id: Some(actor.to_string()),
                is_admin: false,
            },
            AclTarget::Dir(parent_dir_id),
            AclRole::Edit,
        )
        .await
}

/// 参数解析：缺省字段走 `default`；**未知字段直接报错**；类型错误给清晰信息。
///
/// 每个 Args 结构都带 `deny_unknown_fields`：模型拼错字段名（`titel`）时 serde
/// 的报错会列出可接受字段名（`unknown field `titel`, expected one of …`），
/// 模型据此自我纠正；静默丢弃则会让调用“成功”但字段没生效——那是比报错更难查的
/// 失败。取值/类型错误本来就报得清楚，缺的正是“名字”这一维。
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
///
/// `pub(crate)`: the REST validation endpoint (design 32 §3 / plan §7 G4,
/// `api/refs.rs`) answers "does this ref resolve" with this exact resolution
/// — one address grammar, one implementation, so the PM side can never be
/// handed a ref that a later `doc_read` would reject.
pub(crate) async fn resolve_doc_ref(
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

/// Internal `dir_id` → human-readable directory path (`""` for the root).
///
/// The service layer's `DocumentService::path_of` is keyed by `dir_id` (that
/// is the on-disk layout), so it cannot render names on its own — the MCP
/// layer owns `DirectoryService` and does the translation.
///
/// Cost is O(depth) per directory, not O(directories): `DirectoryService::
/// path_of` walks up the `parent` chain. The memo is per call, so a search
/// that renders 100 hits in 3 directories walks each chain once — and no
/// cross-request cache means no invalidation hook to forget on mkdir /
/// rename / delete.
struct PathCtx<'a> {
    dirs: &'a dyn DirectoryService,
    memo: std::collections::HashMap<String, String>,
}

impl<'a> PathCtx<'a> {
    fn new(dirs: &'a dyn DirectoryService) -> Self {
        let mut memo = std::collections::HashMap::new();
        memo.insert(ROOT_DIR_ID.to_string(), String::new());
        Self { dirs, memo }
    }

    async fn dir(&mut self, dir_id: &str) -> Result<&str> {
        if !self.memo.contains_key(dir_id) {
            let path = self.dirs.path_of(dir_id).await?;
            self.memo.insert(dir_id.to_string(), path);
        }
        Ok(self.memo.get(dir_id).map(String::as_str).unwrap_or_default())
    }

    /// Translate a service-rendered `{dir_id}/{name}.md` path into the path
    /// the agent sees (`研发/纪要.md`).
    async fn humanize(&mut self, raw: &str) -> Result<String> {
        let (dir_id, file) = raw.rsplit_once('/').unwrap_or((ROOT_DIR_ID, raw));
        let dir = self.dir(dir_id).await?.to_string();
        Ok(if dir.is_empty() {
            file.to_string()
        } else {
            format!("{dir}/{file}")
        })
    }

    /// Public, human-readable path of a document (`研发/纪要.md`).
    async fn doc(&mut self, docs: &dyn DocumentService, doc_id: &str) -> Result<String> {
        let raw = docs.path_of(doc_id).await?;
        self.humanize(&raw).await
    }
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
#[serde(deny_unknown_fields)]
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
    let mut paths = PathCtx::new(state.dirs.as_ref());
    let path = paths.humanize(&read.path).await?;
    let mut v = doc_to_value(read.meta, path)?;
    v["content"] = Value::String(read.content);
    Ok(v)
}

/// `doc_stat` — 仅元数据（`stat(2)`）：version / size / creator / 时间。
///
/// 取代旧的 `doc_pull`：只为拿一个 `version` 整数而把全文传回调用方是
/// O(n) 换 O(1)。需要正文时用 `doc_read`。
async fn doc_stat(state: &DocState, args: Value) -> Result<Value> {
    let a: DocRefArgs = parse_args("doc_stat", args)?;
    let (_dir, doc_id) =
        resolve_doc_ref(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;
    let st = state.docs.stat(&doc_id).await?;
    let mut paths = PathCtx::new(state.dirs.as_ref());
    let path = paths.humanize(&st.path).await?;
    let mut v = doc_to_value(st.meta, path)?;
    v["size_bytes"] = Value::from(st.size_bytes);
    Ok(v)
}

/// 共享的文档响应序列化（doc_read / doc_stat / doc_write 复用）。
fn doc_to_value(meta: DocMeta, path: String) -> Result<Value> {
    let mut v = json!({
        "doc_id": meta.doc_id,
        "name": meta.name,
        "path": path,
        "version": meta.version,
        "creator": meta.creator,
        "created_at": meta.created_at,
        "updated_at": meta.updated_at,
    });
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
#[serde(deny_unknown_fields)]
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
    // The search service reports `{dir_id}/{name}.md` (its store view); every
    // other tool answers in the agent's addressing space, so a hit path must
    // be readable back into `doc_read(ref=…)` — otherwise the natural
    // `search → read` chain fails with `DirNotFound`.
    //
    // A hit whose directory is gone (deleted after indexing) keeps the raw
    // value: one unaddressable hit must not fail the search.
    let mut paths = PathCtx::new(state.dirs.as_ref());
    let mut items: Vec<Value> = Vec::with_capacity(hits.len());
    for h in hits {
        let path = paths.humanize(&h.path).await.unwrap_or_else(|_| h.path.clone());
        items.push(json!({
            "doc_id": h.doc_id,
            "name": h.name,
            "path": path,
            "snippet": h.snippet,
            "score": h.score,
        }));
    }
    Ok(json!({ "keyword": a.keyword, "hits": items }))
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
    gate_create(state, actor, &parent_dir_id).await?;
    let meta = state
        .dirs
        .create(CreateDirectoryInput {
            parent_dir_id,
            name: name.to_string(),
            // The creator stamp is not provenance-only any more: it is what
            // the structural gate (`gate_structural` → `gate_owner`) reads
            // when someone later tries to rename or prune this directory, so
            // the agent that creates a bucket becomes the one that governs
            // it.
            creator: Some(actor.to_string()),
        })
        .await?;
    Ok(json!({
        "dir_id": meta.dir_id,
        "name": meta.name,
        "path": path,
    }))
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum WriteMode {
    /// `>` — create if absent, replace if present.
    #[default]
    Overwrite,
    /// `O_EXCL` — fail when the name is already taken.
    Create,
    /// `>>` — append to an existing document.
    Append,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[serde(rename = "ref")]
    reference: String,
    content: String,
    #[serde(default)]
    mode: WriteMode,
    #[serde(default)]
    if_version: Option<u64>,
    #[serde(default)]
    source_workspace: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
}

/// Resolve a `ref` into its location **without reading the body**.
///
/// Returns `(dir_id, Some(meta))` when the document exists and
/// `(dir_id, None)` when the ref is a well-formed *path* whose file does not
/// exist yet (the create case). A `doc_id` ref that does not exist is an
/// error — an id never names a non-existent future document.
async fn resolve_target(
    dirs: &dyn DirectoryService,
    docs: &dyn DocumentService,
    reference: &str,
) -> Result<(String, Option<DocMeta>)> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(DocError::BadRequest("ref must not be empty".into()));
    }
    if reference.starts_with("doc-") {
        let st = docs.stat(reference).await?;
        return Ok((String::new(), Some(st.meta)));
    }
    let (dir_path, file) = match reference.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", reference),
    };
    let stem = file.strip_suffix(".md").ok_or_else(|| {
        DocError::BadRequest(format!("document path must end in .md, got: {reference}"))
    })?;
    if stem.is_empty() || stem.contains('/') {
        return Err(DocError::BadRequest(format!(
            "invalid document path: {reference}"
        )));
    }
    let dir_id = resolve_dir(dirs, dir_path).await?;
    let metas = docs.list(&dir_id).await?;
    Ok((dir_id, metas.into_iter().find(|m| m.name == stem)))
}

/// `doc_write` — the single write primitive (create / overwrite / append).
///
/// Governance is an orthogonal dimension: `gate_write` decides whether this
/// write takes effect now; when it lands on review the same call instead
/// records a `pending` update request and returns `status: "pending_review"`.
/// The verb never changes with the gate — the filesystem analogue is one
/// `write(2)` returning EACCES or blocking, not a different system call.
async fn doc_write(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: WriteArgs = parse_args("doc_write", args)?;
    let (dir_id, existing) =
        resolve_target(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;

    match (a.mode, existing) {
        // ── create over an existing name ────────────────────────────
        (WriteMode::Create, Some(meta)) => Err(DocError::NameConflict(meta.name)),

        // ── create / overwrite a missing document ───────────────────
        (WriteMode::Create | WriteMode::Overwrite, None) => {
            if dir_id.is_empty() {
                return Err(DocError::BadRequest(
                    "creating needs a path (it supplies the directory and the file name)".into(),
                ));
            }
            // Creating is the one write with no review fallback: there is no
            // existing document to propose a change to, so the target
            // directory's own ACL is the only gate — same rule the REST face
            // applies (`POST /api/docs` → `AclTarget::Dir` + `Edit`).
            gate_create(state, actor, &dir_id).await?;
            let title = stem_of_ref(&a.reference)?;
            limit::ensure_document_size(a.content.len(), state.config.max_doc_size_bytes)?;
            let import = import_of(actor, &a.source_workspace, &a.source_path);
            let meta = state
                .docs
                .create(CreateDocumentInput {
                    parent_dir_id: dir_id,
                    title,
                    content: a.content,
                    import,
                    creator: Some(actor.to_string()),
                })
                .await?;
            let path = PathCtx::new(state.dirs.as_ref())
                .doc(state.docs.as_ref(), &meta.doc_id)
                .await?;
            Ok(json!({
                "status": "applied",
                "doc_id": meta.doc_id,
                "name": meta.name,
                "path": path,
                "version": meta.version,
            }))
        }

        // ── append to a missing document ────────────────────────────
        (WriteMode::Append, None) => Err(DocError::DocNotFound(a.reference.clone())),

        // ── overwrite / append an existing document ─────────────────
        (mode, Some(meta)) => {
            let base_version = a.if_version.unwrap_or(meta.version);
            if base_version != meta.version {
                return Err(DocError::VersionConflict {
                    base_version,
                    current_version: meta.version,
                });
            }
            // Ceiling on the *resulting* document: `append` must not grow a
            // document past the limit one chunk at a time.
            let resulting = if mode == WriteMode::Append {
                let current = state.docs.stat(&meta.doc_id).await?;
                current.size_bytes as usize + a.content.len()
            } else {
                a.content.len()
            };
            limit::ensure_document_size(resulting, state.config.max_doc_size_bytes)?;
            match gate_write(actor, false, meta.creator.as_deref()) {
                WriteGate::Apply => {
                    let updated = if mode == WriteMode::Append {
                        state
                            .docs
                            .append(
                                &meta.doc_id,
                                AppendDocumentInput {
                                    base_version: a.if_version,
                                    content: a.content,
                                },
                            )
                            .await?
                    } else {
                        state
                            .docs
                            .update(
                                &meta.doc_id,
                                UpdateDocumentInput {
                                    base_version,
                                    title: None,
                                    content: a.content,
                                },
                            )
                            .await?
                    };
                    let path = PathCtx::new(state.dirs.as_ref())
                        .doc(state.docs.as_ref(), &updated.doc_id)
                        .await?;
                    Ok(json!({
                        "status": "applied",
                        "doc_id": updated.doc_id,
                        "name": updated.name,
                        "path": path,
                        "version": updated.version,
                    }))
                }
                WriteGate::Review => {
                    // The review flow models a *content proposal*, so an
                    // append proposal carries the merged body while an
                    // overwrite proposal carries the replacement body as-is.
                    let proposal = if mode == WriteMode::Append {
                        let current = state.docs.read(&meta.doc_id).await?;
                        format!("{}{}", current.content, a.content)
                    } else {
                        a.content
                    };
                    let req = state
                        .requests
                        .submit(SubmitRequestInput {
                            doc_id: meta.doc_id.clone(),
                            base_version,
                            content: proposal,
                            submitted_by: actor.to_string(),
                        })
                        .await?;
                    Ok(json!({
                        "status": "pending_review",
                        "request_id": req.request_id,
                        "doc_id": req.doc_id,
                        "path": req.path,
                        "base_version": req.base_version,
                    }))
                }
            }
        }
    }
}

/// File-name stem of a `ref` path — the title for a create.
fn stem_of_ref(reference: &str) -> Result<String> {
    let file = reference
        .trim()
        .rsplit('/')
        .next()
        .ok_or_else(|| DocError::BadRequest(format!("invalid ref: {reference}")))?;
    let stem = file.strip_suffix(".md").ok_or_else(|| {
        DocError::BadRequest(format!("document path must end in .md, got: {reference}"))
    })?;
    Ok(stem.to_string())
}

/// Build the import provenance (create only; `None` when neither source is
/// supplied).
fn import_of(
    actor: &str,
    source_workspace: &Option<String>,
    source_path: &Option<String>,
) -> Option<ImportSource> {
    let workspace_path = match (source_workspace, source_path) {
        (None, None) => return None,
        (Some(w), Some(p)) => format!("{w}:{p}"),
        (Some(w), None) => w.clone(),
        (None, Some(p)) => p.clone(),
    };
    Some(ImportSource {
        // ADR-073: `actor` is the runtime instance identity (UUID) — the
        // value substituted into the Gateway's `{instance_id}` MCP template.
        instance_id: actor.to_string(),
        workspace_path,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveArgs {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

/// `doc_move` — `mv`: rename and/or move across directories; also the atomic
/// publish primitive (write to a temp name, then move onto the final name).
///
/// Gated like every structural op ([`gate_structural`]): the source must be
/// the caller's, and with `overwrite=true` so must the destination — replacing
/// someone else's document is destructive and the review flow only models
/// content proposals, so ownership (not review) is the gate.
///
/// The replace itself happens **inside** `DocumentService::move_doc`, which
/// sends the displaced body to the recycle bin and rolls every step back on
/// failure. The MCP layer must not delete the destination up front: that made
/// a failed move leave the destination deleted and the source untouched.
async fn doc_move(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: MoveArgs = parse_args("doc_move", args)?;
    let (_src_dir, src) =
        resolve_target(state.dirs.as_ref(), state.docs.as_ref(), &a.from).await?;
    let src = src.ok_or_else(|| DocError::DocNotFound(a.from.clone()))?;
    gate_structural(state, actor, src.creator.as_deref(), AclTarget::Doc(&src.doc_id)).await?;

    let to = a.to.trim();
    let (to_dir_path, _) = to.rsplit_once('/').unwrap_or(("", to));
    let dst_name = stem_of_ref(to)?;
    let dst_dir = resolve_dir(state.dirs.as_ref(), to_dir_path).await?;
    let dst_existing = state
        .docs
        .list(&dst_dir)
        .await?
        .into_iter()
        .find(|m| m.name == dst_name && m.doc_id != src.doc_id);

    match &dst_existing {
        Some(dst) if !a.overwrite => return Err(DocError::NameConflict(dst_name)),
        Some(dst) => {
            gate_structural(state, actor, dst.creator.as_deref(), AclTarget::Doc(&dst.doc_id)).await?
        }
        None => {}
    }

    let meta = state
        .docs
        .move_doc(&src.doc_id, &dst_dir, Some(&dst_name), a.overwrite)
        .await?;
    let path = PathCtx::new(state.dirs.as_ref())
        .doc(state.docs.as_ref(), &meta.doc_id)
        .await?;
    Ok(json!({
        "doc_id": meta.doc_id,
        "name": meta.name,
        "path": path,
        "version": meta.version,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    #[serde(rename = "ref")]
    reference: String,
}

/// `doc_remove` — `unlink`: soft-delete into the recycle bin (recoverable).
/// Gated like every structural op ([`gate_structural`]).
async fn doc_remove(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: RemoveArgs = parse_args("doc_remove", args)?;
    let (_dir, meta) =
        resolve_target(state.dirs.as_ref(), state.docs.as_ref(), &a.reference).await?;
    let meta = meta.ok_or_else(|| DocError::DocNotFound(a.reference.clone()))?;
    gate_structural(state, actor, meta.creator.as_deref(), AclTarget::Doc(&meta.doc_id)).await?;
    let path = PathCtx::new(state.dirs.as_ref())
        .doc(state.docs.as_ref(), &meta.doc_id)
        .await?;
    state.docs.delete(&meta.doc_id).await?;
    Ok(json!({ "removed": true, "doc_id": meta.doc_id, "path": path }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
        "doc_stat" => doc_stat(state, args).await,
        "doc_search" => doc_search(state, args).await,
        "doc_check_request" => doc_check_request(state, args).await,

        // ── 写（需身份）────────────────────────────────────────────
        "doc_mkdir" => {
            let actor = require_actor(actor)?;
            doc_mkdir(state, actor, args).await
        }
        "doc_write" => {
            let actor = require_actor(actor)?;
            doc_write(state, actor, args).await
        }
        "doc_move" => {
            let actor = require_actor(actor)?;
            doc_move(state, actor, args).await
        }
        "doc_remove" => {
            let actor = require_actor(actor)?;
            doc_remove(state, actor, args).await
        }
        "doc_rmdir" => {
            let a = require_actor(actor)?;
            doc_rmdir(state, a, args).await
        }
        "doc_list_requests" => doc_list_requests(state, args).await,
        "doc_list_trash" => doc_list_trash(state, args).await,
        "doc_restore" => {
            let a = require_actor(actor)?;
            doc_restore(state, a, args).await
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
    async fn mkdir_nested_and_write_read_roundtrip() {
        let (_tmp, state) = setup().await;
        // mkdir 逐级
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发" })).await;
        assert_eq!(v["name"], "研发");
        assert!(v["dir_id"].as_str().unwrap().starts_with("dir-"));
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发/文档库" })).await;
        assert_eq!(v["name"], "文档库");

        // doc_write(mode=create) into a sub-directory
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({
                "ref": "研发/文档库/设计纪要.md",
                "mode": "create",
                "content": "# v1 设计纪要",
                "source_workspace": "ws-main",
                "source_path": "notes/design.md",
            }),
        )
        .await;
        assert_eq!(v["status"], "applied");
        assert_eq!(v["version"], 1);
        assert_eq!(v["name"], "设计纪要");
        let doc_id = v["doc_id"].as_str().unwrap().to_string();

        // read by doc_id
        let by_id = ok(&state, None, "doc_read", json!({ "ref": doc_id })).await;
        assert_eq!(by_id["content"], "# v1 设计纪要");
        // ADR-073: the wire field is `instance_id` (UUID), populated from
        // the `X-MCP-Actor` header — here `AGENT` itself acts as the token.
        assert_eq!(by_id["import"]["instance_id"], AGENT);
        assert_eq!(by_id["import"]["workspace_path"], "ws-main:notes/design.md");
        // Regression guard: the legacy `agent_id` field name must not
        // reappear in the wire JSON (would break display-name resolution).
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
    async fn create_conflicts_missing_parent_and_bad_names() {
        let (_tmp, state) = setup().await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "组A" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "组A/周报.md", "mode": "create", "content": "a" }),
        )
        .await;

        // 同名 → NameConflict
        let e = err(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "组A/周报.md", "mode": "create", "content": "b" }),
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

        // 非 .md 路径 → BadRequest
        let e = err(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "组A/周报", "mode": "create", "content": "x" }),
        )
        .await;
        assert!(matches!(e, DocError::BadRequest(_)), "got {e:?}");

        // append 到不存在的文档 → DocNotFound（append 不隐式创建）
        let e = err(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "组A/幽灵.md", "mode": "append", "content": "x" }),
        )
        .await;
        assert!(matches!(e, DocError::DocNotFound(_)), "got {e:?}");

        // overwrite 一个不存在的名字 = create（`>` 重定向语义）
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "组A/新文件.md", "content": "hi" }),
        )
        .await;
        assert_eq!(v["status"], "applied");
        assert_eq!(v["version"], 1);
    }

    /// The core of the redesign: one verb, two outcomes. Ownership decides
    /// whether the write applies or turns into a review request.
    #[tokio::test]
    async fn write_applies_for_owner_and_reviews_for_others() {
        let (_tmp, state) = setup().await;
        const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";

        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "会议纪要.md", "mode": "create", "content": "# v1" }),
        )
        .await;
        let doc_id = v["doc_id"].as_str().unwrap().to_string();

        // 非所有者 overwrite → pending_review（不是换动词，是换结果）
        let v = ok(
            &state,
            Some(OTHER),
            "doc_write",
            json!({ "ref": doc_id, "content": "# v2", "if_version": 1 }),
        )
        .await;
        assert_eq!(v["status"], "pending_review");
        let request_id = v["request_id"].as_str().unwrap().to_string();

        // 文档在审批前未被修改
        let cur = ok(&state, None, "doc_read", json!({ "ref": doc_id })).await;
        assert_eq!(cur["content"], "# v1");
        assert_eq!(cur["version"], 1);

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

        // 所有者自己写入 → 直接生效
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": doc_id, "content": "# v3" }),
        )
        .await;
        assert_eq!(v["status"], "applied");
        assert_eq!(v["version"], 3);

        // stale if_version → VersionConflict（在 gate 之前就被拦住）
        let e = err(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": doc_id, "content": "x", "if_version": 1 }),
        )
        .await;
        assert!(matches!(e, DocError::VersionConflict { .. }), "got {e:?}");
    }

    /// Chunked write + atomic publish: the temp+rename pattern the tool docs
    /// prescribe for documents larger than one call.
    #[tokio::test]
    async fn append_chunks_then_move_publishes_atomically() {
        let (_tmp, state) = setup().await;
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "报告.tmp.md", "mode": "create", "content": "AAA" }),
        )
        .await;
        let tmp_id = v["doc_id"].as_str().unwrap().to_string();

        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": tmp_id, "mode": "append", "content": "BBB" }),
        )
        .await;
        assert_eq!(v["version"], 2);

        // 半成品只在临时名下可见，最终名还不存在
        let v = ok(&state, None, "doc_read", json!({ "ref": "报告.tmp.md" })).await;
        assert_eq!(v["content"], "AAABBB");
        let e = err(&state, None, "doc_read", json!({ "ref": "报告.md" })).await;
        assert!(matches!(e, DocError::DocNotFound(_)), "got {e:?}");

        // 原子发布
        let v = ok(
            &state,
            Some(AGENT),
            "doc_move",
            json!({ "from": "报告.tmp.md", "to": "报告.md" }),
        )
        .await;
        assert_eq!(v["path"], "报告.md");
        let v = ok(&state, None, "doc_read", json!({ "ref": "报告.md" })).await;
        assert_eq!(v["content"], "AAABBB");
    }

    #[tokio::test]
    async fn stat_returns_metadata_without_body() {
        let (_tmp, state) = setup().await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "发布方案.md", "mode": "create", "content": "回滚策略与灰度发布" }),
        )
        .await;

        let v = ok(&state, None, "doc_search", json!({ "keyword": "回滚" })).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "发布方案");

        // stat：版本 + 字节数 + creator，且不带 content
        let v = ok(&state, None, "doc_stat", json!({ "ref": "发布方案.md" })).await;
        assert_eq!(v["version"], 1);
        assert_eq!(v["creator"], AGENT);
        assert_eq!(v["size_bytes"], "回滚策略与灰度发布".len() as u64);
        assert!(v.get("content").is_none(), "stat must not return the body");
    }

    #[tokio::test]
    async fn search_hit_paths_are_addressable_by_the_other_tools() {
        // The addressing contract is one space for the whole tool surface: a
        // path a tool *returns* must be a path a tool *accepts*. The search
        // service reports its store view (`{dir_id}/{name}.md`), so
        // `doc_search` has to translate it — otherwise the natural
        // `search → doc_read(ref=path)` chain answers `DirNotFound`.
        let (_tmp, state) = setup().await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发" })).await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "研发/纪要" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "研发/纪要/周会.md", "mode": "create", "content": "灰度发布与回滚" }),
        )
        .await;

        let v = ok(&state, None, "doc_search", json!({ "keyword": "灰度" })).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        let path = hits[0]["path"].as_str().unwrap().to_string();
        assert_eq!(path, "研发/纪要/周会.md");
        assert!(
            !path.contains("dir-"),
            "a returned path must never carry an internal dir id: {path}"
        );

        // The value round-trips through every path-accepting write/read tool.
        let v = ok(&state, None, "doc_read", json!({ "ref": path })).await;
        assert_eq!(v["content"], "灰度发布与回滚");
        let v = ok(&state, None, "doc_stat", json!({ "ref": path })).await;
        assert_eq!(v["version"], 1);
    }

    #[tokio::test]
    async fn move_and_remove_require_ownership() {
        let (_tmp, state) = setup().await;
        const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "归档" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "旧方案.md", "mode": "create", "content": "x" }),
        )
        .await;

        // 非所有者 move / remove → Forbidden（结构性操作不走审批流）
        let e = err(
            &state,
            Some(OTHER),
            "doc_move",
            json!({ "from": "旧方案.md", "to": "归档/新方案.md" }),
        )
        .await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");
        let e = err(
            &state,
            Some(OTHER),
            "doc_remove",
            json!({ "ref": "旧方案.md" }),
        )
        .await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");

        // 所有者 rename + 跨目录移动
        let v = ok(
            &state,
            Some(AGENT),
            "doc_move",
            json!({ "from": "旧方案.md", "to": "归档/新方案.md" }),
        )
        .await;
        assert_eq!(v["path"], "归档/新方案.md");
        let v = ok(&state, None, "doc_read", json!({ "ref": "归档/新方案.md" })).await;
        assert_eq!(v["content"], "x");

        // 目标已存在且 overwrite=false → NameConflict
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "归档/新方案.md", "mode": "overwrite", "content": "y" }),
        )
        .await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "另一份.md", "mode": "create", "content": "z" }),
        )
        .await;
        let e = err(
            &state,
            Some(AGENT),
            "doc_move",
            json!({ "from": "另一份.md", "to": "归档/新方案.md" }),
        )
        .await;
        assert!(matches!(e, DocError::NameConflict(_)), "got {e:?}");

        // 所有者删除 → 进回收站
        let v = ok(&state, Some(AGENT), "doc_remove", json!({ "ref": "另一份.md" })).await;
        assert_eq!(v["removed"], true);
        let e = err(&state, None, "doc_read", json!({ "ref": "另一份.md" })).await;
        assert!(matches!(e, DocError::DocNotFound(_)), "got {e:?}");
    }

    /// A document with no recorded creator (pre-creator-stamp / imported) has
    /// no owner to protect and no review channel for structural ops, so the
    /// ownership gate must not make it permanently unreachable. It falls
    /// through to the node ACL, which is public unless someone tightened it.
    #[tokio::test]
    async fn unowned_documents_stay_reachable_for_structural_ops() {
        let (_tmp, state) = setup().await;
        const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";
        // Created through the service with `creator: None` — the shape of a
        // document that predates creator stamping.
        state
            .docs
            .create(crate::service::document::CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "legacy".into(),
                content: "old".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();

        // Rename is allowed for any actor (no owner exists)…
        let v = ok(
            &state,
            Some(OTHER),
            "doc_move",
            json!({ "from": "legacy.md", "to": "legacy-v2.md" }),
        )
        .await;
        assert_eq!(v["path"], "legacy-v2.md");
        // …and so is removal (soft: it lands in the recycle bin).
        let v = ok(
            &state,
            Some(OTHER),
            "doc_remove",
            json!({ "ref": "legacy-v2.md" }),
        )
        .await;
        assert_eq!(v["removed"], true);
    }

    /// The same fall-through must not open an explicitly ACL-locked node.
    #[tokio::test]
    async fn unowned_document_with_a_restrictive_acl_still_denies() {        let (_tmp, state) = setup().await;
        const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";
        let meta = state
            .docs
            .create(crate::service::document::CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "locked".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let store = crate::store::library::LibraryStore::new(
            state.config.data_dir.clone(),
        );
        let acl = crate::service::acl::AclService::new(store);
        acl.set(
            &crate::service::acl::ActorCtx {
                id: Some("human".into()),
                is_admin: false,
            },
            crate::service::acl::AclTarget::Doc(&meta.doc_id),
            Some(vec![crate::types::Ace {
                principal: "inst-0000-0000-0000-0000-000000000009".into(),
                role: crate::types::AclRole::View,
            }]),
        )
        .await
        .unwrap();

        let e = err(
            &state,
            Some(OTHER),
            "doc_remove",
            json!({ "ref": "locked.md" }),
        )
        .await;
        assert!(matches!(e, DocError::AclDenied { .. }), "got {e:?}");
    }

    /// Replacing a document is a service-level operation now: the displaced
    /// body is parked in the recycle bin instead of being unlinked, and the
    /// MCP layer no longer deletes the destination before moving.
    #[tokio::test]
    async fn move_over_a_document_parks_the_replaced_body_in_the_trash() {
        let (_tmp, state) = setup().await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "归档" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "归档/方案.md", "mode": "create", "content": "old" }),
        )
        .await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "方案.md", "mode": "create", "content": "new" }),
        )
        .await;

        let v = ok(
            &state,
            Some(AGENT),
            "doc_move",
            json!({ "from": "方案.md", "to": "归档/方案.md", "overwrite": true }),
        )
        .await;
        assert_eq!(v["path"], "归档/方案.md");
        let read = ok(&state, None, "doc_read", json!({ "ref": "归档/方案.md" })).await;
        assert_eq!(read["content"], "new");

        // The displaced document is recoverable, not gone.
        let trash = crate::service::trash::TrashService::list(state.trash.as_ref())
            .await
            .unwrap();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].original_name, "方案");
    }

    /// Same-directory rename onto an existing name: the MCP layer used to
    /// delete the destination first; the service must replace it atomically.
    #[tokio::test]
    async fn same_dir_rename_with_overwrite_replaces_the_destination() {
        let (_tmp, state) = setup().await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "草稿.md", "mode": "create", "content": "new" }),
        )
        .await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "定稿.md", "mode": "create", "content": "old" }),
        )
        .await;

        let v = ok(
            &state,
            Some(AGENT),
            "doc_move",
            json!({ "from": "草稿.md", "to": "定稿.md", "overwrite": true }),
        )
        .await;
        assert_eq!(v["path"], "定稿.md");
        let read = ok(&state, None, "doc_read", json!({ "ref": "定稿.md" })).await;
        assert_eq!(read["content"], "new");
        let list = ok(&state, None, "doc_list", json!({})).await;
        assert_eq!(list["total"], 1, "no duplicate entry left behind: {list}");
        assert_eq!(
            crate::service::trash::TrashService::list(state.trash.as_ref())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn anonymous_writes_rejected_reads_allowed() {
        let (_tmp, state) = setup().await;
        // 匿名写入 → Forbidden
        let e = err(
            &state,
            None,
            "doc_write",
            json!({ "ref": "x.md", "mode": "create", "content": "y" }),
        )
        .await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");
        let e = err(
            &state,
            None,
            "doc_move",
            json!({ "from": "a.md", "to": "b.md" }),
        )
        .await;
        assert!(matches!(e, DocError::Forbidden(_)), "got {e:?}");
        let e = err(&state, None, "doc_remove", json!({ "ref": "a.md" })).await;
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

    /// **Creation is gated on the target directory's ACL, like the REST face.**
    ///
    /// `doc_mkdir` and `doc_write(mode=create)` were the last two ungated entry
    /// points: an agent could seed a directory a user had explicitly
    /// restricted, while `POST /api/dirs` / `POST /api/docs` refused the same
    /// call. Both now route through [`gate_create`].
    #[tokio::test]
    async fn creating_inside_a_restricted_directory_is_denied() {
        let (_tmp, state) = setup().await;
        const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";

        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "私有" })).await;
        ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "私有/已存在.md", "mode": "create", "content": "x" }),
        )
        .await;
        let dir = state
            .dirs
            .list_tree(ROOT_DIR_ID)
            .await
            .unwrap()
            .dirs
            .into_iter()
            .find(|d| d.name == "私有")
            .expect("dir exists");

        // Lock it to one principal; the creator keeps access by construction.
        state
            .acl
            .set(
                &ActorCtx {
                    id: Some("human".into()),
                    is_admin: false,
                },
                AclTarget::Dir(&dir.dir_id),
                Some(vec![crate::types::Ace {
                    principal: AGENT.into(),
                    role: AclRole::Edit,
                }]),
            )
            .await
            .unwrap();

        // 1. outside agent cannot create a sub-directory in it …
        let e = err(&state, Some(OTHER), "doc_mkdir", json!({ "path": "私有/子目录" })).await;
        assert!(matches!(e, DocError::AclDenied { .. }), "got {e:?}");

        // 2. … and cannot seed a document into it.
        let e = err(
            &state,
            Some(OTHER),
            "doc_write",
            json!({ "ref": "私有/注入.md", "mode": "create", "content": "x" }),
        )
        .await;
        assert!(matches!(e, DocError::AclDenied { .. }), "got {e:?}");

        // 3. Nothing was created by the two refused calls.
        let listing = ok(&state, None, "doc_list", json!({ "path": "私有" })).await;
        assert_eq!(listing["total"], 1, "{listing}");

        // 4. The principal that owns the directory still works.
        let v = ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": "私有/子目录" })).await;
        assert_eq!(v["name"], "子目录");
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": "私有/提案.md", "mode": "create", "content": "mine" }),
        )
        .await;
        assert_eq!(v["status"], "applied");
    }

    /// **Design 32 §5.5, verified against the real resolver.**
    ///
    /// The PM surface publishes `doc_path` (`项目管理/{project_id}`); this is
    /// the other half of the contract — that the string is addressable, and
    /// that it is **not** created implicitly. `doc_write` into a missing
    /// directory chain fails with `DirNotFound`, so the convention's carrier
    /// skill must create the chain first. A typo therefore surfaces as an
    /// error instead of silently building a parallel tree.
    #[tokio::test]
    async fn the_project_document_directory_round_trips() {
        let (_tmp, state) = setup().await;
        let doc_path = acowork_core::doc_layout::project_doc_dir("p-1a2b3c");

        // Writing before the chain exists is refused, and the error names the
        // segment that is missing (the actionable half).
        let e = err(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": format!("{doc_path}/PRD.md"), "mode": "create", "content": "# PRD" }),
        )
        .await;
        match e {
            DocError::DirNotFound(seg) => assert_eq!(seg, "directory path segment: 项目管理"),
            other => panic!("expected DirNotFound, got {other:?}"),
        }

        // The convention's two-step setup: root, then the project directory.
        ok(
            &state,
            Some(AGENT),
            "doc_mkdir",
            json!({ "path": acowork_core::doc_layout::PROJECT_DOC_ROOT }),
        )
        .await;
        ok(&state, Some(AGENT), "doc_mkdir", json!({ "path": doc_path })).await;

        // Now the published path addresses a document.
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": format!("{doc_path}/PRD.md"), "mode": "create", "content": "# PRD" }),
        )
        .await;
        assert_eq!(v["path"], format!("{doc_path}/PRD.md"));
        // The human face of the directory: an H1 in README.md, not a nice
        // directory name (the directory is a machine handle).
        let v = ok(
            &state,
            Some(AGENT),
            "doc_write",
            json!({ "ref": format!("{doc_path}/README.md"), "mode": "create", "content": "# 项目" }),
        )
        .await;
        assert_eq!(v["status"], "applied");
        let read = ok(
            &state,
            None,
            "doc_read",
            json!({ "ref": format!("{doc_path}/README.md") }),
        )
        .await;
        assert_eq!(read["content"], "# 项目");
    }
}

// ── Review-request and trash surfaces (design 32 §3, P2) ────────────────
//
// Three questions an agent cannot answer today:
//
// * "what did I submit that is still waiting?" — `doc_check_request` needs a
//   `request_id`, and that id exists only in the write response. Compaction
//   drops it, and the pending change is then unreachable *and* unrevertible.
// * "what is in the trash?" — `doc_remove` promises a recoverable delete but
//   the recovery surface was REST-only, so the promise was not keepable from
//   an agent session.
// * "how do I restructure the library?" — `doc_mkdir` existed with no
//   counterpart, so directory trees could be grown but never pruned.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequestsArgs {
    /// `pending` (default) | `approved` | `rejected` | `expired`.
    #[serde(default)]
    status: Option<String>,
    /// Restrict to one document (`doc-…` id or a library path).
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `doc_list_requests` — the review queue, so a lost `request_id` is not a
/// lost change.
async fn doc_list_requests(state: &DocState, args: Value) -> Result<Value> {
    // No `unwrap_or(default)` here: that would answer a malformed call with a
    // plausible-looking queue, and the caller would never learn it was wrong.
    let a: ListRequestsArgs = parse_args("doc_list_requests", args)?;
    let status = match a.status.as_deref().map(str::trim) {
        None | Some("") | Some("pending") => Some(RequestStatus::Pending),
        Some("approved") => Some(RequestStatus::Approved),
        Some("rejected") => Some(RequestStatus::Rejected),
        Some("expired") => Some(RequestStatus::Expired),
        Some("all") => None,
        Some(other) => {
            return Err(DocError::BadRequest(format!(
                "unknown status `{other}`; valid: pending | approved | rejected | expired | all"
            )));
        }
    };

    let requests = match &a.reference {
        Some(reference) => {
            let (_dir, meta) =
                resolve_target(state.dirs.as_ref(), state.docs.as_ref(), reference).await?;
            let meta = meta.ok_or_else(|| DocError::DocNotFound(reference.clone()))?;
            state.requests.list_for_doc(&meta.doc_id).await?
        }
        None => state.requests.list(status).await?,
    };

    let limit = a.limit.unwrap_or(50).clamp(1, 200);
    let total = requests.len();
    let page: Vec<Value> = requests
        .into_iter()
        .take(limit)
        .map(|r| {
            let mut v = json!({
                "request_id": r.request_id,
                "doc_id": r.doc_id,
                "status": request_status_str(r.status),
                "submitted_by": r.submitted_by,
                "submitted_at": r.created_at,
            });
            if let Some(note) = &r.review_note {
                v["review_note"] = Value::String(note.clone());
            }
            v
        })
        .collect();

    Ok(json!({
        "total": total,
        "returned": page.len(),
        "has_more": total > page.len(),
        "requests": page,
    }))
}

fn request_status_str(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Pending => "pending",
        RequestStatus::Approved => "approved",
        RequestStatus::Rejected => "rejected",
        RequestStatus::Expired => "expired",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListTrashArgs {
    #[serde(default)]
    limit: Option<usize>,
}

/// `doc_list_trash` — what `doc_remove` took away, with the id to bring it
/// back.
async fn doc_list_trash(state: &DocState, args: Value) -> Result<Value> {
    let a: ListTrashArgs = parse_args("doc_list_trash", args)?;
    let entries = state.trash.list().await?;
    let limit = a.limit.unwrap_or(50).clamp(1, 200);
    let total = entries.len();
    let page: Vec<Value> = entries
        .into_iter()
        .take(limit)
        .map(|e| {
            json!({
                "trash_id": e.trash_id,
                "doc_id": e.doc_id,
                "original_name": e.original_name,
                "original_dir_id": e.original_dir_id,
                "deleted_at": e.deleted_at,
                "file_size_bytes": e.file_size_bytes,
                "creator": e.creator,
            })
        })
        .collect();
    Ok(json!({
        "total": total,
        "returned": page.len(),
        "has_more": total > page.len(),
        "entries": page,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreArgs {
    trash_id: String,
}

/// `doc_restore` — undo a `doc_remove`.
///
/// The gate mirrors the REST endpoint: restoring writes a document back into
/// its directory, so it needs the same edit right the delete needed. There is
/// deliberately **no purge** counterpart: an agent may undo a deletion, never
/// make one irreversible.
async fn doc_restore(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: RestoreArgs = parse_args("doc_restore", args)?;
    if !a.trash_id.starts_with(TRASH_ID_PREFIX) {
        return Err(DocError::BadRequest(format!(
            "trash ids start with `{TRASH_ID_PREFIX}`, got: {}",
            a.trash_id
        )));
    }
    let slot = state
        .trash
        .list()
        .await?
        .into_iter()
        .find(|e| e.trash_id == a.trash_id)
        .ok_or_else(|| DocError::DocNotFound(a.trash_id.clone()))?;

    state
        .acl
        .check_trashed(
            &ActorCtx {
                id: Some(actor.to_string()),
                is_admin: false,
            },
            &slot,
            AclRole::Edit,
        )
        .await?;

    let meta = state.trash.restore(&a.trash_id).await?;
    // No `unwrap_or_default()` here: an empty `path` reads as "restored at the
    // library root", which is a claim about where the document is, not a
    // missing decoration. Every other call site propagates the resolver error.
    let path = PathCtx::new(state.dirs.as_ref())
        .doc(state.docs.as_ref(), &meta.doc_id)
        .await?;
    Ok(json!({
        "restored": true,
        "doc_id": meta.doc_id,
        "name": meta.name,
        "path": path,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RmdirArgs {
    path: String,
}

// ── Manifest reconciliation table ─────────────────────────────────────
//
// Same rule as the PM crate (design 32 §7 D2): the field names the manifest
// declares must **equal** the wire names the implementation accepts, in both
// directions. A field the LLM cannot see is a feature that does not exist; a
// field the schema promises but the server ignores makes the caller's input
// vanish without an error. `manifest_fields_cover_every_tool` keeps the table
// from going stale — being on it is the rule, not an opt-in.

#[cfg(test)]
impl ListArgs {
    const FIELDS: &'static [&'static str] = &["path", "query", "offset", "limit"];
}

#[cfg(test)]
impl DocRefArgs {
    const FIELDS: &'static [&'static str] = &["ref"];
}

#[cfg(test)]
impl SearchArgs {
    const FIELDS: &'static [&'static str] = &["keyword", "limit"];
}

#[cfg(test)]
impl MkdirArgs {
    const FIELDS: &'static [&'static str] = &["path"];
}

#[cfg(test)]
impl WriteArgs {
    const FIELDS: &'static [&'static str] = &[
        "ref",
        "content",
        "mode",
        "if_version",
        "source_workspace",
        "source_path",
    ];
}

#[cfg(test)]
impl MoveArgs {
    const FIELDS: &'static [&'static str] = &["from", "to", "overwrite"];
}

#[cfg(test)]
impl RemoveArgs {
    const FIELDS: &'static [&'static str] = &["ref"];
}

#[cfg(test)]
impl CheckRequestArgs {
    const FIELDS: &'static [&'static str] = &["request_id"];
}

#[cfg(test)]
impl ListRequestsArgs {
    const FIELDS: &'static [&'static str] = &["status", "reference", "limit"];
}

#[cfg(test)]
impl ListTrashArgs {
    const FIELDS: &'static [&'static str] = &["limit"];
}

#[cfg(test)]
impl RestoreArgs {
    const FIELDS: &'static [&'static str] = &["trash_id"];
}

#[cfg(test)]
impl RmdirArgs {
    const FIELDS: &'static [&'static str] = &["path"];
}

/// Tool → the **wire field names** its implementation accepts.
#[cfg(test)]
pub(crate) const MANIFEST_FIELDS: &[(&str, &[&str])] = &[
    ("doc_list", ListArgs::FIELDS),
    ("doc_read", DocRefArgs::FIELDS),
    ("doc_stat", DocRefArgs::FIELDS),
    ("doc_write", WriteArgs::FIELDS),
    ("doc_move", MoveArgs::FIELDS),
    ("doc_remove", RemoveArgs::FIELDS),
    ("doc_mkdir", MkdirArgs::FIELDS),
    ("doc_search", SearchArgs::FIELDS),
    ("doc_check_request", CheckRequestArgs::FIELDS),
    ("doc_list_requests", ListRequestsArgs::FIELDS),
    ("doc_list_trash", ListTrashArgs::FIELDS),
    ("doc_restore", RestoreArgs::FIELDS),
    ("doc_rmdir", RmdirArgs::FIELDS),
];

/// `doc_rmdir` — remove an **empty** directory.
///
/// Deliberately not the REST behaviour: `DELETE /api/dirs/{id}` cascades a
/// whole subtree into the trash. An agent that names the wrong path would
/// sweep away an information architecture in one call, so this refuses unless
/// the directory is empty and makes the caller name what it means to delete.
///
/// Gated like every structural op ([`gate_structural`] → `AclTarget::Dir`),
/// which is the same rule the REST face applies to the same operation.
///
/// "Empty" counts the recycle bin too: a directory whose last document sits in
/// the trash still has an owner of record for that slot, and pruning it would
/// leave `doc_restore` pointing at a directory that no longer exists.
async fn doc_rmdir(state: &DocState, actor: &str, args: Value) -> Result<Value> {
    let a: RmdirArgs = parse_args("doc_rmdir", args)?;
    let path = a.path.trim().trim_matches('/');
    if path.is_empty() {
        return Err(DocError::BadRequest(
            "doc_rmdir requires a non-empty `path` (the root cannot be removed)".into(),
        ));
    }
    let dir_id = resolve_dir(state.dirs.as_ref(), path).await?;
    // `resolve_dir` never yields the root for a non-empty path, but the
    // constant is the boundary the call must never cross — keep it explicit
    // rather than relying on the resolver's current behaviour.
    if dir_id == ROOT_DIR_ID {
        return Err(DocError::BadRequest(
            "doc_rmdir cannot remove the library root".into(),
        ));
    }
    let meta = state.dirs.read(&dir_id).await?;
    gate_structural(state, actor, meta.creator.as_deref(), AclTarget::Dir(&dir_id)).await?;
    let tree = state.dirs.list_tree(&dir_id).await?;
    if !tree.files.is_empty() || !tree.dirs.is_empty() {
        return Err(DocError::BadRequest(format!(
            "`{path}` is not empty ({} document(s), {} sub-director(ies)); \
             move or remove its contents first — doc_rmdir never cascades",
            tree.files.len(),
            tree.dirs.len()
        )));
    }
    // A sub-directory would have been rejected above, so the only slots that
    // can target this directory are its own: `original_dir_id == dir_id`.
    let trashed = state
        .trash
        .list()
        .await?
        .into_iter()
        .filter(|e| e.original_dir_id == dir_id)
        .count();
    if trashed > 0 {
        return Err(DocError::BadRequest(format!(
            "`{path}` has {trashed} document(s) in the recycle bin; \
             restore or purge them first — removing the directory would strand them"
        )));
    }
    state.dirs.delete(&dir_id).await?;
    Ok(json!({ "removed": true, "path": path }))
}
