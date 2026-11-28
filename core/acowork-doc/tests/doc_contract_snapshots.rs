//! doc 域契约快照 + 地址不变量（技术方案 P-1）。
//!
//! 这两个东西解决的是同一类问题：**「重构没有改变现有行为」必须由机器证明，而不是由
//! review 的人肉比对来相信**。
//!
//! ## 1. 契约快照（contract snapshot）
//!
//! 不是全文 golden（无关字段一改就红，噪音吞掉真信号），而是**键集 + 形状 + 语义等值**：
//!
//! - [`rest_document_response_shape_is_pinned`]：`GET /docs/{id}` 的顶层键与 `meta` 键；
//! - [`refs_validate_outcome_shapes_are_pinned`]：`/refs/validate` 三种状态的键集与顺序；
//! - [`mcp_stat_path_is_human_readable`]：MCP `doc_stat` 的 path 形状。
//!
//! `DocRead.path` 在 P1 从**内部 `dir_id` 形状**（`dir-xxxx/x.md`）翻成人读路径
//! （`研发/纪要.md`）—— 那条断言就是 P1 的 allowed-diff：它先红，再在 diff 里显式
//! 改成新形状。注释里标 `P1 翻转` 的地方，就是「有意变更」的唯一登记处。
//! 用注释标了 `P1 翻转` 的地方，就是「有意变更」的唯一登记处。
//!
//! ## 2. 地址不变量（addressing invariants）
//!
//! 数据模型是唯一基座，那么它的三条不变量必须被机器证明：
//!
//! - **身份不变**：改名 / 移动之后 `doc_id` 仍是同一个，旧 id 仍可解析；
//! - **地址 ↔ 实体互逆**：`resolve(render(entry)) == entry`，`render` 的唯一实现是
//!   `DocMeta::file_name()`；
//! - **扩展名表全覆盖**：`IMPORTABLE_EXTS` 里每一个扩展名都有可寻址的实体，新增扩展名
//!   时这条测试自动覆盖到，不需要再手写一遍用例。
//!
//! 第三条是本文件最重要的结构：用例是**遍历扩展表**生成的，而不是逐格式手写。加一种格式
//! 只改 `types.rs` 一处，测试自己跟上。
//!
//! ## P0 之后的状态
//!
//! [`path_grammar_covers_every_entry_type`] 断言 markdown 与导入文件（pdf/docx/…）都能
//! 通过库内路径寻址 —— 这是 P-1 里 `path_grammar_covers_markdown_and_not_yet_imported_files`
//! 的翻转点（当时断言导入文件是 `invalid`）。
//! [`resolve_accepts_imported_files_while_validate_stays_markdown_only`] 钉住另一侧：
//! PM 的 `/refs/validate` 仍然只收 Markdown 交付物。两条路由、一个解析器、两层策略。

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use acowork_doc::types::{IMPORTABLE_EXTS, ROOT_DIR_ID};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

// ADR-073：MCP 侧身份（Gateway 注入的 runtime instance id）。
const AGENT: &str = "inst-1111-2222-3333-444444444444";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

async fn spawn() -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
    let addr: SocketAddr = svc
        .clone()
        .serve(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _data: data,
    }
}

impl TestServer {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.client.request(method, format!("{}{}", self.base, path));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    async fn mkdir(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .post("/dirs", json!({ "parent_dir_id": parent, "name": name }))
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdir {name}: {body}");
        body["dir_id"].as_str().unwrap().to_string()
    }

    async fn mkdoc(&self, dir_id: &str, title: &str, content: &str) -> String {
        let (status, body) = self
            .post(
                "/docs",
                json!({ "parent_dir_id": dir_id, "title": title, "content": content }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdoc {title}: {body}");
        body["doc_id"].as_str().unwrap().to_string()
    }

    /// `POST /docs/import?dir_id=…`（手搓 multipart，同 api_integration 的做法）。
    async fn upload(&self, dir_id: &str, file_name: &str, bytes: &[u8]) -> Value {
        const BOUNDARY: &str = "acowork-doc-contract-boundary";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let resp = self
            .client
            .post(format!("{}/docs/import?dir_id={dir_id}", self.base))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let raw = resp.bytes().await.unwrap().to_vec();
        let value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::CREATED, "upload {file_name}: {value:?}");
        value
    }

    /// `/refs/validate` 的逐条结果（与输入同序）。
    async fn validate(&self, refs: &[&str]) -> Vec<Value> {
        let (status, body) = self.post("/refs/validate", json!({ "refs": refs })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["results"].as_array().unwrap().clone()
    }

    /// `/refs/resolve` 的逐条结果（与输入同序）—— 地址层，对任何条目类型都有效。
    async fn resolve(&self, refs: &[&str]) -> Vec<Value> {
        let (status, body) = self.post("/refs/resolve", json!({ "refs": refs })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["results"].as_array().unwrap().clone()
    }

    /// MCP `tools/call`（模拟 Runtime 客户端，Gateway 注入 `X-MCP-Actor`）。
    async fn tool(&self, name: &str, args: Value) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let resp = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("x-mcp-actor", AGENT)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "MCP endpoint always answers 200");
        let body: Value = resp.json().await.unwrap();
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("MCP {name} failed: {body}"));
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
    }

    /// 一个目录下的条目名字（按名字断言 `render` 的唯一实现）。
    async fn entry_names(&self, dir_id: &str) -> Vec<String> {
        let (status, tree) = self.get(&format!("/tree?dir_id={dir_id}")).await;
        assert_eq!(status, StatusCode::OK, "{tree}");
        tree["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect()
    }
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

// ── 契约快照：REST ──────────────────────────────────────────────────────

/// `GET /docs/{id}` 的响应形状。任何新增/删除顶层键都会红 —— 那正是要 review 的契约变更。
#[tokio::test(flavor = "multi_thread")]
async fn rest_document_response_shape_is_pinned() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "研发").await;
    let doc_id = srv.mkdoc(&dir, "纪要", "# 内容").await;

    let (status, body) = srv.get(&format!("/docs/{doc_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(keys(&body), vec!["content", "meta", "path"], "DocRead 顶层键");
    assert_eq!(
        keys(&body["meta"]),
        vec!["created_at", "deleted", "doc_id", "name", "updated_at", "version"],
        "DocMeta 键集（`acl`/`creator`/`ext`/`import` 为 skip_serializing_if 缺省）"
    );
    // P1 翻转：`path` 是**人读路径**，与 MCP / refs 由同一个 `DocPathRenderer`
    // 渲染。这里曾经断言 `{dir_id}/纪要.md` —— 那是把基座的内部布局直接出了边界。
    assert_eq!(body["path"], "研发/纪要.md", "P1 翻转：人读路径");
    // R5 扩到整个 body：`dir_id` 只允许出现在请求里，不允许出现在应答里。
    assert!(
        !body.to_string().contains(&dir),
        "GET /docs/{{id}} 泄漏了内部 dir_id：{body}"
    );
}

/// 死接口保持死：`GET /docs/{id}/path` 已删除（它返回内部 `dir_id/name.md`，
/// 零调用者）。路径的唯一出口是 `GET /docs/{id}` 的 `path` 字段 —— 重新加回一条
/// 会泄漏内部 id 的路由，必须是一处显式的契约变更，而不是随手补个方便接口。
#[tokio::test(flavor = "multi_thread")]
async fn removed_doc_path_route_stays_removed() {
    let srv = spawn().await;
    let doc_id = srv.mkdoc(ROOT_DIR_ID, "纪要", "内容").await;
    let (status, _) = srv.get(&format!("/docs/{doc_id}/path")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "GET /docs/{{id}}/path 不应该存在");
}

/// `GET /tree?dir_id=` 的 `path` 也是人读形状（P1 之前是节点自己的 `dir_id`）。
///
/// 树是桌面最常刷新的边界：`path` 一旦是内部 id，它就会顺着 store 流到面包屑、
/// 搜索结果、标签页身份里 —— 泄漏一次，之后每一处都在复制那个形状。
#[tokio::test(flavor = "multi_thread")]
async fn tree_node_path_is_human_readable() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "研发").await;
    let (status, body) = srv.get(&format!("/tree?dir_id={dir}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["path"], "研发", "TreeNode.path 必须是祖先名字拼出的路径");
    // `dir_id` 是树契约的一部分（客户端靠它列子目录），但 `name` / `path` 不是 id 的
    // 藏身处：这两个字段曾经一起退化成 `dir-xxxx`。
    assert_eq!(body["name"], "研发", "TreeNode.name 必须是目录名，不是 dir_id");
    assert_eq!(body["dirs"].as_array().unwrap().len(), 0);
}

/// `/refs/validate` 三种状态的键集 + 输入顺序。PM 的 `doc_refs` 提交直接依赖这个形状。
#[tokio::test(flavor = "multi_thread")]
async fn refs_validate_outcome_shapes_are_pinned() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "研发").await;
    let doc_id = srv.mkdoc(&dir, "纪要", "内容").await;

    let results = srv
        .validate(&[
            doc_id.as_str(),
            "研发/纪要.md",
            "研发/不存在.md",
            "无后缀",
        ])
        .await;
    assert_eq!(results.len(), 4, "一条输入一条结果，顺序保持");

    // found：doc_id 与 path 都要给，且 path 是人读形状（绝不含内部 dir_id）。
    assert_eq!(keys(&results[0]), vec!["doc_id", "path", "ref", "status"]);
    assert_eq!(results[0]["status"], "found");
    assert_eq!(results[0]["doc_id"], doc_id);
    assert_eq!(results[0]["path"], "研发/纪要.md");
    assert_eq!(results[1]["status"], "found", "库内路径与人读形状互逆");

    // not_found：不给 doc_id/path —— 死链不能被伪装成有效引用。
    assert_eq!(keys(&results[2]), vec!["ref", "status"]);
    assert_eq!(results[2]["status"], "not_found");

    // invalid：必须与 not_found 区分（地址从未合法 ≠ 文档不存在）。
    assert_eq!(keys(&results[3]), vec!["reason", "ref", "status"]);
    assert_eq!(results[3]["status"], "invalid");

    // R5（白名单式收紧）：边界输出里不得出现内部 `dir_id`。P1 之后这条覆盖 `DocRead.path`。
    let (_, whole) = srv
        .post("/refs/validate", json!({ "refs": [doc_id.as_str()] }))
        .await;
    let text = whole.to_string();
    assert!(
        !text.contains("dir-"),
        "refs/validate 泄漏了内部 dir_id：{text}"
    );
}

/// MCP `doc_stat` 的 path 同样是**人读路径** —— 与 REST 的 `dir_id` 形状不一致，这条断言
/// 把差异摆到台面上，P1 收敛时才有一个「一致」的判据。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_stat_path_is_human_readable() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "研发").await;
    let doc_id = srv.mkdoc(&dir, "纪要", "内容").await;

    let stat = srv.tool("doc_stat", json!({ "ref": doc_id })).await;
    assert_eq!(stat["path"], "研发/纪要.md", "MCP 侧 path 已是人读形状");
    assert!(
        !stat.to_string().contains("dir-"),
        "MCP doc_stat 泄漏了内部 dir_id：{stat}"
    );
}

// ── 地址不变量 ──────────────────────────────────────────────────────────

/// 身份不变：改名、跨目录移动之后 `doc_id` 仍是同一个，且旧 id 仍可解析（历史消息、
/// 审批记录、任务的 `doc_refs` 存的都是 id，不能因为一次改名就作废）。
#[tokio::test(flavor = "multi_thread")]
async fn doc_id_survives_rename_and_move() {
    let srv = spawn().await;
    let from = srv.mkdir(ROOT_DIR_ID, "研发").await;
    let to = srv.mkdir(ROOT_DIR_ID, "评审").await;
    let doc_id = srv.mkdoc(&from, "草稿", "v1").await;

    let (status, meta) = srv
        .patch(
            &format!("/docs/{doc_id}/title"),
            json!({ "base_version": 1, "new_title": "评审结论" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    assert_eq!(meta["doc_id"], doc_id, "改名不换身份");

    let (status, meta) = srv
        .post(&format!("/docs/{doc_id}/move"), json!({ "target_dir_id": to }))
        .await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    assert_eq!(meta["doc_id"], doc_id, "移动不换身份");

    // 旧 id 仍可解析，且解析出的路径反映最新位置（path 是渲染结果，不是身份）。
    let results = srv.validate(&[doc_id.as_str()]).await;
    assert_eq!(results[0]["status"], "found");
    assert_eq!(results[0]["path"], "评审/评审结论.md");
}

/// 地址 ↔ 实体互逆：`resolve(render(entry)) == entry`。`render` 的唯一实现是
/// `DocMeta::file_name()`，所以这里只用目录名 + `meta` 拼路径，不读服务端给的 path。
///
/// 只覆盖当前地址语法接受的条目（markdown）。导入文件的互逆性由
/// [`path_grammar_covers_markdown_and_not_yet_imported_files`] 单独登记 —— P0 的翻转点
/// 集中在一处，不散落成两份要同时改的断言。
#[tokio::test(flavor = "multi_thread")]
async fn resolve_render_round_trip_for_markdown() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "资料").await;
    let md_id = srv.mkdoc(&dir, "需求", "# 需求").await;

    let rendered = "资料/需求.md";
    let results = srv.validate(&[rendered]).await;
    assert_eq!(results[0]["status"], "found", "{rendered} 应解析回 {md_id}");
    assert_eq!(results[0]["doc_id"], md_id.as_str(), "render/resolve 必须互逆");
    assert_eq!(results[0]["path"], rendered, "解析出的 path 必须等于渲染结果");
}

/// 扩展名表全覆盖：`IMPORTABLE_EXTS` 的每一项都要有可寻址实体。新增一种格式只改
/// `types.rs`，这条测试自动跟上 —— 结构化覆盖，而不是逐格式手写。
#[tokio::test(flavor = "multi_thread")]
async fn every_importable_extension_is_addressable_by_doc_id() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "上传").await;

    for (ext, kind) in IMPORTABLE_EXTS.iter() {
        // 文本格式导入后变成 markdown 文档（ext 为 null）；其余保留自己的扩展名。
        let file_name = format!("样张{ext}.{ext}");
        let bytes: &[u8] = if kind.as_text { b"# hi\n" } else { b"\x00\x01binary" };
        let meta = srv.upload(&dir, &file_name, bytes).await;
        let doc_id = meta["doc_id"].as_str().unwrap().to_string();

        let expected_ext: Option<&str> = if kind.as_text { None } else { Some(ext) };
        match expected_ext {
            None => assert_eq!(meta["ext"], Value::Null, ".{ext} 应转成 markdown 文档"),
            Some(e) => assert_eq!(meta["ext"], e, ".{ext} 必须保留自己的扩展名"),
        }

        // 身份维度：任何条目都能用 doc_id 寻址（寻址属地址层，与内容类型无关；
        // `/refs/validate` 只用来看「能不能当 Markdown 交付物」，不是这个断言的对象）。
        let results = srv.resolve(&[doc_id.as_str()]).await;
        assert_eq!(results[0]["status"], "found", ".{ext} 的 doc_id 不可寻址");

        // 命名维度：树里的名字必须等于 `DocMeta::file_name()` 去掉扩展名的那一半。
        let names = srv.entry_names(&dir).await;
        assert!(
            names.contains(&meta["name"].as_str().unwrap().to_string()),
            ".{ext} 的树条目名与 meta.name 不一致：{names:?}"
        );
    }
}

/// 地址语法覆盖**每一种**条目：markdown 与 pdf/docx 都给出 `found`，且 `doc_id` 与渲染
/// 结果互逆。这是 P-1 里 `path_grammar_covers_markdown_and_not_yet_imported_files` 的
/// 翻转点（当时导入文件断言为 `invalid`）—— 改动只发生在这些行，且必须在 diff 里可见。
#[tokio::test(flavor = "multi_thread")]
async fn path_grammar_covers_every_entry_type() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "资料").await;
    let md_id = srv.mkdoc(&dir, "纪要", "内容").await;
    let pdf_id = srv.upload(&dir, "设计.pdf", b"%PDF-1.4\n").await["doc_id"]
        .as_str()
        .unwrap()
        .to_string();
    let docx_id = srv.upload(&dir, "方案.docx", b"PK\x03\x04").await["doc_id"]
        .as_str()
        .unwrap()
        .to_string();

    let results = srv
        .resolve(&["资料/纪要.md", "资料/设计.pdf", "资料/方案.docx"])
        .await;

    for (i, expected) in [md_id.as_str(), pdf_id.as_str(), docx_id.as_str()]
        .iter()
        .enumerate()
    {
        assert_eq!(results[i]["status"], "found", "第 {i} 条地址未解析：{results:?}");
        assert_eq!(results[i]["doc_id"], *expected, "地址与实体必须互逆");
    }
    // `ext` 在 found 上恒常存在：`null` 表示「Markdown 条目」，不是「字段缺失」。
    assert_eq!(results[0]["ext"], Value::Null);
    assert_eq!(results[1]["ext"], "pdf");
    assert_eq!(results[2]["ext"], "docx");
}

/// 扩展名大小写不敏感：AI 与用户都会写 `.PDF`，那不构成另一个地址。
#[tokio::test(flavor = "multi_thread")]
async fn address_extension_is_case_insensitive() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "资料").await;
    let pdf_id = srv.upload(&dir, "设计.pdf", b"%PDF-1.4\n").await["doc_id"]
        .as_str()
        .unwrap()
        .to_string();

    let results = srv.resolve(&["资料/设计.PDF"]).await;
    assert_eq!(results[0]["status"], "found");
    assert_eq!(results[0]["doc_id"], pdf_id.as_str());
}

/// 同一条地址在两侧的不同答案是**有意的**：链接侧（`/refs/resolve`）能寻址导入文件，
/// PM 的提交闸门（`/refs/validate`）仍然只收 Markdown 交付物。两条路由共用同一个解析器，
/// 差别只在这层策略 —— 这条测试就是那个差别的看门人。
#[tokio::test(flavor = "multi_thread")]
async fn resolve_accepts_imported_files_while_validate_stays_markdown_only() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "资料").await;
    srv.mkdoc(&dir, "纪要", "内容").await;
    let pdf_id = srv.upload(&dir, "设计.pdf", b"%PDF-1.4\n").await["doc_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resolved = srv
        .resolve(&["资料/纪要.md", "资料/设计.pdf", pdf_id.as_str()])
        .await;
    assert_eq!(resolved[0]["status"], "found");
    assert_eq!(resolved[1]["status"], "found", "链接侧必须能指向 pdf");
    assert_eq!(resolved[2]["status"], "found", "doc_id 形式对任何条目都有效");

    let validated = srv.validate(&["资料/纪要.md", "资料/设计.pdf"]).await;
    assert_eq!(validated[0]["status"], "found");
    assert_eq!(
        validated[1]["status"], "invalid",
        "PM 的 doc_refs 语义不变：导入文件不是 Markdown 交付物"
    );
}

/// `/refs/resolve` 的应答形状：`found` 带齐打开所需的四件套（doc_id / path / name / ext），
/// 其余状态不给这些字段 —— 半截答案会让调用方开出一个空 tab。
#[tokio::test(flavor = "multi_thread")]
async fn refs_resolve_outcome_shapes_are_pinned() {
    let srv = spawn().await;
    let dir = srv.mkdir(ROOT_DIR_ID, "资料").await;
    let pdf_id = srv.upload(&dir, "设计.pdf", b"%PDF-1.4\n").await["doc_id"]
        .as_str()
        .unwrap()
        .to_string();

    let results = srv
        .resolve(&["资料/设计.pdf", "资料/不存在.pdf", "资料/没有扩展名"])
        .await;

    assert_eq!(
        keys(&results[0]),
        vec!["doc_id", "ext", "name", "path", "ref", "status"],
        "found 的字段集"
    );
    assert_eq!(results[0]["doc_id"], pdf_id.as_str());
    assert_eq!(results[0]["path"], "资料/设计.pdf", "path 是人读形状");
    assert_eq!(results[0]["name"], "设计", "name 是不带扩展名的条目名");

    assert_eq!(keys(&results[1]), vec!["ref", "status"]);
    assert_eq!(results[1]["status"], "not_found");

    assert_eq!(keys(&results[2]), vec!["reason", "ref", "status"]);
    assert_eq!(results[2]["status"], "invalid", "没有扩展名从来不是合法地址");

    // R5：地址应答里绝不出现内部 dir_id。
    let (_, whole) = srv
        .post("/refs/resolve", json!({ "refs": [pdf_id.as_str()] }))
        .await;
    assert!(
        !whole.to_string().contains("dir-"),
        "refs/resolve 泄漏了内部 dir_id：{whole}"
    );
}

/// 目录链缺失与文档不存在必须给同一个答案（`not_found`），不能借错误码泄漏目录结构。
#[tokio::test(flavor = "multi_thread")]
async fn missing_dir_chain_and_missing_doc_are_indistinguishable() {
    let srv = spawn().await;
    srv.mkdir(ROOT_DIR_ID, "研发").await;

    let results = srv
        .validate(&["研发/不存在.md", "不存在/也没有.md"])
        .await;
    assert_eq!(results[0]["status"], "not_found");
    assert_eq!(results[1]["status"], "not_found", "缺目录段不得与缺文档区分");
    assert_eq!(keys(&results[1]), keys(&results[0]));
}

/// 导入条目的 `ImportKind` 语义不能在收敛过程中被改写：文本格式转 markdown 文档，
/// 其余原样保存字节。这是 P2「按 ext 分派打开方式」的判据来源。
#[tokio::test(flavor = "multi_thread")]
async fn import_kind_decides_entry_shape() {
    let text_exts: Vec<&str> = IMPORTABLE_EXTS
        .iter()
        .filter(|(_, k)| k.as_text)
        .map(|(e, _)| *e)
        .collect();
    let file_exts: Vec<&str> = IMPORTABLE_EXTS
        .iter()
        .filter(|(_, k)| !k.as_text)
        .map(|(e, _)| *e)
        .collect();

    assert!(
        text_exts.contains(&"md") && file_exts.contains(&"pdf"),
        "扩展表至少要覆盖 markdown 与 pdf：{text_exts:?} / {file_exts:?}"
    );
    assert!(
        !file_exts.contains(&"md") && !file_exts.contains(&"txt"),
        "md/txt 必须走文本导入（否则文档库里会出现不可编辑的 .md 文件条目）"
    );
}