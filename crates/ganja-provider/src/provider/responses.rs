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
//! it follows the **provider id** the session selected (**D555**): `chatgpt`
//! builds the seat and `openai` the platform, and no credential is read to
//! choose between them.
//!
//! | | `Backend::Codex` | `Backend::Platform` |
//! |---|---|---|
//! | provider id | [`CHATGPT_ID`] | [`ID`] |
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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::{self, RefreshOauth};
use crate::protocol::{FinishReason, Part, PartBody, Role, ToolState, Usage};
use crate::provider::openai::{self, arguments, result};
use crate::provider::sse::Frame;
use crate::provider::toolname::{Aliases, OPENAI_CAP, alias};
use crate::provider::{
    Blob, ChatRequest, CredentialSource, Mapper, Provider, ProviderError, ProviderEvent, Resolved,
    ServedOptions, check_base_url, client, key_for, open, opencode, openrouter, setting,
    shown_base_url, splice, steps,
};
use crate::tool::ToolDefinition;

// Documented by its own module doc; an outer one here would be merged with it
// and resolve that doc's intra-doc links in *this* module's scope.
pub mod options;

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
///
/// The same one [`super::openai`] answers to, because it is the same vendor
/// serving the same models at the same prices — and now the same wire as well;
/// what [`super::openai`] still is, is the API that [`super::grok`] and
/// [`super::copilot`] ride.
pub const ID: &str = openai::ID;

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects the ChatGPT
/// seat — this module's other backend (**D555**).
///
/// One module, two ids, **one credential kind each**: [`ID`] is the platform
/// API on an [`API_KEY_ENV`](openai::API_KEY_ENV) key, this is a subscription
/// on a stored login, and selection builds one or the other by the id it was
/// given rather than by whichever credential a machine happens to hold. They
/// bill against different pools, which is why a turn has to report which of
/// them ran it — `Backend::provider_id`.
pub const CHATGPT_ID: &str = auth::openai::PROVIDER_ID;

/// What an [`ID`] session with no key is refused with, in place of
/// [`require_key`](super::require_key)'s one-door sentence.
///
/// Two doors because after **D555** this id reads one credential and there is a
/// second one it used to reach: a machine holding a subscription login and no
/// key would otherwise be told a variable is unset, which is true and is not the
/// repair. The variable is spelled out rather than interpolated so this stays a
/// constant; `responses_tests.rs` pins it against
/// [`API_KEY_ENV`](openai::API_KEY_ENV) and [`CHATGPT_ID`] so the two spellings
/// cannot drift.
const NO_PLATFORM_KEY: &str = "OPENAI_API_KEY is unset; export it for the platform API, or for a \
                               ChatGPT subscription run `ganja auth login chatgpt` and select \
                               `GANJA_PROVIDER=chatgpt`";

/// Where a ChatGPT subscription's requests go (`codex.ts:12`).
///
/// The path this provider appends is `/responses`, so the whole URL is
/// `codex.ts`'s `CODEX_API_ENDPOINT` exactly.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Which backend a provider was built against.
///
/// Not a runtime question: it follows the provider id, which
/// `ganja_core::provider::select` resolves once per session. Keeping it a field
/// rather than re-deriving it per request is what makes "a key request is never
/// filtered by the seat's allow-list" a fact about how the provider was
/// constructed instead of a condition somebody could forget to write.
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
    /// its sealed reasoning would be handed to the wrong wire. The vendor's own
    /// two answer differently for the same reason at a smaller scale (**D555**):
    /// a subscription turn and a platform turn draw on different pools, so the
    /// id an engine holds says which one is being spent.
    pub(super) const fn provider_id(self) -> &'static str {
        match self {
            Self::Codex => CHATGPT_ID,
            Self::Platform => ID,
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

    /// Whether a terminal frame's `service_tier`, `text.verbosity`,
    /// `parallel_tool_calls` and `reasoning.context` mean what **D563** reads
    /// them as: how *this vendor* served the options this request configured.
    ///
    /// The same two backends, and deliberately not the same predicate as
    /// [`Self::replays_reasoning`]: this one is about a field's meaning rather
    /// than a feature's pairing, and a gateway that starts echoing one of the
    /// four moves this answer without moving that one. Everything else here
    /// relays somebody else's response through its own normalization, so a
    /// field arriving under one of those names is that gateway's word for its
    /// own thing — reporting it as a served option would put a number in
    /// `/usage` that nothing this build sent can be compared against.
    const fn echoes_options(self) -> bool {
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

/// The models the **codex backend** serves a ChatGPT seat outright.
///
/// **Scope: [`Backend::Codex`] only.** This is a subscription's offering, not
/// the API's: a session holding a key sees whatever the platform sells, and
/// [`ResponsesProvider::refuses`] is where that scoping is spelled. A seat's
/// offering is somebody else's product decision and **will drift**;
/// [`NEWER_THAN`] is what keeps this from aging badly, and a probe against the
/// live backend is the only thing that can settle a drift.
///
/// **Three ids left on 2026-09-16** — `gpt-5.3-codex-spark`, `gpt-5.4` and
/// `gpt-5.4-mini` — because the seat-parameter probe
/// (`.omc/research/2026-09-16-chatgpt-seat-param-probe.md`, 120 live calls)
/// found the backend refusing them. Keeping a refused id here is worse than
/// dropping it: it makes the wire promise a turn the backend will not take, and
/// it makes [`unsupported`]'s sentence — which is this list, joined — name
/// models nobody can run.
///
/// The two that stay are not redundant even though [`generation`] reads both
/// numbers. `gpt-6-astra` carries no `N.M` after `gpt-`, so the rule below
/// answers [`None`] and `serves` would refuse the newest model the seat offers.
/// `gpt-5.5` clears [`NEWER_THAN`] on its own, and is spelled anyway because
/// this list is also the refusal sentence's roster and because it is
/// [`SUBSCRIPTION_DEFAULT`]: a default the rule alone admits is one line away
/// from a floor bump refusing it silently.
const ALLOWED_MODELS: [&str; 2] = ["gpt-5.5", "gpt-6-astra"];

/// The models a ChatGPT seat is **offered**, in the order to offer them
/// (**D476**, `seat-roster-pinned`).
///
/// A roster a listing derives from the catalog drifts with the catalog. This is
/// the owner's own pin instead — these ids, this order, decided once and
/// answered from the binary (`gpt-6-astra` joined on 2026-09-07, first because
/// it is the newest generation the seat offers).
///
/// **Offered is not servable, and the split is the whole point.** A session
/// that names a model explicitly still takes its turn if `serves` admits it;
/// what this narrows is only what a listing *volunteers*, which is why
/// [`SUBSCRIPTION_DEFAULT`] being second rather than first is not a
/// contradiction — what a seat defaults to and what it offers to browse are two
/// decisions.
///
/// **`gpt-5.3-codex-spark` left on 2026-09-16**, with the two ids the same
/// probe found refused (`ALLOWED_MODELS`'s own doc has the report): a roster
/// row the backend answers with 400 is an offer that cannot be taken.
///
/// **The catalog cannot move this list.** Membership is these lines;
/// `ganja models --refresh` re-reads sizing and pricing and never this. A
/// catalog row is consulted for one thing only, a human-readable name, and its
/// absence costs nothing — the id stands in.
///
/// Every id here has to satisfy `serves`: an offer this backend would then
/// refuse is a lie the listing tells, and the test in `responses_tests.rs`
/// is what keeps it honest — which is why `gpt-6-astra` is also in
/// `ALLOWED_MODELS`, the only route `serves` has to an id with no `N.M`.
pub const SEAT_ROSTER: [&str; 5] =
    ["gpt-6-astra", "gpt-5.5", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

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
/// The one this names has to satisfy `serves` — pinned below, because a default
/// this backend refuses is the bug this constant exists to prevent. It said
/// `gpt-5.4` until 2026-09-16, when the seat-parameter probe found the backend
/// answering that id with a 400: exactly the bug, arrived by drift rather than
/// by a typo, which is why the pin is over both this constant and
/// [`SEAT_ROSTER`].
pub const SUBSCRIPTION_DEFAULT: &str = "gpt-5.5";

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
        let end = text.find(|character: char| !character.is_ascii_digit()).unwrap_or(text.len());

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
    /// on why the store is consulted per request instead. What is fixed now is
    /// *which* entry those per-request reads and renewals will reach:
    /// [`CHATGPT_ID`], the seat's own key since **D555**, never [`ID`]'s.
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
    /// The order the two are read in is [`key_for`]'s and always has been:
    /// exported outranks stored. What changed with the vendor's move to this
    /// wire is only *which* provider a key builds — the lookup and the endpoint
    /// check are the sibling's, unchanged, so a session that used to die at
    /// startup still does.
    ///
    /// What the refusal *says* is this module's own since **D555**
    /// (`NO_PLATFORM_KEY`): a key is now the only credential this arm reads,
    /// so somebody holding a subscription login has to be told the id that
    /// spends it rather than left to conclude their login stopped working.
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

        // `key_for` rather than `require_key`: the same lookup in the same
        // order, over a sentence with one more clause than the one-door
        // message every other key wire is refused with.
        let key = key_for(ID)?.ok_or_else(|| ProviderError::Auth(NO_PLATFORM_KEY.to_owned()))?;

        Self::built(CredentialSource::Key(key), base_url, Backend::Platform)
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
            CredentialSource::Oauth { provider_id: CHATGPT_ID, refresh },
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

        let body = composed(request, self.backend, &self.server_tools);
        built.json(&body).build().map_err(|error| {
            ProviderError::Transport(
                resolved.presented.redact(&format!("malformed request: {error}")),
            )
        })
    }
}

/// The body one request sends: the typed [`Body`] with every layer under it
/// (**D563**).
///
/// Lowest first: the backend's free defaults ([`defaulted`]), the configured
/// body less what this request cannot carry ([`gated`]), the effort — a
/// session selection, so above config — and the directives written into
/// objects the layers below may already hold ([`directed`]). The wire's own
/// fields land over all four, so neither a catalog row nor a config table can
/// unmake `model` or `stream`.
///
/// One function rather than four lines at the send site, so the tests read
/// the body the way a request sends it rather than a reassembly of it.
fn composed<'a>(
    request: &'a ChatRequest,
    backend: Backend,
    server_tools: &[String],
) -> impl Serialize + 'a {
    struct Composed<'a> {
        own: Body<'a>,
        defaults: Map<String, Value>,
        configured: Map<String, Value>,
        effort: &'a Map<String, Value>,
        directives: Map<String, Value>,
    }

    impl Serialize for Composed<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            splice([&self.defaults, &self.configured, self.effort, &self.directives], &self.own)
                .serialize(serializer)
        }
    }

    let own = Body::new(request, backend);
    // Counted before the gateway's own tools join, because what the gateway
    // was sent before this layer existed is what it is still sent:
    // `tool_choice` beside a function roster, never beside a roster of its
    // own tools alone.
    let offered = own.tools.len();

    Composed {
        own: own.serving(server_tools),
        defaults: defaulted(backend, offered, &request.model),
        configured: gated(&request.responses.body, offered > 0, &request.model),
        effort: &request.effort_options,
        directives: directed(request, backend),
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
        matches!(mime, "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "application/pdf")
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

        let custom = CustomArguments::of(&request);
        let model = request.model.clone();
        let requested = request.responses.service_tier.clone();

        open(
            move || Mapping {
                custom: custom.clone(),
                model: model.clone(),
                requested: requested.clone(),
                ..Mapping::for_backend(backend, aliases.clone())
            },
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
        ProviderError::Status { status: status @ (401 | 403), message }
            if backend == Backend::Codex =>
        {
            ProviderError::Auth(format!(
                "the ChatGPT endpoint refused the stored credential (HTTP {status}): \
             {message}; run `ganja auth login {ID}`"
            ))
        }
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
    /// Nothing, where nobody asked for anything — a body with no `include` at
    /// all is the shape every request on this wire carried before there was
    /// anything to opt into (**D563**). Otherwise the ordered,
    /// deduplicated union of three askers: the sealed reasoning
    /// this wire replays, whatever the effort's own map asks for, and a
    /// configured `include`. A union rather than a layer, because this field
    /// is a list and the splice replaces lists: the body is the last layer,
    /// so the effort's entry would otherwise be dropped by the wire's own.
    ///
    /// **Decided by the model, not by the credential**, which is where
    /// upstream decides it: the option rides the model facade
    /// (`packages/llm/src/providers/openai-options.ts:44-58`, applied at
    /// `providers/openai.ts:43`), so both backends this provider reaches send
    /// it for a model that reasons and neither sends it for one that does not.
    /// See [`seals_reasoning`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'a str>,
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
}

/// One entry of a request's `tools` array.
///
/// Untagged, because the shapes are told apart by what they carry: a tool this
/// side will execute names itself — with a schema as a function, without one
/// as a custom tool — and a tool the *provider* will execute is a type and
/// whatever that type's own knobs are.
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
    /// A tool the model may call with one free-text `input` and **this
    /// build** runs (**D563**): a registry tool named in `custom_tools`,
    /// advertised in addition to its [`ToolSpec::Function`] entry, never
    /// instead of it.
    ///
    /// Only a tool whose argument schema has exactly one required property,
    /// and it a string, can be advertised this way: the call's `input` is
    /// mapped onto that one argument and onto nothing else, so optional
    /// arguments are unreachable through the custom advertisement. That is why
    /// the function twin stays on the roster beside it.
    Custom {
        #[serde(rename = "type")]
        kind: &'static str,
        /// The same name, under the same [`alias`], its function twin carries.
        name: Cow<'a, str>,
        description: &'a str,
    },
    /// A tool the model may call and **the provider** runs — a gateway's own
    /// (**D489**) or a hosted one a config named (**D563**).
    ///
    /// Sent verbatim, `type` first: the gateway's are the one field its
    /// reference publishes, `{"type": "openrouter:web_search"}`, and a hosted
    /// entry is whatever the config table carried, whose every key past `type`
    /// is the vendor's own knob and passes through untouched. No name, because
    /// the type *is* the name; no schema, because the vendor owns the tool.
    Server(Map<String, Value>),
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
    Said { role: &'static str, content: Vec<Block<'a>> },
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
    /// A call the model made through a **custom** tool advertisement
    /// (**D563**) — the free-text twin of [`Item::Called`].
    ///
    /// Its own shape because this API's custom item carries the model's words
    /// as `input`, a bare string, where a function call carries an arguments
    /// object encoded as one. Which of the two a stored call replays as is
    /// read off the call's own record ([`PartBody::Tool::custom`]) and never
    /// off this request's `custom_tools`: the configuration may have changed
    /// between the turn that made the call and the turn that replays it, and
    /// an item the backend is handed has to match the item it sent.
    CustomCalled {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        /// Under the same [`alias`] the custom advertisement carried, which is
        /// its function twin's.
        name: Cow<'a, str>,
        /// The one argument's value, unwrapped from the arguments object the
        /// inbound mapper wrapped it in — the model's own words, which is
        /// what it was sent as. Borrowed from the stored call, because
        /// [`replayed_input`] admits this item only when those words are
        /// there to borrow.
        input: &'a str,
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
                    // How the call was advertised decides how it replays, and
                    // that is the part's own record rather than this
                    // request's options (**D563**).
                    input.push(match replayed_input(part) {
                        Some(words) => Item::CustomCalled {
                            kind: CUSTOM_TOOL_CALL,
                            call_id: part.call_id,
                            name: alias(part.tool, OPENAI_CAP),
                            input: words,
                        },
                        None => Item::Called {
                            kind: "function_call",
                            call_id: part.call_id,
                            name: alias(part.tool, OPENAI_CAP),
                            arguments: arguments(part.state),
                        },
                    });
                }
                for part in &calls {
                    input.push(Item::Answered {
                        // The same predicate the call item was written from,
                        // so a pair can never be half custom.
                        kind: if replayed_input(part).is_some() {
                            CUSTOM_TOOL_CALL_OUTPUT
                        } else {
                            "function_call_output"
                        },
                        call_id: part.call_id,
                        output: result(part.state),
                    });
                }
            }
        }

        let mut tools: Vec<ToolSpec<'a>> = request
            .tools
            .iter()
            .map(|tool: &ToolDefinition| ToolSpec::Function {
                kind: "function",
                name: alias(&tool.name, OPENAI_CAP),
                description: &tool.description,
                parameters: &tool.schema,
            })
            .collect();
        // After every function entry rather than beside its twin, so that a
        // function tool's position never depends on a config key and a
        // request stays diffable against the same session without one.
        for (name, advertised) in custom_advertisement(request) {
            let tool = match advertised {
                Ok((tool, _)) => tool,
                Err(reason) => {
                    tracing::debug!(
                        tool = name.as_str(),
                        reason,
                        "a custom_tools name was advertised as a function"
                    );
                    continue;
                }
            };
            tools.push(ToolSpec::Custom {
                kind: CUSTOM,
                name: alias(&tool.name, OPENAI_CAP),
                description: &tool.description,
            });
        }
        tools.extend(request.responses.server_tools.iter().cloned().map(ToolSpec::Server));

        let mut include: Vec<&'a str> = Vec::new();
        let sealed = (backend.replays_reasoning() && seals_reasoning(&request.model))
            .then_some(REASONING_INCLUDE);
        let effort = request.effort_options.get("include").and_then(Value::as_array);
        for entry in sealed
            .into_iter()
            .chain(effort.into_iter().flatten().filter_map(Value::as_str))
            .chain(request.responses.include.iter().map(String::as_str))
        {
            if !include.contains(&entry) {
                include.push(entry);
            }
        }

        Self {
            model: &request.model,
            stream: true,
            store: false,
            include,
            instructions: request.system.as_deref(),
            input,
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
        self.tools.extend(names.iter().map(|name| {
            let mut entry = Map::new();
            entry.insert(
                "type".to_owned(),
                Value::String(format!("{}{name}", openrouter::SERVER_TOOL_PREFIX)),
            );
            ToolSpec::Server(entry)
        }));

        self
    }
}

/// One call a step made, borrowed from the part that recorded it.
struct Made<'a> {
    call_id: &'a str,
    tool: &'a str,
    state: &'a ToolState,
    /// Whether the model made it through this wire's custom advertisement
    /// (**D563**), which is what decides the pair of items it replays as.
    custom: bool,
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
) -> (Option<Cow<'_, str>>, Vec<Attached<'_>>, Vec<Made<'_>>, Vec<Thought<'_>>) {
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
            PartBody::Tool { call_id, tool, state, custom } => {
                calls.push(Made { call_id, tool, state, custom: *custom })
            }
            PartBody::Reasoning { provider, item, encrypted } => {
                thoughts.push(Thought { provider, item, encrypted: encrypted.as_deref() })
            }
            // A binary attachment the engine read at send time, and only for a
            // mime `accepts_attachment` said yes to — the match is by payload
            // shape rather than by allowlist.
            PartBody::File { path, mime, content: Some(content), .. } => {
                attachments.push(Attached { path, mime, content })
            }
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
    /// Which argument a custom call's `input` becomes, by registry name.
    custom: CustomArguments,
    /// Custom calls already announced by their opening frame, by `call_id`,
    /// so the closing frame does not announce them twice.
    opened_custom: HashSet<String>,
    /// The model this request asked for, for the served-tier log line.
    model: String,
    /// The `service_tier` this request carried, for the same line: what was
    /// served is only worth logging beside what was asked.
    requested: Option<String>,
    /// Whether this stream's four echo fields mean what [`Mapping::served`]
    /// reads them as — [`Backend::echoes_options`]'s reading half, the way
    /// `seals` is [`Backend::replays_reasoning`]'s.
    echoes: bool,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            seals: true,
            usage: Usage::default(),
            calls: HashMap::new(),
            aliases: Aliases::default(),
            thinking: None,
            custom: CustomArguments::default(),
            opened_custom: HashSet::new(),
            model: String::new(),
            requested: None,
            echoes: true,
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
            echoes: backend.echoes_options(),
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
            // A custom call's `input` streams here and arrives again, whole,
            // on the item's closing frame — which is the one read, because
            // the input is mapped onto an argument object only once it is
            // complete.
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            // A hosted tool's own progress, one lifecycle frame per stage, for
            // each of the five item kinds [`HOSTED_TOOL_ITEMS`] lists. Every
            // one of them is an announcement about work happening on the
            // vendor's side: the call, its arguments and its answer arrive
            // whole on the item's own `response.output_item.done`, which is
            // where [`server_tool`] reads them, and a hosted call has no row
            // to open early because there is no dialog and nothing to run.
            // `partial_image` is dropped for the same reason and one more —
            // the whole image arrives as the closed item's `result`, so a
            // half-drawn one would be bytes nobody ever replaces.
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.file_search_call.in_progress"
            | "response.file_search_call.searching"
            | "response.file_search_call.completed"
            | "response.image_generation_call.in_progress"
            | "response.image_generation_call.generating"
            | "response.image_generation_call.partial_image"
            | "response.image_generation_call.completed"
            | "response.code_interpreter_call.in_progress"
            | "response.code_interpreter_call.interpreting"
            | "response.code_interpreter_call.completed"
            | "response.mcp_call.in_progress"
            | "response.mcp_call.completed"
            | "response.mcp_call.failed"
            | "keepalive" => {}
            "response.output_item.added" => self.opened(&chunk["item"], events),
            "response.function_call_arguments.delta" => self.filled(&chunk, events),
            "response.output_item.done" => self.closed(&chunk["item"], events),
            "response.completed" | "response.incomplete" => {
                self.served(&chunk["response"], events);
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
        if item["type"].as_str() == Some(CUSTOM_TOOL_CALL) {
            self.custom_opened(item, events);
            return;
        }
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
            name: self.aliases.original(item["name"].as_str().unwrap_or_default().to_owned()),
        });
    }

    /// Appends a fragment of a call's arguments.
    fn filled(&mut self, chunk: &Value, events: &mut Vec<ProviderEvent>) {
        let Some(id) = chunk["item_id"].as_str().and_then(|item_id| self.calls.get(item_id)) else {
            tracing::debug!("arguments arrived for a call that was never opened");
            return;
        };

        if let Some(json) = chunk["delta"].as_str()
            && !json.is_empty()
        {
            events.push(ProviderEvent::ToolCallDelta { id: id.clone(), json: json.to_owned() });
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
            CUSTOM_TOOL_CALL => self.custom_closed(item, events),
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
            // A hosted tool the vendor ran (**D563**): the item types its own
            // server tools close as, enumerated because this vendor's are not
            // namespaced and an unknown item type is not a tool call.
            kind if HOSTED_TOOL_ITEMS.contains(&kind) => server_tool(kind, item, events),
            _ => {}
        }
    }

    /// Announces a custom call the model started making: its start, then the
    /// marker that says how it was made, before any argument arrives.
    fn custom_opened(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        let Some(call_id) = item["call_id"].as_str().filter(|id| !id.is_empty()) else {
            tracing::debug!("a custom tool call arrived without the id that correlates it");
            return;
        };
        if !self.opened_custom.insert(call_id.to_owned()) {
            return;
        }

        events.push(ProviderEvent::ToolCallStart {
            id: call_id.to_owned(),
            name: self.aliases.original(item["name"].as_str().unwrap_or_default().to_owned()),
        });
        events.push(ProviderEvent::ToolCallCustom { id: call_id.to_owned() });
    }

    /// Closes a custom call: its whole `input`, mapped onto the one argument
    /// the tool takes, then its end.
    ///
    /// A call whose opening frame never arrived is announced here first, so
    /// the four events always arrive in the same order.
    fn custom_closed(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        self.custom_opened(item, events);
        let Some(call_id) = item["call_id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        self.opened_custom.remove(call_id);

        let name = self.aliases.original(item["name"].as_str().unwrap_or_default().to_owned());
        match self.custom.argument(&name) {
            Some(argument) => {
                let mut arguments = Map::new();
                arguments.insert(
                    argument.to_owned(),
                    Value::String(item["input"].as_str().unwrap_or_default().to_owned()),
                );
                events.push(ProviderEvent::ToolCallDelta {
                    id: call_id.to_owned(),
                    json: Value::Object(arguments).to_string(),
                });
            }
            // Not a tool this request advertised as custom. Closed with no
            // arguments rather than guessed at: the tool refuses the call,
            // and the model reads why.
            None => tracing::debug!(
                tool = name.as_str(),
                "a custom tool call named a tool this request did not advertise as one"
            ),
        }
        events.push(ProviderEvent::ToolCallEnd { id: call_id.to_owned() });
    }

    /// Reports what the terminal frame echoed about how the request was
    /// served, when it echoed anything.
    ///
    /// Nothing is said for a frame that echoed none of the four fields —
    /// every recorded fixture's — because an empty report is not a report that
    /// the backend served the defaults. Nothing is said on a backend that does
    /// not speak about these options at all ([`Backend::echoes_options`]),
    /// whatever its terminal frame happens to carry under those names.
    fn served(&self, response: &Value, events: &mut Vec<ProviderEvent>) {
        if !self.echoes {
            return;
        }
        let served = ServedOptions {
            service_tier: response["service_tier"].as_str().map(str::to_owned),
            verbosity: response["text"]["verbosity"].as_str().map(str::to_owned),
            parallel_tool_calls: response["parallel_tool_calls"].as_bool(),
            context: response["reasoning"]["context"].as_str().map(str::to_owned),
        };
        if served == ServedOptions::default() {
            return;
        }
        if let Some(tier) = served.service_tier.as_deref() {
            tracing::debug!(
                model = self.model.as_str(),
                served = tier,
                requested = self.requested.as_deref(),
                "service_tier the backend served"
            );
        }

        events.push(ProviderEvent::Served(served));
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
    let Some(encrypted) = item["encrypted_content"].as_str().filter(|state| !state.is_empty())
    else {
        tracing::debug!(item = id, "a reasoning item arrived with no state to replay");
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

    /// The image generator's answer, which is bytes: it rides the [`Blob`] and
    /// never the text a row would print. Dropped for that item kind alone —
    /// on any other hosted tool `result` is whatever that vendor's tool calls
    /// its own field, and this wire has measured none of them, so the
    /// fallback shows it rather than deciding it means the same thing.
    const BINARY_ANSWER: &str = "result";

    let blob = (kind == IMAGE_GENERATION_CALL)
        .then(|| item[BINARY_ANSWER].as_str())
        .flatten()
        .filter(|base64| !base64.is_empty())
        .map(|base64| Blob {
            // The format the vendor documents as the default when the item
            // does not say, so a missing field is not a missing mime.
            mime: format!("image/{}", item["output_format"].as_str().unwrap_or("png")),
            base64: base64.to_owned(),
        });

    let input = match &item["arguments"] {
        Value::String(arguments) => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.clone()))
        }
        // What a web search was asked, which this vendor carries as the
        // item's `action` rather than as arguments.
        Value::Null if kind == WEB_SEARCH_CALL && !item["action"].is_null() => {
            item["action"].clone()
        }
        Value::Null => {
            let rest: serde_json::Map<String, Value> = item
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .filter(|(key, _)| !ENVELOPE.contains(&key.as_str()))
                        .filter(|(key, _)| {
                            kind != IMAGE_GENERATION_CALL || key.as_str() != BINARY_ANSWER
                        })
                        // Already the blob's mime, and a row showing it
                        // beside an image nobody can see says nothing.
                        .filter(|(key, _)| blob.is_none() || key.as_str() != "output_format")
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            if rest.is_empty() { Value::Null } else { Value::Object(rest) }
        }
        arguments => arguments.clone(),
    };
    let output = match &item["output"] {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        structured => structured.to_string(),
    };

    tracing::debug!(
        tool = kind,
        bytes = blob.as_ref().map_or(0, |blob| blob.base64.len()),
        "a server tool answered"
    );
    events.push(ProviderEvent::ServerTool { tool: kind.to_owned(), input, output, blob });
}

/// The item a Responses custom tool call arrives as, and replays as
/// (**D563**).
const CUSTOM_TOOL_CALL: &str = "custom_tool_call";

/// The item a replayed custom call's result rides, the custom twin of
/// `function_call_output` (**D563**).
const CUSTOM_TOOL_CALL_OUTPUT: &str = "custom_tool_call_output";

/// The `type` a custom tool is advertised under.
const CUSTOM: &str = "custom";

/// The model's own words behind a stored custom call — its `input`, recovered
/// from the arguments object the inbound mapper wrapped it in (**D563**) —
/// and `None` for a stored call that cannot be replayed as a custom item at
/// all.
///
/// The wrapping is this wire's own and has exactly one member: a tool can only
/// be advertised as custom when its schema has one required string argument
/// ([`single_string_argument`]), and [`Mapping::custom_closed`] writes the
/// call's `input` under that one name. So the single string member is the
/// words that were sent, and handing the backend anything else would hand it
/// an item it never produced.
///
/// Every other state replays as the `function_call` pair instead. A custom
/// item's `input` is the call's whole content, so writing `""` would put a
/// call that said nothing into the transcript and the model would read it
/// back as words it chose; the stored arguments object says exactly what the
/// record says. Three states reach that fallback: a call whose name the
/// request that received it never advertised as custom, which
/// [`Mapping::custom_closed`] closes with no arguments; a call the model never
/// finished streaming ([`ToolState::Pending`] with none); and an object this
/// wire did not write, which has no single string to unwrap.
///
/// Both replay loops read this one predicate, so a call item and its output
/// twin can never disagree about which pair a stored call is.
fn replayed_input<'a>(made: &Made<'a>) -> Option<&'a str> {
    if !made.custom {
        return None;
    }
    let input = match made.state {
        ToolState::Pending { input: None } => return None,
        ToolState::Pending { input: Some(input) }
        | ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => input,
    };

    let mut values = input.as_object()?.values();
    match (values.next().and_then(Value::as_str), values.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// The item a hosted web search closes as.
const WEB_SEARCH_CALL: &str = "web_search_call";

/// The item a hosted image generation closes as — the one hosted tool whose
/// answer is bytes.
const IMAGE_GENERATION_CALL: &str = "image_generation_call";

/// Every item type a hosted tool this vendor serves closes as, one per
/// `server_tools` type either id accepts.
const HOSTED_TOOL_ITEMS: [&str; 5] = [
    WEB_SEARCH_CALL,
    IMAGE_GENERATION_CALL,
    "file_search_call",
    "code_interpreter_call",
    "mcp_call",
];

/// The one argument a tool's custom `input` becomes, if the tool has one.
///
/// The predicate [`options::CUSTOM_TOOLS`] is derived by: exactly one
/// `required` property, and that property a string. A tool with two required
/// arguments has no way to receive the second through one free-text input.
fn single_string_argument(schema: &Value) -> Option<&str> {
    let [required] = schema["required"].as_array()?.as_slice() else {
        return None;
    };
    let required = required.as_str()?;

    (schema["properties"][required]["type"].as_str() == Some("string")).then_some(required)
}

/// Each name `request` lists in `custom_tools`, beside the roster tool and the
/// argument it is advertised custom with, or the reason it is not: listed, on
/// the roster, and single-string. [`Body::new`] advertises by it and logs each
/// miss; [`CustomArguments::of`] maps calls back by it.
fn custom_advertisement(
    request: &ChatRequest,
) -> impl Iterator<Item = (&String, Result<(&ToolDefinition, &str), &'static str>)> {
    request.responses.custom_tools.iter().map(|name| {
        let Some(tool) = request.tools.iter().find(|tool| tool.name == *name) else {
            return (name, Err("not on this request's roster"));
        };
        let advertised = single_string_argument(&tool.schema)
            .map(|argument| (tool, argument))
            .ok_or("its schema is not exactly one required string argument");
        (name, advertised)
    })
}

/// The argument each tool this request advertised as custom takes its `input`
/// as, by registry name — what the mapper needs to turn a custom call back
/// into an ordinary argument object.
#[derive(Clone, Debug, Default)]
struct CustomArguments(HashMap<String, String>);

impl CustomArguments {
    /// The map for the custom tools `request` actually advertised.
    fn of(request: &ChatRequest) -> Self {
        Self(
            custom_advertisement(request)
                .filter_map(|(name, advertised)| {
                    let (_, argument) = advertised.ok()?;
                    Some((name.clone(), argument.to_owned()))
                })
                .collect(),
        )
    }

    fn argument(&self, tool: &str) -> Option<&str> {
        self.0.get(tool).map(String::as_str)
    }
}

/// The item kind a tool call arrives as.
const FUNCTION_CALL: &str = "function_call";

/// "Call one if you decide to", the only one of this API's three tool-choice
/// values an agent loop ever wants: `"none"` would advertise a roster nothing
/// may call, and naming one tool is a decision the *model* is being asked to
/// make. See [`defaulted`] for which backends are sent it, and why.
const TOOL_CHOICE_AUTO: &str = "auto";

/// What `stream_options.include_obfuscation` defaults to on this vendor's
/// two backends (**D563**): the padding field every delta otherwise carries
/// costs bytes on every frame and protects against a side channel a
/// terminal session reading its own stream is not exposed to. Honored on the
/// seat (probe 2026-09-16, row 22: the deltas lose `obfuscation`).
const INCLUDE_OBFUSCATION: bool = false;

/// What `reasoning.summary` defaults to for a model that reasons: the
/// vendor's own spelling of "show the thinking", and the one value the seat
/// has been measured serving (probe 2026-09-16), which is why **D563** refuses
/// to make the key configurable there at all.
const REASONING_SUMMARY: &str = "auto";

/// The body key that names one tool, or a list of them.
const TOOL_CHOICE: &str = "tool_choice";

/// The two body keys that mean something only beside a tool roster.
const ROSTER_KEYS: [&str; 2] = [TOOL_CHOICE, "parallel_tool_calls"];

/// Every key the typed [`Body`] writes itself, in the order it writes them.
///
/// A layer may not carry one. Most of them are already unmakeable — the body
/// is the last layer, so its `model`, `stream`, `store`, `input` and `tools`
/// win the collision — but `include` and `instructions` are omitted when the
/// body has nothing to put in them, and a layer's value would then be sent as
/// if the wire had chosen it. The others are stripped for the reason a
/// no-op deserves to be visible at all: a configured key that silently does
/// nothing costs somebody an afternoon, and one that says why costs a log
/// line.
const OWN_KEYS: [&str; 7] =
    ["model", "stream", "store", "include", "instructions", "input", "tools"];

/// The item kind sealed thinking arrives as, and goes back as.
const REASONING: &str = "reasoning";

/// OpenAI's name for a fragment of readable thinking, which only exists
/// downstream of a `reasoning.summary` in the request ([`defaulted`]).
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

/// The free defaults a `backend`'s request carries, as the lowest layer
/// (**D563**).
///
/// Exhaustive on purpose: a sixth backend has to decide what it is sent.
///
/// - **This vendor's two** get `stream_options.include_obfuscation: false`,
///   `tool_choice: "auto"` beside a non-empty roster, and — for a model that
///   reasons — `reasoning.summary: "auto"`. The Responses API streams no
///   readable thinking unless the request asks —
///   `response.reasoning_summary_text.delta`, which [`Mapping`] turns into the
///   pane's thinking, only exists downstream of a `reasoning.summary` — and a
///   model that does not reason answers a `reasoning` field with a 400, which
///   is why that one is gated by [`seals_reasoning`] exactly as `include` is.
///   None of the three changes what a turn costs, and every one sits under
///   the layers above it, so a configured value or an effort's own replaces
///   it; there is deliberately no spelling that omits one, and a vendor that
///   refuses one is fixed at its constant.
/// - **[`Backend::OpenRouter`]** gets `tool_choice: "auto"` beside a
///   non-empty roster and nothing else — see [`super::openrouter`]'s ledger
///   for why that vendor gets no `reasoning` default, which is the other
///   vendor's.
/// - **The gateways and a config-named endpoint** get nothing: they are sent
///   exactly what they were sent before this layer existed.
///
/// **No `service_tier`, ever.** The engine resolves that one, so that what a
/// status bar reports is what was sent.
fn defaulted(backend: Backend, roster: usize, model: &str) -> Map<String, Value> {
    let mut layer = Map::new();
    match backend {
        Backend::Codex | Backend::Platform => {
            let mut stream_options = Map::new();
            stream_options.insert("include_obfuscation".to_owned(), INCLUDE_OBFUSCATION.into());
            layer.insert("stream_options".to_owned(), Value::Object(stream_options));
            if roster > 0 {
                layer.insert(TOOL_CHOICE.to_owned(), TOOL_CHOICE_AUTO.into());
            }
            if seals_reasoning(model) {
                let mut reasoning = Map::new();
                reasoning.insert("summary".to_owned(), REASONING_SUMMARY.into());
                layer.insert(REASONING.to_owned(), Value::Object(reasoning));
            }
        }
        Backend::OpenRouter => {
            if roster > 0 {
                layer.insert(TOOL_CHOICE.to_owned(), TOOL_CHOICE_AUTO.into());
            }
        }
        Backend::Opencode(_) | Backend::Compat => {}
    }

    layer
}

/// The configured body, as this one request can carry it (**D563**).
///
/// Two checks only the wire can make, because only the wire sees a request's
/// roster and model: the engine hands every request of a turn the same body,
/// and a title or compaction request offers no tools while the model a child
/// runs may not reason.
///
/// - `tool_choice` and `parallel_tool_calls` go when `offered` is false —
///   when the serialized `tools` array is empty, hosted entries included, so
///   a hosted `tool_choice` beside a hosted tool and no function roster is
///   kept. Measured, not guessed: the seat answered a tool-less request
///   carrying `tool_choice = "required"`, sent ungated, with a 400 reading
///   `Tool choice 'required' must be specified with 'tools' parameter.`
///   (probe 2026-09-17), so without this drop a configured `required` would
///   fail every compaction request.
/// - `reasoning` goes whole when the model does not reason, for the 400
///   [`defaulted`] avoids.
/// - A key the typed [`Body`] writes itself goes always ([`OWN_KEYS`]).
///
/// And one translation, for the same reason: a tool the config named by its
/// registry name is on the roster under [`alias`]'s spelling of it, which
/// only this side knows. `tool_choice.name` and each `allowed_tools`
/// entry's move with it, so a configured choice keeps naming a tool the
/// request actually offered rather than one the model was never told about.
/// The names are deliberately not validated here — the roster is per turn,
/// and the config surface says so.
///
/// Every drop is logged, one line per key, so a configured value that did
/// nothing on a request says why.
fn gated(body: &Map<String, Value>, offered: bool, model: &str) -> Map<String, Value> {
    /// The aliased spelling of whatever `entry` names, where it names
    /// anything and the alias differs.
    fn realias(entry: &mut Value) {
        let Some(entry) = entry.as_object_mut() else {
            return;
        };
        let Some(Cow::Owned(aliased)) =
            entry.get("name").and_then(Value::as_str).map(|name| alias(name, OPENAI_CAP))
        else {
            return;
        };
        entry.insert("name".to_owned(), Value::String(aliased));
    }

    let mut body = body.clone();
    for key in OWN_KEYS {
        if body.remove(key).is_some() {
            dropped(key, "the wire writes this key itself");
        }
    }
    if let Some(choice) = body.get_mut(TOOL_CHOICE) {
        realias(choice);
        if let Some(entries) = choice.get_mut("tools").and_then(Value::as_array_mut) {
            for entry in entries {
                realias(entry);
            }
        }
    }
    if !offered {
        for key in ROSTER_KEYS {
            if body.remove(key).is_some() {
                dropped(key, "no tools");
            }
        }
    }
    if !seals_reasoning(model)
        && let Some(reasoning) = body.remove(REASONING)
    {
        match reasoning.as_object() {
            Some(object) if !object.is_empty() => {
                for key in object.keys() {
                    dropped(&format!("{REASONING}.{key}"), "model does not reason");
                }
            }
            _ => dropped(REASONING, "model does not reason"),
        }
    }

    body
}

/// The directives a request carries that are written into the body rather
/// than spliced as keys, as the topmost layer (**D563**).
///
/// - `service_tier`, already resolved by the engine, on whichever backend was
///   handed one.
/// - `text.format`, the document `run --json-schema` rides, merged into the
///   one `text` object a configured `verbosity` may already have opened.
/// - `reasoning.summary`, on the platform alone — the seat has only ever
///   been measured with `auto` — and only for a model that reasons, so it can
///   never recreate the `reasoning` object [`gated`] just removed. Above the
///   effort, because it is the one key where the catalog effort carries a
///   *default* rather than a selection, and a configured value has to be able
///   to outrank it.
fn directed(request: &ChatRequest, backend: Backend) -> Map<String, Value> {
    let options = &request.responses;
    let mut layer = Map::new();
    if let Some(tier) = &options.service_tier {
        layer.insert("service_tier".to_owned(), Value::String(tier.clone()));
    }
    if let Some(format) = &options.text_format {
        let mut text = Map::new();
        text.insert("format".to_owned(), format.clone());
        layer.insert("text".to_owned(), Value::Object(text));
    }
    if let Some(summary) = &options.reasoning_summary
        && backend == Backend::Platform
    {
        if seals_reasoning(&request.model) {
            let mut reasoning = Map::new();
            reasoning.insert("summary".to_owned(), Value::String(summary.clone()));
            layer.insert(REASONING.to_owned(), Value::Object(reasoning));
        } else {
            dropped("reasoning.summary", "model does not reason");
        }
    }

    layer
}

/// Says that a configured key was left off this request, and why.
fn dropped(key: &str, reason: &'static str) {
    tracing::debug!(key, reason, "a configured Responses key was dropped from this request");
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

    ProviderError::Status { status: 500, message }
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
