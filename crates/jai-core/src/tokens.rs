//! Token estimation and model context windows.
//!
//! jAI never guesses when the provider tells the truth: [`crate::message::TokenUsage`]
//! reported by the API always wins for *task* accounting. This module exists for
//! the numbers a provider does not report — above all the size of the *active
//! context* (system prompt + tool schemas + conversation) before a request is
//! even sent, which has to be estimated locally.
//!
//! The estimator is deliberately simple and documented rather than clever: it is
//! a byte/character heuristic, not a tokenizer. It aims to be within ~15% of the
//! real BPE counts for mixed English/CJK text, which is accurate enough to warn
//! "you are about to fill the window" without shipping a multi-megabyte
//! vocabulary file.

use crate::message::{Message, ToolSpec};

/// Context window assumed when neither configuration nor [`context_window_for`]
/// knows better.
///
/// Sized for the models jAI is pointed at today: the reasoning and coding models
/// in the built-in table are 1M-context or heading there, and underestimating
/// makes the `Cxt:` figure cry wolf — jAI would warn about a window filling up
/// that is not close to full. An explicit `agent.context_window` (or a model id
/// the table knows) still wins; this is only the last resort.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Per-message framing overhead (role, separators) charged by chat APIs.
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;

/// Per-tool framing overhead in the request body.
const TOOL_OVERHEAD_TOKENS: u64 = 8;

/// Estimate the tokens `text` costs.
///
/// ASCII text averages roughly 4 characters per token in current BPE
/// vocabularies; CJK, emoji and other non-ASCII characters are usually one token
/// each (sometimes two). The result rounds up, so it never reports 0 tokens for
/// non-empty text.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii.div_ceil(4) + wide
}

/// Estimate what one message costs on the wire.
pub fn estimate_message(message: &Message) -> u64 {
    let mut tokens = MESSAGE_OVERHEAD_TOKENS + estimate_tokens(&message.content);
    for call in &message.tool_calls {
        tokens += estimate_tokens(&call.name) + estimate_tokens(&call.arguments) + 4;
    }
    if let Some(name) = &message.name {
        tokens += estimate_tokens(name);
    }
    tokens
}

/// Estimate what a whole conversation costs.
pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

/// Estimate what the tool schemas cost.
///
/// The schemas are JSON sent with every request, so they are part of the active
/// context even though the model never "says" them.
pub fn estimate_tool_specs(specs: &[ToolSpec]) -> u64 {
    specs
        .iter()
        .map(|spec| {
            TOOL_OVERHEAD_TOKENS
                + estimate_tokens(&spec.name)
                + estimate_tokens(&spec.description)
                + estimate_tokens(&spec.parameters.to_string())
        })
        .sum()
}

/// Tokens the active context occupies: messages plus tool schemas.
pub fn estimate_context(messages: &[Message], specs: &[ToolSpec]) -> u64 {
    estimate_messages(messages) + estimate_tool_specs(specs)
}

/// Best-effort context window for a model id.
///
/// Provider APIs rarely expose this, so the table is a set of documented
/// pattern matches on the model id. `None` means "unknown", and callers fall
/// back to [`DEFAULT_CONTEXT_WINDOW`] or an explicit configuration value.
///
/// The suffixes used by local models (`-8k`, `-32k`, `-128k`, `-1m`) are parsed
/// first, because they are authoritative and unambiguous.
pub fn context_window_for(model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    if let Some(size) = parse_size_suffix(&model) {
        return Some(size);
    }
    const TABLE: &[(&str, u64)] = &[
        // DeepSeek. Both current ids are 128k; the older 64k reasoner is gone.
        ("deepseek-reasoner", 128_000),
        ("deepseek-r1", 128_000),
        ("deepseek-chat", 128_000),
        ("deepseek-coder", 128_000),
        // A bare `deepseek` catch-all used to sit here and claimed 64k for every
        // unknown `deepseek-*` id, including ones with far larger windows. It
        // shadowed DEFAULT_CONTEXT_WINDOW and produced `Cxt: 105,449 / 64,000
        // (164.8%)` on models it had simply never heard of. An id the table does
        // not know is better served by the default than by a stale guess.
        // OpenAI
        ("gpt-3.5", 16_385),
        ("gpt-4.1", 1_000_000),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4", 8_192),
        ("o4", 200_000),
        ("o3", 200_000),
        ("o1", 200_000),
        // Anthropic (via the OpenAI-compatibility layer)
        ("claude", 200_000),
        // Google
        ("gemini-1.5", 1_000_000),
        ("gemini", 1_000_000),
        // xAI
        ("grok", 131_072),
        // Meta / Llama family
        ("llama-3", 128_000),
        ("llama3", 128_000),
        ("llama", 8_192),
        // Mistral
        ("mixtral", 32_768),
        ("mistral", 32_768),
        // Qwen
        ("qwen", 131_072),
        // Moonshot
        ("moonshot-v1-8k", 8_192),
        ("moonshot-v1-32k", 32_768),
        ("moonshot-v1-128k", 131_072),
        ("kimi", 131_072),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| model.contains(needle))
        .map(|(_, window)| *window)
}

/// Parse an explicit `8k` / `32k` / `128k` / `1m` marker in a model id.
///
/// Only the trailing marker counts, so `moonshot-v1-128k` and `qwen2.5-32k-instruct`
/// resolve, while a version number such as `gpt-4.1-1b` does not.
fn parse_size_suffix(model: &str) -> Option<u64> {
    let tail = model.rsplit(['-', '_', ':']).next()?;
    let (digits, multiplier) = match tail.strip_suffix('k') {
        Some(digits) => (digits, 1_000),
        None => match tail.strip_suffix('m') {
            Some(digits) => (digits, 1_000_000),
            None => return None,
        },
    };
    let value: u64 = digits.parse().ok()?;
    // Anything below 1k or above 4M is not a context window.
    let total = value.checked_mul(multiplier)?;
    (1_000..=4_000_000).contains(&total).then_some(total)
}

/// Resolve the window to display: explicit configuration, then the model table,
/// then [`DEFAULT_CONTEXT_WINDOW`].
pub fn resolve_context_window(model: &str, configured: Option<u64>) -> u64 {
    configured
        .filter(|window| *window > 0)
        .or_else(|| context_window_for(model))
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    #[test]
    fn ascii_text_is_about_four_characters_per_token() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1, "never zero for non-empty text");
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2, "rounds up");
        assert_eq!(estimate_tokens(&"x".repeat(400)), 100);
    }

    #[test]
    fn wide_characters_count_as_one_token() {
        // CJK is roughly one token per character, so a 4-character Chinese
        // sentence must not be estimated like 12 ASCII bytes.
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("实现一个 agent"), 5 + 2);
    }

    #[test]
    fn messages_and_tools_carry_framing_overhead() {
        let plain = estimate_message(&Message::user(""));
        assert_eq!(plain, MESSAGE_OVERHEAD_TOKENS);
        let with_content = estimate_message(&Message::user("abcd"));
        assert_eq!(with_content, MESSAGE_OVERHEAD_TOKENS + 1);

        let with_call = estimate_message(&Message::assistant_with_tools(
            "",
            vec![ToolCall::new(
                "call_1",
                "filesystem_read",
                r#"{"path":"a"}"#,
            )],
        ));
        assert!(with_call > MESSAGE_OVERHEAD_TOKENS);

        let specs = vec![ToolSpec {
            name: "shell_execute".into(),
            description: "run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        assert!(estimate_tool_specs(&specs) > TOOL_OVERHEAD_TOKENS);
        assert_eq!(estimate_tool_specs(&[]), 0);
        assert_eq!(
            estimate_context(&[Message::user("abcd")], &specs),
            estimate_messages(&[Message::user("abcd")]) + estimate_tool_specs(&specs)
        );
    }

    #[test]
    fn explicit_size_suffixes_win_over_the_table() {
        assert_eq!(context_window_for("moonshot-v1-8k"), Some(8_192));
        assert_eq!(context_window_for("moonshot-v1-32k"), Some(32_768));
        assert_eq!(context_window_for("qwen2.5-32k-instruct"), Some(32_000));
        assert_eq!(context_window_for("mistral-7b-1m"), Some(1_000_000));
        // A version-looking suffix must not be mistaken for a window.
        assert_eq!(
            context_window_for("gpt-4.1-1b"),
            context_window_for("gpt-4.1")
        );
    }

    #[test]
    fn known_models_map_to_plausible_windows() {
        assert_eq!(context_window_for("deepseek-chat"), Some(128_000));
        assert_eq!(context_window_for("deepseek-reasoner"), Some(128_000));
        assert_eq!(context_window_for("gpt-4o-mini"), Some(128_000));
        assert_eq!(context_window_for("claude-3-5-haiku-latest"), Some(200_000));
        assert_eq!(context_window_for("llama3.2"), Some(128_000));
        assert_eq!(context_window_for("totally-unknown-model"), None);
    }

    /// Regression: a family catch-all claimed 64k for every unrecognised
    /// `deepseek-*` id — including ids whose real window is far larger — which
    /// shadowed the default and rendered as `Cxt: … / 64,000 (164.8%)`.
    #[test]
    fn an_unknown_deepseek_id_falls_through_to_the_default() {
        assert_eq!(context_window_for("deepseek-flash"), None);
        assert_eq!(
            resolve_context_window("deepseek-flash", None),
            DEFAULT_CONTEXT_WINDOW
        );
        // ... and an explicit setting still wins over both.
        assert_eq!(
            resolve_context_window("deepseek-flash", Some(500_000)),
            500_000
        );
    }

    #[test]
    fn resolution_order_is_config_then_table_then_default() {
        assert_eq!(
            resolve_context_window("deepseek-chat", Some(64_000)),
            64_000
        );
        assert_eq!(resolve_context_window("deepseek-chat", None), 128_000);
        assert_eq!(
            resolve_context_window("totally-unknown-model", None),
            DEFAULT_CONTEXT_WINDOW
        );
        // A zero/absent override must not disable the display.
        assert_eq!(
            resolve_context_window("totally-unknown-model", Some(0)),
            DEFAULT_CONTEXT_WINDOW
        );
    }
}
