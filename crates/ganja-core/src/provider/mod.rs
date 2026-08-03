//! Sources of assistant text.
//!
//! P1 ships one provider — [`FakeProvider`] — but the selection hook already
//! exists because every demo and end-to-end test drives ganja through it.

use std::{
    env::{self, VarError},
    sync::Arc,
};

use futures::stream::BoxStream;

pub mod fake;

pub use fake::FakeProvider;

/// Environment variable naming the provider a session talks to.
pub const PROVIDER_ENV: &str = "GANJA_PROVIDER";

/// A source of assistant text.
///
/// One call to [`Provider::stream`] serves one turn. The engine aborts a turn
/// by dropping the returned stream, so implementations release their resources
/// on drop rather than waiting for a cancellation signal.
pub trait Provider: Send + Sync {
    /// Identifier accepted by [`PROVIDER_ENV`].
    fn id(&self) -> &str;

    /// Streams the reply to `prompt` as text fragments.
    fn stream(&self, prompt: &str) -> BoxStream<'static, String>;
}

/// A provider together with anything the user should be told about how it was
/// picked.
pub struct Selection {
    /// The provider to drive the session with.
    pub provider: Arc<dyn Provider>,
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
            notice: Some(format!(
                "{PROVIDER_ENV} is unset - replying from the built-in {} provider",
                fake::ID
            )),
        }),
    }
}
