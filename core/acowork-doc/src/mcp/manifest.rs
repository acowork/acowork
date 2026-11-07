//! MCP 工具 manifest（JSON 格式，对应 `/mcp/tools/list` 响应）。
//!
//! ## 设计原则：文件系统语义，而不是「用例动词」
//!
//! 文档库本质是一个文件系统。工具面因此按**资源 + 标准动作**定义，而不是
//! 为每个用例新造一个动词。治理（是否需要人工审批）是**正交维度**，不改变
//! 动词：同一个 `doc_write`，策略命中审批时返回 `status: "pending_review"`。
//!
//! ## 工具清单
//!
//! | 工具 | 等价 POSIX 操作 | 鉴权 |
//! |------|----------------|------|
//! | `doc_list` | `ls`（含 query 过滤 + 分页） | 只读 |
//! | `doc_read` | `read`（全文 + 元数据） | 只读 |
//! | `doc_stat` | `stat`（仅元数据，不读正文） | 只读 |
//! | `doc_write` | `open(O_CREAT\|O_TRUNC)` / `>>` / `O_EXCL` + `write` | 写 |
//! | `doc_move` | `mv`（含 rename；temp+rename = 原子发布） | 写（需所有权；无主文档按 ACL） |
//! | `doc_remove` | `unlink`（进回收站，可恢复） | 写（需所有权；无主文档按 ACL） |
//! | `doc_mkdir` | `mkdir -p` | 写 |
//! | `doc_search` | `grep -r` | 只读 |
//! | `doc_check_request` | 查询 `pending_review` 的审核结果 | 只读 |
//!
//! 已删除的旧动词（设计 §6 的 add/submit_update/pull 三件套）：
//! - `doc_add` → `doc_write(mode="create")`
//! - `doc_submit_update` → `doc_write(mode="overwrite", if_version=N)`（审批由
//!   策略决定，不由动词决定）
//! - `doc_pull` → `doc_stat`（取 version 不再需要传回全文）
//!
//! ## 寻址约定
//!
//! 文档参数接受 **doc_id**（`doc-{hex}`）或 **path**（`项目A/纪要.md`，UTF-8
//! 相对路径，缺省根）；目录参数接受 path（`项目A/子组`，空串 = 根）。服务端
//! 把 path 解析为内部 `dir_id` + 文件名（库内同级唯一，见
//! `mcp::tools::resolve`）。
//!
//! ## 大文档写入（为什么 write 必须支持 append）
//!
//! 单次工具调用的载荷受调用方（LLM 单次响应长度）限制，与服务端上限无
//! 关。因此"一次写完一个大文档"在调用方侧不可行。正确用法是文件系统的
//! 经典 temp+rename：
//!
//! 1. `doc_write(ref="报告.md.tmp.md", mode="create", content=第1块)`
//! 2. `doc_write(ref="报告.md.tmp.md", mode="append", content=第2..N块)`
//! 3. `doc_move(from="报告.md.tmp.md", to="报告.md", overwrite=true)` ← 原子发布
//!
//! 这样读者永远看不到半成品，且不需要任何 draft/staging 状态。

/// MCP 工具 manifest（编译时常量，便于 LLM 直接 prompt-include）。
pub const DOC_TOOL_MANIFEST: &str = r#"{
  "schema_version": "2025-06-18",
  "tools": [
    {
      "name": "doc_list",
      "description": "List documents and sub-directories under a directory path (root when omitted). Returns id, name, kind and version for each entry.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {
            "type": "string",
            "description": "Directory path relative to the library root, e.g. \"研发/文档库\". Empty = root.",
            "default": ""
          },
          "query": {
            "type": "string",
            "description": "Optional case-insensitive substring filter on entry names.",
            "default": ""
          },
          "offset": { "type": "integer", "minimum": 0, "default": 0 },
          "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 }
        }
      }
    },
    {
      "name": "doc_read",
      "description": "Read a shared document's full Markdown content with its version and metadata. Accepts a doc_id or a path like \"研发/纪要.md\".",
      "inputSchema": {
        "type": "object",
        "required": ["ref"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          }
        }
      }
    },
    {
      "name": "doc_stat",
      "description": "Return a document's metadata WITHOUT its body: version, size_bytes, creator, timestamps and path. Use this to obtain the version for an optimistic `if_version` guard instead of reading the whole document.",
      "inputSchema": {
        "type": "object",
        "required": ["ref"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          }
        }
      }
    },
    {
      "name": "doc_write",
      "description": "Write a document's content. One tool for create / overwrite / append; whether the write takes effect immediately or becomes a human review request is decided by policy, not by this tool's name. Returns status \"applied\" with the new version, or status \"pending_review\" with a request_id to poll via doc_check_request. For content larger than one call: write chunk 1 with mode=\"create\" to a temporary name, append the rest with mode=\"append\", then publish atomically with doc_move.",
      "inputSchema": {
        "type": "object",
        "required": ["ref", "content"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "Target document: doc_id (doc-xxxx) or relative path ending in .md. For mode=\"create\" a path is required (it supplies the directory and the file name)."
          },
          "content": {
            "type": "string",
            "description": "Markdown content to write. For mode=\"append\" it is appended verbatim to the existing body."
          },
          "mode": {
            "type": "string",
            "enum": ["overwrite", "create", "append"],
            "default": "overwrite",
            "description": "overwrite = create if absent, replace if present (like `>`); create = fail if the name is taken (like O_EXCL); append = append to an existing document (like `>>`)."
          },
          "if_version": {
            "type": "integer",
            "minimum": 0,
            "description": "Optimistic concurrency guard: fail with a version conflict unless the document is still at this version. Omit to write the current version."
          },
          "source_workspace": {
            "type": "string",
            "description": "Create-only provenance: workspace the content came from (recorded on the new document)."
          },
          "source_path": {
            "type": "string",
            "description": "Create-only provenance: original file path the content came from (recorded on the new document)."
          }
        }
      }
    },
    {
      "name": "doc_move",
      "description": "Move and/or rename a document. Atomic publish: write to a temporary name in the same directory, then doc_move it onto the final name with overwrite=true. Requires ownership of the source document (and of the target document when overwrite=true replaces an existing one); a document with no recorded creator is shared and only the node ACL applies. A replaced document is moved to the recycle bin, never unlinked.",
      "inputSchema": {
        "type": "object",
        "required": ["from", "to"],
        "properties": {
          "from": {
            "type": "string",
            "description": "Source document: doc_id (doc-xxxx) or relative path ending in .md."
          },
          "to": {
            "type": "string",
            "description": "Destination path ending in .md, e.g. \"研发/纪要.md\". Its directory must already exist."
          },
          "overwrite": {
            "type": "boolean",
            "default": false,
            "description": "Replace the destination document when the name is already taken."
          }
        }
      }
    },
    {
      "name": "doc_remove",
      "description": "Delete a document into the recycle bin (recoverable, not purged). Requires ownership of the document; a document with no recorded creator is shared and only the node ACL applies.",
      "inputSchema": {
        "type": "object",
        "required": ["ref"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          }
        }
      }
    },
    {
      "name": "doc_mkdir",
      "description": "Create a sub-directory. The parent must already exist; create levels one at a time.",
      "inputSchema": {
        "type": "object",
        "required": ["path"],
        "properties": {
          "path": {
            "type": "string",
            "description": "Full relative path of the new directory, e.g. \"研发/文档库\"."
          }
        }
      }
    },
    {
      "name": "doc_search",
      "description": "Cross-directory keyword search over document titles and bodies. Title matches rank above content matches.",
      "inputSchema": {
        "type": "object",
        "required": ["keyword"],
        "properties": {
          "keyword": { "type": "string", "minLength": 1 },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    },
    {
      "name": "doc_check_request",
      "description": "Poll the review status of a write that returned status \"pending_review\": pending | approved | rejected | expired. Returns the review note when reviewed.",
      "inputSchema": {
        "type": "object",
        "required": ["request_id"],
        "properties": {
          "request_id": {
            "type": "string",
            "pattern": "^r-[a-f0-9]{12}$",
            "description": "Request id returned by doc_write."
          }
        }
      }
    },
    {
      "name": "doc_list_requests",
      "description": "List review requests — the answer to \"what did I submit that is still waiting?\". doc_check_request needs a request_id, and that id only exists in the write response; if your context was compacted the pending change would otherwise be unreachable and unrevertible. Defaults to the pending queue. Use this at the start of a session to find changes waiting on a human, and before re-submitting work you are unsure about.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "status": {
            "type": "string",
            "enum": ["pending", "approved", "rejected", "expired", "all"],
            "default": "pending",
            "description": "Review state to list. `all` drops the filter."
          },
          "reference": {
            "type": "string",
            "description": "Restrict to one document: a doc_id (doc-xxxx) or a library path ending in .md."
          },
          "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
        }
      }
    },
    {
      "name": "doc_list_trash",
      "description": "List deleted documents that can still be restored, each with the trash_id needed by doc_restore. doc_remove only moves a document to the trash — use this and doc_restore to undo a deletion instead of rewriting the file.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
        }
      }
    },
    {
      "name": "doc_restore",
      "description": "Restore a document from the trash back into the directory it was deleted from. Undo for doc_remove. Needs the same edit right the deletion needed. There is no purge tool: an agent can undo a deletion, never make one irreversible.",
      "inputSchema": {
        "type": "object",
        "required": ["trash_id"],
        "properties": {
          "trash_id": {
            "type": "string",
            "pattern": "^tr-[a-f0-9]{12}$",
            "description": "Trash slot id from doc_list_trash."
          }
        }
      }
    },
    {
      "name": "doc_rmdir",
      "description": "Remove an EMPTY directory. Refuses when the directory still holds documents or sub-directories, and never cascades: deleting a whole subtree by naming one path is how an information architecture disappears by accident. Empty it first, bottom-up.",
      "inputSchema": {
        "type": "object",
        "required": ["path"],
        "properties": {
          "path": {
            "type": "string",
            "description": "Relative directory path, e.g. \"engineering/scratch\". The library root cannot be removed."
          }
        }
      }
    }
  ]
}"#;

/// 解析 manifest 中的工具数组（`tools/list` 响应体）。
pub fn manifest_tools() -> Vec<serde_json::Value> {
    let v: serde_json::Value =
        serde_json::from_str(DOC_TOOL_MANIFEST).expect("DOC_TOOL_MANIFEST must be valid JSON");
    v["tools"].as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_to_expected_tool_count() {
        let tools = manifest_tools();
        assert_eq!(tools.len(), 13, "9 filesystem/review tools + 4 P2 additions (requests, trash, restore, rmdir)");
        for t in &tools {
            assert!(t["name"].as_str().unwrap().starts_with("doc_"));
            assert!(t["description"].as_str().unwrap().len() > 10);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    /// The whole point of the redesign: governance must not appear in a tool
    /// name, and the "each use case gets its own verb" trio must be gone.
    #[test]
    fn governance_is_not_encoded_in_tool_names() {
        let names: Vec<String> = manifest_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for banned in [
            "doc_add",
            "doc_submit_update",
            "doc_pull",
            "doc_append",
            "doc_review",
        ] {
            assert!(
                !names.contains(&banned.to_string()),
                "`{banned}` encodes a use case or a gate in the verb; use doc_write + policy instead (tools: {names:?})"
            );
        }
        for required in ["doc_write", "doc_stat", "doc_move", "doc_remove"] {
            assert!(
                names.contains(&required.to_string()),
                "missing filesystem primitive `{required}` (tools: {names:?})"
            );
        }
    }
}