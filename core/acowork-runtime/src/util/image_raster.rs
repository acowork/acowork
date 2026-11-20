//! Shared raster handling for images that travel to a model.
//!
//! ## Why this module exists
//!
//! Two paths put pixels on the wire, and they used to have two separate
//! implementations of the same physics:
//!
//! - the `image_read` builtin tool (the agent opens a workspace file), and
//! - the attachment pipeline (`derive_image_parts_from_items` — the picture
//!   a human pasted into the chat).
//!
//! The second path had no raster handling at all: the uploaded bytes were
//! base64'd verbatim, so a 4K screenshot cost the model its full pixel bill
//! on **every** request of the session. The first path already knew how to
//! downscale (`image_read`'s `detail` levels, Lanczos3, fixed JPEG quality)
//! but that knowledge was welded to the tool.
//!
//! This module is the seam: the scaling / encoding physics live here once,
//! and both callers state their own policy (what long side they allow, how
//! much memory they are willing to spend decoding, what payload they accept).
//!
//! ## The rules
//!
//! - **Deterministic**: same bytes in, same bytes out. The filter, the JPEG
//!   quality and the output format are constants, never content-derived —
//!   a request must be byte-stable across turns (ADR-060 prefix cache).
//! - **Decode budget first**: header dimensions are read before any pixel is
//!   decoded, and a source outside the caller's [`DecodeBudget`] is never
//!   decoded at all (decompression-bomb guard). The budget is a *parameter*
//!   because the two callers can afford different peaks: the tool runs on an
//!   agent's explicit read, the attachment pipeline runs on a human's paste
//!   while the desktop is also running the model.
//! - **Payload budget after**: a downscaled raster that still exceeds the
//!   caller's byte budget is re-encoded as JPEG (fixed quality) before the
//!   caller is told "too large" — a picture that was successfully scaled is
//!   never dropped for its PNG's sake.
//! - **Format whitelist**: PNG / JPEG / GIF / WebP only — the four formats
//!   the providers accept as image input. BMP/TIFF/APNG are out of scope for
//!   v1 (design doc §4.2), an SVG is an image but not one we can decode, and
//!   the runtime does no image recognition.
//!
//! Sniffing by magic bytes rather than by extension is deliberate: a wrong
//! guess costs one provider 400 that poisons the whole turn, and screenshots
//! are routinely mislabelled. This does NOT contradict ADR-046 §2.7
//! ("runtime does not do image recognition") — that rule constrains the
//! **upload-ingest** path, where sniffing would classify user content the
//! runtime has no business interpreting. Both callers of this module are
//! explicit image actions: `image_read` was told the path is an image, and
//! the attachment pipeline carries a human-declared MIME. The upload endpoint
//! itself (`POST /sessions/{sid}/files`) still stores bytes untouched.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{ImageFormat, ImageReader, Limits};

/// Long side (px) a delivered image may have, unless a caller overrides it.
///
/// 1568 is the largest raster OpenAI's `high` detail and Anthropic's image
/// input both accept without further server-side rescaling. Beyond it the
/// pixels cost tokens the model cannot use.
pub const DEFAULT_MAX_SIDE_PX: u32 = 1568;

/// Fixed JPEG re-encode quality when downscaling. Constant, never
/// content-derived: the payload must be byte-stable across calls.
pub const JPEG_QUALITY: u8 = 85;

/// How much memory a caller is willing to spend decoding one source image.
///
/// Two numbers, because the two hazards are different: a source wider than
/// `max_side_px` can be a decoder integer-overflow / bomb vector regardless
/// of how few pixels it totals, while `max_pixels` is the one that decides
/// the actual allocation (`pixels × 4` bytes for RGBA).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeBudget {
    /// Hard ceiling on either source dimension.
    pub max_side_px: u32,
    /// Total pixel budget for the source image.
    pub max_pixels: u64,
}

impl DecodeBudget {
    /// True when a source of this size may be decoded at all.
    pub fn accepts(&self, width: u32, height: u32) -> bool {
        width <= self.max_side_px
            && height <= self.max_side_px
            && u64::from(width) * u64::from(height) <= self.max_pixels
    }

    /// The same budget expressed as a decoder allocation cap (second net,
    /// for formats whose header size lies). RGBA is 4 bytes per pixel; the
    /// slack covers decoder scratch space that is not the output buffer.
    fn max_alloc_bytes(&self) -> u64 {
        self.max_pixels * 4 + (8 << 20)
    }
}

/// The budget for `image_read` — an explicit, agent-initiated read of one
/// file. Unchanged from before the attachment pipeline shared this module.
pub const TOOL_DECODE_BUDGET: DecodeBudget = DecodeBudget {
    max_side_px: 8192,
    max_pixels: 64_000_000,
};

/// One supported raster format: magic bytes → decoder format → MIME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterFormat {
    pub name: &'static str,
    pub format: ImageFormat,
    pub mime: &'static str,
}

pub const PNG: RasterFormat = RasterFormat {
    name: "png",
    format: ImageFormat::Png,
    mime: "image/png",
};
pub const JPEG: RasterFormat = RasterFormat {
    name: "jpeg",
    format: ImageFormat::Jpeg,
    mime: "image/jpeg",
};
pub const GIF: RasterFormat = RasterFormat {
    name: "gif",
    format: ImageFormat::Gif,
    mime: "image/gif",
};
pub const WEBP: RasterFormat = RasterFormat {
    name: "webp",
    format: ImageFormat::WebP,
    mime: "image/webp",
};

/// Identify the format from the first 12 bytes.
///
/// Magic bytes, not the extension: a wrong guess costs one provider 400
/// that would take the whole turn down, and the extension is a claim by
/// whoever uploaded the file.
pub fn sniff_bytes(bytes: &[u8]) -> Option<&'static RasterFormat> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some(&PNG),
        [0xFF, 0xD8, 0xFF, ..] => Some(&JPEG),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some(&GIF),
        // RIFF????WEBP — the 4 bytes after "RIFF" are the (variable) size.
        bytes if bytes.len() >= 12 && bytes[..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" => {
            Some(&WEBP)
        }
        _ => None,
    }
}

/// Header-only dimension read — no pixel decode.
pub fn dimensions(bytes: &[u8], format: &RasterFormat) -> Option<(u32, u32)> {
    let reader = ImageReader::with_format(Cursor::new(bytes), format.format);
    reader.into_dimensions().ok()
}

/// The raster a [`resize_and_encode`] produced: the bytes, the MIME that
/// describes *those* bytes, and the size a caller must report when it claims
/// what the model received.
#[derive(Debug, Clone)]
pub struct EncodedRaster {
    /// The re-encoded payload (NOT the source bytes).
    pub bytes: Vec<u8>,
    /// MIME of `bytes` — always describes the payload actually sent.
    pub mime: &'static str,
    /// Width of the encoded raster.
    pub width: u32,
    /// Height of the encoded raster.
    pub height: u32,
}

/// Why a downscale could not be performed.
///
/// Only [`RasterError::PayloadTooLarge`] is a final answer; the rest mean
/// "the caller must decide", and the two callers decide differently
/// (`image_read` refuses, the attachment pipeline passes the original bytes
/// through).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RasterError {
    /// The bytes are not a raster this module can decode.
    NotDecodable,
    /// The source is outside the decode budget — it is never decoded
    /// (decompression-bomb guard).
    OverDecodeBudget,
    /// The re-encoded raster is still over the caller's payload budget, even
    /// after the deterministic JPEG fallback.
    PayloadTooLarge {
        /// Size of the payload that was produced.
        size: usize,
    },
    /// Decode or encode failed (corrupt payload, out of memory).
    Failed(String),
}

/// Resize a decoded image into the delivery box and encode it within a byte
/// budget. The single implementation of "what a downscale produces".
///
/// The natural output format is [`output_format`]'s; when that payload
/// exceeds `max_payload_bytes` the same raster is re-encoded as JPEG at
/// [`JPEG_QUALITY`] before giving up. Both attempts are fixed-parameter, so
/// the result is byte-stable for a given input (ADR-060).
pub fn resize_and_encode(
    img: &image::DynamicImage,
    source: ImageFormat,
    max_side: u32,
    max_payload_bytes: usize,
) -> Result<EncodedRaster, RasterError> {
    let resized = img.resize(max_side, max_side, image::imageops::FilterType::Lanczos3);
    let (out_format, mut mime) = output_format(source);
    let mut bytes =
        encode_raster(&resized, out_format).map_err(|e| RasterError::Failed(e.to_string()))?;

    // PNG is right for alpha and for lossless sources, and wrong for a noisy
    // photo that landed just over the byte budget: the picture was scaled
    // successfully, so the answer is a smaller *encoding*, not a drop.
    if bytes.len() > max_payload_bytes && out_format != ImageFormat::Jpeg {
        let jpeg = encode_raster(&resized, ImageFormat::Jpeg)
            .map_err(|e| RasterError::Failed(e.to_string()))?;
        if jpeg.len() <= max_payload_bytes {
            tracing::debug!(
                png_bytes = bytes.len(),
                jpeg_bytes = jpeg.len(),
                "downscaled raster exceeded the payload budget as PNG; delivered as JPEG"
            );
            bytes = jpeg;
            mime = "image/jpeg";
        }
    }

    if bytes.len() > max_payload_bytes {
        return Err(RasterError::PayloadTooLarge { size: bytes.len() });
    }
    Ok(EncodedRaster {
        bytes,
        mime,
        width: resized.width(),
        height: resized.height(),
    })
}

/// Full decode, with the decode budget re-asserted as decoder limits
/// (the header pre-check is the first net; this is the second, for
/// formats whose reported size lies).
pub fn decode_bounded(
    bytes: &[u8],
    format: &RasterFormat,
    budget: DecodeBudget,
) -> Result<image::DynamicImage, image::ImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.format);
    apply_decode_limits(&mut reader, budget);
    reader.decode()
}

/// Full decode straight from a file, with the same decode budget.
///
/// `image_read` reads a workspace file and never holds the whole file in
/// memory before decoding; the attachment pipeline already holds the bytes
/// (it base64's them), so it uses [`decode_bounded`].
pub fn decode_bounded_from_path(
    path: &std::path::Path,
    format: &RasterFormat,
    budget: DecodeBudget,
) -> Result<image::DynamicImage, image::ImageError> {
    let file = std::fs::File::open(path)?;
    let mut reader = ImageReader::with_format(std::io::BufReader::new(file), format.format);
    apply_decode_limits(&mut reader, budget);
    reader.decode()
}

/// Re-assert the decode budget as decoder limits.
///
/// `Limits` is `#[non_exhaustive]` upstream — field assignment, not a
/// struct literal.
fn apply_decode_limits<R: std::io::BufRead + std::io::Seek>(
    reader: &mut ImageReader<R>,
    budget: DecodeBudget,
) {
    let mut limits = Limits::default();
    limits.max_image_width = Some(budget.max_side_px);
    limits.max_image_height = Some(budget.max_side_px);
    limits.max_alloc = Some(budget.max_alloc_bytes());
    reader.limits(limits);
}

/// Output format (and its MIME) after a downscale.
///
/// JPEG keeps JPEG at the fixed quality; everything else becomes PNG.
/// The `image` crate's WebP encoder is lossless-only (VP8L) — re-encoding
/// a downscaled lossy WebP as VP8L inflates photo content several-fold and
/// burns the payload budget for no perceptual gain. PNG keeps alpha
/// (GIF/WebP sources), is deterministic at fixed encoder settings, and the
/// MIME always describes the payload actually sent, not the file on disk.
pub fn output_format(source: ImageFormat) -> (ImageFormat, &'static str) {
    match source {
        ImageFormat::Jpeg => (ImageFormat::Jpeg, "image/jpeg"),
        _ => (ImageFormat::Png, "image/png"),
    }
}

/// Deterministic re-encode after a downscale. `format` is the *output*
/// format from [`output_format`] (JPEG or PNG only).
///
/// Animated GIFs collapse to the first frame (decode() yields frame 0).
pub fn encode_raster(
    img: &image::DynamicImage,
    format: ImageFormat,
) -> Result<Vec<u8>, image::ImageError> {
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

    #[test]
    fn sniffs_the_four_supported_formats() {
        assert_eq!(
            sniff_bytes(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Some(&PNG)
        );
        assert_eq!(sniff_bytes(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some(&JPEG));
        assert_eq!(sniff_bytes(b"GIF89a\x01\x00\x01\x00\x00\x00"), Some(&GIF));
        assert_eq!(sniff_bytes(b"GIF87a\x01\x00\x01\x00\x00\x00"), Some(&GIF));
        assert_eq!(sniff_bytes(b"RIFF\x24\x00\x00\x00WEBPVP8 "), Some(&WEBP));
    }

    /// Formats the whitelist deliberately excludes (design doc §4.2): BMP and
    /// TIFF are rasters we could decode but the provider protocols do not
    /// accept, and PDF/SVG are not rasters at all.
    #[test]
    fn sniffs_nothing_outside_the_whitelist() {
        assert_eq!(sniff_bytes(b"%PDF-1.4"), None);
        assert_eq!(
            sniff_bytes(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            None
        );
        assert_eq!(sniff_bytes(b"II*\x00"), None);
        assert_eq!(sniff_bytes(b"<svg xmlns="), None);
        assert_eq!(sniff_bytes(b""), None);
    }
}