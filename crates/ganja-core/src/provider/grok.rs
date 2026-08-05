//! Grok, which is xAI's endpoint speaking the API [`super::openai`] already
//! speaks.
//!
//! Spec: upstream `packages/opencode/src/plugin/xai.ts:460-528`. Two things
//! there decide the whole shape of this module:
//!
//! - `:473-476` — upstream deliberately does **not** set a base URL, because
//!   `@ai-sdk/xai` already defaults to `https://api.x.ai/v1` and overriding it
//!   "would silently route around a user-configured gateway". Ganja has no such
//!   SDK to inherit a default from, so the URL is written down here; what
//!   carries over is that it is one constant in one place rather than a literal
//!   in a request builder.
//! - `:477-528` — the credential is injected by a `fetch` override rather than
//!   captured when the provider is loaded, because an access token expires
//!   under a session and the loader runs once.
//!
//! So this is a base URL and a credential source over the chat-completions
//! provider, and **not a second request/response mapping**: `api.x.ai/v1` is
//! OpenAI-compatible, which a live spike against `grok-4.3` measured rather
//! than assumed. A change here that starts encoding messages or decoding frames
//! is a sign the endpoint stopped being compatible, and that is a new provider
//! rather than a bigger version of this one.
//!
//! **Ganja calls this provider `grok`; the credential file calls it `xai`.**
//! [`ID`] is [`auth::grok::PROVIDER_ID`] rather than a second literal saying the
//! same thing, because a login writing under one name while a provider reads
//! under another fails as a storage bug and is debugged as one.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    provider::{
        ChatRequest, Credential, OpenAiProvider, Provider, ProviderError, ProviderEvent,
        check_base_url,
    },
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
pub const ID: &str = auth::grok::PROVIDER_ID;

/// Where xAI's API lives — the default `@ai-sdk/xai` supplies upstream
/// (`xai.ts:474`), written down because nothing here supplies one.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// Streams replies from xAI, authenticated by the OAuth credential a
/// `ganja auth login grok` stored.
///
/// A newtype over [`OpenAiProvider`] rather than a flag on it: the wire is the
/// same and is shared, but which provider a turn is running as is not a detail
/// — [`Provider::id`] is what the session layer prices a turn by
/// (`session.rs`, filtering the catalog by provider), so a grok turn reporting
/// itself as `openai` would be billed against the wrong table.
#[derive(Debug)]
pub struct GrokProvider(OpenAiProvider);

impl GrokProvider {
    /// The provider against xAI's own endpoints.
    ///
    /// Nothing is read from the credential store here. The store is consulted
    /// per request, which is what lets a login that happens *after* a session
    /// started be picked up, and what makes a renewal mid-session visible to
    /// the next request rather than to the next process.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// which in practice means the TLS backend failed to initialize.
    pub fn from_stored() -> Result<Self, ProviderError> {
        let refresh = auth::grok::Refresh::new().map_err(|error| {
            // `Refresh::new` fails only where `client()` does, and for the same
            // reason, so it is classified the same way: nothing was refused.
            ProviderError::Transport(error.to_string())
        })?;

        Self::at(DEFAULT_BASE_URL, Arc::new(refresh))
    }

    /// The same provider against endpoints of the caller's choosing, which is
    /// how a test drives it against a loopback socket.
    ///
    /// `refresh` is the endpoint half of a renewal — [`auth::grok::Refresh::at`]
    /// for a token endpoint that is not xAI's. The rest of a renewal belongs to
    /// [`auth::Refresher`] and is not the caller's to choose.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// or when `base_url` is somewhere an access token may not travel — the
    /// same rule every other provider's endpoint is held to, applied here so
    /// that a bad endpoint fails at construction as well as at the request.
    pub fn at(
        base_url: impl Into<String>,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self(OpenAiProvider::with_credential(
            Credential::Oauth {
                provider_id: ID,
                refresh,
            },
            base_url,
        )?))
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.0.stream(request, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{DEFAULT_BASE_URL, GrokProvider, ID};
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        catalog,
        provider::{PROVIDERS, Provider as _, ProviderError},
    };

    /// A renewal that must never run, for the cases that are about construction
    /// rather than about a token endpoint.
    struct NeverRenews;

    #[async_trait::async_trait]
    impl RefreshOauth for NeverRenews {
        async fn refresh(
            &self,
            provider_id: &str,
            _credential: &OauthCredential,
        ) -> Result<OauthCredential, AuthError> {
            panic!("{provider_id} was renewed by a test that only builds a provider");
        }
    }

    #[test]
    fn ganja_calls_it_grok_everywhere_the_wire_can_see() {
        assert_eq!(ID, "grok");
        assert_eq!(
            ID,
            auth::grok::PROVIDER_ID,
            "one constant, or a login stores under a name the provider does not read"
        );
        assert!(
            PROVIDERS.contains(&ID),
            "a provider nothing can select is a provider nobody has"
        );
        // What the credential file calls this provider is deliberately not
        // written down anywhere in `provider/`, not even in an assertion:
        // `auth::storage_key` owns that translation and `auth::grok`'s own
        // tests pin it. A second spelling here would be a second opinion about
        // where the credential lives.
        assert_ne!(
            auth::storage_key(ID),
            ID,
            "the store's name for this provider is not ganja's, and only `auth` knows it"
        );
    }

    #[test]
    fn the_endpoint_is_xais_own_and_speaks_chat_completions() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.x.ai/v1");

        let provider = GrokProvider::from_stored().expect("a client builds");
        assert_eq!(provider.id(), ID, "not the wire it borrows");

        // A provider renders as which provider it is and where it points, and
        // never as what it authenticates with — this one has nothing to render
        // yet, because the token is not fetched until a request needs it.
        let rendered = format!("{provider:?}");
        assert!(
            rendered.contains("Oauth") && rendered.contains("grok"),
            "{rendered}"
        );
        assert!(
            rendered.contains("https://api.x.ai/v1"),
            "the endpoint is what tells one provider from another: {rendered}"
        );
    }

    /// The endpoint is not exempt from the rule the base URL of every other
    /// provider is held to just because the credential arrived as a token
    /// rather than as a key.
    #[test]
    fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
        let refused = GrokProvider::at("http://api.x.ai/v1", Arc::new(NeverRenews))
            .expect_err("plain http to a public host puts the token on the wire in the clear");

        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
        assert!(
            GrokProvider::at("http://127.0.0.1:8080/v1", Arc::new(NeverRenews)).is_ok(),
            "loopback never reaches a network, which is what a test depends on"
        );
    }

    /// The other half of the obligation `catalog`'s own table test states: a
    /// provider a session can select has to be one the catalog can size and
    /// price, or the first turn has no model to ask for and no cost to report.
    #[test]
    fn a_grok_session_that_names_no_model_gets_one_the_catalog_can_price() {
        let id = catalog::default_model(ID).expect("grok has a pinned default");
        let info = catalog::model(id).expect("the default is in the table");

        assert_eq!(info.provider_id, ID);
        assert!(info.context_window > 0 && info.max_output > 0);
        assert!(
            info.pricing.input > 0.0 && info.pricing.output > 0.0,
            "a priced provider with a free row is a row nobody filled in"
        );
    }
}
