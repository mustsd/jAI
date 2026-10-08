//! Error type shared by every jAI crate, plus the exit-code taxonomy.
//!
//! Exit codes (see `jai --help`):
//!
//! | code | meaning                                  |
//! |------|------------------------------------------|
//! | 0    | success                                  |
//! | 1    | unexpected internal/storage/IO failure   |
//! | 2    | usage or configuration error             |
//! | 3    | authentication/authorisation failure     |
//! | 4    | network failure (DNS, TLS, timeout)      |
//! | 5    | provider API error (400/402/422/429/5xx) |
//! | 6    | tool denied by policy or tool failure    |
//! | 130  | cancelled (Ctrl-C)                       |

/// Result alias used across jAI.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in a jAI run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),

    #[error("{provider}: authentication failed: {message}")]
    Auth { provider: String, message: String },

    #[error("network error: {0}")]
    Network(String),

    #[error("{provider} returned HTTP {status}: {message}")]
    Api {
        provider: String,
        status: u16,
        message: String,
    },

    #[error("cancelled")]
    Cancelled,

    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },

    #[error("{0}")]
    Policy(String),

    #[error("local storage error: {0}")]
    Storage(String),

    #[error("{0}")]
    Io(String),

    #[error("{0}")]
    Internal(String),
}

impl Error {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn auth(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Auth {
            provider: provider.into(),
            message: message.into(),
        }
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    pub fn api(provider: impl Into<String>, status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            provider: provider.into(),
            status,
            message: message.into(),
        }
    }

    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::Policy(message.into())
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled)
    }

    /// Process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Config(_) => 2,
            Error::Auth { .. } => 3,
            Error::Network(_) => 4,
            Error::Api { .. } => 5,
            Error::Tool { .. } | Error::Policy(_) => 6,
            Error::Cancelled => 130,
            Error::Storage(_) | Error::Io(_) | Error::Internal(_) => 1,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(Error::config("x").exit_code(), 2);
        assert_eq!(Error::auth("deepseek", "bad key").exit_code(), 3);
        assert_eq!(Error::network("dns").exit_code(), 4);
        assert_eq!(Error::api("openai", 429, "slow down").exit_code(), 5);
        assert_eq!(Error::policy("denied").exit_code(), 6);
        assert_eq!(Error::tool("shell.execute", "boom").exit_code(), 6);
        assert_eq!(Error::Cancelled.exit_code(), 130);
        assert_eq!(Error::storage("disk").exit_code(), 1);
    }

    #[test]
    fn messages_do_not_hide_the_cause() {
        assert!(
            Error::api("deepseek", 402, "Insufficient Balance")
                .to_string()
                .contains("402")
        );
        assert!(Error::auth("openai", "401").to_string().contains("openai"));
    }
}
