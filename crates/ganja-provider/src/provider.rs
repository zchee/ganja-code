//! Sources of assistant text.
//!
//! A provider turns a [`ChatRequest`] into a stream of [`ProviderEvent`]s; the
//! engine is what maps those onto the protocol frontends see. Four wires ship:
//! [`FakeProvider`] for demos and tests, [`AnthropicProvider`] for the Messages
//! API, [`OpenAiProvider`] for anything speaking OpenAI chat completions, and
//! [`ResponsesProvider`] for the Responses API.
//!
//! **The vendor picks the wire, not the credential.** Everything filed under
//! `openai` speaks Responses, whether it authenticates with an API key or with
//! a stored ChatGPT login, because that is what upstream's plugin does with no
//! reference to the credential at all (`plugin/provider/openai.ts:185`). What
//! the credential still picks is which *backend* the request goes to and what
//! it carries beside the bearer, decided once per session by
//! `ganja_core::provider::openai_provider` — selection's half of the job, which
//! is why it is on the other side of this crate's edge.
//! [`OpenAiProvider`] therefore no longer serves that vendor directly; it is
//! the wire the two wrappers below ride, and the shape any other
//! OpenAI-compatible endpoint would.
//!
//! Two more providers are chat completions under another name, and are
//! deliberately not second wires: [`GrokProvider`] is xAI's endpoint and
//! [`CopilotProvider`] is GitHub's, each a base URL, a credential source and —
//! for Copilot — a header set over [`OpenAiProvider`]. Neither endpoint speaks
//! Responses, which is why the vendor's move to it left both untouched.
//!
//! [`CompatProvider`] generalises exactly that shape to endpoints this build
//! does not ship: a config's `provider` table names an id, a [`Dialect`], an
//! endpoint, the variable holding its key and whatever headers it wants, and
//! `ganja_core::provider::select` builds one of the two wires above from that.
//! So the set of
//! providers a session may name is **two tiers** — the builtins in
//! [`PROVIDERS`], plus whatever the config declares — while the narrower set
//! the catalog can size and price is a third fact about each, and neither
//! implies the other.
//!
//! Every HTTP provider shares the same shape — build a request, retry it while
//! it has not started answering, split the `text/event-stream` body into
//! [`sse::Frame`]s, and map those onto events — so everything except the
//! mapping lives here.
//!
//! Failures are reported in one of two ways, and never as a completed turn. A
//! request that never starts streaming fails the call to [`Provider::stream`];
//! one that dies mid-stream yields [`ProviderEvent::Failed`]. The engine turns
//! both into a `Failed` finish carrying the message.
//!
//! # Where a credential can travel
//!
//! Both HTTP providers authenticate with a header, so every hop the request
//! takes is a party that sees the key. Three things bound that set, and all
//! three are here rather than in the individual providers:
//!
//! - Redirects are not followed ([`client`]). `reqwest` strips `Authorization`
//!   across hosts but knows nothing about Anthropic's `x-api-key`, so a 3xx
//!   from a hijacked endpoint would hand the key to whatever it names. These
//!   are one-shot `POST`s that never legitimately redirect.
//! - The endpoint must be `https`, or loopback ([`check_base_url`]). The base
//!   URL is environment-controlled, and plain HTTP to anywhere else puts the
//!   key on the wire in the clear.
//! - `reqwest` is built with its `system-proxy` feature, so `HTTPS_PROXY`,
//!   `HTTP_PROXY` and `ALL_PROXY` in the environment redirect provider traffic
//!   through a proxy of their choosing. That is deliberate — a corporate
//!   network is frequently only reachable that way — but it is a trust
//!   boundary: whoever sets those variables chooses who terminates the
//!   connection. For an `https` endpoint a proxy sees a `CONNECT` tunnel it
//!   cannot read without a certificate the machine already trusts; for a
//!   loopback endpoint no proxy is used.

use std::{
    collections::{BTreeMap, VecDeque},
    env, fmt,
    sync::Arc,
};

use async_trait::async_trait;
use futures::{
    Stream, StreamExt as _,
    stream::{self, BoxStream},
};
use reqwest::Url;
use secrecy::{ExposeSecret as _, SecretString};
use tokio_util::sync::CancellationToken;
use url::Host;

use crate::{
    auth, catalog,
    protocol::{FinishReason, Message, Part, PartBody, Usage},
    provider::sse::Frame,
    tool::ToolDefinition,
};

pub mod anthropic;
pub mod compat;
pub mod copilot;
pub mod cursor;
pub mod fake;
pub mod grok;
pub mod openai;
pub mod responses;
pub mod retry;
pub mod sse;

/// What a call that never produced one reports as its result.
///
/// A tool part still pending or running when it reaches a later request
/// belongs to a turn that died — cancelled, or failed — before the tool
/// answered. Dropping the call would leave the assistant text claiming a call
/// that is not there; leaving it unanswered is a request every vendor refuses,
/// because each opened call must be resolved before the next assistant turn.
/// So every wire fills the hole with this one spelling — or the wires tell
/// the model different stories about the same dead turn. Upstream resolves it
/// with the wording "[Tool execution was interrupted]".
pub(crate) const NO_RESULT: &str = "[no result recorded]";

/// Refuses to build a provider whose login is not in the store.
///
/// `storage_key` rather than a second spelling: what the file calls a
/// provider is `auth`'s to know, and asking is not writing it down.
///
/// # Errors
///
/// Returns [`ProviderError::Auth`] when nothing is stored for `id` — naming
/// the login that repairs it — or when the credential store exists and could
/// not be read, which a login does not fix and the store's own message
/// describes.
pub(super) fn require_stored_login(id: &str) -> Result<(), ProviderError> {
    let stored = crate::auth::storage_key(id);
    let listed =
        crate::auth::list_providers().map_err(|error| ProviderError::Auth(error.to_string()))?;
    if !listed.iter().any(|entry| entry.provider_id == stored) {
        return Err(ProviderError::Auth(format!(
            "no {id} credential is stored; run `ganja auth login {id}`"
        )));
    }

    Ok(())
}

pub use anthropic::AnthropicProvider;
pub use compat::{CompatProvider, Dialect};
pub use copilot::CopilotProvider;
pub use cursor::CursorProvider;
pub use fake::FakeProvider;
pub use grok::GrokProvider;
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;

/// Environment variable naming the provider a session talks to.
pub const PROVIDER_ENV: &str = "GANJA_PROVIDER";

/// Environment variable overriding the model a session asks for.
pub const MODEL_ENV: &str = "GANJA_MODEL";

/// Every provider this build ships.
///
/// **Not the whole of what [`PROVIDER_ENV`] accepts.** A config's `provider`
/// table names endpoints of its own, and those are selectable too — see
/// `ganja_core::provider::selectable`, which is the predicate, and
/// `ganja_core::provider::select`, which is where the table is consulted. This
/// list is the half that needs no configuration.
///
/// Being selectable is also not the same as being **cataloged**: the catalog
/// prices and sizes what it has rows for, which is every builtin here except
/// [`fake`] and [`cursor`] and none of the configured ones.
/// [`catalog::carries`] is that second tier, and a provider outside it runs on
/// the degradation path — no auto-compaction, no cost, a title from its own
/// model.
pub const PROVIDERS: [&str; 6] = [
    anthropic::ID,
    openai::ID,
    grok::ID,
    copilot::ID,
    fake::ID,
    cursor::ID,
];

/// One request to a model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    /// Model identifier, spelled the way the provider expects it on the wire.
    pub model: String,
    /// What the model is told before it is told anything else, composed by
    /// `ganja_core::instruction::system_prompt` and installed with
    /// `ganja_core::Engine::with_system`. [`None`] is an engine nobody
    /// configured, and the shape every scripted run asks in.
    pub system: Option<String>,
    /// The conversation so far, oldest first, ending with the message the user
    /// just sent.
    pub messages: Vec<Message>,
    /// Tools the model may call, advertised on every request. Empty means the
    /// model is not offered any.
    pub tools: Vec<ToolDefinition>,
    /// The option map of the catalog effort this turn runs under, spliced
    /// into the wire's request body by [`splice_effort`]. Empty — the shape
    /// every request had before efforts existed — means no effort, and the
    /// body is exactly the wire's own.
    pub effort_options: serde_json::Map<String, serde_json::Value>,
}

/// The request body a wire sends, with the effort's options under it.
///
/// The effort map goes into the merged object **first** and the wire's own
/// fields land after it, so a key both claim resolves to the wire: its required
/// fields — the model, the stream flag, `max_tokens` — are what make the
/// request one its API accepts, and a catalog row must not be able to unmake
/// that. A body that is not a JSON object (no wire here has one) is passed
/// through, because there is nothing to merge into.
///
/// A serialize-time wrapper rather than an eager [`serde_json::Value`]: with
/// no effort selected — every request before efforts existed — the typed
/// body serializes exactly as it always did, field order included, which is
/// what the wires' pinned request bytes hold. Only a request actually
/// carrying options pays the round trip through a map, whose key order no API
/// here reads.
pub(crate) fn splice_effort<'a, B: serde::Serialize>(
    options: &'a serde_json::Map<String, serde_json::Value>,
    body: &'a B,
) -> impl serde::Serialize + 'a {
    struct Spliced<'a, B> {
        options: &'a serde_json::Map<String, serde_json::Value>,
        body: &'a B,
    }

    impl<B: serde::Serialize> serde::Serialize for Spliced<'_, B> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.options.is_empty() {
                return self.body.serialize(serializer);
            }
            let own = serde_json::to_value(self.body).map_err(serde::ser::Error::custom)?;
            let serde_json::Value::Object(own) = own else {
                return own.serialize(serializer);
            };

            let mut merged = self.options.clone();
            // `Map::extend` replaces on a duplicate key: the collision rule.
            merged.extend(own);

            merged.serialize(serializer)
        }
    }

    Spliced { options, body }
}

/// Splits one message's parts into a slice per model request.
///
/// A whole turn accumulates into a single [`Message`]: every request it took
/// opens with a [`PartBody::StepStart`] marker, and everything up to the next
/// one is what the model said and called in that step. Both HTTP providers
/// encode one message per step rather than one per canonical message, because
/// the two APIs carry a call's result in the message *after* the one that made
/// it. Flattening a multi-step turn would emit every call and then every
/// result, which the API accepts — it reads as parallel tool use — but which
/// presents what the model said in a later step as having been said before the
/// earlier step's results came back. The model's reasoning would be misordered
/// against the evidence it was reasoning from, and worse the longer the turn.
///
/// This is upstream's shape. `session/message-v2.ts` keeps `step-start` parts in
/// the message it hands the AI SDK, and `convertToModelMessages` flushes one
/// assistant message — followed by one message holding that step's results — at
/// every marker it meets.
///
/// Parts before the first marker are a step of their own, so a hand-built
/// message or a transcript stored before markers existed still encodes whole.
fn steps(parts: &[Part]) -> impl Iterator<Item = &[Part]> {
    parts.split(|part| matches!(part.body, PartBody::StepStart))
}

/// Something a provider reported while answering.
///
/// The tool variants let a wire report calls without reshaping the trait; the
/// engine folds them into tool parts and executes them when the request ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The next fragment of the reply.
    TextDelta(String),
    /// The next fragment of the model's thinking.
    ReasoningDelta(String),
    /// The model's thinking, sealed by the provider for the provider.
    ///
    /// Where [`ProviderEvent::ReasoningDelta`] is thinking a person could
    /// read, this is thinking only the wire that sealed it can, handed over so
    /// the next request can hand it back
    /// (`packages/llm/test/tool-runtime.test.ts:596-605`). It becomes a
    /// [`PartBody::Reasoning`](crate::protocol::PartBody::Reasoning) and
    /// nothing else: no frontend renders it, and the engine never opens it.
    ///
    /// Reported only when the provider actually sent state. An item that
    /// arrives without any is a step whose thinking cannot be replayed, and
    /// there is nothing for the transcript to carry.
    ReasoningState {
        /// The provider's own identifier for the item, kept because it is the
        /// item's identity and two of them are one item.
        item: String,
        /// The sealed state, verbatim.
        encrypted: String,
    },
    /// The model started calling a tool.
    ToolCallStart {
        /// Correlates the call's fragments and its result.
        id: String,
        /// Tool being called.
        name: String,
    },
    /// The next fragment of a tool call's JSON arguments.
    ToolCallDelta {
        /// Call the fragment belongs to.
        id: String,
        /// Fragment of the arguments, which are only valid JSON once joined.
        json: String,
    },
    /// A tool call's arguments are complete.
    ToolCallEnd {
        /// Call that is now complete.
        id: String,
    },
    /// What the turn cost.
    Usage(Usage),
    /// The turn died part-way through.
    ///
    /// Terminal: nothing follows it. This is what keeps a body that stopped
    /// arriving from reading as a model that finished talking.
    Failed(ProviderError),
    /// The model stopped, and why.
    Finish(FinishReason),
}

/// A provider could not answer.
///
/// The variants are transport-agnostic on purpose: the same taxonomy has to fit
/// a provider that never leaves the process and one that speaks HTTP.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    /// No usable credentials for the provider.
    #[error("no usable credentials: {0}")]
    Auth(String),
    /// The request never produced a response.
    #[error("the request did not complete: {0}")]
    Transport(String),
    /// The provider answered, unsuccessfully.
    #[error("the provider answered {status}: {message}")]
    Status {
        /// HTTP status the provider returned.
        status: u16,
        /// What it said, trimmed to something a status bar can hold.
        message: String,
    },
    /// A response arrived but could not be understood.
    #[error("the response could not be parsed: {0}")]
    Parse(String),
}

impl ProviderError {
    /// Re-spells every message through `presented`'s redaction, so a wire
    /// that echoed back the credential it refused cannot hand it to the
    /// status bar or a log line.
    fn redacted(self, presented: &Presented) -> Self {
        match self {
            Self::Auth(message) => Self::Auth(presented.redact(&message)),
            Self::Transport(message) => Self::Transport(presented.redact(&message)),
            Self::Status { status, message } => Self::Status {
                status,
                message: presented.redact(&message),
            },
            Self::Parse(message) => Self::Parse(presented.redact(&message)),
        }
    }
}

/// What an in-body error object says, for the message the turn reports.
///
/// Every wire here mapped a missing `message` onto one generic sentence — "the
/// provider reported an error" — which is what a user was shown for a failure
/// whose body did carry a `type` and a `code`, and left nothing at all in the
/// log to read afterwards. Three cases, in order:
///
/// - a `message`, which is what almost every vendor sends, is used verbatim;
/// - no message but some naming — `type`, `code`, `param` — is rendered as
///   those fields, because a slug is a thing to search for where a generic
///   sentence is not;
/// - a body with neither says **so**, rather than saying nothing in words that
///   read like a message the provider chose.
///
/// Nothing is redacted here because nothing here can redact: no [`Presented`]
/// is in scope in a decoder. The masking happens at the two seams that do
/// hold the credential — [`retry::refusal`] for an HTTP refusal's body, and
/// [`shielded`] for everything a wire maps into a mid-stream failure — before
/// any of it becomes a message or a log line.
pub(super) fn reported(error: &serde_json::Value) -> String {
    // A frame shaped `{"type": "error", "error": {…}}` keeps its detail one
    // level down — the codex backend's mid-stream 500 arrived exactly so,
    // reading as `(type: error)` and nothing else until this looked inside.
    let error = match &error["error"] {
        nested @ serde_json::Value::Object(_) => nested,
        _ => error,
    };

    if let Some(message) = error["message"].as_str()
        && !message.trim().is_empty()
    {
        return message.to_owned();
    }

    let named: Vec<String> = ["type", "code", "param"]
        .into_iter()
        .filter_map(|field| {
            let value = &error[field];
            let spelled = match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Bool(flag) => flag.to_string(),
                // An array or object here is structure, not a name; inlining
                // one would put a blob where a searchable slug belongs.
                _ => return None,
            };

            (!spelled.trim().is_empty()).then(|| format!("{field}: {spelled}"))
        })
        .collect();

    if named.is_empty() {
        return "the provider reported an error and its body carried no detail".to_owned();
    }

    format!(
        "the provider reported an error with no message ({})",
        named.join(", ")
    )
}

/// A source of assistant text.
///
/// One call to [`Provider::stream`] serves one turn. `cancel` fires when the
/// user interrupts, and implementations are expected to stop reading — for an
/// HTTP provider that means abandoning the response body, which aborts the
/// request.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identifier accepted by [`PROVIDER_ENV`].
    fn id(&self) -> &str;

    /// Streams the reply to `request`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the turn cannot start at all: bad
    /// credentials, an unreachable endpoint, or a rejected request. A failure
    /// after the first fragment arrives is reported as
    /// [`ProviderEvent::Failed`] instead, so that the text already streamed is
    /// not thrown away.
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;

    /// Whether this wire can put a binary attachment of `mime` in front of the
    /// model as a native content block.
    ///
    /// The engine asks before it builds a request: a mime the wire carries is
    /// read and base64-encoded into the request's
    /// [`PartBody::File`](crate::protocol::PartBody::File), and one it does not
    /// is degraded to the file's name in text — never sent as a block the API
    /// would refuse, never dropped, never a failed turn.
    ///
    /// The default is **no**. A wire that never learned attachments — the
    /// chat-completions wire, cursor's Connect wire, a compat endpoint whose
    /// far end is anybody's guess — degrades gracefully rather than guessing
    /// at a block shape the vendor may not document.
    fn accepts_attachment(&self, mime: &str) -> bool {
        let _ = mime;
        false
    }
}

/// The credential one request presents, whatever kind of credential it is.
///
/// An API key and an OAuth access token are the same thing by the time they
/// reach here: a secret that goes into a header and that must be scrubbed out
/// of anything the provider says back. The difference between them is *where
/// they come from*, and that is [`CredentialSource`]'s business rather than this
/// type's.
///
/// The only way to read one is [`Presented::expose`], which is the single place
/// in this crate's provider code that calls `expose_secret`, so that a grep for
/// either finds every place a credential leaves the type. Everything else —
/// [`fmt::Debug`], and therefore every `tracing` field that renders a provider
/// — sees a placeholder, and the material is wiped when the last handle to it
/// drops.
#[derive(Clone)]
pub struct Presented(SecretString);

impl Presented {
    /// Wraps a credential, rejecting a blank one so that an exported-but-empty
    /// variable fails at startup rather than as a 401 mid-turn.
    fn new(secret: impl Into<SecretString>) -> Option<Self> {
        let presented = Self(secret.into());

        (!presented.expose().trim().is_empty()).then_some(presented)
    }

    /// The credential itself, for putting on the wire.
    fn expose(&self) -> &str {
        self.0.expose_secret()
    }

    /// Replaces the credential with a placeholder wherever it appears in
    /// `text`, so that a provider echoing back the credential it rejected
    /// cannot put it in an error message or a log line.
    fn redact(&self, text: &str) -> String {
        text.replace(self.expose(), "[redacted]")
    }
}

impl fmt::Debug for Presented {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Presented([redacted])")
    }
}

/// Where a provider's credential comes from, consulted once per request.
///
/// The two arms differ in exactly one way, and it is the reason this is an enum
/// rather than a [`Presented`] field: a key does not expire, so it is captured
/// at construction and every request presents the same one. An OAuth access
/// token does expire — and may have been renewed by another turn, or by another
/// ganja process, since this provider was built — so it is resolved afresh
/// immediately before each request is built.
///
/// That is the position upstream's fetch wrapper occupies: `plugin/xai.ts:487`
/// and `plugin/openai/codex.ts:341-395` both wrap the request rather than the
/// client, and `codex.ts:353` re-reads the credential on every call for exactly
/// this reason.
pub enum CredentialSource {
    /// A key, held for the life of the provider.
    Key(Presented),
    /// An OAuth credential, read from the store and renewed when it is due.
    Oauth {
        /// The provider whose credential this is, in ganja's vocabulary —
        /// [`auth::storage_key`] is what maps it to the name on disk, so this
        /// is the id a message names and never the file's key.
        provider_id: &'static str,
        /// The endpoint half of a renewal. [`auth::Refresher`] owns the rest:
        /// when to renew, holding concurrent callers to one exchange, and
        /// storing what comes back.
        refresh: Arc<dyn auth::RefreshOauth>,
    },
}

/// A credential as one request carries it.
///
/// The secret that goes in the header, and whatever else the stored record says
/// about *who* is asking. The second half exists because one provider needs it:
/// a ChatGPT credential names which of a person's accounts to bill, and
/// upstream puts it on every request beside the token
/// (`plugin/openai/codex.ts:406-408`). It is resolved in the same step as the
/// token rather than read separately, because a renewal can replace both and
/// two reads would be two answers.
struct Resolved {
    /// What authenticates the request.
    presented: Presented,
    /// The account it is billed to, where the credential names one. Always
    /// [`None`] for a key, which identifies an account by being one.
    account_id: Option<String>,
}

impl CredentialSource {
    /// The credential this request should present.
    ///
    /// For an OAuth provider this goes through [`auth::Refresher`] rather than
    /// reading the store directly, so that a step with a dozen tool calls in
    /// the air meeting the same expiry spends the refresh token **once**. With
    /// a rotating refresh token — xAI's and ChatGPT's both rotate — a second
    /// concurrent exchange presents a token the first has already spent, and
    /// the provider is right to refuse it.
    ///
    /// A credential that is not due is returned without the token endpoint
    /// being troubled at all, and `expires: 0` is upstream's "never expires"
    /// rather than "expired in 1970" — both of those are
    /// [`auth::OauthCredential::needs_refresh`]'s to decide, and neither is
    /// re-decided here.
    ///
    /// # Errors
    ///
    /// Returns whatever [`unusable`] classified the failure as: a refusal is
    /// [`ProviderError::Auth`] and an unreachable endpoint is
    /// [`ProviderError::Transport`], which are two different things to do next.
    async fn resolved(&self) -> Result<Resolved, ProviderError> {
        match self {
            Self::Key(key) => Ok(Resolved {
                presented: key.clone(),
                account_id: None,
            }),
            Self::Oauth {
                provider_id,
                refresh,
            } => {
                let credential = auth::Refresher::shared()
                    .usable(provider_id, Arc::clone(refresh))
                    .await
                    .map_err(|error| unusable(&error))?;
                let account_id = credential.account_id.clone();

                // Only the access token travels. The refresh token stays in the
                // store and never reaches a request, so it cannot reach a
                // redaction pass either — which is the point: what is not here
                // cannot leak from here.
                let presented = Presented::new(credential.access).ok_or_else(|| {
                    ProviderError::Auth(format!(
                        "the stored {provider_id} credential carries no access token; \
                         run `ganja auth login {provider_id}`"
                    ))
                })?;

                Ok(Resolved {
                    presented,
                    account_id,
                })
            }
        }
    }

    /// Just the secret, for a provider with no use for the rest.
    ///
    /// # Errors
    ///
    /// As [`resolved`](Self::resolved).
    async fn presented(&self) -> Result<Presented, ProviderError> {
        Ok(self.resolved().await?.presented)
    }
}

impl fmt::Debug for CredentialSource {
    /// Names the kind and the provider, never the material. [`Presented`] is
    /// already opaque; this exists so that the OAuth arm does not grow a
    /// derived rendering of whatever a future field holds.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(key) => formatter.debug_tuple("Key").field(key).finish(),
            Self::Oauth { provider_id, .. } => formatter
                .debug_struct("Oauth")
                .field("provider_id", provider_id)
                .finish_non_exhaustive(),
        }
    }
}

/// How a credential that could not be produced is reported to the turn.
///
/// The classification is load-bearing rather than cosmetic, because
/// [`ProviderError::is_retryable`] decides what the retry driver in
/// [`retry::send`] does with it — and a refresh sits exactly where that driver
/// applies, before the first byte of the response:
///
/// - A refusal — the refresh token is dead, or there is no OAuth credential
///   stored at all — is [`ProviderError::Auth`], which is **not** retryable.
///   Only a new login fixes it, and retrying it turns one expired grant into a
///   storm against an identity provider.
/// - A renewal that never got that far is [`ProviderError::Transport`], which
///   **is** retryable, correctly: the stored credential is untouched and trying
///   again is what fixes it.
///
/// A store that could not be read lands on the `Auth` side with the refusals.
/// It is not a network failure and repeating it changes nothing; what it needs
/// is the file repaired, which is what its message says.
///
/// Every [`auth::AuthError`] message names the provider and the command that
/// repairs it while quoting nothing out of the store or off the wire, so the
/// whole taxonomy is safe to put in front of a user verbatim.
fn unusable(error: &auth::RefreshError) -> ProviderError {
    match error.kind() {
        auth::AuthErrorKind::RefreshUnavailable => ProviderError::Transport(error.to_string()),
        auth::AuthErrorKind::NotOauth
        | auth::AuthErrorKind::Expired
        | auth::AuthErrorKind::ReauthRequired
        | auth::AuthErrorKind::Storage => ProviderError::Auth(error.to_string()),
    }
}

/// Builds the HTTP client both providers send with.
///
/// Shared so that the redirect policy cannot be forgotten by whichever provider
/// is added next: a turn is a single `POST` that no endpoint has a reason to
/// redirect, and `reqwest` only strips the headers it knows are credentials —
/// `Authorization` and friends — leaving Anthropic's `x-api-key` to be handed
/// to whatever host a 3xx names.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when no client can be built, which in
/// practice means the TLS backend failed to initialize.
fn client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ProviderError::Transport(format!("no HTTP client: {error}")))
}

/// Refuses a base URL that would put the credential on the wire in the clear.
///
/// Plain HTTP is allowed to loopback and nowhere else: the bytes never reach a
/// network there, which is what the test suite and a local inference server
/// both rely on. Everything else has to be `https`, because the key travels in
/// a header on every request.
///
/// The URL is deliberately absent from every message. A base URL is
/// configuration, and configuration is allowed to carry credentials in its
/// userinfo, so echoing it back is how a key reaches a log.
///
/// The host is compared as a parsed host and never as text. Every cheap way of
/// spelling this check is bypassable — `http://127.0.0.1.evil.com` beats a
/// prefix match, `http://127.0.0.1@evil.com` beats a substring match, and
/// `http://localhost.evil.com` beats a "starts with localhost" match — and all
/// three are ordinary hosts belonging to whoever registered them.
fn check_base_url(base_url: &str) -> Result<(), ProviderError> {
    let parsed = Url::parse(base_url)
        .map_err(|error| ProviderError::Transport(format!("the base URL is not a URL: {error}")))?;

    if reachable_in_the_clear(&parsed) {
        return Ok(());
    }

    Err(ProviderError::Transport(
        "the base URL must be https, or http to loopback; anything else puts the \
         API key on the wire in the clear"
            .to_owned(),
    ))
}

/// Whether `url` may be spoken to at all, given that the request will carry a
/// secret — an API key in a header here, a configured token in `headers` there.
///
/// Shared with `ganja_core::config`, which asks this about an MCP server's
/// endpoint. What is shared is the predicate and not the refusal: each caller
/// keeps its own parse and its own message, because the message is the part a
/// person reads and the two are about different things.
pub fn reachable_in_the_clear(url: &Url) -> bool {
    // `Url` has already done the parsing that makes this safe: whatever sits
    // before an `@` is userinfo and never reaches `host()`, and a host that
    // merely contains an address is a domain, not that address.
    let loopback = match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        // Only the exact name. RFC 6761 reserves everything under `.localhost`
        // for loopback too, but that is a promise about resolvers rather than
        // one the resolver on this machine has to keep, and a suffix match is
        // the shape of bypass this function exists to refuse.
        Some(Host::Domain(name)) => name == "localhost",
        None => false,
    };

    url.scheme() == "https" || (url.scheme() == "http" && loopback)
}

/// A base URL as it may be shown.
///
/// [`check_base_url`] treats a URL carrying credentials in its userinfo as
/// legitimate configuration, and a gateway URL is somewhere people put a token
/// in a query string too. Both are therefore credentials this crate holds for
/// the length of a session, and neither may reach a rendering — which is what
/// a provider's [`fmt::Debug`] is, and what every `tracing` field holding one
/// becomes.
///
/// Shared for the same reason [`client`] is: two copies of a redaction are one
/// copy too many.
fn shown_base_url(base_url: &str) -> String {
    let Ok(mut parsed) = Url::parse(base_url) else {
        // Nothing here parsed, so there is no structure to lift a credential
        // out of and no way to know whether what is left holds one.
        return "[unparseable]".to_owned();
    };

    // Each of these can carry a secret, and none of them is needed to tell one
    // endpoint from another: what identifies it is the scheme, host, port and
    // path that survive.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);

    parsed.into()
}

/// Turns one provider's frames into events.
///
/// Implementations are the only provider-specific part of an HTTP turn.
trait Mapper: Send + 'static {
    /// Maps `frame`, appending whatever it means to `events`.
    ///
    /// Appending [`ProviderEvent::Finish`] or [`ProviderEvent::Failed`] ends
    /// the stream; anything the mapper would have produced afterwards is
    /// dropped.
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>);

    /// Reports a body that ended without the provider saying it was done.
    ///
    /// The default is the safe reading — a truncated body is a failure, not a
    /// short answer — which a provider whose terminator is optional overrides.
    fn truncated(&mut self, events: &mut Vec<ProviderEvent>) {
        events.push(ProviderEvent::Failed(ProviderError::Transport(
            "the response body ended before the model finished".to_owned(),
        )));
    }
}

/// Whether nothing may follow `event`.
fn is_terminal(event: &ProviderEvent) -> bool {
    matches!(event, ProviderEvent::Finish(_) | ProviderEvent::Failed(_))
}

/// Sends `request` and turns the event-stream body it answers with into
/// provider events.
///
/// # Errors
///
/// Returns the [`ProviderError`] for a request that never started streaming.
/// A cancelled turn is not one of those: it yields an empty stream, because
/// the engine reads a stream that ends after a cancel as `Cancelled`, and a
/// user who pressed Esc has not hit an error.
async fn open<M: Mapper + Default>(
    client: &reqwest::Client,
    request: reqwest::Request,
    presented: &Presented,
    cancel: CancellationToken,
) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
    // Taken before the request is moved into the send, so the answer can be
    // read against the endpoint it came from. Each wire has already logged the
    // provider and model it chose; this is the one seam that knows the URL.
    let endpoint = endpoint(request.url());

    // A body that cannot be replayed cannot ride D475 either: it is sent
    // exactly once, through the same single-shot arm `retry::send` gives it.
    if request.try_clone().is_none() {
        let response = match retry::send(client, request, presented, &cancel).await {
            Ok(response) => response,
            Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
            Err(error) => return Err(error),
        };

        return Ok(shielded(
            events(response.bytes_stream().boxed(), cancel, M::default()),
            presented.clone(),
            endpoint,
        ));
    }

    let mut attempt = 0;
    let settled = loop {
        let replay = request
            .try_clone()
            .expect("a body that cannot replay took the single-shot arm above");
        let response = match retry::send(client, replay, presented, &cancel).await {
            Ok(response) => response,
            Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
            // A refusal is already logged with its body by `retry::refusal`,
            // and a transport failure carries its own cause chain.
            Err(error) => return Err(error),
        };
        tracing::debug!(
            status = response.status().as_u16(),
            endpoint,
            "the provider answered"
        );

        // A fresh mapper per attempt: a retried turn starts over, and a
        // mapper that remembered the dead attempt's half-open state would
        // splice two turns together.
        match peeked(events(
            response.bytes_stream().boxed(),
            cancel.clone(),
            M::default(),
        ))
        .await
        {
            Peeked::Content { prefix, rest } => break stream::iter(prefix).chain(rest).boxed(),
            Peeked::Ended { prefix } => break stream::iter(prefix).boxed(),
            Peeked::Died { mut prefix, error } => {
                if !reopens(attempt, &error) {
                    prefix.push(ProviderEvent::Failed(error));
                    break stream::iter(prefix).boxed();
                }

                attempt += 1;
                // The attempt count only: the message may quote a credential,
                // and it reaches the log redacted through `shielded` when the
                // last attempt hands its failure on.
                tracing::debug!(
                    attempt,
                    "the turn died before it said anything; reopening it"
                );
                tokio::select! {
                    () = cancel.cancelled() => return Ok(stream::empty().boxed()),
                    () = tokio::time::sleep(retry::stream_backoff(attempt)) => {}
                }
            }
        }
    };

    Ok(shielded(settled, presented.clone(), endpoint))
}

/// How many times a turn that has shown nothing may be reopened (**D475**,
/// `in-body-overload-retry`).
///
/// The standing rule — never retry once the provider has started streaming —
/// exists because a replay would duplicate or discard text somebody has
/// already seen. An in-body failure that arrives *before the first content
/// event* has shown nothing to duplicate: the codex backend reports its
/// overload exactly so (HTTP 200, then an `error` frame carrying "servers are
/// currently overloaded"), and refusing to retry it turned every capacity
/// blip into a dead turn. So a retryable failure on a still-empty transcript
/// is reopened, on the ported backoff schedule, at most this many times; the
/// moment any content arrives the standing rule owns the stream again. A
/// deliberate divergence from the posture `retry`'s module doc pins, recorded
/// there too.
const STREAM_RETRIES: u32 = 3;

/// Whether the transcript would show `event` — the boundary [`STREAM_RETRIES`]
/// may never cross.
fn matters(event: &ProviderEvent) -> bool {
    match event {
        ProviderEvent::TextDelta(_)
        | ProviderEvent::ReasoningDelta(_)
        | ProviderEvent::ReasoningState { .. }
        | ProviderEvent::ToolCallStart { .. }
        | ProviderEvent::ToolCallDelta { .. }
        | ProviderEvent::ToolCallEnd { .. } => true,
        ProviderEvent::Usage(_) | ProviderEvent::Finish(_) | ProviderEvent::Failed(_) => false,
    }
}

/// What peeking a fresh attempt's stream settled on.
enum Peeked {
    /// Content arrived; `prefix` ends with the event that proved it, and
    /// `rest` is everything not yet read.
    Content {
        prefix: Vec<ProviderEvent>,
        rest: BoxStream<'static, ProviderEvent>,
    },
    /// The stream ended without content and without failing.
    Ended { prefix: Vec<ProviderEvent> },
    /// The stream failed while the transcript was still empty: `prefix`
    /// holds only non-content events, and `error` is not among them.
    Died {
        prefix: Vec<ProviderEvent>,
        error: ProviderError,
    },
}

/// Reads `stream` up to its first content event, failure, or end (**D475**).
///
/// Buffered events are re-emitted in order by the caller, so a peeked stream
/// is indistinguishable from an untouched one.
async fn peeked(mut stream: BoxStream<'static, ProviderEvent>) -> Peeked {
    let mut prefix = Vec::new();

    loop {
        match stream.next().await {
            Some(ProviderEvent::Failed(error)) => return Peeked::Died { prefix, error },
            Some(event) => {
                let content = matters(&event);
                prefix.push(event);
                if content {
                    return Peeked::Content {
                        prefix,
                        rest: stream,
                    };
                }
            }
            None => return Peeked::Ended { prefix },
        }
    }
}

/// Whether a turn that died empty on `attempt` (zero-based) is worth
/// reopening (**D475**): only while retries remain, and only for a failure
/// that sending again could plausibly outlive.
fn reopens(attempt: u32, error: &ProviderError) -> bool {
    attempt < STREAM_RETRIES && error.is_retryable()
}

/// Redacts every mid-stream failure before it leaves the wire.
///
/// The HTTP refusal path masks its body through [`retry::refusal`], but an
/// error that arrives *inside* a 200 stream is mapped by each wire's own
/// `failure()`, which holds no [`Presented`] to mask with — and a
/// config-declared endpoint or a gateway can echo the credential it rejected
/// in exactly such a frame. This is the one seam every wire's events flow
/// through that still holds the credential, so the redaction and the one log
/// line both live here rather than being re-plumbed into three decoders.
fn shielded(
    events: BoxStream<'static, ProviderEvent>,
    presented: Presented,
    endpoint: String,
) -> BoxStream<'static, ProviderEvent> {
    events
        .map(move |event| match event {
            ProviderEvent::Failed(error) => {
                let error = error.redacted(&presented);
                tracing::warn!(%error, endpoint, "the turn died mid-stream");
                ProviderEvent::Failed(error)
            }
            other => other,
        })
        .boxed()
}

/// Where a request went, with everything that could be a credential removed.
///
/// Scheme, host, port and path — never the query string and never the
/// userinfo. A base URL is allowed to carry credentials in its userinfo and a
/// query string is a documented place to put a token (`?auth_token=`, which
/// `ganja-serve` refuses to log for the same reason), so a diagnostic that
/// rendered either would be a diagnostic that writes secrets to a file.
pub(super) fn endpoint(url: &Url) -> String {
    let authority = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        // A URL with no host is not one of ours; naming the scheme alone still
        // says which wire was reached without inventing an authority.
        (None, _) => String::new(),
    };

    format!("{}://{authority}{}", url.scheme(), url.path())
}

/// Drives `chunks` through the frame splitter and `mapper`.
///
/// Split out from [`open`] so that the fixture suite exercises exactly the
/// pipeline a live turn runs, minus the socket.
fn events<S, C, E, M>(
    chunks: S,
    cancel: CancellationToken,
    mapper: M,
) -> BoxStream<'static, ProviderEvent>
where
    S: Stream<Item = Result<C, E>> + Send + Unpin + 'static,
    C: AsRef<[u8]> + Send + 'static,
    E: fmt::Display + Send + 'static,
    M: Mapper,
{
    /// Everything the fold carries between polls.
    struct State<F, M> {
        frames: F,
        mapper: M,
        cancel: CancellationToken,
        /// Events one frame produced, not yet handed out.
        ready: VecDeque<ProviderEvent>,
        /// Reused so that mapping a frame does not allocate.
        scratch: Vec<ProviderEvent>,
        done: bool,
    }

    stream::unfold(
        State {
            frames: sse::frames(chunks).boxed(),
            mapper,
            cancel,
            ready: VecDeque::new(),
            scratch: Vec::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                // Checked before handing out a buffered event as well as before
                // pulling a new one, so that a cancel cannot be outrun by
                // frames that were already parsed.
                if state.cancel.is_cancelled() {
                    return None;
                }

                if let Some(event) = state.ready.pop_front() {
                    if is_terminal(&event) {
                        state.done = true;
                        state.ready.clear();
                    }

                    return Some((event, state));
                }

                if state.done {
                    return None;
                }

                let frame = tokio::select! {
                    biased;
                    () = state.cancel.cancelled() => return None,
                    frame = state.frames.next() => frame,
                };

                state.scratch.clear();
                match frame {
                    Some(Ok(frame)) => state.mapper.frame(&frame, &mut state.scratch),
                    Some(Err(error)) => {
                        state.done = true;
                        state
                            .scratch
                            .push(ProviderEvent::Failed(ProviderError::Transport(
                                error.to_string(),
                            )));
                    }
                    None => {
                        state.done = true;
                        state.mapper.truncated(&mut state.scratch);
                    }
                }

                state.ready.extend(state.scratch.drain(..));
            }
        },
    )
    .boxed()
}

/// Feeds a recorded transcript through the pipeline a live turn runs.
///
/// The fixture suite exists to prove the mapping, so it goes through the real
/// splitter rather than handing frames to a mapper directly. Delivering the
/// whole transcript as one chunk is the worst case for cancellation — every
/// frame is already parsed and waiting — which is exactly what the cancel test
/// wants to prove is still stoppable.
#[cfg(test)]
fn replay<M: Mapper>(
    transcript: &'static str,
    cancel: CancellationToken,
    mapper: M,
) -> BoxStream<'static, ProviderEvent> {
    events(
        stream::iter([Ok::<&[u8], std::convert::Infallible>(transcript.as_bytes())]),
        cancel,
        mapper,
    )
}

/// Looks up the API key for `provider_id`.
///
/// [`auth::credential_for`] reads the same environment variables this used to
/// read directly, and layers the stored `auth.json` underneath them, so an
/// exported key still overrides a stored one for a single run. It reads only
/// keys; a provider whose credential is a pair of OAuth tokens is served by
/// [`CredentialSource::Oauth`] instead, which is a different lookup because it is a
/// different thing to look up.
///
/// A store that could not be read is [`Err`], not [`Ok(None)`]: "you have no
/// credential" and "you have one and it was refused" need different things
/// from the person reading the message, and only the second can say what to
/// fix. Reporting it here rather than logging it is what gets the reason in
/// front of someone who is looking at a terminal, not a log file.
pub fn key_for(provider_id: &str) -> Result<Option<Presented>, ProviderError> {
    match auth::credential_for(provider_id) {
        Ok(credential) => Ok(credential.and_then(|credential| Presented::new(credential.api_key))),
        // Every `AuthError` names the file and the command that repairs it
        // while quoting nothing out of it — the parse failure deliberately
        // throws away serde's message because it would echo the value — so the
        // whole taxonomy is safe to put in front of a user verbatim.
        Err(error) => Err(ProviderError::Auth(error.to_string())),
    }
}

/// The key for `provider_id`, or the error a startup should die on.
fn require_key(provider_id: &str, variable: &str) -> Result<Presented, ProviderError> {
    key_for(provider_id)?.ok_or_else(|| {
        ProviderError::Auth(format!(
            "{variable} is unset; export it or run `ganja auth login`"
        ))
    })
}

/// The credential a config-named endpoint presents.
///
/// Two places, in the order the rest of this module reads them: the variable
/// the entry names, then the credential store under the entry's own id.
/// [`auth::storage_key`] passes an id it has no alias for through unchanged,
/// so `ganja auth login local-llama` writes exactly where this reads — the
/// open-set half of a store whose builtin half is a fixed table.
///
/// Only a key. [`CredentialSource::Oauth`] carries a `&'static str` provider id and
/// a refresh endpoint this build implements per provider, and neither is
/// something a config file can supply: a configured endpoint is
/// key-authenticated, and an OAuth one would be a provider rather than an
/// entry.
///
/// # Errors
///
/// Returns [`ProviderError::Auth`] when neither place has one — naming both,
/// because which one is missing is the whole of what the reader has to fix —
/// or when the store exists and could not be read.
pub fn configured_key(id: &str, key_env: Option<&str>) -> Result<Presented, ProviderError> {
    if let Some(variable) = key_env
        && let Some(exported) = setting(variable)
        && let Some(key) = Presented::new(exported)
    {
        return Ok(key);
    }

    key_for(id)?.ok_or_else(|| {
        ProviderError::Auth(match key_env {
            Some(variable) => format!(
                "no credential for the configured provider `{id}`; export {variable} \
                 or run `ganja auth login {id}`"
            ),
            None => format!(
                "no credential for the configured provider `{id}`; run \
                 `ganja auth login {id}`, or name the variable holding it as \
                 `key_env` in its config entry"
            ),
        })
    })
}

/// The `headers` an entry declared, as a request carries them.
///
/// A name or a value the HTTP layer cannot encode fails at startup rather than
/// at the first prompt. The message names the **header** and never its value:
/// `headers` is exactly where a configured endpoint's token goes, which is
/// also why [`check_base_url`]'s rule covers an entry that declares one.
pub fn configured_headers(
    id: &str,
    declared: &BTreeMap<String, String>,
) -> Result<reqwest::header::HeaderMap, ProviderError> {
    let mut headers = reqwest::header::HeaderMap::with_capacity(declared.len());
    for (name, value) in declared {
        let name: reqwest::header::HeaderName = name.parse().map_err(|_| {
            ProviderError::Transport(format!(
                "provider `{id}` declares a header named {name:?}, which is not a header name"
            ))
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
            ProviderError::Transport(format!(
                "provider `{id}`'s `{name}` header holds bytes a request cannot carry"
            ))
        })?;
        headers.insert(name, value);
    }

    Ok(headers)
}

/// The model a config spelling names, when `provider_id` serves it.
///
/// Config spells a model `"provider/model"` — that is what `model`,
/// `small_model` and an agent's own `model` all carry, and what
/// `import-opencode` writes — while a catalog id is the bare half after the
/// slash and the provider is fixed at construction. Handing the whole spelling
/// to [`serves`] therefore asks whether `anthropic` serves a model called
/// `anthropic/claude-…`, which it does not: the answer is no wherever the
/// catalog knows the provider, and an unexamined yes wherever it does not,
/// which would put the prefix on the wire. Splitting first is what makes the
/// documented spelling work in both places.
pub fn adopt(provider_id: &str, spelled: &str) -> Option<String> {
    let model = spelled.split_once('/').map_or(spelled, |(_, rest)| rest);

    serves(provider_id, model).then(|| model.to_owned())
}

/// Whether the provider `provider_id` serves a model called `model`.
///
/// The catalog is the only thing that knows, and it does not know every
/// provider — the built-in fake one is not in it, and neither is whatever a test
/// drives. A provider the catalog says nothing about cannot be contradicted, so
/// any model it is asked for is taken at its word; refusing every switch there
/// would make the command untestable in exactly the runs that are cheapest to
/// run.
pub fn serves(provider_id: &str, model: &str) -> bool {
    if !catalog::carries(provider_id) {
        return !model.trim().is_empty();
    }

    catalog::models().any(|known| known.provider_id == provider_id && known.id == model)
}

/// Reads `variable`, treating an empty value as unset.
///
/// Public because selection reads the same variables under the same rule: an
/// exported-but-empty `GANJA_MODEL` names no model, exactly as an
/// exported-but-empty `ANTHROPIC_BASE_URL` names no endpoint. One reader keeps
/// the two halves from disagreeing about what blank means.
pub fn setting(variable: &str) -> Option<String> {
    env::var(variable)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::{StreamExt as _, stream};
    use tokio_util::sync::CancellationToken;

    use super::{
        CredentialSource, Mapper, Peeked, Presented, ProviderError, ProviderEvent, check_base_url,
        configured_headers, endpoint, events, peeked, reopens, reported, responses, retry,
        shielded, shown_base_url, sse::Frame, unusable,
    };
    use crate::{auth, protocol::FinishReason};

    /// The three shapes an in-body error object arrives in, and the rule that
    /// none of them may be answered with a sentence that says nothing.
    /// D475's boundary, from the peek's side: content settles the stream,
    /// an empty failure reads as a death worth judging, and a clean end
    /// keeps what it buffered.
    #[test]
    fn peeking_settles_on_the_first_content_event_and_hands_back_the_rest() {
        use futures::StreamExt as _;

        let settled = futures::executor::block_on(peeked(
            stream::iter(vec![
                ProviderEvent::Usage(crate::protocol::Usage::default()),
                ProviderEvent::TextDelta("first".to_owned()),
                ProviderEvent::TextDelta("second".to_owned()),
            ])
            .boxed(),
        ));
        let Peeked::Content { prefix, rest } = settled else {
            panic!("text settles the stream");
        };
        assert_eq!(prefix.len(), 2, "the prefix ends with the proving event");
        assert!(matches!(prefix[1], ProviderEvent::TextDelta(ref text) if text == "first"));
        let rest: Vec<ProviderEvent> = futures::executor::block_on(rest.collect());
        assert_eq!(rest.len(), 1, "nothing past the peek is consumed");

        let died = futures::executor::block_on(peeked(
            stream::iter(vec![ProviderEvent::Failed(ProviderError::Status {
                status: 500,
                message: "overloaded".to_owned(),
            })])
            .boxed(),
        ));
        assert!(
            matches!(died, Peeked::Died { ref prefix, .. } if prefix.is_empty()),
            "a failure on an empty transcript is a death, not content"
        );

        let ended = futures::executor::block_on(peeked(
            stream::iter(vec![
                ProviderEvent::Usage(crate::protocol::Usage::default()),
                ProviderEvent::Finish(FinishReason::Completed),
            ])
            .boxed(),
        ));
        assert!(
            matches!(ended, Peeked::Ended { ref prefix } if prefix.len() == 2),
            "a clean end keeps what it buffered"
        );
    }

    /// D475's bound and its classification: three reopenings for a transient
    /// death, none for a fourth and none for a failure a retry cannot fix.
    #[test]
    fn an_empty_turns_transient_death_is_reopened_at_most_three_times() {
        let overloaded = ProviderError::Status {
            status: 500,
            message: "Our servers are currently overloaded.".to_owned(),
        };
        assert!(reopens(0, &overloaded) && reopens(1, &overloaded) && reopens(2, &overloaded));
        assert!(
            !reopens(3, &overloaded),
            "the fourth death is the turn's answer"
        );
        assert!(
            !reopens(0, &ProviderError::Auth("expired".to_owned())),
            "a failure retrying cannot fix is never reopened"
        );

        // The ported schedule, headerless: 2s, 4s, 8s, capped far later.
        assert_eq!(retry::stream_backoff(1), std::time::Duration::from_secs(2));
        assert_eq!(retry::stream_backoff(2), std::time::Duration::from_secs(4));
        assert_eq!(retry::stream_backoff(3), std::time::Duration::from_secs(8));
        assert_eq!(retry::stream_backoff(64), retry::MAX_DELAY);
    }

    /// The mid-stream half of the credential rule: an in-body error frame is
    /// mapped by a decoder holding no [`Presented`], so [`shielded`] is where
    /// the mask goes on — and a message quoting the refused credential must
    /// leave the wire wearing it.
    #[test]
    fn a_mid_stream_failure_cannot_carry_the_credential_it_quoted() {
        use futures::StreamExt as _;

        let presented = Presented::new("sk-canary-0123456789").expect("a credential");
        let failures = stream::iter(vec![
            ProviderEvent::Failed(ProviderError::Status {
                status: 500,
                message: "the key sk-canary-0123456789 was rejected".to_owned(),
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ])
        .boxed();

        let events: Vec<ProviderEvent> = futures::executor::block_on(
            shielded(failures, presented, "https://example.test/v1".to_owned()).collect(),
        );
        let ProviderEvent::Failed(ProviderError::Status { message, .. }) = &events[0] else {
            panic!("the failure survives the shield: {events:?}");
        };
        assert!(
            !message.contains("sk-canary-0123456789") && message.contains("[redacted]"),
            "the credential must not leave the wire: {message}"
        );
        assert!(
            matches!(events[1], ProviderEvent::Finish(_)),
            "everything that is not a failure passes untouched"
        );
    }

    #[test]
    fn an_error_body_is_reported_as_whatever_it_actually_carried() {
        assert_eq!(
            reported(&serde_json::json!({"message": "rate limited", "code": "429"})),
            "rate limited",
            "a message is the provider's own words and wins outright"
        );

        // The shape that produced the report this exists for: a status bar
        // reading "the provider answered 500: the provider reported an error"
        // over a body that named the failure perfectly well.
        let named = reported(&serde_json::json!({
            "type": "server_error",
            "code": "model_overloaded",
            "param": serde_json::Value::Null,
        }));
        assert!(
            named.contains("type: server_error") && named.contains("code: model_overloaded"),
            "a slug is a thing to search for where a generic sentence is not: {named}"
        );
        assert!(
            !named.contains("param"),
            "a null field is not detail, and rendering it as one is noise: {named}"
        );

        // A number where the schema says string, because `code` is a number on
        // at least one wire here and a body is not a contract.
        assert!(reported(&serde_json::json!({"code": 503})).contains("code: 503"));

        // Structure where the schema says string: skipped rather than inlined,
        // so a hostile or malformed body cannot put a blob in the message.
        let structured = reported(&serde_json::json!({
            "type": {"nested": "object"},
            "code": "model_overloaded",
        }));
        assert!(
            structured.contains("code: model_overloaded") && !structured.contains("nested"),
            "an object-valued field is not a name: {structured}"
        );

        // The wrapped shape the codex backend's mid-stream 500 wore: the
        // detail lives one level down, and reading only the wrapper renders
        // the useless `(type: error)`.
        assert_eq!(
            reported(&serde_json::json!({
                "type": "error",
                "error": {"type": "server_error", "message": "boom"},
            })),
            "boom",
            "a nested error object's message outranks the wrapper's naming"
        );
        let wrapped = reported(&serde_json::json!({
            "type": "error",
            "error": {"code": "overloaded"},
        }));
        assert!(
            wrapped.contains("code: overloaded") && !wrapped.contains("type: error"),
            "a nested error object's naming outranks the wrapper's: {wrapped}"
        );

        for empty in [
            serde_json::json!({}),
            serde_json::json!({"message": "   "}),
            serde_json::Value::Null,
        ] {
            assert_eq!(
                reported(&empty),
                "the provider reported an error and its body carried no detail",
                "a body with nothing in it has to say so, not sound like a message"
            );
        }
    }

    /// A log line is a file on disk, so the one field of a URL that is allowed
    /// to carry a credential — and the one that is a documented place to put a
    /// token — must never reach it.
    #[test]
    fn a_logged_endpoint_carries_neither_userinfo_nor_a_query_string() {
        let url = reqwest::Url::parse(
            "https://someone:sk-test-canary-XYZ@api.example.com:8443/v1/responses\
             ?auth_token=sk-test-canary-ABC",
        )
        .expect("a parseable URL");

        let rendered = endpoint(&url);

        assert_eq!(rendered, "https://api.example.com:8443/v1/responses");
        assert!(
            !rendered.contains("canary"),
            "a credential reached a log line: {rendered}"
        );
    }

    /// `headers` is where a configured endpoint's token goes, so a refusal
    /// about one may name the header and never its value.
    #[test]
    fn a_header_a_request_cannot_carry_is_refused_by_name_and_not_by_value() {
        let mut declared = super::BTreeMap::new();
        declared.insert("x-route".to_owned(), "gpu-0".to_owned());
        let carried = configured_headers("local-llama", &declared).expect("an ordinary header");
        assert_eq!(carried["x-route"], "gpu-0");

        let mut refused = super::BTreeMap::new();
        refused.insert(
            "x authorization".to_owned(),
            "sk-test-canary-XYZ".to_owned(),
        );
        let error = configured_headers("local-llama", &refused)
            .expect_err("a space is not legal in a header name");
        let rendered = format!("{error} / {error:?}");
        assert!(rendered.contains("x authorization"), "{rendered}");
        assert!(
            !rendered.contains("sk-test-canary-XYZ"),
            "the value reached the refusal: {rendered}"
        );

        let mut unencodable = super::BTreeMap::new();
        unencodable.insert("x-route".to_owned(), "sk-test-canary-XYZ\n".to_owned());
        let error = configured_headers("local-llama", &unencodable)
            .expect_err("a newline cannot travel in a header value");
        let rendered = format!("{error} / {error:?}");
        assert!(rendered.contains("x-route"), "{rendered}");
        assert!(
            !rendered.contains("sk-test-canary-XYZ"),
            "the value reached the refusal: {rendered}"
        );
    }

    /// The value the subscription backend actually hands over, held to the one
    /// property that makes it correct. `openai_provider` is what pairs the two,
    /// and `responses_wire.rs` is where that pairing is observed with a store
    /// and an environment behind it.
    #[test]
    fn the_subscription_backends_default_is_one_that_backend_serves() {
        assert!(
            responses::serves(responses::SUBSCRIPTION_DEFAULT),
            "a seat that cannot run its own default cannot take a turn at all"
        );
    }

    /// Emits whatever a frame's data spells, so that the plumbing can be
    /// tested without a provider's JSON in the way.
    struct Echo;

    impl Mapper for Echo {
        fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
            match frame.data.as_str() {
                "done" => {
                    events.push(ProviderEvent::Usage(crate::protocol::Usage::default()));
                    events.push(ProviderEvent::Finish(FinishReason::Completed));
                    events.push(ProviderEvent::TextDelta("after the end".to_owned()));
                }
                data => events.push(ProviderEvent::TextDelta(data.to_owned())),
            }
        }
    }

    /// Feeds `chunks` through the real pipeline.
    fn pipeline(
        chunks: Vec<&'static str>,
        cancel: CancellationToken,
    ) -> impl futures::Stream<Item = ProviderEvent> {
        events(
            stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok::<&[u8], Infallible>(chunk.as_bytes()))
                    .collect::<Vec<_>>(),
            ),
            cancel,
            Echo,
        )
    }

    #[test]
    fn a_credential_never_renders_itself() {
        let key = Presented::new("sk-test-canary-XYZ").expect("a non-blank key");

        assert_eq!(format!("{key:?}"), "Presented([redacted])");
        assert_eq!(
            key.redact("rejected sk-test-canary-XYZ, sorry"),
            "rejected [redacted], sorry"
        );
        assert_eq!(key.expose(), "sk-test-canary-XYZ");
        assert!(Presented::new("   ").is_none(), "a blank key is not a key");
        assert!(Presented::new("").is_none());

        // The same has to hold of the source a request resolves one from, or a
        // provider's own `Debug` — which is what every `tracing` field holding
        // one becomes — would print what the type it wraps refuses to.
        let held = CredentialSource::Key(key);
        assert_eq!(format!("{held:?}"), "Key(Presented([redacted]))");
        assert!(!format!("{held:?}").contains("sk-test-canary-XYZ"));
    }

    /// A dead refresh token and a token endpoint that could not be reached are
    /// two different situations, and the difference is not a wording choice:
    /// [`ProviderError::is_retryable`] is what the retry driver reads, and a
    /// refresh sits exactly where that driver applies. Classifying a refusal as
    /// transport turns one expired grant into a retry storm against an identity
    /// provider; classifying an unreachable endpoint as auth sends someone
    /// whose network dropped through a browser login they did not need.
    #[test]
    fn only_a_refusal_is_worth_a_new_login_and_only_a_reachable_failure_is_worth_retrying() {
        let refused = unusable(
            &auth::AuthError::ReauthRequired {
                provider_id: "grok".to_owned(),
                reason: "HTTP 401, invalid_grant".to_owned(),
            }
            .into(),
        );
        let unreachable = unusable(
            &auth::AuthError::RefreshUnavailable {
                provider_id: "grok".to_owned(),
                reason: "connection refused".to_owned(),
            }
            .into(),
        );

        assert!(
            matches!(refused, ProviderError::Auth(_)),
            "a dead refresh token is not a transport failure: {refused:?}"
        );
        assert!(
            !refused.is_retryable(),
            "retrying a refused grant is a storm against an identity provider"
        );
        assert!(
            format!("{refused}").contains("ganja auth login grok"),
            "the message is what a status bar shows, and only a login fixes this: {refused}"
        );

        assert!(
            matches!(unreachable, ProviderError::Transport(_)),
            "an endpoint that never answered has not refused anything: {unreachable:?}"
        );
        assert!(
            unreachable.is_retryable(),
            "trying again is exactly what fixes a refresh that could not be reached"
        );

        // The rest of the taxonomy is a credential that has to be replaced or a
        // file that has to be repaired, and repeating the request fixes
        // neither.
        for error in [
            auth::AuthError::NotOauth {
                provider_id: "grok".to_owned(),
                found: "an API key",
            },
            auth::AuthError::Expired {
                provider_id: "grok".to_owned(),
            },
        ] {
            let classified = unusable(&error.into());

            assert!(
                matches!(classified, ProviderError::Auth(_)) && !classified.is_retryable(),
                "{classified:?} should be a non-retryable auth failure"
            );
        }
    }

    /// The key travels in a header on every request, so the transport is what
    /// decides who else gets to read it. Loopback is exempt because the bytes
    /// never reach a network — which is what the test suite and a local
    /// inference server both depend on.
    #[test]
    fn only_https_or_loopback_may_carry_a_key() {
        let allowed = [
            "https://api.anthropic.com",
            "https://gateway.example/v1",
            "http://127.0.0.1:8080",
            // The whole 127/8 block is loopback, not just the one address.
            "http://127.10.20.30:1234",
            "http://[::1]:8080/v1",
            "http://localhost:11434/v1",
            // Userinfo is legal configuration, and does not change the hop.
            "http://ganja:secret@127.0.0.1:8080",
        ];
        // Every one of these is an ordinary host belonging to whoever
        // registered it, and every one of them defeats some cheaper spelling of
        // this check: a prefix match, a substring match, a suffix match, or a
        // look at the URL rather than at its host.
        let refused = [
            "http://api.anthropic.com",
            "http://192.168.1.10:8080",
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1@evil.com",
            "http://localhost@evil.com",
            "http://localhost.evil.com",
            "http://evil.com/127.0.0.1",
            "http://evil.com/?host=localhost",
            "http://evil.com#127.0.0.1",
            "http://notlocalhost",
            // An IPv4-mapped IPv6 address does reach loopback, but `is_loopback`
            // is only true of `::1`; refusing it fails in the safe direction.
            "http://[::ffff:127.0.0.1]",
            "ftp://127.0.0.1",
            "file:///etc/passwd",
            "not a url at all",
            "",
        ];

        for base_url in allowed {
            assert!(
                check_base_url(base_url).is_ok(),
                "{base_url} should be usable"
            );
        }
        for base_url in refused {
            let error = check_base_url(base_url)
                .expect_err(&format!("{base_url} should not be handed a key"));

            assert!(
                matches!(error, ProviderError::Transport(_)),
                "{base_url}: got {error:?}"
            );
            // A base URL is allowed to carry credentials in its userinfo, so
            // the refusal must describe the rule rather than quote the URL.
            assert!(
                !format!("{error} / {error:?}").contains(base_url) || base_url.is_empty(),
                "{base_url} was echoed back by its own refusal"
            );
        }
    }

    /// What a base URL may carry into a rendering, and what it may not. The
    /// stripped parts are the ones a credential fits in; the kept parts are the
    /// ones that say which endpoint this is.
    #[test]
    fn a_shown_base_url_keeps_the_endpoint_and_drops_the_secrets() {
        let cases = [
            ("https://api.anthropic.com", "https://api.anthropic.com/"),
            (
                "https://ganja:secret@gateway.invalid:8443/v1",
                "https://gateway.invalid:8443/v1",
            ),
            // A token in a query string is a real shape for a gateway URL.
            (
                "https://gateway.invalid/v1?token=secret",
                "https://gateway.invalid/v1",
            ),
            (
                "https://gateway.invalid/v1#secret",
                "https://gateway.invalid/v1",
            ),
            // Userinfo with no password at all still names an account.
            ("https://secret@gateway.invalid", "https://gateway.invalid/"),
            ("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080/v1"),
            // Nothing parsed, so nothing can be said to be safe.
            ("not a url at all", "[unparseable]"),
            ("", "[unparseable]"),
        ];

        for (base_url, expected) in cases {
            assert_eq!(shown_base_url(base_url), expected, "showing {base_url}");
        }
    }

    #[tokio::test]
    async fn nothing_survives_a_terminal_event() {
        let seen: Vec<ProviderEvent> = pipeline(
            vec!["data: hi\n\ndata: done\n\ndata: more\n\n"],
            CancellationToken::new(),
        )
        .collect()
        .await;

        assert_eq!(
            seen,
            vec![
                ProviderEvent::TextDelta("hi".to_owned()),
                ProviderEvent::Usage(crate::protocol::Usage::default()),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "a finish ends the stream, including events from the same frame"
        );
    }

    #[tokio::test]
    async fn a_body_that_just_stops_fails_rather_than_completing() {
        let seen: Vec<ProviderEvent> = pipeline(vec!["data: hi\n\n"], CancellationToken::new())
            .collect()
            .await;

        assert_eq!(
            seen,
            vec![
                ProviderEvent::TextDelta("hi".to_owned()),
                ProviderEvent::Failed(ProviderError::Transport(
                    "the response body ended before the model finished".to_owned()
                )),
            ]
        );
    }

    #[tokio::test]
    async fn a_transport_error_mid_body_becomes_a_failure() {
        let chunks = stream::iter(vec![
            Ok::<&[u8], &str>(b"data: hi\n\n".as_slice()),
            Err("connection reset by peer"),
        ]);
        let seen: Vec<ProviderEvent> = events(chunks, CancellationToken::new(), Echo)
            .collect()
            .await;

        assert_eq!(
            seen,
            vec![
                ProviderEvent::TextDelta("hi".to_owned()),
                ProviderEvent::Failed(ProviderError::Transport(
                    "connection reset by peer".to_owned()
                )),
            ]
        );
    }

    #[tokio::test]
    async fn a_cancelled_stream_stops_without_reporting_a_failure() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(pipeline(
            vec!["data: one\n\ndata: two\n\ndata: three\n\n"],
            cancel.clone(),
        ));

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::TextDelta("one".to_owned()))
        );
        cancel.cancel();

        assert_eq!(
            stream.next().await,
            None,
            "a cancelled stream ends, and never with a failure the engine would report"
        );
    }

    /// The merge rule every wire's send site leans on, stated once at the
    /// helper: the effort map goes in first, so on a shared key the wire's
    /// own field is what survives.
    #[test]
    fn a_spliced_body_keeps_the_wires_fields_over_the_efforts() {
        let options = serde_json::json!({"model": "theirs", "extra": 1})
            .as_object()
            .cloned()
            .expect("the fixture options are an object");
        let body = serde_json::json!({"model": "ours", "stream": true});

        let merged = serde_json::to_value(super::splice_effort(&options, &body))
            .expect("a spliced body serializes");

        assert_eq!(
            merged,
            serde_json::json!({"model": "ours", "stream": true, "extra": 1})
        );

        let untouched = serde_json::to_value(super::splice_effort(&serde_json::Map::new(), &body))
            .expect("a spliced body serializes");
        assert_eq!(untouched, body, "no effort means the wire's body exactly");
    }
}
