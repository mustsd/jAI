//! Text helpers.

/// Truncate `text` to at most `max_bytes`, never splitting a character.
///
/// Returns the (possibly shortened) text and whether it was truncated.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    let mut out = text[..end].to_string();
    out.push_str("\n…[truncated]");
    (out, true)
}

/// Keep the first `max_lines` lines of `text`.
pub fn head_lines(text: &str, max_lines: usize) -> String {
    text.lines().take(max_lines).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_untouched() {
        let (text, truncated) = truncate_bytes("hello", 10);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Each CJK character is 3 bytes and each emoji 4 bytes.
        let text = "实现一个 智能体 🚀 tail";
        let (truncated_text, truncated) = truncate_bytes(text, 7);
        assert!(truncated);
        assert!(truncated_text.starts_with("实"));
        assert!(truncated_text.ends_with("[truncated]"));
        // No replacement characters were introduced by slicing.
        assert!(!truncated_text.contains('\u{FFFD}'));
    }

    #[test]
    fn exact_boundary_is_not_truncated() {
        let (text, truncated) = truncate_bytes("你好", 6);
        assert_eq!(text, "你好");
        assert!(!truncated);
    }

    #[test]
    fn zero_budget_still_produces_valid_utf8() {
        let (text, truncated) = truncate_bytes("abc", 0);
        assert!(truncated);
        assert!(text.starts_with('…') || text.starts_with("\n…"));
    }

    #[test]
    fn head_lines_keeps_the_prefix() {
        assert_eq!(head_lines("a\nb\nc", 2), "a\nb");
        assert_eq!(head_lines("", 3), "");
    }
}
