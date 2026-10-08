//! Mapping HTTP/transport failures onto jAI's error taxonomy (and exit codes).

use jai_core::Error;
use reqwest::StatusCode;

/// Turn a non-success HTTP response into an error the CLI can exit on.
pub fn map_status(provider: &str, status: StatusCode, body: &str) -> Error {
    let message = extract_message(body);
    match status.as_u16() {
        401 | 403 => Error::auth(provider, message),
        402 => Error::api(
            provider,
            402,
            format!("insufficient balance or credits: {message}"),
        ),
        404 => Error::api(
            provider,
            404,
            format!("endpoint or model not found: {message}"),
        ),
        408 => Error::network(format!("{provider}: request timeout: {message}")),
        429 => Error::api(provider, 429, format!("rate limited: {message}")),
        code => Error::api(provider, code, message),
    }
}

/// Turn a transport failure into a network or protocol error.
pub fn map_transport(provider: &str, err: reqwest::Error) -> Error {
    if err.is_timeout() {
        return Error::network(format!("{provider}: request timed out"));
    }
    if err.is_connect() {
        return Error::network(format!("{provider}: could not connect: {err}"));
    }
    if err.is_body() || err.is_decode() {
        return Error::api(provider, 0, format!("malformed response body: {err}"));
    }
    Error::network(format!("{provider}: {err}"))
}

/// Best-effort extraction of a provider's error message.
///
/// Handles `{"error": {"message": ...}}`, `{"error": "..."}`, `{"message": ...}`
/// and finally falls back to a truncated raw body.
pub fn extract_message(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty error response".to_string();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message").or(Some(error)))
            .and_then(|message| message.as_str())
        {
            return message.to_string();
        }
        if let Some(message) = value.get("message").and_then(|value| value.as_str()) {
            return message.to_string();
        }
    }
    truncate(trimmed, 512)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > max {
            break;
        }
        end = next;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failures_map_to_exit_code_three() {
        let error = map_status(
            "deepseek",
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Authentication Fails"}}"#,
        );
        assert!(error.to_string().contains("Authentication Fails"));
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn insufficient_balance_is_reported_clearly() {
        let error = map_status(
            "deepseek",
            StatusCode::PAYMENT_REQUIRED,
            r#"{"error":{"message":"Insufficient Balance"}}"#,
        );
        assert!(
            error.to_string().contains("insufficient balance"),
            "{error}"
        );
        assert_eq!(error.exit_code(), 5);
    }

    #[test]
    fn rate_limits_and_bad_requests_map_to_api_errors() {
        assert_eq!(
            map_status("openai", StatusCode::TOO_MANY_REQUESTS, "{}").exit_code(),
            5
        );
        assert_eq!(
            map_status("openai", StatusCode::BAD_REQUEST, "invalid model").exit_code(),
            5
        );
        assert_eq!(
            map_status("openai", StatusCode::NOT_FOUND, "nope").exit_code(),
            5
        );
    }

    #[test]
    fn message_extraction_handles_all_shapes() {
        assert_eq!(extract_message(r#"{"error":{"message":"a"}}"#), "a");
        assert_eq!(extract_message(r#"{"error":"b"}"#), "b");
        assert_eq!(extract_message(r#"{"message":"c"}"#), "c");
        assert_eq!(extract_message("plain text"), "plain text");
        assert_eq!(extract_message("   "), "empty error response");
    }

    #[test]
    fn long_bodies_are_truncated_on_char_boundaries() {
        let body = "实现".repeat(400);
        let message = extract_message(&body);
        assert!(message.ends_with('…'));
        assert!(!message.contains('\u{FFFD}'));
    }
}
