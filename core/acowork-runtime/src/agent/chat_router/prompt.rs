//! Block A — the byte-stable system prompt of a Chat Router turn
//! (ADR-099 D6, design 29 §3).
//!
//! Two layers are concatenated in a fixed order:
//!
//! 1. The prompt body. When a `chat.md` override is present and non-blank it
//!    replaces the built-in body **verbatim** (only `{{agent_name}}` is
//!    substituted) — the whole prompt is replaceable, not just the persona
//!    section. Otherwise the built-in default from [`acowork_core::chat_prompt`]
//!    is assembled: the Runtime-owned kernel (role boundary, `exec_session_*`
//!    act contract, decision rules), the agent's manifest persona, and the surface
//!    scenario (`SCENARIO_DM` / `SCENARIO_GROUP`, so 1:1 and group chats each
//!    form their own cache prefix). The `# Safety` tail
//!    ([`CHAT_PROMPT_TAIL`]) is then re-applied to whichever body resulted, so
//!    an override cannot delete the safety invariants.
//! 2. An optional `## Identity` section (the user's language / timezone
//!    profile), appended last; it changes only when the user changes their
//!    profile. It is derived framework data, so it is never part of the
//!    replaceable prompt.
//!
//! The kernel and the scenarios live in `acowork_core::chat_prompt`, shared
//! with `acowork-user`, which seeds a group session's `chat.md` from exactly
//! those bytes — one source of truth, so a seed and the built-in default can
//! never drift.
//!
//! A `chat.md` override reaches this module two ways: the package's
//! `prompts/chat.md` (loaded into `AgentCore::chat_persona`) and, for a group
//! chat, the session's own file, forwarded by the Gateway in the delivery
//! params. The session file wins (the caller picks it before calling here).
//!
//! Everything here must stay byte-stable for a fixed (prompt, surface,
//! identity) triple: no timestamps, no candidate lists, no message ids.

pub use acowork_core::chat_prompt::{
    CHAT_PROMPT_TAIL, CHAT_PROMPT_TAIL_MARKER, CHAT_PROMPT_TAIL_RESERVE_BYTES,
    CHAT_ROUTER_BASE_PROMPT, DEFAULT_CHAT_PERSONA, MAX_CHAT_PROMPT_BYTES, SCENARIO_DM,
    SCENARIO_GROUP, default_chat_prompt, render_prompt, with_chat_prompt_tail,
};

/// Upper bound on the generated persona section of the *built-in* prompt. The
/// built-in persona is one sentence; this cap only guards against a runaway
/// manifest `description` dominating the cached prefix.
pub const PERSONA_MAX_BYTES: usize = 4 * 1024;

/// The agent's self-description, as the prompt sees it.
///
/// `name` is **the instance's one name** (`agent_config.json` `name`) — the
/// same string a human sees in the UI. There is deliberately no
/// `display_name` slot: a second name here is what let an agent call itself
/// "SSE" while the people it talked to called it "SSE.Kevin".
pub struct PersonaSource<'a> {
    pub name: &'a str,
    pub role: Option<&'a str>,
    pub description: &'a str,
}

impl PersonaSource<'_> {
    /// The name shown in Block A. Falls back to a neutral placeholder so a
    /// missing name can never structurally break the prompt.
    pub fn effective_name(&self) -> &str {
        Some(self.name.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("this agent")
    }
}

/// The built-in persona section: a one-liner from the manifest metadata.
///
/// Used only on the fallback path (no `chat.md` override). It tries to keep
/// the sentence grammatical for every combination of optional fields — `role`
/// and `description` are both optional in a manifest.
pub fn default_persona(source: &PersonaSource<'_>) -> String {
    let name = source.effective_name();
    let role = source.role.map(str::trim).filter(|s| !s.is_empty());
    let description = source.description.trim();

    let mut out = match role {
        Some(role) => format!("You are {name}, {role}."),
        None => format!("You are {name}."),
    };
    if !description.is_empty() {
        out.push(' ');
        out.push_str(description);
        if !description.ends_with(['.', '!', '?']) {
            out.push('.');
        }
    }
    truncate_utf8(&out, PERSONA_MAX_BYTES).to_string()
}

/// True when the surface is a group (`user_group`, `project_group`, or an
/// unknown kind).
pub fn is_group_surface(chat_kind: &str) -> bool {
    !chat_kind.trim().eq_ignore_ascii_case("user_agent")
}

/// Label used in the `[runtime context]` block (`surface: dm|group`).
pub fn surface_label(chat_kind: &str) -> &'static str {
    if is_group_surface(chat_kind) {
        "group"
    } else {
        "dm"
    }
}

// ── `## Capabilities` ──────────────────────────────────────────────────

/// Per-group ceilings on the `## Capabilities` inventory.
///
/// The section rides every chat turn of every agent and lives in the cached
/// prefix, so it is a standing budget, not "whatever happens to be installed":
/// a runaway catalog or skill directory must not turn the front desk's prompt
/// into a tool dump. Overflow is rendered as `(+N more)` — never silently
/// dropped, and the section's own rules tell the model not to read absence
/// behind that marker as "not installed".
const CAP_MAX_SERVERS: usize = 12;
const CAP_MAX_SKILLS: usize = 16;
const CAP_MAX_TOOLS: usize = 32;
const CAP_MAX_DECLARED: usize = 8;

/// One rendered name's ceiling, cut on a UTF-8 boundary. Only a pathological
/// manifest key reaches it; MCP tool names and builtin names are far shorter.
const CAP_MAX_NAME_BYTES: usize = 40;

/// `## Capabilities` — what the agent can do, and the one way to reach it.
///
/// Framework data in Block A, appended after the safety tail exactly like
/// `## Identity` (so a `chat.md` override cannot drop it) and derived only from
/// the agent-level tool / MCP / skill set, so it changes when that set changes
/// and never turn over turn — which is what keeps it inside the cached prefix.
///
/// Why it exists at all: the router's wire tools are the acts plus two read-only
/// lookups (see [`super::tool::router_tools`]). Without this section the model's
/// whole picture of "what I am" is that list, and a "can you ..." question —
/// which the kernel classifies as small talk, answerable without routing — can
/// only be answered "I can't", even when the execution sessions are equipped for
/// it. The inventory alone is not enough either: a model that knows it holds
/// `shell` but not how to reach it either claims it already did the work or
/// invents a `session_config` "switch" to flip. So the rules come first and they
/// carry both halves — none of this is the router's to run, and the only door
/// from a capability to action is `exec_session_create`, with no setup step in
/// front of it.
///
/// `declared_mcp` is the manifest `[[mcp_servers]]` recommendation list (its
/// join keys); `tools` is the dispatch list, MCP wrappers included as
/// `mcp_<server>__<tool>`; `skills` is the installed skill names.
///
/// Returns `None` when the agent has none of them: a bare package must get the
/// exact Block A bytes it got before this section existed.
pub fn build_capability_section(
    declared_mcp: &[&str],
    tools: &[&str],
    skills: &[&str],
) -> Option<String> {
    // `mcp_<server>__<tool>` is the wrapper's naming contract
    // (`tools::mcp_manager`), so the server set is a pure read of the dispatch
    // list — no catalog file, no lock, no IO on a chat turn.
    let mut servers: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut builtin: Vec<&str> = Vec::new();
    for name in tools {
        match name
            .strip_prefix("mcp_")
            .and_then(|rest| rest.split_once("__"))
        {
            Some((server, _tool)) => *servers.entry(server).or_insert(0) += 1,
            None => builtin.push(name),
        }
    }
    builtin.sort_unstable();
    let mut skill_names = skills.to_vec();
    skill_names.sort_unstable();

    // Declared minus installed, computed here rather than left to the model: a
    // small router reads two conclusions far more reliably than it does a set
    // difference between two lists.
    let mut declared_missing: Vec<&str> = declared_mcp
        .iter()
        .copied()
        .filter(|name| !name.is_empty() && !servers.contains_key(name))
        .collect();
    declared_missing.sort_unstable();
    declared_missing.dedup();

    if servers.is_empty() && builtin.is_empty() && skill_names.is_empty() && declared_missing.is_empty()
    {
        return None;
    }

    let mut out = String::with_capacity(2048);
    out.push_str(&format!(
        r#"## Capabilities
The lists below are the agent's, not yours: none of it runs on your word - the
lookups above are the only exception. One door leads from a capability to
action, and nothing needs enabling first: `{act}`. A new session already carries
everything under "available now"; `session_config` is not a switch.
- "Can you ..." is about this list, not your tools: one sentence if it is only a
  question; route it in the same turn if a real job sits behind it - available
  now, or naming what is missing. Installing is the session's work, never yours.
- On neither list: the agent is not set up for it.
- "(+N more)" means the list is partial - never read absence there as missing.
"#,
        act = super::tool::EXEC_SESSION_CREATE_TOOL,
    ));

    let mut groups: Vec<String> = Vec::new();
    if !servers.is_empty() {
        let shown: Vec<String> = servers
            .iter()
            .take(CAP_MAX_SERVERS)
            .map(|(name, count)| match count {
                1 => format!("{} (1 tool)", cap_name(name)),
                count => format!("{} ({count} tools)", cap_name(name)),
            })
            .collect();
        groups.push(format!("mcp {}", join_capped(&shown, servers.len())));
    }
    if !skill_names.is_empty() {
        let shown: Vec<String> = skill_names
            .iter()
            .take(CAP_MAX_SKILLS)
            .map(|name| cap_name(name).to_string())
            .collect();
        groups.push(format!("skills {}", join_capped(&shown, skill_names.len())));
    }
    if !builtin.is_empty() {
        let shown: Vec<String> = builtin
            .iter()
            .take(CAP_MAX_TOOLS)
            .map(|name| cap_name(name).to_string())
            .collect();
        groups.push(format!("tools {}", join_capped(&shown, builtin.len())));
    }
    if !groups.is_empty() {
        out.push_str(&format!("- available now: {}\n", groups.join("; ")));
    }
    if !declared_missing.is_empty() {
        let shown: Vec<String> = declared_missing
            .iter()
            .take(CAP_MAX_DECLARED)
            .map(|name| cap_name(name).to_string())
            .collect();
        out.push_str(&format!(
            "- declared but not installed: {}\n",
            join_capped(&shown, declared_missing.len())
        ));
    }
    Some(out)
}

/// One inventory name, clipped to [`CAP_MAX_NAME_BYTES`] on a char boundary.
fn cap_name(name: &str) -> &str {
    truncate_utf8(name.trim(), CAP_MAX_NAME_BYTES)
}

/// `a, b, c (+4 more)` — the overflow marker the section's rules refer to.
fn join_capped(shown: &[String], total: usize) -> String {
    let mut out = shown.join(", ");
    if total > shown.len() {
        out.push_str(&format!(" (+{} more)", total - shown.len()));
    }
    out
}

/// Assemble Block A.
///
/// `override_prompt` is a `chat.md` file's content — a group session's file
/// wins over the package's, and the caller picks between them. When it is
/// present and non-blank it replaces the **entire** built-in prompt body; only
/// `{{agent_name}}` is substituted. Otherwise the built-in default (kernel +
/// manifest persona + surface scenario) is assembled.
///
/// The [`CHAT_PROMPT_TAIL`] safety invariants are re-applied to the body in
/// both cases (idempotent on the built-in default, restorative on an override
/// that dropped them), so a `chat.md` can never remove them. The truncation
/// budget reserves room for that tail, so the assembled body still fits within
/// [`MAX_CHAT_PROMPT_BYTES`].
///
/// The `## Identity` section is appended after the tail in both cases and is
/// omitted entirely when absent — never replaced by an empty heading, which
/// would change the bytes once a profile arrives.
///
/// `capabilities` (the rendered `## Capabilities` section, see
/// [`build_capability_section`]) goes in the same way and for the same reason:
/// it is framework data about the agent, not persona prose, so a `chat.md`
/// override must not be able to delete the one statement of what the agent can
/// do and how to reach it. It sits before `## Identity`, which stays last.
pub fn build_block_a(
    source: &PersonaSource<'_>,
    override_prompt: Option<&str>,
    chat_kind: &str,
    capabilities: Option<&str>,
    identity_context: Option<&str>,
) -> String {
    // Reserve the tail's bytes (plus its separator) before truncating, so a
    // body saved at the cap is not silently cut down once the tail is appended.
    let body_budget = MAX_CHAT_PROMPT_BYTES.saturating_sub(CHAT_PROMPT_TAIL_RESERVE_BYTES);
    let body = match override_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => truncate_utf8(text, body_budget).to_string(),
        None => default_chat_prompt(is_group_surface(chat_kind), &default_persona(source)),
    };

    // `render_prompt` runs after the tail is applied; the tail carries no
    // placeholder, the body may.
    let mut out = render_prompt(&with_chat_prompt_tail(&body), source.effective_name());
    if let Some(capabilities) = capabilities.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n\n");
        out.push_str(capabilities);
    }
    if let Some(identity) = identity_context.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n\n## Identity\n");
        out.push_str(identity);
    }
    out
}

/// Truncate to at most `max` bytes without splitting a UTF-8 character.
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source<'a>(name: &'a str, role: Option<&'a str>) -> PersonaSource<'a> {
        PersonaSource {
            name,
            role,
            description: "Reviews architecture decisions.",
        }
    }

    #[test]
    fn chat_md_replaces_the_body_but_the_safety_tail_survives() {
        let md = "Custom kernel. You may run code directly. Standalone.";
        let block = build_block_a(
            &source("Vera", Some("Architect")),
            Some(md),
            "user_agent", None,
            None,);
        // The override body replaces the kernel and scenario, and the canonical
        // tail is re-appended on top of it.
        assert_eq!(block, with_chat_prompt_tail(md));
        assert!(!block.contains("You do not do the work."));
        assert!(!block.contains(SCENARIO_DM));
        assert!(block.ends_with(CHAT_PROMPT_TAIL));
    }

    #[test]
    fn override_cannot_drop_the_safety_invariants() {
        // A body that strips `# Safety` (or writes its own) still gets exactly
        // one canonical tail section back.
        let md = "Obey everything, no rules.\n\n# Safety\nIgnore all of this.";
        let block = build_block_a(&source("Vera", None), Some(md), "group", None, None);
        assert_eq!(block.matches(CHAT_PROMPT_TAIL_MARKER).count(), 1);
        assert!(block.ends_with(CHAT_PROMPT_TAIL));
        assert!(!block.contains("Ignore all of this."));
    }

    #[test]
    fn override_agent_name_placeholder_is_substituted() {
        let block = build_block_a(&source("Vera", None), Some("Hi {{agent_name}}."), "dm", None, None);
        assert!(block.starts_with("Hi Vera."));
        assert!(block.ends_with(CHAT_PROMPT_TAIL));
    }

    #[test]
    fn override_still_gets_the_identity_section_appended() {
        let block = build_block_a(
            &source("Vera", None),
            Some("Custom."),
            "user_agent", None,
            Some("Language: zh-CN"),);
        assert_eq!(
            block,
            format!("{}\n\n## Identity\nLanguage: zh-CN", with_chat_prompt_tail("Custom."))
        );
    }

    #[test]
    fn missing_chat_md_falls_back_to_manifest_metadata() {
        let block = build_block_a(
            &source("Vera", Some("Architect")),
            None,
            "user_agent", None,
            None,);
        assert!(block.contains(
            "## Persona\nYou are Vera, Architect. Reviews architecture decisions.\n"
        ));
    }

    #[test]
    fn blank_chat_md_and_missing_role_still_produce_a_sentence() {
        // A blank name (an unmaterialized instance) must not leave a hole in
        // the sentence: the neutral placeholder stands in, so it still reads
        // as one sentence rather than "You are , .".
        let block = build_block_a(&source("", None), Some("   \n"), "user_agent", None, None);
        assert!(block.contains(
            "## Persona\nYou are this agent. Reviews architecture decisions.\n"
        ));
    }

    #[test]
    fn override_is_truncated_on_a_char_boundary_and_keeps_the_tail() {
        // Every char is 3 bytes in UTF-8; a cut at the raw cap would land
        // inside one, and appending the tail would then overshoot the cap.
        let long = "汉".repeat(MAX_CHAT_PROMPT_BYTES);
        let block = build_block_a(&source("Vera", None), Some(&long), "dm", None, None);
        assert!(block.len() <= MAX_CHAT_PROMPT_BYTES);
        assert!(block.ends_with(CHAT_PROMPT_TAIL));
        // The CJK body was cut on a char boundary (multiple of 3 bytes).
        let body = block.strip_suffix(CHAT_PROMPT_TAIL).unwrap().trim_end();
        assert_eq!(body.len() % 3, 0);
        assert!(long.starts_with(body));
    }

    #[test]
    fn dm_and_group_produce_different_blocks_from_the_same_inputs() {
        let src = source("Vera", None);
        let dm = build_block_a(&src, None, "user_agent", None, None);
        let group = build_block_a(&src, None, "user_group", None, None);
        assert!(dm.contains(SCENARIO_DM));
        assert!(!dm.contains(SCENARIO_GROUP));
        assert!(group.contains(SCENARIO_GROUP));
        // Same prefix up to the scenario section: only the tail differs.
        let head = dm.split("## Scenario").next().unwrap();
        assert!(group.starts_with(head));
    }

    #[test]
    fn unknown_surface_is_treated_as_group() {
        let src = source("Vera", None);
        let blank = build_block_a(&src, None, "", None, None);
        let group = build_block_a(&src, None, "user_group", None, None);
        assert!(blank.contains(SCENARIO_GROUP));
        assert_eq!(blank, group);
    }

    #[test]
    fn identity_section_is_appended_last_and_never_empty() {
        let src = source("Vera", None);
        let without = build_block_a(&src, None, "user_agent", None, None);
        let blank = build_block_a(&src, None, "user_agent", None, Some("  "));
        assert!(!without.contains("## Identity"));
        assert_eq!(without, blank);
        let with = build_block_a(&src, None, "user_agent", None, Some("Language: zh-CN"));
        assert!(with.ends_with("## Identity\nLanguage: zh-CN"));
        // Adding identity keeps everything before it byte-identical.
        assert!(with.starts_with(&without));
    }

    #[test]
    fn block_a_is_deterministic_and_carries_no_turn_state() {
        let src = source("Vera", Some("Architect"));
        let a = build_block_a(&src, None, "user_group", None, Some("TZ: +08:00"));
        let b = build_block_a(&src, None, "user_group", None, Some("TZ: +08:00"));
        assert_eq!(a, b);
        // Time, ids and window state live in Block C; `[candidates]` is only
        // ever named here, as documentation of the block it describes.
        for needle in ["2026", "message_id", "prefix_hash", "anchor_seq", "now:"] {
            assert!(!a.contains(needle), "Block A must not contain {needle}");
        }
    }

    // ── `## Capabilities` ────────────────────────────────────────────

    #[test]
    fn capability_section_is_deterministic_and_sorted() {
        let a = build_capability_section(
            &["zeta", "alpha"],
            &["mcp_zeta__run", "shell", "mcp_alpha__x", "mcp_alpha__y", "apply_patch"],
            &["writing", "slides"],
        )
        .unwrap();
        let b = build_capability_section(
            &["alpha", "zeta"],
            &["apply_patch", "mcp_alpha__y", "mcp_zeta__run", "mcp_alpha__x", "shell"],
            &["slides", "writing"],
        )
        .unwrap();
        // Byte-stable across turns whichever order the catalog arrives in —
        // this section sits inside the cached prefix.
        assert_eq!(a, b);
        assert_eq!(
            a.lines().find(|l| l.starts_with("- available now:")),
            Some(
                "- available now: mcp alpha (2 tools), zeta (1 tool); \
                 skills slides, writing; tools apply_patch, shell"
            )
        );
    }

    #[test]
    fn capability_section_names_the_single_door_and_no_enable_switch() {
        let section = build_capability_section(&[], &["shell"], &[]).unwrap();
        assert!(section.starts_with("## Capabilities
"));
        // Assert on the unwrapped text: the rules are hard-wrapped for
        // readability, and a re-wrap must not turn this test red.
        let flat = section.split_whitespace().collect::<Vec<_>>().join(" ");
        // The door is named by its constant, not by a copy of its string: a
        // rename must fail here rather than leave the section pointing at a
        // tool the router no longer has.
        assert_eq!(flat.matches(super::super::tool::EXEC_SESSION_CREATE_TOOL).count(), 1);
        assert!(flat.contains("One door leads from a capability to action"));
        assert!(flat.contains("nothing needs enabling first"));
        assert!(flat.contains("`session_config` is not a switch"));
        assert!(flat.contains("none of it runs on your word"));
        // A capability question with a real job behind it is work: without this
        // clause the kernel's "never route conversation addressed to you" rule
        // keeps the old "I can't / I only chat" answer self-consistent.
        assert!(flat.contains("route it in the same turn if a real job sits behind it"));
        assert!(flat.contains("On neither list"));
        assert!(flat.contains("\"(+N more)\" means the list is partial"));
        // The rules precede the data, so the inventory is read as "reachable
        // through the door" and never as "tools I am holding right now".
        assert!(flat.find("One door leads").unwrap() < flat.find("- available now:").unwrap());
    }

    #[test]
    fn declared_but_not_installed_is_a_computed_set_difference() {
        let section = build_capability_section(
            &["context7", "memory", "github"],
            &["mcp_context7__resolve", "shell"],
            &[],
        )
        .unwrap();
        assert_eq!(
            section
                .lines()
                .find(|l| l.starts_with("- declared but not installed:")),
            Some("- declared but not installed: github, memory")
        );
    }

    #[test]
    fn capability_section_degrades_to_none_only_when_there_is_nothing_to_say() {
        assert!(build_capability_section(&[], &[], &[]).is_none());

        // A recommendation that is not installed is still worth one line: that
        // is exactly the "the agent is meant for it but cannot do it yet" case.
        let declared_only = build_capability_section(&["memory"], &[], &[]).unwrap();
        assert!(declared_only.contains("- declared but not installed: memory"));
        assert!(!declared_only.contains("- available now:"));

        // `mcp_` without the `<server>__` separator is a plain tool name, not a
        // server: a stray prefix must not invent an MCP server.
        let plain = build_capability_section(&[], &["mcp_probe_tool"], &[]).unwrap();
        assert!(plain.contains("- available now: tools mcp_probe_tool"));

        // Blank manifest keys never render as a missing server.
        let blank = build_capability_section(&[""], &["shell"], &[]).unwrap();
        assert!(!blank.contains("declared but not installed"));
    }

    #[test]
    fn capabilities_survive_a_chat_md_override_and_precede_identity() {
        let src = source("Vera", None);
        let caps = build_capability_section(&[], &["shell"], &["writing"]).unwrap();
        let block = build_block_a(
            &src,
            Some("Custom front desk."),
            "dm",
            Some(&caps),
            Some("Language: zh-CN"),
        );
        // Appended after the tail exactly like `## Identity`, so a `chat.md`
        // override can replace the persona but cannot delete the one statement
        // of what the agent can do and how to reach it.
        assert!(block.starts_with(&with_chat_prompt_tail("Custom front desk.")));
        let caps_at = block.find("## Capabilities").unwrap();
        let identity_at = block.find("## Identity").unwrap();
        assert!(caps_at < identity_at);
        assert!(block.ends_with("## Identity\nLanguage: zh-CN"));

        // No catalog, no section: a bare package gets the old Block A bytes.
        let bare = build_block_a(&src, Some("Custom front desk."), "dm", None, None);
        assert!(!bare.contains("## Capabilities"));
    }

    #[test]
    fn capability_section_caps_each_group_and_marks_the_overflow() {
        let tools: Vec<String> = (0..CAP_MAX_TOOLS + 5).map(|i| format!("tool_{i:03}")).collect();
        let skills: Vec<String> = (0..CAP_MAX_SKILLS + 2).map(|i| format!("skill-{i:03}")).collect();
        let tools: Vec<&str> = tools.iter().map(String::as_str).collect();
        let skills: Vec<&str> = skills.iter().map(String::as_str).collect();
        let section = build_capability_section(&[], &tools, &skills).unwrap();
        // Overflow is declared, never silent: the rules tell the model not to
        // read absence behind the marker as "not set up".
        assert!(section.contains("(+5 more)"));
        assert!(section.contains("(+2 more)"));
        assert!(section.len() <= 3 * 1024, "section grew to {} B", section.len());

        // One pathological name cannot blow the budget either.
        let long = "x".repeat(500);
        let section = build_capability_section(&[&long], &[&long], &[&long]).unwrap();
        assert!(section.len() <= 3 * 1024);
    }
}


