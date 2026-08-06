//! The deferred cursor wire: a stub that refuses by name.
//!
//! Cursor's agent backend speaks gRPC, and porting it means taking on protocol
//! knowledge this build has deliberately not taken on yet — the wire, the
//! login flows, the catalog rows and the license review are all deferred
//! together. What ships today is the *identity and the fence*: `cursor` is
//! selectable, so asking for it gets ganja's own refusal rather than a typo's,
//! and CI holds this crate and the engine free of `prost`/`tonic` — so the day
//! the real wire arrives, the red gate forces its gRPC stack into a crate of
//! its own instead of letting it slide in here. The fence is older than what
//! it fences, and deliberately cheaper than an empty crate would have been.
//!
//! Until then: `GANJA_PROVIDER=cursor` builds a session cheaply — grok's
//! construction posture, nothing read at construction — and the first request
//! is refused with [`REFUSAL`]. It rides the uncataloged tier, so it must be
//! told which model to ask for, exactly like a config-declared endpoint.

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, Provider, ProviderError, ProviderEvent};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects the stub.
pub const ID: &str = "cursor";

/// What every request is answered with until the real wire lands.
///
/// Model-facing *and* user-facing — a headless run prints it, a TUI session
/// shows it in the status bar — so it says what to do next rather than only
/// what is missing.
pub const REFUSAL: &str = "cursor support is a stub: this build ships no cursor wire yet, \
     and nothing was sent. Select another provider, or reach an \
     openai-compatible endpoint through the config's `provider` table.";

/// The stub. Holds nothing, so its `Debug` can leak nothing.
#[derive(Debug, Default)]
pub struct CursorProvider;

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &str {
        ID
    }

    /// Refuses before anything exists to cancel or to stream.
    ///
    /// `Transport` is the honest variant of the four: the request did not
    /// complete and no provider answered — `Auth` would send somebody to log
    /// in to a wire that does not exist. Revisit the taxonomy with the real
    /// wire, not before.
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        Err(ProviderError::Transport(REFUSAL.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{super::PROVIDERS, *};

    /// The smallest request there is; the stub must refuse it without reading
    /// it.
    fn request() -> ChatRequest {
        ChatRequest {
            model: "still-imaginary".to_owned(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_stub_refuses_every_request_naming_the_deferral() {
        let refusal = CursorProvider
            .stream(request(), CancellationToken::new())
            .await
            .err()
            .expect("a stub that streamed something would be claiming a wire it does not have");

        let rendered = refusal.to_string();
        assert!(rendered.contains("stub"), "{rendered}");
        assert!(
            rendered.contains("`provider` table"),
            "the refusal has to say what to do instead, not only what is missing: {rendered}"
        );
    }

    #[test]
    fn the_identity_answered_is_the_one_the_shipped_list_carries() {
        assert_eq!(CursorProvider.id(), ID);
        assert!(
            PROVIDERS.contains(&ID),
            "a stub outside the shipped list would be selectable by nobody"
        );
    }

    /// Fieldless is the mechanism, not the aspiration: the day this struct
    /// grows a credential or a channel, this test is the reminder that its
    /// `Debug` is part of the no-secrets surface every other provider holds.
    #[test]
    fn the_debug_rendering_is_the_bare_name() {
        assert_eq!(format!("{CursorProvider:?}"), "CursorProvider");
    }
}
