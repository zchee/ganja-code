//! Endpoints a config names, spoken to in a dialect it names.
//!
//! Spec: upstream's `provider.<id>` config block. Three of its keys are what
//! this port carries, and each has a site:
//!
//! - `options.baseURL` — where the endpoint is
//!   (`packages/opencode/src/provider/provider.ts:356`, which reads
//!   `options?.endpoint ?? options?.baseURL`);
//! - `options.apiKey` — what authenticates it (`:310`). **Not** carried as a
//!   value: ganja's credentials travel the environment or `auth.json` in a
//!   `SecretString` end to end, so a ganja entry names the *variable*
//!   (`key_env`) rather than holding the key;
//! - `npm` — which SDK loads it, which is how upstream spells what wire an
//!   endpoint speaks. Ganja spells the same thing as [`Dialect`], because a
//!   package name is a fact about somebody's node_modules and the wire is a
//!   fact about the endpoint.
//!
//! **This is not a third wire.** Both dialects are the two providers that
//! already ship, built through the seams they publish for exactly this —
//! [`OpenAiProvider::with_credential`] and its two siblings, and the set
//! [`AnthropicProvider`] gained to match. Nothing here encodes a message or
//! decodes a frame, and a change that starts to is a sign the endpoint stopped
//! being compatible, which is a new provider rather than a bigger version of
//! this one ([`super::grok`]'s standing rule, applied to a whole tier).
//!
//! What this module *does* own is the id. A config-named provider reports
//! itself as the name its entry was written under, because [`Provider::id`] is
//! what the session layer prices a turn by and what a permission rule and a
//! status bar name — so an endpoint called `local-llama` reporting itself as
//! `openai` would be billed against the wrong table and disclosed under the
//! wrong name.

use std::fmt;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::provider::{
    AnthropicProvider, ChatRequest, CredentialSource, OpenAiProvider, Provider, ProviderError,
    ProviderEvent, check_base_url,
};

/// The wire a config-named endpoint speaks.
///
/// Two, and deliberately only two: these are the request/response mappings
/// this build already has. A third value here would mean a third mapping,
/// which is a provider rather than a dialect.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Dialect {
    /// `POST {base}/chat/completions`, `Authorization: Bearer` — the de-facto
    /// standard a local llama.cpp, an OpenRouter key or a vLLM deployment all
    /// serve. [`super::openai`] is the wire.
    OpenaiChatCompletions,
    /// `POST {base}/v1/messages`, `x-api-key` plus the pinned
    /// [`API_VERSION`](super::anthropic::API_VERSION).
    /// [`super::anthropic`] is the wire.
    AnthropicMessages,
}

/// Streams replies from an endpoint a config declared.
///
/// A newtype over whichever wire the dialect names, for [`super::grok`]'s
/// reason: the wire is shared, and which provider a turn is running as is not
/// a detail.
pub struct CompatProvider {
    /// The id its config entry was written under — owned, because it is a
    /// name a person chose rather than one this build ships.
    id: String,
    wire: Wire,
}

/// The provider a dialect resolves to.
enum Wire {
    ChatCompletions(OpenAiProvider),
    Messages(AnthropicProvider),
}

impl fmt::Debug for CompatProvider {
    /// Names which provider this is and delegates the rest, so that the
    /// endpoint and the credential are rendered by the wire that already
    /// refuses to render either in the clear.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rendered = formatter.debug_struct("CompatProvider");
        rendered.field("id", &self.id);
        match &self.wire {
            Wire::ChatCompletions(wire) => rendered.field("wire", wire),
            Wire::Messages(wire) => rendered.field("wire", wire),
        }
        .finish()
    }
}

impl CompatProvider {
    /// Builds the provider a config entry describes.
    ///
    /// Plain data by design: an id, a dialect, an endpoint, a credential and a
    /// header set. This constructor never sees a `ganja_core::config::Config`
    /// — reading one is `ganja_core::provider::select`'s half of the work, and
    /// keeping the split here is what let the wires move to a crate of their
    /// own while selection stayed where the config is.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when `base_url` is somewhere a
    /// credential may not travel — the rule every other provider's endpoint is
    /// held to, applied at construction so a bad entry fails at startup where
    /// the message is readable — or when no HTTP client can be built.
    pub fn new(
        id: impl Into<String>,
        dialect: Dialect,
        base_url: &str,
        credential: CredentialSource,
        headers: reqwest::header::HeaderMap,
    ) -> Result<Self, ProviderError> {
        check_base_url(base_url)?;

        let wire = match dialect {
            Dialect::OpenaiChatCompletions => Wire::ChatCompletions(
                OpenAiProvider::with_credential(credential, base_url)?.with_headers(headers),
            ),
            Dialect::AnthropicMessages => Wire::Messages(
                AnthropicProvider::with_credential(credential, base_url)?.with_headers(headers),
            ),
        };

        Ok(Self {
            id: id.into(),
            wire,
        })
    }
}

#[async_trait]
impl Provider for CompatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        match &self.wire {
            Wire::ChatCompletions(wire) => wire.stream(request, cancel).await,
            Wire::Messages(wire) => wire.stream(request, cancel).await,
        }
    }

    /// Delegated for [`super::grok`]'s reason. A config-declared endpoint that
    /// sends one of the two known families is metered exactly like a builtin
    /// one; one that sends neither meters nothing, which is the same answer
    /// this build gives about everything else it cannot size about such an
    /// endpoint.
    fn rate_windows(&self) -> Vec<super::RateWindow> {
        match &self.wire {
            Wire::ChatCompletions(wire) => wire.rate_windows(),
            Wire::Messages(wire) => wire.rate_windows(),
        }
    }

    /// The plan buckets of whichever wire this endpoint turned out to be
    /// (**D485**) — an endpoint that sends neither family meters neither.
    fn plan_windows(&self) -> Vec<super::PlanWindow> {
        match &self.wire {
            Wire::ChatCompletions(wire) => wire.plan_windows(),
            Wire::Messages(wire) => wire.plan_windows(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CompatProvider, Dialect};
    use crate::{
        catalog,
        provider::{CredentialSource, PROVIDERS, Presented, Provider as _, ProviderError},
    };

    /// A credential that must never be rendered by anything here.
    const CANARY: &str = "sk-test-canary-XYZ";

    fn built(dialect: Dialect, base_url: &str) -> Result<CompatProvider, ProviderError> {
        CompatProvider::new(
            "local-llama",
            dialect,
            base_url,
            CredentialSource::Key(Presented::new(CANARY).expect("a non-blank key")),
            reqwest::header::HeaderMap::new(),
        )
    }

    /// The whole reason this is a newtype rather than a flag: a turn is priced,
    /// gated and disclosed by the name its provider reports.
    #[test]
    fn a_config_named_provider_answers_to_the_name_its_entry_was_written_under() {
        for dialect in [Dialect::OpenaiChatCompletions, Dialect::AnthropicMessages] {
            let provider = built(dialect, "http://127.0.0.1:8080/v1").expect("a client builds");

            assert_eq!(
                provider.id(),
                "local-llama",
                "{dialect:?} reported the wire it borrows"
            );
            assert!(
                !PROVIDERS.contains(&provider.id()),
                "the config tier is what makes this selectable, not the builtin list"
            );
            assert!(
                !catalog::carries(provider.id()),
                "an endpoint a person named is one no published table knows"
            );
        }
    }

    /// The endpoint is not exempt from the rule every other provider's is held
    /// to just because it arrived as configuration — and neither the key nor
    /// the URL may reach a rendering, which is what a `tracing` field holding
    /// a provider becomes.
    #[test]
    fn a_configured_endpoint_may_carry_a_key_only_where_a_builtin_one_could() {
        for dialect in [Dialect::OpenaiChatCompletions, Dialect::AnthropicMessages] {
            let refused = built(dialect, "http://gateway.example/v1")
                .expect_err("plain http to a public host puts the key on the wire in the clear");
            assert!(
                matches!(refused, ProviderError::Transport(_)),
                "{dialect:?}: {refused:?}"
            );

            let provider = built(dialect, "https://ganja:secret@gateway.example/v1")
                .expect("https is where a key may travel");
            let rendered = format!("{provider:?}");
            assert!(
                rendered.contains("local-llama") && rendered.contains("gateway.example"),
                "a provider renders as which one it is and where it points: {rendered}"
            );
            assert!(
                !rendered.contains(CANARY) && !rendered.contains("secret"),
                "the credential — in the header or in the userinfo — reached a \
                 rendering: {rendered}"
            );
        }
    }

    /// The two words a config file may spell, held to the spelling `serde`
    /// derives, because a third value here would be a third request/response
    /// mapping rather than another endpoint.
    #[test]
    fn the_dialects_are_spelled_the_way_a_config_file_spells_them() {
        for (dialect, spelled) in [
            (Dialect::OpenaiChatCompletions, "openai-chat-completions"),
            (Dialect::AnthropicMessages, "anthropic-messages"),
        ] {
            // Through the derive, which is the whole mechanism: a dialect is
            // read from a config file and never written back, so this
            // direction is the only one there is.
            assert_eq!(
                serde_json::from_value::<Dialect>(serde_json::json!(spelled))
                    .expect("the word a config file writes"),
                dialect
            );
        }
        assert!(
            serde_json::from_value::<Dialect>(serde_json::json!("anthropic")).is_err(),
            "a dialect nothing implements is refused rather than guessed at"
        );
    }
}
