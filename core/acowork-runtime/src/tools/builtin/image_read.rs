//! Image read tool — returns a local image file as a multimodal content part
//!
//! Closes the "agent has a path but no verb" gap (design doc
//! `t-65a1a89e` §1.2 R2): screenshots, workspace images and generated
//! PNGs had no tool that could turn them into model-visible pixels.
//! The bytes travel back in [`ToolResult::content_parts`]; the agent
//! loop injects them as a user-role multimodal message (S2) because
//! provider-native `tool_result` image blocks exist on only 2 of the 5
//! supported protocols.
//!
//! Contract (design doc §4.2):
//! - `path`   — local file only, resolved exactly like `file_read`
//!   (relative against `work_dir`; workspace containment enforced by
//!   `PathGuardedTool`, so the denial wording is identical to
//!   `file_read`'s).
//! - `detail` — `auto` | `low` (512 px long side) | `high` (1568 px,
//!   default) | `original` (no downscale). `region` is deliberately
//!   absent in v1 (§8 open question 2).
//!
//! Failure messages give the next action, never a bare cause (§4.2
//! failure table — copy the strings, do not paraphrase).

use std::io::{Cursor, Read};
use std::path::Path;

use acowork_core::providers::traits::{ContentPart, ImageUrlPart};
use acowork_core::tools::traits::{Tool, ToolContext, ToolResult, ToolSpec};
use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::{ImageFormat, ImageReader, Limits};
use serde_json::Value;

use crate::agent::attachment_to_image::{MAX_INLINE_IMAGE_BYTES, build_data_url};

/// Hard ceiling on either source dimension before decoding (design doc
/// §4.6: header pre-check first, decoder `Limits` as the second net).
const MAX_SIDE_PX: u32 = 8192;
/// Total pixel budget for the source image (decompression-bomb guard).
const MAX_SOURCE_PIXELS: u64 = 64_000_000;
/// Fixed JPEG re-encode quality when downscaling. Constant, never
/// content-derived: the payload must be byte-stable across calls.
const JPEG_QUALITY: u8 = 85;
const DEFAULT_MAX_IMAGES_PER_TURN: usize = 6;
const DEFAULT_MAX_BASE64_BYTES_PER_TURN: usize = 24 * 1024 * 1024;

fn max_images_per_turn() -> usize {
    std::env::var("ACOWORK_IMAGE_READ_MAX_IMAGES_PER_TURN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_IMAGES_PER_TURN)
}

fn max_base64_bytes_per_turn() -> usize {
    std::env::var("ACOWORK_IMAGE_READ_MAX_BASE64_BYTES_PER_TURN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_BASE64_BYTES_PER_TURN)
}

/// Long-side cap per `detail` level (design doc §4.2 table).
fn detail_max_side(detail: &Detail) -> Option<u32> {
    match detail {
        Detail::Low => Some(512),
        Detail::High | Detail::Auto => Some(1568),
        Detail::Original => None,
    }
}

/// The `detail` value the wire part carries (design doc §4.2 rightmost
/// column: `original` still ships as `"high"` — the *raster* is what
/// differs, not the provider hint).
fn detail_wire(detail: &Detail) -> &'static str {
    match detail {
        Detail::Low => "low",
        Detail::High => "high",
        Detail::Auto => "auto",
        Detail::Original => "high",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    Auto,
    Low,
    High,
    Original,
}

/// One supported raster format: magic bytes → decoder format → MIME.
struct Sniffed {
    name: &'static str,
    format: ImageFormat,
    mime: &'static str,
}

const PNG: Sniffed = Sniffed {
    name: "png",
    format: ImageFormat::Png,
    mime: "image/png",
};
const JPEG: Sniffed = Sniffed {
    name: "jpeg",
    format: ImageFormat::Jpeg,
    mime: "image/jpeg",
};
const GIF: Sniffed = Sniffed {
    name: "gif",
    format: ImageFormat::Gif,
    mime: "image/gif",
};
const WEBP: Sniffed = Sniffed {
    name: "webp",
    format: ImageFormat::WebP,
    mime: "image/webp",
};

/// Identify the format from the first 12 bytes.
///
/// Why magic bytes and not the extension: a wrong guess costs one
/// provider 400 that poisons the whole turn, and screenshots are
/// routinely mislabelled. This does NOT contradict ADR-046 §2.7
/// ("runtime does not do image recognition"): that rule constrains the
/// **upload-ingest** path, where sniffing would classify user content
/// the runtime has no business interpreting. `image_read` is an
/// explicit read-image action — the caller already declared the file is
/// an image; the sniff only picks the decoder.
///
/// BMP/TIFF/APNG are out of scope in v1 (design doc §4.2): the five
/// provider protocols accept the four formats below natively.
fn sniff_magic(head: &[u8]) -> Option<&'static Sniffed> {
    if head.len() >= 8 && head[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some(&PNG);
    }
    if head.len() >= 3 && head[..3] == [0xFF, 0xD8, 0xFF] {
        return Some(&JPEG);
    }
    if head.len() >= 6 && (head[..6] == *b"GIF87a" || head[..6] == *b"GIF89a") {
        return Some(&GIF);
    }
    // RIFF????WEBP — the 4 bytes after "RIFF" are the (variable) size.
    if head.len() >= 12 && head[..4] == *b"RIFF" && head[8..12] == *b"WEBP" {
        return Some(&WEBP);
    }
    None
}

/// True when the head looks like SVG (text markup, not a raster).
fn looks_like_svg(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && text.contains("<svg"))
}

/// Hex rendering of the first bytes for the "got <magic>" error slot.
fn magic_hex(head: &[u8]) -> String {
    head.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Human-readable byte size for the summary line (MiB above 1 MiB,
/// KiB below — screenshots are rarely sub-KiB but the line must not
/// read "0.0 MiB").
fn human_size(bytes: usize) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

fn fail(error: impl Into<String>) -> ToolResult {
    ToolResult {
        ok: false,
        content: String::new(),
        content_parts: None,
        error: Some(error.into()),
        token_usage: None,
    }
}

/// Image read tool — local raster file → summary text + one `ImageUrl`
/// content part carrying a `data:` URI.
pub struct ImageReadTool;

impl Default for ImageReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageReadTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "image_read".to_string(),
            description: "Read a local image file (png/jpeg/gif/webp) and return it as an image you can see, plus a text summary (path, original and delivered dimensions, size). Use for screenshots, UI mockups or any workspace image — never file_read (text only) on a raster. Local paths only; for a network image, download it first with http_request + file_write. detail: low (512px long side, cheap overview), high (default, 1568px), original (exact pixels, up to 8 MiB), auto.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Image file path, relative to the workspace or absolute (same resolution rules as file_read)" },
                    "detail": { "type": "string", "enum": ["auto", "low", "high", "original"], "description": "Resolution budget. high (default) caps the long side at 1568 px; low at 512 px; original sends the exact pixels (8 MiB / 8192 px ceilings still apply)" }
                },
                "required": ["path"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ImageReadTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        Ok(execute_inner(params, work_dir))
    }

    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        if !ctx.supports_image_input {
            return Ok(fail(
                "current model has no image input capability; switch to a vision model, or ask the user to describe it",
            ));
        }
        let Some(budget) = ctx.image_read_budget.as_ref() else {
            return Ok(execute_inner(params, ctx.work_dir.as_deref()));
        };
        let mut guard = budget.lock().expect("image budget mutex poisoned");
        let max_images = max_images_per_turn();
        if guard.images >= max_images {
            return Ok(fail(format!(
                "image_read turn limit reached: this turn has already read {max_images} images; continue based on the images already available, or wait for the next turn"
            )));
        }
        let result = execute_inner(params, ctx.work_dir.as_deref());
        if result.ok {
            let bytes = result
                .content_parts
                .as_ref()
                .and_then(|parts| parts.first())
                .and_then(image_part_base64_bytes)
                .unwrap_or(0);
            let max_bytes = max_base64_bytes_per_turn();
            if guard.base64_bytes.saturating_add(bytes) > max_bytes {
                return Ok(fail(format!(
                    "image_read turn byte limit reached: this turn has used the configured {} MiB image-data limit; continue based on the images already available, or wait for the next turn",
                    max_bytes / (1024 * 1024)
                )));
            }
            guard.images += 1;
            guard.base64_bytes += bytes;
        }
        Ok(result)
    }
}

/// Sync body: every step below is CPU/IO work the `image` crate only
/// exposes synchronously, and the payloads are bounded by the budgets
/// checked here — same trade-off `doc_reader` already makes for PDF
/// parsing. Keeping it sync also lets unit tests call `execute_inner`
/// directly without a runtime.
fn execute_inner(params: Value, work_dir: Option<&str>) -> ToolResult {
    let path = params["path"].as_str().unwrap_or("");
    if path.is_empty() {
        return fail("Missing 'path' parameter");
    }
    // URLs are rejected outright: fetching here would smuggle an
    // SSRF surface into a read tool. The auditable path is
    // http_request (network:<url> permission) + file_write, then
    // image_read on the saved file (design doc §4.6).
    if path.starts_with("http://") || path.starts_with("https://") {
        return fail(
            "image_read takes a local path, not a URL; download with http_request + file_write first, then image_read the saved file",
        );
    }
    let detail = match params["detail"].as_str() {
        None | Some("high") => Detail::High,
        Some("auto") => Detail::Auto,
        Some("low") => Detail::Low,
        Some("original") => Detail::Original,
        Some(other) => {
            return fail(format!(
                "unknown detail '{other}'; use auto, low, high or original"
            ));
        }
    };

    // Same resolution rule as file_read; workspace containment and the
    // denial wording are the PathGuardedTool wrapper's job (image_read
    // is registered in its filesystem-tool list).
    let full_path = acowork_core::path_utils::resolve(path, work_dir);

    let meta = match std::fs::metadata(&full_path) {
        Ok(m) if m.is_file() => m,
        _ => {
            return fail(format!(
                "image not found at {path}; use glob_search to locate it"
            ));
        }
    };

    let sniffed = match sniff_head(&full_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let head = read_head(&full_path, 512).unwrap_or_default();
            if looks_like_svg(&head) {
                return fail(
                    "svg is not accepted by model providers; convert to png first (e.g. via shell)",
                );
            }
            return fail(format!(
                "not a supported image (png/jpeg/gif/webp); got {}; use file_read for text or doc_reader for pdf/docx",
                magic_hex(&head)
            ));
        }
        Err(e) => return fail(format!("failed to read {path}: {e}")),
    };

    // Decompression-bomb order (design doc §4.6): header dimensions
    // first — decoding never starts on an over-budget source.
    let (src_w, src_h) = match header_dimensions(&full_path, sniffed.format) {
        Ok(d) => d,
        Err(_) => {
            return fail(format!(
                "failed to decode {} image (corrupt or truncated); re-export it, or use file_read to inspect the bytes",
                sniffed.name
            ));
        }
    };
    if src_w > MAX_SIDE_PX
        || src_h > MAX_SIDE_PX
        || u64::from(src_w) * u64::from(src_h) > MAX_SOURCE_PIXELS
    {
        return fail(format!(
            "image dimensions {src_w}x{src_h} exceed the decode budget"
        ));
    }

    // The payload. Unchanged raster ⇒ original bytes: no re-encode, so
    // the base64 is trivially stable and lossless formats keep their
    // exact pixels. Re-encoding only happens when a downscale actually
    // occurred, with fixed parameters (JPEG_QUALITY constant, single
    // filter) so the same file + detail is byte-identical across calls
    // — prefix-cache stability (ADR-060), not cosmetics.
    //
    // EXIF note: no orientation is applied (image 0.25 does not
    // auto-rotate; applying it here would rewrite pixels `original`
    // promises not to touch), and the tool makes NO promise of
    // stripping EXIF or other metadata — passthrough ships the file
    // as-is. Say so rather than imply a privacy guarantee.
    let max_side = detail_max_side(&detail);
    let needs_scale = max_side.is_some_and(|cap| src_w.max(src_h) > cap);

    let (payload, out_w, out_h, scaled, out_mime) = if needs_scale {
        let cap = max_side.unwrap_or(MAX_SIDE_PX);
        let img = match decode_bounded(&full_path, sniffed.format) {
            Ok(img) => img,
            Err(_) => {
                return fail(format!(
                    "failed to decode {} image (corrupt or truncated); re-export it, or use file_read to inspect the bytes",
                    sniffed.name
                ));
            }
        };
        let resized = img.resize(cap, cap, image::imageops::FilterType::Lanczos3);
        let out_w = resized.width();
        let out_h = resized.height();
        let (out_format, mime) = output_format(sniffed.format);
        let payload = match encode_raster(&resized, out_format) {
            Ok(p) => p,
            Err(_) => {
                return fail(format!(
                    "failed to encode resized {} image; retry with detail=low",
                    sniffed.name
                ));
            }
        };
        if payload.len() > MAX_INLINE_IMAGE_BYTES {
            return fail(format!(
                "image too large after downscale ({} > 8 MiB); crop or re-encode at lower quality",
                human_size(payload.len())
            ));
        }
        (payload, out_w, out_h, true, mime)
    } else {
        // Passthrough ships the file bytes as-is, so the 8 MiB budget
        // is a statement about the *file* — and the pixel pre-check
        // above does not bound file bytes (chunk padding is arbitrary).
        // Reject on the recorded size before anything is read.
        if meta.len() as usize > MAX_INLINE_IMAGE_BYTES {
            return fail(format!(
                "image too large ({} > 8 MiB); crop or re-encode at lower quality",
                human_size(meta.len() as usize)
            ));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        if let Err(e) = std::fs::File::open(&full_path).and_then(|mut f| f.read_to_end(&mut buf)) {
            return fail(format!("failed to read {path}: {e}"));
        }
        (buf, src_w, src_h, false, sniffed.mime)
    };

    let url = build_data_url(out_mime, &payload);
    let part = ContentPart::ImageUrl {
        image_url: ImageUrlPart {
            url,
            detail: Some(detail_wire(&detail).to_string()),
            // D6: the delivered raster's real dimensions, so
            // `estimate_image_tokens` prices this part exactly instead
            // of falling back to the 512x512 unknown-size estimate.
            width: Some(out_w),
            height: Some(out_h),
        },
    };

    let shown = display_path(&full_path);
    let summary = if scaled {
        format!(
            "image: {shown}\nsize: {src_w}x{src_h} {}, {} → {out_w}x{out_h} (scaled, detail={})\nfollows as 1 image part",
            sniffed.name,
            human_size(meta.len() as usize),
            detail_name(&detail),
        )
    } else {
        format!(
            "image: {shown}\nsize: {out_w}x{out_h} {}, {} (detail={})\nfollows as 1 image part",
            sniffed.name,
            human_size(payload.len()),
            detail_name(&detail),
        )
    };

    ToolResult {
        ok: true,
        content: summary,
        content_parts: Some(vec![part]),
        error: None,
        token_usage: None,
    }
}

fn image_part_base64_bytes(part: &ContentPart) -> Option<usize> {
    match part {
        ContentPart::ImageUrl { image_url } => {
            let encoded = image_url.url.split_once(',')?.1;
            let padding = encoded
                .as_bytes()
                .iter()
                .rev()
                .take_while(|byte| **byte == b'=')
                .count();
            Some(encoded.len().saturating_mul(3) / 4 - padding)
        }
        _ => None,
    }
}

fn detail_name(detail: &Detail) -> &'static str {
    match detail {
        Detail::Auto => "auto",
        Detail::Low => "low",
        Detail::High => "high",
        Detail::Original => "original",
    }
}

/// Absolute path with `/` separators for the summary line — the string
/// the model will feed back to `glob_search` / `file_read`, so it must
/// be copy-pasteable on every platform.
fn display_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn read_head(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf)?;
    buf.truncate(got);
    Ok(buf)
}

fn sniff_head(path: &Path) -> std::io::Result<Option<&'static Sniffed>> {
    Ok(sniff_magic(&read_head(path, 12)?))
}

fn open_reader(
    path: &Path,
    format: ImageFormat,
) -> std::io::Result<ImageReader<std::io::BufReader<std::fs::File>>> {
    let file = std::fs::File::open(path)?;
    Ok(ImageReader::with_format(
        std::io::BufReader::new(file),
        format,
    ))
}

/// Header-only dimension read — no pixel decode.
fn header_dimensions(path: &Path, format: ImageFormat) -> image::ImageResult<(u32, u32)> {
    open_reader(path, format)?.into_dimensions()
}

/// Full decode, with the same budgets re-asserted as decoder limits
/// (the header pre-check is the first net; this is the second, for
/// formats whose reported size lies).
fn decode_bounded(path: &Path, format: ImageFormat) -> image::ImageResult<image::DynamicImage> {
    let mut reader = open_reader(path, format)?;
    // `Limits` is `#[non_exhaustive]` upstream — field assignment,
    // not a struct literal.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE_PX);
    limits.max_image_height = Some(MAX_SIDE_PX);
    reader.limits(limits);
    reader.decode()
}

/// Output format (and its MIME) after a downscale. JPEG keeps JPEG at
/// the fixed quality; everything else becomes PNG. The `image` crate's
/// WebP encoder is lossless-only (VP8L) — re-encoding a downscaled
/// lossy WebP as VP8L inflates photo content several-fold and burns
/// the 8 MiB payload budget for no perceptual gain. PNG keeps alpha
/// (GIF/WebP sources), is deterministic at fixed encoder settings,
/// and the data URI's MIME always describes the payload actually sent,
/// not the file on disk.
fn output_format(source: ImageFormat) -> (ImageFormat, &'static str) {
    match source {
        ImageFormat::Jpeg => (ImageFormat::Jpeg, "image/jpeg"),
        _ => (ImageFormat::Png, "image/png"),
    }
}

/// Deterministic re-encode after a downscale. `format` is the
/// *output* format from [`output_format`] (JPEG or PNG only).
/// Animated GIFs collapse to the first frame (decode() yields frame 0)
/// — v1 accepts that.
fn encode_raster(img: &image::DynamicImage, format: ImageFormat) -> image::ImageResult<Vec<u8>> {
    let mut buf = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            // JPEG has no alpha channel: flattening is the encoder's
            // call, quality is ours and CONSTANT (determinism).
            let rgb = img.to_rgb8();
            let mut cursor = Cursor::new(&mut buf);
            JpegEncoder::new_with_quality(&mut cursor, JPEG_QUALITY).encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )?;
        }
        other => img.write_to(Cursor::new(&mut buf), other)?,
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::parse_data_uri;
    use image::{DynamicImage, ImageBuffer, Rgb};

    /// Deterministic gradient — same generator every call, so
    /// re-encode stability tests compare against themselves.
    fn gradient(w: u32, h: u32) -> DynamicImage {
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        DynamicImage::ImageRgb8(buf)
    }

    fn write_sample(dir: &Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let p = dir.join(name);
        gradient(w, h).save(&p).expect("save sample");
        p
    }

    fn call(path: &Path, detail: Option<&str>) -> ToolResult {
        let mut params = serde_json::json!({ "path": path.to_string_lossy() });
        if let Some(d) = detail {
            params["detail"] = Value::String(d.to_string());
        }
        // work_dir = path's parent so the relative-resolution branch
        // is exercised too.
        execute_inner(params, path.parent().and_then(|p| p.to_str()))
    }

    fn part_dims(r: &ToolResult) -> (Option<u32>, Option<u32>) {
        match &r.content_parts.as_ref().expect("parts present")[0] {
            ContentPart::ImageUrl { image_url } => (image_url.width, image_url.height),
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    fn part_url(r: &ToolResult) -> String {
        match &r.content_parts.as_ref().expect("parts present")[0] {
            ContentPart::ImageUrl { image_url } => image_url.url.clone(),
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_vision_fails_before_touching_image_path() {
        let ctx = ToolContext {
            supports_image_input: false,
            ..Default::default()
        };
        let result = ImageReadTool::new()
            .execute_with_context(serde_json::json!({"path":"missing.png"}), &ctx)
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "current model has no image input capability; switch to a vision model, or ask the user to describe it"
            )
        );
    }

    #[tokio::test]
    async fn per_turn_image_count_budget_rejects_next_call_actionably() {
        use acowork_core::tools::traits::ImageReadBudget;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), "s.png", 40, 30);
        let budget = Arc::new(Mutex::new(ImageReadBudget {
            images: 6,
            base64_bytes: 0,
        }));
        let ctx = ToolContext {
            work_dir: Some(dir.path().to_string_lossy().into_owned()),
            image_read_budget: Some(budget),
            ..Default::default()
        };
        let result = ImageReadTool::new()
            .execute_with_context(serde_json::json!({"path":path}), &ctx)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("already read 6 images; continue based on the images already available, or wait for the next turn"));
    }

    #[tokio::test]
    async fn per_turn_base64_budget_rejects_next_call_actionably() {
        use acowork_core::tools::traits::ImageReadBudget;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), "s.png", 40, 30);
        let budget = Arc::new(Mutex::new(ImageReadBudget {
            images: 1,
            base64_bytes: DEFAULT_MAX_BASE64_BYTES_PER_TURN - 1,
        }));
        let ctx = ToolContext {
            work_dir: Some(dir.path().to_string_lossy().into_owned()),
            image_read_budget: Some(budget),
            ..Default::default()
        };
        let result = ImageReadTool::new()
            .execute_with_context(serde_json::json!({"path":path}), &ctx)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("configured 24 MiB image-data limit; continue based on the images already available, or wait for the next turn"));
    }

    #[tokio::test]
    async fn successful_reads_accumulate_per_turn_image_budget() {
        use acowork_core::tools::traits::ImageReadBudget;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), "s.png", 40, 30);
        let budget = Arc::new(Mutex::new(ImageReadBudget::default()));
        let ctx = ToolContext {
            work_dir: Some(dir.path().to_string_lossy().into_owned()),
            image_read_budget: Some(budget.clone()),
            ..Default::default()
        };
        let result = ImageReadTool::new()
            .execute_with_context(serde_json::json!({"path":path}), &ctx)
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(budget.lock().unwrap().images, 1);
        assert!(budget.lock().unwrap().base64_bytes > 0);
    }

    #[test]
    fn four_formats_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["s.png", "s.jpg", "s.gif", "s.webp"] {
            let p = write_sample(dir.path(), name, 40, 30);
            let r = call(&p, None);
            assert!(r.ok, "{name}: {:?}", r.error);
            assert_eq!(part_dims(&r), (Some(40), Some(30)), "{name}");
            assert_eq!(r.content_parts.as_ref().unwrap().len(), 1);
            let uri = part_url(&r);
            let (mime, _) = parse_data_uri(&uri).expect("data uri");
            let want = match name.split('.').next_back().unwrap() {
                "png" => "image/png",
                "jpg" => "image/jpeg",
                "gif" => "image/gif",
                _ => "image/webp",
            };
            assert_eq!(mime, want, "{name}");
            assert!(r.content.contains("follows as 1 image part"));
        }
    }

    #[test]
    fn detail_caps_long_side_and_backfills_dims() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path(), "big.png", 2000, 1500);

        let r = call(&p, Some("high"));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(part_dims(&r), (Some(1568), Some(1176)));
        // D6: the delivered part must price exactly — the data URI's
        // real raster equals the backfilled width/height.
        let uri = part_url(&r);
        let (_, b64) = parse_data_uri(&uri).unwrap();
        let decoded = image::load_from_memory(&base64_decode(b64)).expect("payload decodes");
        assert_eq!((decoded.width(), decoded.height()), (1568, 1176));
        assert!(r.content.contains("2000x1500 png"));
        assert!(r.content.contains("1568x1176 (scaled, detail=high)"));

        let r = call(&p, Some("low"));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(part_dims(&r), (Some(512), Some(384)));

        let r = call(&p, Some("auto"));
        assert_eq!(
            part_dims(&r),
            (Some(1568), Some(1176)),
            "auto behaves as high"
        );
    }

    /// Small helper so the test above reads linearly.
    fn base64_decode(s: &str) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap()
    }

    #[test]
    fn original_does_not_touch_the_raster() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path(), "orig.png", 2000, 1500);
        let raw = std::fs::read(&p).unwrap();

        let r = call(&p, Some("original"));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(part_dims(&r), (Some(2000), Some(1500)));
        // Passthrough: the payload IS the file's bytes.
        let uri = part_url(&r);
        let (_, b64) = parse_data_uri(&uri).unwrap();
        assert_eq!(base64_decode(b64), raw);
        assert!(r.content.contains("detail=original"));
    }

    #[test]
    fn same_file_same_detail_is_byte_stable() {
        // ADR-060: any per-call jitter in the base64 would miss the
        // prompt cache on every turn.
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path(), "stable.jpg", 1900, 900);
        let a = part_url(&call(&p, Some("high")));
        let b = part_url(&call(&p, Some("high")));
        assert_eq!(a, b);
    }

    #[test]
    fn small_images_pass_through_unscaled() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path(), "small.png", 100, 80);
        let raw = std::fs::read(&p).unwrap();
        let r = call(&p, Some("low"));
        assert!(r.ok, "{:?}", r.error);
        assert!(!r.content.contains("scaled"));
        let uri = part_url(&r);
        let (_, b64) = parse_data_uri(&uri).unwrap();
        assert_eq!(
            base64_decode(b64),
            raw,
            "already under the cap: no re-encode"
        );
    }

    /// Deterministic photo-like noise (fixed-seed LCG) — the content a
    /// lossless codec cannot compress. Smooth gradients hide payload
    /// bloat, so the downscale-size regressions below must use this.
    fn noise(w: u32, h: u32) -> DynamicImage {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |_, _| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = (state >> 33) as u8;
            Rgb([v, v.rotate_left(3), v.rotate_right(5)])
        });
        DynamicImage::ImageRgb8(buf)
    }

    /// M1 regression: the `image` crate's WebP encoder is lossless-only
    /// (VP8L). Re-encoding a downscaled lossy WebP as VP8L inflated the
    /// payload several-fold and could exceed the 8 MiB budget outright;
    /// GIF got the same format-preserving treatment. After a downscale,
    /// non-JPEG sources must ship as PNG and the data URI's MIME must
    /// describe the payload actually sent, not the file on disk.
    #[test]
    fn downscaled_webp_and_gif_reencode_as_png() {
        for name in ["photo.webp", "photo.gif"] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join(name);
            noise(2000, 1500).save(&p).unwrap();
            let r = call(&p, Some("high"));
            assert!(r.ok, "{name}: {:?}", r.error);
            assert_eq!(part_dims(&r), (Some(1568), Some(1176)), "{name}");
            let uri = part_url(&r);
            let (mime, b64) = parse_data_uri(&uri).unwrap();
            assert_eq!(mime, "image/png", "{name}: downscaled non-JPEG ships PNG");
            let payload = base64_decode(b64);
            assert!(
                payload.len() < MAX_INLINE_IMAGE_BYTES,
                "{name}: payload {} must stay under the 8 MiB budget",
                payload.len()
            );
            let decoded = image::load_from_memory(&payload).expect("payload decodes");
            assert_eq!((decoded.width(), decoded.height()), (1568, 1176));
        }
    }

    /// M2 regression: the passthrough branch used to `read_to_end` the
    /// whole file before checking the 8 MiB budget. A valid header with
    /// arbitrary trailing bytes must be rejected on the recorded file
    /// size, before anything is loaded into memory.
    #[test]
    fn oversized_file_is_rejected_before_being_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("padded.jpg");
        gradient(100, 80).save(&p).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, &vec![0u8; 9 * 1024 * 1024]).unwrap();
        drop(f);
        let r = call(&p, Some("original"));
        assert!(!r.ok);
        assert_eq!(
            r.error.unwrap(),
            "image too large (9.0 MiB > 8 MiB); crop or re-encode at lower quality"
        );
    }

    #[test]
    fn error_paths_match_design_doc_table() {
        let dir = tempfile::tempdir().unwrap();

        // 1. not found
        let missing = dir.path().join("nope.png");
        let r = call(&missing, None);
        assert!(!r.ok);
        assert_eq!(
            r.error.unwrap(),
            format!(
                "image not found at {}; use glob_search to locate it",
                missing.display()
            )
        );

        // 2. not an image (magic mismatch)
        let txt = dir.path().join("note.txt");
        std::fs::write(&txt, b"hello world, plain text").unwrap();
        let r = call(&txt, None);
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(
            e.starts_with("not a supported image (png/jpeg/gif/webp); got "),
            "{e}"
        );
        assert!(
            e.ends_with("; use file_read for text or doc_reader for pdf/docx"),
            "{e}"
        );

        // 3. svg
        let svg = dir.path().join("icon.svg");
        std::fs::write(&svg, b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
        let r = call(&svg, None);
        assert_eq!(
            r.error.unwrap(),
            "svg is not accepted by model providers; convert to png first (e.g. via shell)"
        );

        // 4. pixel budget (side > 8192 — header-only, no big decode)
        let huge = dir.path().join("huge.png");
        gradient(9000, 10).save(&huge).unwrap();
        let r = call(&huge, None);
        assert_eq!(
            r.error.unwrap(),
            "image dimensions 9000x10 exceed the decode budget"
        );

        // 5. URL rejection (SSRF surface)
        let r = execute_inner(
            serde_json::json!({ "path": "https://example.com/a.png" }),
            None,
        );
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("not a URL"));

        // 6. bad detail value
        let r = execute_inner(
            serde_json::json!({ "path": txt.to_string_lossy(), "detail": "ultra" }),
            None,
        );
        assert!(r.error.unwrap().starts_with("unknown detail 'ultra'"));
    }

    #[test]
    fn relative_path_resolves_against_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "rel.png", 20, 20);
        let params = serde_json::json!({ "path": "rel.png" });
        let r = execute_inner(params, dir.path().to_str());
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(part_dims(&r), (Some(20), Some(20)));
    }

    #[test]
    fn wire_detail_mapping() {
        assert_eq!(detail_wire(&Detail::Original), "high");
        assert_eq!(detail_wire(&Detail::Low), "low");
        assert_eq!(detail_wire(&Detail::Auto), "auto");
        assert_eq!(detail_wire(&Detail::High), "high");
    }

    #[test]
    fn magic_sniffing_accepts_only_the_four() {
        assert!(
            sniff_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0]).is_some()
        );
        assert!(sniff_magic(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0]).is_some());
        assert!(sniff_magic(b"GIF89a\x01\x00\x01\x00\x00\x00").is_some());
        assert!(sniff_magic(b"RIFF\x24\x00\x00\x00WEBPVP8 ").is_some());
        assert!(sniff_magic(b"%PDF-1.4\x00\x00\x00\x00").is_none());
        assert!(sniff_magic(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").is_none());
    }
}
