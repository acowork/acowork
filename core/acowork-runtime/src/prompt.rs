//! Centralized prompt constants for the ACowork Agent Runtime.
//!
//! All hardcoded prompt strings that appear in production code should be
//! defined here as named constants to ensure consistency and ease of maintenance.

/// Default system prompt when no prompt files are found in the package.
pub const PROMPT_BUILDER_FALLBACK: &str = "You are a helpful AI assistant.";

/// System prompt used for context compaction via LLM.
/// Replaces the agent's full system prompt during compaction to ensure
/// the LLM focuses on summarization rather than tool usage.
///
/// Carries the *task* and *output format* (the role alone was too thin —
/// small/long-context models would echo the input's `[User]:` / `[Tool(...)]:`
/// role labels into the summary instead of writing prose). The user prompt
/// ([`COMPACT_PROMPT`]) provides the conversation body and a worked example
/// as reinforcement.
///
/// ADR-061 §8.1 (triples-removed): the output MUST contain exactly two
/// blocks in this order — `<summary>`, `<user_intent>` — with nothing
/// outside them. `<user_intent>` is the mandatory intent-preservation
/// block: every original user intent and explicit constraint must be
/// listed even if already satisfied, because user messages are the only
/// hard constraints the LLM cannot re-derive after compaction (ADR-061 §3.4).
///
/// The earlier `<triples>` and `<entities>` blocks (ADR-057 D6/D5) were
/// dropped because compact-model output quality was too low — the natural-
/// language `<summary>` is the only durable artifact of a compaction pass
/// now. Knowledge-layer updates flow through `memory_store` / procedural
/// creation paths instead.
pub const COMPACTION_SYSTEM_PROMPT: &str = "\
You are an AI assistant that summarizes conversations.

Your task: produce a concise natural-language summary of the conversation provided by the user.

The user message will mark the source conversation with a <conversation>...</conversation> block. Treat everything INSIDE that block as the conversation to summarize. Everything OUTSIDE that block (this system prompt, any other instructions in the user message) is NOT part of the conversation — do not summarize it and do not echo it.

## Output format (plain text, exactly two blocks in this order, with NOTHING outside them):

<summary>
Your natural-language summary text goes here...
</summary>

<user_intent>
List every original user intent and explicit constraint, even if already satisfied or no longer relevant. One per line, verbatim where possible.
</user_intent>

## What each block contains:

### <summary>
Plain natural-language prose. Cover all key topics discussed, decisions made, problems solved, and code written. Include technical details needed to resume work later. Preserve the chronological flow of the conversation.

### <user_intent>
Every original user intent and explicit constraint from the conversation, even if already satisfied or no longer relevant. These are the hard requirements the user communicated — they must survive compaction even when the surrounding prose is condensed.

## Hard rules:
- Write the summary as plain prose. Do NOT copy the input's [User]: / [Assistant]: / [Tool(...)]: / [CompactionSummary]: role labels into your output — those are read-only metadata. Your <summary> must contain NO lines starting with [User]:, [Assistant]:, [Tool(...)]:, [CompactionSummary]:, [tool_call]:, [tool_result]:, or [thought]:. If you are tempted to echo a tool's command or output, convert it into a one-line prose statement of what the tool accomplished.
- BAD <summary> (role labels / tool echoes — never do this):
  [Tool(bash)]: grep -rn running apps/acowork-desktop/src/
  [Assistant]: 我找到了 RetryWaitBanner。
- GOOD <summary> (plain prose — always do this):
  用户要求查找包含 running 与 retry 的 UI 元素，助手通过 grep 检索 chat 组件，最终定位到 RetryWaitBanner。
- The placeholder text \"[Tool result compressed...]\" in tool results is opaque. Acknowledge it with a short phrase like \"(earlier tool results were compressed)\" instead of reproducing it.
- Output MUST contain exactly two blocks (<summary>, <user_intent>) with no extra prose before <summary>, between blocks, or after </user_intent>. The legacy `<entities>` and `<triples>` blocks are no longer emitted (ADR-057 D5/D6 — triples-removed).
- Language (MUST follow):
  - First, detect the language of the conversation inside <conversation>...</conversation>. If you can identify it confidently (the conversation is long enough or clearly monolingual — e.g. contains CJK characters, or is clearly English prose), use THAT language for <summary> and <user_intent>.
  - If the conversation is too short or too ambiguous to determine the language (e.g. only \"hi\", only \"hello\", a single emoji, or a single sentence that is identical in multiple languages), fall back to the Language field in the user identity context (provided as a separate \"About the user:\" block appended to this prompt). Use the code written there (e.g. \"zh-CN\" → Simplified Chinese, \"en-US\" → English).
  - If neither signal is available, default to English.";

/// Prompt for context compaction and episode distillation.
///
/// Per [ADR-011], the LLM outputs a plain natural-language summary — not JSON.
/// The summary serves both as in-memory context replacement and as a Grafeo
/// episodic memory entry.
///
/// Memory-hint extraction (triples) was removed entirely in ADR-057:
/// compaction now emits only a `<summary>` block (which doubles as the
/// Grafeo episodic memory entry). Knowledge persistence is handled by
/// the separate `memory_store` tool and offline consolidation, not by
/// this compaction path.
///
/// Role, task, output format, and all hard rules live in
/// [`COMPACTION_SYSTEM_PROMPT`] (higher priority). This prompt's only job
/// is to deliver the conversation body inside a clear `<conversation>`
/// delimiter so the LLM cannot confuse it with instructions.
pub const COMPACT_PROMPT: &str = r#"<conversation>
{messages_text}
</conversation>"#;

/// Maximum character length for a session title after truncation.
///
/// Titles longer than this are shortened by [`truncate_title_for_display`],
/// which prefers natural break points (`,.!?;。！？；`) and falls back to
/// a hard cut + `…` when no break exists within the budget.
///
/// 60 chars accommodates a meaningful Chinese sentence (e.g. "帮我重构
/// Acowork 桌面端 Settings 页面" — 23 chars at 90–120 LLM tokens once
/// tokenizer overhead is accounted for). LLM `max_tokens` must be sized
/// generously (see caller in `loop_.rs::run_inner`) so the API layer
/// does not truncate the model output before our display truncation runs.
pub const SESSION_TITLE_MAX_CHARS: usize = 60;

/// Prompt for generating a session title from the first user message.
///
/// `{language}` and `{user_message}` are resolved at the call site
/// (`agent/loop_.rs::run_inner`).
///
/// ## Output contract: PLAIN WORDS ONLY, no punctuation
///
/// The LLM output is rendered directly in the sidebar session tab, so the
/// prompt is intentionally strict about what the title may contain:
///
/// - **No punctuation marks** of any kind — neither ASCII (`. , ! ? ; : - — _ / \ | ' " ` ( ) [ ] { } …`)
///   nor their full-width CJK counterparts (`。，！？；：「」『』（）【】《》、· …`).
/// - No emojis, no symbols, no markdown decoration, no surrounding quotes.
/// - No leading or trailing punctuation, no ellipsis, no decoration at all.
/// - Write in the user's preferred language as `{language}`.
/// - Keep it concise (≤[`SESSION_TITLE_MAX_CHARS`] characters).
/// - Return **only** the title — no explanation, no preamble.
///
/// `truncate_title_for_display` (defined below) still strips any trailing
/// sentence-ending break that slips through as a defense-in-depth layer,
/// but the prompt is the primary guard so we never rely on post-hoc
/// stripping for the common case.
pub const TITLE_PROMPT: &str = r#"Generate a session title (max 60 characters) for the user_message.

Strict output rules — read carefully:
- Output PLAIN WORDS ONLY. No punctuation of any kind.
- Do NOT use any of these characters (or their full-width equivalents):
  . , ! ? ; : - — _ / \ | ' " ` ( ) [ ] {{ }} < > … · • * # @ ~ ^ + = % $ &
  ， 。 ！ ？ ； ： — … 「 」 『 （ ） 【 】 《 》 、 ·
- No emojis, no symbols, no markdown, no surrounding quotes.
- Do not end the title with a period, comma, or any punctuation.
- Write the title in the user's preferred language as {language}.
- Keep it concise (≤ 60 characters).
- Return ONLY the title text — nothing else.

{user_message}
"#;

/// Truncate a title to at most [`SESSION_TITLE_MAX_CHARS`] characters for
/// display, preferring natural sentence break points over a hard cut.
///
/// ## Break-character set
///
/// `BREAK_CHARS` is intentionally **narrow**: only sentence-ending
/// punctuation. `.` is **not** included because it appears mid-token in
/// URLs, file paths and version numbers (`v1.2.3`), where cutting on it
/// would mangle otherwise valid input. The other half/full-width pairs
/// cover both English and CJK writers.
///
/// ## Behaviour
///
/// - `s.len() <= SESSION_TITLE_MAX_CHARS` (in **characters**, not bytes):
///   returned verbatim. No `…` is appended for inputs that already fit —
///   we never want to display a truncated title when we had the whole
///   thing.
///
/// - Otherwise: take the first `SESSION_TITLE_MAX_CHARS` chars and scan
///   for the **last** break char in that window. If found at position
///   `pos > 0`, cut there. The cut is taken to **just before** the break
///   (excluding the trailing punctuation) so the result has no dangling
///   `,`/`;` and reads as a clean noun-phrase; this matches how the
///   sidebar renders. No `…` is appended because the result is a
///   complete, self-contained substring of the original.
///
/// - No break found in the budget window: hard-cut to
///   `SESSION_TITLE_MAX_CHARS` chars and append `…`. This is the only
///   path that produces a "truncated mid-word" visual, and only triggers
///   when the title is one long un-broken run (e.g. a URL or a single
///   long word in the user's first message).
///
/// ## Single source of truth
///
/// Used by both `loop_.rs` (async LLM-titled path, after
/// `compact_session_title_with_llm` returns) and `conversation.rs`
/// (`set_title` + `update_title_force`). Keeping one implementation
/// prevents drift between the two call sites — see `loop_.rs::run_inner`
/// for the LLM-titled path.
pub fn truncate_title_for_display(s: &str) -> String {
    // Sentence-ending punctuation only. NO `.` — see doc above.
    // Half-width + full-width pairs for both English and CJK.
    const BREAK_CHARS: &[char] = &[',', '，', '!', '！', '?', '？', ';', '；', '\n'];

    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= SESSION_TITLE_MAX_CHARS {
        return s.to_string();
    }

    let window = &chars[..SESSION_TITLE_MAX_CHARS];
    if let Some(pos) = window
        .iter()
        .rposition(|c| BREAK_CHARS.contains(c))
        .filter(|&p| p > 0)
    {
        // Cut strictly before the break char so the result ends on a word
        // boundary, not on a dangling `,`. The break is preserved in the
        // original (just dropped here) so the result reads as a clean
        // noun-phrase and the user can still recognise the topic.
        chars[..pos].iter().collect()
    } else {
        // No break (or break at position 0 — degenerate) within the
        // budget: hard-cut to budget and append `…`.
        let truncated: String = window.iter().collect();
        format!("{truncated}…")
    }
}

/// Build the final system prompt for context compaction by concatenating the
/// base [`COMPACTION_SYSTEM_PROMPT`] with the user's identity context.
///
/// The user's `identity_context` is a small (~200B) text block produced by
/// [`super::session::session_manager::format_user_profile_context`], e.g.:
///
/// ```text
/// - Display Name: ...
/// - Language: zh-CN
/// - Timezone: Asia/Shanghai
/// ...
/// ```
///
/// We embed it inline (rather than parsing the `Language:` line with a regex)
/// so the LLM itself reads the language field — no schema, no fragile parsing,
/// and any future field added to identity is automatically picked up.
///
/// Behaviour:
/// - `None` or empty/whitespace identity → returns `base` unchanged
///   (English default — safe fallback for sessions with no user profile).
/// - Non-empty identity → returns `base` + identity block + language directive.
pub fn build_compaction_system_prompt(base: &str, identity_context: Option<&str>) -> String {
    build_language_aware_system_prompt(
        base,
        identity_context,
        "Write the summary and knowledge triples in the user's preferred \
         language as indicated above.",
    )
}

/// Shared mechanism behind [`build_compaction_system_prompt`]: embed the
/// identity block in `base` and point the model at its `Language` field so
/// it can decide the output language itself.
///
/// Every summarization surface routes through here rather than embedding
/// the identity text on its own, so they cannot drift apart on *which*
/// language they produce. Only the closing `directive` differs — compaction
/// also distills knowledge triples; the global-search summary
/// ([`crate::http::search_summary`]) does not.
///
/// `None`/blank identity returns `base` unchanged, so each caller keeps
/// control of its own default. Note the `base` prompt may still carry a
/// query-language rule of its own — the identity block only *adds* an
/// override when one exists, it never removes the caller's fallback.
pub fn build_language_aware_system_prompt(
    base: &str,
    identity_context: Option<&str>,
    directive: &str,
) -> String {
    let Some(ctx) = identity_context.map(str::trim).filter(|s| !s.is_empty()) else {
        return base.to_string();
    };
    format!(
        "{base}\n\n\
         User identity context (use the Language field to determine what language \
         to write the summary in):\n\
         {ctx}\n\n\
         {directive}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_compaction_system_prompt_none_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(build_compaction_system_prompt(base, None), base);
    }

    #[test]
    fn build_compaction_system_prompt_empty_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(build_compaction_system_prompt(base, Some("")), base);
    }

    #[test]
    fn build_compaction_system_prompt_whitespace_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(
            build_compaction_system_prompt(base, Some("   \n\t  ")),
            base
        );
    }

    #[test]
    fn build_compaction_system_prompt_with_identity_includes_directive_and_context() {
        let base = "You are a summarizer.";
        let identity = "- Display Name: Alice\n- Language: zh-CN\n- Timezone: Asia/Shanghai";
        let out = build_compaction_system_prompt(base, Some(identity));
        // base preserved at the head
        assert!(out.starts_with(base), "base must be preserved at the start");
        // identity text embedded verbatim
        assert!(
            out.contains(identity),
            "identity text must be embedded verbatim"
        );
        // explicit pointers so the LLM knows where to look for the language field
        assert!(
            out.contains("User identity context"),
            "must label the identity block"
        );
        assert!(
            out.contains("Language field"),
            "must point the LLM at the Language field"
        );
        assert!(
            out.contains("preferred language"),
            "must include a language directive"
        );
    }

    #[test]
    fn build_compaction_system_prompt_trims_surrounding_whitespace() {
        // Identity surrounded by whitespace should be accepted; the surrounding
        // whitespace is stripped before concatenation, but the inner content
        // (e.g. "  Language: en-US  ") is preserved verbatim.
        let base = "base";
        let out = build_compaction_system_prompt(base, Some("  - Language: en-US  \n"));
        assert!(out.contains("- Language: en-US"));
    }

    // ----- truncate_title_for_display -----

    #[test]
    fn truncate_short_english_returned_as_is() {
        // Well under the budget — must be returned verbatim, no ellipsis.
        let input = "Hello world";
        assert_eq!(truncate_title_for_display(input), input);
    }

    #[test]
    fn truncate_short_chinese_returned_as_is() {
        // A common short Chinese session title — keep it whole.
        let input = "帮我重构 Settings 页面";
        assert_eq!(truncate_title_for_display(input), input);
    }

    #[test]
    fn truncate_exact_budget_returned_as_is() {
        // Exactly SESSION_TITLE_MAX_CHARS chars — must NOT be touched (no
        // ellipsis appended for a title that already fits).
        let input: String = "a".repeat(SESSION_TITLE_MAX_CHARS);
        assert_eq!(truncate_title_for_display(&input), input);
    }

    #[test]
    fn truncate_empty_string_returned_as_is() {
        assert_eq!(truncate_title_for_display(""), "");
    }

    #[test]
    fn truncate_english_with_comma_breaks_at_last_comma() {
        // Total length > 60 chars (so we enter the truncation branch), with
        // a comma well inside the first-60-chars window so rposition finds it.
        let input = "Short prefix, then a much longer suffix that doesn't fit in 60 chars at all really nope";
        let out = truncate_title_for_display(input);
        // The cut is BEFORE the only comma inside the budget window — at
        // position 12 (the comma). We strip the trailing `,` so the
        // result reads as a clean noun-phrase, not as a dangling punctuation.
        assert_eq!(out, "Short prefix");
        assert!(!out.ends_with('…'), "complete substring needs no ellipsis");
        assert!(!out.ends_with(','), "no dangling punctuation");
        assert!(out.chars().count() <= SESSION_TITLE_MAX_CHARS);
    }

    // Mirror of the private BREAK_CHARS in truncate_title_for_display — kept
    // here so the test can assert about dropped characters without exposing
    // the original private constant. Kept in sync intentionally.
    // (NOTE: `.` is NOT included — see the doc comment in truncate_title_for_display.)
    const BREAK_CHARS_USED: &[char] = &[',', '，', '!', '！', '?', '？', ';', '；', '\n'];

    #[test]
    fn truncate_chinese_with_full_stop_breaks_at_last_break() {
        // A representative Chinese title — the LLM might return a 70-char
        // sentence for a long user message. The last `，` (full-width comma)
        // within the first 60 chars should win.
        let input = "帮我把 Acowork 桌面端的 Settings 页面完整重构一遍，重点关注表单校验逻辑和国际化处理方案。顺便检查一下导航栏";
        let out = truncate_title_for_display(input);
        // Must end BEFORE a break — the implementation strips the trailing
        // break char so the result reads as a clean noun-phrase.
        assert!(out.chars().count() <= SESSION_TITLE_MAX_CHARS);
        assert!(
            !BREAK_CHARS_USED.contains(&out.chars().last().unwrap()),
            "result should NOT end on a break char (we strip it): got {:?}",
            out
        );
        // Sanity: the cut should be at the last `，` before position 60.
        // Locate the last `，` in the first 60 chars and confirm.
        let window: Vec<char> = input.chars().take(SESSION_TITLE_MAX_CHARS).collect();
        let last_break = window
            .iter()
            .rposition(|c| BREAK_CHARS_USED.contains(c))
            .unwrap();
        let expected: String = window[..last_break].iter().collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn truncate_dot_is_not_a_break_char_preserves_urls() {
        // URLs contain many `.` chars. We must NOT cut on `.` because that
        // would mangle the URL into useless fragments like "https://example".
        let input = "https://example.com/very/long/path/that/has/no/breakpoints/at/all/really/long";
        let out = truncate_title_for_display(input);
        // URL has no `,!?;` — only `.` and `/`. `.` is intentionally NOT a
        // break char per the doc, so we fall through to hard-cut + `…`.
        assert!(
            out.ends_with('…'),
            "URLs contain only `.` (not a break char) — must hard-cut + ellipsis: {out:?}"
        );
        assert_eq!(out.chars().count(), SESSION_TITLE_MAX_CHARS + 1);
    }

    #[test]
    fn truncate_break_at_position_zero_does_not_cut() {
        // If the budget starts with a break char (pos 0), the break
        // candidate is filtered out (we require pos > 0). Otherwise we
        // would return an empty string for inputs starting with `,`.
        let input = String::from(
            ",rest of title that is also quite long and goes way past sixty characters in total really",
        );
        // Make sure it's longer than the budget.
        assert!(input.chars().count() > SESSION_TITLE_MAX_CHARS);
        let out = truncate_title_for_display(&input);
        // Since the only break is at position 0, we fall through to
        // hard-cut + `…` of the FULL budget.
        assert!(!out.is_empty());
        assert_eq!(out.chars().count(), SESSION_TITLE_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_one_char_over_budget_is_appended_ellipsis() {
        // Off-by-one sanity check: input length == budget + 1.
        let mut input: String = "x".repeat(SESSION_TITLE_MAX_CHARS);
        input.push('y');
        let out = truncate_title_for_display(&input);
        // No break exists in the budget, so hard cut + …
        assert_eq!(out.chars().count(), SESSION_TITLE_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_unicode_aware_uses_char_count_not_byte_count() {
        // Chinese chars are 3 bytes each in UTF-8 but count as 1 character.
        // A 60-char Chinese title must NOT be truncated; a 61-char title must.
        let exactly_fit: String = "中".repeat(SESSION_TITLE_MAX_CHARS);
        assert_eq!(
            truncate_title_for_display(&exactly_fit).chars().count(),
            SESSION_TITLE_MAX_CHARS,
            "60-char CJK input must be returned as-is"
        );

        let one_over: String = "中".repeat(SESSION_TITLE_MAX_CHARS + 1);
        let out = truncate_title_for_display(&one_over);
        // No break exists (none of the CJK chars are break chars), so we
        // hard cut to 60 and add `…`. Total 61 chars.
        assert_eq!(out.chars().count(), SESSION_TITLE_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    // ----- TITLE_PROMPT output contract -----

    /// TITLE_PROMPT must carry the {language} and {user_message}
    /// placeholders so the caller in `agent/loop_.rs::run_inner` can
    /// resolve them. Catches accidental template regressions.
    #[test]
    fn title_prompt_carries_required_placeholders() {
        assert!(
            TITLE_PROMPT.contains("{language}"),
            "TITLE_PROMPT must carry the {{language}} placeholder"
        );
        assert!(
            TITLE_PROMPT.contains("{user_message}"),
            "TITLE_PROMPT must carry the {{user_message}} placeholder"
        );
    }

    /// TITLE_PROMPT must explicitly forbid ASCII punctuation so the LLM
    /// does not decorate its output with `. , ! ? ; : - …` etc. This is
    /// the primary guard for the sidebar's "plain words only" contract;
    /// `truncate_title_for_display` is only a defense-in-depth layer.
    #[test]
    fn title_prompt_forbids_ascii_punctuation() {
        for forbidden in [
            '.', ',', '!', '?', ';', ':', '-', '_', '/', '\\', '|', '\'', '"', '`', '(', ')', '[',
            ']', '{', '}', '<', '>', '…', '*', '#', '@', '~', '^', '+', '=', '%', '$', '&',
        ] {
            // The prompt lists these in a single "do NOT use" line, so
            // even one occurrence of the char in the prose is enough to
            // satisfy the test (the LLM is told they are forbidden).
            assert!(
                TITLE_PROMPT.contains(forbidden),
                "TITLE_PROMPT must list {:?} as a forbidden char so the LLM is told to avoid it",
                forbidden
            );
        }
    }

    /// TITLE_PROMPT must also forbid full-width CJK punctuation — CJK
    /// users writing in zh-CN/ja/ko would otherwise get titles wrapped in
    /// `。 ， ！ ？` etc., which look ugly in the sidebar session tab.
    #[test]
    fn title_prompt_forbids_fullwidth_cjk_punctuation() {
        for forbidden in [
            '，', '。', '！', '？', '；', '：', '—', '…', '「', '」', '『', '（', '）', '【', '】',
            '《', '》', '、', '·',
        ] {
            assert!(
                TITLE_PROMPT.contains(forbidden),
                "TITLE_PROMPT must list {:?} (full-width CJK) as a forbidden char",
                forbidden
            );
        }
    }

    /// TITLE_PROMPT must state the "plain words only / no punctuation"
    /// contract in plain English — both for documentation value in the
    /// file and so model output remains stable across providers that
    /// weigh different phrasing of the rule differently.
    #[test]
    fn title_prompt_states_plain_words_only_contract() {
        assert!(
            TITLE_PROMPT.contains("PLAIN WORDS ONLY"),
            "TITLE_PROMPT must declare the PLAIN WORDS ONLY contract verbatim"
        );
        assert!(
            TITLE_PROMPT.contains("No punctuation"),
            "TITLE_PROMPT must mention 'No punctuation' as a hard rule"
        );
        assert!(
            TITLE_PROMPT.contains("Return ONLY the title"),
            "TITLE_PROMPT must tell the LLM to return only the title (no preamble)"
        );
    }

    /// Resolves {language} and {user_message} and checks that the user
    /// message is appended verbatim — guards the template-formatting path
    /// the caller relies on.
    #[test]
    fn title_prompt_resolves_placeholders() {
        let resolved = TITLE_PROMPT
            .replace("{language}", "zh-CN")
            .replace("{user_message}", "帮我重构 Settings 页面");
        assert!(!resolved.contains("{language}"));
        assert!(!resolved.contains("{user_message}"));
        assert!(resolved.contains("zh-CN"));
        assert!(resolved.contains("帮我重构 Settings 页面"));
        // The resolved string is what we ship to the LLM — re-check the
        // contract rules survive substitution (the contract lives in the
        // prose, not in the placeholders).
        assert!(resolved.contains("PLAIN WORDS ONLY"));
    }
}
