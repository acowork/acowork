//! Bridge ADR-046 `AttachedItem::ImageUpload` items into the multimodal
//! `ContentPart` shape the LLM actually consumes.
//!
//! ## Why this lives here
//!
//! ADR-046 splits attachments into 5 variants in [`AttachedItem`]:
//! `file_upload`, `image_upload`, `attached_file`, `attached_selection`,
//! `attached_folder`. Each one is persisted to JSONL as a separate
//! `system` entry (handled by [`crate::agent::loop_persist`]'s
//! `write_attached_items`), but only `image_upload` payloads need to
//! flow into the LLM request as multimodal content.
//!
//! The desktop frontend previously inlined `content_parts` with base64
//! in `params_json`; that path was deleted in ADR-046. The runtime now
//! reads the `image_upload` items from `attached_items`, fetches the
//! raw bytes from the [`AttachmentService`] blob store, and synthesises
//! a `ContentPart::ImageUrl` carrying a `data:` URI — keeping the
//! "user text is never contaminated with base64" invariant from
//! ADR-046 §2.4 while still delivering the picture to the model.
//!
//! ## Pipeline
//!
//! 1. `safe_mime(format)` whitelists the format string into a known
//!    `image/<subtype>` MIME (PNG/JPG/GIF/WebP); anything else returns
//!    `None` and the item is **silently skipped** — we don't try to
//!    sniff bytes for MIME detection (ADR-046 §2.7: the runtime does
//!    no image recognition).
//! 2. `build_data_url(mime, bytes)` base64-encodes the bytes and glues
//!    them into a single `data:<mime>;base64,<…>` URI.
//! 3. `derive_image_parts_from_items(attachment, items)` iterates the
//!    items, drops non-image variants, reads each image's bytes, and
//!    produces a `Vec<ContentPart>` of `ImageUrl` parts (in attach
//!    order). Empty input ⇒ empty output (the caller decides whether
//!    that means "send nothing" or "prepend a Text part").
//!
//! [`AttachedItem`]: acowork_core::protocol::AttachedItem
//! [`AttachmentService`]: crate::usecases::AttachmentService

use std::sync::Arc;

use acowork_core::protocol::AttachedItem;
use acowork_core::providers::traits::{ContentPart, ImageUrlPart};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::usecases::AttachmentService;

/// Maximum single image size we will inline as a data URI (8 MiB).
///
/// Data URIs are sent in every LLM request that includes this image,
/// so the budget matters: beyond ~8 MiB the LLM API typically rejects
/// the request (OpenAI limits multimodal payloads to ~10 MiB total).
/// Items larger than this are **silently dropped** from the multimodal
/// pipeline — the JSONL system entry still records the upload (so the
/// agent sees the filename), but the model doesn't see the picture.
/// We log a warning so operators can notice disk-heavy uploads.
pub const MAX_INLINE_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Map a user-supplied `format` (lowercased extension — e.g. `"png"`,
/// `"jpeg"`, `"gif"`) to an `image/<subtype>` MIME. Whitelisted rather
/// than pass-through, mirroring [`crate::usecases::attachment_impl`]:
/// we never trust the raw `format` string to be a real image MIME.
///
/// Returns `None` for non-image formats (e.g. `pdf`, `bin`, `""`,
/// `exe`, `../../etc/passwd`). Callers should silently skip these —
/// they are valid `file_upload` items that just don't belong in the
/// multimodal channel.
pub fn safe_mime(format: &str) -> Option<&'static str> {
    match format.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Build a `data:<mime>;base64,<…>` URI for an image payload.
///
/// The MIME must come from [`safe_mime`] — passing an arbitrary string
/// here would defeat the whitelisting above. No length cap is enforced
/// at this level; callers that need a cap should pre-filter
/// (see [`MAX_INLINE_IMAGE_BYTES`]).
pub fn build_data_url(mime: &str, bytes: &[u8]) -> String {
    // Pre-allocate the encoded body so we get a single allocation for
    // the final String instead of the typical Vec→String copy.
    let encoded = BASE64_STANDARD.encode(bytes);
    // RFC 2397 data URL: `data:<media-type>;base64,<data>`. The fixed
    // tail length is the 7 ASCII chars of `;base64,` (1 + 6 + 1).
    let mut out = String::with_capacity(mime.len() + 7 + encoded.len());
    out.push_str("data:");
    out.push_str(mime);
    out.push_str(";base64,");
    out.push_str(&encoded);
    out
}

/// Construct a single `ContentPart::ImageUrl` from a data URL and the
/// optional width/height the desktop reported when uploading.
///
/// Width/height are hint metadata used for token estimation; we
/// preserve them when present so the token-counter can be more
/// accurate. They don't affect the multimodal model itself.
fn image_part(url: String, width: Option<u32>, height: Option<u32>) -> ContentPart {
    ContentPart::ImageUrl {
        image_url: ImageUrlPart {
            url,
            detail: None,
            width,
            height,
        },
    }
}

/// Walk `items`, pull each `ImageUpload` variant through
/// `attachment.read_file(document_id)`, and return a `Vec<ContentPart>`
/// of `ImageUrl` parts in the order the user attached them.
///
/// Behaviour:
/// - Non-image variants are ignored silently.
/// - Items whose `format` isn't a known image MIME are ignored
///   silently (after a debug-level log).
/// - Items whose blob can't be read return an error — the LLM call
///   will not proceed without all attachments accounted for.
/// - Items whose bytes exceed [`MAX_INLINE_IMAGE_BYTES`] are dropped
///   with a `warn!` so operators can investigate; the JSONL system
///   entry is still written so the agent sees the filename.
pub async fn derive_image_parts_from_items(
    attachment: &dyn AttachmentService,
    items: &[AttachedItem],
) -> Result<Vec<ContentPart>, crate::error::RuntimeError> {
    let mut parts = Vec::new();
    for item in items {
        let AttachedItem::ImageUpload {
            document_id,
            filename,
            format,
            size_bytes,
            width,
            height,
            ..
        } = item
        else {
            continue;
        };

        let Some(mime) = safe_mime(format) else {
            tracing::debug!(
                document_id = %document_id,
                filename = %filename,
                format = %format,
                "skipping non-image attachment in multimodal pipeline"
            );
            continue;
        };

        if *size_bytes as usize > MAX_INLINE_IMAGE_BYTES {
            tracing::warn!(
                document_id = %document_id,
                filename = %filename,
                size_bytes,
                limit = MAX_INLINE_IMAGE_BYTES,
                "image exceeds inline data-URI budget; dropping from multimodal payload"
            );
            continue;
        }

        let bytes = attachment.read_file(document_id).await.map_err(|e| {
            crate::error::RuntimeError::Config(format!(
                "read attachment {document_id} ({filename}): {e}"
            ))
        })?;

        let url = build_data_url(mime, &bytes);
        parts.push(image_part(url, *width, *height));
    }
    Ok(parts)
}

/// Late-bound convenience wrapper around
/// [`derive_image_parts_from_items`] for the [`AgentCore`] storage
/// shape (`Option<Arc<dyn AttachmentService>>`).
///
/// Behavioural table:
///
/// | `attachment_service` | `items`     | result                       |
/// |----------------------|-------------|------------------------------|
/// | `None`               | empty       | `Ok(vec![])`                 |
/// | `None`               | any         | `Ok(vec![])` — silently no-op; **a warning log is emitted** so the operator notices images were dropped without runtime knowledge of the blob store |
/// | `Some(svc)`          | empty       | `Ok(vec![])`                 |
/// | `Some(svc)`          | any images  | `Ok(parts)`                  |
/// | `Some(svc)`          | read fails  | `Err(RuntimeError)`          |
///
/// Returned errors **must** propagate — silently dropping an image the
/// user explicitly attached would be a worse failure mode than
/// rejecting the whole turn.
pub async fn derive_image_parts(
    attachment_service: Option<&Arc<dyn AttachmentService>>,
    items: &[AttachedItem],
) -> Result<Vec<ContentPart>, crate::error::RuntimeError> {
    let Some(svc) = attachment_service else {
        if !items.is_empty() {
            tracing::warn!(
                attached_count = items.len(),
                "Chat message carried attached items but AgentCore.attachment_service is None — \
                 runtime cannot derive multimodal image parts. Check Phase B injection."
            );
        }
        return Ok(Vec::new());
    };
    derive_image_parts_from_items(svc.as_ref(), items).await
}

/// Merge pre-existing `content_parts` (whatever the frontend opted to
/// inline directly) with image parts derived from `attached_items`.
///
/// Order: text parts first, image parts after. Returns `None` if both
/// sources are empty — in that case the caller should fall back to
/// [`ChatMessage::user(text)`]; a multimodal message with no parts
/// is rejected by some providers.
///
/// When the caller has already supplied content parts via the
/// pre-ADR-046 `params.content_parts` channel, those parts are
/// preserved verbatim and the derived image parts are appended after
/// them — we never re-derive text, only images.
pub fn merge_content_parts(
    frontend_parts: Option<Vec<ContentPart>>,
    derived_images: Vec<ContentPart>,
) -> Option<Vec<ContentPart>> {
    let mut all: Vec<ContentPart> = frontend_parts.unwrap_or_default();
    all.extend(derived_images);
    if all.is_empty() { None } else { Some(all) }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::usecases::attachment::{AttachmentError, UploadFileParams, UploadedFileResponse};

    /// In-memory fake of [`AttachmentService`].
    ///
    /// We don't exercise `upload_file` here — only `read_file`, which is
    /// what the multimodal pipeline calls. The trait's other method is
    /// stubbed to `unimplemented!()` so any accidental call is loud.
    struct MockAttachment {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MockAttachment {
        fn new() -> Self {
            Self {
                blobs: Mutex::new(HashMap::new()),
            }
        }

        fn with_blob(document_id: &str, bytes: Vec<u8>) -> Self {
            let mut blobs = HashMap::new();
            blobs.insert(document_id.to_string(), bytes);
            Self {
                blobs: Mutex::new(blobs),
            }
        }
    }

    #[async_trait::async_trait]
    impl AttachmentService for MockAttachment {
        async fn upload_file(
            &self,
            _params: UploadFileParams,
        ) -> Result<UploadedFileResponse, AttachmentError> {
            unimplemented!("not exercised by these tests")
        }

        async fn read_file(&self, document_id: &str) -> Result<Vec<u8>, AttachmentError> {
            self.blobs
                .lock()
                .unwrap()
                .get(document_id)
                .cloned()
                .ok_or_else(|| AttachmentError::NotFound(document_id.to_string()))
        }
    }

    fn png_item(document_id: &str, format: &str, size_bytes: u64) -> AttachedItem {
        AttachedItem::ImageUpload {
            document_id: document_id.to_string(),
            filename: format!("snap.{format}"),
            format: format.to_string(),
            size_bytes,
            width: Some(640),
            height: Some(480),
            client_id: None,
        }
    }

    #[test]
    fn safe_mime_whitelists_images_only() {
        // Image formats go through.
        for (input, expected) in [
            ("png", "image/png"),
            ("PNG", "image/png"),
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("JPEG", "image/jpeg"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
        ] {
            assert_eq!(safe_mime(input), Some(expected), "format={input:?}");
        }
        // Non-image formats return None — we never fabricate an
        // image MIME for arbitrary input.
        for input in [
            "pdf",
            "docx",
            "exe",
            "html",
            "",
            "image/jpeg",
            "../png",
            ".",
        ] {
            assert_eq!(
                safe_mime(input),
                None,
                "non-image format {input:?} must not be inlined"
            );
        }
    }

    #[test]
    fn build_data_url_round_trips() {
        let bytes = b"\x89PNG\r\n\x1a\nsome-bytes";
        let url = build_data_url("image/png", bytes);
        assert!(url.starts_with("data:image/png;base64,"));
        let payload = &url["data:image/png;base64,".len()..];
        assert_eq!(BASE64_STANDARD.decode(payload).unwrap(), bytes);
    }

    #[test]
    fn build_data_url_is_single_allocation_safe() {
        // Sanity: pre-allocation does not produce truncated output.
        let url = build_data_url("image/jpeg", &[0u8; 1024]);
        assert_eq!(url.len(), "data:image/jpeg;base64,".len() + 1368);
        // 1024 bytes → ceil(1024/3)*4 = 1368 base64 chars.
    }

    /// Empty input → empty output. Caller decides whether to send
    /// nothing or to fall back to plain text.
    #[tokio::test]
    async fn derive_image_parts_from_empty_items_returns_empty_vec() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let parts = derive_image_parts_from_items(&*svc, &[]).await.unwrap();
        assert!(parts.is_empty());
    }

    /// Only image items survive; file_upload / attached_file /
    /// attached_selection / attached_folder are silently dropped.
    #[tokio::test]
    async fn derive_image_parts_filters_non_image_variants() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::with_blob(
            "img-1",
            b"\x89PNG\r\n\x1a\nfake-png".to_vec(),
        ));
        let items = vec![
            AttachedItem::FileUpload {
                document_id: "doc-1".into(),
                filename: "report.pdf".into(),
                format: "pdf".into(),
                size_bytes: 4096,
                client_id: None,
            },
            png_item("img-1", "png", 16),
            AttachedItem::AttachedFile {
                abs_path: "/workspace/foo.rs".into(),
                name: "foo.rs".into(),
                client_id: None,
            },
            AttachedItem::AttachedSelection {
                abs_path: "/workspace/foo.rs".into(),
                name: "foo.rs".into(),
                start_line: 1,
                end_line: 5,
                client_id: None,
            },
            AttachedItem::AttachedFolder {
                abs_path: "/workspace/src".into(),
                name: "src".into(),
                client_id: None,
            },
        ];
        let parts = derive_image_parts_from_items(&*svc, &items).await.unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
                assert_eq!(image_url.width, Some(640));
                assert_eq!(image_url.height, Some(480));
            }
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    /// ImageUpload with a non-image `format` (e.g. `pdf`) is silently
    /// dropped — the JSONL system entry is still written so the agent
    /// sees the filename, but the bytes never reach the multimodal
    /// payload.
    #[tokio::test]
    async fn derive_image_parts_skips_non_image_format_silently() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![png_item("img-1", "pdf", 8)];
        let parts = derive_image_parts_from_items(&*svc, &items).await.unwrap();
        assert!(parts.is_empty());
    }

    /// If the underlying blob store can't find the document_id, the
    /// pipeline bubbles the error up — partial image lists are not
    /// silently sent to the LLM.
    #[tokio::test]
    async fn derive_image_parts_propagates_read_errors() {
        // No blob pre-loaded for "missing".
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![png_item("missing", "png", 8)];
        let err = derive_image_parts_from_items(&*svc, &items)
            .await
            .unwrap_err();
        // Wrapped under RuntimeError::Config per current helper impl.
        let msg = err.to_string();
        assert!(
            msg.contains("missing"),
            "error must mention the failing document_id, got: {msg}"
        );
    }

    /// Multiple images produce multiple parts in the attach order the
    /// frontend sent them.
    #[tokio::test]
    async fn derive_image_parts_preserves_attach_order() {
        let concrete = Arc::new(MockAttachment::with_blob(
            "img-1",
            b"\x89PNG\r\n\x1a\nA".to_vec(),
        ));
        // Add a second blob to the same fake. We need the concrete
        // Arc to reach the inner `Mutex<HashMap>` field, then expose
        // the assembled fake as a trait-object reference.
        concrete
            .blobs
            .lock()
            .unwrap()
            .insert("img-2".to_string(), b"\xff\xd8\xffB".to_vec());
        let svc: Arc<dyn AttachmentService> = concrete;

        let items = vec![
            png_item("img-1", "png", 9),
            AttachedItem::ImageUpload {
                document_id: "img-2".into(),
                filename: "snap.jpg".into(),
                format: "jpg".into(),
                size_bytes: 4,
                width: None,
                height: None,
                client_id: None,
            },
        ];
        let parts = derive_image_parts_from_items(&*svc, &items).await.unwrap();
        assert_eq!(parts.len(), 2);
        match &parts[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
                assert_eq!(image_url.width, Some(640));
            }
            _ => panic!("expected ImageUrl"),
        }
        match &parts[1] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/jpeg;base64,"));
                assert_eq!(image_url.width, None);
            }
            _ => panic!("expected ImageUrl"),
        }
    }

    /// Images over [`MAX_INLINE_IMAGE_BYTES`] are silently dropped with a
    /// warn log. JSONL system entry is still written (handled by
    /// `loop_persist::write_attached_items` upstream), so the agent sees
    /// the filename but the LLM doesn't see the picture.
    #[tokio::test]
    async fn derive_image_parts_drops_oversized_images() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::with_blob(
            "huge",
            vec![0u8; MAX_INLINE_IMAGE_BYTES + 1],
        ));
        let items = vec![AttachedItem::ImageUpload {
            document_id: "huge".into(),
            filename: "huge.png".into(),
            format: "png".into(),
            size_bytes: (MAX_INLINE_IMAGE_BYTES as u64) + 1,
            width: None,
            height: None,
            client_id: None,
        }];
        let parts = derive_image_parts_from_items(&*svc, &items).await.unwrap();
        assert!(parts.is_empty());
    }

    /// `derive_image_parts` with `None` svc + empty items → empty
    /// output, no log noise expected.
    #[tokio::test]
    async fn derive_image_parts_none_svc_empty_items() {
        let parts = derive_image_parts(None, &[]).await.unwrap();
        assert!(parts.is_empty());
    }

    /// `derive_image_parts` with `None` svc + non-empty items → empty
    /// output **and** the warn log must fire so operators notice
    /// (smoke-tested via `tracing-test` would be heavyweight; we just
    /// verify the no-panic, no-leak behaviour here).
    #[tokio::test]
    async fn derive_image_parts_none_svc_with_items_logs_warning_silently() {
        let items = vec![png_item("img-1", "png", 9)];
        let parts = derive_image_parts(None, &items).await.unwrap();
        assert!(parts.is_empty());
    }

    /// `derive_image_parts` with `Some(svc)` + empty items → empty
    /// output.
    #[tokio::test]
    async fn derive_image_parts_some_svc_empty_items() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let parts = derive_image_parts(Some(&svc), &[]).await.unwrap();
        assert!(parts.is_empty());
    }

    /// `derive_image_parts` end-to-end: Some(svc) + an image item →
    /// produces the correct data URL.
    #[tokio::test]
    async fn derive_image_parts_some_svc_with_image() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::with_blob(
            "img-1",
            b"\x89PNG\r\n\x1a\nfake".to_vec(),
        ));
        let items = vec![png_item("img-1", "png", 8)];
        let parts = derive_image_parts(Some(&svc), &items).await.unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
            }
            _ => panic!("expected ImageUrl"),
        }
    }

    /// `merge_content_parts`: no inputs on either side ⇒ `None`. The
    /// caller treats this as "plain ChatMessage::user(text)".
    #[test]
    fn merge_content_parts_both_empty_returns_none() {
        let merged = merge_content_parts(None, Vec::new());
        assert!(merged.is_none());
    }

    /// `merge_content_parts`: only derived images ⇒ `Some(images)`.
    #[test]
    fn merge_content_parts_only_images_returns_some() {
        let images = vec![ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                width: None,
                height: None,
            },
        }];
        let merged = merge_content_parts(None, images.clone()).unwrap();
        assert_eq!(merged.len(), 1);
    }

    /// `merge_content_parts`: text first, then images. Verifies the
    /// LLM sees the user's text **before** the picture (ordering
    /// matters for some vision models).
    #[test]
    fn merge_content_parts_text_first_then_images() {
        let frontend = vec![ContentPart::Text {
            text: "see this image:".into(),
        }];
        let images = vec![ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                width: None,
                height: None,
            },
        }];
        let merged = merge_content_parts(Some(frontend), images).unwrap();
        assert_eq!(merged.len(), 2);
        assert!(matches!(&merged[0], ContentPart::Text { text } if text == "see this image:"));
        assert!(matches!(&merged[1], ContentPart::ImageUrl { .. }));
    }
}
