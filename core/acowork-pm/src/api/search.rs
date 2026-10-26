//! `GET /api/pm/search?q=` — 项目/任务全文子串搜索（ADR-081 P0-2）。
//!
//! 数据量小（千任务级），线性扫描 `list_projects` + `find_tasks` 即可，
//! 不建索引（YAGNI）。命中字段加权对齐 doc `LibrarySearchService` 的
//! score() 模式：title(10) > description(5) > assignee(3)。
//!
//! 返回统一 `PmSearchHit`（`kind: "project" | "task"`），Desktop 按
//! `kind` 决定跳转 ProjectBoard / TaskDetailDrawer。

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::store::tree::PmStore;
use crate::types::TaskFilter;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// ADR-081 D4 SearchHit（pm 类型专用 wire 形状）。
#[derive(Debug, Clone, Serialize)]
pub struct PmSearchHit {
    /// `"project" | "task"` — Desktop 决定跳转目标。
    pub kind: &'static str,
    /// 源内唯一 id（project id 或 `task:{tid}`）。
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub score: i32,
    /// 定位跳转 meta（ADR-081 D4 project 类型）。
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// 围绕首次命中截取 snippet（`…` 收尾，char 边界安全）——复制 doc 语义。
fn snippet(body: &str, needle: &str, radius: usize) -> String {
    let lower = body.to_lowercase();
    let n = needle.to_lowercase();
    match lower.find(&n) {
        Some(pos) => {
            let s = body[..pos.saturating_sub(radius)]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let e = body[..(pos + n.len() + radius).min(body.len())]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let mut out = body[s..e].to_string();
            if s > 0 {
                out.insert(0, '…');
            }
            if e < body.len() {
                out.push('…');
            }
            out
        }
        None => body.chars().take(60).collect(),
    }
}

#[tracing::instrument(skip(state))]
pub async fn search(
    State(state): State<ApiState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<PmSearchHit>>, crate::error::PmError> {
    let q = params.q.trim();
    let limit = params.limit.unwrap_or(20).min(50);
    if q.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let mut hits: Vec<PmSearchHit> = Vec::new();

    // ── Projects：title(10) / description(5) ─────────────────────
    for p in state.store.list_projects().await? {
        let mut score = 0i32;
        let mut snip = String::new();
        if contains_ci(&p.title, q) {
            score += 10;
            snip = p.title.clone();
        }
        if contains_ci(&p.description, q) {
            score += 5;
            if snip.is_empty() {
                snip = snippet(&p.description, q, 30);
            }
        }
        if score > 0 {
            hits.push(PmSearchHit {
                kind: "project",
                id: p.id.as_str().to_string(),
                title: p.title.clone(),
                snippet: snip,
                score,
                project_id: p.id.as_str().to_string(),
                task_id: None,
            });
        }
    }

    // ── Tasks：title(10) / description(5) / assignee(3) ──────────
    // `find_tasks(TaskFilter::default())` 即"遍历二级索引 + 逐 task.json"。
    let tasks = state.store.find_tasks(&TaskFilter::default()).await?;
    for t in tasks {
        let mut score = 0i32;
        let mut snip = String::new();
        if contains_ci(&t.title, q) {
            score += 10;
            snip = t.title.clone();
        }
        if contains_ci(&t.description, q) {
            score += 5;
            if snip.is_empty() {
                snip = snippet(&t.description, q, 30);
            }
        }
        if let Some(a) = &t.assignee
            && contains_ci(a, q)
        {
            score += 3;
            if snip.is_empty() {
                snip = a.clone();
            }
        }
        if score > 0 {
            hits.push(PmSearchHit {
                kind: "task",
                id: format!("task:{}", t.id.as_str()),
                title: t.title.clone(),
                snippet: snip,
                score,
                project_id: t.project_id.as_str().to_string(),
                task_id: Some(t.id.as_str().to_string()),
            });
        }
    }

    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    hits.truncate(limit);
    Ok(Json(hits))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PmConfig;
    use crate::mcp::NoopAgentDirectory;
    use crate::store::tree::TreePmStore;
    use crate::types::{CreateProject, CreateTask, Priority, ProjectId, TaskId, TaskType};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn test_config() -> PmConfig {
        let dir = tempdir().unwrap();
        PmConfig {
            data_dir: dir.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        }
    }

    async fn seed(store: &TreePmStore) -> (ProjectId, TaskId) {
        let p = store
            .create_project(
                CreateProject {
                    title: "全球搜索开发".into(),
                    description: "六源聚合检索方案".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        store
            .add_project_member(&p.id, "agent-1", crate::types::MemberKind::Agent)
            .await
            .unwrap();
        let t = store
            .create_task(
                &p.id,
                CreateTask {
                    title: "pm 搜索端点".into(),
                    description: "title description assignee 加权".into(),
                    task_type: TaskType::Task,
                    priority: Priority::Normal,
                    parent_task_id: None,
                    depends_on: vec![],
                    attachment_ids: vec![],
                    assignee: Some("agent-1".into()),
                    due_at: None,
                    start_at: None,
                },
                "human",
            )
            .await
            .unwrap();
        (p.id, t.id)
    }

    #[tokio::test]
    async fn empty_query_returns_nothing() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        seed(&store).await;
        let state = ApiState {
            store: Arc::new(store),
            config: test_config(),
            agent_dir: Arc::new(NoopAgentDirectory),
            gateway: None,
        };
        let Json(hits) = search(State(state), Query(SearchParams { q: "".into(), limit: None }))
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn title_ranks_above_description() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        // 项目 A：title 命中（score 10）。
        store
            .create_project(
                CreateProject {
                    title: "搜索标题项目".into(),
                    description: "无关描述".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        // 项目 B：仅 description 命中（score 5）。
        store
            .create_project(
                CreateProject {
                    title: "其他项目".into(),
                    description: "正文里提到搜索方案".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let state = ApiState {
            store: Arc::new(store),
            config: test_config(),
            agent_dir: Arc::new(NoopAgentDirectory),
            gateway: None,
        };
        let Json(hits) = search(State(state), Query(SearchParams { q: "搜索".into(), limit: None }))
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "搜索标题项目");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn task_matches_title_description_and_assignee() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let (pid, tid) = seed(&store).await;
        let state = ApiState {
            store: Arc::new(store),
            config: test_config(),
            agent_dir: Arc::new(NoopAgentDirectory),
            gateway: None,
        };
        // 任务 description 命中（"…assignee 加权"）。
        let Json(desc_hits) = search(State(state.clone()), Query(SearchParams { q: "加权".into(), limit: None }))
            .await
            .unwrap();
        assert_eq!(desc_hits.len(), 1);
        assert_eq!(desc_hits[0].task_id.as_deref(), Some(tid.as_str()));
        // 任务 assignee 命中。
        let Json(assignee_hits) = search(State(state), Query(SearchParams { q: "agent-1".into(), limit: None }))
            .await
            .unwrap();
        assert_eq!(assignee_hits.len(), 1);
        assert_eq!(assignee_hits[0].kind, "task");
        assert_eq!(assignee_hits[0].project_id, pid.as_str());
    }

    #[tokio::test]
    async fn limit_is_respected() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        seed(&store).await;
        let state = ApiState {
            store: Arc::new(store),
            config: test_config(),
            agent_dir: Arc::new(NoopAgentDirectory),
            gateway: None,
        };
        let Json(hits) = search(State(state), Query(SearchParams { q: "搜索".into(), limit: Some(1) }))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }
}
