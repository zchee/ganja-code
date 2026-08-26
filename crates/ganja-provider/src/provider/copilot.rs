//! GitHub Copilot, which is a subscription answering on the API
//! [`super::openai`] already speaks.
//!
//! Spec: upstream `packages/opencode/src/plugin/github-copilot/copilot.ts`,
//! whose request half is four headers and a bearer (`:74-79`, `:363`), and
//! `packages/llm/src/providers/github-copilot.ts`, which routes a Copilot model
//! through the plain chat-completions protocol and supplies nothing but a base
//! URL and a bearer to do it.
//!
//! So this is [`super::grok`]'s shape rather than [`super::responses`]'s: a base
//! URL, a credential source, and a header set over the chat-completions
//! provider, and **not a second request/response mapping**. That
//! `api.githubcopilot.com/chat/completions` really does speak that API — with a
//! raw GitHub token and nothing else — is something this build measured against
//! the live endpoint rather than inferred. A change here that starts encoding
//! messages or decoding frames is a sign the endpoint stopped being compatible,
//! and that is a new provider rather than a bigger version of this one.
//!
//! # What the credential is
//!
//! The `gho_…` token the device login stored, presented verbatim. There is **no
//! exchange** — no `copilot_internal/v2/token` step exists anywhere in the pin,
//! and the live spike confirmed the raw token is accepted — so a future
//! "improvement" that trades the stored token for something before putting it on
//! the wire is a regression, and `tests/copilot_wire.rs` pins the bearer against
//! the stored token byte for byte so that it reddens rather than passes.
//!
//! It also never expires ([`auth::copilot`]'s `expires: 0`), which is why this
//! module's `NeverRenews` is a refusal rather than an endpoint: there is no
//! renewal path to call, and the only thing that repairs a Copilot credential
//! the endpoint has stopped accepting is another login.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, AuthError, OauthCredential, RefreshOauth},
    provider::{
        ChatRequest, CredentialSource, OpenAiProvider, Provider, ProviderError, ProviderEvent,
        check_base_url,
    },
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
///
/// [`auth::copilot::PROVIDER_ID`] rather than a second literal saying the same
/// thing, for the reason [`super::grok::ID`] is `auth::grok`'s: a login writing
/// under one name while a provider reads under another fails as a storage bug
/// and is debugged as one. Unlike grok there is no alias underneath — upstream
/// calls this provider `github-copilot` too, so the command line, the catalog
/// and the credential file all spell it the one way.
pub const ID: &str = auth::copilot::PROVIDER_ID;

/// The API version every request declares (`copilot.ts:76`, `:363`).
const API_VERSION_HEADER: &str = "x-github-api-version";

/// What kind of request the endpoint is being asked to serve
/// (`copilot.ts:78`).
const INTENT_HEADER: &str = "openai-intent";

/// The value [`INTENT_HEADER`] carries.
const INTENT: &str = "conversation-edits";

/// Who caused this request — a person, or the agent acting on its own
/// (`copilot.ts:79`).
const INITIATOR_HEADER: &str = "x-initiator";

/// The value [`INITIATOR_HEADER`] carries.
///
/// Always `user`, because every request ganja sends is one a person's prompt
/// asked for: even a step deep inside a turn is downstream of a prompt, and
/// there is no path here — no background summarizer, no speculative call — that
/// a person did not start. Upstream distinguishes the two by whether the last
/// message is the user's; a build with no other kind of request has nothing to
/// distinguish.
const INITIATOR: &str = "user";

/// Streams replies from GitHub Copilot, authenticated by the OAuth credential a
/// `ganja auth login github-copilot` stored.
///
/// A newtype over [`OpenAiProvider`] rather than a flag on it, for
/// [`super::grok::GrokProvider`]'s reason: the wire is shared but which provider
/// a turn is running as is not a detail — [`Provider::id`] is what the session
/// layer sizes and prices a turn by.
#[derive(Debug)]
pub struct CopilotProvider(OpenAiProvider);

impl CopilotProvider {
    /// The provider against whichever deployment the stored login is for.
    ///
    /// This reads the credential store, which [`super::grok::GrokProvider::
    /// from_stored`] deliberately does not — and the difference is the one thing
    /// about this provider that is not grok's shape. What is read is not the
    /// token but the **deployment**: a Copilot request goes to `github.com`'s
    /// API base or to an enterprise one derived from the `enterpriseUrl` stored
    /// beside the token (`copilot.ts:26-28`), and that is a property of the
    /// login rather than of the credential — it cannot change without another
    /// login, so reading it once is reading it as often as it can differ. The
    /// token itself is still resolved per request, so a login that happens
    /// *after* this session started is picked up by the next request.
    ///
    /// A store with nothing in it, or one that cannot be read at all, yields the
    /// public deployment rather than an error. That is deliberate and it is
    /// grok's posture inherited verbatim: neither provider refuses to be built
    /// over a missing credential, because the failure a person needs to see is
    /// the one that names the login, and that message is produced once, at the
    /// first request, by `CredentialSource::resolved`. Failing here as well would
    /// be a second, earlier, differently-worded version of the same refusal.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// which in practice means the TLS backend failed to initialize.
    pub fn from_stored() -> Result<Self, ProviderError> {
        Self::at(stored_api_base())
    }

    /// The same provider against an endpoint of the caller's choosing, which is
    /// how a test drives it against a loopback socket.
    ///
    /// There is no `refresh` parameter, unlike every other OAuth provider here:
    /// a Copilot credential has no renewal path at all, so there is no endpoint
    /// half for a caller to choose. See `NeverRenews`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built, or
    /// when `base_url` is somewhere an access token may not travel — the same
    /// rule every other provider's endpoint is held to, applied here so that a
    /// bad endpoint fails at construction as well as at the request.
    pub fn at(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self(
            OpenAiProvider::with_credential(
                CredentialSource::Oauth {
                    provider_id: ID,
                    refresh: Arc::new(NeverRenews),
                },
                base_url,
            )?
            .with_headers(headers()),
        ))
    }
}

/// Where the stored login says this provider's requests go.
///
/// [`auth::copilot::api_base_for`] owns the derivation, so a domain spelled any
/// of the ways a person can spell it reaches the same base; all this decides is
/// whether there is a domain to derive from.
fn stored_api_base() -> String {
    let stored = match auth::oauth_for(ID) {
        Ok(stored) => stored,
        Err(error) => {
            // Not returned: the same read happens again, properly, when the
            // first request resolves its credential, and that is where the
            // message a person can act on comes from. Reporting it here would
            // put a second copy of one failure in front of them, phrased as
            // though the endpoint were the problem.
            tracing::debug!(
                provider_id = ID,
                %error,
                "the Copilot deployment could not be read; assuming github.com"
            );
            None
        }
    };

    stored
        .and_then(|credential| credential.enterprise_url)
        .map_or_else(
            || auth::copilot::DEFAULT_API_BASE.to_owned(),
            |domain| auth::copilot::api_base_for(&domain),
        )
}

/// The headers every Copilot request carries beside the bearer.
///
/// All four are upstream's (`copilot.ts:74-79`) and all four were confirmed
/// against the live endpoint together, which is why they are one function rather
/// than four decisions: nothing here knows which of them
/// `api.githubcopilot.com` would still serve a request without, and finding out
/// is not something a turn should be doing.
///
/// Every value is a compile-time constant, so the conversions cannot fail on
/// anything a caller supplies.
fn headers() -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(4);

    for (name, value) in [
        // Upstream's own product name, for the reason `UPSTREAM_USER_AGENT`
        // records: the token was minted against a client registration that
        // belongs to that project.
        (
            reqwest::header::USER_AGENT,
            auth::device::UPSTREAM_USER_AGENT,
        ),
        (
            HeaderName::from_static(API_VERSION_HEADER),
            auth::copilot::API_VERSION,
        ),
        (HeaderName::from_static(INTENT_HEADER), INTENT),
        (HeaderName::from_static(INITIATOR_HEADER), INITIATOR),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }

    headers
}

/// The renewal a Copilot credential does not have.
///
/// [`CredentialSource::Oauth`] wants a [`RefreshOauth`], and this credential has no
/// endpoint that would implement one: the token never expires
/// ([`auth::copilot`]'s `expires: 0`), so [`auth::Refresher::usable`] returns it
/// without consulting this at all, at every clock including `u64::MAX` —
/// `auth::copilot`'s own tests pin that as a property of the credential.
///
/// It is therefore unreachable, and what makes it worth writing anyway is *how*
/// it is unreachable. A refusal is the honest answer if the shared expiry rule
/// ever changes under this provider: there would still be no endpoint to ask,
/// and the only thing that repairs a Copilot credential is another login —
/// which is exactly what [`AuthError::ReauthRequired`] means and what its
/// message says. A panic would take a turn down over a credential that is
/// probably still good, and a silent success would hand the caller a token it
/// had claimed to renew.
struct NeverRenews;

#[async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        Err(AuthError::ReauthRequired {
            provider_id: provider_id.to_owned(),
            reason: "a GitHub Copilot credential has no renewal endpoint".to_owned(),
        })
    }
}

#[async_trait]
impl Provider for CopilotProvider {
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

    /// Delegated for [`super::grok`]'s reason: the buckets belong to whichever
    /// wire the response reached, not to the name wrapped around it.
    fn rate_windows(&self) -> Vec<super::RateWindow> {
        self.0.rate_windows()
    }

    /// Delegated for the same reason, to the same wire (**D485**).
    fn plan_windows(&self) -> Vec<super::PlanWindow> {
        self.0.plan_windows()
    }
}

#[cfg(test)]
#[path = "copilot_tests.rs"]
mod tests;
