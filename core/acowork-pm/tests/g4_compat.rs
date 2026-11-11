//! Plan §7 G4 — backward compatibility of `TaskResult::doc_refs`.
//!
//! `doc_refs` was added to the persisted `TaskResult` after the field had
//! already shipped in `task.json` files on disk. The whole compatibility
//! story rests on two properties, and this test is the only place they are
//! checked **as a round trip** (unit tests check each half separately):
//!
//! 1. a pre-G4 `task.json` (no `doc_refs` key) still deserializes —
//!    `serde(default)` on the field;
//! 2. once the store writes a record again, the field is present — so
//!    `Task`'s `deny_unknown_fields` never turns an old file into a
//!    permanently un-writable one.
//!
//! If either property breaks, old tasks on upgraded machines become
//! unreadable or un-updatable — the failure this test exists to catch.

use acowork_pm::types::{Task, TaskResult};

/// The exact JSON shape a pre-G4 `submit_task` wrote: `result` carries
/// `text` / `attachment_ids` / `submitted_by` / `submitted_at` — nothing
/// else. Written from the field list of the commit before G4, by hand, so
/// it cannot silently track the current struct.
const PRE_G4_TASK_JSON: &str = r#"{
    "id": "t-0f4c1a2b3d4e5f60718293a4b5c6d7e8",
    "project_id": "p-0f4c1a2b3d4e5f60718293a4b5c6d7e8",
    "title": "pre-G4 task",
    "description": "",
    "type": "task",
    "status": "submitted",
    "priority": "normal",
    "review_status": "not_required",
    "assignee": "422c3696-c762-4e19-b8fd-bdec5eb6b1bb",
    "created_by": "human",
    "created_at": "2026-11-01T00:00:00Z",
    "updated_at": "2026-11-02T00:00:00Z",
    "claimed_at": "2026-11-02T00:00:00Z",
    "submitted_at": "2026-11-02T01:00:00Z",
    "due_at": null,
    "start_at": null,
    "dispatched_at": null,
    "dispatch_message_id": null,
    "dispatch_attempts": 0,
    "depends_on": [],
    "attachments": [],
    "result": {
        "text": "delivered before doc_refs existed",
        "attachment_ids": [],
        "submitted_by": "422c3696-c762-4e19-b8fd-bdec5eb6b1bb",
        "submitted_at": "2026-11-02T01:00:00Z"
    }
}"#;

#[test]
fn pre_g4_task_json_deserializes_and_round_trips_with_doc_refs() {
    // 1. Old file reads: `doc_refs` defaults to empty.
    let mut task: Task = serde_json::from_str(PRE_G4_TASK_JSON)
        .expect("a pre-G4 task.json must stay readable after G4");
    let result = task
        .result
        .clone()
        .expect("result present");
    assert!(result.doc_refs.is_empty());

    // 2. Re-emitting the record includes the field, so the store's next
    //    write lands a file that still deserializes against
    //    `deny_unknown_fields` — and the value survives the trip.
    task.result = Some(TaskResult {
        doc_refs: vec!["项目管理/p-1/技术方案/x.md".to_string()],
        ..result
    });
    let written = serde_json::to_string(&task).unwrap();
    assert!(written.contains("doc_refs"), "the field must be emitted");
    let reread: Task = serde_json::from_str(&written)
        .expect("a post-G4 task.json must re-read against the same struct");
    assert_eq!(
        reread.result.as_ref().unwrap().doc_refs,
        vec!["项目管理/p-1/技术方案/x.md".to_string()],
    );
}
