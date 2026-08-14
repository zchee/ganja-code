//! OpenCode Zen and OpenCode Go — one vendor's gateway, **three dialects off
//! one base URL**, chosen per model.
//!
//! Spec standing: **absent from the pinned v1.18.13 checkout**, which knows
//! `zenmux` and nothing called Zen or Go. These are the vendor's later
//! subscription products, so the spec here is the cursor-wire method
//! (**D488**): the live model catalog, the vendor's own open-source client at a
//! pinned commit, its published docs, and a set of live probes the user ran.
//! Every load-bearing claim below carries which of those settled it. The
//! research lane's full source ledger is `.omc/handoffs/p19-wb1-report.md`;
//! `[G]` below is `github.com/sst/opencode` at `e23586af`.
//!
//! # Two ids, one credential, one module
//!
//! | | Zen | Go |
//! |---|---|---|
//! | catalog id | [`ZEN_ID`] | [`GO_ID`] |
//! | base URL | [`ZEN_BASE_URL`] | [`GO_BASE_URL`] |
//! | what it is | pay-as-you-go curated gateway | a monthly subscription over a narrower roster |
//! | credential | [`API_KEY_ENV`] | [`API_KEY_ENV`] — **the same variable, and one key really does serve both** |
//!
//! Two provider ids rather than one with a flag, because the catalog files two
//! rosters and the vendor's own config syntax names two (`opencode/<model>` and
//! `opencode-go/<model>`). Ganja must use exactly these ids or the rows do not
//! resolve — the same rule [`super::openrouter`] is held to.
//!
//! That the id is `opencode` inside a program that is itself an opencode port
//! is confusing and deliberate: it is the vendor's published id, and a name of
//! ganja's own would cost the provider its sizing and pricing to save a
//! paragraph of documentation.
//!
//! **One key serves both ids** — probed live, not inferred: the same
//! `OPENCODE_API_KEY` answered `200` on Go's `/messages` and `/chat/completions`
//! as well as Zen's. The credential is nevertheless *stored* per provider id,
//! with no alias between them, because that is what the vendor's own client
//! does and because a shared store is somebody else's territory
//! (`super::openrouter` makes the same argument for the same reason).
//!
//! # The dialect is per (provider, model), and only the catalog knows it
//!
//! This is the one genuinely new shape here. Every other provider in this
//! directory picks its dialect once, at the provider. This one cannot: one Zen
//! key is presented under *different header names* depending on which model is
//! asked for, and the same model id can differ between the two ids —
//! `minimax-m3` is chat-completions on Zen and Anthropic Messages on Go.
//!
//! The hint is the catalog row's own `npm`, falling back to the provider's
//! (`catalog::ModelInfo::npm` resolves that fallback). Retaining it is the one
//! schema change this feature forced.
//!
//! | catalog `npm` | path | auth header | wire | Zen rows | Go rows |
//! |---|---|---|---|---|---|
//! | `@ai-sdk/openai` | `/responses` | `Authorization: Bearer` | [`super::responses`] | 24 | 2 |
//! | `@ai-sdk/anthropic` | `/messages` | `x-api-key` | [`super::anthropic`] | 19 | 8 |
//! | absent → `@ai-sdk/openai-compatible` | `/chat/completions` | `Authorization: Bearer` | [`super::openai`] | 41 | 15 |
//! | `@ai-sdk/google` | `/models/<id>` | `x-goog-api-key` | **none — refused by name** | 7 | 0 |
//!
//! **The auth-header switch is mandatory and this module contains none of it.**
//! Probed: `/messages` answers a bearer with `401 AuthError "Missing API key."`,
//! so presenting the key under the wrong name is a dead turn rather than a
//! style question. It needs no code here because each of ganja's three wires
//! already sends its own vendor's header — `x-api-key` is what
//! [`super::anthropic`] has always sent. **Choosing the wire chooses the
//! header**, which is why `Dialect` carries no header of its own; the test
//! `each_dialect_presents_the_key_under_the_name_that_dialect_requires` pins
//! that this stays true.
//!
//! `anthropic-version` is *optional* here (probed: `200` without it). The
//! Messages wire pins `2023-06-01` and keeps doing so — following what that
//! wire already sends is the whole point of reusing it.
//!
//! # What is refused, and what is merely absent
//!
//! - **`@ai-sdk/google` rows are refused by name.** Not a gap: the vendor's own
//!   current runner refuses them too, from an allowlist of exactly the three
//!   dialects above (\[G\] `packages/core/src/session/runner/model.ts:164-170`,
//!   restated `:176-179`). Ganja has no Google wire, and inventing one from an
//!   undocumented path shape would be the guess this port does not make. Zen
//!   publishes 7 such rows; Go publishes none.
//! - **Any other `npm` is refused by name too**, for the same reason and by the
//!   same allowlist: a transport this build has never met is not a dialect to
//!   guess at.
//! - **No usage, plan or rate headers exist.** Probed across every dialect on
//!   both ids: a `200` carries `date`, `content-type`, `content-length`,
//!   `cf-placement`, `server`, `cf-ray` and nothing else. Zen and Go therefore
//!   join **D471**'s honest-absence tail — a credential that serves no meter —
//!   and **nothing here invents one**.
//!
//!   Limits surface only at exhaustion, as a refusal whose body names
//!   `FreeUsageLimitError` or `GoUsageLimitError` (\[G\]
//!   `packages/opencode/src/session/retry.ts:98-140`), carrying a `retry-after`.
//!   **Both halves are already handled by machinery this module does not
//!   touch**, which is why it adds no code for either: `retry::refusal` puts
//!   the refusal body into the error it returns (`retry::summarize`, tested
//!   there), so the vendor's own error type name is what the user reads; and
//!   `retry::delay` honours `retry-after`/`retry-after-ms` ahead of its own
//!   backoff schedule, which is upstream's behaviour and the only use this
//!   vendor's one limit-bearing header has. Writing either again here would be
//!   a second copy of a working answer.
//!
//!   The success body's top-level `cost` field — a per-request *actual* cost,
//!   which would be a truer source than catalog-price arithmetic — is recorded
//!   as a finding and deliberately not read: pricing is `catalog::cost`'s for
//!   every provider, and one wire reporting its own would be a number nothing
//!   else could reproduce.
//!
//! # Attachments
//!
//! [`Provider::accepts_attachment`] is left at the trait's `false`, and that is
//! a decision rather than an omission. The method sees a mime and not a model,
//! so it must answer once for three dialects that disagree — the chat wire
//! carries no file block at all, while the other two carry five mime types. A
//! `false` produces the engine's *announced* degradation (the attachment
//! becomes a text block naming the file, and the composer warned at submit
//! time); a `true` would hand a file part to a wire with nowhere to put it.
//! Announcing beats dropping. A model-aware signature would let all three
//! answer honestly and is a follow-up, not a thing to fake here.

use async_trait::async_trait;
use futures::stream::BoxStream;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

use crate::{
    catalog,
    provider::{
        AnthropicProvider, ChatRequest, CredentialSource, OpenAiProvider, Presented, Provider,
        ProviderError, ProviderEvent, ResponsesProvider, require_key, responses::Backend,
    },
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects Zen.
///
/// The vendor's published id, and the key its catalog rows are filed under.
pub const ZEN_ID: &str = "opencode";

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects Go.
pub const GO_ID: &str = "opencode-go";

/// Environment variable carrying the credential, for **both** ids — the one the
/// catalog names in each provider's `env`.
pub const API_KEY_ENV: &str = "OPENCODE_API_KEY";

/// Where Zen's gateway lives, which is also the `api` its catalog row publishes.
pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// Where Go's lives: the same host, one path segment further in.
pub const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// The transport the catalog names for a row served over chat completions.
///
/// Also the value **both** ids declare at the provider level, so it is what a
/// row that overrides nothing inherits — and what [`Dialect::of`] falls back to
/// for a model the table has never heard of.
const CHAT: &str = "@ai-sdk/openai-compatible";

/// The transport for a row served over the Responses API.
const RESPONSES: &str = "@ai-sdk/openai";

/// The transport for a row served over Anthropic's Messages API.
const MESSAGES: &str = "@ai-sdk/anthropic";

/// The transport this build has no wire for, named so the refusal can name it.
const GOOGLE: &str = "@ai-sdk/google";

/// The base the Messages wire has to be given to land on this gateway's
/// `/messages`.
///
/// **The two vendors disagree about where `/v1` lives, and the gateway follows
/// only one of them.** Anthropic's own base is `https://api.anthropic.com` and
/// its wire posts to `{base}/v1/messages`; OpenAI's base already carries the
/// segment and its wires post to `{base}/chat/completions` and
/// `{base}/responses`. This gateway serves all three off **one** base that ends
/// in `/v1`, so handing that base to the Messages wire unchanged would ask for
/// `…/zen/v1/v1/messages` — a `404` on every Anthropic-dialect row, which is 19
/// of Zen's 91 and 8 of Go's 25.
///
/// So the segment is removed for that wire alone, which puts it back exactly
/// where that wire will re-add it. Derived rather than written down as a fourth
/// constant, because the two are the same URL and a second literal is the way
/// they drift apart.
///
/// A base that does not end in `/v1` is passed through: that is a test's
/// loopback socket, or a future endpoint that spells its paths differently, and
/// neither is this function's to correct.
fn messages_base(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');

    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_owned()
}

/// Which of the three wires a model is served over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// `<base>/chat/completions`, bearer.
    Chat,
    /// `<base>/responses`, bearer.
    Responses,
    /// `<base>/messages`, `x-api-key`.
    Messages,
}

impl Dialect {
    /// The dialect `npm` names, or why this build will not send the request.
    ///
    /// An allowlist rather than a fallthrough, mirroring the vendor's own
    /// runner: a transport nobody here has met is refused by name, because the
    /// alternative is sending an Anthropic body to a Google endpoint and
    /// reading whatever comes back as a model's answer.
    ///
    /// [`None`] — a model the catalog has no row for — takes [`CHAT`], which is
    /// the value both ids publish at the provider level and therefore what the
    /// vendor's own fallback resolves to. So a model added to the gateway since
    /// this machine last refreshed its catalog still runs, on the dialect most
    /// of the roster uses.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Transport`] — the variant this crate uses for a request
    /// it declines to *make*, following `responses::unsupported`. Returned
    /// before anything is sent, so the retry driver never sees it.
    fn of(npm: Option<&str>, provider_id: &str, model: &str) -> Result<Self, ProviderError> {
        match npm {
            None | Some(CHAT) => Ok(Self::Chat),
            Some(RESPONSES) => Ok(Self::Responses),
            Some(MESSAGES) => Ok(Self::Messages),
            Some(named) => Err(ProviderError::Transport(format!(
                "{provider_id} serves `{model}` over {named}, which this build has no \
                 wire for; it speaks {CHAT}, {RESPONSES} and {MESSAGES}{}",
                if named == GOOGLE {
                    " - and neither does the vendor's own client, which refuses \
                     these rows from the same list"
                } else {
                    ""
                }
            ))),
        }
    }
}

/// One of the vendor's two gateways, dispatching each turn to the wire its
/// model is served over.
///
/// All three wires are built up front rather than per request: each is a
/// client, a base URL and a credential, and building one costs a TLS client
/// setup that a turn should not pay for. Which of them a request uses is the
/// catalog's answer, not this struct's.
#[derive(Debug)]
pub struct OpencodeProvider {
    /// Which of the two gateways this is, in the catalog's vocabulary.
    id: &'static str,
    chat: OpenAiProvider,
    responses: ResponsesProvider,
    messages: AnthropicProvider,
}

impl OpencodeProvider {
    /// Zen, authenticated by the key [`API_KEY_ENV`] or the credential store
    /// carries.
    ///
    /// # Errors
    ///
    /// As [`at`](Self::at).
    pub fn zen() -> Result<Self, ProviderError> {
        Self::from_env(ZEN_ID, ZEN_BASE_URL)
    }

    /// Go, on the same credential.
    ///
    /// # Errors
    ///
    /// As [`at`](Self::at).
    pub fn go() -> Result<Self, ProviderError> {
        Self::from_env(GO_ID, GO_BASE_URL)
    }

    /// The gateway `id` at its own base, reading the key once at startup.
    ///
    /// The lookup is [`super::key_for`]'s, so the precedence is every key
    /// provider's here: an exported key outranks a stored one. The key is read
    /// under `id`, so `ganja auth login --provider opencode-go` is found by a Go
    /// session — while the *variable* is shared, which is what makes one export
    /// serve both.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Auth`] when there is no key to send, and
    /// [`ProviderError::Transport`] when no HTTP client can be built.
    fn from_env(id: &'static str, base_url: &'static str) -> Result<Self, ProviderError> {
        Self::built(id, base_url, require_key(id, API_KEY_ENV)?)
    }

    /// The same gateway against an endpoint of the caller's choosing, which is
    /// how a test drives it against a loopback socket — [`super::grok`]'s and
    /// [`super::responses`]'s `at` exist for the same reason and are public for
    /// the same one.
    ///
    /// It is public here rather than crate-internal because the thing worth
    /// proving about this provider cannot be proved inside the crate: which
    /// header the key travels under depends on the *catalog*, and installing a
    /// catalog is process-wide state that belongs in a test binary of its own
    /// (`tests/opencode_dialects.rs`).
    ///
    /// The one credential is cloned into all three wires deliberately: it is
    /// one key, and which header it travels under is the wire's business.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Auth`] for a blank credential, and
    /// [`ProviderError::Transport`] when no HTTP client can be built or when
    /// `base_url` is somewhere a key may not travel — each wire's own
    /// constructor applies that rule, so all three agree by construction.
    pub fn at(
        id: &'static str,
        base_url: &str,
        key: impl Into<SecretString>,
    ) -> Result<Self, ProviderError> {
        let key = Presented::new(key)
            .ok_or_else(|| ProviderError::Auth(format!("{API_KEY_ENV} is empty")))?;

        Self::built(id, base_url, key)
    }

    /// The one constructor, so no route can skip a wire or an endpoint check.
    ///
    /// # Errors
    ///
    /// As [`at`](Self::at), less the blank-credential case its caller handled.
    fn built(id: &'static str, base_url: &str, key: Presented) -> Result<Self, ProviderError> {
        Ok(Self {
            id,
            chat: OpenAiProvider::with_credential(
                CredentialSource::Key(key.clone()),
                base_url.to_owned(),
            )?,
            responses: ResponsesProvider::built(
                CredentialSource::Key(key.clone()),
                base_url.to_owned(),
                // The gateway backend, so a turn here reports itself as this
                // provider and takes the same refuse-to-guess posture
                // openrouter does: this vendor documents nothing about the
                // sealed-reasoning pairing, so nothing is asked for, nothing is
                // replayed and nothing is recorded.
                Backend::Opencode(id),
            )?,
            messages: AnthropicProvider::with_credential(
                CredentialSource::Key(key),
                messages_base(base_url),
            )?,
        })
    }

    /// Which wire this turn's model is served over.
    ///
    /// # Errors
    ///
    /// As [`Dialect::of`].
    fn dialect(&self, model: &str) -> Result<Dialect, ProviderError> {
        let npm = catalog::model_for(self.id, model).and_then(|info| info.npm.clone());

        Dialect::of(npm.as_deref(), self.id, model)
    }
}

#[async_trait]
impl Provider for OpencodeProvider {
    fn id(&self) -> &str {
        self.id
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let dialect = self.dialect(&request.model)?;

        // The one line that explains a turn on this provider: the wires
        // underneath log their own vendor's name, so without this a Zen turn
        // over Messages reads in a log as an anthropic turn.
        tracing::debug!(
            provider = self.id,
            model = request.model,
            ?dialect,
            "the catalog's transport hint picked this turn's wire"
        );

        match dialect {
            Dialect::Chat => self.chat.stream(request, cancel).await,
            Dialect::Responses => self.responses.stream(request, cancel).await,
            Dialect::Messages => self.messages.stream(request, cancel).await,
        }
    }

    /// Every wire's buckets, because any of the three may have taken the last
    /// turn and only the one that did has anything to report.
    ///
    /// Expected to be empty for this vendor — the probe found no rate family on
    /// any `200` — but delegated rather than stubbed for the reason grok's is
    /// (**D484**): a wrapper answering "nothing" over wires that really did
    /// capture buckets is the invented-absence twin of an invented number.
    fn rate_windows(&self) -> Vec<super::RateWindow> {
        let mut windows = self.chat.rate_windows();
        windows.extend(self.responses.rate_windows());
        windows.extend(self.messages.rate_windows());

        windows
    }

    /// The plan half, delegated the same way and expected to stay empty:
    /// this vendor serves no plan headers at all, so Zen and Go sit in
    /// **D471**'s honest-absence tail rather than being given a meter (**D485**).
    fn plan_windows(&self) -> Vec<super::PlanWindow> {
        let mut windows = self.chat.plan_windows();
        windows.extend(self.responses.plan_windows());
        windows.extend(self.messages.plan_windows());

        windows
    }
}

#[cfg(test)]
mod tests {
    use super::{
        API_KEY_ENV, CHAT, Dialect, GO_BASE_URL, GO_ID, GOOGLE, MESSAGES, OpencodeProvider,
        RESPONSES, ZEN_BASE_URL, ZEN_ID, messages_base,
    };
    use crate::{
        auth,
        provider::{PROVIDERS, Provider as _, ProviderError, responses::Backend},
    };

    /// A key no other value in this module could be mistaken for.
    const KEY: &str = "sk-zen-canary-5150";

    fn gateway(id: &'static str) -> OpencodeProvider {
        OpencodeProvider::at(id, "http://127.0.0.1:8080/zen/v1", KEY)
            .expect("loopback may carry a key")
    }

    #[test]
    fn two_ids_one_variable_and_both_spelled_the_way_the_catalog_files_them() {
        assert_eq!(ZEN_ID, "opencode");
        assert_eq!(GO_ID, "opencode-go");
        assert_eq!(ZEN_BASE_URL, "https://opencode.ai/zen/v1");
        assert_eq!(GO_BASE_URL, "https://opencode.ai/zen/go/v1");

        for id in [ZEN_ID, GO_ID] {
            assert!(
                PROVIDERS.contains(&id),
                "{id} ships, or nothing can select it"
            );
            assert_eq!(
                auth::key_var(id),
                Some(API_KEY_ENV),
                "both ids read the one variable the catalog names for each"
            );
            assert_eq!(
                auth::storage_key(id),
                id,
                "stored under its own name, as the vendor's own client does — an \
                 alias invented here would hide the credential from it"
            );
        }

        assert_eq!(gateway(ZEN_ID).id(), ZEN_ID);
        assert_eq!(gateway(GO_ID).id(), GO_ID, "not the wire it borrows");
    }

    /// The allowlist, and the two refusals that are the point of having one.
    #[test]
    fn a_transport_this_build_has_no_wire_for_is_refused_by_name() {
        assert_eq!(Dialect::of(None, ZEN_ID, "glm-5"), Ok(Dialect::Chat));
        assert_eq!(Dialect::of(Some(CHAT), ZEN_ID, "glm-5"), Ok(Dialect::Chat));
        assert_eq!(
            Dialect::of(Some(RESPONSES), ZEN_ID, "gpt-5.6-luna"),
            Ok(Dialect::Responses)
        );
        assert_eq!(
            Dialect::of(Some(MESSAGES), ZEN_ID, "qwen3.6-plus"),
            Ok(Dialect::Messages)
        );

        let refused = Dialect::of(Some(GOOGLE), ZEN_ID, "gemini-3-pro")
            .expect_err("this build has no Google wire and will not invent one");
        let ProviderError::Transport(message) = refused else {
            panic!("a request declined before it is made is a transport refusal");
        };
        assert!(message.contains("gemini-3-pro"), "{message}");
        assert!(
            message.contains(GOOGLE),
            "the refusal names the transport, or nobody can act on it: {message}"
        );
        assert!(
            message.contains("the vendor's own client"),
            "parity, not a gap — say so: {message}"
        );

        let unknown = Dialect::of(Some("@ai-sdk/something-new"), GO_ID, "future-model")
            .expect_err("an allowlist, not a fallthrough");
        assert!(
            matches!(unknown, ProviderError::Transport(ref m) if m.contains("@ai-sdk/something-new")),
            "{unknown:?}"
        );
    }

    /// Whichever wire a turn lands on, the credential is not something any of
    /// them renders.
    ///
    /// The *header* each dialect presents it under is the probe's hardest-won
    /// fact and cannot be proved here: it depends on the catalog, and
    /// installing one is process-wide state. `tests/opencode_dialects.rs` is
    /// where all three are driven against a real socket and their headers read
    /// off the requests that actually arrived.
    #[test]
    fn no_wire_behind_the_gateway_renders_the_key_it_holds() {
        let provider = gateway(ZEN_ID);

        let rendered = format!(
            "{:?}{:?}{:?}",
            provider.chat, provider.responses, provider.messages
        );
        assert!(!rendered.contains(KEY), "{rendered}");
        assert!(
            rendered.contains("Key"),
            "each wire still says what kind of credential it holds: {rendered}"
        );
    }

    /// The Responses rows here take openrouter's posture, and for openrouter's
    /// reason: this vendor documents nothing about the sealed-reasoning
    /// pairing, so nothing is asked for and nothing is replayed.
    #[test]
    fn the_gateways_responses_rows_guess_nothing_about_sealed_reasoning() {
        for id in [ZEN_ID, GO_ID] {
            assert!(
                !Backend::Opencode(id).replays_reasoning(),
                "{id} would be asking a gateway for a field its vendor never documented"
            );
            assert_eq!(Backend::Opencode(id).provider_id(), id);
        }
    }

    /// The catalog is the only thing that knows a dialect, so a provider that
    /// cannot read one still has to take a turn rather than refuse every model.
    #[test]
    fn a_model_the_table_has_never_heard_of_falls_back_to_the_published_default() {
        // Both ids declare `@ai-sdk/openai-compatible` at the provider level,
        // so this is the vendor's own fallback rather than a house choice.
        assert_eq!(
            Dialect::of(None, ZEN_ID, "a-model-added-since-this-cache"),
            Ok(Dialect::Chat)
        );
        assert_eq!(
            gateway(GO_ID)
                .dialect("a-model-added-since-this-cache")
                .expect("an unknown model still takes a turn"),
            Dialect::Chat
        );
    }

    /// The gateway's three paths hang off one base; two of ganja's wires agree
    /// with that and the third puts `/v1` in itself. Getting this wrong is a
    /// `404` on a third of both rosters and nothing else — no error at
    /// construction, no wrong header, just a path with the segment twice.
    #[test]
    fn the_messages_wire_is_handed_the_base_it_will_re_add_the_version_to() {
        assert_eq!(messages_base(ZEN_BASE_URL), "https://opencode.ai/zen");
        assert_eq!(messages_base(GO_BASE_URL), "https://opencode.ai/zen/go");
        // Which is to say: the URL that wire then builds is the gateway's own.
        for (base, want) in [
            (ZEN_BASE_URL, "https://opencode.ai/zen/v1/messages"),
            (GO_BASE_URL, "https://opencode.ai/zen/go/v1/messages"),
        ] {
            assert_eq!(format!("{}/v1/messages", messages_base(base)), want);
        }

        assert_eq!(
            messages_base("https://opencode.ai/zen/v1/"),
            "https://opencode.ai/zen",
            "a trailing slash is not a different endpoint"
        );
        assert_eq!(
            messages_base("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080",
            "a base that never carried the segment is not this function's to edit"
        );
    }

    /// A key may not be sent anywhere the other wires' keys could not be, and
    /// the refusal has to come from construction rather than from the request.
    #[test]
    fn every_wire_behind_one_gateway_is_held_to_the_same_endpoint_rule() {
        let refused = OpencodeProvider::at(ZEN_ID, "http://opencode.ai/zen/v1", KEY)
            .expect_err("plain http to a public host puts the key on the wire in the clear");

        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
    }
}
