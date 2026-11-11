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
//! | `pm_list_agents` | list installed agent instances | Gateway `GET /api/agents` |
//! | `pm_add_project_member` | invite a member (= makes it assignable) | `POST /api/pm/projects/:pid/members` |
//! | `pm_add_activity` | append a reply to a task | `POST /api/pm/tasks/:tid/activity` |
//! | `pm_list_activity` | task activity stream | `GET /api/pm/tasks/:tid/activity` |
//!
//! ## 与 REST 的差异
//!
//! - **自动注入 actor**：从 MCP 客户端 ID（`X-MCP-Actor` header）注入 `created_by` / `assignee`
//! - **精简响应**：仅返回 LLM 需要的字段（无 metadata、空 description 字段省略）
//! - **批量友好**：列表接口默认 `limit=20`，避免一次性返回大量数据撑爆上下文
//!
//! ## Schema and implementation must be reconciled (the cause of defect D2)
//!
//! `pm_update_task` long accepted `type` / `depends_on` in its implementation
//! while the manifest did not declare them — a field the LLM cannot see in the
//! schema is a feature that does not exist, and the old test only spot-checked
//! 6 fields, so it passed silently. The [`tools::manifest_fields`] table now
//! asserts **set equality** between "field names the implementation accepts"
//! and the manifest properties. Add a row when adding a tool.

/// MCP 工具 manifest（编译时常量，便于 LLM 直接 prompt-include）。
///
/// JSON Schema 子集严格遵循 MCP 规范（[tools/list](https://modelcontextprotocol.io/docs/concepts/tools#tool-definition)）。
pub const PM_TOOL_MANIFEST: &str = r#"{
  "schema_version": "2025-06-18",
  "version": "0.2.0",
  "tools": [
    {
      "name": "pm_list_projects",
      "description": "List all projects. Returns summary: id, title, status, task counts, and `doc_path` (the project's document-library directory, design 32 §5.5).",
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
      "description": "Get project details: task count breakdown AND the member roster. `members[]` is the set of agents you may assign work to — pm_create_task rejects any assignee outside it with error `assignee_not_project_member`, so read the roster here BEFORE dispatching. Each entry carries `instance_id` (the only value valid as `assignee`), `kind` (agent|user), and `agent_id` / `name` / `role` for display (may be null). If the agent you want is not in `members`, invite it with pm_add_project_member (project owner only). `doc_path` is where this project's deliverables go in the document library (design 32 §5.5): create it with doc_mkdir before the first write, and never re-derive the path from the title — a retitle does not move it.",
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
      "description": "Create a new project. Returns the created project with generated id, and `doc_path` — the document-library directory reserved for its deliverables (design 32 §5.5). Run doc_mkdir on `doc_path` right after creating the project so the deliverables have somewhere to land; the path is derived from the id and survives retitles.",
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
      "description": "The one task query. `project_id` given = inside that project; omitted = across every project, which then REQUIRES at least one narrowing filter (status / assignee / review_status / type / query / parent_task_id / only_roots / only_blocked) and caps `limit` at 50. `assignee` also accepts the sentinel \"me\" (resolved from your own MCP identity — you never need to know your instance id). Rows carry the parent link as `parent_id` (the create/update input is named `parent_task_id`). Returns an envelope, not an array: {total, offset, limit, returned, has_more, tasks[]} — `has_more` is how you know paging is left, since a bare limit hides everything past it. `review_status=pending` is the approval backlog (who is waiting on a human); `order_by`: created_at (default, newest first) | updated_at | due_at (soonest first) | priority (highest first).",
      "inputSchema": {
        "type": "object",
        "properties": {
          "project_id": { "type": "string", "pattern": "^p-[a-zA-Z0-9-]{1,62}$" },
          "status": {
            "type": "string",
            "enum": ["pending", "in_progress", "submitted", "done", "rejected", "cancelled"]
          },
          "assignee": { "type": "string", "description": "human literal, agent_instance_id (UUID v4, ADR-073 §1.2), or \"me\"" },
          "only_blocked": { "type": "boolean", "default": false },
          "review_status": {
            "type": "string",
            "enum": ["not_required", "pending", "approved", "rejected"],
            "description": "Approval gate state; \"pending\" = waiting on a human"
          },
          "type": {
            "type": "string",
            "enum": ["task", "bug", "feature", "chore", "checkpoint", "milestone"],
            "description": "Task kind filter — the same six values pm_create_task / pm_update_task accept (plan §7 G7). `epic` was never a TaskType variant and deserialized to a parse error; it is removed here rather than added to the store so no value is ever accepted in one call and refused in another."
          },
          "query": { "type": "string", "description": "Case-insensitive substring over the task title" },
          "parent_task_id": {
            "type": "string",
            "pattern": "^t-[a-zA-Z0-9-]{1,62}$",
            "description": "Direct children of this task. null means \"no filter\" here — use only_roots for the roots"
          },
          "only_roots": { "type": "boolean", "default": false },
          "order_by": { "type": "string", "enum": ["created_at", "updated_at", "due_at", "priority"] },
          "updated_since": {
            "type": "string",
            "format": "date-time",
            "description": "RFC3339 delta filter: only tasks whose updated_at is at or after this instant (\"what changed since my last read\"). Covers task-record rewrites (edits, status, submit/review, attachments); reparent moves and activity comments do not change updated_at and are NOT in this window. Composes with every other filter; an unparseable value returns bad_request."
          },
          "offset": { "type": "integer", "minimum": 0, "default": 0 },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    },
    {
      "name": "pm_get_task",
      "description": "Get task details including dependency status and depth. `is_blocked` / `blocked_by` are OMITTED when the task is not blocked — absence means unblocked, not unknown.",
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
      "description": "Create a task. Set parent_task_id to create a child task. Use depends_on to declare dependencies (other tasks must exist). Tasks created by an agent enter review_status=pending and CANNOT be claimed until a human approves them (pm_claim_task returns 409 review_not_approved); check `review_status` on `pm_get_task` to see whether approval landed.",
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
            "description": "agent_instance_id (UUID v4, ADR-073 §1.2) to assign, or \"me\" for yourself (resolved server-side; both gates below apply unchanged). TWO gates must both pass or the call is rejected: (1) the instance must exist in the Gateway agent directory — `assignee_not_found`; (2) it must already be a MEMBER of this project — `assignee_not_project_member`, enforced by the store. Read the roster with pm_get_project (members[].instance_id), discover installable agents with pm_list_agents, and invite with pm_add_project_member (project owner only). Never pass a package id like com.acowork.foo. Tasks created by an agent enter review_status=pending regardless: a human must approve them before pm_claim_task will move them to in_progress. Read `review_status` back with pm_get_task; the dispatch scanner (not your polling) wakes the assignee once start_at is reached."
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
      "name": "pm_update_task",
      "description": "Update task fields. Only provided fields are modified. Authorization (G1-c): the assignee may change any field; the project owner may change any field EXCEPT `status` — and is the only role that may set `assignee` on a task the assignee cannot act on (adoption of an unassigned or orphaned task); the task's creator may fix non-status fields (title / description / type / priority / parent_task_id / depends_on / due_at / start_at) but not `assignee` or `status`; anyone else gets `forbidden`. `status` follows the task state machine and is NOT free-form: in_progress can only go to submitted (never straight to done — submit first), cancelled is terminal, rejected can be re-claimed. A task created by an agent cannot enter in_progress until a human approves it (409 review_not_approved). Wrong transitions return `invalid_state_transition` with the legal target — read it instead of retrying.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "title": { "type": "string", "minLength": 1, "maxLength": 200 },
          "description": { "type": "string", "maxLength": 20000 },
          "type": {
            "type": "string",
            "enum": ["task", "bug", "feature", "chore", "checkpoint", "milestone"],
            "description": "Setting type=checkpoint or type=bug makes pm_submit_task require human acceptance instead of auto-completing."
          },
          "status": {
            "type": "string",
            "enum": ["pending", "in_progress", "submitted", "done", "rejected", "cancelled"]
          },
          "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"] },
          "assignee": {
            "type": ["string", "null"],
            "description": "agent_instance_id (UUID v4) or null to unassign. Changing it is a project-owner action (G1-c adoption of an unassigned or orphaned task); the assignee themselves may also hand the task over. The new assignee must exist in the agent directory (`assignee_not_found`) AND already be a member of the task's project (`assignee_not_project_member`) — adoption is not an inspection exemption. Check with pm_get_project (members[]), invite with pm_add_project_member. Do NOT pass the package id like com.acowork.foo."
          },
          "due_at": {
            "type": ["string", "null"],
            "format": "date-time",
            "description": "RFC3339 due timestamp, e.g. 2026-09-05T00:00:00Z. Null clears it."
          },
          "start_at": {
            "type": ["string", "null"],
            "format": "date-time",
            "description": "RFC3339 start timestamp. The PM dispatch scanner sends the wake-up intent to the assignee agent when start_at is reached (after human approval). Null clears it (dispatch as soon as the task is actionable)."
          },
          "depends_on": {
            "type": "array",
            "description": "Replaces the task's dependency list. Referenced tasks must already exist.",
            "items": {
              "type": "object",
              "required": ["task_id"],
              "properties": {
                "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
                "kind": { "type": "string", "enum": ["blocks", "relates", "duplicates"], "default": "blocks" }
              }
            }
          },
          "parent_task_id": {
            "type": ["string", "null"],
            "description": "Move the task in the tree: a task id re-parents it under that task, null promotes it to a project root, omitting the key leaves the position alone. Cycles are rejected (you cannot move a task under its own descendant)."
          }
        }
      }
    },
    {
      "name": "pm_claim_task",
      "description": "Claim a task as the current agent. Transitions pending → in_progress. For an ASSIGNED task only the assignee may claim it (403 otherwise). For an UNASSIGNED task any project member may claim it, and claiming sets the assignee to you atomically (403 if you are not a member). Returns 409 if the task was created by an agent and is still awaiting human approval (review_status=pending), or if blocked by dependencies. Actor is taken from MCP client identity.",
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
      "description": "Submit work results for a claimed task. Transitions in_progress → submitted. If task type is checkpoint/bug, enters pending review; otherwise auto-approved → done. Put everything the reviewer needs in `text`. Deliverables that live in the doc library go in `doc_refs` (doc id `doc-…` or library path `…/技术方案/x.md`) — the doc service validates each ref at submit time and an unresolved ref fails the submission (`doc_refs_unresolved`) rather than being dropped, so the task panel can always render them as working links. There is no file-upload path from an agent: `attachment_ids` is accepted by the API but only a human can upload through the Desktop task panel.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id", "text"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "text": { "type": "string", "minLength": 1, "maxLength": 20000 },
          "attachment_ids": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Optional attachment ids. There is no upload path from an agent (a human attaches files in the Desktop task panel), so leave this out and put what the reviewer needs in `text`."
          },
          "doc_refs": {
            "type": "array",
            "items": { "type": "string", "maxLength": 512 },
            "maxItems": 50,
            "description": "Doc-library artifacts this submission delivers: doc ids (`doc-…`) or library paths (`项目管理/p-…/技术方案/x.md`). Every ref is validated against the doc service BEFORE the transition — a missing or unreadable ref returns `doc_refs_unresolved` and the task stays in_progress. What is recorded (and readable back via pm_get_task) is the resolved canonical doc id, not your input string, so a later rename or move cannot rot the link; refs naming the same document collapse into one entry. Omit (default) and the submission behaves exactly as before."
          }
        }
      }
    },
    {
      "name": "pm_list_agents",
      "description": "Find the `instance_id` values that `assignee` accepts (ADR-073: a UUID instance id, never a package id like com.acowork.foo). TWO MODES. With `project_id`: returns that project's agent members — the set you may actually assign work to, because the store rejects every other assignee (`assignee_not_project_member`) and the dispatch scanner can only wake an installed agent instance. Omit `project_id`: returns every agent installed on this machine — the set to *invite*, which is wider than any one project's assignable set; use it before pm_add_project_member. Human accounts that belong to the project are NOT listed as assignable (they are reachable through pm_get_project's roster, and assigning them is a Desktop-side action); when any exist the result carries `human_member_count`. `online` is ADR-085 liveness as the Gateway reports it and is informational only: dispatching to an offline agent works, the Gateway starts the target when the intent is delivered. `null` means the directory did not report it, NOT that the agent is down. `name` is the instance's own name (e.g. `SSE.Kevin`) — what a human calls this agent, what the agent calls itself, and the ONLY field that distinguishes several installs of one package; `package_name` (e.g. `SSE`), `agent_id` and `role` are package-level and identical across them. `query` filters on instance name / package_name / role / agent_id, so it drops any row whose metadata is unavailable — omit it to get the complete set. Returns 503 `directory_unavailable` ONLY in the no-project mode when the directory has not finished its first refresh: that is NOT an empty roster, retry shortly instead of concluding there is nobody to dispatch to.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "project_id": {
            "type": "string",
            "pattern": "^p-[a-zA-Z0-9-]{1,62}$",
            "description": "Scope the result to one project's members (the assignable set). Omit to list the whole local directory (the invitable set)."
          },
          "query": {
            "type": "string",
            "description": "Case-insensitive substring over the instance name / package_name / role / agent_id. Use it on a large directory instead of paging; it drops rows without directory metadata."
          },
          "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
        }
      }
    },
    {
      "name": "pm_add_project_member",
      "description": "Invite an agent instance (or a human account) into a project, which is what makes it assignable: pm_create_task rejects any assignee outside the project roster with `assignee_not_project_member`. Adding a member widens the project, so it is a governance action — only the project owner may do it (you are the owner of a project you created). Re-inviting an existing member is not an error: the response carries `already_member: true`. The newcomer also joins the project group chat.",
      "inputSchema": {
        "type": "object",
        "required": ["project_id", "instance_id"],
        "properties": {
          "project_id": { "type": "string", "pattern": "^p-[a-zA-Z0-9-]{1,62}$" },
          "instance_id": {
            "type": "string",
            "description": "Agent instance UUID v4 (from pm_list_agents), or a user id when kind=user. NOT the package id."
          },
          "kind": {
            "type": "string",
            "enum": ["agent", "user"],
            "default": "agent",
            "description": "agent = must exist in the agent directory; user = a human account id, not looked up."
          }
        }
      }
    },
    {
      "name": "pm_add_activity",
      "description": "Append a note to a task's activity stream. This is the channel for context that must NOT overwrite the task: blockers, re-scheduling reasons, links to documents in the doc library. Editing `description` for the same purpose destroys the original acceptance criteria — comment instead. Allowed for the task's assignee, its creator, and the project owner (G1-c: the owner who adopts or reassigns a task can record why; the auto-generated `[reassign]` trail carries no reason).",
      "inputSchema": {
        "type": "object",
        "required": ["task_id", "text"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "text": { "type": "string", "minLength": 1, "maxLength": 20000 }
        }
      }
    },
    {
      "name": "pm_list_activity",
      "description": "Read a task's activity stream (submissions, reviews, comments), oldest first. pm_get_task only carries the LAST submission in `result`; the history lives here. Returns the most recent `limit` entries and sets `truncated: true` when older ones were left out — do not treat a truncated page as the whole history.",
      "inputSchema": {
        "type": "object",
        "required": ["task_id"],
        "properties": {
          "task_id": { "type": "string", "pattern": "^t-[a-zA-Z0-9-]{1,62}$" },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    },
    {
      "name": "pm_update_project",
      "description": "Rename, re-describe, archive or complete a project. Owner only — the same rule as the REST endpoint, so an agent cannot reshape a project it does not own. `status=completed` also ends every agent task context under the project (the agents stop holding that context); `archived` makes it read-only without ending them. At least one of title / description / status must be given: a call that changes nothing is rejected rather than answered with 200.",
      "inputSchema": {
        "type": "object",
        "required": ["project_id"],
        "properties": {
          "project_id": { "type": "string", "pattern": "^p-[a-zA-Z0-9-]{1,62}$" },
          "title": { "type": "string", "description": "New title." },
          "description": { "type": "string", "description": "New description (replaces the old one)." },
          "status": {
            "type": "string",
            "enum": ["active", "archived", "completed"],
            "description": "active = in progress; archived = read-only, no new tasks; completed = done, ends the agents' task contexts."
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
    use std::collections::BTreeSet;

    /// Plan §7 G7: the `type` enum must be **one set across the three
    /// surfaces** (create / update / list-filter) and equal to what the
    /// implementation can actually deserialize — `TaskType`'s serde wire
    /// values. The pre-G7 state: create declared `epic` (which is not a
    /// `TaskType` variant: the call died in `parse_args`) and omitted
    /// `chore`/`milestone` (which work); list/update carried the real six.
    /// A schema that promises a value the parser refuses is the D2 defect
    /// in the other direction — declared-but-unacceptable.
    #[test]
    fn type_enum_is_one_set_across_create_update_list() {
        use crate::types::TaskType;
        let wire = |t: TaskType| -> String {
            serde_json::to_value(t).unwrap().as_str().unwrap().to_string()
        };
        let impl_values: BTreeSet<String> = [
            TaskType::Task,
            TaskType::Bug,
            TaskType::Feature,
            TaskType::Chore,
            TaskType::Checkpoint,
            TaskType::Milestone,
        ]
        .into_iter()
        .map(wire)
        .collect();

        let v: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let type_enum_of = |tool: &str| -> BTreeSet<String> {
            let t = v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"].as_str() == Some(tool))
                .unwrap_or_else(|| panic!("tool {tool} missing from the manifest"));
            let e = t["inputSchema"]["properties"]["type"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{tool} declares no type enum"));
            e.iter()
                .map(|s| s.as_str().unwrap().to_string())
                .collect()
        };

        for tool in ["pm_create_task", "pm_update_task", "pm_list_tasks"] {
            let declared = type_enum_of(tool);
            assert_eq!(
                declared, impl_values,
                "{tool} type enum drifted from TaskType wire values"
            );
        }
        // The removed phantom: `epic` must not come back without a variant.
        for tool in ["pm_create_task", "pm_update_task", "pm_list_tasks"] {
            assert!(
                !type_enum_of(tool).contains("epic"),
                "{tool} declares `epic`, which TaskType cannot deserialize"
            );
        }
    }

    #[test]
    fn manifest_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 14);
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["inputSchema"].is_object());
        }
    }

    /// Regression lock for defect D2: **the field set the manifest declares
    /// must equal the field set the implementation accepts**.
    ///
    /// Both directions are asserted because both are real defects:
    /// - in the implementation, missing from the manifest → invisible to the
    ///   LLM, so the feature effectively does not exist (that is exactly how
    ///   `pm_update_task`'s `type` / `depends_on` drifted; the old spot-check
    ///   test let it through);
    /// - in the manifest, missing from the implementation → the caller sends
    ///   what the schema promises and the server drops it silently, which is
    ///   harder to diagnose than an error.
    ///
    /// See [`crate::mcp::tools::MANIFEST_FIELDS`]; a tool absent from the table
    /// is a tool left unguarded.
    #[test]
    fn manifest_properties_equal_impl_fields() {
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let tools = parsed["tools"].as_array().unwrap();

        for (name, fields) in crate::mcp::tools::MANIFEST_FIELDS {
            let tool = tools
                .iter()
                .find(|t| t["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("`{name}` is implemented but missing from the manifest"));
            let declared: BTreeSet<&str> = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            let implemented: BTreeSet<&str> = fields.iter().copied().collect();
            assert_eq!(
                declared, implemented,
                "`{name}`: manifest and implementation disagree (only-in-manifest: {:?}, only-in-impl: {:?})",
                declared
                    .difference(&implemented)
                    .copied()
                    .collect::<Vec<_>>(),
                implemented
                    .difference(&declared)
                    .copied()
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// **Every** tool served by `tools/list` must be on the reconciliation
    /// table. Without this assertion the table is opt-in, and opt-in is how two
    /// tools kept unguarded schemas for months: `pm_update_task` accepted
    /// `type` / `depends_on` invisibly, and `pm_submit_task` accepted
    /// `attachment_ids` invisibly (design 32 §7 D2). Adding a tool without a
    /// row now fails here instead of drifting quietly.
    #[test]
    fn manifest_fields_cover_every_tool() {
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let tools = parsed["tools"].as_array().unwrap();
        let covered: BTreeSet<&str> = crate::mcp::tools::MANIFEST_FIELDS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        let unguarded: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .filter(|name| !covered.contains(name))
            .collect();
        assert!(
            unguarded.is_empty(),
            "tools missing from MANIFEST_FIELDS (their schema is unreconciled): {unguarded:?}"
        );
        assert_eq!(
            covered.len(),
            tools.len(),
            "MANIFEST_FIELDS has {} rows for {} tools",
            covered.len(),
            tools.len()
        );
    }

    /// The table itself must not name a tool that does not exist (a typo would
    /// make the guard fail silently).
    #[test]
    fn manifest_fields_table_names_real_tools() {
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let names: Vec<&str> = parsed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for (name, _) in crate::mcp::tools::MANIFEST_FIELDS {
            assert!(names.contains(name), "unknown tool in MANIFEST_FIELDS: {name}");
        }
    }

    #[test]
    fn manifest_tools_matches_raw_manifest() {
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let raw = parsed["tools"].as_array().unwrap();
        assert_eq!(super::manifest_tools().len(), raw.len());
    }

    #[test]
    fn update_and_create_advertise_the_same_schedule_fields() {
        // The code accepts start_at / due_at on update; if the schema does
        // not advertise them, the model can never discover them (review
        // #4). Both tools must speak the same vocabulary.
        let parsed: serde_json::Value = serde_json::from_str(super::PM_TOOL_MANIFEST).unwrap();
        let props = |tool: &str| {
            parsed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == tool)
                .unwrap()["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<String>>()
        };
        let create = props("pm_create_task");
        let update = props("pm_update_task");
        // The schedule pair is the point of this test; the shared body
        // fields guard against a rename drifting one tool only.
        // (`status` is update-only by design: a fresh task starts pending.)
        for field in ["start_at", "due_at", "title", "description", "priority", "assignee"] {
            assert!(create.contains(field), "pm_create_task missing {field}");
            assert!(update.contains(field), "pm_update_task missing {field}");
        }
        assert!(!create.contains("status"));
        assert!(update.contains("status"));
    }
}
