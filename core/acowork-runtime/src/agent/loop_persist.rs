//! Conversation-JSONL side effects for AgentLoop.
//!
//! These used to live in `loop_memory.rs` (ADR-014 Phase 6) only because the
//! file was the catch-all for "things the turn writes outside `history`".
//! ADR-100 moves memory out of the Runtime, but both helpers below are
//! **session history**, not memory: they persist attachment records and the
//! intent-rotation handoff note so the transcript/UI can show them. They stay
//! here, memory-free.
//!
//! Contains:
//! - Attachment record persistence (ADR-046)
//! - Intent-rotation handoff note persistence (design 27 §2.1 / §4.1)

impl super::loop_::AgentLoop {
    /// Persist attached items as standalone system entries in the
    /// conversation JSONL (ADR-046).
    pub fn write_attached_items(&self, items: &[acowork_core::protocol::AttachedItem]) {
        let Some(ref conversation) = self.session.conversation else {
            return;
        };
        for item in items {
            let (content, metadata, client_id) = match item {
                acowork_core::protocol::AttachedItem::FileUpload {
                    document_id,
                    filename,
                    format,
                    size_bytes,
                    client_id,
                    ..
                } => {
                    let meta = crate::conversation::FileUploadMeta {
                        document_id: document_id.clone(),
                        filename: filename.clone(),
                        format: format.clone(),
                        size_bytes: *size_bytes,
                    };
                    let content = format!(
                        "Uploaded file: {} ({}, {} bytes)",
                        filename, format, size_bytes
                    );
                    let metadata =
                        serde_json::to_value(crate::conversation::AttachmentMeta::FileUpload(meta))
                            .expect("FileUploadMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::ImageUpload {
                    document_id,
                    filename,
                    format,
                    size_bytes,
                    width,
                    height,
                    client_id,
                    ..
                } => {
                    let meta = crate::conversation::ImageUploadMeta {
                        document_id: document_id.clone(),
                        filename: filename.clone(),
                        format: format.clone(),
                        size_bytes: *size_bytes,
                        width: *width,
                        height: *height,
                    };
                    let content = format!(
                        "Uploaded image: {} ({}, {} bytes)",
                        filename, format, size_bytes
                    );
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::ImageUpload(meta),
                    )
                    .expect("ImageUploadMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedFile {
                    abs_path,
                    name,
                    client_id,
                    ..
                } => {
                    let meta = crate::conversation::AttachedFileMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                    };
                    let content = format!("Attached file: {}", name);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedFile(meta),
                    )
                    .expect("AttachedFileMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedSelection {
                    abs_path,
                    name,
                    start_line,
                    end_line,
                    client_id,
                    ..
                } => {
                    let meta = crate::conversation::AttachedSelectionMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                        start_line: *start_line,
                        end_line: *end_line,
                    };
                    let content = format!(
                        "Attached selection: {} (L{}-L{})",
                        name, start_line, end_line
                    );
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedSelection(meta),
                    )
                    .expect("AttachedSelectionMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedFolder {
                    abs_path,
                    name,
                    client_id,
                    ..
                } => {
                    let meta = crate::conversation::AttachedFolderMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                    };
                    let content = format!("Attached folder: {}", name);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedFolder(meta),
                    )
                    .expect("AttachedFolderMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
            };
            conversation.append_message_with_id("system", &content, metadata, client_id);
        }
    }

    /// Persist an intent-rotation handoff note (design 27 §2.1 / §4.1) as a
    /// standalone `system` entry in the conversation JSONL — the
    /// **transcript/UI** copy only.
    ///
    /// The note is stored as its own `system` JSONL row so the execute-session
    /// shows a quiet centered notice of the rotation. The row is keyed
    /// `handoff:{message_id}`: the conversation writer appends unconditionally
    /// (it does *not* dedup by id), so a stable id lets an at-least-once intent
    /// redelivery collapse to one visible notice when the frontend dedups
    /// polled messages by id. It deliberately does NOT append to
    /// `session.history`: this row is the durable copy. The model sees the
    /// note two ways — folded into the `user_message` for the live turn (the
    /// caller passes `format!("{note}\n\n{content}")` to `run()` with a clean
    /// `raw_user_message`, so the auto-title + persisted `user` row stay the
    /// user's verbatim text, ADR-046), and — after a cold restart — by the
    /// restorer replaying this `rotation_handoff` row as a User message (a
    /// plain `System` row would be stripped by `ContextBuilder::build`).
    pub(crate) fn record_intent_handoff(&self, note: &str, message_id: &str) {
        if let Some(ref conversation) = self.session.conversation {
            conversation.append_message_with_id(
                "system",
                note,
                Some(serde_json::json!({ "rotation_handoff": true })),
                Some(format!("handoff:{message_id}")),
            );
        }
    }
}
