//! `WritePolicy` — the single governance decision for every document mutation.
//!
//! ## Why this is a separate module
//!
//! Governance is **orthogonal** to the kind of write. The caller says *what*
//! it wants (create / overwrite / append / move / remove); this module says
//! *whether it may take effect immediately or must be reviewed by a human*.
//! Encoding the gate in the operation name (`doc_add` = no review,
//! `doc_submit_update` = review) makes the two remaining combinations —
//! "create, but reviewed" and "overwrite, without review" — inexpressible
//! and forces a new verb per combination.
//!
//! The filesystem analogue: a `write(2)` does not change its name when a
//! mandatory-access-control layer is stacked on top — the same `write`
//! either succeeds or returns `EACCES` / blocks. Here the same `doc_write`
//! either applies or returns `status: "pending_review"`.
//!
//! ## The rule (deterministic, no configuration)
//!
//! | Situation | Gate |
//! |-----------|------|
//! | Caller is an admin | `Apply` |
//! | Caller created the document (`creator == actor`) | `Apply` |
//! | Document has no recorded creator (pre-creator-stamp / legacy) | `Review` |
//! | Document belongs to someone else | `Review` |
//!
//! Rationale: publishing and iterating on your own document is the common
//! agent workflow and must not need a human in the loop; mutating a
//! document someone else owns is exactly the shared-state change the review
//! flow (design §5) exists to gate. "No creator recorded" is treated as
//! *shared* — the safe side of the ambiguity.
//!
//! Structural operations (`move`, `remove`) are **not** review-gated: the
//! review flow models a *content proposal* (`UpdateRequest { content }`) and
//! has no way to represent "delete this" or "rename this". They are gated by
//! ownership instead ([`gate_owner`]) — and, for a node with no recorded
//! creator, by its ACL.

use crate::error::{DocError, Result};

/// What may happen to a content write right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Apply immediately; the document version bumps.
    Apply,
    /// Do not touch the document. Record a `pending` update request and
    /// return its id so the caller can poll `doc_check_request`.
    Review,
}

/// Decide the gate for a **content** write (create / overwrite / append).
///
/// `creator` is the document's recorded creator (`None` for documents that
/// predate creator stamping, which are treated as shared).
///
/// Creation is always `Apply` under this rule: there is no prior owner to
/// protect, and the caller becomes the creator, so its subsequent appends
/// stay direct. If a policy ever needs "create, but reviewed", it belongs
/// here — not in a new tool name.
pub fn gate_write(actor: &str, is_admin: bool, creator: Option<&str>) -> WriteGate {
    if is_admin || creator == Some(actor) {
        WriteGate::Apply
    } else {
        WriteGate::Review
    }
}

/// Gate for **structural** operations (move / remove): the document must be
/// the caller's, and there is no review fallback (see the module docs).
///
/// Unlike a content write, an unowned document here is **not** a dead end.
/// "Review" is the safe side for a content proposal because the proposal can
/// be approved by a human; structural operations have no such channel, so
/// ownership-gating an unowned node would mean *nobody* could ever rename or
/// remove it — a permanently stuck node, not a protected one. Instead the
/// caller gets [`OwnerGate::Unowned`] and must fall back to the node's ACL
/// (public by default, tightened when someone set an explicit ACL, admin
/// bypass inside the ACL layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerGate {
    /// `creator == actor`, or the caller is an admin: proceed.
    Owner,
    /// No creator recorded (pre-creator-stamp / imported): the node is
    /// shared, so the ACL decides.
    Unowned,
}

pub fn gate_owner(actor: &str, is_admin: bool, creator: Option<&str>) -> Result<OwnerGate> {
    if is_admin || creator == Some(actor) {
        Ok(OwnerGate::Owner)
    } else if creator.is_none() {
        Ok(OwnerGate::Unowned)
    } else {
        Err(DocError::Forbidden(Some(actor.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "inst-0000-0000-0000-0000-000000000001";
    const OTHER: &str = "inst-0000-0000-0000-0000-000000000002";

    #[test]
    fn owner_writes_directly() {
        assert_eq!(gate_write(AGENT, false, Some(AGENT)), WriteGate::Apply);
    }

    #[test]
    fn foreign_document_requires_review() {
        assert_eq!(gate_write(AGENT, false, Some(OTHER)), WriteGate::Review);
    }

    #[test]
    fn unowned_document_is_treated_as_shared() {
        assert_eq!(gate_write(AGENT, false, None), WriteGate::Review);
    }

    #[test]
    fn admin_bypasses_review() {
        assert_eq!(gate_write(AGENT, true, Some(OTHER)), WriteGate::Apply);
        assert_eq!(gate_write(AGENT, true, None), WriteGate::Apply);
    }

    #[test]
    fn structural_ops_gate_on_ownership() {
        assert_eq!(gate_owner(AGENT, false, Some(AGENT)).unwrap(), OwnerGate::Owner);
        assert_eq!(gate_owner(AGENT, true, Some(OTHER)).unwrap(), OwnerGate::Owner);
        assert!(matches!(
            gate_owner(AGENT, false, Some(OTHER)),
            Err(DocError::Forbidden(_))
        ));
    }

    #[test]
    fn unowned_document_is_shared_not_forbidden() {
        // A pre-creator-stamp / imported document has no owner to protect;
        // structural ops fall through to the ACL rather than becoming
        // unreachable (there is no review channel for move / remove).
        assert_eq!(gate_owner(AGENT, false, None).unwrap(), OwnerGate::Unowned);
    }
}