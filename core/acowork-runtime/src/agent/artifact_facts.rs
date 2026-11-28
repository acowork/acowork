//! The artifact-fact registry: which `metadata` keys describe something a tool
//! *produced*, and the one pure fold the transcript projection uses.
//!
//! ## Why a registry instead of knowledge
//!
//! A `tool_result` row may carry facts about what that call produced: the
//! producing tool (or the per-service adapter that wraps it) stamps them on the
//! row's `metadata` — [`super::turn_deliverables`] for files/images,
//! [`super::doc_deliverables`] for online documents. Those facts are the input
//! of the client's turn-end deliverables banner, which is **frontend business
//! logic** (ADR-119: what counts, how many and how it looks is decided there).
//!
//! This module therefore knows only *key names*, never tools: a fact is a
//! registered metadata key, and forwarding one is a copy, not a judgement. That
//! is what keeps it reusable — a new artifact kind is one entry in
//! [`FACT_KEYS`], and no tool name, product name or MIME type appears here at
//! all (the ADR-119 red line enforces this file too).
//!
//! ## Why the projection forwards them
//!
//! `GET /sessions/{sid}/messages?groups=stub` folds every contiguous
//! thought / tool_call / tool_result run into ONE `explore_stub` row. The rows
//! that carried the facts disappear; if the facts disappear with them, a
//! reloaded session reconstructs differently from the turn that was streamed
//! live — every historical turn silently loses its deliverables row. So the fold
//! carries them across ([`fold_run`]), verbatim.
//!
//! Nothing is persisted and no second source of truth appears: the projection
//! runs on every read, over the rows it already parsed, so the stored
//! `tool_result` metadata stays the only author of the fact (and an old session
//! gains the banner the moment it is read, with no migration).
//!
//! ## Deliberately NOT here
//!
//! No dedupe, no caps, no `created` filtering, no "is this displayable". Those
//! are display decisions and they live in the client's derivation
//! (`deriveDeliverables.ts`), because the live path and the projected path must
//! run through *one* policy — two policies would drift into two banners.

use serde_json::{Map, Value};

/// The registered artifact-fact keys.
///
/// **Adding a kind of produced artifact = one entry here.** Each entry is the
/// `metadata` key a producer stamps; the value is forwarded untouched, so its
/// inner shape stays the producer's contract with whoever reads it (today: the
/// Desktop, `deliverableFiles.ts` / `deliverableDocs.ts`). A reader that does
/// not know a key ignores it, which is what makes this list forward-compatible
/// across a Runtime/Desktop version skew.
pub const FACT_KEYS: &[&str] = &[
    // File/image coordinates: `[{workspace_id, rel_path, mime, bytes, created}]`
    // (`super::turn_deliverables`).
    "produced",
    // One online document: `{doc_id, path, name, version}`
    // (`super::doc_deliverables`).
    "doc",
];

/// The pairing keys every tool row carries. They belong to the detail read
/// (`/messages/detail`, paired by `tool_call_id`), never to a turn's artifacts —
/// forwarding them would add N copies of a key nobody reads. They are named
/// here only so a test can assert the registry never swallows one (the table is
/// the entry point for new fact kinds; this is the invariant it must keep).
pub const PAIRING_KEYS: &[&str] = &["tool_name", "tool_call_id"];

/// The artifact facts carried by ONE row's metadata, or `None`.
///
/// Only registered keys are copied; an unregistered key is somebody else's
/// metadata and stays on the row. `null` values are dropped so the projection
/// never invents an empty entry.
pub fn fact_row(metadata: &Value) -> Option<Value> {
    let obj = metadata.as_object()?;
    let mut out = Map::new();
    for key in FACT_KEYS {
        match obj.get(*key) {
            Some(value) if !value.is_null() => {
                out.insert((*key).to_string(), value.clone());
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(Value::Object(out))
}

/// Fold a run's rows into the fact array the stub forwards, in log order.
///
/// Empty when nothing in the run produced an artifact — the caller then omits
/// the key entirely, so a plain run costs exactly what it cost before the key
/// existed.
pub fn fold_run<'a>(metadatas: impl IntoIterator<Item = Option<&'a Value>>) -> Vec<Value> {
    metadatas.into_iter().flatten().filter_map(fact_row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forwards_registered_keys_and_drops_pairing_keys() {
        let doc = json!({"doc_id": "d1", "path": "研发/纪要.md", "name": "纪要", "version": 2});
        let meta = json!({
            "tool_name": "whatever_the_server_is_called",
            "tool_call_id": "c1",
            "doc": doc,
        });
        let row = fact_row(&meta).expect("a stamped doc is a fact");
        assert_eq!(row["doc"], doc);
        // The pairing keys are the detail read's business, not the banner's.
        assert!(row.get("tool_name").is_none());
        assert!(row.get("tool_call_id").is_none());
    }

    #[test]
    fn a_pairing_only_row_carries_no_fact() {
        assert!(fact_row(&json!({"tool_name": "bash", "tool_call_id": "c1"})).is_none());
        assert!(fact_row(&json!({})).is_none());
    }

    #[test]
    fn an_unregistered_key_is_not_an_artifact() {
        // The registry is the whole policy: prose, arguments, an error blob —
        // anything a producer happens to stamp is NOT forwarded, so the stub
        // keeps costing what the fold promised.
        let meta = json!({"tool_name": "x", "tool_call_id": "c1", "stdout": "a very long log"});
        assert!(fact_row(&meta).is_none());
    }

    #[test]
    fn null_values_are_not_facts() {
        assert!(fact_row(&json!({"tool_name": "x", "tool_call_id": "c1", "doc": null})).is_none());
    }

    #[test]
    fn fold_run_keeps_log_order_and_skips_silent_rows() {
        let produced = json!([{"workspace_id": "ws", "rel_path": "a.md", "mime": "text/markdown"}]);
        let doc = json!({"doc_id": "d1", "path": "a.md"});
        let rows = [
            Some(json!({"tool_name": "t", "tool_call_id": "c1", "produced": produced})),
            None,
            Some(json!({"tool_name": "t", "tool_call_id": "c2"})),
            Some(json!({"tool_name": "t", "tool_call_id": "c3", "doc": doc})),
        ];
        let facts = fold_run(rows.iter().map(|r| r.as_ref()));
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0]["produced"], produced);
        assert_eq!(facts[1]["doc"], doc);
    }

    #[test]
    fn a_run_that_produced_nothing_folds_to_empty() {
        let rows = [Some(json!({"tool_name": "bash", "tool_call_id": "c1"}))];
        assert!(fold_run(rows.iter().map(|r| r.as_ref())).is_empty());
    }

    #[test]
    fn the_registry_never_swallows_a_pairing_key() {
        // `fact_row` copies registered keys blindly, so "FACT_KEYS must not
        // contain a pairing key" is an invariant of the TABLE, not something the
        // function should paper over at runtime: a registered pairing key would
        // ship two meanings for one name, and a client reading `tool_call_id`
        // as a fact is a bug no defensive `remove` would fix.
        for key in FACT_KEYS {
            assert!(
                !PAIRING_KEYS.contains(key),
                "FACT_KEYS lists the pairing key {key}; the two sets must be disjoint"
            );
        }
    }

    #[test]
    fn no_registered_key_names_a_tool() {
        // ADR-119 §Falsifiability, applied to this table: the registry may name
        // metadata keys, never a tool, a product or an MCP server.
        for key in FACT_KEYS {
            let lowered = key.to_ascii_lowercase();
            for forbidden in ["mcp_", "doc_write", "doc_move", "file_write", "browser", "bash"] {
                assert!(
                    !lowered.contains(forbidden),
                    "FACT_KEYS names a producer ({key}); the registry must stay tool-agnostic"
                );
            }
        }
    }
}