//! Bridge ADR-046 `AttachedItem::FileUpload` PDFs into the
//! `ContentPart::Document` shape a document-capable model reads natively.
//!
//! ## Why this lives next to [`crate::agent::attachment_to_image`]
//!
//! A PDF the user attached is already on this machine's disk and already
//! announced to the model by `build_attachment_hint`, so `doc_reader` can open
//! it on *any* model. Inlining the bytes is therefore not about capability but
//! about fidelity: text extraction throws the page away. A model that takes
//! document input gets the layout, the tables and the figures as their author
//! left them — which is the whole reason the desktop preview offers "send this
//! document" in the first place.
//!
//! ## Why the gate is stricter than the image gate
//!
//! [`attachment_to_image`] sends images when capability info is missing: a
//! text-only model answers "I cannot see this" and the turn still completes.
//! A `document` block on an endpoint that does not accept one is a hard HTTP
//! 400 that loses the entire turn, and the alternative path (hint +
//! `doc_reader`) was always available and loses nothing. So the rule here is
//! **inline only when the Gateway told us the model declares `pdf` input**.
//! Unknown capability means no inline, deliberately: coverage we do not need
//! bought with a failure mode we cannot recover from.
//!
//! ## What this module never touches
//!
//! The hint. Stripping it when the bytes are inlined would make the degraded
//! paths (over budget, unsupported model) lose the file *as well*, so the two
//! channels are one-directional by design: this module can add a payload,
//! never remove the pointer to the one on disk.
//!
//! ## Pipeline
//!
//! 1. [`model_accepts_pdf`] — the capability gate, once per turn.
//! 2. Keep `FileUpload` items whose `format` is `pdf`; everything else is a
//!    valid attachment that simply has no document block, so it is skipped in
//!    silence (the same treatment the image pipeline gives non-images).
//! 3. [`over_byte_budget`] on the *declared* size, before any disk read.
//! 4. Read the blob. A failure here propagates: something the user attached
//!    explicitly must not vanish without a word (see the image pipeline, which
//!    makes the same choice).
//! 5. Count the pages and encode, off the event loop, because a 20 MiB PDF is
//!    both a parse and a base64 inflation.
//! 6. [`within_page_budget`] on the count, then emit one
//!    [`ContentPart::Document`].
//!
//! [`attachment_to_image`]: crate::agent::attachment_to_image

use std::sync::Arc;

use acowork_core::protocol::{AttachedItem, ModelCapabilitiesInfo};
use acowork_core::providers::traits::ContentPart;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::usecases::AttachmentService;

/// Largest PDF we inline as a document part (20 MiB).
///
/// Under the 50 MiB an upload may carry at all, and under Anthropic's own
/// 32 MB document ceiling — base64 inflates the payload by about a third, so
/// 20 MiB of PDF leaves ~27 MiB on the wire and still fits.
pub const MAX_INLINE_DOCUMENT_BYTES: usize = 20 * 1024 * 1024;

/// Largest page count we inline as a document part.
///
/// Anthropic caps one document at 100 pages; 60 keeps two PDFs in one turn
/// inside that limit and holds the token estimate (see
/// [`crate::token::counter`], 1500 per page) to roughly 90k of context rather
/// than silently eating a 200k window.
pub const MAX_INLINE_DOCUMENT_PAGES: u32 = 60;

/// Does this model take a PDF as document input?
///
/// Two absences both answer `no`, and unlike the image path they are not
/// treated as "probably fine": no capability info at all, and capability info
/// that says nothing about modalities, each mean we cannot promise the request
/// will be accepted. The modalities list is matched on the literal `pdf`
/// because that is what the Gateway's catalog carries; a catalogue that spells
/// the same capability differently is answered by `doc_reader`, not by a 400.
pub fn model_accepts_pdf(caps: Option<&ModelCapabilitiesInfo>) -> bool {
    caps.and_then(|c| c.modalities.as_ref())
        .is_some_and(|m| m.input.iter().any(|s| s.eq_ignore_ascii_case("pdf")))
}

/// Over the byte budget? Checked on the size the client declared *before* the
/// blob is read, and again on the bytes that actually came back — the declared
/// number is a hint, the file is the fact.
pub fn over_byte_budget(size_bytes: usize) -> bool {
    size_bytes > MAX_INLINE_DOCUMENT_BYTES
}

/// May a PDF with this page count be inlined?
///
/// `None` means the page tree could not be read, which is a **no**: an
/// unverifiable count is an unverifiable budget. `lopdf` is stricter than the
/// viewers people use, so this does reject the occasional PDF a model could
/// have read — and that PDF is still on disk, still named in the hint, and
/// still readable by `doc_reader`, which is why choosing the certain path
/// costs nothing.
pub fn within_page_budget(pages: Option<u32>) -> bool {
    matches!(pages, Some(p) if p <= MAX_INLINE_DOCUMENT_PAGES)
}

/// Page count of a PDF blob, or `None` when its structure cannot be parsed.
///
/// Page tree only — no text extraction — because the count is what decides the
/// budget; anything heavier would be paid for on every attachment.
pub fn pdf_page_count(bytes: &[u8]) -> Option<u32> {
    let doc = lopdf::Document::load_mem(bytes).ok()?;
    Some(doc.get_pages().len() as u32)
}

/// Count pages and base64 the blob in one hand-off to the blocking pool.
///
/// Returns `None` when the page count cannot be read (which also covers a
/// panic inside the parser: a PDF that takes the parse down with it is not a
/// PDF we can size). Encoding is deliberately *after* the count, so an
/// oversized document costs no 27 MiB string.
async fn scan_and_encode(bytes: Vec<u8>) -> Option<(u32, String)> {
    tokio::task::spawn_blocking(move || {
        let pages = pdf_page_count(&bytes)?;
        Some((pages, BASE64_STANDARD.encode(&bytes)))
    })
    .await
    .ok()
    .flatten()
}

/// Walk `items` and return one [`ContentPart::Document`] per inlinable PDF, in
/// attach order. See the module docs for the pipeline and the gate.
///
/// Behaviour mirrors [`derive_image_parts_from_items`], with two differences:
/// the capability gate is required rather than best-effort, and exceeding
/// either budget skips the document with a `warn!` instead of failing the turn
/// — the file stays on disk and stays in the hint, so the agent can still read
/// it.
///
/// [`derive_image_parts_from_items`]: crate::agent::attachment_to_image::derive_image_parts_from_items
pub async fn derive_document_parts_from_items(
    attachment: &dyn AttachmentService,
    items: &[AttachedItem],
    gateway_capabilities: Option<&ModelCapabilitiesInfo>,
) -> Result<Vec<ContentPart>, crate::error::RuntimeError> {
    if !model_accepts_pdf(gateway_capabilities) {
        // Not a warning: on a model without document input this is the
        // designed outcome, and the hint path is how the agent reads the file.
        return Ok(Vec::new());
    }

    let mut parts = Vec::new();
    for item in items {
        let AttachedItem::FileUpload {
            document_id,
            filename,
            format,
            size_bytes,
            ..
        } = item
        else {
            continue;
        };

        if !format.eq_ignore_ascii_case("pdf") {
            // docx / pptx / xlsx have no cross-provider document block and
            // stay on `doc_reader` (see the format note in `build_attachment_hint`).
            continue;
        }

        if over_byte_budget(*size_bytes as usize) {
            tracing::warn!(
                document_id = %document_id,
                filename = %filename,
                size_bytes,
                limit = MAX_INLINE_DOCUMENT_BYTES,
                "PDF exceeds the inline document budget; the agent reads it through doc_reader instead"
            );
            continue;
        }

        let bytes = attachment.read_file(document_id).await.map_err(|e| {
            crate::error::RuntimeError::Config(format!(
                "read attachment {document_id} ({filename}): {e}"
            ))
        })?;

        if over_byte_budget(bytes.len()) {
            tracing::warn!(
                document_id = %document_id,
                filename = %filename,
                size_bytes = bytes.len(),
                limit = MAX_INLINE_DOCUMENT_BYTES,
                "PDF is larger on disk than the upload declared; skipping the inline document part"
            );
            continue;
        }

        let (pages, data) = match scan_and_encode(bytes).await {
            Some(pair) => pair,
            None => {
                tracing::warn!(
                    document_id = %document_id,
                    filename = %filename,
                    "PDF page count unreadable; skipping the inline document part"
                );
                continue;
            }
        };

        if !within_page_budget(Some(pages)) {
            tracing::warn!(
                document_id = %document_id,
                filename = %filename,
                pages,
                limit = MAX_INLINE_DOCUMENT_PAGES,
                "PDF has too many pages to inline; the agent reads it through doc_reader instead"
            );
            continue;
        }

        parts.push(ContentPart::document(data, filename.clone(), Some(pages)));
    }
    Ok(parts)
}

/// Convenience wrapper for the [`AgentCore`] storage shape.
///
/// A missing `attachment_service` means the runtime cannot reach the blob at
/// all, so nothing is inlined — and unlike the image path this needs no warn
/// about lost content, because the document is still in the workspace and
/// still named in the hint.
pub async fn derive_document_parts(
    attachment_service: Option<&Arc<dyn AttachmentService>>,
    items: &[AttachedItem],
    gateway_capabilities: Option<&ModelCapabilitiesInfo>,
) -> Result<Vec<ContentPart>, crate::error::RuntimeError> {
    let Some(svc) = attachment_service else {
        return Ok(Vec::new());
    };
    derive_document_parts_from_items(svc.as_ref(), items, gateway_capabilities).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use acowork_core::protocol::ModelModalities;
    use acowork_core::providers::traits::DocumentPart;
    use crate::usecases::attachment::{AttachmentError, UploadFileParams, UploadedFileResponse};

    /// In-memory fake of [`AttachmentService`], same shape as the one in
    /// `attachment_to_image::tests`.
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

    fn pdf_item(document_id: &str, filename: &str, format: &str, size_bytes: u64) -> AttachedItem {
        AttachedItem::FileUpload {
            document_id: document_id.to_string(),
            filename: filename.to_string(),
            format: format.to_string(),
            size_bytes,
            client_id: None,
        }
    }

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
            modalities: Some(ModelModalities {
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

    /// A parseable PDF with `page_count` pages, xref offsets computed from the
    /// assembled body so `lopdf` accepts it. Written by hand rather than built
    /// with `lopdf`: the fixture has to be as independent as possible from the
    /// library the code under test uses to read it.
    fn make_pdf(page_count: u32) -> Vec<u8> {
        // Object layout: 1 Catalog, 2 Pages, then per page a Page and its
        // content stream.
        let page_ids: Vec<usize> = (0..page_count as usize).map(|i| 3 + i * 2).collect();
        let kids = page_ids
            .iter()
            .map(|id| format!("{id} 0 R"))
            .collect::<Vec<_>>()
            .join(" ");
        let stream = "BT /F1 12 Tf 72 720 Td (page) Tj ET";
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", page_count),
        ];
        for _ in 0..page_count {
            let contents_id = objects.len() + 2;
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {contents_id} 0 R >>"
            ));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }

        let mut body = Vec::new();
        body.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objects.len());
        for (idx, obj) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
            body.extend_from_slice(obj.as_bytes());
            body.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = body.len();
        let size = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
        body.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            // One xref entry is exactly 20 bytes: 10 digits, space, 5 digits,
            // space, the in-use flag and a two-character line ending.
            body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        body.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        body
    }

    fn document_of(part: &ContentPart) -> &DocumentPart {
        match part {
            ContentPart::Document { document } => document,
            other => panic!("expected a document part, got {other:?}"),
        }
    }

    // ── Pure rules ───────────────────────────────────────────────────

    /// The gate is the capability, spelled `pdf`, and nothing else.
    #[test]
    fn model_accepts_pdf_requires_a_declared_pdf_modality() {
        assert!(model_accepts_pdf(Some(&caps_with_input(&["text", "image", "pdf"]))));
        // Case-insensitive: catalogs disagree about spelling, not about meaning.
        assert!(model_accepts_pdf(Some(&caps_with_input(&["PDF"]))));
        assert!(!model_accepts_pdf(Some(&caps_with_input(&["text", "image"]))));
        assert!(!model_accepts_pdf(Some(&caps_with_input(&[]))));
    }

    /// The two absences the module doc argues for: no caps at all, and caps
    /// that carry no modality list.
    #[test]
    fn model_accepts_pdf_is_no_when_capability_is_unknown() {
        assert!(!model_accepts_pdf(None));
        let mut caps = caps_with_input(&["pdf"]);
        caps.modalities = None;
        assert!(!model_accepts_pdf(Some(&caps)));
    }

    #[test]
    fn byte_budget_fires_on_the_boundary_and_only_just_past_it() {
        assert!(!over_byte_budget(MAX_INLINE_DOCUMENT_BYTES));
        assert!(over_byte_budget(MAX_INLINE_DOCUMENT_BYTES + 1));
    }

    /// `None` is unreadable, and unreadable is not "small enough".
    #[test]
    fn page_budget_accepts_up_to_the_limit_only_when_the_count_is_known() {
        assert!(within_page_budget(Some(1)));
        assert!(within_page_budget(Some(MAX_INLINE_DOCUMENT_PAGES)));
        assert!(!within_page_budget(Some(MAX_INLINE_DOCUMENT_PAGES + 1)));
        assert!(!within_page_budget(None));
    }

    /// The count comes from the page tree, on a real PDF.
    #[test]
    fn pdf_page_count_reads_the_page_tree() {
        assert_eq!(pdf_page_count(&make_pdf(3)), Some(3));
        assert_eq!(pdf_page_count(&make_pdf(1)), Some(1));
        // Not a PDF, or a truncated one: no count, so no inline.
        assert_eq!(pdf_page_count(b"%PDF-1.4 but no xref at all"), None);
    }

    // ── Derivation ───────────────────────────────────────────────────

    /// The happy path, end to end: the part carries the name the upload was
    /// classified by, the page count, and bare base64.
    #[tokio::test]
    async fn derive_document_parts_inlines_a_pdf_when_the_model_accepts_it() {
        let bytes = make_pdf(2);
        let size = bytes.len() as u64;
        let svc: Arc<dyn AttachmentService> =
            Arc::new(MockAttachment::with_blob("doc-1", bytes));
        let items = vec![pdf_item("doc-1", "report.pdf", "pdf", size)];
        let caps = caps_with_input(&["text", "pdf"]);

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps))
            .await
            .unwrap();
        assert_eq!(parts.len(), 1);
        let doc = document_of(&parts[0]);
        assert_eq!(doc.filename, "report.pdf");
        assert_eq!(doc.media_type, acowork_core::providers::traits::PDF_MEDIA_TYPE);
        assert_eq!(doc.page_count, Some(2));
        assert!(!doc.data.starts_with("data:"));
        assert_eq!(
            BASE64_STANDARD.decode(&doc.data).unwrap(),
            make_pdf(2),
            "the payload must be the file's own bytes"
        );
    }

    /// No capability info is not a licence to try: the whole turn would be
    /// lost on a 400.
    #[tokio::test]
    async fn derive_document_parts_sends_nothing_when_capability_is_unknown() {
        let bytes = make_pdf(1);
        let size = bytes.len() as u64;
        let svc: Arc<dyn AttachmentService> =
            Arc::new(MockAttachment::with_blob("doc-1", bytes));
        let items = vec![pdf_item("doc-1", "report.pdf", "pdf", size)];

        assert!(
            derive_document_parts(Some(&svc), &items, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["text", "image"])))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Only PDFs of the `file_upload` kind survive the walk: other formats
    /// have no cross-provider document block, and the other variants are not
    /// uploads at all.
    #[tokio::test]
    async fn derive_document_parts_keeps_pdf_file_uploads_only() {
        let bytes = make_pdf(1);
        let size = bytes.len() as u64;
        let svc: Arc<dyn AttachmentService> =
            Arc::new(MockAttachment::with_blob("doc-1", bytes));
        let items = vec![
            pdf_item("doc-2", "deck.pptx", "pptx", size),
            pdf_item("doc-1", "report.pdf", "PDF", size),
            AttachedItem::ImageUpload {
                document_id: "img-1".into(),
                filename: "snap.png".into(),
                format: "png".into(),
                size_bytes: size,
                width: None,
                height: None,
                client_id: None,
            },
            AttachedItem::AttachedFile {
                abs_path: "/workspace/x.pdf".into(),
                name: "x.pdf".into(),
                client_id: None,
            },
        ];

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert_eq!(parts.len(), 1, "the workspace reference is not an upload");
        assert_eq!(document_of(&parts[0]).filename, "report.pdf");
    }

    /// An oversized upload is rejected **before its bytes are read** — the
    /// fake has no blob for it at all, and reading 200 MB to throw it away
    /// would be the bug.
    #[tokio::test]
    async fn derive_document_parts_skips_oversized_documents_without_reading_them() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![pdf_item(
            "huge",
            "huge.pdf",
            "pdf",
            MAX_INLINE_DOCUMENT_BYTES as u64 + 1,
        )];

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert!(parts.is_empty());
    }

    /// Too many pages is a skip with a warning, not an error: the document is
    /// on disk, the hint names it, and `doc_reader` takes it page by page.
    #[tokio::test]
    async fn derive_document_parts_skips_a_pdf_over_the_page_limit() {
        let bytes = make_pdf(MAX_INLINE_DOCUMENT_PAGES + 1);
        let size = bytes.len() as u64;
        let svc: Arc<dyn AttachmentService> =
            Arc::new(MockAttachment::with_blob("long", bytes));
        let items = vec![pdf_item("long", "long.pdf", "pdf", size)];

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert!(parts.is_empty());
    }

    /// A blob that is not a parseable PDF gets no page count, and an
    /// unverifiable count is an unverifiable budget.
    #[tokio::test]
    async fn derive_document_parts_skips_a_pdf_whose_pages_cannot_be_counted() {
        let junk = b"%PDF-1.4 written by a printer driver, half a file".to_vec();
        let size = junk.len() as u64;
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::with_blob("junk", junk));
        let items = vec![pdf_item("junk", "junk.pdf", "pdf", size)];

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert!(parts.is_empty());
    }

    /// A missing blob is an error, not a silent drop: the user attached the
    /// file and must hear about it, exactly as the image pipeline requires.
    #[tokio::test]
    async fn derive_document_parts_propagates_read_errors() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![pdf_item("missing", "gone.pdf", "pdf", 1024)];

        let err = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing"),
            "error must name the failing document_id, got: {msg}"
        );
    }

    /// The gate runs first, so a model that cannot take the document does not
    /// pay for a blob read either.
    #[tokio::test]
    async fn derive_document_parts_reads_nothing_when_the_gate_is_closed() {
        let svc: Arc<dyn AttachmentService> = Arc::new(MockAttachment::new());
        let items = vec![pdf_item("missing", "gone.pdf", "pdf", 1024)];

        let parts = derive_document_parts(Some(&svc), &items, None).await.unwrap();
        assert!(parts.is_empty());
    }

    /// No blob store is no inline, and no panic — the hint still carries the
    /// file.
    #[tokio::test]
    async fn derive_document_parts_without_service_is_empty() {
        let items = vec![pdf_item("doc-1", "report.pdf", "pdf", 1024)];
        let parts = derive_document_parts(None, &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert!(parts.is_empty());
    }

    /// Several PDFs in one turn all go, in the order they were attached.
    #[tokio::test]
    async fn derive_document_parts_keeps_attach_order() {
        let one = make_pdf(1);
        let two = make_pdf(2);
        let (one_size, two_size) = (one.len() as u64, two.len() as u64);
        let concrete = Arc::new(MockAttachment::with_blob("a", one));
        concrete
            .blobs
            .lock()
            .unwrap()
            .insert("b".to_string(), two);
        let svc: Arc<dyn AttachmentService> = concrete;
        let items = vec![
            pdf_item("a", "first.pdf", "pdf", one_size),
            pdf_item("b", "second.pdf", "pdf", two_size),
        ];

        let parts = derive_document_parts(Some(&svc), &items, Some(&caps_with_input(&["pdf"])))
            .await
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(document_of(&parts[0]).filename, "first.pdf");
        assert_eq!(document_of(&parts[0]).page_count, Some(1));
        assert_eq!(document_of(&parts[1]).filename, "second.pdf");
        assert_eq!(document_of(&parts[1]).page_count, Some(2));
    }
}
