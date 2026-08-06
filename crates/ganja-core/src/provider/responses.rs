//! OpenAI's Responses API, streamed — the wire this vendor speaks, both ways in.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts:183-186`, whose
//! whole body for an OpenAI model is `evt.language = evt.sdk.responses(...)`.
//! It reads no credential: **the vendor picks the wire, not the token**, which
//! is why an API key session belongs here too and not on chat completions. The
//! same file disables `gpt-5-chat-latest` at `:164-171` with the consequence
//! written on it — that alias is chat-completions-only, so a Responses-only
//! vendor cannot serve it — and [`CHAT_COMPLETIONS_ONLY`] is that arm ported.
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
//! One mapping, one encoder, two places a request can go — [`Backend`] is the
//! whole of the difference, and it is fixed when the provider is built because
//! it follows the credential the session resolved:
//!
//! | | [`Backend::Codex`] | [`Backend::Platform`] |
//! |---|---|---|
//! | credential | a stored ChatGPT login | an API key |
//! | base URL | [`DEFAULT_BASE_URL`] | [`openai::DEFAULT_BASE_URL`] |
//! | extra headers | [`ACCOUNT_HEADER`], [`ORIGINATOR_HEADER`], [`BETA_HEADER`] | none |
//! | model gate | [`serves`] | whatever the platform serves |
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
        ChatRequest, Credential, Mapper, Provider, ProviderError, ProviderEvent, Resolved,
        check_base_url, client, open,
        openai::{self, arguments, result},
        require_key, setting, shown_base_url,
        sse::Frame,
        steps,
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

/// Which of this vendor's two backends a provider was built against.
///
/// Not a runtime question: it follows the credential, which is resolved once
/// per session in [`super::openai_provider`]. Keeping it a field rather than
/// re-deriving it per request is what makes "a key request is never filtered by
/// the seat's allow-list" a fact about how the provider was constructed instead
/// of a condition somebody could forget to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    /// The backend a ChatGPT subscription is served by, reached with an OAuth
    /// access token (`codex.ts:12`).
    Codex,
    /// OpenAI's own platform API, reached with an API key.
    Platform,
}

impl Backend {
    /// Where this backend lives when [`BASE_URL_ENV`](openai::BASE_URL_ENV)
    /// names nothing.
    ///
    /// Two hosts because the credential decides which one will take it: a
    /// ChatGPT token is refused by the platform, and a key is refused by the
    /// codex backend.
    const fn default_base_url(self) -> &'static str {
        match self {
            Self::Codex => DEFAULT_BASE_URL,
            Self::Platform => openai::DEFAULT_BASE_URL,
        }
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

/// What a subscription session asks for when nothing named a model.
///
/// **Not [`crate::catalog::default_model`]**, and the reason is the shape of
/// that table: it is one row per *vendor*, and this vendor has two backends
/// that serve different sets. A catalog default is therefore free to name a
/// model the platform sells and the seat does not — which is exactly what
/// `gpt-5.6` is — and handing it to a subscription session produces a seat that
/// cannot take a turn at all. A model named explicitly is never substituted:
/// somebody who asked for `gpt-5.6` on a ChatGPT login is told what the seat
/// serves ([`unsupported`]) rather than quietly answered by something else.
///
/// The one this names is the model the P8 live pass measured taking a whole
/// tool-calling turn on this backend, and it has to satisfy [`serves`] — pinned
/// below, because a default this backend refuses is the bug this constant
/// exists to prevent.
pub(crate) const SUBSCRIPTION_DEFAULT: &str = "gpt-5.4";

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
    credential: Credential,
    base_url: String,
    /// Which backend this provider was built for — see [`Backend`] for the
    /// table of what it decides.
    backend: Backend,
}

impl fmt::Debug for ResponsesProvider {
    /// Renders without the credential, the way every provider here does. The
    /// base URL goes through [`shown_base_url`] for the same reason the
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
            Credential::Key(require_key(ID, openai::API_KEY_ENV)?),
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
            Credential::Oauth {
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
    fn built(
        credential: Credential,
        base_url: String,
        backend: Backend,
    ) -> Result<Self, ProviderError> {
        check_base_url(&base_url)?;

        Ok(Self {
            client: client()?,
            credential,
            base_url,
            backend,
        })
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
    fn refuses(&self, model: &str) -> Option<ProviderError> {
        if CHAT_COMPLETIONS_ONLY.contains(&model) {
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

        built.json(&Body::new(request)).build().map_err(|error| {
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
    fn id(&self) -> &str {
        ID
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

        open(
            &self.client,
            built,
            &resolved.presented,
            cancel,
            Mapping::default(),
        )
        .await
        .map_err(|error| reauth(backend, error))
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
}

/// One tool as the Responses API advertises it.
///
/// Flatter than chat completions', which nests the same four fields under
/// `function` (`openai-responses-prepare-tools.ts`, `prepareFunctionTool`).
#[derive(Debug, Serialize)]
struct ToolSpec<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    /// The argument schema, which this API names `parameters` as well.
    parameters: &'a Value,
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
        name: &'a str,
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
/// The kind differs by who said it: what reaches the model is `input_text`,
/// what the model said is `output_text`.
#[derive(Debug, Serialize)]
struct Block<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
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
    /// A step carrying neither text nor calls contributes nothing, which is
    /// what the marker opening a turn and a turn that died before its first
    /// fragment both are.
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
    fn new(request: &'a ChatRequest) -> Self {
        let mut input: Vec<Item<'a>> = Vec::new();

        for message in &request.messages {
            let (role, block) = match message.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            // Scoped to the message, as upstream's `reasoningItems` map is.
            let mut replayed: HashSet<&str> = HashSet::new();

            for step in steps(&message.parts) {
                let (texts, calls, thoughts) = split(step);

                if let Some(text) = texts {
                    input.push(Item::Said {
                        role,
                        content: vec![Block { kind: block, text }],
                    });
                }
                for thought in &thoughts {
                    if thought.provider != ID {
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
                        name: part.tool,
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

        Self {
            model: &request.model,
            stream: true,
            store: false,
            include: seals_reasoning(&request.model).then_some([REASONING_INCLUDE]),
            instructions: request.system.as_deref(),
            input,
            tools: request
                .tools
                .iter()
                .map(|tool: &ToolDefinition| ToolSpec {
                    kind: "function",
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.schema,
                })
                .collect(),
        }
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

/// Splits one step into the text it said, the calls it made, and the thinking
/// it sealed.
fn split(parts: &[Part]) -> (Option<Cow<'_, str>>, Vec<Made<'_>>, Vec<Thought<'_>>) {
    let mut texts: Vec<&str> = Vec::new();
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
            // A mentioned file is a *reference*, resolved into a text block
            // before a request is built (`session::resolve_mentions`); see the
            // same arm in `openai.rs`. `StepFinish` carries a step's bill
            // rather than content, and `StepStart` was consumed as the boundary
            // this step was cut at.
            PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
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

    (text, calls, thoughts)
}

/// Accumulates what the frames so far said.
#[derive(Debug, Default)]
struct Mapping {
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
            "response.reasoning_summary_text.delta" => {
                self.delta(&chunk, events, ProviderEvent::ReasoningDelta);
            }
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
            "error" => events.push(ProviderEvent::Failed(failure(&chunk))),
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
            name: item["name"].as_str().unwrap_or_default().to_owned(),
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
            REASONING => sealed(item, events),
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

/// The item kind a tool call arrives as.
const FUNCTION_CALL: &str = "function_call";

/// The item kind sealed thinking arrives as, and goes back as.
const REASONING: &str = "reasoning";

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

/// The chat-completions sentinel, which some deployments send here too.
const DONE: &str = "[DONE]";

/// Turns an error object into the failure the turn reports.
///
/// The status was 200 by the time any of these arrived and this API's `code` is
/// a slug rather than a number, so `500` is the truest thing there is to say —
/// the same reading the sibling's error chunks get.
fn failure(error: &Value) -> ProviderError {
    ProviderError::Status {
        status: 500,
        message: error["message"]
            .as_str()
            .unwrap_or("the provider reported an error")
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt as _;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        ACCOUNT_HEADER, ALLOWED_MODELS, BETA, BETA_HEADER, Backend, Body, CHAT_COMPLETIONS_ONLY,
        DEFAULT_BASE_URL, ID, Mapping, ORIGINATOR, ORIGINATOR_HEADER, ResponsesProvider,
        SUBSCRIPTION_DEFAULT, generation, reauth, seals_reasoning, serves,
    };
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        catalog,
        protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage},
        provider::{
            ChatRequest, Credential, PROVIDERS, Presented, Provider as _, ProviderError,
            ProviderEvent, Resolved,
            openai::{self, NO_RESULT},
            replay,
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
            Credential::Key(Presented::new(KEY).expect("a non-blank key")),
            "http://127.0.0.1:8080/v1".to_owned(),
            Backend::Platform,
        )
        .expect("loopback may carry a key")
    }

    /// One turn's worth of request, on a model this backend serves — anything
    /// else is refused before a request is built at all.
    fn ask() -> ChatRequest {
        ChatRequest {
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
        // An account id on a key credential is impossible — `Credential::Key`
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
            serde_json::to_value(Body::new(&ask())).expect("the body serializes")["store"],
            json!(false),
            "one encoder, so `store: false` is not a subscription special case"
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

    /// The field the live pass died on. A body without it is answered
    /// `400 {"detail":"Store must be set to false"}`, which is every
    /// subscription turn this build could take — so it is asserted on the
    /// serialized body rather than on the struct that produces it.
    ///
    /// Its companion is here too: with the backend keeping nothing, `include`
    /// is the only reason a second request can carry the first one's thinking.
    #[test]
    fn every_body_tells_the_backend_to_keep_nothing_and_to_seal_what_it_thought() {
        let body = serde_json::to_value(Body::new(&ask())).expect("the body serializes");

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
        let body = serde_json::to_value(Body::new(&plain)).expect("the body serializes");
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
            model: SERVED.to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

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
            model: SERVED.to_owned(),
            system: None,
            messages: vec![Message::user("hello"), assistant],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

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
            model: "gpt-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![Message::user("hello"), empty, Message::user("again")],
            tools: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(Body::new(&request)).expect("the body serializes"),
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

    #[test]
    fn a_request_advertises_the_tools_it_was_given() {
        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs")],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

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
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![
                Message::user("read src/main.rs"),
                assistant,
                Message::user("thanks"),
            ],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

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
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("fix the bug"), assistant],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");
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
        assistant
            .parts
            .push(tool_part("call_read", "read", ToolState::Pending));

        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

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
