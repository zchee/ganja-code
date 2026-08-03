//! Sources of assistant text.
//!
//! A provider turns a [`ChatRequest`] into a stream of [`ProviderEvent`]s; the
//! engine is what maps those onto the protocol frontends see. P2 wave 1 ships
//! one provider — [`FakeProvider`] — and the trait the HTTP ones implement.

use std::{
    env::{self, VarError},
    sync::Arc,
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::protocol::{FinishReason, Message, Usage};

pub mod fake;

pub use fake::FakeProvider;

/// Environment variable naming the provider a session talks to.
pub const PROVIDER_ENV: &str = "GANJA_PROVIDER";

/// One request to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRequest {
    /// Model identifier, spelled the way the provider expects it on the wire.
    pub model: String,
    /// System prompt. P2 has nothing to put here; P5 fills it from `AGENTS.md`
    /// and the agent definitions.
    pub system: Option<String>,
    /// The conversation so far, oldest first, ending with the message the user
    /// just sent.
    pub messages: Vec<Message>,
}

/// Something a provider reported while answering.
///
/// The tool variants exist so that P3 can execute tool calls without reshaping
/// the trait; the engine currently ignores them, because no protocol part
/// renders them yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The next fragment of the reply.
    TextDelta(String),
    /// The next fragment of the model's thinking.
    ReasoningDelta(String),
    /// The model started calling a tool.
    ToolCallStart {
        /// Correlates the call's fragments and its result.
        id: String,
        /// Tool being called.
        name: String,
    },
    /// The next fragment of a tool call's JSON arguments.
    ToolCallDelta {
        /// Call the fragment belongs to.
        id: String,
        /// Fragment of the arguments, which are only valid JSON once joined.
        json: String,
    },
    /// A tool call's arguments are complete.
    ToolCallEnd {
        /// Call that is now complete.
        id: String,
    },
    /// What the turn cost.
    Usage(Usage),
    /// The model stopped, and why.
    Finish(FinishReason),
}

/// A provider could not answer.
///
/// The variants are transport-agnostic on purpose: `ganja-core` does not
/// depend on an HTTP client, and the same taxonomy has to fit the SSE
/// providers P2 wave 2 adds.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    /// No usable credentials for the provider.
    #[error("no usable credentials: {0}")]
    Auth(String),
    /// The request never produced a response.
    #[error("the request did not complete: {0}")]
    Transport(String),
    /// The provider answered, unsuccessfully.
    #[error("the provider answered {status}: {message}")]
    Status {
        /// HTTP status the provider returned.
        status: u16,
        /// What it said, trimmed to something a status bar can hold.
        message: String,
    },
    /// A response arrived but could not be understood.
    #[error("the response could not be parsed: {0}")]
    Parse(String),
}

/// A source of assistant text.
///
/// One call to [`Provider::stream`] serves one turn. `cancel` fires when the
/// user interrupts, and implementations are expected to stop reading — for an
/// HTTP provider that means abandoning the response body, which aborts the
/// request.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identifier accepted by [`PROVIDER_ENV`].
    fn id(&self) -> &str;

    /// Streams the reply to `request`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the turn cannot start at all: bad
    /// credentials, an unreachable endpoint, or a rejected request.
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;
}

/// A provider together with the model to ask, and anything the user should be
/// told about how the two were picked.
pub struct Selection {
    /// The provider to drive the session with.
    pub provider: Arc<dyn Provider>,
    /// Model identifier handed to every [`ChatRequest`].
    pub model: String,
    /// Set when the provider was defaulted rather than requested.
    pub notice: Option<String>,
}

/// The value of [`PROVIDER_ENV`] names no provider this build can serve.
#[derive(Debug, thiserror::Error)]
#[error(
    "unsupported {PROVIDER_ENV}={requested:?}; this build only ships the {available:?} provider"
)]
pub struct UnknownProvider {
    requested: String,
    available: &'static str,
}

/// Resolves the provider named by [`PROVIDER_ENV`].
///
/// An unset variable selects the fake provider and reports a notice, so that a
/// bare `cargo run` still demonstrates a streamed reply while making clear that
/// nothing real is being asked.
///
/// # Errors
///
/// Returns [`UnknownProvider`] when the variable names a provider this build
/// does not have; configuration mistakes fail at startup rather than silently
/// answering with canned text.
pub fn from_env() -> Result<Selection, UnknownProvider> {
    match env::var(PROVIDER_ENV) {
        Ok(requested) if requested == fake::ID => Ok(Selection {
            provider: Arc::new(FakeProvider::default()),
            model: fake::MODEL.to_owned(),
            notice: None,
        }),
        Ok(requested) => Err(UnknownProvider {
            requested,
            available: fake::ID,
        }),
        Err(VarError::NotUnicode(requested)) => Err(UnknownProvider {
            requested: requested.to_string_lossy().into_owned(),
            available: fake::ID,
        }),
        Err(VarError::NotPresent) => Ok(Selection {
            provider: Arc::new(FakeProvider::default()),
            model: fake::MODEL.to_owned(),
            notice: Some(format!(
                "{PROVIDER_ENV} is unset - replying from the built-in {} provider",
                fake::ID
            )),
        }),
    }
}
