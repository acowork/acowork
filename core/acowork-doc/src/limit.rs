//! Request-size limits: the transport ceiling and the document ceiling.
//!
//! ## Why this is two numbers, not one
//!
//! `DocConfig::max_doc_size_bytes` answers a *user* question: "how large a
//! document may I store?" It must therefore be measured on the **decoded
//! content**, the thing the user typed. The HTTP body that carries it is
//! JSON, and JSON escaping is not size preserving: a control character
//! becomes `\uXXXX` (6×), a quote or newline doubles. Checking the raw body
//! against the document limit would reject a legitimate document once its
//! escaping pushed the envelope over the line — the same class of "the
//! documented knob does not mean what it says" bug this module exists to
//! remove.
//!
//! So:
//!
//! - [`transport_ceiling`] bounds the **wire** body: document limit, times
//!   the worst-case JSON escape factor, plus slack for the envelope's other
//!   fields. Its only job is to stop unbounded memory use.
//! - [`ensure_document_size`] bounds the **document** after decoding, and is
//!   what a caller actually observes. It is checked on the resulting size, so
//!   `doc_write(mode="append")` cannot grow a document past the limit one
//!   chunk at a time.

use crate::error::{DocError, Result};

/// Worst case JSON string escaping: `c` → `\u000c` (6 bytes per byte).
const JSON_ESCAPE_WORST_CASE: usize = 6;

/// Slack for the rest of the envelope (`jsonrpc`, `id`, `method`, tool name,
/// small argument objects).
const ENVELOPE_SLACK: usize = 64 * 1024;

/// Maximum size of an HTTP request body that may carry `content_max` bytes of
/// document content once JSON-escaped.
pub fn transport_ceiling(content_max: usize) -> usize {
    content_max
        .saturating_mul(JSON_ESCAPE_WORST_CASE)
        .saturating_add(ENVELOPE_SLACK)
}

/// Enforce the document ceiling on the **resulting** document size.
///
/// `size` is the document size that would result from the write — the
/// replacement body for create/overwrite, `current + chunk` for append.
pub fn ensure_document_size(size: usize, max: usize) -> Result<()> {
    if size > max {
        return Err(DocError::PayloadTooLarge {
            size: Some(size),
            max,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn transport_ceiling_covers_worst_case_escaping() {
        // A 2 MiB document whose every byte escapes to 6 bytes still fits.
        assert!(transport_ceiling(2 * MIB) > 6 * 2 * MIB);
        // ... and the ceiling is bounded, not "effectively unlimited".
        assert!(transport_ceiling(2 * MIB) < 6 * 2 * MIB + MIB);
    }

    #[test]
    fn document_exactly_at_limit_is_accepted() {
        assert!(ensure_document_size(2 * MIB, 2 * MIB).is_ok());
    }

    #[test]
    fn one_byte_over_limit_is_payload_too_large() {
        let err = ensure_document_size(2 * MIB + 1, 2 * MIB).unwrap_err();
        assert!(
            matches!(err, DocError::PayloadTooLarge { size: Some(s), max } if s == 2 * MIB + 1 && max == 2 * MIB),
            "got {err:?}"
        );
    }
}