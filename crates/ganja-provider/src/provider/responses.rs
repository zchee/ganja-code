//! OpenAI's Responses API, streamed — the wire this vendor speaks, both ways in.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts:183-186`, whose
//! whole body for an OpenAI model is `evt.language = evt.sdk.responses(...)`.
//! It reads no credential: **the vendor picks the wire, not the token**, which
//! is why an API key session belongs here too and not on chat completions. The
//! same file disables `gpt-5-chat-latest` at `:164-171` with the consequence
//! written on it — that alias is chat-completions-only, so a Responses-only
//! vendor cannot serve it — and `CHAT_COMPLETIONS_ONLY` is that arm ported.
//! What such a request looks like on the wire is
//! `packages/opencode/src/plugin/openai/codex.ts:341-426`, the fetch override
//! that authenticates it and decides where it goes, cross-checked against
//! `@ai-sdk/openai@3.0.84`'s `src/responses/*` for the body and the frames.
//!
//! **This is a second request/response mapping, and that is the point.** The
//! sibling [`super::grok`] is a base URL and a credential source over
//! [`super::openai`] because xAI's endpoint speaks that API; the Responses API
//! does not. It carries *items* rather than chat messages, names every frame,
//! and reports a tool call across three event types instead of one. So the
//! encoder and the mapper are here, and everything else — the client, the
//! endpoint check, the retry driver, the frame splitter — is still `mod.rs`'s.
//!
//! # Two backends, and what differs between them
//!
//! One mapping, one encoder, two places a request can go — `Backend` is the
//! whole of the difference, and it is fixed when the provider is built because
//! it follows the credential the session resolved:
//!
//! | | `Backend::Codex` | `Backend::Platform` |
//! |---|---|---|
//! | credential | a stored ChatGPT login | an API key |
//! | base URL | [`DEFAULT_BASE_URL`] | [`openai::DEFAULT_BASE_URL`] |
//! | extra headers | `ACCOUNT_HEADER`, `ORIGINATOR_HEADER`, `BETA_HEADER` | none |
//! | model gate | `serves` | whatever the platform serves |
//! | default model | [`SUBSCRIPTION_DEFAULT`] | the catalog's |
//!
//! Every one of those rows is upstream's, and all of them come off the same
//! branch: `codex.ts:356` returns the *unwrapped* `fetch` for a credential that
//! is not OAuth, so a key request keeps the URL the SDK built
//! (`api.openai.com/v1/responses`) and gains none of the three headers
//! `:405-408` set; `codex.ts:281` returns the models unfiltered for the same
//! condition, so the allow-list is a property of the seat and not of the API.
//!
//! [`DEFAULT_BASE_URL`] being `https://chatgpt.com/backend-api/codex` rather
//! than the platform is `codex.ts:12`'s `CODEX_API_ENDPOINT`, and
//! `codex.ts:414-418` rewrites every Responses URL to it for an OAuth
//! credential. The reason that is not an implementation detail of upstream's
//! fetch layer: a ChatGPT access token is minted for that backend, and the
//! platform API refuses it.
//!
//! [`BASE_URL_ENV`](super::openai::BASE_URL_ENV) overrides either default,
//! under the same `check_base_url` rule the sibling is held to, which is what
//! points this provider at a loopback socket for a test. **Known cost of the
//! move, accepted deliberately**: that variable now points a *Responses* client
//! at whatever it names, so a chat-completions-only server — a local
//! llama.cpp — stops being reachable as `GANJA_PROVIDER=openai`. The wire is
//! the vendor's, and a compatible endpoint that is not this vendor wants a
//! provider id of its own rather than this one's environment.
//!
//! # What the credential is
//!
//! A key is captured at construction and presents itself unchanged. An access
//! token expires and rotates, so it is resolved **per request** through the
//! seam [`super::grok`] uses — `codex.ts:353` re-reads it on every call for
//! exactly that reason. Nothing is captured for that arm, so a login that
//! happened after this session started, and a renewal another turn performed,
//! are both picked up by the next request rather than the next process.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    protocol::{FinishReason, Part, PartBody, Role, ToolState, Usage},
    provider::{
        ChatRequest, CredentialSource, Mapper, Provider, ProviderError, ProviderEvent, Resolved,
        check_base_url, client, open,
        openai::{self, arguments, result},
        opencode, openrouter, require_key, setting, shown_base_url, splice_effort,
        sse::Frame,
        steps,
        toolname::{Aliases, OPENAI_CAP, alias},
    },
    tool::ToolDefinition,
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
///
/// The same one [`super::openai`] answers to, because it is the same vendor
/// serving the same models at the same prices — and now the same wire as well;
/// what [`super::openai`] still is, is the API that [`super::grok`] and
/// [`super::copilot`] ride.
pub const ID: &str = openai::ID;

/// Where a ChatGPT subscription's requests go (`codex.ts:12`).
///
/// The path this provider appends is `/responses`, so the whole URL is
/// `codex.ts`'s `CODEX_API_ENDPOINT` exactly.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Which backend a provider was built against.
///
/// Not a runtime question: it follows the credential, which is resolved once
/// per session in `ganja_core::provider::openai_provider`. Keeping it a field rather than
/// re-deriving it per request is what makes "a key request is never filtered by
/// the seat's allow-list" a fact about how the provider was constructed instead
/// of a condition somebody could forget to write.
///
/// Two of the three are one vendor's; the third is a different vendor serving
/// the same dialect, which is why this enum answers [`Self::provider_id`] as
/// well. See [`super::openrouter`] for what that one keeps and drops, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Backend {
    /// The backend a ChatGPT subscription is served by, reached with an OAuth
    /// access token (`codex.ts:12`).
    Codex,
    /// OpenAI's own platform API, reached with an API key.
    Platform,
    /// OpenRouter's Responses surface, reached with that vendor's own API key.
    OpenRouter,
    /// One of the OpenCode gateways' Responses rows, under the id the catalog
    /// files them beneath — [`super::opencode::ZEN_ID`] or
    /// [`super::opencode::GO_ID`]. Carries the id because *two* providers share
    /// this arm and a turn must report which one it ran as.
    Opencode(&'static str),
}

impl Backend {
    /// Where this backend lives when [`BASE_URL_ENV`](openai::BASE_URL_ENV)
    /// names nothing.
    ///
    /// Three hosts because the credential decides which one will take it: a
    /// ChatGPT token is refused by the platform, a key is refused by the codex
    /// backend, and neither vendor's credential is the other's.
    const fn default_base_url(self) -> &'static str {
        match self {
            Self::Codex => DEFAULT_BASE_URL,
            Self::Platform => openai::DEFAULT_BASE_URL,
            Self::OpenRouter => openrouter::DEFAULT_BASE_URL,
            // Zen and Go do not share a base, so this arm cannot answer for
            // both — and never has to: only `configured` reads this, and that
            // is the `openai` environment override, which no gateway honours.
            // `opencode::at` passes its base URL explicitly, like every
            // caller that knows its own endpoint.
            Self::Opencode(_) => opencode::ZEN_BASE_URL,
        }
    }

    /// Which provider a turn on this backend reports itself as.
    ///
    /// [`Provider::id`] is what the session layer prices a turn by — it filters
    /// the catalog on it — so this is not cosmetic: an OpenRouter turn reporting
    /// itself as `openai` would be sized and billed against the wrong table, and
    /// its sealed reasoning would be handed to the wrong wire.
    pub(super) const fn provider_id(self) -> &'static str {
        match self {
            Self::Codex | Self::Platform => ID,
            Self::OpenRouter => openrouter::ID,
            Self::Opencode(id) => id,
        }
    }

    /// Whether this backend documents the sealed-reasoning pairing
    /// ([`Body::include`] out, a `reasoning` input item back).
    ///
    /// OpenAI's two do. Neither gateway does, and nothing here guesses on
    /// their behalf — the whole reasoning is in [`super::openrouter`]'s module
    /// doc, and [`super::opencode`] inherits it for the same reason: a vendor
    /// that documents no way to hand sealed state back is not one to hand it
    /// back to. One predicate rather than three sites, because asking for
    /// state and replaying it are one feature and half of it is worse than
    /// neither.
    pub(super) const fn replays_reasoning(self) -> bool {
        matches!(self, Self::Codex | Self::Platform)
    }
}

/// Which of a person's ChatGPT accounts to bill (`codex.ts:406-408`).
const ACCOUNT_HEADER: &str = "chatgpt-account-id";

/// Who the backend is told is asking (`codex.ts:551`).
///
/// **Deliberately still `opencode`**, for the reason
/// [`auth::openai`]'s own originator is: the access token was minted against a
/// client registration that belongs to that project, and a value the
/// registration has never been sent is a rejection nothing here could test for.
const ORIGINATOR_HEADER: &str = "originator";

/// The value [`ORIGINATOR_HEADER`] carries.
const ORIGINATOR: &str = "opencode";

/// Opts the request into the Responses surface the Codex CLI talks to.
///
/// **Not from the pin.** Upstream sends `openai-beta` only as the websocket
/// protocol header (`plugin/openai/ws.ts:80`), a different value on a transport
/// ganja does not have; this is the header the Codex CLI sends on its own HTTP
/// requests to the same endpoint. It is additive — the backend serves the same
/// stream without it — and it is here so that ganja's request differs from that
/// CLI's in as few ways as possible.
const BETA_HEADER: &str = "openai-beta";

/// The value [`BETA_HEADER`] carries.
const BETA: &str = "responses=experimental";

/// The models the **codex backend** serves a ChatGPT seat outright
/// (`codex.ts:15`).
///
/// A positive list, and the reason it is spelled out rather than derived: three
/// of these four are *older* than the floor [`NEWER_THAN`] sets, so the rule
/// below would refuse them.
///
/// **Scope: [`Backend::Codex`] only.** This is a subscription's offering, not
/// the API's — upstream filters the model list on the same `auth.type ===
/// "oauth"` condition the fetch override branches on (`codex.ts:281`), so a
/// session holding a key sees whatever the platform sells. The list is a
/// snapshot of somebody else's product decision as of v1.18.13 and **will
/// drift**; [`NEWER_THAN`] is what keeps it from aging badly, and when the
/// seat's offering changes these four lines are what to re-read against
/// `codex.ts:15-16`.
const ALLOWED_MODELS: [&str; 4] = ["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini"];

/// The models a ChatGPT seat is **offered**, in the order to offer them
/// (**D476**, `seat-roster-pinned`).
///
/// No upstream counterpart: `codex.ts` filters the vendor's catalog through
/// `serves` and offers whatever survives, so the roster a seat browses drifts
/// with `models.dev`. This is the owner's own pin instead — five ids, this
/// order, decided once and answered from the binary.
///
/// **Offered is not servable, and the split is the whole point.** `serves`
/// stays the `codex.ts:281-292` port it always was, so a session that names
/// `--model openai/gpt-5.4` explicitly still takes its turn; what this narrows
/// is only what a listing *volunteers*. `gpt-5.4` and `gpt-5.4-mini` are
/// therefore servable and deliberately unoffered, and
/// [`SUBSCRIPTION_DEFAULT`] — one of them — is deliberately not first here,
/// because what a seat defaults to and what it offers to browse are two
/// decisions.
///
/// **The catalog cannot move this list.** Membership is these five lines;
/// `ganja models --refresh` re-reads sizing and pricing and never this. A
/// catalog row is consulted for one thing only, a human-readable name, and its
/// absence costs nothing — the id stands in.
///
/// Every id here has to satisfy `serves`: an offer this backend would then
/// refuse is a lie the listing tells, and the test below is what keeps it
/// honest.
pub const SEAT_ROSTER: [&str; 5] = [
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.3-codex-spark",
];

/// What a subscription session asks for when nothing named a model.
///
/// **Not [`crate::catalog::default_model`]**, and the reason is the shape of
/// that table: it is one row per *vendor*, and this vendor has two backends
/// that serve different sets. A catalog default is therefore free to name a
/// model the platform sells and the seat does not — which is exactly what
/// `gpt-5.6` is — and handing it to a subscription session produces a seat that
/// cannot take a turn at all. A model named explicitly is never substituted:
/// somebody who asked for `gpt-5.6` on a ChatGPT login is told what the seat
/// serves (`unsupported`) rather than quietly answered by something else.
///
/// The one this names is the model the P8 live pass measured taking a whole
/// tool-calling turn on this backend, and it has to satisfy `serves` — pinned
/// below, because a default this backend refuses is the bug this constant
/// exists to prevent.
pub const SUBSCRIPTION_DEFAULT: &str = "gpt-5.4";

/// Models this vendor publishes that no Responses request can name
/// (`plugin/provider/openai.ts:164-171`).
///
/// Upstream hides `gpt-5-chat-latest` from the OpenAI catalog outright, with
/// the reason in a comment: the plugin sends every OpenAI model through
/// Responses and that alias is chat-completions-only. Ganja refuses it at the
/// wire instead of hiding it, because the two builds hold their catalogs
/// differently — ganja's compiled-in snapshot carries no such row today, but
/// the **fetched** catalog is upstream's own file and can carry rows the
/// snapshot does not, so a filter over the snapshot alone would be a rule that
/// silently stops applying the first time somebody runs `ganja models
/// --refresh`. Refusing where the request is built covers both tables and every
/// spelling that reaches one, and costs nothing when the list is empty of
/// whatever was asked for.
///
/// Unlike [`ALLOWED_MODELS`] this is **not** per-backend: it is a fact about
/// the model rather than about the seat, so it holds for a key as well.
const CHAT_COMPLETIONS_ONLY: [&str; 1] = ["gpt-5-chat-latest"];

/// The models it refuses although [`NEWER_THAN`] would admit them
/// (`codex.ts:16`).
const DISALLOWED_MODELS: [&str; 1] = ["gpt-5.5-pro"];

/// The one model named in its own arm (`codex.ts:289`).
///
/// Newer than everything served and refused anyway, which is why neither list
/// can express it.
const REFUSED_MODEL: &str = "gpt-5.6";

/// The generation an unlisted `gpt-N.M` has to beat (`codex.ts:290-291`).
///
/// Upstream's forward hedge, ported for the same reason it exists: a model this
/// build's catalog gains later should not need a code change to be reachable.
const NEWER_THAN: f64 = 5.4;

/// Whether the ChatGPT backend will serve `model` to a subscription.
///
/// **Asked only of a [`Backend::Codex`] provider.** `codex.ts:281` is the same
/// early return the fetch override takes: a session that is not on an OAuth
/// credential gets `provider.models` unfiltered, so a key is never held to a
/// seat's offering. [`ResponsesProvider::refuses`] is where that scoping is
/// spelled.
///
/// Ports `codex.ts:281-292` in its order, which is load-bearing — the explicit
/// lists have to be read before the generation rule or `gpt-5.4` refuses
/// itself.
///
/// **One arm is not ported.** Upstream refuses anything whose
/// `options.reasoningMode` is `"pro"` before consulting either list; that is a
/// per-model capability flag ganja's catalog does not carry, and inventing one
/// here would be a guess. The only model it is known to cover, `gpt-5.5-pro`,
/// is in [`DISALLOWED_MODELS`] anyway, so the gap is narrower than it reads.
///
/// Visible to the crate so that [`crate::catalog`]'s own tests can hold its
/// `openai` default to this: a default this refuses is a seat that cannot take
/// a turn.
pub(crate) fn serves(model: &str) -> bool {
    if ALLOWED_MODELS.contains(&model) {
        return true;
    }
    if DISALLOWED_MODELS.contains(&model) || model == REFUSED_MODEL {
        return false;
    }

    generation(model).is_some_and(|generation| generation > NEWER_THAN)
}

/// The `\d+\.\d+` a model id opens with after `gpt-`, as the number upstream's
/// `parseFloat` reads.
///
/// Anchored and greedy per half, exactly as the regex is: `gpt-5.4-mini` reads
/// as 5.4, `gpt-5.4.1` as 5.4, and `gpt-5` as nothing at all, because the
/// fractional half is required rather than optional.
fn generation(model: &str) -> Option<f64> {
    /// The leading run of digits, and whatever follows it.
    fn digits(text: &str) -> (&str, &str) {
        let end = text
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(text.len());

        text.split_at(end)
    }

    let (major, rest) = digits(model.strip_prefix("gpt-")?);
    let (minor, _) = digits(rest.strip_prefix('.')?);

    if major.is_empty() || minor.is_empty() {
        return None;
    }

    format!("{major}.{minor}").parse().ok()
}

/// Says what this seat serves, rather than only that it does not serve this.
///
/// [`ProviderError::Transport`] for the reason [`check_base_url`]'s refusal is:
/// the variant this crate uses for a request the provider declines to *make*.
/// It is classified retryable, which is wrong for a model name and harmless
/// here — the refusal is returned before [`open`] is reached, so the retry
/// driver never sees it.
fn unsupported(model: &str) -> ProviderError {
    ProviderError::Transport(format!(
        "a ChatGPT subscription cannot run `{model}`: this backend serves {served}, \
         or a newer gpt model — name one with `--model {ID}/{SUBSCRIPTION_DEFAULT}`, \
         or export {key} to reach the models an API key can",
        served = ALLOWED_MODELS.join(", "),
        key = openai::API_KEY_ENV,
    ))
}

/// Says why a model neither backend can name is not a turn to take.
///
/// Separate from [`unsupported`] because the two are different facts and only
/// one of them has a way out: a seat's offering is escaped by exporting a key,
/// while a chat-completions-only alias is refused on every wire this vendor
/// speaks, so the only useful thing to say is which model to ask for instead.
fn chat_completions_only(model: &str) -> ProviderError {
    ProviderError::Transport(format!(
        "`{model}` is a chat-completions-only model and {ID} speaks the Responses \
         API: there is no wire here that can serve it — name another model with \
         `--model {ID}/{SUBSCRIPTION_DEFAULT}`"
    ))
}

/// Streams replies from OpenAI's Responses API, against whichever of its two
/// backends the session's credential belongs to.
pub struct ResponsesProvider {
    client: reqwest::Client,
    credential: CredentialSource,
    base_url: String,
    /// Which backend this provider was built for — see [`Backend`] for the
    /// table of what it decides.
    backend: Backend,
    /// What this backend last said was left of the account's budget
    /// (**D484**). The platform backend sends the `x-ratelimit-*` family with
    /// Go-spelled resets; the codex backend was observed sending no such
    /// family at all, and meters nothing rather than being given an invented
    /// one — [`super::rate`]'s table picks up either without a change here.
    rates: super::RateWindows,
    /// The gateway's own tools this session opted into, by the name a config
    /// asked for them under (**D489**).
    ///
    /// Empty on every backend but [`Backend::OpenRouter`] and on that one too
    /// unless a config named some: they bill per call, so nothing is asked for
    /// unasked. Held on the provider rather than on the request because it is a
    /// property of the endpoint this session is talking to — the same reason
    /// [`Backend`] is a field here — and because a request type shared by four
    /// wires must not grow a field only one of them can honour.
    server_tools: Vec<String>,
}

impl fmt::Debug for ResponsesProvider {
    /// Renders without the credential, the way every provider here does. The
    /// base URL goes through `shown_base_url` for the same reason the
    /// sibling's does: it is overridable configuration, and configuration is
    /// allowed to carry a secret in its userinfo.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesProvider")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
            .field("backend", &self.backend)
            .finish()
    }
}

impl ResponsesProvider {
    /// The provider against ChatGPT's own backend, or wherever
    /// [`BASE_URL_ENV`](openai::BASE_URL_ENV) points.
    ///
    /// Nothing is read from the credential store here — see the module's note
    /// on why the store is consulted per request instead.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// or when [`BASE_URL_ENV`](openai::BASE_URL_ENV) names an endpoint an
    /// access token may not travel to — so that a misconfigured session dies at
    /// startup rather than at the first prompt.
    pub fn from_stored() -> Result<Self, ProviderError> {
        let login = auth::openai::Login::new().map_err(|error| {
            // `Login::new` fails only where `client()` does, and for the same
            // reason, so it is classified the same way: nothing was refused.
            ProviderError::Transport(error.to_string())
        })?;

        Self::at(configured(Backend::Codex), Arc::new(login))
    }

    /// The provider against OpenAI's platform API, authenticated by the key
    /// [`API_KEY_ENV`](openai::API_KEY_ENV) or the credential store carries.
    ///
    /// The order the two are read in is [`super::key_for`]'s and always has
    /// been: exported outranks stored. What changed with the vendor's move to
    /// this wire is only *which* provider a key builds — the lookup, the
    /// endpoint check and the message a missing key produces are the sibling's,
    /// unchanged, so a session that used to die at startup still does and says
    /// the same thing.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when
    /// [`BASE_URL_ENV`](openai::BASE_URL_ENV) names an endpoint a key may not
    /// travel to, and [`ProviderError::Auth`] when there is no key to send —
    /// in that order, matching [`super::openai::OpenAiProvider::from_env`], so
    /// a session with neither a key nor a login is told about the credential
    /// rather than about a base URL it never set.
    pub fn from_env() -> Result<Self, ProviderError> {
        let base_url = configured(Backend::Platform);
        check_base_url(&base_url)?;

        Self::built(
            CredentialSource::Key(require_key(ID, openai::API_KEY_ENV)?),
            base_url,
            Backend::Platform,
        )
    }

    /// The subscription provider against endpoints of the caller's choosing,
    /// which is how a test drives it against a loopback socket.
    ///
    /// `refresh` is the endpoint half of a renewal — [`auth::openai::Login`]
    /// for a token endpoint that is not ChatGPT's. The rest of a renewal
    /// belongs to [`auth::Refresher`] and is not the caller's to choose.
    ///
    /// # Errors
    ///
    /// As [`from_stored`](Self::from_stored).
    pub fn at(
        base_url: impl Into<String>,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<Self, ProviderError> {
        Self::built(
            CredentialSource::Oauth {
                provider_id: ID,
                refresh,
            },
            base_url.into(),
            Backend::Codex,
        )
    }

    /// The one constructor, so that no arm can forget the endpoint check.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// or when `base_url` names an endpoint a credential may not travel to.
    pub(super) fn built(
        credential: CredentialSource,
        base_url: String,
        backend: Backend,
    ) -> Result<Self, ProviderError> {
        check_base_url(&base_url)?;

        Ok(Self {
            client: client()?,
            credential,
            base_url,
            backend,
            rates: super::RateWindows::default(),
            server_tools: Vec::new(),
        })
    }

    /// The same provider, asking the gateway to serve `tools` on its own side
    /// (**D489**).
    ///
    /// Taken by value and set once, at selection, because it is configuration
    /// rather than per-turn state: which server tools a session opted into
    /// cannot change inside a turn, and a request that could name its own would
    /// be a way for a transcript to start spending money.
    ///
    /// **Ignored on every other backend**, and quietly: the names are one
    /// vendor's namespace and the config key that carries them is named after
    /// that vendor, so the only way to reach this with another backend is a
    /// caller bug — one that must not put an unknown tool type on somebody
    /// else's request.
    #[must_use]
    pub fn serving(mut self, tools: Vec<String>) -> Self {
        if self.backend != Backend::OpenRouter {
            tracing::debug!(
                backend = ?self.backend,
                "server tools are one gateway's own, and this is not it"
            );
            return self;
        }
        self.server_tools = tools;

        self
    }

    /// Why this provider will not put `model` on the wire, where it will not.
    ///
    /// Two refusals, and the scope of each is the point. A chat-completions-only
    /// alias is refused on both backends, because it is a fact about the model:
    /// the vendor speaks Responses, and that alias does not
    /// (`plugin/provider/openai.ts:164-171`). The seat's allow-list is refused
    /// on [`Backend::Codex`] alone, because it is a fact about the
    /// subscription: `codex.ts:281` hands back the unfiltered model list for
    /// any credential that is not an OAuth one, so the platform serves whatever
    /// it sells and a key session is never held to somebody's seat.
    pub(super) fn refuses(&self, model: &str) -> Option<ProviderError> {
        if CHAT_COMPLETIONS_ONLY.contains(&model) {
            return Some(chat_completions_only(model));
        }
        if self.backend == Backend::OpenRouter && openrouter::CHAT_COMPLETIONS_ONLY.contains(&model)
        {
            return Some(chat_completions_only(model));
        }
        if self.backend == Backend::Codex && !serves(model) {
            return Some(unsupported(model));
        }

        None
    }

    /// Builds the request one turn sends, given the credential it resolved.
    ///
    /// Split out from [`Provider::stream`] so that the header set — which is
    /// the whole difference between a request the codex backend serves and one
    /// it refuses, and the whole difference between the two backends' requests
    /// — is provable without a socket.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when the URL or a header value will
    /// not build, with the credential scrubbed out of the message.
    fn request(
        &self,
        resolved: &Resolved,
        request: &ChatRequest,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut built = self
            .client
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(resolved.presented.expose());

        // Subscription-only, all four, and for one reason: each of them is
        // about talking to the codex backend as the Codex CLI, whose client
        // registration the stored access token was minted against
        // (`codex.ts:405-408`, and `auth::openai`'s own originator). A key is
        // the caller's own credential against the platform, which asks for
        // nothing but the bearer — `codex.ts:356` hands such a request to the
        // unwrapped `fetch`, so upstream sends it none of these either. Adding
        // one on a hunch is a header travelling with somebody's API key to an
        // endpoint that never asked for it.
        if self.backend == Backend::Codex {
            built = built
                .header(ORIGINATOR_HEADER, ORIGINATOR)
                .header(BETA_HEADER, BETA)
                .header(
                    reqwest::header::USER_AGENT,
                    auth::device::UPSTREAM_USER_AGENT,
                );

            // Absent where the credential names no account: most people have
            // exactly one, and `auth::openai` treats a token with no such claim
            // as a login that worked rather than as a failure.
            if let Some(account_id) = &resolved.account_id {
                built = built.header(ACCOUNT_HEADER, account_id);
            }
        }

        // The effort's options go under the wire's own fields, so a catalog
        // row can add `reasoning` but can never unmake `model` or `stream`.
        let own = Body::new(request, self.backend).serving(&self.server_tools);
        let options = summarized(&request.effort_options, &request.model, self.backend);
        let body = splice_effort(&options, &own);
        built.json(&body).build().map_err(|error| {
            ProviderError::Transport(
                resolved
                    .presented
                    .redact(&format!("malformed request: {error}")),
            )
        })
    }
}

/// Where a `backend` provider points, honouring the one override.
///
/// [`BASE_URL_ENV`](openai::BASE_URL_ENV) is read for both backends because it
/// is one vendor's variable and this is now one vendor's wire; what it names is
/// then held to [`check_base_url`] like every other endpoint a credential
/// travels to.
fn configured(backend: Backend) -> String {
    setting(openai::BASE_URL_ENV).unwrap_or_else(|| backend.default_base_url().to_owned())
}

#[async_trait]
impl Provider for ResponsesProvider {
    /// The backend's, not the module's: two of the three are this vendor and
    /// one is not — see `Backend::provider_id` for what rides on the answer.
    fn id(&self) -> &str {
        self.backend.provider_id()
    }

    /// The media types the Responses API documents: `input_image` takes
    /// png/jpeg/webp/gif and `input_file` takes PDF. `image/avif` is on the
    /// attachment allowlist and still degrades — a block the vendor does not
    /// document is a guess, and the engine's text fallback is not.
    fn accepts_attachment(&self, mime: &str) -> bool {
        matches!(
            mime,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "application/pdf"
        )
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // Checked here as well as at construction because this is the last
        // moment before the access token goes on the wire.
        check_base_url(&self.base_url)?;

        // Before the credential is even read, let alone spent. The codex
        // backend answers a model outside its list `400 {"detail":"The
        // 'gpt-5.6' model is not supported when using Codex with a ChatGPT
        // account."}` — a whole turn's latency to be told something that was
        // knowable here.
        if let Some(refused) = self.refuses(&request.model) {
            return Err(refused);
        }

        // Resolved before the request is built, never captured at construction:
        // the token expires under a long session, and one renewed a moment ago
        // by another turn is the one this request should carry.
        let resolved = self.credential.resolved().await?;
        let built = self.request(&resolved, &request)?;
        let backend = self.backend;
        // Built from the same roster the body just advertised, so the decoder
        // reads back exactly what this request offered. Cloned per attempt
        // because `open` may call the factory again on a retry.
        let aliases = Aliases::of(&request.tools, OPENAI_CAP);

        // The backend is here and nowhere else: it is what decides the URL, the
        // headers and whether the seat's allowlist applies at all, so a turn
        // read back from a log file without it is a turn whose refusals cannot
        // be explained.
        tracing::debug!(
            provider = ID,
            model = request.model,
            ?backend,
            endpoint = super::endpoint(built.url(), &self.base_url),
            "requesting a turn"
        );

        open(
            move || Mapping::for_backend(backend, aliases.clone()),
            &self.client,
            built,
            &self.base_url,
            &resolved.presented,
            &self.rates,
            cancel,
        )
        .await
        .map_err(|error| reauth(backend, error))
    }

    fn rate_windows(&self) -> Vec<super::RateWindow> {
        self.rates.latest()
    }

    /// The plan half of the same store (**D485**).
    fn plan_windows(&self) -> Vec<super::PlanWindow> {
        self.rates.latest_plans()
    }
}

/// Says what a refused credential needs, rather than only what happened.
///
/// A `401` or `403` on the subscription backend is it rejecting the stored
/// access token, and the only thing that fixes it is a new login. The status
/// alone reaches a status bar as a number, so the command goes in the message
/// beside it. The classification changes nothing the retry driver does: neither
/// status is in [`RETRYABLE_STATUS`](super::retry::RETRYABLE_STATUS), and
/// [`ProviderError::Auth`] is not retryable either.
///
/// [`Backend::Platform`] is deliberately left alone: the same status there is
/// the platform refusing an API key, which `ganja auth login` does not mint —
/// telling somebody to run it would send them through a browser flow that
/// stores a credential their session will not even reach while the key is
/// exported. The endpoint's own message is the honest one.
fn reauth(backend: Backend, error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Status {
            status: status @ (401 | 403),
            message,
        } if backend == Backend::Codex => ProviderError::Auth(format!(
            "the ChatGPT endpoint refused the stored credential (HTTP {status}): \
             {message}; run `ganja auth login {ID}`"
        )),
        other => other,
    }
}

/// The JSON a request carries.
///
/// # `include` and `store` are one feature, and both halves are here now
///
/// `include: ["reasoning.encrypted_content"]` is [`store`](Body::store)'s
/// companion: with `store: false` the backend keeps no trace of a turn, so
/// `include` is the only way a reasoning model's own thinking survives to the
/// next request — the backend seals it, the *client* keeps it, and the client
/// hands it back as a `reasoning` input item
/// (`packages/llm/test/tool-runtime.test.ts:592,601`).
///
/// This build sent `store` without `include` for exactly as long as it had
/// nowhere to put what came back. It has one now
/// ([`PartBody::Reasoning`]), so the pairing the pin describes is whole:
/// [`Body::new`] replays the sealed state and drops any reasoning item that
/// has none, which is upstream's own rule under `store: false`
/// (`packages/llm/src/protocols/openai-responses.ts:446-451`).
#[derive(Debug, Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    /// Whether the backend keeps this turn on its own side.
    ///
    /// **Required to be `false`.** A body without it is answered
    /// `400 {"detail":"Store must be set to false"}` by the ChatGPT backend, so
    /// the field is the difference between a subscription that answers and one
    /// that cannot start a turn at all.
    ///
    /// Not configurable, because nothing here could use `true`: a stored turn
    /// is one the backend can be asked to continue by id, and every ganja
    /// request rebuilds the whole conversation from ganja's own transcript.
    /// Upstream carries it as a route-level default rather than as a
    /// subscription special case (`openai-responses.ts:991`), which is the same
    /// statement about the same wire.
    store: bool,
    /// What the backend should hand back beside the reply.
    ///
    /// One entry or nothing: the only thing this build asks for is the sealed
    /// reasoning it can replay, and a body with no `include` at all is the
    /// shape upstream sends where nobody opted in
    /// (`test/provider/openai-responses.test.ts:638`).
    ///
    /// **Decided by the model, not by the credential**, which is where
    /// upstream decides it: the option rides the model facade
    /// (`packages/llm/src/providers/openai-options.ts:44-58`, applied at
    /// `providers/openai.ts:43`), so both backends this provider reaches send
    /// it for a model that reasons and neither sends it for one that does not.
    /// See [`seals_reasoning`].
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<[&'static str; 1]>,
    /// The system prompt.
    ///
    /// **A divergence, deliberately.** `@ai-sdk/openai` pushes it as an input
    /// item whose role is `system` for an ordinary model and `developer` for a
    /// reasoning one (`openai-responses-language-model.ts`, `systemMessageMode`).
    /// Which of the two a model wants is a per-model capability flag ganja's
    /// catalog does not carry, and guessing it wrong puts the whole system
    /// prompt in the wrong register. `instructions` is the Responses API's own
    /// field for the same text, means the same thing to either kind of model,
    /// and is what the Codex CLI sends — so it is one shape rather than a
    /// coin-flip between two.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    /// The conversation, as items rather than as messages.
    input: Vec<Item<'a>>,
    /// Omitted rather than sent empty, the way the sibling omits it: a turn
    /// with nothing to offer is the ordinary case for a session with no
    /// registry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec<'a>>,
    /// Whether the model may decide to call one of them.
    ///
    /// **[`Backend::OpenRouter`] only, and only beside a non-empty roster.**
    /// `"auto"` is the value the agent loop always wants and the one that
    /// vendor's reference spells in every tool example it publishes
    /// (`api_reference/responses/tool-calling`); what its API defaults to when
    /// the field is absent, that reference does not say, and a gateway
    /// defaulting to `none` would be a session whose tools are advertised and
    /// never called. The other two backends are unchanged because their request
    /// is the Codex CLI's, which sends no `tool_choice` and has been answered by
    /// that endpoint on every turn this build has taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

/// One entry of a request's `tools` array.
///
/// Untagged, because the two shapes are told apart by what they carry: a tool
/// this side will execute names itself, and a tool the *provider* will execute
/// is a type and nothing else.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ToolSpec<'a> {
    /// A tool the model may call and **this build** runs.
    ///
    /// Flatter than chat completions', which nests the same four fields under
    /// `function` (`openai-responses-prepare-tools.ts`, `prepareFunctionTool`).
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        /// The name the model is told, which is the registry's own unless that
        /// one is outside this API's `^[a-zA-Z0-9_-]{1,64}$` — see [`alias`].
        name: Cow<'a, str>,
        description: &'a str,
        /// The argument schema, which this API names `parameters` as well.
        parameters: &'a Value,
    },
    /// A tool the model may call and **the gateway** runs (**D489**).
    ///
    /// One field, which is the whole shape the reference publishes:
    /// `{"type": "openrouter:web_search"}`. No name, because the type *is* the
    /// name; no schema, because the vendor owns the tool; and no `parameters`,
    /// because every knob the reference documents there — search engines,
    /// result caps, shell environments — is a per-tool object this build has no
    /// config surface for and would be inventing defaults for. What a config
    /// asks for today is the tool, at the vendor's own defaults.
    Server {
        #[serde(rename = "type")]
        kind: String,
    },
}

/// One entry in a request's `input`.
///
/// Untagged because the three shapes are told apart by the fields they carry —
/// a `role` for something that was said, a `type` for a call or its result —
/// which is how `convert-to-openai-responses-input.ts` builds them.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Item<'a> {
    /// Something a person or the model said.
    Said {
        role: &'static str,
        content: Vec<Block<'a>>,
    },
    /// A call the model made (`convert-to-openai-responses-input.ts:3338-3344`).
    ///
    /// Its own item rather than a field on the message that made it, which is
    /// the shape difference this whole encoder exists for.
    Called {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        /// Under the same [`alias`] the model was originally offered the tool
        /// as — aliasing is deterministic, so replaying a transcript needs
        /// nothing remembered from the turn that made the call.
        name: Cow<'a, str>,
        /// The arguments as a JSON *string*, which is how this API carries them
        /// too — the model streams them as text.
        arguments: String,
    },
    /// What that call produced (`convert-to-openai-responses-input.ts:3740-3743`).
    Answered {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        output: &'a str,
    },
    /// Thinking the backend sealed on an earlier request, handed back.
    ///
    /// The shape is upstream's replay item verbatim
    /// (`openai-responses.ts:400-406`, asserted at
    /// `tool-runtime.test.ts:601`), and what it does *not* carry is as
    /// deliberate as what it does: no `id`, because under `store: false` there
    /// is no server-side item for one to name, and `summary: []` because the
    /// summary an id would group belongs to a reasoning *text* part this build
    /// does not have.
    Reasoned {
        #[serde(rename = "type")]
        kind: &'static str,
        summary: [&'static str; 0],
        encrypted_content: &'a str,
    },
}

/// One piece of a said item's content.
///
/// Untagged because the `type` value is data here, not a serde tag: text's
/// kind differs by who said it — what reaches the model is `input_text`, what
/// the model said is `output_text` — while the attachment kinds are fixed.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Block<'a> {
    /// Words, from either side of the conversation.
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: Cow<'a, str>,
    },
    /// An image the user attached, as the data URL this API takes base64 in.
    Image {
        #[serde(rename = "type")]
        kind: &'static str,
        image_url: String,
    },
    /// A PDF the user attached. `filename` is the mentioned path, which is
    /// this build's most honest answer to a field the API wants for display.
    File {
        #[serde(rename = "type")]
        kind: &'static str,
        filename: &'a str,
        file_data: String,
    },
}

impl<'a> Body<'a> {
    /// Turns a request into the JSON the Responses API expects.
    ///
    /// # How a transcript becomes a request
    ///
    /// The same split into [`steps`] the sibling makes, and for the same
    /// reason — a call's result belongs after the step that made the call, not
    /// bundled with everything the turn ever said — but flattened differently:
    /// this API has no message that holds calls, so a step becomes up to three
    /// runs of items in order.
    ///
    /// - its text becomes one said item, `input_text` for a user and
    ///   `output_text` for the model;
    /// - the thinking it sealed becomes a `reasoning` item, before the calls
    ///   that thinking produced — the order the pin's second request shows
    ///   (`tool-runtime.test.ts:599-604`);
    /// - each call becomes a `function_call` item;
    /// - each of those calls' results becomes a `function_call_output` item,
    ///   after all of them, because the API pairs them by `call_id` rather than
    ///   by position.
    ///
    /// A step carrying neither text, attachments, nor calls contributes
    /// nothing, which is what the marker opening a turn and a turn that died
    /// before its first fragment both are.
    ///
    /// # Which reasoning is replayed
    ///
    /// Two rules, both upstream's, and both about not sending something the
    /// backend will refuse:
    ///
    /// - **State or nothing.** A reasoning part with no sealed state is
    ///   dropped, because with `store: false` the backend accepts a previous
    ///   reasoning item only when it carries one (`openai-responses.ts:451`).
    ///   Such a part is either a step whose thinking was never sealed or one
    ///   whose record a reader could not decode; either way there is nothing to
    ///   replay, and inventing something is the one thing that must not happen.
    /// - **One item per item id.** Upstream folds a message's reasoning parts
    ///   into one replay entry per id (`openai-responses.ts:394-406`); the same
    ///   id twice in one request is one item said twice.
    ///
    /// A third rule is this build's own, and it is what the part's `provider`
    /// field exists for: sealed state is handed back only to the wire that
    /// sealed it. A session that changes vendors mid-conversation carries
    /// another provider's blobs in its transcript, and they mean nothing here.
    /// The wire is `backend`'s to name rather than this module's, because one
    /// of the three backends is a different vendor entirely.
    ///
    /// And a fourth: a backend that does not document the replay does not get
    /// one ([`Backend::replays_reasoning`]). Nothing is asked for and nothing is
    /// handed back there, which is [`super::openrouter`]'s reasoning and is
    /// stated once in that module rather than twice here.
    fn new(request: &'a ChatRequest, backend: Backend) -> Self {
        let mut input: Vec<Item<'a>> = Vec::new();

        for message in &request.messages {
            let (role, block) = match message.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            // Scoped to the message, as upstream's `reasoningItems` map is.
            let mut replayed: HashSet<&str> = HashSet::new();

            for step in steps(&message.parts) {
                let (texts, attachments, calls, thoughts) = split(step);

                let mut content: Vec<Block<'a>> = Vec::new();
                if let Some(text) = texts {
                    content.push(Block::Text { kind: block, text });
                }
                for file in attachments {
                    // Both shapes carry base64 as a data URL; which item kind
                    // it rides is the mime's to decide, and only mimes
                    // `accepts_attachment` said yes to reach this point.
                    content.push(if file.mime == "application/pdf" {
                        Block::File {
                            kind: "input_file",
                            filename: file.path,
                            file_data: format!("data:{};base64,{}", file.mime, file.content),
                        }
                    } else {
                        Block::Image {
                            kind: "input_image",
                            image_url: format!("data:{};base64,{}", file.mime, file.content),
                        }
                    });
                }
                if !content.is_empty() {
                    input.push(Item::Said { role, content });
                }
                for thought in &thoughts {
                    if !backend.replays_reasoning() {
                        continue;
                    }
                    if thought.provider != backend.provider_id() {
                        tracing::debug!(
                            provider = thought.provider,
                            "reasoning sealed by another provider is not this wire's to \
                             hand back"
                        );
                        continue;
                    }
                    let Some(encrypted) = thought.encrypted else {
                        tracing::debug!(
                            item = thought.item,
                            "a reasoning part carries no state; this step's thinking \
                             cannot be replayed"
                        );
                        continue;
                    };
                    if !replayed.insert(thought.item) {
                        continue;
                    }

                    input.push(Item::Reasoned {
                        kind: REASONING,
                        summary: [],
                        encrypted_content: encrypted,
                    });
                }
                for part in &calls {
                    input.push(Item::Called {
                        kind: "function_call",
                        call_id: part.call_id,
                        name: alias(part.tool, OPENAI_CAP),
                        arguments: arguments(part.state),
                    });
                }
                for part in &calls {
                    input.push(Item::Answered {
                        kind: "function_call_output",
                        call_id: part.call_id,
                        output: result(part.state),
                    });
                }
            }
        }

        let tools: Vec<ToolSpec<'a>> = request
            .tools
            .iter()
            .map(|tool: &ToolDefinition| ToolSpec::Function {
                kind: "function",
                name: alias(&tool.name, OPENAI_CAP),
                description: &tool.description,
                parameters: &tool.schema,
            })
            .collect();

        Self {
            model: &request.model,
            stream: true,
            store: false,
            include: (backend.replays_reasoning() && seals_reasoning(&request.model))
                .then_some([REASONING_INCLUDE]),
            instructions: request.system.as_deref(),
            input,
            tool_choice: (backend == Backend::OpenRouter && !tools.is_empty())
                .then_some(TOOL_CHOICE_AUTO),
            tools,
        }
    }
}

impl<'a> Body<'a> {
    /// Adds the gateway's own tools to the roster, after the ones this build
    /// runs (**D489**).
    ///
    /// Order is deliberate and matches the reference's combined example, where
    /// the server tools lead and the function tools follow — reversed here for
    /// one reason: the aliased function names are what this build is
    /// responsible for, and keeping their positions independent of a config key
    /// keeps a request diffable against the same session without one.
    ///
    /// Called only where a provider knows its own configuration, so a body
    /// built by a fixture carries none of these and every existing request is
    /// byte-identical.
    fn serving(mut self, names: &[String]) -> Self {
        self.tools.extend(names.iter().map(|name| ToolSpec::Server {
            kind: format!("{}{name}", openrouter::SERVER_TOOL_PREFIX),
        }));

        self
    }
}

/// One call a step made, borrowed from the part that recorded it.
struct Made<'a> {
    call_id: &'a str,
    tool: &'a str,
    state: &'a ToolState,
}

/// One step's sealed thinking, borrowed from the part that recorded it.
struct Thought<'a> {
    provider: &'a str,
    item: &'a str,
    encrypted: Option<&'a str>,
}

/// One binary attachment the engine filled at send time, borrowed from the
/// request's own copy of the file part that carries it.
struct Attached<'a> {
    path: &'a str,
    mime: &'a str,
    content: &'a str,
}

/// Splits one step into the text it said, the attachments it carried, the
/// calls it made, and the thinking it sealed.
fn split(
    parts: &[Part],
) -> (
    Option<Cow<'_, str>>,
    Vec<Attached<'_>>,
    Vec<Made<'_>>,
    Vec<Thought<'_>>,
) {
    let mut texts: Vec<&str> = Vec::new();
    let mut attachments = Vec::new();
    let mut calls = Vec::new();
    let mut thoughts = Vec::new();

    for part in parts {
        match &part.body {
            PartBody::Text { text } => {
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            PartBody::Tool {
                call_id,
                tool,
                state,
            } => calls.push(Made {
                call_id,
                tool,
                state,
            }),
            PartBody::Reasoning {
                provider,
                item,
                encrypted,
            } => thoughts.push(Thought {
                provider,
                item,
                encrypted: encrypted.as_deref(),
            }),
            // A binary attachment the engine read at send time, and only for a
            // mime `accepts_attachment` said yes to — the match is by payload
            // shape rather than by allowlist.
            PartBody::File {
                path,
                mime,
                content: Some(content),
                ..
            } => attachments.push(Attached {
                path,
                mime,
                content,
            }),
            // A mentioned *text* file is a reference, resolved into a text
            // block before a request is built (`session::resolve_mentions`);
            // see the same arm in `openai.rs`. `StepFinish` carries a step's
            // bill rather than content, and `StepStart` was consumed as the
            // boundary this step was cut at. `ReasoningText` is thinking this
            // build renders rather than replays — what this API asked to have
            // handed back is the sealed item, which the arm above sends. A
            // `Peer` part is rendered into the user turn at request assembly
            // (D495) and never encoded here as a message of its own.
            PartBody::File { content: None, .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
            | PartBody::Patch { .. } => {}
        }
    }

    let text = match texts.as_slice() {
        [] => None,
        [only] => Some(Cow::Borrowed(*only)),
        // One said item, one text block. Joining fragments without a separator
        // would run the last word of one into the first of the next.
        many => Some(Cow::Owned(many.join("\n"))),
    };

    (text, attachments, calls, thoughts)
}

/// Accumulates what the frames so far said.
///
/// [`Default`] is the recording mapper, because recording is what every backend
/// but one does and a default that silently dropped state would be the wrong
/// way round: a mapper built without thinking about it keeps what it was sent.
#[derive(Debug)]
struct Mapping {
    /// Whether a sealed reasoning item is worth recording.
    ///
    /// The other half of [`Backend::replays_reasoning`], and it has to be the
    /// same answer: a part recording state this build will never hand back is
    /// the row-that-can-never-do-anything [`sealed`]'s own doc refuses to mint,
    /// only with the emptiness one layer further out.
    seals: bool,
    usage: Usage,
    /// Call identifiers by the *item* id their argument deltas name.
    ///
    /// The two are different strings here, which is the trap this API sets:
    /// `response.output_item.added` carries both an item `id` and a `call_id`,
    /// the arguments arrive keyed by the item id
    /// (`response.function_call_arguments.delta`'s `item_id`), and the id a
    /// result has to quote back is the `call_id`. Keying tool events by the
    /// wrong one produces a transcript whose calls nothing answers.
    calls: HashMap<String, String>,
    /// What this request's advertised names map back to, empty for the
    /// ordinary roster whose names this API already accepts.
    aliases: Aliases,
    /// Which spelling of the reasoning-delta event first delivered readable
    /// thinking on this stream, and [`None`] until one has.
    ///
    /// Two vendors name the same event differently — [`REASONING_SUMMARY_DELTA`]
    /// is OpenAI's and [`REASONING_DELTA`] is OpenRouter's — and a gateway
    /// relaying one vendor's stream through its own normalization can carry
    /// both. **First spelling wins for the whole response**, which is stricter
    /// than keying the latch on the item id and deliberately so: what the pane
    /// must never do is render one train of thought twice, and an item id is a
    /// correlation the two spellings are not guaranteed to agree on. The cost
    /// is a summary dropped on a stream that also streamed raw thinking, which
    /// is the richer of the two.
    thinking: Option<&'static str>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            seals: true,
            usage: Usage::default(),
            calls: HashMap::new(),
            aliases: Aliases::default(),
            thinking: None,
        }
    }
}

impl Mapping {
    /// The mapper a `backend`'s stream is read by, reading back the names
    /// `aliases` was built from.
    ///
    /// The `seals` field is the reading half of the same decision the encoder
    /// makes: ask for state and keep it, or do neither.
    fn for_backend(backend: Backend, aliases: Aliases) -> Self {
        Self {
            seals: backend.replays_reasoning(),
            aliases,
            ..Self::default()
        }
    }
}

impl Mapper for Mapping {
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
        // Some deployments close the stream with the chat-completions sentinel
        // as well as with `response.completed`. It is not JSON, so reading it
        // as a chunk would report a parse failure on a stream that ended
        // correctly.
        if frame.data.trim() == DONE {
            return;
        }

        let chunk: Value = match serde_json::from_str(&frame.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                // Skipping would drop reply text without anything downstream
                // knowing a gap exists, which is worse than ending the turn
                // with a message that says so.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "responses chunk: {error}"
                ))));
                return;
            }
        };

        // Every frame names itself twice — an `event:` line and a `type` field
        // — and this reads the field, the way `@ai-sdk/openai` does. A frame
        // arriving with only the `event:` line is a shape this build has never
        // measured; a frame arriving with only the field is what a proxy that
        // re-serializes the stream produces.
        match chunk["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => self.delta(&chunk, events, ProviderEvent::TextDelta),
            REASONING_SUMMARY_DELTA => self.thought(&chunk, events, REASONING_SUMMARY_DELTA),
            REASONING_SUMMARY_PART => self.thought_break(events),
            // OpenRouter's own spelling of the same thing, which is what makes
            // it worth a second arm: that vendor serves a dialect it documents
            // as a drop-in for this one and then names this one event itself
            // (`api_reference/responses/reasoning`, the streaming example, read
            // 2026-08-14). Unmapped, a gateway turn's thinking reached the
            // debug log and the pane stayed empty.
            REASONING_DELTA => self.thought(&chunk, events, REASONING_DELTA),
            REASONING_TEXT_DELTA => self.thought(&chunk, events, REASONING_TEXT_DELTA),
            REASONING_TEXT_DONE => self.thought_break(events),
            // Structure and lifecycle announcements whose content arrives on
            // the arms above, named so the debug log stops calling them
            // unmapped (4855 lines on 2026-08-25): the stream's opening
            // pair, a content part's own open and close, the whole-text and
            // whole-arguments echoes of what already streamed, the summary
            // blocks' own closes — the `.added` boundary is the one that
            // breaks — and the gateway's keepalive.
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.done"
            | "keepalive" => {}
            "response.output_item.added" => self.opened(&chunk["item"], events),
            "response.function_call_arguments.delta" => self.filled(&chunk, events),
            "response.output_item.done" => self.closed(&chunk["item"], events),
            "response.completed" | "response.incomplete" => {
                self.absorb(&chunk["response"]["usage"]);
                if let Some(reason) = chunk["response"]["incomplete_details"]["reason"].as_str() {
                    // No `FinishReason` says "stopped early but said something":
                    // the reply that arrived is whole as far as the loop is
                    // concerned, and the sibling logs its `finish_reason` the
                    // same way rather than inventing a verdict.
                    tracing::debug!(reason, "the model stopped before it was done");
                }

                events.push(ProviderEvent::Usage(self.usage));
                events.push(ProviderEvent::Finish(FinishReason::Completed));
            }
            "response.failed" => {
                events.push(ProviderEvent::Failed(failure(&chunk["response"]["error"])));
            }
            // A chunk-level error, which is how this API reports a failure that
            // happened after the status was already 200.
            "error" => {
                // Field *names* only, both levels: an error frame's keys are
                // schema, never content, so this can say what shape the frame
                // had without quoting a word of what it said — the words reach
                // the log redacted, through `provider::shielded`. This is the
                // ground truth to read the next mid-stream 500 against.
                let fields: Vec<&str> = chunk
                    .as_object()
                    .map(|object| object.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                let nested: Vec<&str> = chunk["error"]
                    .as_object()
                    .map(|object| object.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                tracing::debug!(?fields, ?nested, "an error frame arrived");
                events.push(ProviderEvent::Failed(failure(&chunk)));
            }
            other => tracing::debug!(event = other, "an unmapped responses event"),
        }
    }
}

impl Mapping {
    /// Maps a `delta` field onto `make`, dropping an empty one.
    fn delta(
        &mut self,
        chunk: &Value,
        events: &mut Vec<ProviderEvent>,
        make: fn(String) -> ProviderEvent,
    ) {
        if let Some(delta) = chunk["delta"].as_str()
            && !delta.is_empty()
        {
            events.push(make(delta.to_owned()));
        }
    }

    /// Maps one fragment of readable thinking, whichever of the two spellings
    /// carried it.
    ///
    /// The latch is applied to a fragment that has something to say, never to
    /// an empty one: a stream that opened with an empty delta under the spelling
    /// it then abandoned would otherwise lock the other one out for good.
    fn thought(&mut self, chunk: &Value, events: &mut Vec<ProviderEvent>, spelling: &'static str) {
        let Some(delta) = chunk["delta"].as_str().filter(|delta| !delta.is_empty()) else {
            return;
        };
        if let Some(first) = self.thinking
            && first != spelling
        {
            tracing::debug!(
                first,
                dropped = spelling,
                "a second reasoning-delta spelling on one stream is one train of \
                 thought relayed twice"
            );
            return;
        }

        self.thinking = Some(spelling);
        events.push(ProviderEvent::ReasoningDelta(delta.to_owned()));
    }

    /// Marks the boundary the provider announced between two summary blocks.
    ///
    /// Emitted only once readable thinking has streamed: the same frame also
    /// precedes the *first* block, where there is nothing yet to break from,
    /// and a stream whose readable channel is latched shut has no thought to
    /// end.
    fn thought_break(&self, events: &mut Vec<ProviderEvent>) {
        if self.thinking.is_some() {
            events.push(ProviderEvent::ReasoningBreak);
        }
    }

    /// The thinking a settled reasoning item carries, for a stream that streamed
    /// none.
    ///
    /// OpenRouter's reference shows the summary arriving on the response's own
    /// `reasoning` output item as an array of strings, and documents no
    /// parameter to ask for it — so on that vendor it can arrive on a turn that
    /// streamed nothing readable at all, and the closing frame is the only place
    /// it exists. Guarded on [`Mapping::thinking`] rather than per item, because
    /// what must not happen is the same thinking rendered twice; a stream that
    /// streamed anything readable is already served.
    ///
    /// The latch is deliberately *not* set here. Each reasoning item closes with
    /// its own summary, and a stream where the first item was summarized and the
    /// second streams its thinking must keep both.
    fn settled(&self, item: &Value, events: &mut Vec<ProviderEvent>) {
        if self.thinking.is_some() {
            return;
        }
        let Some(summary) = item["summary"].as_array() else {
            return;
        };

        let blocks: Vec<&str> = summary
            .iter()
            // Two documented shapes for one field: OpenRouter publishes bare
            // strings (`api_reference/responses/reasoning`, "Response with
            // Reasoning") and OpenAI publishes `{type, text}` blocks. Reading
            // both is two references rather than a guess, and an entry in
            // neither shape is skipped rather than rendered as JSON.
            .filter_map(|entry| entry.as_str().or_else(|| entry["text"].as_str()))
            .filter(|line| !line.is_empty())
            .collect();

        // One delta per block with the boundary said between them, exactly
        // as the streaming path says it (2026-08-25): each summary block is
        // a thought of its own, and a joined string would splice them back
        // together.
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                events.push(ProviderEvent::ReasoningBreak);
            }
            events.push(ProviderEvent::ReasoningDelta((*block).to_owned()));
        }
    }

    /// Opens a call the model started making.
    ///
    /// Items of every other kind — a message, a server-side tool this build
    /// never offered, and a block of reasoning, which is still empty when it
    /// opens — are the stream announcing structure rather than content, and
    /// produce nothing here. What a reasoning item is worth arrives when it
    /// closes; see [`sealed`].
    fn opened(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        if item["type"].as_str() != Some(FUNCTION_CALL) {
            return;
        }

        let (Some(item_id), Some(call_id)) = (item["id"].as_str(), item["call_id"].as_str()) else {
            tracing::debug!("a function call arrived without the ids that correlate it");
            return;
        };

        self.calls.insert(item_id.to_owned(), call_id.to_owned());
        events.push(ProviderEvent::ToolCallStart {
            id: call_id.to_owned(),
            // Back through this request's own map: what the engine executes,
            // what the permission rules match and what the transcript records
            // is the registry name, never the one the wire had to advertise.
            name: self
                .aliases
                .original(item["name"].as_str().unwrap_or_default().to_owned()),
        });
    }

    /// Appends a fragment of a call's arguments.
    fn filled(&mut self, chunk: &Value, events: &mut Vec<ProviderEvent>) {
        let Some(id) = chunk["item_id"]
            .as_str()
            .and_then(|item_id| self.calls.get(item_id))
        else {
            tracing::debug!("arguments arrived for a call that was never opened");
            return;
        };

        if let Some(json) = chunk["delta"].as_str()
            && !json.is_empty()
        {
            events.push(ProviderEvent::ToolCallDelta {
                id: id.clone(),
                json: json.to_owned(),
            });
        }
    }

    /// Closes an item whose content is complete: a call to execute, or the
    /// thinking the backend has now sealed.
    ///
    /// A call is executed when it closes, so a stream that died mid-call must
    /// never reach here — which it cannot, because this event is the API's own
    /// terminator for one and an incomplete frame is not a frame.
    fn closed(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        match item["type"].as_str().unwrap_or_default() {
            FUNCTION_CALL => {
                if let Some(id) = item["id"].as_str().and_then(|item_id| {
                    // Removed rather than read: the item is done, and leaving
                    // it would let a later frame quoting a reused id reopen a
                    // closed call.
                    self.calls.remove(item_id)
                }) {
                    events.push(ProviderEvent::ToolCallEnd { id });
                }
            }
            REASONING => {
                if self.seals {
                    sealed(item, events);
                }
                // Unconditional, unlike the sealing half: what a person reads
                // is nobody's replay, so a backend that documents no way to
                // hand state back still has thinking to show.
                self.settled(item, events);
            }
            // A tool the gateway ran itself (**D489**). Recognized by the
            // namespace rather than by an enumerated list of item types,
            // because the namespace is what the vendor documents — "on the
            // Responses API the call becomes an `openrouter:shell` output
            // item" — and because a roster this build has not caught up with
            // is better rendered under its own name than skipped.
            //
            // Ungated by backend: the prefix is one vendor's, and a `Mapping`
            // does not carry which backend it reads for. An item of this shape
            // arriving from anywhere else is that endpoint claiming this
            // vendor's namespace, and a row saying so is the honest outcome.
            kind if kind.starts_with(openrouter::SERVER_TOOL_PREFIX) => {
                server_tool(kind, item, events);
            }
            _ => {}
        }
    }

    /// Reads the usage the terminal frame carries.
    ///
    /// This API reports `input_tokens` as the whole prompt — cache reads and
    /// cache writes included — while [`Usage`]'s input counters are disjoint so
    /// that each can be billed at its own rate. So the two cached counts come
    /// back out of it, exactly as `convert-openai-responses-usage.ts` derives
    /// its `noCache`. Without this a cached session reports its prompt twice
    /// and is priced several times over.
    ///
    /// `output_tokens` is *not* reduced by the reasoning count: [`Usage`]
    /// documents `reasoning_tokens` as a subset of `output_tokens` rather than
    /// a count beside it, and nothing prices it separately.
    ///
    /// Saturating rather than wrapping: an endpoint claiming more cached tokens
    /// than prompt tokens must read as nothing fresh, not as a bill for
    /// eighteen quintillion tokens.
    fn absorb(&mut self, usage: &Value) {
        let input = &usage["input_tokens_details"];
        let output = &usage["output_tokens_details"];

        self.usage.cache_read_tokens = input["cached_tokens"].as_u64().unwrap_or_default();
        self.usage.cache_write_tokens = input["cache_write_tokens"].as_u64().unwrap_or_default();
        self.usage.reasoning_tokens = output["reasoning_tokens"].as_u64().unwrap_or_default();
        self.usage.output_tokens = usage["output_tokens"].as_u64().unwrap_or_default();
        self.usage.input_tokens = usage["input_tokens"]
            .as_u64()
            .unwrap_or_default()
            .saturating_sub(self.usage.cache_read_tokens)
            .saturating_sub(self.usage.cache_write_tokens);
    }
}

/// Reports the sealed thinking a finished reasoning item carries.
///
/// Only the closing frame is read. The API opens a reasoning item before the
/// summary blocks and seals it after
/// (`openai-responses.ts:644-650`), so `response.output_item.added` carries
/// `encrypted_content: null` and the state arrives with
/// `response.output_item.done` (`tool-runtime.test.ts:544-553`).
///
/// An item that closes without state produces nothing at all. There is no
/// replaying it — the backend refuses a previous reasoning item that carries
/// none — so a part recording it would be a row that can never do anything,
/// on every turn of a session that never asked for state
/// (deviation: a-reasoning-item-without-state-is-not-recorded). The *stateless*
/// reasoning part this build does mint means something else entirely: state
/// that existed and was lost, which is `storage::Storage::lost_reasoning`'s.
fn sealed(item: &Value, events: &mut Vec<ProviderEvent>) {
    let Some(id) = item["id"].as_str().filter(|id| !id.is_empty()) else {
        tracing::debug!("a reasoning item arrived without the id that identifies it");
        return;
    };
    let Some(encrypted) = item["encrypted_content"]
        .as_str()
        .filter(|state| !state.is_empty())
    else {
        tracing::debug!(
            item = id,
            "a reasoning item arrived with no state to replay"
        );
        return;
    };

    events.push(ProviderEvent::ReasoningState {
        item: id.to_owned(),
        encrypted: encrypted.to_owned(),
    });
}

/// Reports a tool the gateway ran on its own side (**D489**).
///
/// # What it reads, and what it refuses to assume
///
/// The vendor documents the item's *type* and one tool's fields; it documents
/// no envelope common to all ten, and each tool's arguments mirror whatever
/// shape that tool was modelled on (`openrouter:shell`'s mirror OpenAI's
/// `shell_call.action`). So this reads the two field names the reference does
/// use — `arguments`, which every function-shaped call carries, and `output`,
/// which the shell tool's result section names — and falls back to **the item
/// itself minus its envelope** rather than to a guess: whatever the gateway
/// sent is what a person is shown, which is worse-looking and never wrong.
///
/// A string `arguments` is parsed, because that is how this API carries a
/// call's arguments everywhere else; one that will not parse is kept as the
/// string it was, since a row that says `"{"` is more honest than one that
/// says nothing.
///
/// Nothing here is a [`ProviderEvent::ToolCallStart`]: the work is finished,
/// there is no registry entry to run and no dialog whose answer could change
/// anything. See [`PartBody::ServerTool`] for the rest of that rule.
fn server_tool(kind: &str, item: &Value, events: &mut Vec<ProviderEvent>) {
    /// The keys that identify an item rather than describing the call it made.
    /// `output` is here because it is the *answer*: it has a row of its own,
    /// and a fallback that swept it in would show it twice.
    const ENVELOPE: [&str; 5] = ["type", "id", "status", "call_id", "output"];

    let input = match &item["arguments"] {
        Value::String(arguments) => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.clone()))
        }
        Value::Null => {
            let rest: serde_json::Map<String, Value> = item
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .filter(|(key, _)| !ENVELOPE.contains(&key.as_str()))
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            if rest.is_empty() {
                Value::Null
            } else {
                Value::Object(rest)
            }
        }
        arguments => arguments.clone(),
    };
    let output = match &item["output"] {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        structured => structured.to_string(),
    };

    events.push(ProviderEvent::ServerTool {
        tool: kind.to_owned(),
        input,
        output,
    });
}

/// The item kind a tool call arrives as.
const FUNCTION_CALL: &str = "function_call";

/// "Call one if you decide to", the only one of this API's three tool-choice
/// values an agent loop ever wants: `"none"` would advertise a roster nothing
/// may call, and naming one tool is a decision the *model* is being asked to
/// make. See [`Body::tool_choice`] for which backend is sent it, and why only
/// that one.
const TOOL_CHOICE_AUTO: &str = "auto";

/// The item kind sealed thinking arrives as, and goes back as.
const REASONING: &str = "reasoning";

/// OpenAI's name for a fragment of readable thinking, which only exists
/// downstream of a `reasoning.summary` in the request ([`summarized`]).
const REASONING_SUMMARY_DELTA: &str = "response.reasoning_summary_text.delta";

/// The frame that opens a new summary block inside one reasoning item. Worth
/// mapping for its boundary alone: the deltas of two blocks carry no
/// separator, so this frame is the only place the stream says one thought
/// ended and another began.
const REASONING_SUMMARY_PART: &str = "response.reasoning_summary_part.added";

/// The third spelling of readable thinking, live-observed from
/// `google/gemini-3.7-flash` over the OpenRouter gateway (2026-08-25): the
/// model's thinking streamed under this name and nothing else, so unmapped it
/// reached the debug log and the pane stayed empty — `REASONING_DELTA`'s
/// story, one vendor later.
const REASONING_TEXT_DELTA: &str = "response.reasoning_text.delta";

/// The close of one `REASONING_TEXT_DELTA` block, and the only boundary that
/// stream carries between two thoughts.
const REASONING_TEXT_DONE: &str = "response.reasoning_text.done";

/// OpenRouter's name for the same fragment, published in that vendor's own
/// streaming example and carried in the same `delta` field
/// (`api_reference/responses/reasoning`, read 2026-08-14). It arrives on a
/// request that asked for no summary at all, which is why the gateway sends
/// none: see [`super::openrouter`]'s ledger.
const REASONING_DELTA: &str = "response.reasoning.delta";

/// The one thing this build asks the backend to include beside the reply.
const REASONING_INCLUDE: &str = "reasoning.encrypted_content";

/// Whether a model's thinking is worth asking to have sealed.
///
/// Upstream's predicate, ported literally
/// (`packages/llm/src/providers/openai-options.ts:44-48`): every `gpt-5`
/// except the chat model and `gpt-5-pro`. It is a statement about the *model* —
/// which is why one function answers for both backends — and the two
/// exclusions are models that do not reason at all, so asking would spend a
/// field on every request for state that never comes.
///
/// Note what the second exclusion does *not* catch: `gpt-5.5-pro` does not
/// contain `gpt-5-pro`, so upstream asks for its state too. The literal is
/// kept literal rather than tidied, because a tidier rule would be this
/// build's rule and not the one the endpoint has been answering.
fn seals_reasoning(model: &str) -> bool {
    let id = model.to_ascii_lowercase();

    id.contains("gpt-5") && !id.contains("gpt-5-chat") && !id.contains("gpt-5-pro")
}

/// The effort's options with `reasoning.summary` defaulted to `"auto"` for a
/// model that reasons.
///
/// The Responses API streams no readable thinking unless the request asks —
/// `response.reasoning_summary_text.delta` (which [`Mapping`] already turns
/// into the pane's thinking) only exists downstream of a `reasoning.summary`
/// in the body. `"auto"` is what the Codex CLI itself sends, on the same
/// backend, so it is the vendor's own spelling of "show the thinking".
///
/// Merged key-wise rather than set whole: an effort's `reasoning.effort` must
/// survive beside the summary, and a summary somebody spelled out in their own
/// options is theirs — the default fills absence only. Gated by
/// [`seals_reasoning`] exactly as `include` is, and for the same reason: a
/// model that does not reason answers a `reasoning` field with a 400.
fn summarized(
    options: &serde_json::Map<String, serde_json::Value>,
    model: &str,
    backend: Backend,
) -> serde_json::Map<String, serde_json::Value> {
    let mut options = options.clone();
    // Gated on the backend as well as the model, because both halves of the
    // default are this vendor's: `seals_reasoning` is a rule about *its* model
    // ids, and `"auto"` is what *its* CLI sends. Neither is a statement about a
    // gateway whose roster is mostly other people's models — see
    // [`super::openrouter`], which sends a `reasoning` object only when an
    // effort put one there.
    if !backend.replays_reasoning() || !seals_reasoning(model) {
        return options;
    }

    let reasoning = options
        .entry("reasoning".to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(object) = reasoning {
        object
            .entry("summary".to_owned())
            .or_insert_with(|| serde_json::Value::String("auto".to_owned()));
    }

    options
}

/// The chat-completions sentinel, which some deployments send here too.
const DONE: &str = "[DONE]";

/// Turns an error object into the failure the turn reports.
///
/// The status was 200 by the time any of these arrived and this API's `code` is
/// a slug rather than a number, so `500` is the truest thing there is to say —
/// the same reading the sibling's error chunks get. What the object *did* say
/// is [`super::reported`]'s business, so that a body carrying a `code` and no
/// `message` stops reading as a body that carried nothing.
fn failure(error: &Value) -> ProviderError {
    // Not logged here: the failure is warned once, redacted, at
    // `provider::shielded`, the seam that holds the credential to mask with.
    let message = super::reported(error);

    ProviderError::Status {
        status: 500,
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt as _;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        ACCOUNT_HEADER, ALLOWED_MODELS, Aliases, BETA, BETA_HEADER, Backend, Body,
        CHAT_COMPLETIONS_ONLY, DEFAULT_BASE_URL, Frame, ID, Mapper as _, Mapping, OPENAI_CAP,
        ORIGINATOR, ORIGINATOR_HEADER, ResponsesProvider, SEAT_ROSTER, SUBSCRIPTION_DEFAULT, alias,
        generation, reauth, seals_reasoning, serves, summarized,
    };
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        catalog,
        protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage},
        provider::{
            ChatRequest, CredentialSource, NO_RESULT, PROVIDERS, Presented, Provider as _,
            ProviderError, ProviderEvent, Resolved, openai, openrouter, replay, splice_effort,
        },
        tool::ToolDefinition,
    };

    /// A token no other value in this module could be mistaken for.
    const ACCESS: &str = "at-responses-canary-7717";

    /// The account the credential names.
    const ACCOUNT: &str = "acct_2f7QpL9";

    /// An API key no other value in this module could be mistaken for.
    const KEY: &str = "sk-responses-key-canary-3131";

    /// A model this backend serves (`codex.ts:15`).
    const SERVED: &str = "gpt-5.4";

    /// One it does not, and the one the live pass actually named
    /// (`codex.ts:289`).
    const REFUSED: &str = "gpt-5.6";

    /// A renewal that must never run, for the cases about construction rather
    /// than about a token endpoint.
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

    /// Runs a recorded transcript through the real splitter and mapper.
    async fn events(transcript: &'static str) -> Vec<ProviderEvent> {
        replay(transcript, CancellationToken::new(), Mapping::default())
            .collect()
            .await
    }

    /// The same, read by the mapper a gateway turn installs: nothing sealed,
    /// and this vendor's own event spellings.
    async fn gateway_events(transcript: &'static str) -> Vec<ProviderEvent> {
        replay(
            transcript,
            CancellationToken::new(),
            Mapping::for_backend(Backend::OpenRouter, Aliases::default()),
        )
        .collect()
        .await
    }

    /// The thinking a transcript streams, which is what the ✻ pane renders.
    fn thinking(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The reply text a transcript streams.
    fn text(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A resolved credential, the way one reaches [`ResponsesProvider::request`].
    fn resolved(account_id: Option<&str>) -> Resolved {
        presenting(ACCESS, account_id)
    }

    /// The same, for a test that needs to say which secret travelled.
    fn presenting(secret: &str, account_id: Option<&str>) -> Resolved {
        Resolved {
            presented: Presented::new(secret).expect("a non-blank credential"),
            account_id: account_id.map(str::to_owned),
        }
    }

    /// A subscription provider pointed somewhere a token may travel.
    fn provider() -> ResponsesProvider {
        ResponsesProvider::at(
            "http://127.0.0.1:8080/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect("loopback may carry a token")
    }

    /// The same wire against the platform, authenticated by a key.
    ///
    /// Built through the private constructor rather than through
    /// [`ResponsesProvider::from_env`] because that one reads the environment,
    /// which is process-wide state a unit test must not mutate; what it would
    /// add is the key lookup, and `credentials_env.rs` already owns that.
    fn keyed() -> ResponsesProvider {
        ResponsesProvider::built(
            CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
            "http://127.0.0.1:8080/v1".to_owned(),
            Backend::Platform,
        )
        .expect("loopback may carry a key")
    }

    /// A model the gateway publishes, in that vendor's own namespaced spelling.
    const GATEWAY_MODEL: &str = "openai/gpt-5.4";

    /// The same wire against the gateway, authenticated by that vendor's key.
    fn routed() -> ResponsesProvider {
        ResponsesProvider::built(
            CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
            "http://127.0.0.1:8080/api/v1".to_owned(),
            Backend::OpenRouter,
        )
        .expect("loopback may carry a key")
    }

    /// One turn's worth of request against the gateway.
    fn gateway_ask() -> ChatRequest {
        ChatRequest {
            model: GATEWAY_MODEL.to_owned(),
            ..ask()
        }
    }

    /// One turn's worth of request, on a model this backend serves — anything
    /// else is refused before a request is built at all.
    fn ask() -> ChatRequest {
        ChatRequest {
            effort_options: Default::default(),
            model: SERVED.to_owned(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
        }
    }

    #[test]
    fn the_subscription_wire_is_the_same_vendor_as_the_key_one() {
        assert_eq!(ID, openai::ID, "one provider id, or a turn is priced wrong");
        assert!(
            PROVIDERS.contains(&ID),
            "a provider nothing can select is a provider nobody has"
        );
        assert_eq!(ID, auth::openai::PROVIDER_ID, "and one credential to read");
        assert_eq!(
            format!("{DEFAULT_BASE_URL}/responses"),
            "https://chatgpt.com/backend-api/codex/responses",
            "codex.ts:12 — a ChatGPT token is minted for this backend and \
             api.openai.com refuses it"
        );
        assert_eq!(
            format!("{}/responses", Backend::Platform.default_base_url()),
            "https://api.openai.com/v1/responses",
            "the endpoint the live 400 named: \"To use function tools, use \
             /v1/responses\""
        );
    }

    /// Every header the codex backend uses to decide whether to serve a request
    /// at all. Dropping any one of them is a turn that fails in production and
    /// nowhere else, which is why this is asserted on the request rather than
    /// on the code that builds it.
    #[test]
    fn every_subscription_request_names_the_account_the_originator_and_the_agent() {
        let built = provider()
            .request(&resolved(Some(ACCOUNT)), &ask())
            .expect("the request builds");
        let headers = built.headers();
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

        assert_eq!(
            built.url().as_str(),
            "http://127.0.0.1:8080/backend-api/codex/responses"
        );
        assert_eq!(
            header("authorization"),
            Some(format!("Bearer {ACCESS}")).as_deref()
        );
        assert_eq!(
            header(ACCOUNT_HEADER),
            Some(ACCOUNT),
            "codex.ts:406-408 — without it the backend cannot tell which of a \
             person's accounts to serve"
        );
        assert_eq!(header(ORIGINATOR_HEADER), Some(ORIGINATOR));
        assert_eq!(header(BETA_HEADER), Some(BETA));
        assert_eq!(
            header("user-agent"),
            Some(auth::device::UPSTREAM_USER_AGENT),
            "one User-Agent for every request this build makes against \
             somebody else's client registration"
        );

        // A credential naming no account still makes a request: most people
        // have exactly one, and `auth::openai` reads a token with no such claim
        // as a login that worked.
        let anonymous = provider()
            .request(&resolved(None), &ask())
            .expect("the request builds");
        assert!(
            !anonymous.headers().contains_key(ACCOUNT_HEADER),
            "an account nobody named must not travel as an empty string"
        );
    }

    /// The other backend's request, which is the same body under a bearer and
    /// **nothing else**.
    ///
    /// Each of the four headers above exists because the subscription request
    /// impersonates the Codex CLI against a client registration this project
    /// borrowed; a key is the caller's own credential against the platform, and
    /// upstream sends such a request through the unwrapped `fetch`
    /// (`codex.ts:356`), so it gains none of them. Asserted as absences because
    /// that is the failure mode — a header added on a hunch travels with
    /// somebody's API key to an endpoint that never asked for it.
    #[test]
    fn a_key_request_carries_the_bearer_and_none_of_the_subscription_headers() {
        // An account id on a key credential is impossible — `CredentialSource::Key`
        // resolves with `account_id: None` — but the header is skipped by
        // *backend* rather than by whether one was resolved, so handing it one
        // anyway proves the branch instead of the coincidence.
        let built = keyed()
            .request(&presenting(KEY, Some(ACCOUNT)), &ask())
            .expect("the request builds");
        let headers = built.headers();

        assert_eq!(
            built.url().as_str(),
            "http://127.0.0.1:8080/v1/responses",
            "the platform's own Responses path, under whatever base URL points \
             at it"
        );
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {KEY}")).as_deref()
        );
        for absent in [ACCOUNT_HEADER, ORIGINATOR_HEADER, BETA_HEADER, "user-agent"] {
            assert!(
                !headers.contains_key(absent),
                "`{absent}` belongs to the subscription backend and reached the \
                 platform: {headers:?}"
            );
        }
        assert_eq!(
            serde_json::to_value(Body::new(&ask(), Backend::Platform))
                .expect("the body serializes")["store"],
            json!(false),
            "one encoder, so `store: false` is not a subscription special case"
        );
    }

    /// The third backend's request: the same encoder pointed at another vendor,
    /// under that vendor's own bearer and with **nothing** of this one's.
    ///
    /// Spelled as absences for the reason the platform test above is: a field
    /// carried over on the assumption that one Responses surface is every
    /// Responses surface is exactly the failure this backend exists to prevent,
    /// and each absence here is a row of `super::openrouter`'s ledger.
    #[test]
    fn an_openrouter_request_carries_only_what_that_vendor_documents() {
        let provider = routed();
        let built = provider
            .request(&presenting(KEY, Some(ACCOUNT)), &gateway_ask())
            .expect("the request builds");
        let headers = built.headers();

        assert_eq!(
            built.url().as_str(),
            "http://127.0.0.1:8080/api/v1/responses",
            "the vendor's own Responses path, under whatever base URL points at it"
        );
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {KEY}")).as_deref(),
            "the reference asks for a bearer and a content type, and this is the \
             half that is a credential"
        );
        for absent in [ACCOUNT_HEADER, ORIGINATOR_HEADER, BETA_HEADER, "user-agent"] {
            assert!(
                !headers.contains_key(absent),
                "`{absent}` belongs to a ChatGPT seat impersonating the Codex CLI \
                 and reached a different vendor entirely: {headers:?}"
            );
        }

        let body = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
            .expect("the body serializes");
        assert_eq!(
            body["store"],
            json!(false),
            "the reference rejects `store: true` outright, so `false` is the only \
             value a stateless API takes"
        );
        assert!(
            body.get("include").is_none(),
            "the reference documents no `include` parameter at all: {body}"
        );
        assert!(
            body.get("previous_response_id").is_none(),
            "the other half of the same rejection, and a field this encoder has \
             never had: {body}"
        );
        assert_eq!(
            body["model"],
            json!(GATEWAY_MODEL),
            "the id passes through \
             verbatim, namespace and all — it is the vendor's own spelling"
        );
    }

    /// Asking for sealed reasoning and replaying it are one feature, and this
    /// backend does neither: the vendor documents no `include` to ask with and
    /// no way to hand the state back. Half of the pairing would be worse than
    /// neither — a request that asked and never replayed spends a field every
    /// turn for nothing, and one that replayed unasked is a guess whose failure
    /// lands on the *second* request of every reasoning turn.
    ///
    /// The same transcript on the platform backend is the control: what differs
    /// is the backend and nothing else about the fixture.
    #[test]
    fn an_openrouter_turn_neither_asks_for_sealed_reasoning_nor_replays_it() {
        let mut assistant = Message::assistant("gpt");
        assistant.parts.push(Part::text("thinking about it"));
        assistant.parts.push(Part::reasoning(
            openrouter::ID,
            "rs_gateway",
            Some("sealed-by-the-gateway".to_owned()),
        ));

        let mut request = gateway_ask();
        request.messages.push(assistant);

        let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");
        assert!(
            body.get("include").is_none(),
            "nothing was asked for: {body}"
        );
        assert!(
            !body["input"]
                .as_array()
                .expect("input is a list")
                .iter()
                .any(|item| item["type"] == json!("reasoning")),
            "and nothing is handed back: {body}"
        );

        // The control. A model whose id `seals_reasoning` recognizes, on the
        // backend whose vendor documents the pairing, does both — so what the
        // assertions above prove is the backend and not the fixture.
        let mut owned = Message::assistant("gpt");
        owned
            .parts
            .push(Part::reasoning(ID, "rs_1", Some("sealed".to_owned())));
        let mut control = ask();
        control.messages.push(owned);

        let platform = serde_json::to_value(Body::new(&control, Backend::Platform))
            .expect("the body serializes");
        assert_eq!(platform["include"], json!(["reasoning.encrypted_content"]));
        assert!(
            platform["input"]
                .as_array()
                .expect("input is a list")
                .iter()
                .any(|item| item["type"] == json!("reasoning")),
            "{platform}"
        );
    }

    /// The reading half of the same decision. A mapper that recorded state this
    /// build will never hand back would mint the row-that-can-never-do-anything
    /// [`super::sealed`]'s own doc refuses, one layer further out.
    #[test]
    fn the_backend_that_replays_sealed_state_is_the_one_that_records_it() {
        for backend in [Backend::Codex, Backend::Platform] {
            assert!(
                Mapping::for_backend(backend, Aliases::default()).seals
                    && backend.replays_reasoning(),
                "{backend:?} documents the pairing, so both halves are on"
            );
        }
        assert!(
            !Mapping::for_backend(Backend::OpenRouter, Aliases::default()).seals
                && !Backend::OpenRouter.replays_reasoning(),
            "and both halves are off together, or the transcript fills with \
             state nothing will ever send"
        );
    }

    /// `reasoning.summary: "auto"` is two of this vendor's decisions in one
    /// field — [`seals_reasoning`] is a rule about *its* model ids, and `"auto"`
    /// is what *its* CLI sends — so a gateway fronting mostly other people's
    /// models gets neither. What an effort put there still travels, because the
    /// reference does document `reasoning` with effort levels.
    #[test]
    fn an_openrouter_request_defaults_no_summary_and_still_carries_an_effort() {
        let bare = summarized(
            &serde_json::Map::new(),
            "openai/gpt-5.4",
            Backend::OpenRouter,
        );
        assert!(
            bare.is_empty(),
            "an id that merely *contains* this vendor's model family is not this \
             vendor's model: {bare:?}"
        );

        let mut request = gateway_ask();
        request.effort_options = json!({"reasoning": {"effort": "high"}})
            .as_object()
            .cloned()
            .expect("object fixture");
        let own = Body::new(&request, Backend::OpenRouter);
        let options = summarized(&request.effort_options, &request.model, Backend::OpenRouter);
        let body =
            serde_json::to_value(splice_effort(&options, &own)).expect("a spliced body serializes");

        assert_eq!(
            body["reasoning"],
            json!({"effort": "high"}),
            "the effort's own object, and not a summary nobody asked for"
        );
    }

    /// A gateway row as the catalog holds it once [`crate::effort::roster`] has
    /// run over it, which is what `/effort`, `run --effort` and the config seed
    /// all read.
    fn gateway_row() -> catalog::ModelInfo {
        let mut row = catalog::ModelInfo {
            id: GATEWAY_MODEL.to_owned(),
            provider_id: openrouter::ID.to_owned(),
            name: "GPT-5.4".to_owned(),
            context_window: 1_050_000,
            max_output: 128_000,
            input_limit: None,
            pricing: catalog::Pricing {
                input: 2.5,
                output: 15.0,
                cache_read: 0.25,
                cache_write: None,
            },
            family: None,
            release_date: None,
            tool_call: true,
            status: catalog::ModelStatus::Active,
            reasoning: true,
            reasoning_options: None,
            npm: None,
            variants: std::collections::BTreeMap::new(),
        };
        row.variants = crate::effort::roster(&row);

        row
    }

    /// The whole of R1, end to end on the one seam that matters: the roster the
    /// catalog row carries is what a person is offered, and the option map each
    /// entry holds is what the request body ends up carrying — exactly
    /// `reasoning.effort`, and none of the two fields this vendor's ledger
    /// drops.
    #[test]
    fn every_effort_this_gateway_offers_travels_as_the_one_field_it_documents() {
        let row = gateway_row();
        assert_eq!(
            row.variants.keys().map(String::as_str).collect::<Vec<_>>(),
            ["high", "low", "medium", "minimal"],
            "the reference's own four levels reach the chooser"
        );

        for (name, options) in &row.variants {
            let mut request = gateway_ask();
            request.effort_options = options.clone();

            let own = Body::new(&request, Backend::OpenRouter);
            let spliced = summarized(&request.effort_options, &request.model, Backend::OpenRouter);
            let body = serde_json::to_value(splice_effort(&spliced, &own))
                .expect("a spliced body serializes");

            assert_eq!(
                body["reasoning"],
                json!({"effort": name}),
                "`{name}` has to reach the wire as the reference's own object"
            );
            assert!(
                body.get("include").is_none() && body["reasoning"].get("summary").is_none(),
                "the effort door is not the way the dropped fields come back: {body}"
            );
        }

        // The standing posture, re-pinned beside them: no effort selected is no
        // `reasoning` key at all.
        let bare = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
            .expect("the body serializes");
        assert!(bare.get("reasoning").is_none(), "got {bare}");
    }

    /// The splice order at this wire's send site: an effort adds what the body
    /// does not carry — `reasoning` is the catalog's use of it — and loses
    /// every key the wire itself writes; `store: false` in particular is what
    /// the ChatGPT backend requires, so no catalog row may unmake it.
    #[test]
    fn an_effort_adds_reasoning_but_cannot_claim_store() {
        let mut request = ask();
        request.effort_options = json!({
            "reasoning": {"effort": "high"},
            "store": true,
            "model": "someone-elses",
        })
        .as_object()
        .cloned()
        .expect("the fixture options are an object");

        let own = Body::new(&request, Backend::Platform);
        let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
            .expect("a spliced body serializes");

        assert_eq!(
            body["reasoning"],
            json!({"effort": "high"}),
            "a key the wire does not write arrives verbatim"
        );
        assert_eq!(
            body["store"],
            json!(false),
            "a key the wire writes resolves to the wire"
        );
        assert_eq!(body["model"], json!(SERVED));
    }

    /// Readable thinking exists only downstream of asking for it: the body
    /// carries `reasoning.summary: "auto"` for a model that reasons, the
    /// default fills absence only, and a model that does not reason gets no
    /// `reasoning` field to be refused over.
    #[test]
    fn a_reasoning_model_is_asked_to_show_its_thinking() {
        let cases: Vec<(
            &str,
            serde_json::Map<String, serde_json::Value>,
            serde_json::Value,
        )> = vec![
            (
                "bare request asks for auto",
                serde_json::Map::new(),
                json!({"summary": "auto"}),
            ),
            (
                "an effort's own keys survive beside the default",
                json!({"reasoning": {"effort": "high"}})
                    .as_object()
                    .cloned()
                    .expect("object fixture"),
                json!({"effort": "high", "summary": "auto"}),
            ),
            (
                "a summary somebody spelled out is theirs",
                json!({"reasoning": {"summary": "detailed"}})
                    .as_object()
                    .cloned()
                    .expect("object fixture"),
                json!({"summary": "detailed"}),
            ),
        ];
        for (name, options, expected) in cases {
            let mut request = ask();
            request.effort_options = options;
            let own = Body::new(&request, Backend::Platform);
            let options = summarized(&request.effort_options, &request.model, Backend::Platform);
            let body = serde_json::to_value(splice_effort(&options, &own))
                .expect("a spliced body serializes");
            assert_eq!(body["reasoning"], expected, "{name}");
        }

        let mut request = ask();
        request.model = "gpt-5-chat".to_owned();
        let own = Body::new(&request, Backend::Platform);
        let options = summarized(&request.effort_options, &request.model, Backend::Platform);
        let body =
            serde_json::to_value(splice_effort(&options, &own)).expect("a spliced body serializes");
        assert!(
            body.get("reasoning").is_none(),
            "a model that does not reason is asked nothing about reasoning"
        );
    }

    /// The allow-list is a ChatGPT seat's product decision, and applying it to
    /// a key would be somebody else's catalog deciding what an API key may ask
    /// for. Upstream scopes it the same way and on the same condition:
    /// `codex.ts:281` returns `provider.models` unfiltered whenever the
    /// credential is not an OAuth one.
    ///
    /// The model is the one the live pass proved the seat refuses, so this
    /// says the two backends answer the same name differently.
    #[test]
    fn the_seats_allow_list_gates_the_subscription_backend_and_not_the_platform() {
        assert!(
            provider().refuses(REFUSED).is_some(),
            "{REFUSED} is `codex.ts:289`'s own arm and the seat cannot run it"
        );
        assert!(
            keyed().refuses(REFUSED).is_none(),
            "a key session held to a seat's allow-list cannot reach the models \
             it is paying for, which is the whole reason this wire moved"
        );

        // Both directions of the scoping, so removing the backend check fails
        // rather than merely widening what is served.
        assert!(provider().refuses(SERVED).is_none() && keyed().refuses(SERVED).is_none());
    }

    /// The one refusal that is not about a seat.
    ///
    /// Upstream hides `gpt-5-chat-latest` from the OpenAI catalog outright,
    /// with the reason in a comment at `plugin/provider/openai.ts:164-171`: the
    /// plugin sends every OpenAI model through Responses and that alias is
    /// chat-completions-only. It is therefore refused on **both** backends —
    /// the vendor has no wire left that could serve it — and the message says
    /// so rather than pointing at an API key that would not help.
    #[test]
    fn a_chat_completions_only_model_is_refused_on_both_backends() {
        // Named literally as well as iterated: a list this test only reads out
        // of would agree with itself however it was edited, and this is the one
        // string `plugin/provider/openai.ts:166` actually disables.
        assert!(
            CHAT_COMPLETIONS_ONLY.contains(&"gpt-5-chat-latest"),
            "the alias upstream hides is what this list is for"
        );

        for alias in CHAT_COMPLETIONS_ONLY {
            for (backend, provider) in [("subscription", provider()), ("key", keyed())] {
                let refused = provider
                    .refuses(alias)
                    .unwrap_or_else(|| panic!("{backend}: {alias} has no wire here to serve it"))
                    .to_string();

                assert!(
                    refused.contains(alias) && refused.contains("Responses"),
                    "{backend}: the refusal has to say why there is no wire for \
                     it: {refused}"
                );
                assert!(
                    !refused.contains(openai::API_KEY_ENV),
                    "{backend}: a key is not the way out of this one, and \
                     offering it sends somebody to buy nothing: {refused}"
                );
            }
        }

        // The catalog this build compiles in carries no such row, which is
        // exactly why the refusal lives at the wire: `ganja models --refresh`
        // replaces that table with upstream's own file, and this list has to
        // keep applying to whatever it brings.
        assert!(
            CHAT_COMPLETIONS_ONLY
                .iter()
                .all(|alias| catalog::model(alias).is_none()),
            "a row the snapshot now carries wants deciding here as well as at \
             the wire"
        );
    }

    /// The endpoint is not exempt from the rule every other provider's is held
    /// to just because the credential arrived as a token rather than as a key.
    #[test]
    fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
        let refused = ResponsesProvider::at(
            "http://chatgpt.com/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect_err("plain http to a public host puts the token on the wire in the clear");

        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
        assert!(
            ResponsesProvider::at("http://127.0.0.1:8080", Arc::new(NeverRenews)).is_ok(),
            "loopback never reaches a network, which is what a test depends on"
        );
    }

    #[test]
    fn a_provider_never_renders_its_credential() {
        let provider = ResponsesProvider::at(
            "http://ganja:at-url-canary-9999@127.0.0.1:8080/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect("loopback may carry a token");
        let rendered = format!("{provider:?}");

        assert!(
            !rendered.contains("at-url-canary-9999") && !rendered.contains("ganja:"),
            "a provider leaked its endpoint's userinfo: {rendered}"
        );
        assert!(
            rendered.contains("Oauth") && rendered.contains(ID),
            "a provider renders as which provider it is: {rendered}"
        );
        assert!(
            rendered.contains("127.0.0.1:8080"),
            "the endpoint is what tells one provider from another: {rendered}"
        );
    }

    /// A subscription session that names no model does **not** take the
    /// catalog's default, and this is the half of that statement this module
    /// owns.
    ///
    /// The catalog holds one default per vendor, which is one too few here: a
    /// row the platform sells and the seat does not — `gpt-5.6` is exactly that
    /// — would hand a ChatGPT login a model its backend refuses outright, which
    /// is a session that cannot take a single turn. So the seat brings its own,
    /// and the obligations on it are both directions at once: the backend has
    /// to serve it, and the catalog has to be able to size and price it.
    ///
    /// The other half — that `select` actually reaches for this rather than for
    /// the catalog — is `responses_wire.rs`'s, because it takes an environment
    /// and a store to observe.
    #[test]
    fn a_subscription_session_that_names_no_model_gets_one_the_seat_can_run() {
        let info =
            catalog::model(SUBSCRIPTION_DEFAULT).expect("the subscription default is in the table");

        assert_eq!(info.provider_id, ID);
        assert!(info.context_window > 0 && info.max_output > 0);
        assert!(
            serves(SUBSCRIPTION_DEFAULT),
            "a default this backend refuses is a seat that cannot take a turn"
        );
        assert!(
            ALLOWED_MODELS.contains(&SUBSCRIPTION_DEFAULT),
            "it is named outright by `codex.ts:15` rather than admitted by the \
             generation rule, which is what keeps it from moving under us when \
             the rule does"
        );
    }

    /// The obligation [`SEAT_ROSTER`] carries: an offer this backend would
    /// then refuse is a listing that lies, and the two halves of the roster
    /// reach [`serves`] by different routes — two are named by
    /// [`ALLOWED_MODELS`], three are admitted by the generation rule — so the
    /// pin has to be asserted over the whole list rather than over either.
    #[test]
    fn every_model_the_seat_offers_is_one_the_seat_serves() {
        for offered in SEAT_ROSTER {
            assert!(
                serves(offered),
                "the roster offers `{offered}`, which this backend refuses"
            );
        }
    }

    /// The other half of **D476**: the pin narrows what is *offered*, never
    /// what is *servable*. Somebody who types `--model openai/gpt-5.4` on a
    /// seat still takes their turn, although no listing volunteered it — which
    /// is why the roster is a separate constant rather than a shorter
    /// [`ALLOWED_MODELS`].
    #[test]
    fn a_model_the_roster_leaves_out_is_still_one_an_explicit_request_may_name() {
        for unoffered in ["gpt-5.4", "gpt-5.4-mini"] {
            assert!(
                !SEAT_ROSTER.contains(&unoffered),
                "`{unoffered}` is deliberately unoffered"
            );
            assert!(
                serves(unoffered),
                "and deliberately still servable: the pin is an offer, not a gate"
            );
        }
    }

    /// The split that makes "display-only" a fact about this build rather
    /// than a sentence in a doc comment (bead `pwe`), and the wire where both
    /// halves are visible at once: this API *does* take reasoning back, so the
    /// body below has to carry the sealed item and not the readable one.
    ///
    /// A single moved match arm is all it would take to send both, and the
    /// failure would be invisible — the request would still be accepted, and
    /// the model would simply be handed the same thought twice, once in a
    /// form it never asked for.
    #[test]
    fn a_transcript_held_thought_is_absent_from_the_body_while_the_sealed_half_travels() {
        const THOUGHT: &str = "the-user-is-probably-testing-me";
        const SEALED: &str = "sealed-blob-0001";

        let mut turn = Message::assistant(SERVED);
        turn.parts.push(Part::reasoning_text(THOUGHT));
        turn.parts.push(Part::text("Hello!"));
        turn.parts
            .push(Part::reasoning(ID, "rs_1", Some(SEALED.to_owned())));

        let request = ChatRequest {
            messages: vec![Message::user("hi"), turn, Message::user("again")],
            ..ask()
        };
        let body = serde_json::to_string(&Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert!(
            !body.contains(THOUGHT),
            "the thought reached the wire; nothing sends readable reasoning: {body}"
        );
        assert!(
            body.contains(SEALED),
            "the *sealed* half is what this API asked to have handed back, and \
             a build that dropped it starts every reasoning turn from nothing \
             — this test must fail if the split is collapsed either way: {body}"
        );
        assert!(
            body.contains("Hello!"),
            "the reply still has to be sent: {body}"
        );
    }

    /// The field the live pass died on. A body without it is answered
    /// `400 {"detail":"Store must be set to false"}`, which is every
    /// subscription turn this build could take — so it is asserted on the
    /// serialized body rather than on the struct that produces it.
    ///
    /// Its companion is here too: with the backend keeping nothing, `include`
    /// is the only reason a second request can carry the first one's thinking.
    #[test]
    fn every_body_tells_the_backend_to_keep_nothing_and_to_seal_what_it_thought() {
        let body = serde_json::to_value(Body::new(&ask(), Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["store"],
            json!(false),
            "without this the backend refuses the turn outright: got {body}"
        );
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "`store: false` without this is a reasoning model whose every turn \
             starts from nothing: got {body}"
        );
    }

    /// Upstream attaches the include to the *model*, so this build asks the
    /// same question of both backends rather than of the credential. The
    /// exclusions are the two OpenAI models that do not reason — and
    /// `gpt-5.5-pro`, which reads like one and is not spelled like one, is
    /// deliberately still asked, because that is what the pin's literal does.
    #[test]
    fn asking_for_sealed_reasoning_is_a_question_about_the_model() {
        for reasons in [
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.3-codex-spark",
            "GPT-5.4-MINI",
            "gpt-5.5-pro",
        ] {
            assert!(seals_reasoning(reasons), "{reasons} reasons");
        }
        for plain in ["gpt-5-chat-latest", "gpt-5-pro", "gpt-4.1", "o3", "claude"] {
            assert!(!seals_reasoning(plain), "{plain} has nothing to seal");
        }

        let plain = ChatRequest {
            model: "gpt-4.1".to_owned(),
            ..ask()
        };
        let body = serde_json::to_value(Body::new(&plain, Backend::Platform))
            .expect("the body serializes");
        assert!(
            body["include"].is_null(),
            "a body that asks for nothing omits the field entirely, the way \
             upstream's does: got {body}"
        );
        assert_eq!(body["store"], json!(false), "and still keeps nothing");
    }

    /// A step's sealed thinking goes back in the shape the pin's second
    /// request carries — before the calls it produced, with an empty summary
    /// and **no** item id, because under `store: false` there is no
    /// server-side item for one to name
    /// (`packages/llm/test/tool-runtime.test.ts:599-604`).
    #[test]
    fn a_sealed_thought_is_replayed_before_the_calls_it_produced() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant
            .parts
            .push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: SERVED.to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
                {"type": "reasoning", "summary": [], "encrypted_content": "sealed-state"},
                {
                    "type": "function_call",
                    "call_id": "call_read",
                    "name": "read",
                    "arguments": r#"{"filePath":"src/main.rs"}"#,
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_read",
                    "output": "fn main() {}",
                },
            ]),
            "got {body}"
        );
    }

    /// The exact item shapes the Responses API documents for attachments,
    /// pinned so a drift in the encoder is a red test rather than a 400 from
    /// the vendor: an image rides `input_image` as a base64 data URL, a PDF
    /// rides `input_file` with the mentioned path as its `filename`, and a
    /// file part carrying no content is a reference the engine already
    /// resolved, encoding nothing.
    #[test]
    fn an_attachment_becomes_the_input_item_its_mime_names() {
        let mut user = Message::user("what are these");
        user.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::File {
                path: "shot.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: Some("aW1n".to_owned()),
            },
        });
        user.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::File {
                path: "docs/paper.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                start: None,
                end: None,
                content: Some("cGRm".to_owned()),
            },
        });
        user.parts.push(Part::file("notes.md", "text/plain"));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: SERVED.to_owned(),
            system: None,
            messages: vec![user],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what are these"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aW1n"},
                    {
                        "type": "input_file",
                        "filename": "docs/paper.pdf",
                        "file_data": "data:application/pdf;base64,cGRm",
                    },
                ],
            }]),
            "got {body}"
        );
    }

    /// The three reasoning parts a request must **not** put on the wire, each
    /// for its own reason, and each pinned because sending one is a refused
    /// request rather than a degraded one.
    #[test]
    fn reasoning_with_nothing_to_replay_never_reaches_the_wire() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        // 1. State this build does not hold — an item that arrived without
        //    any, or a stored record that would not decode. Upstream drops it
        //    at `openai-responses.ts:451` and so does this.
        assistant.parts.push(Part::reasoning(ID, "rs_lost", None));
        // 2. Another wire's state, which means nothing to this one.
        assistant.parts.push(Part::reasoning(
            "anthropic",
            "th_1",
            Some("someone-elses-state".to_owned()),
        ));
        // 3. The same item twice, which is one item said twice.
        assistant
            .parts
            .push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));
        assistant
            .parts
            .push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: SERVED.to_owned(),
            system: None,
            messages: vec![Message::user("hello"), assistant],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "reasoning", "summary": [], "encrypted_content": "sealed-state"},
            ]),
            "one item survives all three rules, and it is the one this wire \
             sealed itself: got {body}"
        );
    }

    /// Two summary blocks on one stream stay two thoughts: the part
    /// boundary the provider announces between them becomes a break, so they
    /// cannot glue into "PlanningDesigning" downstream — and the boundary
    /// ahead of the first block says nothing, because there is nothing yet
    /// to break from.
    #[tokio::test]
    async fn a_second_summary_part_breaks_the_thought_before_it() {
        let seen = events(concat!(
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Planning"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Designing"}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        let thoughts: Vec<&ProviderEvent> = seen
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak
                )
            })
            .collect();
        assert_eq!(
            thoughts,
            vec![
                &ProviderEvent::ReasoningDelta("Planning".to_owned()),
                &ProviderEvent::ReasoningBreak,
                &ProviderEvent::ReasoningDelta("Designing".to_owned()),
            ],
            "got {seen:?}"
        );
    }

    /// The spelling Gemini serves over the gateway: thinking streams as
    /// `response.reasoning_text.delta` and its blocks close with `.done`,
    /// which is the only boundary that stream carries — mapped, the two
    /// thoughts stay two thoughts; unmapped, the pane stayed empty
    /// (2026-08-25).
    #[tokio::test]
    async fn a_reasoning_text_stream_breaks_at_its_own_block_close() {
        let seen = events(concat!(
            r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_1","delta":"Weighing"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_2","delta":"Steeping"}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        let thoughts: Vec<&ProviderEvent> = seen
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak
                )
            })
            .collect();
        assert_eq!(
            thoughts,
            vec![
                &ProviderEvent::ReasoningDelta("Weighing".to_owned()),
                &ProviderEvent::ReasoningBreak,
                &ProviderEvent::ReasoningDelta("Steeping".to_owned()),
            ],
            "got {seen:?}"
        );
    }

    /// The receiving half: the state arrives on the item's *closing* frame,
    /// the opening one having carried `encrypted_content: null`
    /// (`tool-runtime.test.ts:544-553`).
    #[tokio::test]
    async fn a_reasoning_item_is_taken_when_it_closes_and_only_if_it_was_sealed() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1","encrypted_content":null}}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Short is right."}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"sealed-state"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        assert!(
            seen.contains(&ProviderEvent::ReasoningState {
                item: "rs_1".to_owned(),
                encrypted: "sealed-state".to_owned(),
            }),
            "the sealed state has to reach the loop or nothing can replay it: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, ProviderEvent::ReasoningState { .. }))
                .count(),
            1,
            "the item opened once and closed once: {seen:?}"
        );
        assert!(
            seen.contains(&ProviderEvent::ReasoningDelta("Short is right.".to_owned())),
            "the readable half is unchanged by any of this: {seen:?}"
        );
    }

    /// The gateway's own event name reaches the pane the vendor's does.
    ///
    /// Unmapped it reached the debug log instead, so a reasoning turn over
    /// OpenRouter streamed a reply with no thinking under it — the one symptom
    /// this arm exists to remove.
    #[tokio::test]
    async fn a_gateways_reasoning_delta_is_read_as_thinking() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"First, the year. "}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Then the difference."}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Yes."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(thinking(&seen), "First, the year. Then the difference.");
        assert_eq!(text(&seen), "Yes.");
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
            "readable thinking is not sealed state, and this backend records \
             none of the latter: {seen:?}"
        );
    }

    /// Pre-mortem 1: a gateway relaying one vendor's stream through its own
    /// normalization can carry both spellings of the same event. The first one
    /// to say anything wins for the whole response, because a pane that renders
    /// one train of thought twice is worse than one that renders the shorter of
    /// the two.
    #[tokio::test]
    async fn both_reasoning_spellings_on_one_stream_render_one_train_of_thought() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Thinking"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Thinking"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":" it through."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(thinking(&seen), "Thinking it through.");
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, ProviderEvent::ReasoningDelta(_)))
                .count(),
            2,
            "the relayed copy is dropped rather than appended: {seen:?}"
        );

        // The latch is about *content*: an empty fragment under a spelling the
        // stream then abandons must not lock the other one out.
        let recovered = gateway_events(concat!(
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":""}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Still heard."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;
        assert_eq!(thinking(&recovered), "Still heard.");
    }

    /// The vendor documents a `summary` array on the settled reasoning item and
    /// no parameter to ask for one, so on a turn that streamed nothing readable
    /// the closing frame is the only place thinking exists.
    #[tokio::test]
    async fn a_summary_that_was_never_streamed_still_reaches_the_pane() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
            r#""encrypted_content":"sealed","summary":["First the year","Then the difference"]}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        let thoughts: Vec<&ProviderEvent> = seen
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak
                )
            })
            .collect();
        assert_eq!(
            thoughts,
            vec![
                &ProviderEvent::ReasoningDelta("First the year".to_owned()),
                &ProviderEvent::ReasoningBreak,
                &ProviderEvent::ReasoningDelta("Then the difference".to_owned()),
            ],
            "each block is a thought of its own: {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
            "a summary is not state, and this backend still seals nothing: {seen:?}"
        );

        // A stream that streamed its thinking is already served: the same item
        // closing with the same words must not say them again.
        let streamed = gateway_events(concat!(
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"First the year"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
            r#""summary":["First the year"]}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;
        assert_eq!(thinking(&streamed), "First the year");

        // And the other vendor's shape of the same field, which arrives as
        // blocks rather than as strings, is read too — a summary that closed a
        // turn nobody asked a summary of is thinking either way.
        let blocked = events(concat!(
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
            r#""encrypted_content":"sealed","summary":[{"type":"summary_text","text":"Blocked."}]}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;
        assert_eq!(thinking(&blocked), "Blocked.");
    }

    /// An item with nothing to replay produces no part at all: there is no
    /// sending it back, and a row that can never do anything on every turn is
    /// worse than none (deviation:
    /// a-reasoning-item-without-state-is-not-recorded).
    #[tokio::test]
    async fn a_reasoning_item_that_was_never_sealed_leaves_no_trace() {
        for (item, transcript) in [
            (
                "a null state",
                concat!(
                    r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":null}}"#,
                    "\n\n",
                    r#"data: {"type":"response.completed","response":{}}"#,
                    "\n\n",
                ),
            ),
            (
                "no state field at all",
                concat!(
                    r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1"}}"#,
                    "\n\n",
                    r#"data: {"type":"response.completed","response":{}}"#,
                    "\n\n",
                ),
            ),
            (
                "an empty state",
                concat!(
                    r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":""}}"#,
                    "\n\n",
                    r#"data: {"type":"response.completed","response":{}}"#,
                    "\n\n",
                ),
            ),
            (
                // No id is no item: upstream requires a non-empty one
                // (`openai-responses.ts:572-573`).
                "state on an item with no id",
                concat!(
                    r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"sealed-state"}}"#,
                    "\n\n",
                    r#"data: {"type":"response.completed","response":{}}"#,
                    "\n\n",
                ),
            ),
        ] {
            let seen = events(transcript).await;

            assert!(
                !seen
                    .iter()
                    .any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
                "{item} has no state to replay: {seen:?}"
            );
            assert!(
                seen.contains(&ProviderEvent::Finish(FinishReason::Completed)),
                "and the turn still ends normally: {seen:?}"
            );
        }
    }

    /// The backend serves a pinned list, and the first live ChatGPT turn met it
    /// as `400 {"detail":"The 'gpt-5.6' model is not supported when using Codex
    /// with a ChatGPT account."}`. Every arm of `codex.ts:281-292` that ganja
    /// can express is here, in the order that makes it correct.
    #[test]
    fn the_backend_serves_a_pinned_list_and_the_order_of_the_rules_is_the_rule() {
        for served in ALLOWED_MODELS {
            assert!(serves(served), "codex.ts:15 names {served}");
        }
        // Three of those four are older than the floor, so a check that read
        // the generation rule first would refuse the models the list exists to
        // allow — including the one this build now defaults to.
        assert!(
            serves("gpt-5.4") && generation("gpt-5.4") == Some(5.4),
            "gpt-5.4 is not newer than 5.4 and is served anyway, which is what \
             makes the list order load-bearing"
        );

        for refused in ["gpt-5.6", "gpt-5.5-pro", "gpt-5.4-nano", "gpt-5.3-codex"] {
            assert!(
                !serves(refused),
                "{refused} is refused by the backend and has to be refused here"
            );
        }

        // The forward hedge: a row the catalog gains later is reachable without
        // a code change, and anything that is not a `gpt-N.M` at all is not.
        assert!(serves("gpt-5.7") && serves("gpt-6.0-codex"));
        assert!(!serves("gpt-5") && !serves("o3") && !serves("claude-sonnet-5"));
        assert_eq!(
            generation("gpt-5.4-mini"),
            Some(5.4),
            "the halves are the id's"
        );
        assert_eq!(generation("gpt-5"), None, "the fraction is required");
    }

    /// A refusal that only says no leaves a person guessing at a list they
    /// cannot see. This one is what they read instead of the backend's JSON.
    #[tokio::test]
    async fn an_unsupported_model_is_refused_here_naming_what_the_seat_does_serve() {
        // The success arm is a boxed stream with no `Debug`, so `expect_err`
        // would not compile; the match is the same assertion said a way that
        // does, as elsewhere in this suite.
        let Err(refused) = provider()
            .stream(
                ChatRequest {
                    model: REFUSED.to_owned(),
                    ..ask()
                },
                CancellationToken::new(),
            )
            .await
        else {
            panic!("a model this backend will not serve is not a turn to take");
        };
        let said = refused.to_string();

        assert!(
            said.contains(REFUSED),
            "it has to name what was asked for: {said}"
        );
        for served in ALLOWED_MODELS {
            assert!(
                said.contains(served),
                "the served set is the part that is actionable: {said}"
            );
        }
        assert!(
            said.contains(openai::API_KEY_ENV),
            "the other way out is a key, which reaches models a seat cannot: {said}"
        );
        // Refused before the credential is read at all: this provider has no
        // store behind it and would have panicked renewing one.
        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "a request the provider declines to make, the way a bad base URL \
             is one: {refused:?}"
        );
    }

    #[test]
    fn a_refused_credential_says_which_login_repairs_it() {
        for status in [401, 403] {
            let named = reauth(
                Backend::Codex,
                ProviderError::Status {
                    status,
                    message: "invalid token".to_owned(),
                },
            );

            assert!(matches!(named, ProviderError::Auth(_)), "{named:?}");
            assert!(
                format!("{named}").contains("ganja auth login openai"),
                "the message is what a status bar shows: {named}"
            );
            assert!(
                !named.is_retryable(),
                "retrying a refused token is a storm against an identity provider"
            );

            // The same status against the platform is an API key being
            // refused, and `ganja auth login` does not mint one: sending
            // somebody through a browser flow would store a credential their
            // session cannot even reach while the key is exported.
            let keyed = reauth(
                Backend::Platform,
                ProviderError::Status {
                    status,
                    message: "invalid token".to_owned(),
                },
            );
            assert!(
                matches!(keyed, ProviderError::Status { .. }),
                "the endpoint's own message is the honest one here: {keyed:?}"
            );
        }

        // Everything else is left as it was: a rate limit is not a login.
        let limited = reauth(
            Backend::Codex,
            ProviderError::Status {
                status: 429,
                message: "slow down".to_owned(),
            },
        );
        assert!(
            matches!(limited, ProviderError::Status { status: 429, .. }) && limited.is_retryable(),
            "{limited:?}"
        );
    }

    #[test]
    fn the_system_prompt_travels_as_instructions_and_the_turn_as_items() {
        let mut empty = Message::assistant("gpt");
        empty.parts.push(Part::text(""));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![Message::user("hello"), empty, Message::user("again")],
            tools: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(Body::new(&request, Backend::Platform))
                .expect("the body serializes"),
            json!({
                "model": "gpt-test",
                "stream": true,
                // The backend answers a body without this
                // `400 {"detail":"Store must be set to false"}`.
                "store": false,
                "instructions": "be brief",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                    {"role": "user", "content": [{"type": "input_text", "text": "again"}]},
                ],
            })
        );
    }

    /// The live field failure the alias exists for: a plugin-contributed MCP
    /// server arrives namespaced `plugin:<name>:<server>` (**D473**), so its
    /// tools are named like this — 69 characters, with colons besides, which
    /// `meta/muse-spark-1.2` over openrouter refused as
    /// ``\`name\` must be at most 64 characters, got 69``.
    const REFUSED_NAME: &str =
        "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

    /// [`a_tool`] under the name that got a live turn killed.
    fn a_refused_tool() -> ToolDefinition {
        ToolDefinition {
            name: REFUSED_NAME.to_owned(),
            ..a_tool()
        }
    }

    /// Whether `name` is one this API's `^[a-zA-Z0-9_-]{1,64}$` accepts.
    fn conforms(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= OPENAI_CAP
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }

    #[test]
    fn a_tool_name_this_api_refuses_is_advertised_under_a_conforming_alias() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("research it")],
            tools: vec![a_refused_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");
        let advertised = body["tools"][0]["name"]
            .as_str()
            .expect("the tool is advertised");

        assert_ne!(
            advertised, REFUSED_NAME,
            "the refused name must not go out again"
        );
        assert!(conforms(advertised), "{advertised} is still refusable");
    }

    /// The other half of the same seam. What the engine executes, what the
    /// permission rules match and what the transcript records is the registry
    /// name, so an alias the model calls back has to be undone before the
    /// event leaves the wire.
    #[test]
    fn a_call_answering_with_the_alias_comes_back_out_under_the_registry_name() {
        let tools = vec![a_refused_tool()];
        let advertised = alias(REFUSED_NAME, OPENAI_CAP).into_owned();
        let mut mapping =
            Mapping::for_backend(Backend::OpenRouter, Aliases::of(&tools, OPENAI_CAP));
        let mut seen = Vec::new();

        mapping.frame(
            &Frame {
                event: None,
                data: json!({
                    "type": "response.output_item.added",
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": advertised,
                    },
                })
                .to_string(),
            },
            &mut seen,
        );

        assert_eq!(
            seen,
            vec![ProviderEvent::ToolCallStart {
                id: "call_1".to_owned(),
                name: REFUSED_NAME.to_owned(),
            }],
            "got {seen:?}"
        );
    }

    /// A call replayed on a later request has to name what that request's own
    /// roster named, or the model is handed a trace citing a tool it was never
    /// offered. Aliasing is deterministic, so both sides recompute it rather
    /// than remembering anything across turns.
    #[test]
    fn a_completed_call_replays_under_the_same_alias_the_roster_advertises() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(tool_part(
            "call_1",
            REFUSED_NAME,
            completed(json!({"filePath": "src/main.rs"}), "a report"),
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("research it"), assistant],
            tools: vec![a_refused_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");
        let advertised = &body["tools"][0]["name"];
        let called = body["input"]
            .as_array()
            .expect("input is a list")
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("the replayed call is there");

        assert!(
            conforms(advertised.as_str().expect("a name")),
            "got {advertised}"
        );
        assert_eq!(
            called["name"], *advertised,
            "the replayed call has to name exactly what the roster named: {body}"
        );
    }

    #[test]
    fn a_request_advertises_the_tools_it_was_given() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs")],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([{
                // Flatter than chat completions', which nests these under
                // `function` — a tool advertised in the sibling's shape is one
                // this API ignores, so the model would be offered nothing.
                "type": "function",
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            }]),
            "got {body}"
        );
    }

    /// Every tool-calling shape OpenRouter's reference publishes, held against
    /// what this encoder sends (`api_reference/responses/tool-calling`, read
    /// 2026-08-14).
    ///
    /// Two fields in that reference are worth naming for what ganja does *not*
    /// do with them:
    ///
    /// - **`strict: null`** rides every tool definition it prints. `null` is
    ///   that field's absent value — the reference never sets it to a boolean
    ///   and documents no behavior for one — and ganja's schemas are generated
    ///   from the argument structs rather than written to the strict subset, so
    ///   a `true` would be a promise the roster cannot keep and a `null` is the
    ///   request that is already being sent. Omitted, deliberately.
    /// - **`tool_choice: "auto"`** rides every one of its tool examples, and
    ///   *that* one ganja now sends — on this backend only, because the value
    ///   the API assumes in its absence is the one thing the reference does not
    ///   say, and the failure it would cause is a roster nothing ever calls.
    #[test]
    fn an_openrouter_tool_roster_is_the_shape_that_vendors_reference_documents() {
        let request = ChatRequest {
            tools: vec![a_tool()],
            ..gateway_ask()
        };
        let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            }]),
            "the reference's own flat shape, minus the `strict: null` it prints: {body}"
        );
        assert!(
            body["tools"][0].get("strict").is_none(),
            "a null-valued field is an absent field, and a true one is a promise \
             a generated schema cannot keep: {body}"
        );
        assert_eq!(
            body["tool_choice"],
            json!("auto"),
            "the value every tool example in that reference sends: {body}"
        );

        // A turn with nothing to offer sends neither key: `tool_choice` beside
        // an absent roster is a choice about nothing.
        let bare = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
            .expect("the body serializes");
        assert!(
            bare.get("tools").is_none() && bare.get("tool_choice").is_none(),
            "got {bare}"
        );

        // And the two OpenAI backends are untouched: their request is the Codex
        // CLI's, which sends no `tool_choice` and has been served without one on
        // every turn this build has taken.
        for backend in [Backend::Codex, Backend::Platform] {
            let owned = serde_json::to_value(Body::new(
                &ChatRequest {
                    tools: vec![a_tool()],
                    ..ask()
                },
                backend,
            ))
            .expect("the body serializes");
            assert!(
                owned.get("tool_choice").is_none(),
                "{backend:?} gained a field its vendor never asked for: {owned}"
            );
        }
    }

    /// The gateway's own tools ride the same array, after the ones this build
    /// runs (**D489**).
    ///
    /// Two shapes in one list is the whole of what the reference's combined
    /// example shows, and the absences are the opt-in: a provider nobody
    /// configured sends none of these, and a request that carries them still
    /// carries every function tool it had.
    #[test]
    fn a_configured_gateway_asks_for_its_own_tools_beside_this_builds() {
        let request = ChatRequest {
            tools: vec![a_tool()],
            ..gateway_ask()
        };

        let asked = Body::new(&request, Backend::OpenRouter)
            .serving(&["web_search".to_owned(), "datetime".to_owned()]);
        let body = serde_json::to_value(asked).expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([
                {
                    "type": "function",
                    "name": "read",
                    "description": "Reads a file from disk.",
                    "parameters": {
                        "type": "object",
                        "properties": {"filePath": {"type": "string"}},
                        "required": ["filePath"],
                    },
                },
                {"type": "openrouter:web_search"},
                {"type": "openrouter:datetime"},
            ]),
            "the reference's own row shape, and this build's tools still first: {body}"
        );

        // Nothing configured, nothing asked for — on a request that is
        // otherwise identical, so what this proves is the config and not the
        // fixture.
        let unasked = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");
        assert_eq!(
            unasked["tools"].as_array().map(Vec::len),
            Some(1),
            "server tools bill per call, so a session that named none sends \
             none: {unasked}"
        );

        // And a session with no tools of its own still gets the gateway's.
        let alone = serde_json::to_value(
            Body::new(&gateway_ask(), Backend::OpenRouter).serving(&["shell".to_owned()]),
        )
        .expect("the body serializes");
        assert_eq!(alone["tools"], json!([{"type": "openrouter:shell"}]));
    }

    /// The other end of the same key: what a provider does with a roster is
    /// decided by which backend it was built for, because these names are one
    /// vendor's namespace.
    #[test]
    fn only_the_gateway_that_serves_server_tools_is_given_any() {
        let routed = routed().serving(vec!["web_search".to_owned()]);
        let body = serde_json::to_value(
            Body::new(&gateway_ask(), Backend::OpenRouter).serving(&["web_search".to_owned()]),
        )
        .expect("the body serializes");
        assert_eq!(body["tools"], json!([{"type": "openrouter:web_search"}]));
        // The provider kept them, which is what `request` will splice.
        assert_eq!(routed.server_tools, ["web_search"]);

        let elsewhere = keyed().serving(vec!["web_search".to_owned()]);
        assert!(
            elsewhere.server_tools.is_empty(),
            "one vendor's tool namespace must not reach another vendor's request"
        );
    }

    /// The roster is the reference's, and the config key is validated against
    /// it — so the two halves of the opt-in cannot disagree.
    #[test]
    fn the_server_tool_roster_is_the_one_that_vendor_publishes() {
        assert_eq!(
            openrouter::SERVER_TOOLS,
            [
                "web_search",
                "datetime",
                "image_generation",
                "web_fetch",
                "apply_patch",
                "shell",
                "fusion",
                "advisor",
                "subagent",
                "experimental__search_models",
            ],
            "the roster table of `docs/guides/features/server-tools`, read 2026-08-14"
        );
        assert!(openrouter::serves_server_tool("web_search"));
        assert!(
            !openrouter::serves_server_tool("openrouter:web_search"),
            "a config names the half after the colon; the prefix is the wire's"
        );
    }

    /// The round trip the reference's multi-turn example shows: a
    /// `function_call` item and a `function_call_output` that quotes its
    /// `call_id`.
    ///
    /// The reference's own note is what this pins — "Only `type`, `call_id`, and
    /// `output` are required — `call_id` is what pairs the output with its
    /// originating function_call" — so the optional `id` is absent here by
    /// agreement rather than by omission.
    #[test]
    fn a_gateway_turn_replays_a_call_and_its_output_in_the_documented_pair() {
        let mut assistant = Message::assistant(GATEWAY_MODEL);
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(tool_part(
            "call_xyz789",
            "read",
            completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
        ));

        let request = ChatRequest {
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: vec![a_tool()],
            ..gateway_ask()
        };
        let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
            .expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
                {
                    "type": "function_call",
                    "call_id": "call_xyz789",
                    "name": "read",
                    "arguments": r#"{"filePath":"src/main.rs"}"#,
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_xyz789",
                    "output": "fn main() {}",
                },
            ]),
            "got {body}"
        );
    }

    /// The streaming sequence that reference prints, frame for frame — including
    /// the `response.function_call_arguments.done` its own example watches for
    /// the finished arguments.
    ///
    /// **Ganja terminates a call on `response.output_item.done`** and treats
    /// that `.done` frame as the announcement it is: the arguments were already
    /// accumulated from the deltas, and reading them again there would send the
    /// model's JSON twice. The two frames arriving together must therefore
    /// produce exactly one of each event, which is what this holds. If a live
    /// turn ever shows this gateway ending a call *without* an
    /// `output_item.done`, the fix is one arm here — recorded in
    /// [`super::openrouter`]'s ledger rather than guessed at now.
    #[tokio::test]
    async fn the_references_own_streaming_tool_sequence_opens_fills_and_closes_once() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_abc123","call_id":"call_xyz789","#,
            r#""name":"read","arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_abc123","#,
            r#""delta":"{\"filePath\":\"src/main.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.done","item_id":"fc_abc123","#,
            r#""arguments":"{\"filePath\":\"src/main.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_abc123","call_id":"call_xyz789","#,
            r#""name":"read","arguments":"{\"filePath\":\"src/main.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":45,"output_tokens":25}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(event, ProviderEvent::Usage(_)))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "call_xyz789".to_owned(),
                    name: "read".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_xyz789".to_owned(),
                    json: r#"{"filePath":"src/main.rs"}"#.to_owned(),
                },
                &ProviderEvent::ToolCallEnd {
                    id: "call_xyz789".to_owned(),
                },
                &ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// A tool the gateway ran arrives as a row to render and **nothing to run**
    /// (**D489**).
    ///
    /// The three absences are the whole rule, and each of them is a way the
    /// turn would break: a `ToolCallStart` would send the loop looking for a
    /// tool no registry has, the dialog that call would raise would ask about
    /// work already done, and a `Failed` would end a turn the gateway
    /// completed.
    #[tokio::test]
    async fn a_gateway_run_tool_becomes_a_row_and_never_a_call_to_execute() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"openrouter:web_search","id":"or_1","status":"in_progress"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
            r#""type":"openrouter:web_search","id":"or_1","status":"completed","#,
            r#""arguments":"{\"query\":\"rust 2024 edition\"}","#,
            r#""output":"3 results"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"It ships."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        assert!(
            seen.contains(&ProviderEvent::ServerTool {
                tool: "openrouter:web_search".to_owned(),
                input: json!({"query": "rust 2024 edition"}),
                output: "3 results".to_owned(),
            }),
            "the row has to carry the call and its answer: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| matches!(
                event,
                ProviderEvent::ToolCallStart { .. }
                    | ProviderEvent::ToolCallDelta { .. }
                    | ProviderEvent::ToolCallEnd { .. }
                    | ProviderEvent::Failed(_)
            )),
            "nothing here is this build's to run, ask about, or die over: {seen:?}"
        );
        assert_eq!(
            text(&seen),
            "It ships.",
            "and the reply the gateway's own tool fed goes on arriving"
        );
        // Once: the opening frame announces structure, exactly as a reasoning
        // item's does, and a row minted there would be a claim about nothing.
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, ProviderEvent::ServerTool { .. }))
                .count(),
            1,
            "{seen:?}"
        );
    }

    /// What the decode reads when the item is not shaped like the one tool the
    /// reference spells out — which is nine of the ten, since the vendor
    /// documents a different argument shape per tool.
    #[tokio::test]
    async fn a_gateway_tools_own_fields_are_shown_rather_than_guessed_at() {
        // `openrouter:shell`'s arguments mirror OpenAI's `shell_call.action`,
        // so they arrive under no `arguments` key at all.
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.done","item":{"#,
            r#""type":"openrouter:shell","id":"or_2","status":"completed","#,
            r#""action":{"commands":["ls -la"]},"#,
            r#""output":[{"stdout":"total 0","stderr":"","outcome":{"type":"exit","exit_code":0}}]}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        assert!(
            seen.contains(&ProviderEvent::ServerTool {
                tool: "openrouter:shell".to_owned(),
                // The item minus its envelope, which is what arrived rather
                // than a shape assumed for it — and `output` is not in it,
                // because the row shows that separately.
                input: json!({"action": {"commands": ["ls -la"]}}),
                output:
                    r#"[{"outcome":{"exit_code":0,"type":"exit"},"stderr":"","stdout":"total 0"}]"#
                        .to_owned(),
            }),
            "got {seen:?}"
        );

        // An item this build has no idea about still ends nothing: the
        // skip-with-debug arm is the safety net a beta surface needs.
        let unknown = gateway_events(concat!(
            r#"data: {"type":"response.output_item.done","item":{"#,
            r#""type":"some_future_call","id":"x_1","payload":{"a":1}}}"#,
            "\n\n",
            r#"data: {"type":"response.some.future.event","item_id":"x_1"}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;
        assert_eq!(
            unknown,
            vec![
                ProviderEvent::Usage(Usage::default()),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "an unrecognized item is skipped, and the turn still completes: {unknown:?}"
        );
    }

    /// Parallel calls, which that reference has a section of its own for: two
    /// items open in one response, their argument fragments interleave, and each
    /// is answered under its own `call_id`.
    #[tokio::test]
    async fn parallel_calls_in_one_response_keep_their_own_identities() {
        let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_2","call_id":"call_glob","name":"glob","arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"pattern\":"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"filePath\":\"a.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"\"**/*.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","arguments":"{}"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_2","call_id":"call_glob","name":"glob","arguments":"{}"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

        let arguments = |call: &str| {
            seen.iter()
                .filter_map(|event| match event {
                    ProviderEvent::ToolCallDelta { id, json } if id == call => Some(json.as_str()),
                    _ => None,
                })
                .collect::<String>()
        };

        assert_eq!(arguments("call_read"), r#"{"filePath":"a.rs"}"#);
        assert_eq!(
            arguments("call_glob"),
            r#"{"pattern":"**/*.rs"}"#,
            "a fragment keyed by the *item* id has to reach the call that item \
             opened, or two concurrent calls trade arguments: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, ProviderEvent::ToolCallEnd { .. }))
                .count(),
            2,
            "each call ends once and on its own frame: {seen:?}"
        );
    }

    /// A request offering `read`, which is what a session with a registry sends
    /// on every turn.
    fn a_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".to_owned(),
            description: "Reads a file from disk.".to_owned(),
            schema: json!({
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            }),
        }
    }

    /// A tool part carrying `state`, as an assistant message holds one.
    fn tool_part(call_id: &str, tool: &str, state: ToolState) -> Part {
        Part {
            id: PartId::ascending(),
            body: PartBody::Tool {
                call_id: call_id.to_owned(),
                tool: tool.to_owned(),
                state,
            },
        }
    }

    /// A call that ran and what it produced.
    fn completed(input: serde_json::Value, output: &str) -> ToolState {
        ToolState::Completed {
            input,
            output: output.to_owned(),
            title: "src/main.rs".to_owned(),
            metadata: json!({}),
            started: 1,
            completed: 2,
        }
    }

    /// A turn that called tools reads back as items: what the model said, the
    /// calls beside it, then every result — each its own entry, because this
    /// API has no message that holds a call.
    #[test]
    fn a_finished_call_is_sent_back_as_a_call_item_and_an_output_item() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part::text("Reading the file first."));
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
        ));
        assistant.parts.push(tool_part(
            "call_glob",
            "glob",
            ToolState::Error {
                input: json!({"pattern": "**/*.rs"}),
                error: "no such directory".to_owned(),
                started: 3,
                completed: 4,
            },
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![
                Message::user("read src/main.rs"),
                assistant,
                Message::user("thanks"),
            ],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
                {
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Reading the file first."}],
                },
                {
                    "type": "function_call",
                    "call_id": "call_read",
                    "name": "read",
                    // Arguments travel as a string here too: the model streams
                    // them as text and the API carries them as it got them.
                    "arguments": r#"{"filePath":"src/main.rs"}"#,
                },
                {
                    "type": "function_call",
                    "call_id": "call_glob",
                    "name": "glob",
                    "arguments": r#"{"pattern":"**/*.rs"}"#,
                },
                {"type": "function_call_output", "call_id": "call_read", "output": "fn main() {}"},
                // A failure has nowhere to be flagged here either, so it
                // travels as the text the model reads.
                {
                    "type": "function_call_output",
                    "call_id": "call_glob",
                    "output": "no such directory",
                },
                {"role": "user", "content": [{"type": "input_text", "text": "thanks"}]},
            ]),
            "got {body}"
        );
    }

    /// A turn that took two model requests reads back as two of them. The API
    /// would accept one flattened run, but it would put everything the model
    /// said ahead of every result, so a model re-reading its own trace would
    /// find its reasoning shuffled out from under the evidence it reasoned
    /// from.
    #[test]
    fn a_two_step_turn_is_sent_back_one_group_per_step() {
        let mut assistant = Message::assistant("gpt-test");

        for (text, call_id, tool, input, output) in [
            (
                "Reading.",
                "call_read",
                "read",
                json!({"filePath": "src/main.rs"}),
                "fn main() { let x = 1; }",
            ),
            (
                "Now editing.",
                "call_edit",
                "edit",
                json!({"filePath": "src/main.rs", "oldString": "1", "newString": "2"}),
                "1 replacement",
            ),
        ] {
            assistant.parts.push(Part {
                id: PartId::ascending(),
                body: PartBody::StepStart,
            });
            assistant.parts.push(Part::text(text));
            assistant
                .parts
                .push(tool_part(call_id, tool, completed(input, output)));
        }

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("fix the bug"), assistant],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");
        let wire = body["input"].to_string();
        let position = |needle: &str| wire.find(needle).expect("the wire holds it");

        assert!(
            position("Now editing.") > position("fn main() { let x = 1; }"),
            "what the model said in the second step must read as having been \
             said after the first step's result came back: {wire}"
        );
    }

    /// A turn cancelled while a tool was running leaves a call nobody answered,
    /// and this API pairs a call with its output by `call_id`: a call with no
    /// output is one the model is still waiting on. Dropping the call instead
    /// would leave the reply talking about one that is not there.
    #[test]
    fn a_call_that_never_finished_is_answered_rather_than_left_dangling() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            ToolState::Pending { input: None },
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request, Backend::Platform))
            .expect("the body serializes");

        assert_eq!(
            body["input"][2],
            json!({
                "type": "function_call_output",
                "call_id": "call_read",
                // One spelling across both wires: the sibling's, imported.
                "output": NO_RESULT,
            }),
            "an unanswered call must not reach the API unanswered: {body}"
        );
    }

    /// The happy path: text, a summarized thought, and the bill the terminal
    /// frame carries.
    #[tokio::test]
    async fn a_happy_path_transcript_maps_to_text_reasoning_and_a_bill() {
        let seen = events(concat!(
            "event: response.created\n",
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6"}}"#,
            "\n\n",
            "event: response.output_item.added\n",
            r#"data: {"type":"response.output_item.added","output_index":0,"#,
            r#""item":{"type":"reasoning","id":"rs_1"}}"#,
            "\n\n",
            "event: response.reasoning_summary_text.delta\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","#,
            r#""summary_index":0,"delta":"A greeting is enough."}"#,
            "\n\n",
            "event: response.output_item.added\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"#,
            r#""item":{"type":"message","id":"msg_1"}}"#,
            "\n\n",
            "event: response.output_text.delta\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello, "}"#,
            "\n\n",
            "event: response.output_text.delta\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"world!"}"#,
            "\n\n",
            "event: response.completed\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":42,"input_tokens_details":{"cached_tokens":16},"#,
            r#""output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Hello, world!");
        assert!(
            seen.contains(&ProviderEvent::ReasoningDelta(
                "A greeting is enough.".to_owned()
            )),
            "a summarized thought should not be dropped, got {seen:?}"
        );
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    // 42 prompt tokens of which the cache served 16: 26 fresh.
                    input_tokens: 26,
                    output_tokens: 9,
                    reasoning_tokens: 4,
                    cache_read_tokens: 16,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// This API reports the whole prompt as `input_tokens` and then says how
    /// much of it the cache served and wrote; [`Usage`] keeps the three apart so
    /// each can be billed at its own rate, a cache read costing a fraction of
    /// fresh input. Handing every count through unchanged bills the cached part
    /// twice.
    #[tokio::test]
    async fn a_cached_prompt_reports_only_its_fresh_half_as_input() {
        let cases = [
            (
                "a cached prompt bills only what the cache did not serve",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"#,
                    r#""output_tokens":20}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 200,
                    output_tokens: 20,
                    cache_read_tokens: 800,
                    ..Usage::default()
                },
            ),
            (
                "a written cache entry is not fresh input either",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":600,"#,
                    r#""cache_write_tokens":300},"output_tokens":20}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 600,
                    cache_write_tokens: 300,
                    ..Usage::default()
                },
            ),
            (
                "an endpoint claiming more cached tokens than prompt tokens reads as \
                 nothing fresh rather than wrapping into a bill nobody owes",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":100,"input_tokens_details":{"cached_tokens":900},"#,
                    r#""output_tokens":5}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 0,
                    output_tokens: 5,
                    cache_read_tokens: 900,
                    ..Usage::default()
                },
            ),
            (
                "a prompt nothing was cached for is fresh in full, thinking included \
                 in the output rather than counted beside it",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"output_tokens":120,"#,
                    r#""output_tokens_details":{"reasoning_tokens":100}}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 1_000,
                    output_tokens: 120,
                    reasoning_tokens: 100,
                    ..Usage::default()
                },
            ),
        ];

        for (name, transcript, expected) in cases {
            let seen = events(transcript).await;

            assert!(
                seen.contains(&ProviderEvent::Usage(expected)),
                "{name}: got {seen:?}"
            );
        }
    }

    /// The bill the corrected counts actually produce. Priced apart, a heavily
    /// cached prompt costs a fraction of what the same tokens would fresh —
    /// which is exactly the difference double-counting erases.
    #[test]
    fn a_cached_prompt_is_billed_once_rather_than_twice() {
        let model = catalog::model("gpt-5.6").expect("the catalog knows the model");
        let corrected = Usage {
            input_tokens: 200_000,
            cache_read_tokens: 800_000,
            ..Usage::default()
        };
        let doubled = Usage {
            input_tokens: 1_000_000,
            ..corrected
        };

        assert!(
            catalog::cost(&doubled, &model).total_usd
                > catalog::cost(&corrected, &model).total_usd * 3.0,
            "the uncorrected counts over-report by more than a factor of three, \
             which is the size of the error this pins"
        );
    }

    /// A call arrives across three event types, and the id that correlates them
    /// is not the id its result has to quote back.
    #[tokio::test]
    async fn tool_calls_are_opened_filled_and_closed() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
            r#""delta":"Reading the file first."}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
            r#""arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
            r#""output_index":1,"delta":"{\"file"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
            r#""output_index":1,"delta":"Path\":\"src/main.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
            r#""arguments":"{\"filePath\":\"src/main.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":10,"output_tokens":5}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Reading the file first.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(
                    event,
                    ProviderEvent::TextDelta(_) | ProviderEvent::Usage(_)
                ))
                .collect::<Vec<_>>(),
            vec![
                // The `call_id`, never the item id the deltas were keyed by:
                // this is the string a `function_call_output` has to quote.
                &ProviderEvent::ToolCallStart {
                    id: "call_read".to_owned(),
                    name: "read".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "{\"file".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "Path\":\"src/main.rs\"}".to_owned(),
                },
                // This API *does* terminate a call, unlike chat completions.
                &ProviderEvent::ToolCallEnd {
                    id: "call_read".to_owned(),
                },
                &ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// The SSE decoder must tolerate anything: this stream carries a dozen
    /// event types this build has no use for, and several more the API has not
    /// invented yet.
    #[tokio::test]
    async fn an_unmapped_event_is_skipped_rather_than_ending_the_turn() {
        let seen = events(concat!(
            r#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"web_search_call","id":"ws_1","status":"in_progress"}}"#,
            "\n\n",
            r#"data: {"type":"response.something.nobody.has.written.yet","delta":"x"}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"hi"}"#,
            "\n\n",
            "data: [DONE]\n\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":1,"output_tokens":1}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "hi");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed)),
            "an unknown event is a log line, and `[DONE]` is not a parse \
             failure: {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
            r#""delta":"The connection drops right"}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "The connection drops right");
        assert!(
            matches!(
                seen.last(),
                Some(ProviderEvent::Failed(ProviderError::Transport(_)))
            ),
            "a dropped connection must never read as a finished turn, got {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_malformed_chunk_ends_the_turn_rather_than_being_skipped() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"Hello"}"#,
            "\n\n",
            "data: {\"type\": not json at all\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":" there"}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Hello", "text before the break is kept");
        assert_eq!(
            seen.len(),
            2,
            "nothing after the broken chunk is read, got {seen:?}"
        );
        assert!(
            matches!(
                seen.last(),
                Some(ProviderEvent::Failed(ProviderError::Parse(_)))
            ),
            "got {seen:?}"
        );
    }

    /// This API has two ways of saying a turn broke after the status was
    /// already 200, and neither may read as a model that finished talking.
    #[tokio::test]
    async fn a_failed_response_and_an_error_chunk_both_end_the_turn_as_failures() {
        for transcript in [
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
                "\n\n",
                r#"data: {"type":"response.failed","sequence_number":9,"response":{"#,
                r#""error":{"code":"server_error","message":"upstream capacity exceeded"}}}"#,
                "\n\n",
            ),
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
                "\n\n",
                r#"data: {"type":"error","sequence_number":9,"code":"server_error","#,
                r#""message":"upstream capacity exceeded"}"#,
                "\n\n",
            ),
        ] {
            let seen = events(transcript).await;

            assert_eq!(text(&seen), "partial");
            assert_eq!(
                seen.last(),
                Some(&ProviderEvent::Failed(ProviderError::Status {
                    status: 500,
                    message: "upstream capacity exceeded".to_owned(),
                })),
                "got {seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(replay(
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"one"}"#,
                "\n\n",
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"two"}"#,
                "\n\n",
            ),
            cancel.clone(),
            Mapping::default(),
        ));

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::TextDelta("one".to_owned()))
        );
        cancel.cancel();

        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(rest.is_empty(), "a cancelled stream ends: {rest:?}");
    }

    #[tokio::test]
    async fn a_turn_that_stopped_early_still_reports_what_it_spent() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"as far as"}"#,
            "\n\n",
            r#"data: {"type":"response.incomplete","response":{"#,
            r#""incomplete_details":{"reason":"max_output_tokens"},"#,
            r#""usage":{"input_tokens":10,"output_tokens":128}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "as far as");
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 128,
                    ..Usage::default()
                }),
                // A reply that stopped at the output ceiling is still a reply,
                // and the loop has no verdict between "done" and "broke".
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }
}
