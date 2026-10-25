//! Conversation-JSONL side effects for AgentLoop.
//!
//! These used to live in `loop_memory.rs` (ADR-014 Phase 6) only because the
//! file was the catch-all for "things the turn writes outside `history`".
//! ADR-101 moves memory out of the Runtime, but both helpers below are
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
}

