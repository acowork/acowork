//! 与存储格式无关的业务规则 / 路径规则。
//!
//! 这些函数不依赖任何后端实例，SQLite 后端直接使用；把它们放在 `store`
//! 根层，避免运行代码反向依赖某个具体实现文件。

use std::path::{Component, Path};

use crate::error::{PmError, Result};
use crate::types::{Priority, ReviewStatus, Task, TaskActivity, TaskId, TaskStatus};

/// 校验 ID 格式：`{prefix}{suffix}`，suffix 字符集 `[a-zA-Z0-9-]`，长度 3-64。
pub fn validate_id_format(id: &str, prefix: &str, kind: &str) -> Result<()> {
    if id.len() < 3 || id.len() > 64 || !id.starts_with(prefix) {
        return Err(PmError::InvalidId(format!("{}: {}", kind, id)));
    }
    let suffix = &id[prefix.len()..];
    if !suffix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(PmError::InvalidId(format!("{}: {}", kind, id)));
    }
    Ok(())
}

/// 实体 ID 不得占用数据文件 / 目录的保留名。
pub const TASK_DIR_RESERVED: &[&str] = &[
    "task.json",
    "activity.json",
    "attachments",
    "result.json",
    "notes.jsonl",
    "checklist.json",
];

pub fn check_not_reserved(id: &str) -> Result<()> {
    if TASK_DIR_RESERVED.contains(&id) {
        return Err(PmError::ReservedId(id.to_string()));
    }
    Ok(())
}

pub fn parse_task_id(s: &str) -> Result<TaskId> {
    validate_id_format(s, "t-", "task")?;
    check_not_reserved(s)?;
    TaskId::parse(s).map_err(|_| PmError::InvalidId(format!("task: {}", s)))
}

/// 状态流转合法校验（对齐设计文档 §4 状态机图）。
pub fn validate_transition(from: TaskStatus, to: TaskStatus, task_id: &TaskId) -> Result<()> {
    let legal = from.legal_successors();
    if legal.contains(&to) {
        return Ok(());
    }
    let allowed = if legal.is_empty() {
        "(none — terminal status)".to_string()
    } else {
        legal
            .iter()
            .map(|s| s.board_column())
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(PmError::InvalidStateTransition {
        task_id: task_id.to_string(),
        from: from.board_column().to_string(),
        to: to.board_column().to_string(),
        allowed,
    })
}

/// **创建审批闸门**：Agent 创建的任务（`review_status=pending`）未经人类
/// 批准前，不得进入 `in_progress`。挂在所有进入该状态的写路径上。
pub fn ensure_creation_approved(task: &Task) -> Result<()> {
    if task.review_status == ReviewStatus::Pending && task.status == TaskStatus::Pending {
        return Err(PmError::ReviewNotApproved {
            task_id: task.id.to_string(),
        });
    }
    Ok(())
}

pub fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::Urgent => 0,
        Priority::High => 1,
        Priority::Normal => 2,
        Priority::Low => 3,
    }
}

/// 由 `task.result` 衍生出来的历史条目：提交历史能力上线前的存量任务，
/// 读历史时把 `result` 补种成一条 `Submitted`（确定性 ID）。
pub fn seeded_activity(record: &Task) -> Vec<TaskActivity> {
    let Some(result) = record.result.as_ref() else {
        return Vec::new();
    };
    let mut activity = TaskActivity::submitted(result);
    activity.id = TaskActivity::seeded_id(&record.id);
    vec![activity]
}

/// 只由普通组件构成的相对路径。`contains("..")` 不够：`PathBuf::join`
/// 遇到绝对路径会整体替换 base，所以根前缀也必须拒绝。
pub fn safe_rel(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_rejects_reserved_names_and_bad_shapes() {
        assert!(parse_task_id("t-abc123").is_ok());
        assert!(parse_task_id("task.json").is_err());
        assert!(parse_task_id("t-a/b").is_err());
        assert!(parse_task_id("p-abc").is_err());
    }

    #[test]
    fn safe_rel_rejects_absolute_and_traversal_paths() {
        assert!(safe_rel("attachments/a-1/original.png"));
        assert!(!safe_rel(""));
        assert!(!safe_rel("../pm.sqlite"));
        assert!(!safe_rel("a/../../b"));
        assert!(!safe_rel("/etc/passwd"));
    }
}
