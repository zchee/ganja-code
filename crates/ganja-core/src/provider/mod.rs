//! Sources of assistant text.
//!
//! A provider turns a [`ChatRequest`] into a stream of [`ProviderEvent`]s; the
//! engine is what maps those onto the protocol frontends see. Four wires ship:
//! [`FakeProvider`] for demos and tests, [`AnthropicProvider`] for the Messages
//! API, [`OpenAiProvider`] for anything speaking OpenAI chat completions, and
//! [`ResponsesProvider`] for the Responses API a ChatGPT subscription answers
//! on. The last two are the same vendor and answer to the same
//! [`Provider::id`]; which of them a session gets is decided by the credential
//! it has, in [`select`].
//!
//! Two more providers are chat completions under another name, and are
//! deliberately not second wires: [`GrokProvider`] is xAI's endpoint and
//! [`CopilotProvider`] is GitHub's, each a base URL, a credential source and —
//! for Copilot — a header set over [`OpenAiProvider`].
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
    collections::VecDeque,
    env::{self, VarError},
    fmt,
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
    config::{Config, split_model},
    protocol::{FinishReason, Message, Part, PartBody, Usage},
    provider::sse::Frame,
    tool::ToolDefinition,
};

pub mod anthropic;
pub mod copilot;
pub mod fake;
pub mod grok;
pub mod openai;
pub mod responses;
pub mod retry;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use copilot::CopilotProvider;
pub use fake::FakeProvider;
pub use grok::GrokProvider;
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;

/// Environment variable naming the provider a session talks to.
pub const PROVIDER_ENV: &str = "GANJA_PROVIDER";

/// Environment variable overriding the model a session asks for.
pub const MODEL_ENV: &str = "GANJA_MODEL";

/// Every value [`PROVIDER_ENV`] accepts.
pub const PROVIDERS: [&str; 5] = [anthropic::ID, openai::ID, grok::ID, copilot::ID, fake::ID];

/// One request to a model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    /// Model identifier, spelled the way the provider expects it on the wire.
    pub model: String,
    /// What the model is told before it is told anything else, composed by
    /// [`crate::instruction::system_prompt`] and installed with
    /// [`Engine::with_system`](crate::Engine::with_system). [`None`] is an
    /// engine nobody configured, and the shape every scripted run asks in.
    pub system: Option<String>,
    /// The conversation so far, oldest first, ending with the message the user
    /// just sent.
    pub messages: Vec<Message>,
    /// Tools the model may call, advertised on every request. Empty means the
    /// model is not offered any.
    pub tools: Vec<ToolDefinition>,
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
/// The tool variants exist so that P3 can execute tool calls without reshaping
/// the trait; the engine currently ignores them, because no protocol part
/// renders them yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The next fragment of the reply.
    TextDelta(String),
    /// The next fragment of the model's thinking.
    ReasoningDelta(String),
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
}

/// The credential one request presents, whatever kind of credential it is.
///
/// An API key and an OAuth access token are the same thing by the time they
/// reach here: a secret that goes into a header and that must be scrubbed out
/// of anything the provider says back. The difference between them is *where
/// they come from*, and that is [`Credential`]'s business rather than this
/// type's.
///
/// The only way to read one is [`Presented::expose`], which is the single place
/// in this crate's provider code that calls `expose_secret`, so that a grep for
/// either finds every place a credential leaves the type. Everything else —
/// [`fmt::Debug`], and therefore every `tracing` field that renders a provider
/// — sees a placeholder, and the material is wiped when the last handle to it
/// drops.
#[derive(Clone)]
struct Presented(SecretString);

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
enum Credential {
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

impl Credential {
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

impl fmt::Debug for Credential {
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
/// Shared with [`crate::config`], which asks this about an MCP server's
/// endpoint. What is shared is the predicate and not the refusal: each caller
/// keeps its own parse and its own message, because the message is the part a
/// person reads and the two are about different things.
pub(crate) fn reachable_in_the_clear(url: &Url) -> bool {
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
async fn open<M: Mapper>(
    client: &reqwest::Client,
    request: reqwest::Request,
    presented: &Presented,
    cancel: CancellationToken,
    mapper: M,
) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
    let response = match retry::send(client, request, presented, &cancel).await {
        Ok(response) => response,
        Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
        Err(error) => return Err(error),
    };

    Ok(events(response.bytes_stream().boxed(), cancel, mapper))
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

/// A provider together with the model to ask, and anything the user should be
/// told about how the two were picked.
pub struct Selection {
    /// The provider to drive the session with.
    pub provider: Arc<dyn Provider>,
    /// Model identifier handed to every [`ChatRequest`].
    pub model: String,
    /// Set when the provider was defaulted rather than requested.
    pub notice: Option<String>,
}

impl fmt::Debug for Selection {
    /// Renders what was chosen, never how it authenticates: [`Provider`] has no
    /// way to hand a credential back, so there is nothing here to leak.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Selection")
            .field("provider", &self.provider.id())
            .field("model", &self.model)
            .field("notice", &self.notice)
            .finish()
    }
}

/// The environment does not describe a session this build can run.
#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    /// [`PROVIDER_ENV`] names a provider this build does not have.
    #[error("unsupported {PROVIDER_ENV}={requested:?}; this build ships {}", PROVIDERS.join(", "))]
    Unknown {
        /// What the variable said.
        requested: String,
    },
    /// The provider was named but cannot be talked to.
    #[error(transparent)]
    Unusable(#[from] ProviderError),
    /// The model catalog has no default for a provider this build ships, which
    /// is a gap in the table rather than anything the user did.
    #[error("no default model for {provider}; name one with {MODEL_ENV}")]
    NoDefaultModel {
        /// Provider the catalog has no default for.
        provider: String,
    },
}

/// Looks up the API key for `provider_id`.
///
/// [`auth::credential_for`] reads the same environment variables this used to
/// read directly, and layers the stored `auth.json` underneath them, so an
/// exported key still overrides a stored one for a single run. It reads only
/// keys; a provider whose credential is a pair of OAuth tokens is served by
/// [`Credential::Oauth`] instead, which is a different lookup because it is a
/// different thing to look up.
///
/// A store that could not be read is [`Err`], not [`Ok(None)`]: "you have no
/// credential" and "you have one and it was refused" need different things
/// from the person reading the message, and only the second can say what to
/// fix. Reporting it here rather than logging it is what gets the reason in
/// front of someone who is looking at a terminal, not a log file.
fn key_for(provider_id: &str) -> Result<Option<Presented>, ProviderError> {
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

/// Which OpenAI wire a session speaks, decided by the credential it has.
///
/// One vendor, two request/response shapes, and the credential is what picks
/// between them — which is upstream's arrangement too: the ChatGPT plugin's
/// loader looks at `ctx.auth?.type` and only rewrites the request when it finds
/// an OAuth one (`plugin/openai/codex.ts:331`, `:356`).
///
/// - **A key** — exported, or stored, in exactly the order [`key_for`] has
///   always read them — keeps the session on chat completions against
///   `api.openai.com`. Nothing about that path changes, including which error
///   a bad base URL produces, because it is still [`OpenAiProvider::from_env`]
///   that builds it.
/// - **No key but a stored ChatGPT login** speaks Responses against the backend
///   that credential was minted for. Only its *presence* is read here; the
///   token itself is resolved per request, so this costs one small file and
///   captures nothing.
/// - **Neither** is the startup failure it has always been, naming the variable
///   and the login — [`require_key`]'s message, reached by the same call.
///
/// A store that cannot be read is reported rather than treated as "no
/// credential": those are different situations needing different repairs, and
/// only the second can say what to fix.
fn openai_provider() -> Result<Arc<dyn Provider>, ProviderError> {
    if key_for(openai::ID)?.is_some() {
        return Ok(Arc::new(OpenAiProvider::from_env()?));
    }

    let stored =
        auth::oauth_for(openai::ID).map_err(|error| ProviderError::Auth(error.to_string()))?;
    if stored.is_some() {
        return Ok(Arc::new(ResponsesProvider::from_stored()?));
    }

    Ok(Arc::new(OpenAiProvider::from_env()?))
}

/// Whether the provider `provider_id` serves a model called `model`.
///
/// The catalog is the only thing that knows, and it does not know every
/// provider — the built-in fake one is not in it, and neither is whatever a test
/// drives. A provider the catalog says nothing about cannot be contradicted, so
/// any model it is asked for is taken at its word; refusing every switch there
/// would make the command untestable in exactly the runs that are cheapest to
/// run.
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
pub(crate) fn adopt(provider_id: &str, spelled: &str) -> Option<String> {
    let model = spelled.split_once('/').map_or(spelled, |(_, rest)| rest);

    serves(provider_id, model).then(|| model.to_owned())
}

pub(crate) fn serves(provider_id: &str, model: &str) -> bool {
    let mut known = crate::catalog::models()
        .filter(|known| known.provider_id == provider_id)
        .peekable();

    match known.peek() {
        Some(_) => known.any(|known| known.id == model),
        None => !model.trim().is_empty(),
    }
}

/// Reads `variable`, treating an empty value as unset.
fn setting(variable: &str) -> Option<String> {
    env::var(variable)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Resolves the provider named by [`PROVIDER_ENV`] and the model named by
/// [`MODEL_ENV`].
///
/// An unset [`PROVIDER_ENV`] selects the fake provider and reports a notice, so
/// that a bare `cargo run` still demonstrates a streamed reply while making
/// clear that nothing real is being asked.
///
/// Equivalent to [`select`] with a config that asks for nothing, which is what
/// it is: the environment is one tier of a chain, and this is the chain with
/// every other tier empty.
///
/// # Errors
///
/// Returns [`SelectionError`] when the variable names a provider this build
/// does not have, or names one whose credentials are missing. Both fail here,
/// before the terminal is put into raw mode, so that the message is readable.
pub fn from_env() -> Result<Selection, SelectionError> {
    select(&Config::default())
}

/// Resolves the provider and model a session runs on.
///
/// Four tiers, and the first one that says something wins each half of the
/// answer separately — a flag may name the model while the config names the
/// provider:
///
/// 1. `--model`, carried on [`Config::overrides`];
/// 2. [`PROVIDER_ENV`] and [`MODEL_ENV`], where an empty [`MODEL_ENV`] counts
///    as unset and an empty [`PROVIDER_ENV`] is a provider nothing ships;
/// 3. [`Config::model`], `"provider/model"`, split on its first slash;
/// 4. the catalog's default model for whichever provider the tiers above
///    named — and, when none of them named one at all, the built-in fake
///    provider with a notice saying so.
///
/// # Errors
///
/// Returns [`SelectionError`] when a provider is named that this build does not
/// have, or one whose credentials are missing, or one the catalog has no
/// default model for. All of them fail here, before the terminal is put into
/// raw mode, so that the message is readable.
pub fn select(config: &Config) -> Result<Selection, SelectionError> {
    let flag = config.overrides.model.as_deref().map(split_model);
    let file = config.model.as_deref().map(split_model);

    let environment = match env::var(PROVIDER_ENV) {
        // Not `setting`: an exported-but-empty `GANJA_PROVIDER` is a mistake
        // worth naming rather than a variable to look past, and it reaches the
        // "no such provider" refusal below saying exactly what it was set to.
        Ok(requested) => Some(requested),
        Err(VarError::NotUnicode(requested)) => {
            return Err(SelectionError::Unknown {
                requested: requested.to_string_lossy().into_owned(),
            });
        }
        Err(VarError::NotPresent) => None,
    };

    // Each half falls through the tiers on its own. A flag naming a bare model
    // leaves the provider to whatever named one next.
    let requested = flag
        .and_then(|(provider, _)| provider)
        .map(str::to_owned)
        .or(environment)
        .or_else(|| file.and_then(|(provider, _)| provider).map(str::to_owned));
    let named_model = flag
        .map(|(_, model)| model.to_owned())
        .or_else(|| setting(MODEL_ENV))
        .or_else(|| file.map(|(_, model)| model.to_owned()));

    let Some(requested) = requested else {
        return Ok(Selection {
            provider: Arc::new(FakeProvider::default()),
            model: named_model.unwrap_or_else(|| fake::MODEL.to_owned()),
            notice: Some(format!(
                "{PROVIDER_ENV} is unset - replying from the built-in {} provider",
                fake::ID
            )),
        });
    };

    let provider: Arc<dyn Provider> = match requested.as_str() {
        fake::ID => Arc::new(FakeProvider::default()),
        anthropic::ID => Arc::new(AnthropicProvider::from_env()?),
        openai::ID => openai_provider()?,
        grok::ID => Arc::new(GrokProvider::from_stored()?),
        // Grok's construction shape, and grok's posture with it: neither reads
        // a token here, so a session with no stored login is built and fails at
        // its first request, with the message that names the login. What
        // Copilot does read is which deployment its login was against, because
        // that decides the endpoint rather than the credential.
        copilot::ID => Arc::new(CopilotProvider::from_stored()?),
        _ => return Err(SelectionError::Unknown { requested }),
    };

    // The catalog owns the defaults, so a session that names no model still
    // asks for one whose context window and price this build knows.
    let model = match named_model {
        Some(model) => model,
        None if requested == fake::ID => fake::MODEL.to_owned(),
        None => catalog::default_model(&requested)
            .ok_or(SelectionError::NoDefaultModel {
                provider: requested,
            })?
            .to_owned(),
    };

    Ok(Selection {
        provider,
        model,
        notice: None,
    })
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::{StreamExt as _, stream};
    use tokio_util::sync::CancellationToken;

    use super::{
        Credential, Mapper, Presented, ProviderError, ProviderEvent, check_base_url, events,
        shown_base_url, sse::Frame, unusable,
    };
    use crate::{auth, protocol::FinishReason};

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
        let held = Credential::Key(key);
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
}
