//! Provider interface: anything that can stream OpenAI-style chat completions.

use crate::error::Result;
use crate::message::{ChatEvent, ChatRequest, ModelInfo};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;

/// A boxed future, so the trait does not need an async-trait macro.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A chat backend.
///
/// Implementations must never print, never read configuration, and never touch
/// local storage: they only talk HTTP and report what the server said.
pub trait Provider: Send + Sync {
    /// Provider name as selected on the command line.
    fn name(&self) -> &str;

    /// API root the provider talks to (for display and errors).
    fn base_url(&self) -> &str;

    /// Stream one assistant turn.
    ///
    /// The returned receiver yields [`ChatEvent`]s and then closes. Errors are
    /// reported as `Err` items so that partial output is not lost. Dropping the
    /// receiver cancels the request.
    fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> BoxFuture<'_, Result<mpsc::Receiver<Result<ChatEvent>>>>;

    /// Models advertised by `GET /models`.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>>>;
}
