//! [`NETWORK_REQUEST`]: a deliberately small HTTP client.
//!
//! Only `http`/`https` URLs, only one request per call, response bodies capped
//! and returned as text. Network tools are disabled in the default config.

use crate::NETWORK_REQUEST;
use jai_core::error::{Error, Result};
use jai_core::text::truncate_bytes;
use jai_core::tool::ToolOutcome;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Arguments of [`NETWORK_REQUEST`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpArgs {
    pub url: String,
    /// `GET`, `HEAD`, `POST`, `PUT`, `PATCH` or `DELETE` (default `GET`).
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub body: Option<String>,
}

const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Perform the request and return status, headers and body.
pub async fn request(
    client: &reqwest::Client,
    args: &HttpArgs,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<ToolOutcome> {
    let url = args.url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::tool(
            NETWORK_REQUEST,
            "only http:// and https:// URLs are supported",
        ));
    }
    let method = args
        .method
        .as_deref()
        .unwrap_or("GET")
        .trim()
        .to_ascii_uppercase();
    if !ALLOWED_METHODS.contains(&method.as_str()) {
        return Err(Error::tool(
            NETWORK_REQUEST,
            format!(
                "method `{method}` is not allowed (use {})",
                ALLOWED_METHODS.join(", ")
            ),
        ));
    }

    let mut builder = client.request(
        reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| Error::tool(NETWORK_REQUEST, format!("invalid method `{method}`")))?,
        url,
    );
    if let Some(headers) = &args.headers {
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    if let Some(body) = &args.body {
        builder = builder.body(body.clone());
    }

    let response = builder
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| Error::tool(NETWORK_REQUEST, format!("request failed: {err}")))?;

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| Error::tool(NETWORK_REQUEST, format!("could not read body: {err}")))?;

    let body = String::from_utf8_lossy(&bytes);
    let (body, truncated) = truncate_bytes(&body, max_output_bytes);
    let content = format!(
        "status: {}\ncontent-type: {content_type}\n\n{body}",
        status.as_u16()
    );
    Ok(ToolOutcome {
        ok: status.is_success(),
        content,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn non_http_schemes_are_rejected() {
        let error = request(
            &client(),
            &HttpArgs {
                url: "file:///etc/passwd".into(),
                ..HttpArgs::default()
            },
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("only http://"), "{error}");
    }

    #[tokio::test]
    async fn unsupported_methods_are_rejected() {
        let error = request(
            &client(),
            &HttpArgs {
                url: "http://127.0.0.1:1/".into(),
                method: Some("TRACE".into()),
                ..HttpArgs::default()
            },
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not allowed"), "{error}");
    }

    #[tokio::test]
    async fn connection_failures_are_tool_errors() {
        let error = request(
            &client(),
            &HttpArgs {
                url: "http://127.0.0.1:1/".into(),
                ..HttpArgs::default()
            },
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(error.exit_code(), 6);
        assert!(error.to_string().contains("request failed"), "{error}");
    }
}
