//! Intent delivery gating — the pure accept/reject decisions behind the
//! HTTP delivery entry ([`crate::http::intent_api`]).
//!
//! Kept out of the `http/` layer on purpose: identity attribution (D4),
//! manifest permission checks (D8) and repo format validation (PR-D) are
//! domain rules, not transport plumbing. Each is a pure function so the
//! gating matrix is testable without spinning up state (see
//! `tests/intent_delivery.rs`).

use crate::gateway::state::AgentInfo;

/// Pure attribution logic (D4), split out for testing:
/// `caller_node` = node resolved from the presented node token (None =
/// no token / unenrolled), `sender_status` = the retained AgentStatus
/// entry for the claimed sender (None = never seen online).
pub fn attribute_sender(
    caller_node: Option<String>,
    sender_status: Option<&crate::mqtt::agent_registry::AgentOnlineState>,
) -> Result<(), String> {
    match caller_node {
        Some(node_id) => match sender_status {
            Some(st) if st.node_id == node_id => Ok(()),
            Some(st) => Err(format!(
                "intent sender is hosted on node {}, not the caller's node",
                st.node_id
            )),
            None => Err("intent sender is not online — cannot attribute".to_string()),
        },
        // Local mode / unenrolled caller: require the sender to be an
        // online instance (an offline claim can't be session-verified).
        None => {
            if sender_status.is_some() {
                Ok(())
            } else {
                Err("intent sender is not online — cannot attribute".to_string())
            }
        }
    }
}

/// D8 static check against the two manifests. Returns Some(reason) when
/// the declared permissions do not cover this send.
pub fn permission_denial(sender: &AgentInfo, target: &AgentInfo) -> Option<String> {
    use acowork_core::Permission;

    let sender_ok = sender.manifest.permissions.iter().any(|p| match p {
        Permission::IntentSend(None) => true,
        Permission::IntentSend(Some(scope)) => {
            scope == &target.manifest.agent_id || scope == &target.instance_id
        }
        _ => false,
    });
    if !sender_ok {
        return Some(format!(
            "sender manifest lacks intent:send for target {}",
            target.instance_id
        ));
    }
    let target_ok = target.manifest.permissions.iter().any(|p| match p {
        Permission::IntentReceive(None) => true,
        Permission::IntentReceive(Some(scope)) => {
            scope == &sender.manifest.agent_id || scope == &sender.instance_id
        }
        _ => false,
    });
    if !target_ok {
        return Some(format!(
            "target manifest lacks intent:receive for sender {}",
            sender.instance_id
        ));
    }
    None
}

/// PR-D repo validation — **format only**. The repo is a remote URL, not
/// a path: existence and authorized-directory boundaries are enforced on
/// the target side at execution time (design 27 §4.1 — the protocol
/// carries "which repo this task is about", the receiving model decides
/// how to use it within its registered workspaces). The Gateway must not
/// touch the sender's node-local workspace to "verify" the claim: that is
/// the ADR-009 bug class (silently right on one machine, wrong the moment
/// Gateway and Node are split) and §4.1 explicitly rejects a Gateway-side
/// repo→workspace binding table. No branch field.
pub fn validate_repo(repo: &str) -> Result<(), String> {
    let trimmed = repo.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err("repo must be a non-empty git URL (<= 512 chars)".into());
    }
    let looks_like_url = trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://");
    if !looks_like_url {
        return Err("repo must be an https/ssh git URL".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_format_gate() {
        assert!(validate_repo("https://github.com/o/p").is_ok());
        assert!(validate_repo("git@github.com:o/p.git").is_ok());
        assert!(validate_repo("ssh://git@host/p").is_ok());
        assert!(validate_repo("").is_err(), "empty");
        assert!(validate_repo("   ").is_err(), "whitespace only");
        assert!(
            validate_repo("C:/Users/dev/project").is_err(),
            "local paths must not ride the wire (design 27 §4.1)"
        );
        assert!(validate_repo("https://example.com/p").is_ok());
        assert!(validate_repo(&format!("https://x/{}", "p".repeat(600))).is_err(), "length cap");
    }
}
