//! Secret redaction.
//!
//! Rule (see `docs/security.md`): anything that could carry a credential —
//! shell output, file contents, HTTP responses, logs, persisted text — passes
//! through a [`Redactor`] before it is stored, displayed, or sent back to a
//! provider.
//!
//! The redactor is deliberately dependency-free and conservative: it catches
//! known API-key shapes, `Authorization` headers, `KEY=value` assignments, and
//! PEM private-key blocks. It is a safety net, not a guarantee.

use std::collections::BTreeMap;

const REDACTED: &str = "[REDACTED]";

/// Known secret values plus shape-based patterns.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Secret value -> environment variable / label it came from.
    secrets: BTreeMap<String, String>,
}

impl Redactor {
    /// A redactor that performs no substitution (used when disabled).
    pub fn none() -> Self {
        Self::default()
    }

    /// Collect every credential-looking environment variable in this process.
    pub fn from_env() -> Self {
        let mut redactor = Self::default();
        for (name, value) in std::env::vars() {
            if is_secret_env_name(&name) && value.trim().len() >= 8 {
                redactor = redactor.with_secret_named(name, value);
            }
        }
        redactor
    }

    pub fn with_secret(mut self, value: impl Into<String>) -> Self {
        self.insert(REDACTED.to_string(), value.into());
        self
    }

    pub fn with_secret_named(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.insert(name.into(), value.into());
        self
    }

    fn insert(&mut self, name: String, value: String) {
        let value = value.trim().to_string();
        if value.len() >= 8 {
            self.secrets.insert(value, name);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace credentials in `text`.
    pub fn redact(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut out = text.to_string();
        out = redact_private_key_blocks(&out);
        out = redact_literals(&out, &self.secrets);
        out = redact_token_shapes(&out);
        redact_assignments(&out)
    }

    /// Convenience wrapper returning `None` for empty input.
    pub fn redact_opt(&self, text: &str) -> Option<String> {
        if text.is_empty() {
            None
        } else {
            Some(self.redact(text))
        }
    }
}

fn redact_literals(text: &str, secrets: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (secret, name) in secrets {
        if secret.is_empty() || !out.contains(secret.as_str()) {
            continue;
        }
        let replacement = format!("[REDACTED:{name}]");
        out = out.replace(secret.as_str(), &replacement);
    }
    out
}

fn is_secret_env_name(name: &str) -> bool {
    if is_benign_key_name(name) {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    upper.ends_with("API_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_PASSWD")
        || upper.ends_with("ACCESS_KEY")
        || upper == "TOKEN"
        || upper == "PASSWORD"
}

fn is_benign_key_name(name: &str) -> bool {
    const BENIGN: &[&str] = &[
        "max_tokens",
        "tokens",
        "token_count",
        "tokenizer",
        "max_output_tokens",
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "no_proxy",
    ];
    let lower = name.trim().trim_matches('"').to_ascii_lowercase();
    BENIGN.contains(&lower.as_str())
}

fn is_secret_key_name(name: &str) -> bool {
    if is_benign_key_name(name) {
        return false;
    }
    let lower = name.trim().trim_matches('"').to_ascii_lowercase();
    lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("api-key")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("credential")
        || lower.contains("authorization")
        || lower.contains("access_key")
        || lower.contains("private_key")
        || lower.ends_with("_token")
        || lower == "token"
}

fn looks_like_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty()
        || trimmed.starts_with("[REDACTED")
        || trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
}

const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AIza",
    "AKIA",
    "ASIA",
];

fn redact_token_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(prefix) = TOKEN_PREFIXES
            .iter()
            .find(|prefix| text[index..].starts_with(*prefix))
        {
            let body_start = index + prefix.len();
            let body_end = body_end(bytes, body_start);
            // Short bodies are not credentials (e.g. "sk-a" in prose).
            if body_end - body_start >= 16 {
                out.push_str(REDACTED);
                index = body_end;
                continue;
            }
        }
        let next = next_char_boundary(text, index);
        out.push_str(&text[index..next]);
        index = next;
    }
    out
}

fn body_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && is_token_body_byte(bytes[end]) {
        end += 1;
    }
    end
}

fn is_token_body_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    let mut next = index + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

fn redact_private_key_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const PRIVATE: &str = "PRIVATE KEY-----";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after_begin = &rest[start..];
        // Only PEM blocks that carry private key material are secrets.
        let header_end = after_begin.find('\n').unwrap_or(after_begin.len());
        if !after_begin[..header_end].contains(PRIVATE) {
            out.push_str(&rest[..start + BEGIN.len()]);
            rest = &rest[start + BEGIN.len()..];
            continue;
        }
        out.push_str(&rest[..start]);
        let end_marker = after_begin.find("-----END ").map(|pos| {
            let tail = &after_begin[pos..];
            let end_len = tail.find('\n').map(|n| n + 1).unwrap_or(tail.len());
            pos + end_len
        });
        match end_marker {
            Some(pos) => {
                out.push_str("[REDACTED PRIVATE KEY]");
                rest = &after_begin[pos..];
            }
            None => {
                // Unterminated block: drop the remainder rather than leak it.
                out.push_str("[REDACTED PRIVATE KEY]");
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive('\n') {
        let (line, newline) = match chunk.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (chunk, ""),
        };
        out.push_str(&redact_assignment_line(line));
        out.push_str(newline);
    }
    out
}

fn redact_assignment_line(line: &str) -> String {
    // `Authorization: Bearer <token>` on its own line or in a header dump.
    let lower = line.to_ascii_lowercase();
    if let Some(index) = lower.find("authorization") {
        if let Some(separator) = line[index..].find([':', '=']) {
            if let Some(redacted) = redact_value(line, index + separator + 1, 8) {
                return redacted;
            }
        }
    }

    for separator in ["=", ": "] {
        if let Some(index) = line.find(separator) {
            let key = &line[..index];
            if is_secret_key_name(key) {
                if let Some(redacted) = redact_value(line, index + separator.len(), 4) {
                    return redacted;
                }
            }
        }
    }
    line.to_string()
}

/// Replace only the value token starting at `value_start`, keeping the rest of
/// the line intact (so `{"api_key": "x", "model": "y"}` keeps the model).
/// Returns `None` when the value is too short or already redacted.
fn redact_value(line: &str, value_start: usize, min_len: usize) -> Option<String> {
    let rest = line.get(value_start..)?;
    let padding = rest.len() - rest.trim_start().len();
    let value = rest.get(padding..)?;
    let end = value_token_end(value);
    let token = value.get(..end)?;
    let token = token.trim_end();
    if token.len() < min_len || looks_like_placeholder(token) {
        return None;
    }
    let token_start = value_start + padding;
    let token_end = token_start + end;
    Some(format!(
        "{}{REDACTED}{}",
        &line[..token_start],
        &line[token_end..]
    ))
}

/// End offset of the value token: a quoted string ends at its closing quote,
/// an unquoted value ends at the next comma.
fn value_token_end(value: &str) -> usize {
    let bytes = value.as_bytes();
    if bytes.first() == Some(&b'"') {
        let mut index = 1;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => index += 2,
                b'"' => return index + 1,
                _ => index += 1,
            }
        }
        return value.len();
    }
    value.find(',').unwrap_or(value.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_secrets_are_replaced_with_their_label() {
        let redactor =
            Redactor::none().with_secret_named("DEEPSEEK_API_KEY", "sk-live-abcdefghijklmnop");
        let output = redactor.redact("using sk-live-abcdefghijklmnop now");
        assert_eq!(output, "using [REDACTED:DEEPSEEK_API_KEY] now");
    }

    #[test]
    fn short_values_are_not_treated_as_secrets() {
        let redactor = Redactor::none().with_secret_named("SHORT", "abc");
        assert_eq!(redactor.redact("abc"), "abc");
        assert!(redactor.is_empty());
    }

    #[test]
    fn token_shapes_are_redacted() {
        let redactor = Redactor::none();
        let output = redactor.redact("key sk-proj-1234567890abcdefghij end");
        assert_eq!(output, "key [REDACTED] end");
        assert!(!output.contains("1234567890"));
    }

    #[test]
    fn short_prefix_matches_survive() {
        let redactor = Redactor::none();
        assert_eq!(redactor.redact("sk-a and sk-"), "sk-a and sk-");
    }

    #[test]
    fn assignments_and_headers_are_redacted() {
        let redactor = Redactor::none();
        let output = redactor
            .redact("OPENAI_API_KEY=secretvalue123\nAuthorization: Bearer abcdefghijklmnop\nok\n");
        assert!(output.contains("OPENAI_API_KEY=[REDACTED]"), "{output}");
        assert!(!output.contains("abcdefghijklmnop"), "{output}");
        assert!(output.ends_with("ok\n"));
    }

    #[test]
    fn quoted_values_leave_the_rest_of_the_line_alone() {
        let redactor = Redactor::none();
        let output =
            redactor.redact("{\"api_key\": \"secret-value\", \"model\": \"deepseek-chat\"}");
        assert_eq!(
            output,
            "{\"api_key\": [REDACTED], \"model\": \"deepseek-chat\"}"
        );
    }

    #[test]
    fn token_counts_are_preserved() {
        let redactor = Redactor::none();
        let output = redactor.redact("max_tokens=1024\nprompt_tokens: 12\ntotal_tokens: 30\n");
        assert_eq!(
            output,
            "max_tokens=1024\nprompt_tokens: 12\ntotal_tokens: 30\n"
        );
    }

    #[test]
    fn private_key_blocks_are_removed() {
        let redactor = Redactor::none();
        let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\nafter\n";
        let output = redactor.redact(text);
        assert!(!output.contains("MIIEow"), "{output}");
        assert!(output.contains("[REDACTED PRIVATE KEY]"));
        assert!(output.starts_with("before"));
        assert!(output.ends_with("after\n"));
    }

    #[test]
    fn unterminated_private_key_blocks_do_not_leak() {
        let redactor = Redactor::none();
        let output = redactor.redact("-----BEGIN PRIVATE KEY-----\nMIIEow");
        assert!(!output.contains("MIIEow"));
    }

    #[test]
    fn unicode_and_multiline_text_survive() {
        let redactor = Redactor::none().with_secret_named("JAI_API_KEY", "tok-abcdefghij");
        let text = "密钥: 秘密\ntok-abcdefghij\nمرحبا بالعالم\n";
        let output = redactor.redact(text);
        assert!(output.starts_with("密钥: 秘密\n"));
        assert!(output.contains("[REDACTED:JAI_API_KEY]"));
        assert!(output.contains("مرحبا بالعالم"));
    }

    #[test]
    fn env_names_are_classified() {
        assert!(is_secret_env_name("DEEPSEEK_API_KEY"));
        assert!(is_secret_env_name("HF_TOKEN"));
        assert!(!is_secret_env_name("MAX_TOKENS"));
        assert!(!is_secret_env_name("PATH"));
    }

    #[test]
    fn json_content_is_handled() {
        let redactor = Redactor::none();
        let output = redactor.redact(r#"{"api_key": "abcdefgh", "model": "deepseek-chat"}"#);
        assert!(output.contains("[REDACTED]"), "{output}");
        assert!(!output.contains("abcdefgh"), "{output}");
        assert!(output.contains("deepseek-chat"), "{output}");
    }
}
