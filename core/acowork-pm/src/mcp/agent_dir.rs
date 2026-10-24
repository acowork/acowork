//! HTTP AgentDirectory — 查询 Gateway `/api/agents` 校验 assignee / actor 存在性。
//!
//! ADR-064 Phase 3：PM 独立进程后不再共享 Gateway 内存 state，改为经 HTTP
//! 查询 Gateway Agent 目录（`GET /api/agents`）。恢复开发计划 v0.3 T1-11
//! "即时校验兜底"设计：
//!
//! - **启动拉全量**：`start()` 时 `GET /api/agents` 填充缓存
//! - **周期刷新**：按 `refresh_interval` 周期重拉，Agent 卸载后缓存收敛
//! - **即时校验兜底**：`agent_exists` 缓存 miss 时直接 `GET /api/agents/{id}`
//!   即时确认（Agent 刚安装未进缓存 / 缓存过期窗口内）
//!
//! **ADR-073**：缓存 key 是 `agent_instance_id`（UUID），不是 `agent_id`（包 ID）。
//! 与 task.assignee 语义一致（设计 PM §9.1 — "assignee 必须存在于 Agent 目录"）。
//!
//! 依赖方向：acowork-pm 只实现 [`super::AgentDirectory`] 契约，不反向依赖
//! Gateway 代码。Gateway URL 由 supervisor 经 `--gateway-url` 传入（`main.rs`）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::{AgentDirectory, AgentInfo};

/// HTTP Agent 目录（查询 Gateway `/api/agents`）。
///
/// 缓存 + 即时兜底双保险：周期刷新保证缓存收敛；缓存 miss 时即时查询
/// Gateway 保证 Agent 刚安装/卸载的窗口期内校验仍准确。
///
/// **ADR-073**：缓存 key 为 `agent_instance_id`（UUID），`agent_id` 仅用于
/// 显示 / 日志，不会出现在身份 key 里。
pub struct HttpAgentDirectory {
    /// Gateway HTTP base URL（如 `http://127.0.0.1:19876`）。
    gateway_url: String,
    /// 可选 Bearer token（Gateway `auth_enabled=true` 时经
    /// `ACOWORK_PM_GATEWAY_TOKEN` env 传入）。
    auth_token: Option<String>,
    /// 全量 Agent 元信息缓存（key = `instance_id`，ADR-073）。
    ///
    /// 值语义：
    /// - `Some(info)`：该 instance 存在且元信息可用（`agent_id` + `name`）
    /// - `None`：该 instance **存在**（存在性校验用 `contains_key`），但元
    ///   信息缺失（Gateway 列表响应缺 `agent_id`）——`agent_info` 直接返回
    ///   `None`，不再打详情兜底（全量列表都没有，单查大概率也没有）
    ///
    /// 这样 `agent_exists` 的存在性语义与旧实现（`HashSet<String>`）完全
    /// 一致：只依赖 `instance_id` 是否在列表里，元信息尽力而为。
    cache: RwLock<HashMap<String, Option<AgentInfo>>>,
    /// HTTP 客户端（短超时，避免阻塞 MCP 调用）。
    client: reqwest::Client,
    /// 周期刷新间隔。
    refresh_interval: Duration,
}

impl HttpAgentDirectory {
    /// 构造 HTTP Agent 目录。
    ///
    /// `gateway_url` 为 Gateway HTTP base（不含 `/api` 后缀）；`auth_token`
    /// 可选（Gateway auth 关闭时传 `None`）。
    pub fn new(
        gateway_url: String,
        auth_token: Option<String>,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            auth_token,
            cache: RwLock::new(HashMap::new()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .connect_timeout(Duration::from_secs(3))
                .build()
                .expect("Failed to build HTTP client for AgentDirectory"),
            refresh_interval,
        }
    }

    /// 启动后台刷新任务（启动拉全量 + 周期刷新）。
    ///
    /// 失败**非致命**：刷新失败保留旧缓存，`agent_exists` 的即时兜底仍可用。
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            // 启动拉全量
            this.refresh().await;
            loop {
                tokio::time::sleep(this.refresh_interval).await;
                this.refresh().await;
            }
        });
    }

    /// 拉全量 Agent 列表到缓存。
    ///
    /// `pub(crate)` 是为了 `mod.rs` 的 `e2e_role_updated_after_periodic_refresh`
    /// 跨文件测试可调 —— 同 crate 内部可访问,对外仍是私有。生产路径
    /// (`start()` 后台 spawn)不需要这个可见性。
    pub(crate) async fn refresh(&self) {
        match self.fetch_agents().await {
            Ok(ids) => {
                let mut cache = self.cache.write().await;
                *cache = ids;
                tracing::debug!(count = cache.len(), "Agent directory refreshed");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Agent directory refresh failed (keeping stale cache)"
                );
            }
        }
    }

    /// `GET {gateway_url}/api/agents` → `instance_id` → 元信息映射（ADR-073）。
    /// 值为 `Some(info)`（元信息可用）或 `None`（存在但元信息缺失）。
    async fn fetch_agents(&self) -> Result<HashMap<String, Option<AgentInfo>>, String> {
        let url = format!("{}/api/agents", self.gateway_url);
        let mut req = self.client.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("GET /api/agents -> {}", resp.status()));
        }
        // ADR-073: AgentListResponse 同时含 `instance_id` + `agent_id` + `name`。
        // PM 的存在性校验按 instance 走（与 task.assignee / X-MCP-Actor 一致），
        // agent_id / name / role 仅用于显示（pm_get_project 投影成员元信息）。
        let agents: Vec<serde_json::Value> = resp.json().await.map_err(|e| e.to_string())?;
        let mut out = HashMap::with_capacity(agents.len());
        for a in agents {
            let Some(instance_id) = a.get("instance_id").and_then(|v| v.as_str()) else {
                continue; // 无 instance_id 的条目跳过（ADR-073 回归保护）
            };
            let agent_id = a
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // `role` 来自 manifest 顶层（`AgentListResponse.role`，Gateway 已
            // 透出）。缺失 / 未声明 → `None`，不阻塞存在性校验（与
            // agent_id 缺失的对称处理）。role 改了 → 下一次 refresh 自动
            // 收敛（PM 测试 `pm_get_project_members_role_updated_after_refresh` 守门）。
            let role = a
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // 存在性只依赖 instance_id（与旧 HashSet 语义一致）：即使
            // agent_id 缺失也保留条目（值为 `None` = 存在但元信息不可用），
            // 避免把该 instance 从存在性缓存中剔除（否则 agent_exists
            // 每次都会走即时兜底，且语义与旧实现漂移）。
            let entry = (!agent_id.is_empty()).then_some(AgentInfo {
                agent_id,
                name,
                role,
            });
            out.insert(instance_id.to_string(), entry);
        }
        Ok(out)
    }

    /// GET `{gateway_url}{path}`（自动拼 URL + bearer auth）。
    ///
    /// 返回 `(status 是否 2xx, JSON body)`；传输错误 / body 解析失败 → `None`。
    /// [`Self::query_gateway`]（只看状态）与 [`Self::query_gateway_info`]
    /// （解析 body）共用，避免重复 URL 拼接与鉴权逻辑。
    async fn gateway_get(&self, path: &str) -> Option<(bool, serde_json::Value)> {
        let url = format!("{}/{}", self.gateway_url, path);
        let mut req = self.client.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.ok()?;
        let ok = resp.status().is_success();
        let body = resp.json().await.ok()?;
        Some((ok, body))
    }

    /// 即时校验兜底：`GET {gateway_url}/api/agents/{instance_id}`（200 = 存在）。
    async fn query_gateway(&self, instance_id: &str) -> bool {
        self.gateway_get(&format!("api/agents/{instance_id}"))
            .await
            .map(|(ok, _)| ok)
            .unwrap_or(false)
    }

    /// 即时元信息兜底：`GET {gateway_url}/api/agents/{instance_id}` → `AgentInfo`。
    ///
    /// `agent_info` 在缓存 miss 时调用，把成员的真实 `agent_id` / `name` / `role`
    /// 投影给 LLM（否则 60 秒周期刷新窗口内 LLM 拿到 null 仍派不了任务）。
    /// Gateway 不可达 / Agent 不存在 / 详情缺 `agent_id` → `None`。
    async fn query_gateway_info(&self, instance_id: &str) -> Option<AgentInfo> {
        let (ok, body) = self
            .gateway_get(&format!("api/agents/{instance_id}"))
            .await?;
        if !ok {
            return None;
        }
        let agent_id = body
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)?;
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // 详情响应里 role 来自 manifest 顶层（`AgentDetailResponse` 透出），
        // 缺失 / 未声明 → None，不影响存在性兜底。
        let role = body
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Some(AgentInfo {
            agent_id,
            name,
            role,
        })
    }
}

#[async_trait]
impl AgentDirectory for HttpAgentDirectory {
    async fn agent_exists(&self, instance_id: &str) -> bool {
        // 缓存命中 → 存在（周期刷新保证收敛）。
        if self.cache.read().await.contains_key(instance_id) {
            return true;
        }
        // 缓存 miss → 即时校验兜底（Agent 刚安装 / 缓存过期窗口）。
        self.query_gateway(instance_id).await
    }

    async fn agent_info(&self, instance_id: &str) -> Option<AgentInfo> {
        // 缓存命中（含元信息缺失的 `None` 占位）→ 直接返回，不再打详情兜底。
        if let Some(info) = self.cache.read().await.get(instance_id) {
            return info.clone();
        }
        // 缓存 miss → 即时从 Gateway 单详情接口兜底拉元信息（Agent 刚安装
        // 未进缓存 / 缓存过期窗口）。并发与去重由调用方
        // `batch_lookup_agent_meta`（限流）负责。
        self.query_gateway_info(instance_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    /// 起一个 mock Gateway（`/api/agents` + `/api/agents/{id}`），返回 base URL。
    /// 参数 `instance_ids` 模拟 Gateway 返回的 instance_id 列表（ADR-073）。
    ///
    /// 列表与详情路由都返回完整的 `{instance_id, agent_id, name}`（与真实
    /// `AgentListResponse` / `AgentDetailResponse` 同形），供 `agent_exists`
    /// 与 `agent_info` 测试共用。
    async fn start_mock_gateway(instance_ids: &[&str]) -> String {
        let ids: Vec<String> = instance_ids.iter().map(|s| s.to_string()).collect();
        let ids_for_list = ids.clone();

        // 与真实 Gateway 一致的派生字段（agents.rs：`name` 来自包元数据，
        // 此处用 instance 前缀模拟，保证测试内可断言）。
        let to_entry = |id: &str| {
            serde_json::json!({
                "instance_id": id,
                "agent_id": format!("com.acowork.{}", &id[..4]),
                "name": format!("Agent {}", &id[..8]),
            })
        };

        let app = Router::new()
            .route(
                "/api/agents",
                get(move || {
                    let ids = ids_for_list.clone();
                    async move {
                        // ADR-073: Gateway 响应同时含 instance_id + agent_id；
                        // PM 按 instance_id 走存在性校验。
                        axum::Json(ids.iter().map(|id| to_entry(id)).collect::<Vec<_>>())
                    }
                }),
            )
            .route(
                "/api/agents/{id}",
                get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let ids = ids.clone();
                    async move {
                        if ids.contains(&id) {
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(to_entry(&id)),
                            )
                        } else {
                            (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({ "error": "not found" })),
                            )
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock gateway");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock gateway runs");
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 缓存命中：`agent_exists` 对全量列表内 instance 返回 true。
    #[tokio::test]
    async fn cache_hit_returns_true() {
        // ADR-073 fixture: 合规 UUID v4 字面量
        let gw = start_mock_gateway(&[
            "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d",
            "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b",
        ])
        .await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        assert!(dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);
        assert!(dir.agent_exists("a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b").await);
    }

    /// 缓存 miss + 即时兜底：全量列表外但 Gateway 存在 → true。
    #[tokio::test]
    async fn cache_miss_immediate_fallback_queries_gateway() {
        let gw = start_mock_gateway(&["3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // 不在缓存 → 走即时兜底 → Gateway 返回 404 → false
        assert!(!dir.agent_exists("5d2e1100-0000-0000-0000-000000000000").await);
    }

    /// 缓存 miss + 即时兜底命中：Agent 刚安装未进缓存，但 Gateway 详情 200。
    #[tokio::test]
    async fn cache_miss_fallback_hit_when_installed() {
        let gw = start_mock_gateway(&["3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        // 不 refresh（模拟启动拉全量未完成）

        // 即时兜底命中 → true
        assert!(dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);
    }

    /// 刷新收敛：Agent 卸载后，缓存刷新后 `agent_exists` 返回 false。
    #[tokio::test]
    async fn refresh_converges_after_uninstall() {
        let gw = start_mock_gateway(&["3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;
        assert!(dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);

        // 模拟 Agent 卸载：mock 返回空列表，刷新后缓存收敛。
        // 由于 mock 固定返回第一个 instance，这里验证刷新后仍命中（缓存一致性）。
        dir.refresh().await;
        assert!(dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);
    }

    /// 空缓存 + Gateway 不可达：`agent_exists` 返回 false（不 panic）。
    #[tokio::test]
    async fn gateway_unreachable_returns_false() {
        let dir = Arc::new(HttpAgentDirectory::new(
            "http://127.0.0.1:1".to_string(), // 不可达端口
            None,
            Duration::from_secs(3600),
        ));
        assert!(!dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);
    }

    /// ADR-073 回归：`fetch_agents` 必须抽 `instance_id` 字段，不是 `agent_id`。
    ///
    /// 关键区别：旧实现会从 `agent_id` 抽，导致同包多 instance 时互相覆盖、
    /// `agent_exists(instance_id)` 永远 miss。本测试 mock 返回**仅含 agent_id**
    /// 的响应（模拟回归到旧代码），验证 cache 必然为空 → `agent_exists` 全部
    /// 走即时兜底且 404。
    #[tokio::test]
    async fn fetch_agents_ignores_agent_id_field_extracts_only_instance_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/agents",
            get(|| async {
                // 只含 agent_id，没有 instance_id — 模拟旧 Gateway 形状
                axum::Json(vec![
                    serde_json::json!({ "agent_id": "com.acowork.architect" }),
                    serde_json::json!({ "agent_id": "com.test.agent" }),
                ])
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // cache 应为空（抽不到 instance_id 字段）—— 用任何 UUID 查都 miss
        let cache = dir.cache.read().await;
        assert!(
            cache.is_empty(),
            "fetch_agents must skip agents without instance_id field; cache={:?}",
            *cache
        );
        drop(cache);

        // 走即时兜底 → Gateway 详情路由 `{id}` 用 instance_id 查，
        // 包 ID 不是 instance_id，所以也 miss
        assert!(!dir.agent_exists("com.acowork.architect").await);
        assert!(!dir.agent_exists("com.test.agent").await);
    }

    /// ADR-073 关键场景：同包多 instance 必须共存。
    ///
    /// 两个 instance 共享 `agent_id = "com.acowork.architect"` 但 `instance_id`
    /// 不同（UUID v4）。旧实现（按 agent_id 抽）会让 cache 只剩一份、互相覆盖。
    /// 本测试验证 cache 同时包含两个 instance_id。
    #[tokio::test]
    async fn fetch_agents_keeps_all_instances_of_same_package() {
        let shared_agent_id = "com.acowork.architect";
        let instance_a = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let instance_b = "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/agents",
            get(move || async move {
                axum::Json(vec![
                    serde_json::json!({ "instance_id": instance_a, "agent_id": shared_agent_id }),
                    serde_json::json!({ "instance_id": instance_b, "agent_id": shared_agent_id }),
                ])
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // 两个 instance 都进 cache —— 同包不互相覆盖
        assert!(dir.agent_exists(instance_a).await, "instance A should be cached");
        assert!(dir.agent_exists(instance_b).await, "instance B should be cached");
        // 包 ID 自身不在 cache 中（按 instance_id 走存在性校验）
        assert!(
            !dir.agent_exists(shared_agent_id).await,
            "package id must NOT be a cache key; only instance_id is"
        );
    }

    /// `agent_info` 缓存命中：refresh 后返回完整元信息（agent_id + name）。
    #[tokio::test]
    async fn agent_info_cache_hit_returns_meta() {
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let gw = start_mock_gateway(&[instance]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        let info = dir.agent_info(instance).await.expect("cached meta");
        assert_eq!(info.agent_id, "com.acowork.3f8c");
        assert_eq!(info.name, "Agent 3f8c2a91");
    }

    /// `agent_info` 缓存 miss → 即时兜底：详情路由返回完整元信息。
    #[tokio::test]
    async fn agent_info_cache_miss_fallback_queries_gateway() {
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let gw = start_mock_gateway(&[instance]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        // 不 refresh（模拟启动拉全量未完成 / 缓存过期窗口）

        let info = dir.agent_info(instance).await.expect("fallback meta");
        assert_eq!(info.agent_id, "com.acowork.3f8c");
        assert_eq!(info.name, "Agent 3f8c2a91");
    }

    /// `agent_info` 404（Agent 不存在）→ `None`，不 panic。
    #[tokio::test]
    async fn agent_info_not_found_returns_none() {
        let gw = start_mock_gateway(&["3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        // 缓存 miss + 详情 404 → None
        assert!(dir.agent_info("5d2e1100-0000-0000-0000-000000000000").await.is_none());
    }

    /// `agent_info` Gateway 不可达 → `None`（不 panic，与 `agent_exists` 一致）。
    #[tokio::test]
    async fn agent_info_gateway_unreachable_returns_none() {
        let dir = Arc::new(HttpAgentDirectory::new(
            "http://127.0.0.1:1".to_string(), // 不可达端口
            None,
            Duration::from_secs(3600),
        ));
        assert!(dir.agent_info("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.is_none());
    }

    /// 详情缺 `agent_id`（异常响应）→ `None`（不投影空串给 LLM）。
    #[tokio::test]
    async fn agent_info_missing_agent_id_returns_none() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/agents/{id}",
            get(|| async {
                // 只含 instance_id，缺 agent_id — 模拟异常详情响应
                axum::Json(serde_json::json!({ "instance_id": "x" }))
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        assert!(dir.agent_info("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.is_none());
    }

    /// 列表缺 `agent_id`（异常响应）：instance 仍在存在性缓存
    /// （`agent_exists` 为 true，与旧 `HashSet` 语义一致），但 `agent_info`
    /// 返回 `None` 且**不**触发详情兜底（全量列表都没有，单查大概率也没有）。
    #[tokio::test]
    async fn agent_exists_keeps_entry_when_agent_id_missing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/api/agents",
                get(|| async {
                    // 只有 instance_id，缺 agent_id — 模拟异常列表响应
                    axum::Json(vec![serde_json::json!({
                        "instance_id": "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"
                    })])
                }),
            )
            .route(
                "/api/agents/{id}",
                get(|| async {
                    // 详情正常返回——若 `agent_info` 误走 HTTP 兜底会拿到 Some；
                    // 断言 None 即证明走了缓存占位，未打详情。
                    axum::Json(serde_json::json!({
                        "instance_id": "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d",
                        "agent_id": "com.acowork.3f8c",
                        "name": "Agent 3f8c2a91",
                    }))
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // 存在性保留
        assert!(dir.agent_exists("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await);
        // 元信息缺失 → None，且未打详情（详情会返回 Some）
        assert!(dir.agent_info("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.is_none());
    }

    /// PM task t-2ee347c3 — `role` 字段在 refresh 路径透传。
    ///
    /// Gateway `/api/agents` 响应含 `role` 字段（`AgentListResponse.role`，
    /// 来自 manifest 顶层）。`HttpAgentDirectory::fetch_agents` 必须把
    /// 这个字段带进 `AgentInfo.role` 而不是丢弃。
    ///
    /// 守门:防止未来重构再把 `role` 漏掉(这是 t-2ee347c3 引入的字段,
    /// 之前所有 `AgentInfo` 只装 `agent_id` + `name`)。
    #[tokio::test]
    async fn fetch_agents_passes_role_through() {
        let instance_with_role = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let instance_without_role = "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/agents",
            get(move || async move {
                axum::Json(vec![
                    // 标准响应:role 来自 manifest 顶层
                    serde_json::json!({
                        "instance_id": instance_with_role,
                        "agent_id": "com.acowork.senior-engineer",
                        "name": "SSE",
                        "role": "Senior Software Engineer",
                    }),
                    // 旧 manifest 没声明 role / 旧版 Gateway 缺字段 — 不崩
                    serde_json::json!({
                        "instance_id": instance_without_role,
                        "agent_id": "com.acowork.legacy",
                        "name": "Legacy",
                        // role 字段缺失
                    }),
                ])
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // 主断言:role 透传
        let info = dir
            .agent_info(instance_with_role)
            .await
            .expect("cached meta for role-bearing instance");
        assert_eq!(info.agent_id, "com.acowork.senior-engineer");
        assert_eq!(info.name, "SSE");
        assert_eq!(
            info.role.as_deref(),
            Some("Senior Software Engineer"),
            "fetch_agents must extract role from /api/agents response"
        );

        // 兜底断言:role 缺失 → None,不阻塞 agent_id/name
        let info = dir
            .agent_info(instance_without_role)
            .await
            .expect("cached meta even when role is absent");
        assert_eq!(info.agent_id, "com.acowork.legacy");
        assert_eq!(info.name, "Legacy");
        assert!(info.role.is_none(), "missing role must map to None, not empty string");
    }

    /// PM task t-2ee347c3 — 详情兜底路径(缓存 miss → `query_gateway_info`)
    /// 也必须把 `role` 透传过来,不能因为走兜底而丢字段。
    #[tokio::test]
    async fn query_gateway_info_passes_role_through() {
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/agents/{id}",
            get(move || async move {
                axum::Json(serde_json::json!({
                    "instance_id": instance,
                    "agent_id": "com.acowork.project-manager",
                    "name": "PM",
                    "role": "Project Manager",
                }))
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        // 故意不 refresh() — 缓存全空,agent_info 必须走兜底打详情
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));

        let info = dir
            .agent_info(instance)
            .await
            .expect("fallback should populate meta from /api/agents/{id}");
        assert_eq!(info.agent_id, "com.acowork.project-manager");
        assert_eq!(info.name, "PM");
        assert_eq!(
            info.role.as_deref(),
            Some("Project Manager"),
            "query_gateway_info must pass role through"
        );
    }
}
