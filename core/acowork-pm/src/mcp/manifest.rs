//! MCP 工具 manifest（JSON 格式，对应 `/mcp/tools/list` 响应）。
//!
//! ## 工具清单
//!
//! | 工具 | 用途 | 对应 REST（公开路径 `/api/pm/*`） |
//! |------|------|-----------|
//! | `pm_list_projects` | 列出项目 | `GET /api/pm/projects` |
//! | `pm_get_project` | 项目详情 | `GET /api/pm/projects/:pid` |
//! | `pm_create_project` | 创建项目 | `POST /api/pm/projects` |
//! | `pm_list_tasks` | 列出任务 | `GET /api/pm/projects/:pid/tasks` |
//! | `pm_get_task` | 任务详情 | `GET /api/pm/tasks/:tid` |
//! | `pm_create_task` | 创建任务 | `POST /api/pm/projects/:pid/tasks` |
//! | `pm_update_task` | 更新任务 | `PATCH /api/pm/tasks/:tid` |
//! | `pm_claim_task` | 认领任务 | `POST /api/pm/tasks/:tid/claim` |
//! | `pm_submit_task` | 提交结果 | `POST /api/pm/tasks/:tid/submit` |
//! | `pm_list_my_tasks` | Agent 自查 | `GET /api/pm/tasks?assignee=X` |
//!
//! ## 与 REST 的差异
//!
//! - **自动注入 actor**：从 MCP 客户端 ID（`X-MCP-Actor` header）注入 `created_by` / `assignee`
//! - **精简响应**：仅返回 LLM 需要的字段（无 metadata、空 description 字段省略）
//! - **批量友好**：列表接口默认 `limit=20`，避免一次性返回大量数据撑爆上下文

/// MCP 工具 manifest（编译时常量，便于 LLM 直接 prompt-include）。
///
/// JSON Schema 子集严格遵循 MCP 规范（[tools/list](https://modelcontextprotocol.io/docs/concepts/tools#tool-definition)）。
pub const PM_TOOL_MANIFEST: &str = r#"{
  "schema_version": "2025-06-18",
  "tools": [
    {
      "name": "pm_list_projects",
      "description": "List all projects. Returns summary: id, title, status, task counts.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "include_archived": {
            "type": "boolean",
            "description": "Include archived/completed projects (default: false)",
            "default": false
          }
        }
      }
    },
    {
      "name": "pm_get_project",
      "description": "Get project details including task count breakdown.",
      "inputSchema": {
        "type": "object",
        "required": ["project_id"],
        "properties": {
          "project_id": {
            "type": "string",
            "pattern": "^p-[a-zA-Z0-9-]{1,62}$"
          }
        }
      }
    },
    {
      "name": "pm_create_project",
      "description": "Create a new project. Returns the created project with generated id.",
      "inputSchema": {
        "type": "object",
        "required": ["title"],
        "properties": {
          "title": { "type": "string", "minLength": 1, "maxLength": 200 },
          "description": { "type": "string", "maxLength": 5000 }
        }
      }
    },
    {
      "name": "pm_list_tasks",
      "description": "List tasks in a project. Supports filter by status and assignee.",
      "inputSchema": {
        "type": "object",
        "required": ["project_id"],
        "properties": {
          "project_id": { "type": "string", "pattern": "^p-[a-zA-Z0-9-]{1,62}$" },
          "status": {
            "type": "string",
            "enum": ["pending", "in_progress", "submitted", "done", "rejected", "cancelled"]
          },
          "assignee": { "type": "string", "description": "human literal or agent_instance_id (UUID v4, ADR-073 §1.2)" },
          "only_blocked": { "type": "boolean", "default": false },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    },
    {
      "name": "pm_get_task",
      "description": "Get task details including dependency status (is_blocked, blocked_by) and depth.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" }
        }
      }
    },
    {
      "name": "pm_create_task",
      "description": "Create a task. Set parent_task_id to create a child task. Use depends_on to declare dependencies (other tasks must exist). Tasks created by an agent enter review_status=pending and CANNOT be claimed until a human approves them (pm_claim_task returns 409 review_not_approved); check pm_check_task to see whether approval landed.",
      "inputSchema": {
        "type": "object",
        "required": ["project_id", "title"],
        "properties": {
          "project_id": { "type": "string", "pattern": "^p-[a-zA-Z0-9-]{1,62}$" },
          "title": { "type": "string", "minLength": 1, "maxLength": 200 },
          "description": { "type": "string", "maxLength": 20000 },
          "type": {
            "type": "string",
            "enum": ["task", "bug", "feature", "chore", "checkpoint", "milestone"],
            "default": "task"
          },
          "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"], "default": "normal" },
          "parent_task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "assignee": {
            "type": "string",
            "description": "agent_instance_id (UUID v4, ADR-073 §1.2) to assign. Must exist in the Gateway agent directory (design §9.1, `GET /api/agents/{instance_id}`). Agent-created tasks enter review_status=pending regardless: a human must approve them before pm_claim_task will move them to in_progress. Poll pm_check_task to learn when approval landed."
          },
          "due_at": {
            "type": "string",
            "format": "date-time",
            "description": "RFC3339 due timestamp, e.g. 2026-09-05T00:00:00Z"
          },
          "start_at": {
            "type": "string",
            "format": "date-time",
            "description": "RFC3339 start timestamp. The PM dispatch scanner sends the wake-up intent to the assignee agent when start_at is reached (after human approval). Omit or null = dispatch as soon as the task is actionable."
          },
          "depends_on": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["task_id"],
              "properties": {
                "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
                "kind": { "type": "string", "enum": ["blocks", "relates", "duplicates"], "default": "blocks" }
              }
            }
          }
        }
      }
    },
    {
      "name": "pm_check_task",
      "description": "Check whether a task created by the calling agent has been approved by a human. Returns status + review_status. Only the task creator may call this.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" }
        }
      }
    },
    {
      "name": "pm_update_task",
      "description": "Update task fields. Only provided fields are modified.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "title": { "type": "string", "minLength": 1, "maxLength": 200 },
          "description": { "type": "string", "maxLength": 20000 },
          "status": {
            "type": "string",
            "enum": ["pending", "in_progress", "submitted", "done", "rejected", "cancelled"]
          },
          "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"] },
          "assignee": { "type": ["string", "null"] }
        }
      }
    },
    {
      "name": "pm_claim_task",
      "description": "Claim a task as the current agent. Transitions pending → in_progress. Returns 409 if the task was created by an agent and is still awaiting human approval (review_status=pending), or if blocked by dependencies. Actor is taken from MCP client identity.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" }
        }
      }
    },
    {
      "name": "pm_submit_task",
      "description": "Submit work results for a claimed task. Transitions in_progress → submitted. If task type is checkpoint/bug, enters pending review; otherwise auto-approved → done.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id", "text"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "text": { "type": "string", "minLength": 1, "maxLength": 20000 },
          "attachment_ids": {
            "type": "array",
            "items": { "type": "string", "pattern": "^att-[a-zA-Z0-9-]{1,58}$" }
          }
        }
      }
    },
    {
      "name": "pm_list_my_tasks",
      "description": "List tasks currently assigned to the calling agent. Useful for self-check on session start.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "status": {
            "type": "string",
            "enum": ["pending", "in_progress", "submitted", "done", "rejected", "cancelled"]
          },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    },
    {
      "name": "pm_reparent_task",
      "description": "Move a task to a new parent. Set new_parent=null to promote to project root. Fails with cycle_detected if would create a cycle.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id", "new_parent"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "new_parent": {
            "type": ["string", "null"],
            "pattern": "^t-[a-zA-Z0-9-]{1,62}$"
          }
        }
      }
    }
  ]
}"#;

/// 解析 manifest 中的工具数组（`tools/list` 响应的 `tools` 字段）。
///
/// 每次调用解析一次（manifest 是编译时常量，解析成本可忽略）。
/// 与 `PM_TOOL_MANIFEST` 保持单一事实来源。
pub fn manifest_tools() -> Vec<serde_json::Value> {
    let v: serde_json::Value =
        serde_json::from_str(PM_TOOL_MANIFEST).expect("PM_TOOL_MANIFEST must be valid JSON");
    v["tools"].as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 12);
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["inputSchema"].is_object());
        }
    }

    #[test]
    fn manifest_tools_matches_raw_manifest() {
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let raw = parsed["tools"].as_array().unwrap();
        assert_eq!(super::manifest_tools().len(), raw.len());
    }
}
