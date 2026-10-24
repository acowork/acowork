//! 字符安全的文本工具
//!
//! ## 为什么需要这个模块
//!
//! 之前 runtime 各处散落 `&s[..bytes]` 形式的字节切片用作日志预览 / 错误信息
//! 截断。一旦内容超过切片长度且第 N 字节落在多字节 UTF-8 字符（如 CJK 3 字节、
//! emoji 4 字节）中间，会触发 `byte index N is not a char boundary` panic。
//!
//! 2026-08-24 线上 session 就是因此死亡：
//! - LLM 流式返回的 tool call arguments 含中文自然语言
//! - JSON 解析失败，runtime 走 error 日志分支
//! - `&args[..args.len().min(200)]` 切到 `'原'` 字中间（bytes 198..201）
//! - SessionTask panic，前端永远卡在「回复中」直到 runtime 重启
//!
//! ## 设计原则
//!
//! - **统一收口**：所有「取前 N 个字节/字符用于预览/截断」的需求都走这里。
//! - **tracing 友好**：`Preview<'_>` 实现 `Display`，可以直接放进 `tracing::field`
//!   的 `%` 占位符，不会产生额外堆分配（截断时一次性 collect）。
//! - **与现有 `truncate_utf8` 等价**：本模块承接 `tools/output.rs::truncate_utf8`
//!   的语义（原本该函数被孤立、未被复用），所有调用点都迁移过来。
//!
//! ## 使用方式
//!
//! ```ignore
//! use crate::util::text::{preview, truncate_utf8, TextPreview};
//!
//! // 1. 直接调用 helper
//! tracing::error!(raw_preview = %preview(args, 200), "...");
//!
//! // 2. 用 trait 方法
//! tracing::error!(raw_preview = %args.preview(200), "...");
//!
//! // 3. 想要 `&str` 而不是 Display：用 truncate_utf8
//! let preview: &str = truncate_utf8(args, 200);
//! ```

use std::fmt;

/// 按字节上限截取到一个字符安全的边界。
///
/// 行为：
/// - `input.len() <= max_bytes`：返回原文。
/// - 否则从 `max_bytes` 向左查找最近的 `is_char_boundary` 位置（即最近的 UTF-8
///   字符起点），返回该位置的切片。
///
/// 与 `str::floor_char_boundary`（nightly）等价；自实现以兼容 stable。
///
/// 不分配。
pub fn truncate_utf8(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

/// Return the largest byte index `<= at` that lies on a `is_char_boundary`,
/// then return the substring starting there.
///
/// Used by head+tail truncation: we want the **last** `tail_bytes` bytes, so
/// we compute `start = len - tail_bytes` and slice from there — but if that
/// byte lands in the middle of a multi-byte UTF-8 sequence (CJK 3-byte char,
/// emoji 4-byte char), `&input[start..]` panics with
/// `byte index N is not a char boundary`. Walking **down** to the previous
/// char boundary is the safe fix.
///
/// Behaviour:
/// - `at >= input.len()` → `input.len()` (whole string)
/// - `at == 0` → `0`
/// - `at` already on a boundary → `at`
/// - Otherwise → nearest `< at` char boundary (never panics)
///
/// Does not allocate.
pub fn tail_slice_from(input: &str, at: usize) -> &str {
    let total = input.len();
    if at > total {
        // 防御：调用方传了超过 len() 的 byte index，fallback 返回原文
        // （不应该 panic）
        return input;
    }
    if at == total {
        // 从 byte total 开始到末尾 → 空（与 `&input[total..]` 等价且安全）
        return "";
    }
    if at == 0 {
        return "";
    }
    let mut start = at;
    if input.is_char_boundary(start) {
        return &input[start..];
    }
    while start > 0 && !input.is_char_boundary(start) {
        start -= 1;
    }
    &input[start..]
}

/// 字符安全的预览视图，专为 `tracing` field 设计。
///
/// 通过 `Display` 实现，按字符（不是字节）截断，附加 `…(truncated, N chars)`
/// 后缀。命中多字节 UTF-8 字符（中文、emoji）绝不会 panic。
///
/// ## 性能
///
/// 单次完整遍历：累计总字符数并记录截断位置（不提前 break——后缀需要总字符数）。
/// 对调试日志（O(可见字符)）开销可忽略；若调用方对超大字符串敏感，可以改用
/// [`truncate_utf8`] 拿 `&str` 而不统计总数。
///
/// `Debug` 实现等同于 `Display`（仅供 `tracing` 的 `?` 字段使用），方便同一个
/// preview 同时满足 `DisplayValue` 和 `DebugValue` 的 trait 约束。
#[derive(Clone)]
pub struct Preview<'a> {
    text: &'a str,
    max_chars: usize,
}

impl<'a> Preview<'a> {
    pub fn new(text: &'a str, max_chars: usize) -> Self {
        Self { text, max_chars }
    }
}

impl<'a> From<(&'a str, usize)> for Preview<'a> {
    fn from((text, max_chars): (&'a str, usize)) -> Self {
        Self::new(text, max_chars)
    }
}

impl<'a> fmt::Display for Preview<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 单次遍历：累计总字符数，并在第 max_chars 个字符"之后"的位置打标记。
        // 标记位 = 第 (max_chars + 1) 个字符的字节起点 = 前 max_chars 个字符的终点。
        // 不 break —— 后续字符继续累计 total，用于后缀的总字符数。
        let mut total = 0usize;
        let mut cut_byte: Option<usize> = None;
        for (byte_idx, _) in self.text.char_indices() {
            if total == self.max_chars {
                cut_byte = Some(byte_idx);
            }
            total += 1;
        }

        match cut_byte {
            None => f.write_str(self.text),
            Some(end) => write!(
                f,
                "{}…(truncated, {} chars total)",
                &self.text[..end],
                total
            ),
        }
    }
}

impl fmt::Debug for Preview<'_> {
    /// 与 `Display` 输出完全一致 —— 兑现上方文档承诺，并保证 `tracing` 的 `?`
    /// 字段（如 `observer_impl` 的 `ws_preview = ?...preview(80)`）输出可读的
    /// 预览文本，而不是 `Preview { text: ..., max_chars: ... }` 内部结构。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// 便利函数：构造 [`Preview`]。
pub fn preview(text: &str, max_chars: usize) -> Preview<'_> {
    Preview::new(text, max_chars)
}

/// 给 `&str` / `String` 添加 `.preview(n)` / `.truncate_bytes(n)` 方法。
///
/// 用意是替代散落的 `&s[..s.len().min(N)]` 模式。
pub trait TextPreview {
    /// 字符安全的预览（用于 tracing field）。
    fn preview(&self, max_chars: usize) -> Preview<'_>;
    /// 按字节上限截取到字符安全边界，返回 `&str`。
    fn truncate_bytes(&self, max_bytes: usize) -> &str;
}

impl TextPreview for str {
    fn preview(&self, max_chars: usize) -> Preview<'_> {
        Preview::new(self, max_chars)
    }
    fn truncate_bytes(&self, max_bytes: usize) -> &str {
        truncate_utf8(self, max_bytes)
    }
}

impl TextPreview for String {
    fn preview(&self, max_chars: usize) -> Preview<'_> {
        Preview::new(self.as_str(), max_chars)
    }
    fn truncate_bytes(&self, max_bytes: usize) -> &str {
        truncate_utf8(self.as_str(), max_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_utf8 ─────────────────────────────────────────────

    #[test]
    fn truncate_utf8_ascii_under_limit() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
    }

    #[test]
    fn truncate_utf8_ascii_over_limit() {
        assert_eq!(truncate_utf8("hello world", 5), "hello");
    }

    #[test]
    fn truncate_utf8_cjk_safe_boundary() {
        // "中文测试字符串" 7 个汉字 × 3 字节 = 21 字节
        // 字节布局： 0..3 中, 3..6 文, 6..9 测, 9..12 试, 12..15 字, 15..18 符, 18..21 串
        let s = "中文测试字符串";
        assert_eq!(s.len(), 21);

        // max=3 字节 = char boundary → "中"
        assert_eq!(truncate_utf8(s, 3), "中");
        // max=4..5 字节 = "文" 中间 → 回退到 3 → "中"
        assert_eq!(truncate_utf8(s, 4), "中");
        assert_eq!(truncate_utf8(s, 5), "中");
        // max=6 字节 = char boundary → "中文"
        assert_eq!(truncate_utf8(s, 6), "中文");
        // max=7..8 字节 = "测" 中间 → 回退到 6 → "中文"
        assert_eq!(truncate_utf8(s, 7), "中文");
        assert_eq!(truncate_utf8(s, 8), "中文");
        // max=9 字节 = char boundary → "中文测"
        assert_eq!(truncate_utf8(s, 9), "中文测");
        // max=0 → 空
        assert_eq!(truncate_utf8(s, 0), "");
    }

    #[test]
    fn truncate_utf8_emoji_4byte() {
        // 3 个 emoji × 4 字节 = 12 字节
        let s = "😀😁😂";
        assert_eq!(s.len(), 12);
        assert_eq!(truncate_utf8(s, 4), "😀");
        // max=5 落在第二个 emoji 中间 → 回退到 4 字节
        assert_eq!(truncate_utf8(s, 5), "😀");
    }

    #[test]
    fn truncate_utf8_empty() {
        assert_eq!(truncate_utf8("", 0), "");
        assert_eq!(truncate_utf8("", 100), "");
    }

    #[test]
    fn truncate_utf8_exact_boundary() {
        let s = "中文abc"; // 6 + 3 = 9 字节
        assert_eq!(truncate_utf8(s, 6), "中文");
        assert_eq!(truncate_utf8(s, 9), "中文abc");
    }

    #[test]
    fn truncate_utf8_at_max_zero_does_not_panic() {
        let s = "中文";
        assert_eq!(truncate_utf8(s, 0), "");
    }

    #[test]
    fn truncate_utf8_max_smaller_than_single_char_returns_empty() {
        // 边界：max_bytes < 单个多字节字符长度 → 回退到 0（空串），绝不 panic
        let s = "中文"; // 6 字节，但单字符 3 字节
        assert_eq!(truncate_utf8(s, 2), "");
        assert_eq!(truncate_utf8(s, 1), "");
        // 混合：CJK 3 字节 + ASCII 1 字节
        let mixed = "中a"; // 3 + 1 = 4 字节
        assert_eq!(truncate_utf8(mixed, 3), "中");
        assert_eq!(truncate_utf8(mixed, 2), "");
        assert_eq!(truncate_utf8(mixed, 1), "");
    }

    // ── tail_slice_from ───────────────────────────────────────────

    #[test]
    fn tail_slice_from_ascii_at_boundary() {
        let s = "hello world"; // 11 bytes, all ASCII
        // at == 0 → empty
        assert_eq!(tail_slice_from(s, 0), "");
        // at == 5 (start of " world") → " world" (with leading space)
        assert_eq!(tail_slice_from(s, 5), " world");
        // at == 6 (start of "world") → "world" (no leading space)
        assert_eq!(tail_slice_from(s, 6), "world");
        // at == len → empty (nothing from end of string onward)
        assert_eq!(tail_slice_from(s, 11), "");
    }

    #[test]
    fn tail_slice_from_at_past_end_returns_whole() {
        // 保护：at > input.len() 不应 panic
        let s = "abc";
        assert_eq!(tail_slice_from(s, 100), "abc");
    }

    #[test]
    fn tail_slice_from_cjk_floors_to_prev_boundary() {
        // "中文abc" — 9 bytes
        // byte layout: 0..3 中, 3..6 文, 6..7 a, 7..8 b, 8..9 c
        let s = "中文abc";
        // at == 8 (mid-char: 'b' is 1 byte, but byte 8 is mid 'b'? no — byte 7..8 is 'b')
        // at == 7 is exact boundary (start of 'b')
        assert_eq!(tail_slice_from(s, 7), "bc");
        // at == 5 落在 '文' 中间 → 回退到 3 (start of 文) → "文abc"
        assert_eq!(tail_slice_from(s, 5), "文abc");
        // at == 4 落在 '文' 中间 → 回退到 3 → "文abc"
        assert_eq!(tail_slice_from(s, 4), "文abc");
        // at == 2 落在 '中' 中间 → 回退到 0 → "中文abc"
        assert_eq!(tail_slice_from(s, 2), "中文abc");
    }

    #[test]
    fn tail_slice_from_emoji_4byte_safe() {
        // "😀😁😂" — 3 emojis × 4 bytes = 12 bytes
        // byte layout: 0..4 😀, 4..8 😁, 8..12 😂
        let s = "😀😁😂";
        // at == 8 (exact boundary, start of 😂) → "😂"
        assert_eq!(tail_slice_from(s, 8), "😂");
        // at == 10 落在 😂 中间 → 回退到 8 → "😂"
        assert_eq!(tail_slice_from(s, 10), "😂");
        // at == 5 落在 😁 中间 → 回退到 4 → "😁😂"
        assert_eq!(tail_slice_from(s, 5), "😁😂");
    }

    #[test]
    fn tail_slice_from_does_not_panic_on_mid_multibyte() {
        // 这个调用如果用 `&s[..at]` 会 panic（byte index N is not a char boundary）
        // 是 head+tail 截断的核心 panic 防御
        let s = "中文abc";
        let _ = tail_slice_from(s, 4);
        let _ = tail_slice_from(s, 1);
    }

    // ── Preview ───────────────────────────────────────────────────

    #[test]
    fn preview_short_text_no_truncation() {
        let p = Preview::new("hello", 100);
        assert_eq!(format!("{p}"), "hello");
    }

    #[test]
    fn preview_max_chars_zero_truncates_everything() {
        // 边界：max_chars = 0 → 任何非空文本都应截断为 "…(truncated, N chars total)"
        let p = Preview::new("中文abc", 0);
        let out = format!("{p}");
        assert!(out.starts_with('…'), "got: {out}");
        assert!(out.contains("5 chars total"), "got: {out}");
    }

    #[test]
    fn preview_empty_text_no_cut_marker() {
        // 空文本：没有字符可截断，直接输出原文（不产生虚假的 truncated 后缀）
        let p = Preview::new("", 0);
        assert_eq!(format!("{p}"), "");
        let p = Preview::new("", 5);
        assert_eq!(format!("{p}"), "");
    }

    #[test]
    fn preview_debug_output_matches_display() {
        // 回归 A1：Debug 必须等同 Display（tracing `?` 字段依赖此行为）
        let p = Preview::new("中文测试", 2);
        assert_eq!(format!("{p:?}"), format!("{p}"));
        let out = format!("{p:?}");
        assert!(
            out.starts_with("中文"),
            "Debug must render preview text, got: {out}"
        );
        assert!(
            !out.contains("Preview {"),
            "Debug must not leak internal struct, got: {out}"
        );
    }

    #[test]
    fn preview_cjk_truncates_at_char_boundary() {
        // 7 字符 → max 4 → 取前 4 个汉字
        let p = Preview::new("中文测试字符串", 4);
        let s = format!("{p}");
        assert!(s.starts_with("中文测试"), "got: {s}");
        assert!(s.contains("truncated"));
        assert!(s.contains("7 chars total"), "got: {s}");
    }

    #[test]
    fn preview_emoji_safe_at_char_boundary() {
        let p = Preview::new("😀😁😂😃", 2);
        let s = format!("{p}");
        assert!(s.starts_with("😀😁"));
        assert!(s.contains("truncated"));
    }

    #[test]
    fn preview_helper_function() {
        let p = preview("hello world", 5);
        assert_eq!(format!("{p}"), "hello…(truncated, 11 chars total)");
    }

    #[test]
    fn preview_does_not_split_cjk() {
        // 关键回归测试：不能切到汉字中间
        // 触发原 panic 场景的中文字符串
        let original = "全景 + P0 的单一文档，还是拆分成 2 个文档？";
        let p = Preview::new(original, 4);
        let out = format!("{p}");
        // 前 4 个字符必须是完整的 4 个字符（不切汉字）：全 / 景 / 空格 / +
        assert!(out.starts_with("全景 +"), "got: {out}");
        // 不能切到 '+' 中间（'+' 是 ASCII 1 字节所以这个回归保护只对 multi-byte 起作用）
        assert!(!out.contains('P'), "got: {out}");
    }

    #[test]
    fn preview_total_count_is_correct_for_cjk() {
        // 5 个汉字 = 5 chars
        let p = Preview::new("一二三四五", 2);
        let out = format!("{p}");
        assert!(out.contains("5 chars total"), "got: {out}");
    }

    // ── trait TextPreview ─────────────────────────────────────────

    #[test]
    fn str_trait_preview() {
        // "中文测试字符串abcdef" — 7 CJK chars then 6 ASCII chars = 13 chars total
        // bytes layout: 0..21 CJK (7 chars × 3 bytes), 21..27 ASCII
        // max=6 chars → cuts at byte offset of char #6 (zero-indexed #5) = byte 15
        // → first 6 chars = "中文测试字符"
        let s = "中文测试字符串abcdef";
        assert_eq!(
            format!("{}", s.preview(6)),
            "中文测试字符…(truncated, 13 chars total)"
        );
    }

    #[test]
    fn str_trait_truncate_bytes() {
        let s = "中文测试字符串";
        assert_eq!(s.truncate_bytes(7), "中文");
    }

    #[test]
    fn string_trait_preview() {
        let owned: String = "中文测试字符串".to_string();
        assert_eq!(
            format!("{}", owned.preview(4)),
            "中文测试…(truncated, 7 chars total)"
        );
    }

    #[test]
    fn string_trait_truncate_bytes() {
        let owned: String = "中文".to_string();
        assert_eq!(owned.truncate_bytes(100), "中文");
    }
}
