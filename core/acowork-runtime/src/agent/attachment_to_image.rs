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
//! 1. [`model_accepts_image`] answers whether the target model takes image
//!    input at all. When it does not, no part is produced and every image is
//!    recorded as `Skipped(NoVisionCapability)` **before** the blob is read:
//!    the request-side filter strips `ImageUrl` for exactly the same reason
//!    (`context.rs` §7.6), so producing the part would only put base64 on the
//!    wire that the model never receives.
//! 2. `safe_mime(format)` whitelists the format string into a known
//!    `image/<subtype>` MIME (PNG/JPG/GIF/WebP); anything else produces no
//!    part and the item is recorded as `Skipped(FormatNotReadable)` — we don't
//!    try to sniff bytes for MIME detection (ADR-046 §2.7: the runtime does
//!    no image recognition).
//! 3. `build_data_url(mime, bytes)` base64-encodes the bytes and glues
//!    them into a single `data:<mime>;base64,<…>` URI.
//! 4. `derive_image_parts_from_items(attachment, items, caps)` iterates the
//!    items, drops non-image variants, reads each image's bytes, and
//!    produces an [`ImageParts`] — the `ImageUrl` parts in attach order plus
//!    one [`ImageDelivery`] verdict per item.
//!
//! Step 4's verdicts exist for the prompt hint: "did the pixels reach the
//! model" is a fact only this module knows, and the hint is the model's only
//! account of it. A hint that guessed used to announce `pixels were attached`
//! for images dropped here, and to invite `image_read` for images already in
//! the message. `Delivered` therefore means "the pixels are in `parts` **and**
//! the model accepts image input" — the two halves the hint has to promise.
//!
//! [`AttachedItem`]: acowork_core::protocol::AttachedItem
//! [`AttachmentService`]: crate::usecases::AttachmentService

use std::sync::Arc;

use acowork_core::protocol::{AttachedItem, ModelCapabilitiesInfo};
use acowork_core::providers::traits::{ContentPart, ImageUrlPart};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::usecases::AttachmentService;

/// Does this model take an image as model input?
///
/// The **single** answer to that question: the request-side filter that strips
/// `ContentPart::ImageUrl` for text-only models (`context.rs` §7.6), the
/// `supports_image_input` flag `image_read` gates itself on, and this module's
/// delivery verdict all call it, so a hint can never promise pixels the
/// request will drop.
///
/// Unknown capability answers **yes**, the opposite of
/// [`crate::agent::attachment_to_document::model_accepts_pdf`], and for the
/// same underlying reason: a document part on an endpoint that cannot take one
/// is a hard 400 that costs the whole turn, while a model that cannot see an
/// image still answers the text. Sending is the recoverable failure here.
/// `modalities: None` (catalogued model, no modality metadata) is therefore
/// "go ahead", exactly as the §7.6 filter has always read it.
pub fn model_accepts_image(caps: Option<&ModelCapabilitiesInfo>) -> bool {
    caps.and_then(|c| c.modalities.as_ref())
        .map(|m| m.input.iter().any(|s| s.eq_ignore_ascii_case("image")))
        .unwrap_or(true)
}

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

/// What became of one attached image on its way to the model.
///
/// The derivation used to answer only "which parts did I produce", which left
/// the prompt hint guessing: it announced `pixels were attached` for every
/// `image_upload`, including the ones this module had just dropped. The hint is
/// the model's only account of what it can see, so the derivation now reports
/// the verdict per item and the hint states that verdict instead of the
/// attachment list. `Skipped` is not an internal detail — it is the fact that
/// turns a silent degradation into an instruction the model can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageDelivery {
    /// The pixels are in this turn's `content_parts` **and** the target model
    /// accepts image input ([`model_accepts_image`]). Both halves are part of
    /// the claim: a part the request filter later strips never reaches the
    /// model, so calling it delivered would be the same lie in a new place.
    Delivered,
    /// The pixels are not in the request, for this reason.
    Skipped(ImageSkip),
}

/// Why an `image_upload` item produced no part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSkip {
    /// The model does not take image input at all ([`model_accepts_image`]
    /// said no) — `context.rs` §7.6 would strip the part from the request, so
    /// it is never built. On this path the file is still on disk and
    /// `image_read` still answers with its own "switch to a vision model"
    /// error, which is the action a human can take.
    NoVisionCapability,
    /// `format` is not on the provider whitelist (svg, pdf, an unknown
    /// extension). Not "this is not an image" — an svg *is* one — but "not a
    /// format the model's image input can carry", which is why the hint names
    /// converting it as the fix.
    FormatNotReadable,
    /// Bigger than [`MAX_INLINE_IMAGE_BYTES`] — the data URI would be resent on
    /// every request that carries this message.
    TooLarge,
    /// The runtime has no [`AttachmentService`] (Phase B injection missing), so
    /// no blob could be read at all.
    NoBlobStore,
}

/// The two things the derivation owes its caller: the parts to send, and one
/// verdict per `image_upload` item keyed by `document_id`.
///
/// The field is private on purpose. A caller that can push a part, or set a
/// verdict, on its own can build a pair that contradicts itself — a "delivered"
/// image with no part, or a part the hint never accounts for — and the whole
/// reason this type exists is that the prompt hint trusts it. Parts leave only
/// through [`ImageParts::take_parts`], which keeps what is left behind honest.
#[derive(Debug, Default)]
pub struct ImageParts {
    parts: Vec<ContentPart>,
    verdicts: std::collections::HashMap<String, ImageDelivery>,
}

impl ImageParts {
    /// The parts this derivation produced, in attach order.
    pub fn parts(&self) -> &[ContentPart] {
        &self.parts
    }
    /// Verdict for one `image_upload` item, or `None` when this turn never
    /// offered that item to the derivation (a reference merged in later, for
    /// instance) — the caller must then say "not inlined", not guess.
    pub fn verdict(&self, document_id: &str) -> Option<&ImageDelivery> {
        self.verdicts.get(document_id)
    }

    /// Move the parts out, leaving the verdicts behind.
    ///
    /// The caller puts the parts into the message *before* it builds the hint,
    /// and the hint is exactly what still needs the verdicts — so the two have
    /// to be separable, and named as such. Reaching for `parts` directly and
    /// `std::mem::take`ing it leaves a struct that describes verdicts for parts
    /// it no longer holds; this method says that move out loud.
    pub fn take_parts(&mut self) -> Vec<ContentPart> {
        std::mem::take(&mut self.parts)
    }

    /// Record the fate of one item. Private: `ImageParts` is only meaningful
    /// when its parts and its verdicts are produced together, and a caller
    /// that can set either half alone can build the lying pair.
    fn record(&mut self, document_id: &str, verdict: ImageDelivery) {
        self.verdicts.insert(document_id.to_string(), verdict);
    }

    /// A verdict-only instance, for tests that hand
    /// `build_attachment_hint` a map without standing up an
    /// [`AttachmentService`].
    #[cfg(test)]
    pub(crate) fn for_test(document_id: &str, verdict: ImageDelivery) -> Self {
        let mut out = Self::default();
        out.record(document_id, verdict);
        out
    }
}

/// Walk `items`, pull each `ImageUpload` variant through
/// `attachment.read_file(document_id)`, and return the `ImageUrl` parts in the
/// order the user attached them, plus a verdict per item.
///
/// Behaviour:
/// - Non-image variants are ignored (they are not on this channel).
/// - When `caps` says the model takes no image input, every image is recorded
///   as [`ImageSkip::NoVisionCapability`] and **the blob is never read**: the
///   part would be stripped from the request anyway (§7.6), and base64 for an
///   image nobody can see is pure cost. This check runs before the format and
///   size checks because it is the one fact the other two cannot fix.
/// - Items whose `format` isn't a known image MIME are recorded as
///   [`ImageSkip::FormatNotReadable`] and announced to the caller.
/// - Items whose bytes exceed [`MAX_INLINE_IMAGE_BYTES`] are recorded as
///   [`ImageSkip::TooLarge`]; the JSONL system entry is still written, so the
///   prompt hint can point the model at the on-disk file.
/// - Items whose blob can't be read return an error — the LLM call will not
///   proceed without all attachments accounted for.
pub async fn derive_image_parts_from_items(
    attachment: &dyn AttachmentService,
    items: &[AttachedItem],
    caps: Option<&ModelCapabilitiesInfo>,
) -> Result<ImageParts, crate::error::RuntimeError> {
    let accepts_image = model_accepts_image(caps);
    let mut out = ImageParts::default();
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

        if !accepts_image {
            tracing::debug!(
                document_id = %document_id,
                filename = %filename,
                "model takes no image input; not inlining attached image"
            );
            out.record(
                document_id,
                ImageDelivery::Skipped(ImageSkip::NoVisionCapability),
            );
            continue;
        }

        let Some(mime) = safe_mime(format) else {
            tracing::debug!(
                document_id = %document_id,
                filename = %filename,
                format = %format,
                "skipping non-image attachment in multimodal pipeline"
            );
            out.record(
                document_id,
                ImageDelivery::Skipped(ImageSkip::FormatNotReadable),
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
            out.record(document_id, ImageDelivery::Skipped(ImageSkip::TooLarge));
            continue;
        }

        let bytes = attachment.read_file(document_id).await.map_err(|e| {
            crate::error::RuntimeError::Config(format!(
                "read attachment {document_id} ({filename}): {e}"
            ))
        })?;

        let url = build_data_url(mime, &bytes);
        out.parts.push(image_part(url, *width, *height));
        out.record(document_id, ImageDelivery::Delivered);
    }
    Ok(out)
}

/// Late-bound convenience wrapper around
/// [`derive_image_parts_from_items`] for the [`AgentCore`] storage
/// shape (`Option<Arc<dyn AttachmentService>>`).
///
/// Behavioural table:
///
/// | `attachment_service` | `items`     | result                       |
/// |----------------------|-------------|------------------------------|
/// | `None`               | empty       | no parts, no verdicts        |
/// | `None`               | any         | no parts; every `image_upload` is `Skipped(NoBlobStore)` and **a warning log is emitted** — the caller still learns the pixels are not in the request, which is the difference between a degraded turn and a hallucinated one |
/// | `Some(svc)`          | empty       | no parts, no verdicts        |
/// | `Some(svc)`          | any images  | `Ok(parts)` + one verdict per item |
/// | `Some(svc)`          | read fails  | `Err(RuntimeError)`          |
///
/// `caps` is the target model's capability set and is forwarded unchanged;
/// when the model takes no image input every image is `Skipped(
/// NoVisionCapability)`, which outranks the `NoBlobStore` verdict of the
/// `None`-service branch (the model is the outer gate — a readable blob still
/// would not reach it).
///
/// Returned errors **must** propagate — silently dropping an image the
/// user explicitly attached would be a worse failure mode than
/// rejecting the whole turn.
pub async fn derive_image_parts(
    attachment_service: Option<&Arc<dyn AttachmentService>>,
    items: &[AttachedItem],
    caps: Option<&ModelCapabilitiesInfo>,
) -> Result<ImageParts, crate::error::RuntimeError> {
    let accepts_image = model_accepts_image(caps);
    let Some(svc) = attachment_service else {
        let mut out = ImageParts::default();
        if !items.is_empty() {
            tracing::warn!(
                attached_count = items.len(),
                "Chat message carried attached items but AgentCore.attachment_service is None — \
                 runtime cannot derive multimodal image parts. Check Phase B injection."
            );
        }
        for item in items {
            if let AttachedItem::ImageUpload { document_id, .. } = item {
                let skip = if accepts_image {
                    ImageSkip::NoBlobStore
                } else {
                    ImageSkip::NoVisionCapability
                };
                out.record(document_id, ImageDelivery::Skipped(skip));
            }
        }
        return Ok(out);
    };
    derive_image_parts_from_items(svc.as_ref(), items, caps).await
}

/// Assemble one turn's part list: the caller's own parts first, then whole
/// documents, then images.
///
/// The order is a reading order, not a technical necessity — every provider
/// takes the list as it comes. Text first because it is what the human typed;
/// documents before images because a document is "this file" while an image
/// is "these pictures", and the large object reads more naturally first when
/// both arrived in the same message.
///
/// `None` when everything is empty: a multimodal message with no parts is
/// rejected by some providers, and a plain `ChatMessage::user(text)` is what
/// the caller falls back to.
///
/// This is the **only** assembly rule for a turn's parts. It lives beside the
/// two derivations that feed it (`attachment_to_image`,
/// `attachment_to_document`) rather than in the caller, so a test can pin the
/// real merge instead of a look-alike — which is how the e2e path
/// (`tests/mqtt_e2e.rs`) exercises it.
pub fn merge_turn_parts(
    frontend_parts: Option<Vec<ContentPart>>,
    documents: Vec<ContentPart>,
    images: Vec<ContentPart>,
) -> Option<Vec<ContentPart>> {
    let mut all: Vec<ContentPart> = frontend_parts.unwrap_or_default();
    if all.is_empty() && documents.is_empty() && images.is_empty() {
        return None;
    }
    all.extend(documents);
    all.extend(images);
    Some(all)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::usecases::attachment::{AttachmentError, UploadFileParams, UploadedFileResponse};

    /// Capabilities carrying exactly these input modalities.
    fn caps_with_input(inputs: &[&str]) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window: 200_000,
            max_output_tokens: 8_192,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: Some(acowork_core::protocol::ModelModalities {
                input: inputs.iter().map(|s| s.to_string()).collect(),
                output: vec!["text".to_string()],
            }),
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
            reasoning_efforts: None,
        }
    }

    /// A vision model — what every test that is not about capability assumes.
    fn vision() -> Option<ModelCapabilitiesInfo> {
        Some(caps_with_input(&["text", "image"]))
    }

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
        let derived = derive_image_parts_from_items(&*svc, &[], vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
        assert!(derived.verdict("img-1").is_none(), "no item, no verdict");
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
        let derived = derive_image_parts_from_items(&*svc, &items, vision().as_ref()).await.unwrap();
        assert_eq!(derived.parts().len(), 1);
        match &derived.parts()[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
                assert_eq!(image_url.width, Some(640));
                assert_eq!(image_url.height, Some(480));
            }
            other => panic!("expected ImageUrl, got {other:?}"),
        }
        // Only the `image_upload` item is this channel's business, so only it
        // gets a verdict: the hint must not claim anything about a file_upload
        // or a workspace reference.
        assert_eq!(derived.verdict("img-1"), Some(&ImageDelivery::Delivered));
        assert!(derived.verdict("doc-1").is_none());
    }

    /// ImageUpload with a non-image `format` (e.g. `pdf`) produces no part.
    /// The verdict is what makes that visible: the hint can then tell the model
    /// the pixels are not coming instead of announcing that they are attached.
    #[tokio::test]
    async fn derive_image_parts_skips_non_image_format_with_a_verdict() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![png_item("img-1", "pdf", 8)];
        let derived = derive_image_parts_from_items(&*svc, &items, vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
        assert_eq!(
            derived.verdict("img-1"),
            Some(&ImageDelivery::Skipped(ImageSkip::FormatNotReadable))
        );
    }

    /// The capability gate reads the catalog the way the request-side filter
    /// does: an explicit `image` modal means yes, an explicit list without it
    /// means no, and *unknown* means yes.
    #[test]
    fn model_accepts_image_follows_the_catalog_and_defaults_to_yes() {
        assert!(model_accepts_image(Some(&caps_with_input(&["text", "image"]))));
        assert!(model_accepts_image(Some(&caps_with_input(&["TEXT", "IMAGE"]))));
        assert!(!model_accepts_image(Some(&caps_with_input(&["text", "pdf"]))));
        // No capability info at all / no modalities block → don't filter —
        // the same reading as `context.rs` §7.6, because the two call sites
        // must not disagree.
        assert!(model_accepts_image(None));
        let mut caps = caps_with_input(&["text"]);
        caps.modalities = None;
        assert!(model_accepts_image(Some(&caps)));
    }

    /// A model that takes no image input gets no part, and **the blob is never
    /// read**: the mock has no blob for `img-1`, so a read would have failed
    /// the call. The verdict has to be `NoVisionCapability`, not `Delivered` —
    /// §7.6 strips the part from the request, so `Delivered` would put the
    /// hint's "pixels are part of this message" back to lying.
    #[tokio::test]
    async fn derive_image_parts_skips_images_when_the_model_has_no_vision() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![png_item("img-1", "png", 8)];
        let caps = caps_with_input(&["text"]);
        let derived = derive_image_parts_from_items(&*svc, &items, Some(&caps))
            .await
            .expect("no blob read should happen, so no read error either");
        assert!(derived.parts().is_empty());
        assert_eq!(
            derived.verdict("img-1"),
            Some(&ImageDelivery::Skipped(ImageSkip::NoVisionCapability))
        );
    }

    /// The vision gate outranks the other skip reasons: on a text-only model an
    /// unreadable format changes nothing, and saying "convert it to png" would
    /// send the model after a file it still could not be shown.
    #[tokio::test]
    async fn no_vision_outranks_format_and_size() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let caps = caps_with_input(&["text"]);
        let items = vec![
            png_item("img-svg", "svg", 8),
            png_item("img-big", "png", MAX_INLINE_IMAGE_BYTES as u64 + 1),
        ];
        let derived = derive_image_parts_from_items(&*svc, &items, Some(&caps))
            .await
            .unwrap();
        assert!(derived.parts().is_empty());
        for id in ["img-svg", "img-big"] {
            assert_eq!(
                derived.verdict(id),
                Some(&ImageDelivery::Skipped(ImageSkip::NoVisionCapability)),
                "{id}"
            );
        }
    }

    /// Missing blob store + no vision: the model is the outer gate, so that is
    /// the reason the hint states.
    #[tokio::test]
    async fn no_vision_outranks_the_missing_blob_store() {
        let caps = caps_with_input(&["text"]);
        let items = vec![png_item("img-1", "png", 9)];
        let derived = derive_image_parts(None, &items, Some(&caps)).await.unwrap();
        assert_eq!(
            derived.verdict("img-1"),
            Some(&ImageDelivery::Skipped(ImageSkip::NoVisionCapability))
        );
    }

    /// If the underlying blob store can't find the document_id, the
    /// pipeline bubbles the error up — partial image lists are not
    /// silently sent to the LLM.
    #[tokio::test]
    async fn derive_image_parts_propagates_read_errors() {
        // No blob pre-loaded for "missing".
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![png_item("missing", "png", 8)];
        let err = derive_image_parts_from_items(&*svc, &items, vision().as_ref())
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
        let derived = derive_image_parts_from_items(&*svc, &items, vision().as_ref()).await.unwrap();
        assert_eq!(derived.parts().len(), 2);
        match &derived.parts()[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
                assert_eq!(image_url.width, Some(640));
            }
            _ => panic!("expected ImageUrl"),
        }
        match &derived.parts()[1] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/jpeg;base64,"));
                assert_eq!(image_url.width, None);
            }
            _ => panic!("expected ImageUrl"),
        }
        assert_eq!(derived.verdict("img-1"), Some(&ImageDelivery::Delivered));
        assert_eq!(derived.verdict("img-2"), Some(&ImageDelivery::Delivered));
    }

    /// Images over [`MAX_INLINE_IMAGE_BYTES`] are dropped with a warn log, and the
    /// verdict says why — that is the whole point: the model gets an actionable
    /// line (`too large to inline, open it with image_read`) instead of a false
    /// "pixels were attached". The JSONL system entry is still written (handled
    /// by `loop_persist::write_attached_items` upstream), so the on-disk path
    /// the hint quotes exists.
    #[tokio::test]
    async fn derive_image_parts_drops_oversized_images_with_a_verdict() {
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
        let derived = derive_image_parts_from_items(&*svc, &items, vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
        assert_eq!(
            derived.verdict("huge"),
            Some(&ImageDelivery::Skipped(ImageSkip::TooLarge))
        );
    }

    /// `derive_image_parts` with `None` svc + empty items → no parts and no
    /// verdicts, no log noise expected.
    #[tokio::test]
    async fn derive_image_parts_none_svc_empty_items() {
        let derived = derive_image_parts(None, &[], vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
        assert!(derived.verdict("img-1").is_none());
    }

    /// `derive_image_parts` with `None` svc + non-empty items → no parts, the
    /// warn log fires so operators notice, **and** every image carries a verdict
    /// — a missing blob store is exactly the silent-failure case the hint has to
    /// state out loud.
    #[tokio::test]
    async fn derive_image_parts_none_svc_with_items_reports_no_blob_store() {
        let items = vec![png_item("img-1", "png", 9)];
        let derived = derive_image_parts(None, &items, vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
        assert_eq!(
            derived.verdict("img-1"),
            Some(&ImageDelivery::Skipped(ImageSkip::NoBlobStore))
        );
    }

    /// `derive_image_parts` with `Some(svc)` + empty items → no parts.
    #[tokio::test]
    async fn derive_image_parts_some_svc_empty_items() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let derived = derive_image_parts(Some(&svc), &[], vision().as_ref()).await.unwrap();
        assert!(derived.parts().is_empty());
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
        let derived = derive_image_parts(Some(&svc), &items, vision().as_ref()).await.unwrap();
        assert_eq!(derived.parts().len(), 1);
        match &derived.parts()[0] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
            }
            _ => panic!("expected ImageUrl"),
        }
        assert_eq!(derived.verdict("img-1"), Some(&ImageDelivery::Delivered));
    }

    /// `merge_turn_parts`: nothing from any source ⇒ `None`. The caller treats
    /// this as "plain `ChatMessage::user(text)`", because some providers reject
    /// a multimodal message carrying no parts.
    #[test]
    fn merge_turn_parts_both_empty_returns_none() {
        let merged = merge_turn_parts(None, Vec::new(), Vec::new());
        assert!(merged.is_none());
    }

    /// `merge_turn_parts`: only derived images ⇒ `Some(images)`.
    #[test]
    fn merge_turn_parts_only_images_returns_some() {
        let images = vec![ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                width: None,
                height: None,
            },
        }];
        let merged = merge_turn_parts(None, Vec::new(), images).unwrap();
        assert_eq!(merged.len(), 1);
    }

    /// `merge_turn_parts`: text first, then documents, then images. The order
    /// is a reading order — the human's words before the objects they point at,
    /// and the large object (a document) before the pictures.
    #[test]
    fn merge_turn_parts_reads_text_then_documents_then_images() {
        let frontend = vec![ContentPart::Text {
            text: "see this image:".into(),
        }];
        let documents = vec![ContentPart::document("JVBERi0=", "report.pdf", Some(1))];
        let images = vec![ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                width: None,
                height: None,
            },
        }];
        let merged = merge_turn_parts(Some(frontend), documents, images).unwrap();
        assert_eq!(merged.len(), 3);
        assert!(matches!(&merged[0], ContentPart::Text { text } if text == "see this image:"));
        assert!(
            matches!(&merged[1], ContentPart::Document { .. }),
            "the document is the larger object and reads first"
        );
        assert!(matches!(&merged[2], ContentPart::ImageUrl { .. }));
    }
}
