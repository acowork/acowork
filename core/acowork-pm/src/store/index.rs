//! 二级内存索引：加速跨任务/跨项目查询。
//!
//! 启动时由 [`crate::store::tree::TreePmStore::rebuild_index`] 一次性填充。
//! 写操作时同步更新所有索引；崩溃后从 `task.json` 重建（幂等）。
//!
//! ## 设计原则
//!
//! 1. **派生数据也算冗余** —— 索引是缓存，重建成本 < 1 秒（千任务级别）
//! 2. **写时同步更新** —— 单一写路径，不引入最终一致性问题
//! 3. **不做事件溯源** —— 不持久化索引状态，避免与文件系统双写

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::types::{AttachmentId, Priority, ProjectId, ReviewStatus, TaskId, TaskStatus};

/// 二级索引。
///
/// 读写并发策略：`RwLock` 包裹。读多写少（看板/详情查询），
/// 写路径（create/update/delete）持写锁批量更新所有相关二级索引。
#[derive(Default, Debug)]
pub struct TaskIndex {
    /// Task ID → 索引条目
    pub by_id: HashMap<TaskId, TaskEntry>,
    /// Project ID → 该项目下所有任务
    pub by_project: HashMap<ProjectId, HashSet<TaskId>>,
    /// 责任人 → 任务集合（Agent 自查 / 看板按 assignee 分组）
    pub by_assignee: HashMap<String, HashSet<TaskId>>,
    /// 状态 → 任务集合（看板四列 + 拒绝/取消辅助列）
    pub by_status: HashMap<TaskStatus, HashSet<TaskId>>,
    /// 反向依赖图：A 依赖 B 时，`blocked_by[B]` 包含 A
    pub blocked_by: HashMap<TaskId, Vec<TaskId>>,
    /// 附件 ID → 所属任务（`GET /api/attachments/:id` O(1) 定位）。
    /// 由 tree.rs 在 register/delete_attachment 时同步维护。
    pub by_attachment: HashMap<AttachmentId, TaskId>,
}

/// 单条任务的索引条目。
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub project_id: ProjectId,
    pub status: TaskStatus,
    pub assignee: Option<String>,
    /// 物理嵌套深度（根=0）。限制 `max_task_depth`（默认 5）。
    pub depth: u8,
    /// 任务目录绝对路径（含 `task.json` 的目录）。
    ///
    /// 嵌套任务靠此字段 O(1) 定位物理位置；reparent / 删除时同步更新。
    pub dir_path: PathBuf,
    /// 派发扫描器过滤字段（纯内存过滤，避免每 tick 读 task.json）
    pub start_at: Option<DateTime<Utc>>,
    pub dispatched_at: Option<DateTime<Utc>>,
    pub review_status: ReviewStatus,
    pub priority: Priority,
}

impl TaskEntry {
    /// 从持久化 `Task` 派生索引条目（写路径唯一构造入口，防止字段漂移）。
    pub(crate) fn new(
        project_id: ProjectId,
        depth: u8,
        dir_path: PathBuf,
        task: &crate::types::Task,
    ) -> Self {
        Self {
            project_id,
            status: task.status,
            assignee: task.assignee.clone(),
            depth,
            dir_path,
            start_at: task.start_at,
            dispatched_at: task.dispatched_at,
            review_status: task.review_status,
            priority: task.priority,
        }
    }
}

impl TaskIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入任务条目（写时调用）。
    pub fn insert(&mut self, task_id: TaskId, entry: TaskEntry) {
        self.by_id.insert(task_id.clone(), entry.clone());
        self.by_project
            .entry(entry.project_id.clone())
            .or_default()
            .insert(task_id.clone());
        if let Some(assignee) = &entry.assignee {
            self.by_assignee
                .entry(assignee.clone())
                .or_default()
                .insert(task_id.clone());
        }
        self.by_status
            .entry(entry.status)
            .or_default()
            .insert(task_id);
    }

    /// 移除任务条目（删除时调用）。
    pub fn remove(&mut self, task_id: &TaskId) -> Option<TaskEntry> {
        let entry = self.by_id.remove(task_id)?;
        if let Some(set) = self.by_project.get_mut(&entry.project_id) {
            set.remove(task_id);
        }
        if let Some(assignee) = &entry.assignee
            && let Some(set) = self.by_assignee.get_mut(assignee)
        {
            set.remove(task_id);
        }
        if let Some(set) = self.by_status.get_mut(&entry.status) {
            set.remove(task_id);
        }
        // 反向依赖图中的引用由反向 `blocked_by` 维护方负责清理
        Some(entry)
    }

    /// 反向依赖图：注册"A 依赖 B"。
    pub fn add_dependency(&mut self, dependent: TaskId, blocker: TaskId) {
        self.blocked_by.entry(blocker).or_default().push(dependent);
    }

    /// 反向依赖图：移除某任务**作为 dependent** 的所有引用
    ///（删除任务 / 重写 depends_on 时调用，避免悬挂引用）。
    pub fn remove_dependent_refs(&mut self, dependent: &TaskId) {
        self.blocked_by
            .retain(|_blocker, dependents| {
                dependents.retain(|d| d != dependent);
                !dependents.is_empty()
            });
    }

    /// 反向依赖图：移除某任务**作为 blocker** 的整条记录
    ///（删除被依赖任务时调用）。
    pub fn remove_blocker(&mut self, blocker: &TaskId) {
        self.blocked_by.remove(blocker);
    }

    /// 附件反向索引：注册 附件 ID → 所属任务。
    pub fn register_attachment(&mut self, att_id: AttachmentId, task_id: TaskId) {
        self.by_attachment.insert(att_id, task_id);
    }

    /// 附件反向索引：移除某任务下的全部附件（删除任务时调用）。
    pub fn remove_task_attachments(&mut self, task_id: &TaskId) {
        self.by_attachment.retain(|_att, tid| tid != task_id);
    }

    /// 附件反向索引：移除单个附件（删除附件时调用）。
    pub fn unregister_attachment(&mut self, att_id: &AttachmentId) {
        self.by_attachment.remove(att_id);
    }

    /// 清空所有索引（重建前调用）。
    pub fn clear(&mut self) {
        self.by_id.clear();
        self.by_project.clear();
        self.by_assignee.clear();
        self.by_status.clear();
        self.blocked_by.clear();
        self.by_attachment.clear();
    }

    /// 当前索引中的任务数。
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_remove() {
        let mut idx = TaskIndex::new();
        let tid = TaskId::generate();
        let pid = ProjectId::generate();

        idx.insert(
            tid.clone(),
            TaskEntry {
                project_id: pid.clone(),
                status: TaskStatus::Pending,
                // ADR-073: assignee 是 instance_id（UUID）
                assignee: Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string()),
                depth: 0,
                dir_path: "/tmp/pm/tasks/t-1".into(),
                start_at: None,
                dispatched_at: None,
                review_status: ReviewStatus::NotRequired,
                priority: Priority::Normal,
            },
        );
        assert_eq!(idx.len(), 1);
        assert!(idx.by_project.get(&pid).unwrap().contains(&tid));

        let removed = idx.remove(&tid);
        assert!(removed.is_some());
        assert_eq!(idx.len(), 0);
        assert!(idx.by_project.get(&pid).unwrap().is_empty());
    }
}