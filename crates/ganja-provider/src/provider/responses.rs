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
//! | extra headers | `ACCOUNT_HEADER`, `ORIGINATOR_HEADER`, `BETA_HEADER`, `CODEX_USER_AGENT` | none |
//! | model gate | `serves` | whatever the platform serves |
//! | default model | [`SUBSCRIPTION_DEFAULT`] | the catalog's |
//!
//! The branch those rows describe is upstream's, and all of them come off it
//! together: `codex.ts:356` returns the *unwrapped* `fetch` for a credential
//! that is not OAuth, so a key request keeps the URL the SDK built
//! (`api.openai.com/v1/responses`) and gains none of the four headers the
//! subscription branch adds; `codex.ts:281` returns the models unfiltered for
//! the same condition, so the allow-list is a property of the seat and not of
//! the API.
//!
//! What those four *say* is not all upstream's: `BETA_HEADER` is the Codex
//! CLI's own rather than the pin's, and since W3 the originator and the
//! User-Agent are ganja's own name. Each is documented at its constant.
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
/// Two are one vendor's; the rest are other people's endpoints serving the
/// same dialect, which is why this enum answers [`Self::provider_id`] as
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
    /// An endpoint a config named ([`super::compat`]'s `openai-responses`
    /// dialect), reached with whatever credential the entry's `key_env` holds.
    ///
    /// The one backend whose vendor this build has never met at all, so every
    /// predicate below gives it [`super::openrouter`]'s refuse-to-guess
    /// answer: nothing sealed is asked for, nothing is replayed, and no
    /// default is written into somebody else's `reasoning` object.
    Compat,
}

impl Backend {
    /// Where this backend lives when [`BASE_URL_ENV`](openai::BASE_URL_ENV)
    /// names nothing.
    ///
    /// One host per backend because the credential decides which will take
    /// it: a ChatGPT token is refused by the platform, a key is refused by the
    /// codex backend, and neither vendor's credential is the other's.
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
            // Never read, for the gateway arm's reason: a config entry is
            // refused at load without a `base_url`, and `CompatProvider`
            // hands it over explicitly. The platform's base answers only so
            // the function stays total.
            Self::Compat => openai::DEFAULT_BASE_URL,
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
            // The vendor whose mapping the dialect borrows — and never the
            // answer a session sees: a config-named endpoint is wrapped by
            // `CompatProvider`, whose `id` shadows this one with the name the
            // entry was written under, exactly as the other two dialects'
            // wires are shadowed. The replay guard never reads this arm,
            // because [`Self::replays_reasoning`] already said no.
            Self::Compat => ID,
        }
    }

    /// Whether this backend documents the sealed-reasoning pairing
    /// ([`Body::include`] out, a `reasoning` input item back).
    ///
    /// OpenAI's two do. Neither gateway does, and nothing here guesses on
    /// their behalf — the whole reasoning is in [`super::openrouter`]'s module
    /// doc, and [`super::opencode`] inherits it for the same reason: a vendor
    /// that documents no way to hand sealed state back is not one to hand it
    /// back to. A config-named endpoint ([`Self::Compat`]) is the strongest
    /// case of the same rule — a vendor this build has never met — and it has
    /// a mechanical half too: the session records reasoning under the
    /// *wrapper's* id, the name the config entry was written under, so state
    /// asked for here would be state the replay guard below could never match.
    /// One predicate rather than four sites, because asking for
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
/// **ganja's own name**, for the reason [`auth::openai`]'s own originator is:
/// the field is not checked against the client registration the access token
/// was minted under. OpenAI's own Codex CLI sends `codex_cli_rs` where
/// upstream opencode sends `opencode` on that same registration, so what this
/// decides is which feature cohort the backend serves — not whether it answers
/// at all.
const ORIGINATOR_HEADER: &str = "originator";

/// The value [`ORIGINATOR_HEADER`] carries.
const ORIGINATOR: &str = "ganja-code";

/// What the codex backend is told this build is.
///
/// [`auth::device::GANJA_USER_AGENT`]'s bytes, named for
/// `chatgpt.com/backend-api/codex` rather than reached for directly, because
/// this header and [`ORIGINATOR`] beside it decide which feature cohort the
/// backend serves — and a request naming itself one thing in the header and
/// another in the query is the one shape that cannot be the intended answer.
///
/// Moved in W3 of `.omc/plans/2026-08-25-ganja-code-identity-headers.md`, with
/// that originator, and only after a live probe had recorded the model roster
/// this seat is served: the exposure here is cohort placement rather than
/// refusal, which is measurable and was therefore measured rather than argued.
/// That recording is
/// `crates/ganja-core/tests/fixtures/codex-identity-probe.txt`, overwritten on
/// each probe run, so the recording made under the borrowed name — the
/// baseline this rename is diffed against — is commit 5d5a52a's copy of it.
pub(crate) const CODEX_USER_AGENT: &str = auth::device::GANJA_USER_AGENT;

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
/// snapshot of somebody else's product decision as of v1.18.22 and **will
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
/// Unlike [`ALLOWED_MODELS`] this is **not** per-seat: it is a fact about the
/// model rather than about the seat, so it holds for a key as well. It is a
/// fact about the *vendor's* model, though, so it is not applied on
/// [`Backend::Compat`] — what a config-named endpoint serves under any name
/// is its own to answer for ([`ResponsesProvider::refuses`]).
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
    /// Headers a config entry declared, sent with every request
    /// ([`Self::with_headers`]). Empty everywhere but [`Backend::Compat`],
    /// and left out of the [`fmt::Debug`] rendering for the sibling wires'
    /// reason: a header value is somewhere a token fits.
    headers: reqwest::header::HeaderMap,
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
            headers: reqwest::header::HeaderMap::new(),
        })
    }

    /// Puts `headers` on every request this provider sends — the config
    /// entry's own, reaching this wire the way they reach the other two.
    ///
    /// Crate-internal for
    /// [`with_credential`](super::openai::OpenAiProvider::with_credential)'s
    /// reason: what a caller outside this module picks between is providers.
    #[must_use]
    pub(super) fn with_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.headers = headers;
        self
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
    /// alias is refused wherever the vendor's own roster is served, because
    /// there it is a fact about the model: the vendor speaks Responses, and
    /// that alias does not (`plugin/provider/openai.ts:164-171`) — and it is
    /// **not** refused on [`Backend::Compat`], because a config-named endpoint
    /// is not the vendor, and what it serves under any name is its own to
    /// answer for; pre-refusing would be a guess, and the guess would come
    /// with another provider's advice attached. The seat's allow-list is
    /// refused on [`Backend::Codex`] alone, because it is a fact about the
    /// subscription: `codex.ts:281` hands back the unfiltered model list for
    /// any credential that is not an OAuth one, so the platform serves whatever
    /// it sells and a key session is never held to somebody's seat.
    pub(super) fn refuses(&self, model: &str) -> Option<ProviderError> {
        if self.backend != Backend::Compat && CHAT_COMPLETIONS_ONLY.contains(&model) {
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
    /// it refuses, the whole difference between one backend's request and
    /// another's, and where a config entry declared headers of its own, the
    /// only place they travel — is provable without a socket.
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
            .bearer_auth(resolved.presented.expose())
            // After the bearer, and never carrying one: these are the config
            // entry's own — empty on every backend a config did not build —
            // and a credential put here would travel outside the redaction
            // `presented` is the single source of.
            .headers(self.headers.clone());

        // Subscription-only, all four, and for one reason: each of them is
        // about talking to the codex backend, whose endpoint and client
        // registration are the Codex CLI's even though this build no longer
        // answers to its name
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
                .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT);

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
    /// The backend's, not the module's: two of the backends are this vendor
    /// and the rest are not — see `Backend::provider_id` for what rides on
    /// the answer.
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
        // be explained. `wire` and not `provider`, for the sibling's reason: a
        // config-named session answers to its entry's own name, which is not
        // knowable here, and the endpoint beside it is what tells those turns
        // apart.
        tracing::debug!(
            wire = ID,
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
#[path = "responses_tests.rs"]
mod tests;
