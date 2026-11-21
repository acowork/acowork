//! Content-addressed image parts.
//!
//! An image part is the only payload in history that can be *identical* to an
//! earlier one without being redundant text: `image_read` re-reads the same
//! file at the same `detail` and produces byte-identical bytes, so the model
//! would be charged twice for pixels it already has. This module is the single
//! place that answers "is this payload already in the conversation?" — hashes
//! only, no paths, because the payload is the identity: two different paths to
//! the same PNG are the same image to the provider, and the same path re-encoded
//! at another `detail` is a different one.
//!
//! Why a content address rather than a path or an index: a path is renamed and
//! an index shifts on every compaction. The hash of the delivered bytes survives
//! both, which is what lets the dedupe decision stay correct across a summary.
//!
//! Dedupe is closed-world on purpose: only parts **still live** in history
//! count. A reference to an image that compaction already removed would tell
//! the model to look at something that is no longer above it — the pixel would
//! be gone and the pointer would be a lie.

use std::collections::HashSet;

use acowork_core::providers::traits::{ChatMessage, ContentPart};
use sha2::{Digest, Sha256};

/// The `name` every model-directed image message carries.
///
/// Not a tool name: the message is the Runtime pointing the *model* at a file
/// (`image_read` returned the bytes), so readers use this marker to tell it
/// apart from something the human typed.
pub const TOOL_IMAGE_MESSAGE_NAME: &str = "tool_image";

/// One compact line telling the model what the pictures it just received cost,
/// so the read/no-read and `high`/`low` decision stays with the party that knows
/// what it needs.
///
/// The figure is the same estimate the budget uses (a provider-preprocessed
/// size). It carries two things, and they age differently:
///
/// - **the marginal price of these bytes** — a fact that never changes, so it is
///   always stated;
/// - **how full the conversation is** — true only at the moment of writing. It
///   is stated only from [`SHARE_DISCLOSURE_PERCENT`] upward, where it is about
///   to change the next decision. A percentage baked into the prompt at 12% is
///   a stale claim echoing in the cached prefix for the rest of the session;
///   omitting it below the threshold costs the model nothing it can act on.
///
/// Deliberately plain text, not a banner: it rides inside the image message, so
/// it is part of the append and never a rewrite.
pub fn cost_note(
    projected_tokens: u64,
    budget_tokens: u64,
    image_tokens: u64,
    image_count: usize,
) -> String {
    let images = if image_count == 1 {
        "1 image".to_string()
    } else {
        format!("{image_count} images")
    };
    let mut note = format!("[image cost: {images} ≈ {image_tokens} tokens");
    if budget_tokens > 0 {
        let percent = (projected_tokens as f64 / budget_tokens as f64) * 100.0;
        if percent >= SHARE_DISCLOSURE_PERCENT {
            note.push_str(&format!(
                "; context now {percent:.0}% of the {}k input budget",
                budget_tokens / 1000
            ));
            if percent >= 90.0 {
                note.push_str(" — it will be compacted before the next request");
            }
        }
    }
    note.push_str("; re-reading the same file at the same detail adds nothing]");
    note
}

/// Context share at which the note starts quoting the percentage.
///
/// Below this the number cannot change what the model does with the image, and
/// quoting it would only plant a figure that goes stale inside the cached
/// prefix. Above it, the next read or two is the decision that matters.
pub const SHARE_DISCLOSURE_PERCENT: f64 = 60.0;

///
/// Used by compaction to decide what may not be summarized away: a message with
/// an [`ContentPart::ImageUrl`] is the only place those bytes exist in the
/// session, so removing it turns the model's picture of the file into a path
/// string it has to pay a round-trip to see again.
pub fn carries_image(message: &ChatMessage) -> bool {
    message.content_parts.as_ref().is_some_and(|parts| {
        parts
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))
    })
}

/// The content address of an image part, or `None` for any other part kind.
///
/// Hashes the delivered URL (data URI included) — the same bytes the provider
/// receives, so the address is stable across processes and re-encodes that
/// produce identical output.
pub fn image_part_hash(part: &ContentPart) -> Option<String> {
    let ContentPart::ImageUrl { image_url } = part else {
        return None;
    };
    Some(hex(&Sha256::digest(image_url.url.as_bytes())))
}

/// Content addresses of every image part currently live in `messages`.
///
/// Only image-carrying messages are walked; the caller pays O(history) once per
/// read, which is the same order as the token recount that follows the append.
pub fn live_image_hashes(messages: &[ChatMessage]) -> HashSet<String> {
    messages
        .iter()
        .flat_map(|msg| msg.content_parts.iter().flatten())
        .filter_map(image_part_hash)
        .collect()
}

/// Split `parts` into the ones worth appending and the ones already above.
///
/// Returns `(kept, deduped_count)`. Order is preserved, so a turn that reads the
/// same file twice keeps exactly one copy.
pub fn dedup_parts(parts: Vec<ContentPart>, live: &HashSet<String>) -> (Vec<ContentPart>, usize) {
    let mut seen = live.clone();
    let mut kept = Vec::with_capacity(parts.len());
    let mut deduped = 0usize;
    for part in parts {
        match image_part_hash(&part) {
            Some(hash) => {
                if !seen.insert(hash) {
                    deduped += 1;
                } else {
                    kept.push(part);
                }
            }
            None => kept.push(part),
        }
    }
    (kept, deduped)
}

/// The `<tool_image path="…"/>` labels of every image-carrying message, plus a
/// placeholder for images that have no path to return to.
///
/// Used by compaction to carry references forward: the pixels leave with the
/// middle of the conversation, the *addresses* must not, or the model can no
/// longer tell which file it was looking at. Generated from history rather than
/// from the summarizer's prose, because the transcript the summarizer sees holds
/// these labels verbatim — asking an LLM to transcribe them can only lose rows.
pub fn image_references(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|msg| carries_image(msg))
        .flat_map(message_image_references)
        .collect()
}

/// The reference lines one image-carrying message contributes.
pub fn message_image_references(message: &ChatMessage) -> Vec<String> {
    debug_assert!(carries_image(message), "only image-carrying messages have refs");
    if message.name.as_deref() == Some(TOOL_IMAGE_MESSAGE_NAME) {
        let labels: Vec<String> = message
            .content
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("<tool_image") && !line.contains("note="))
            .map(str::to_string)
            .collect();
        if !labels.is_empty() {
            return labels;
        }
    }
    // A pasted attachment has no path to re-read: the bytes only ever existed in
    // this session, so the honest reference is "ask for it again", not a fake
    // `<tool_image path=…/>` the model would try to open.
    vec![ATTACHED_IMAGE_LOST_NOTE.to_string()]
}

/// The note that replaces an image message's pixels when compaction folds them.
pub const FOLDED_IMAGE_NOTE: &str = "<tool_image note=\"pixels removed by compaction \
     — re-read the path with image_read if you need to see it again\"/>";

/// The note standing in for a pasted attachment whose bytes are gone.
pub const ATTACHED_IMAGE_LOST_NOTE: &str = "<image note=\"an attached image is no longer in \
     context — ask the user to send it again if you need to see it\"/>";

/// Strip the image parts of `message`, keeping its text and recording the trade.
///
/// This is the image half of what `build_level_plan` already does to tool
/// results: a round that survives compaction keeps its *place* in the transcript
/// but not necessarily its payload. The label stays, so the model still knows
/// which file it was looking at and can pay for it again deliberately instead of
/// having every pixel it ever saw pinned in the window forever.
///
/// Returns the number of parts removed. `content_parts` is set to `None` when
/// nothing else survives in it, matching every other part-folding path.
pub fn fold_image_parts(message: &mut ChatMessage) -> usize {
    let Some(parts) = message.content_parts.as_mut() else {
        return 0;
    };
    let before = parts.len();
    parts.retain(|part| !matches!(part, ContentPart::ImageUrl { .. }));
    let removed = before - parts.len();
    if removed == 0 {
        return 0;
    }
    if parts.is_empty() {
        message.content_parts = None;
    }
    if !message.content.contains("<tool_image note=") && !message.content.contains("<image note=") {
        message.content.push('\n');
        message.content.push_str(FOLDED_IMAGE_NOTE);
    }
    removed
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::ImageUrlPart;

    fn image(url: &str) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: url.to_string(),
                detail: Some("high".into()),
                width: Some(1568),
                height: Some(1176),
            },
        }
    }

    fn tool_image(label: &str, parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: acowork_core::providers::traits::MessageRole::User,
            name: Some(TOOL_IMAGE_MESSAGE_NAME.into()),
            content: label.into(),
            content_parts: Some(parts),
            ..Default::default()
        }
    }

    #[test]
    fn the_same_bytes_hash_the_same_whatever_the_path() {
        let a = image("data:image/png;base64,iVBORw0KGgo=");
        let b = image("data:image/png;base64,iVBORw0KGgo=");
        assert_eq!(image_part_hash(&a), image_part_hash(&b));
        assert!(image_part_hash(&a).is_some_and(|h| h.len() == 64));
        // A re-encode at another detail is a different payload, not a duplicate.
        let other = image("data:image/png;base64,iVBORw0KGgoAAAA=");
        assert_ne!(image_part_hash(&a), image_part_hash(&other));
        // Non-image parts have no content address.
        assert_eq!(
            image_part_hash(&ContentPart::Text { text: "hi".into() }),
            None
        );
    }

    #[test]
    fn a_repeat_inside_one_turn_keeps_one_copy() {
        let same = image("data:image/png;base64,AAAA");
        let (kept, deduped) = dedup_parts(vec![same.clone(), same], &HashSet::new());
        assert_eq!(kept.len(), 1);
        assert_eq!(deduped, 1);
    }

    #[test]
    fn dedupe_is_closed_world_on_live_parts() {
        let same = image("data:image/png;base64,AAAA");
        let live = live_image_hashes(&[tool_image(
            "<tool_image path=\"a.png\"/>",
            vec![same.clone()],
        )]);
        let (kept, deduped) = dedup_parts(vec![same.clone()], &live);
        assert!(kept.is_empty(), "the pixels are already above");
        assert_eq!(deduped, 1);

        // Once the earlier message is gone (compaction), the same bytes must be
        // appended again — a reference to a removed image would be a lie.
        let nothing_live = live_image_hashes(&[ChatMessage::user("summary only")]);
        let (kept, deduped) = dedup_parts(vec![same], &nothing_live);
        assert_eq!(kept.len(), 1);
        assert_eq!(deduped, 0);
    }

    #[test]
    fn references_survive_when_pixels_do_not() {
        let messages = vec![
            tool_image(
                "<tool_image tool=\"image_read\" path=\"shot.png\"/>",
                vec![image("data:image/png;base64,AAAA")],
            ),
            // A deduped turn keeps the label but carries no parts: it is not a
            // live reference, and compaction must not claim it is one.
            tool_image(
                "<tool_image tool=\"image_read\" path=\"copy.png\"/>",
                vec![],
            ),
        ];
        assert_eq!(
            image_references(&messages),
            vec!["<tool_image tool=\"image_read\" path=\"shot.png\"/>".to_string()]
        );
    }

    /// The note is the model's only signal that a read is expensive. It has to
    /// quote the same estimate the compaction line uses, or "9% here / 90%
    /// The note is the model's only signal that a read is expensive. It has to
    /// quote the same estimate the compaction line uses, or "9% here / 90%
    /// there" would be two different units.
    #[test]
    fn the_cost_note_names_the_price_and_withholds_a_stale_share() {
        // Far below the line: the price, and nothing that will read as wrong in
        // ten rounds' time.
        let quiet = cost_note(78_200, 218_000, 2_458, 1);
        assert!(quiet.contains("2458 tokens"), "{quiet}");
        assert!(!quiet.contains('%'), "{quiet}");
        assert!(quiet.contains("re-reading the same file at the same detail adds nothing"));

        // Close enough that the next read is the decision: state the share.
        let warm = cost_note(140_000, 218_000, 2_458, 1);
        assert!(warm.contains("64%"), "{warm}");
        assert!(warm.contains("218k"), "{warm}");
        assert!(!warm.contains("compacted"), "{warm}");

        // Plural, and no percentage when there is no budget to take one of.
        let plural = cost_note(1_000, 0, 765, 3);
        assert!(plural.contains("3 images"), "{plural}");
        assert!(plural.contains("765 tokens"), "{plural}");
        assert!(!plural.contains('%'), "{plural}");

        // At the line, say so - that is the round where the pixels get
        // summarized and a re-read buys a cache miss.
        let at_the_line = cost_note(200_000, 218_000, 765, 1);
        assert!(at_the_line.contains("92%"), "{at_the_line}");
        assert!(
            at_the_line.contains("compacted before the next request"),
            "{at_the_line}"
        );
    }
}
