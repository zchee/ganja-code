//! The cursor wire: cursor's agent backend over the Connect protocol.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md`, the record of a live
//! probe against `api2.cursor.sh` — this wire has no upstream TypeScript to
//! port, so the recorded wire facts are its specification the way an
//! upstream file is every other wire's. What they pin: the dialect is
//! Connect (bare `application/proto` on unary RPCs, the enveloped
//! `application/connect+proto` on streaming ones), the failure of a stream
//! arrives as an in-body EndStream frame rather than HTTP/2 trailers, and
//! bare gRPC never reaches the application at all. The framing is
//! hand-written in `connect` over the same `reqwest`/rustls stack every
//! other wire sends with — small enough to own, and the unary path needs no
//! framing at all.
//!
//! The messages are carried by [`buffa`] — Anthropic's pure-Rust, Apache-2.0
//! protobuf runtime, whose license matches this workspace's. It is
//! codegen-only by design, so the shapes live in `cursor.proto` (ganja's
//! own, authored against the recorded facts; its header says what was
//! derived from where) and the generated Rust is checked in under [`proto`],
//! regenerated and diffed by a drift test.
//!
//! **Streamed as it arrives.** The Run body is cut into Connect frames the
//! moment the transport hands bytes over (`connect::Splitter`), each frame
//! mapped onto events (`decode::Mapping`) and handed to the session while
//! the server is still talking. The request that opens the exchange retries
//! before the first byte only — a fresh body per attempt around the shared
//! driver, because a streamed body cannot be replayed — and a cancel
//! mid-stream ends the stream without a verdict and closes the request
//! body, which is the engine's cue to call the turn cancelled rather than
//! failed.
//!
//! **The Run RPC is a duplex.** The request body is a held-open stream, not
//! a sent-and-done message: the run request goes out first, then the body
//! waits, because the server answers a bare turn by *asking* — a mid-stream
//! `requestContextArgs` exec it will not generate past until the client
//! replies (the 2026-08-10 live turn hung in silence on exactly that,
//! skipped). The reply (`request::context_answer`) echoes the exec ids
//! and carries `ChatRequest.system` on `RequestContext.cloud_rule`, the one
//! prompt channel cursor's agent honors, beside the switchboard that tells
//! the server which asks are worth making at all.
//!
//! **Ganja's tools run on this wire** (**D552**). The server's *other* execs
//! are the tools it asks a client to run for it, and this client answers
//! them with its own: the roster on `ChatRequest.tools` is declared on the
//! run request and on every context answer (`request::declaration`), an
//! `mcp_args` naming one of those tools and the seven native kinds of the
//! redirect table (`native`) are surfaced to ganja's engine as ordinary tool
//! calls, and the Run is **held open** while the engine runs them (`bridge`)
//! — under the session's own permission dialogs, rules and transcript, which
//! is why nothing is executed inside this crate. Every kind the roster has no
//! tool for keeps D550's typed refusal (`request::refusal_answer`), never run
//! here and never left to hang the turn. The rulings behind both — D550's and
//! D552's — are stated in full in `crates/ganja-provider/AGENTS.md`.
//!
//! What is still deliberately not here is the conversation-state machinery
//! that carries history on cursor's content-addressed blob channel;
//! `request`'s module docs say why, and `bridge`'s say what it costs a resume
//! whose held Run is gone.
//!
//! The provider rides the uncataloged tier, so a session must be told which
//! model to ask for; [`CursorWire::usable_models`] is the listing that says
//! what the seat may name. Construction reads nothing — grok's posture — and
//! the stored login is read per request, so a login that happens after a
//! session starts is picked up by its next request.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use buffa::Message as _;
use futures::channel::mpsc;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt as _, stream};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::auth::{self, RefreshOauth};
use crate::protocol::FinishReason;
use crate::provider::{
    ChatRequest, CredentialSource, Presented, Provider, ProviderError, ProviderEvent,
    check_base_url, client, endpoint, is_terminal, retry, shielded, shown_base_url,
};
use crate::tool::ToolDefinition;

mod bridge;
mod connect;
mod decode;
pub mod history;
mod native;
mod request;
pub mod value;

/// The cursor wire's protobuf messages, generated from `cursor.proto` by
/// `buffa`'s codegen and checked in.
///
/// `@generated` — never edited by hand; `buf generate` rewrites it and the
/// drift test in this module's tests proves the checked-in copy still
/// matches the `.proto`.
///
/// The suppression is for the generated decoders of fieldless messages, whose
/// unknown-field handling is a one-arm match: the codegen's own allow list
/// covers its view module but not these, and the alternative is hand-editing
/// a file whose whole contract is that nobody does.
#[expect(
    clippy::match_single_binding,
    reason = "generated fieldless-message decoders reduce to a one-arm match"
)]
pub mod proto {
    include!("cursor/ganja.cursor.v1.rs");
}

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this
/// provider. [`auth::cursor::PROVIDER_ID`] rather than a second literal, for
/// grok's reason: a login writing under one name while a provider reads
/// under another fails as a storage bug and is debugged as one.
pub const ID: &str = auth::cursor::PROVIDER_ID;

/// Where cursor's agent backend lives, as the live probe reached it.
pub const DEFAULT_BASE_URL: &str = "https://api2.cursor.sh";

/// The model listing, the service's cheapest unary RPC.
const MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";

/// The chat turn, a streaming RPC.
const RUN_PATH: &str = "/agent.v1.AgentService/Run";

/// What a unary RPC carries: bare protobuf, both directions.
const UNARY_CONTENT_TYPE: &str = "application/proto";

/// What a streaming RPC carries: Connect-enveloped protobuf.
const STREAMING_CONTENT_TYPE: &str = "application/connect+proto";

/// The client the recorded requests identified as, live-confirmed accepted.
/// One constant so the day the server starts gating on it there is one
/// place to move.
const CLIENT_VERSION: &str = "cli-2026.01.09-231024f";

/// How long the fold keeps reading after a bridgeable exec is in hand, before
/// it pauses.
///
/// The server issues **concurrent** execs — two `grep_args` arrived within 5 ms
/// of each other on a recorded run — so pausing on the first one would hand
/// the engine one call, run it, resume, and immediately pause again on the
/// second. Waiting for a short quiet lets a batch the server sent together
/// ride one step, which is also how the engine runs them: concurrently, up to
/// `agents.concurrency`.
///
/// Deliberately short. It is a gap between frames the server already sent, not
/// a poll interval, and every millisecond of it is latency added to a turn.
const GATHER_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether this turn can serve a `fetch_args` exec, which is the value the
/// context answer sends as `RequestContext.web_fetch_enabled = 24`.
///
/// **The roster is the honest local signal.** That switchboard member asks
/// whether the server should generate fetch execs at all, and this client
/// can serve one exactly when it has a `webfetch` to redirect it to — which
/// is exactly when the engine advertised a tool roster on *this* request. A
/// request carrying no tools is one the engine is not offering tools for,
/// and a one-shot title or summary turn (`turn_start == 0`, empty
/// [`ChatRequest::tools`]) correctly draws no fetch execs.
///
/// It is deliberately a predicate over the request rather than a fact about
/// the provider: this crate declares no dependency on the engine and is
/// forbidden one by `depgate.toml`, so a fact the engine owns cannot be
/// named here. The engine cross-checks the two agree; see its own tests.
///
/// `pub` for that cross-check, which lives in the one crate that can see
/// both sides.
#[must_use]
pub fn serves_fetch(request: &ChatRequest) -> bool {
    serves_fetch_for(&request.tools)
}

/// The same verdict off the roster alone, for the answer that sends it —
/// `request::context_answer` holds the roster and not the request. One
/// predicate, so narrowing it later (bead `ganja-code-tcfb`) is one edit.
fn serves_fetch_for(tools: &[ToolDefinition]) -> bool {
    !tools.is_empty()
}

/// The identity `GANJA_PROVIDER=cursor` selects.
///
/// It owns two things a [`CursorWire`] cannot own for itself. The first is
/// **where the wire points**: [`default`](Self::default) is the stored login
/// at cursor's own endpoint, and [`at`](Self::at) is a loopback a test drives.
/// The second is the **held-run table** (`bridge`), which has to outlive any
/// one `stream()` call — a Run paused for a tool call is resumed by the *next*
/// call, and the two are different `CursorWire`s built from this one provider.
///
/// The engine clones one `Arc<dyn Provider>` for every turn a session runs,
/// subagents included (`Turn::child`), so every wire built here shares that
/// one table; `bridge::HeldRuns` says what sharing it buys.
#[derive(Default)]
pub struct CursorProvider {
    /// Where the wire points, or [`None`] for the stored login at
    /// [`DEFAULT_BASE_URL`] — which is what a shipped session uses.
    endpoint: Option<Endpoint>,
    held: Arc<bridge::HeldRuns>,
}

/// An endpoint a caller pointed a provider at, with the credential its
/// requests present.
struct Endpoint {
    base_url: String,
    credential: CredentialSource,
}

impl fmt::Debug for CursorProvider {
    /// Renders where it points and nothing else — no credential ever reaches
    /// this type, and the held table's contents are somebody's conversation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let endpoint = self
            .endpoint
            .as_ref()
            .map_or_else(|| shown_base_url(DEFAULT_BASE_URL), |at| shown_base_url(&at.base_url));

        formatter.debug_struct("CursorProvider").field("base_url", &endpoint).finish()
    }
}

impl CursorProvider {
    /// A provider pointed at an endpoint of the caller's choosing, presenting
    /// a credential the caller supplies — which is how a test drives a whole
    /// bridged turn against a loopback socket.
    ///
    /// **Store-free on purpose** (**D552**, Dv-11). The credential is a value
    /// rather than a lookup, so a suite built on this has no code path to
    /// `auth.json` at all: an engine-level bridge test needs a token but has no
    /// business owning a credential store, and the alternative —
    /// [`CredentialSource::Oauth`], which resolves per request — would make
    /// every such test redirect `XDG_DATA_HOME`, whose documented invariant is
    /// one test per binary. [`CredentialSource::key`] is the door.
    /// [`from_stored`](CursorWire::from_stored) reaches the same construction
    /// with an `Oauth` source, so the shipped path is this path.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when `base_url` is somewhere an
    /// access token may not travel — the rule every other provider's endpoint
    /// is held to, checked here so a bad endpoint is refused at construction
    /// rather than at the first turn.
    pub fn at(
        base_url: impl Into<String>,
        credential: CredentialSource,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self { endpoint: Some(Endpoint { base_url, credential }), held: Arc::default() })
    }

    /// The wire one request runs on, sharing this provider's held-run table.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] as [`CursorWire::from_stored`] does on the
    /// shipped path; the endpoint-pointed path only builds a client.
    fn wire(&self) -> Result<CursorWire, ProviderError> {
        let mut wire = match &self.endpoint {
            Some(at) => CursorWire::presenting(&at.base_url, at.credential.clone())?,
            None => CursorWire::from_stored()?,
        };
        wire.held = Arc::clone(&self.held);

        Ok(wire)
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.wire()?.stream(request, cancel).await
    }
}

/// The wire itself: an endpoint, a client, the credential source every
/// request resolves afresh, and the Runs this session is holding open.
///
/// Split from [`CursorProvider`] so a test can point it at a loopback
/// socket; the provider above is the one the selection layer names.
pub struct CursorWire {
    client: reqwest::Client,
    base_url: String,
    credential: CredentialSource,
    /// The held-run table. Its own by default — a wire built straight through
    /// [`at`](Self::at) is one nobody shares — and replaced by the provider's
    /// when [`CursorProvider::wire`] builds one, which is every shipped turn.
    held: Arc<bridge::HeldRuns>,
}

impl fmt::Debug for CursorWire {
    /// Renders which endpoint and which kind of credential, never the
    /// credential — the same rule every other wire's `Debug` holds.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CursorWire")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
            .finish()
    }
}

impl CursorWire {
    /// The wire against cursor's own endpoint, for a session that has a
    /// login to run as.
    ///
    /// The store is asked one question — is there a credential at all — and
    /// the answer is discarded once counted; the token a request carries is
    /// still resolved per request, through the shared refresher. Grok's
    /// posture, for grok's reasons: refusing here is what puts the "log in
    /// first" message ahead of a turn instead of inside one.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] when no cursor credential is stored —
    /// naming `ganja auth login cursor`, which is the repair — or when the
    /// store exists and could not be read, which a login does not fix and
    /// the store's own message describes. Returns
    /// [`ProviderError::Transport`] when no HTTP client can be built.
    pub fn from_stored() -> Result<Self, ProviderError> {
        super::require_stored_login(ID)?;

        let refresh = auth::cursor::Refresh::new()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        Self::at(DEFAULT_BASE_URL, Arc::new(refresh))
    }

    /// The wire against `base_url`, presenting whatever credential the caller
    /// resolved — the one constructor behind all three doors,
    /// [`from_stored`](Self::from_stored), [`at`](Self::at) and
    /// [`CursorProvider::at`], so the shipped path is the tested path.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// or when `base_url` is somewhere an access token may not travel.
    fn presenting(
        base_url: impl Into<String>,
        credential: CredentialSource,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self { client: client()?, base_url, credential, held: Arc::default() })
    }

    /// The same wire against an endpoint of the caller's choosing, which is
    /// how a test drives it against a loopback socket.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be
    /// built, or when `base_url` is somewhere an access token may not travel
    /// — the rule every other provider's endpoint is held to.
    pub fn at(
        base_url: impl Into<String>,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<Self, ProviderError> {
        Self::presenting(base_url, CredentialSource::Oauth { provider_id: ID, refresh })
    }

    /// The models the stored login may name, from the live listing.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] as [`stream`](Self::stream) classifies
    /// them: `Auth` for a credential problem, `Transport` when nothing
    /// answered, `Status` when the server refused, `Parse` when the answer
    /// could not be read.
    pub async fn usable_models(&self) -> Result<Vec<proto::ModelEntry>, ProviderError> {
        let presented = self.credential.presented().await?;
        // The request message has no fields, and an empty message encodes to
        // no bytes at all — the zero-byte body the live probe was answered
        // on. A `Vec` body stays replayable, so the shared driver may retry.
        let built =
            self.build(MODELS_PATH, false, Vec::new().into(), &presented, &request::fresh_id()?)?;
        // A listing has no cancel channel of its own, so the retry driver
        // rides under a token nothing fires.
        let never = CancellationToken::new();
        let response = retry::send(&self.client, built, &presented, &never).await?;
        // The retry driver owns every failure up to the first byte; this read
        // after it is the one the unary RPC still makes whole.
        let body = response.bytes().await.map_err(retry::transport)?;

        decode::model_list(&body)
    }

    /// One turn: either a Run held open by an earlier step, read on from
    /// where it paused, or a fresh Run request out on a body held open for
    /// the exec answers the server asks for mid-stream.
    ///
    /// **The resume comes first**, and it is a lookup rather than a guess:
    /// `bridge::HeldRuns::resolve` answers from the request alone, and only a
    /// request whose key matches a held Run *and* whose messages carry a
    /// finished result for every exec that Run is waiting on continues it. A
    /// request that keys nowhere opens a fresh Run and disturbs nothing —
    /// which is what a title one-shot, a compaction summary and every
    /// subagent turn do while a root turn is paused.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the turn cannot start — no credential,
    /// an endpoint that refused or never answered, every retry spent.
    /// Everything after the first byte arrives inside the stream instead: an
    /// in-body EndStream verdict, a dead connection, a frame this build
    /// cannot read and an exec ask it cannot answer all end it with
    /// [`ProviderEvent::Failed`], because by then text may already be on
    /// somebody's screen.
    pub async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let resuming =
            bridge::Key::of(&request).map(|key| Bridge::new(Arc::clone(&self.held), key));

        match self.held.resolve(&request) {
            bridge::Resolution::Resume(fold) => return Ok(run(*fold, cancel, resuming)),
            bridge::Resolution::Closed => {
                return Ok(failed(
                    "the cursor run this turn was resuming closed its request body before the \
                     tool results could be answered",
                ));
            }
            bridge::Resolution::Dead(reason) => return Ok(failed(&reason)),
            bridge::Resolution::Fresh => {}
        }

        // Composed once for the whole turn, before the retry loop the run
        // request already sits ahead of: every attempt names the same blobs,
        // and the store they live in seeds the Run that finally opens.
        let composed = history::compose(&request);
        let opening = connect::envelope(&request::run_message(&request, &composed)?);
        let presented = self.credential.presented().await?;
        // Minted once for the whole turn: every attempt below is the same
        // request under the same stamp, the shape the shared driver's
        // replays have always had.
        let request_id = request::fresh_id()?;

        // A streamed body cannot be replayed, so the shared driver sends
        // each attempt exactly once and this loop owns the schedule — the
        // driver's own, minus its jitter and the retry-after refinement,
        // whose headers the driver consumed with the refusal. The boundary
        // it holds is the one that matters: retries happen before the first
        // byte of a response body only.
        let mut attempt = 1;
        let (response, answers) = loop {
            // A fresh channel per attempt: the previous attempt's body
            // belongs to the request that failed.
            let (answers, body): (Answers, _) = mpsc::unbounded();
            answers
                .unbounded_send(Ok(opening.clone()))
                .expect("the receiver is alive in this scope");
            let built = self.build(
                RUN_PATH,
                true,
                reqwest::Body::wrap_stream(body),
                &presented,
                &request_id,
            )?;

            match retry::send(&self.client, built, &presented, &cancel).await {
                Ok(response) => break (response, answers),
                // A turn the user already left is not a failed one: the
                // engine reads a stream that ends after a cancel as
                // `Cancelled`.
                Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
                Err(error) if attempt < retry::MAX_ATTEMPTS && error.is_retryable() => {
                    tracing::debug!(
                        provider = ID,
                        attempt,
                        "retrying the run request that opens the turn"
                    );
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ok(stream::empty().boxed()),
                        () = tokio::time::sleep(retry::delay(attempt, None)) => {}
                    }
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        };

        // Read before the body is taken, so the failures below are logged
        // against the endpoint they came from. No redirect was followed to
        // get here — the client refuses them — so this is the URL the
        // request was built with.
        let endpoint = endpoint(response.url(), &self.base_url);

        // This wire opens its own request rather than riding the shared
        // `open`, so it has to join `shielded` by hand: every failure it
        // reports arrives in-body, mapped by a decoder that holds no
        // `Presented`, and a server that echoes the token it rejected
        // would otherwise put it on the screen and in the log.
        Ok(shielded(
            run(
                Fold::new(
                    normalize(response.bytes_stream().boxed()),
                    Duplex {
                        answers,
                        system: request.system.clone(),
                        roster: request.tools.clone(),
                        // Moved in rather than cloned: the request bytes
                        // above already carry the ids, and this is the last
                        // reader of the composition.
                        blobs: composed.blobs,
                    },
                ),
                cancel,
                resuming,
            ),
            presented,
            endpoint,
        ))
    }

    /// Builds one RPC's request: the recorded header set, verbatim, over
    /// `body`.
    ///
    /// The split between the two content types — and
    /// `connect-protocol-version` on the streaming RPC only — is exactly
    /// what the live probe measured the server enforcing. The caller mints
    /// `request_id` once per turn and hands it to every build, so a retried
    /// request — the driver's clone of a replayable body, or this wire's
    /// rebuild around a streamed one — is the same request under the same
    /// id rather than a new one wearing a fresh stamp.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when the request cannot be
    /// assembled; nothing was sent.
    fn build(
        &self,
        path: &str,
        streaming: bool,
        body: reqwest::Body,
        presented: &Presented,
        request_id: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut built = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", presented.expose()))
            .header("x-cursor-client-version", CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("x-ghost-mode", "true")
            .header("x-request-id", request_id)
            // Meaningful only to a server that would send trailers — this
            // one does not — but the recorded client sends it and this build
            // identifies as that client.
            .header(reqwest::header::TE, "trailers")
            .header(
                reqwest::header::CONTENT_TYPE,
                if streaming { STREAMING_CONTENT_TYPE } else { UNARY_CONTENT_TYPE },
            );
        if streaming {
            built = built.header("connect-protocol-version", "1");
        }

        built.body(body).build().map_err(|error| {
            ProviderError::Transport(presented.redact(&format!("malformed request: {error}")))
        })
    }
}

/// A stream that reports one failure and ends, for the two states a resume
/// can find instead of a Run to read on.
fn failed(reason: &str) -> BoxStream<'static, ProviderEvent> {
    let failure = ProviderEvent::Failed(ProviderError::Transport(reason.to_owned()));

    stream::once(async move { failure }).boxed()
}

/// The client half of the Run duplex: the sender feeding the held-open
/// request body, and what every answer on it is built from.
///
/// The fold owns it, so its lifetime is the event stream's — except across a
/// pause, where the whole fold moves into the held-run table and the sender
/// travels with it, which is exactly what keeps the request body open for the
/// answer a bridged tool will produce. When the stream is finally dropped —
/// after a clean finish, a failure, a cancel, or a held Run being dropped —
/// the sender goes with it, the channel closes, and the request body ends.
struct Duplex {
    answers: Answers,
    system: Option<String>,
    /// The tools this request declared, which four answers read: the
    /// declaration on every context answer and the `web_fetch_enabled`
    /// verdict beside it, the roster a `tool_not_found` carries, and the
    /// membership test that decides whether a native exec is redirected or
    /// refused.
    roster: Vec<ToolDefinition>,
    /// The Run's blob store, answering the server's kv gets. Seeded with the
    /// **composed history** — every blob the run request's state names
    /// (`history::Composed::blobs`, **D553**) — and holding beside it what
    /// the server asks this client to store mid-turn. Per-Run on purpose:
    /// this build carries no conversation state across turns, so every Run
    /// starts from the transcript it was composed from and nothing else, the
    /// reference's own rebuild-on-every-request ground; a Run held across a
    /// pause carries its store with it, and a resumed Run answers from the
    /// same map it was seeded with. A get for an id nobody composed and
    /// nobody set is answered not-found rather than failed, because a store
    /// holding only what this side minted is a state the server itself is
    /// reading.
    blobs: HashMap<Vec<u8>, Vec<u8>>,
}

impl Duplex {
    /// The registry names this request declared.
    fn names(&self) -> Vec<String> {
        self.roster.iter().map(|tool| tool.name.clone()).collect()
    }

    /// A duplex answering on `answers` and declaring `roster`, for a test that
    /// drives the fold without a socket.
    #[cfg(test)]
    fn for_tests(answers: Answers, roster: Vec<ToolDefinition>) -> Self {
        Self { answers, system: None, roster, blobs: HashMap::new() }
    }

    /// The same, carrying a system prompt for the context answer to put on
    /// `cloud_rule`.
    #[cfg(test)]
    fn speaking(answers: Answers, system: Option<&str>) -> Self {
        Self { system: system.map(str::to_owned), ..Self::for_tests(answers, Vec::new()) }
    }
}

/// The sending half of the request body: what every answer, refusal and
/// heartbeat is written into, and what `reqwest` streams to the server.
type Answers = mpsc::UnboundedSender<Result<Vec<u8>, Infallible>>;

/// Chunks of a response body, normalized to one concrete type.
///
/// The transport hands over `Bytes` and a test hands over `Vec<u8>`; the fold
/// takes neither, because a fold that is generic over its chunk stream cannot
/// be **stored**, and storing it is what a pause is. One boxed stream of owned
/// bytes costs a copy per chunk — the splitter copies into its own buffer
/// anyway — and buys a `Fold` that fits in a table.
type Chunks = BoxStream<'static, Result<Vec<u8>, String>>;

/// Whatever a caller has, as [`Chunks`].
fn normalize<S, C, E>(chunks: S) -> Chunks
where
    S: Stream<Item = Result<C, E>> + Send + 'static,
    C: AsRef<[u8]> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    chunks
        .map(|chunk| chunk.map(|bytes| bytes.as_ref().to_vec()).map_err(|error| error.to_string()))
        .boxed()
}

/// Everything one Run needs to keep reading — and everything a pause has to
/// carry across it.
///
/// It is one struct rather than a closure's captured state precisely so that
/// it can be moved: a bridged exec pauses the stream by lifting this whole
/// value into `bridge::HeldRuns` and dropping the stream around it, and the
/// next request lifts it back out and reads on. The response body, the
/// splitter's half-frame, the mapping's `turn_ended`, the request body's
/// sender and the blob store all have to survive that, and every one of them
/// is here.
struct Fold {
    chunks: Chunks,
    splitter: connect::Splitter,
    mapping: decode::Mapping,
    duplex: Duplex,
    /// Events already decoded, not yet handed out.
    ready: VecDeque<ProviderEvent>,
    /// Reused so that mapping a frame does not allocate.
    scratch: Vec<ProviderEvent>,
    /// The run-level heartbeat, on the fold's own clock while it is reading.
    /// While the Run is *held* the keeper task beats instead — a fold nobody
    /// is polling cannot tick.
    beat: Interval,
    done: bool,
}

impl Fold {
    /// A fold over `chunks`, answering on `duplex`.
    fn new(chunks: Chunks, duplex: Duplex) -> Self {
        Self {
            chunks,
            splitter: connect::Splitter::default(),
            mapping: decode::Mapping::default(),
            duplex,
            ready: VecDeque::new(),
            scratch: Vec::new(),
            beat: beats(bridge::HEARTBEAT),
            done: false,
        }
    }
}

/// An interval whose first tick is one period out rather than immediate — an
/// interval that fired at zero would put a heartbeat ahead of the run
/// request's own first answer for no reason.
fn beats(period: std::time::Duration) -> Interval {
    let mut interval = interval_at(Instant::now() + period, period);
    // A tick missed because the fold was busy decoding fires once, late, and
    // the cadence restarts from it: liveness is a cadence, and a burst to
    // catch up would report nothing extra.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// Where a paused Run is parked, under which key, and the execs gathering
/// towards the pause.
///
/// The batch lives here rather than beside the fold so that a stream with
/// nowhere to park — a fixture replay, or a request with no message to key on
/// — has no batch and no window by construction: it refuses every exec
/// instead, which is what this wire did before the bridge.
struct Bridge {
    held: Arc<bridge::HeldRuns>,
    key: bridge::Key,
    /// Execs waiting to be handed to the engine when the gather window closes.
    pending: Vec<bridge::Pending>,
    /// When the gather window closes, once a bridgeable exec is in hand.
    gather: Option<Instant>,
}

impl Bridge {
    /// A bridge parking under `key` in `held`, with nothing gathering yet.
    fn new(held: Arc<bridge::HeldRuns>, key: bridge::Key) -> Self {
        Self { held, key, pending: Vec::new(), gather: None }
    }
}

/// Everything the event stream carries between polls.
struct Streaming {
    /// The fold, until a pause lifts it out — after which the stream hands out
    /// what it already decided and ends.
    fold: Option<Fold>,
    cancel: CancellationToken,
    /// Events a pause committed to: whatever was decoded, then the tool calls,
    /// then the finish.
    tail: VecDeque<ProviderEvent>,
    /// Where to park on a pause, and `None` for a stream that cannot pause.
    bridge: Option<Bridge>,
}

/// What one turn of the read loop found.
enum Next {
    Cancelled,
    /// The gather window closed: the batch is complete and the Run pauses.
    Gathered,
    /// The run-level heartbeat came due.
    Beat,
    Chunk(Option<Result<Vec<u8>, String>>),
}

/// Drives a fold's chunks through the Connect splitter and the mapping,
/// handing out each frame's events the moment the frame completes.
///
/// The duplex's answer path rides here too: a frame that decodes to one of
/// the server's asks — the exec channel's context ask and its tool execs, the
/// kv channel's blob exchanges — is answered on the held-open request body
/// before the next frame is read, because the server holds generation until
/// the answer lands. An ask the body can no longer carry an answer to fails
/// the turn readably — an unanswered ask is the silent hang the 2026-08-10
/// live turns died of, once on the exec channel and once on the kv channel,
/// and never again an outcome.
///
/// **A bridged exec is answered later, and elsewhere.** It is surfaced as a
/// tool call, the step is finished, and the Run is held — see `bridge`. The
/// answer goes out when the engine's result arrives on the next request.
///
/// The cancellation posture is the SSE fold's, verbatim: the token is
/// checked before handing out a buffered event as well as before pulling a
/// new chunk, so a cancel cannot be outrun by frames that were already
/// parsed, and a terminal event drops whatever decoded behind it.
fn run(
    fold: Fold,
    cancel: CancellationToken,
    bridge: Option<Bridge>,
) -> BoxStream<'static, ProviderEvent> {
    stream::unfold(
        Streaming { fold: Some(fold), cancel, tail: VecDeque::new(), bridge },
        |mut state| async move {
            loop {
                // Checked before handing out a buffered event as well as
                // before pulling a new chunk, so that a cancel cannot be
                // outrun by frames that were already parsed.
                if state.cancel.is_cancelled() {
                    return None;
                }

                // A pause has already decided what this step says; nothing is
                // read again.
                if let Some(event) = state.tail.pop_front() {
                    return Some((event, state));
                }

                // Once per poll: a fold the pause has lifted out is a stream
                // that has said everything it will.
                let Streaming { fold: slot, cancel, tail, bridge } = &mut state;
                let Some(fold) = slot else { return None };

                if let Some(event) = fold.ready.pop_front() {
                    if is_terminal(&event) {
                        fold.done = true;
                        fold.ready.clear();
                        if let Some(bridge) = bridge.as_mut()
                            && !bridge.pending.is_empty()
                        {
                            // The server ended the Run while execs it had
                            // asked for were still gathering. It decided
                            // not to wait, so there is nothing left to
                            // hold open and nothing to resume — but a
                            // model that asked for a tool and got a turn
                            // instead is worth a line rather than silence.
                            tracing::debug!(
                                provider = ID,
                                execs = bridge.pending.len(),
                                "the run ended before its own tool asks could be bridged"
                            );
                            bridge.pending.clear();
                        }
                    }

                    return Some((event, state));
                }
                if fold.done {
                    return None;
                }

                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => Next::Cancelled,
                    () = gathered(bridge.as_ref().and_then(|bridge| bridge.gather)) => {
                        Next::Gathered
                    }
                    _ = fold.beat.tick() => Next::Beat,
                    chunk = fold.chunks.next() => Next::Chunk(chunk),
                };

                match next {
                    Next::Cancelled => return None,
                    // The window fires only while a bridge is gathering, so
                    // both the fold and the bridge are here to take.
                    Next::Gathered => *tail = pause(slot.take()?, bridge.as_mut()?, cancel),
                    Next::Beat => beat(fold),
                    Next::Chunk(chunk) => absorb(fold, bridge, chunk),
                }
            }
        },
    )
    .boxed()
}

/// Waits for the gather window to close, or forever when nothing is gathering.
///
/// The `None` arm is what lets one `select!` serve a step with a bridgeable
/// exec in hand and one without: a branch that never completes is a branch
/// that is not there.
async fn gathered(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Sends one run-level heartbeat on the open request body.
///
/// A body that has closed is not reported here: the turn's own failure arrives
/// the next time an ask cannot be answered, which says the same thing about a
/// state the session can act on.
fn beat(fold: &Fold) {
    let framed = connect::envelope(&request::run_heartbeat().encode_to_vec());
    if fold.duplex.answers.unbounded_send(Ok(framed)).is_err() {
        tracing::debug!(provider = ID, "the request body closed under the run heartbeat");
    }
}

/// Takes one chunk of the body: cuts frames, maps them, answers what must be
/// answered now, and collects what must be answered by the engine.
fn absorb(fold: &mut Fold, bridge: &mut Option<Bridge>, chunk: Option<Result<Vec<u8>, String>>) {
    fold.scratch.clear();
    match chunk {
        Some(Ok(chunk)) => fold.splitter.push(&chunk),
        Some(Err(error)) => {
            fold.done = true;
            fold.scratch.push(ProviderEvent::Failed(ProviderError::Transport(error)));
        }
        None => {
            fold.done = true;
            fold.mapping.truncated(&mut fold.scratch);
        }
    }

    while !fold.done {
        let ask = match fold.splitter.frame() {
            Ok(Some(frame)) => fold.mapping.frame(&frame, &mut fold.scratch),
            Ok(None) => break,
            Err(error) => {
                fold.done = true;
                fold.scratch.push(ProviderEvent::Failed(error));
                break;
            }
        };
        if let Some(ask) = ask
            && !answer(fold, bridge, ask)
        {
            break;
        }
    }

    let decoded = std::mem::take(&mut fold.scratch);
    fold.ready.extend(decoded);
}

/// Answers one ask, or records it as one the engine will answer.
///
/// Returns `false` when the exchange is over — the request body closed, which
/// is a hang if it is not reported — so the caller stops cutting frames.
fn answer(fold: &mut Fold, bridge: &mut Option<Bridge>, ask: decode::Ask) -> bool {
    // Answered the moment it decodes: the server holds generation until the
    // reply lands on the body the run request opened. Every kind goes out on
    // that one channel in frame order, so a kv answer can never overtake the
    // context answer ahead of it, and a refusal's two messages — the rejection
    // and the close — reach the body in that order with nothing between them.
    let (answers, asked) = match ask {
        decode::Ask::Context(ask) => (
            vec![request::context_answer(ask, fold.duplex.system.as_deref(), &fold.duplex.roster)],
            "context ask",
        ),
        decode::Ask::Kv(ask) => (vec![request::kv_answer(ask, &mut fold.duplex.blobs)], "kv ask"),
        decode::Ask::Exec(ask) => match exec(fold, bridge, ask) {
            // Bridged: nothing goes out now, and the gather window is what
            // decides when the step ends.
            Exec::Bridged => return true,
            Exec::Answered(messages) => (messages, "tool exec"),
        },
    };

    let closed = answers.into_iter().any(|answer| {
        let enveloped = connect::envelope(&answer);
        fold.duplex.answers.unbounded_send(Ok(enveloped)).is_err()
    });
    if closed {
        // A body nothing holds open cannot carry the answer, and an
        // unanswered ask is a hang — so the turn fails, readably.
        fold.done = true;
        fold.scratch.push(ProviderEvent::Failed(ProviderError::Transport(format!(
            "the request body closed before the server's {asked} could be answered"
        ))));

        return false;
    }

    true
}

/// What one tool exec became.
enum Exec {
    /// Handed to the engine; the answer goes out on a later request.
    Bridged,
    /// Answered here and now, in the kind's own vocabulary.
    Answered(Vec<Vec<u8>>),
}

/// Decides what one tool exec gets: ganja's own tool, or a refusal.
fn exec(fold: &Fold, bridge: &mut Option<Bridge>, ask: decode::ExecAsk) -> Exec {
    let roster = fold.duplex.names();

    let (call_id, tool, input, answer) = match &ask.args {
        decode::ExecArgs::Mcp(call) => match mcp(call, &roster) {
            Ok((call_id, arguments)) => {
                (call_id, call.called().to_owned(), arguments, native::Answer::Mcp)
            }
            Err(result) => return Exec::Answered(refused_mcp(&ask, *result)),
        },
        args => {
            let Some(bridged) = native::redirect(args, &roster) else {
                // A kind outside the table, or one whose tool this request is
                // not offering, is refused for its *kind*; an exec whose own
                // arguments are the reason says so instead.
                return Exec::Answered(match native::argument_refusal(args) {
                    Some(reason) => request::refusal_answer_because(&ask, reason),
                    None => request::refusal_answer(&ask),
                });
            };
            let Ok(call_id) = request::fresh_id() else {
                // No id, no call the engine could answer. The typed refusal is
                // still an outcome the server's loop reads.
                return Exec::Answered(request::refusal_answer(&ask));
            };

            (call_id, bridged.tool, bridged.input, bridged.answer)
        }
    };

    // A wire that cannot pause cannot bridge: a fixture replay and a request
    // with no message to key on both fall through to a refusal — and a call
    // this client serves is refused for the reason that actually holds, the
    // pause rather than the roster, where every other kind keeps D550's
    // kind-level sentence.
    let Some(bridge) = bridge.as_mut() else {
        return Exec::Answered(match answer {
            native::Answer::Mcp => {
                request::refusal_answer_because(&ask, &request::unbridgeable_reason(&tool))
            }
            _ => request::refusal_answer(&ask),
        });
    };

    tracing::debug!(
        provider = ID,
        exec = ask.id,
        kind = ask.kind,
        tool,
        call = call_id,
        "bridging an exec to a ganja tool"
    );
    bridge.pending.push(bridge::Pending {
        id: ask.id,
        exec_id: ask.exec_id,
        call_id,
        tool,
        input,
        answer,
    });
    // Restarted on every bridged exec, so a burst the server sent together
    // rides one step. Text, thinking and kv frames do not extend it: a chatty
    // server cannot postpone the step it is waiting on.
    bridge.gather = Some(Instant::now() + GATHER_WINDOW);

    Exec::Bridged
}

/// Whether an `mcp_args` is a call this client serves — and if so under which
/// id and with which arguments — or the `McpResult` that refuses it.
///
/// The order is the one the checks have to happen in: a preflight is answered
/// before anything is resolved, and a call for another server is not this
/// client's to look up.
fn mcp(
    call: &decode::McpCall,
    roster: &[String],
) -> Result<(String, serde_json::Value), Box<proto::McpResult>> {
    if call.approval_only {
        // A preflight asks whether the call *would* be allowed, and answering
        // it by running the tool would run a side-effecting `bash` or `write`
        // for a policy question — possibly twice, since the real call follows.
        // So it is approved without executing anything: the real call is what
        // meets ganja's permission dialog, and a deny there becomes `rejected`
        // on that call.
        tracing::debug!(
            provider = ID,
            call = call.tool_call_id,
            "answering a smart-mode approval preflight without executing anything"
        );
        return Err(Box::new(proto::McpResult {
            approved: buffa::MessageField::some(proto::McpApproved::default()),
            ..Default::default()
        }));
    }

    // Absent is not foreign: a server that sent no identifier has not named
    // somebody else's, and the name is what decides the rest.
    if !call.provider_identifier.is_empty()
        && call.provider_identifier != request::PROVIDER_IDENTIFIER
    {
        return Err(Box::new(proto::McpResult {
            server_not_found: buffa::MessageField::some(proto::McpServerNotFound {
                name: Some(call.provider_identifier.clone()),
                available_servers: vec![request::PROVIDER_IDENTIFIER.to_owned()],
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    let Some(arguments) = call.arguments.as_ref() else {
        return Err(Box::new(unreadable()));
    };
    debug_assert!(arguments.is_object(), "an argument map decodes to an object or to nothing");

    let name = call.called();
    if roster.is_empty() {
        // With nothing declared there is no roster to be missing from, and
        // `tool_not_found` carrying an empty list would state that this client
        // publishes one and that the name is not on it. The honest answer is
        // the one that carries only a reason, and the reason names what was
        // called — which is what this wire answered before it published a
        // roster at all.
        return Err(Box::new(proto::McpResult {
            rejected: buffa::MessageField::some(
                proto::McpRejected::default().with_reason(request::mcp_refusal_reason(name)),
            ),
            ..Default::default()
        }));
    }
    if !roster.iter().any(|tool| tool == name) {
        return Err(Box::new(proto::McpResult {
            tool_not_found: buffa::MessageField::some(proto::McpToolNotFound {
                name: Some(name.to_owned()),
                available_tools: roster.to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    // A server that called without minting an id still gets an answer, keyed
    // on one of ours: the engine needs a call id, and an empty one would
    // collide with the next empty one.
    let call_id = if call.tool_call_id.is_empty() {
        request::fresh_id().unwrap_or_else(|_| call.name.clone())
    } else {
        call.tool_call_id.clone()
    };

    Ok((call_id, arguments.clone()))
}

/// The failure an argument map this build cannot read earns — that one call,
/// never the turn.
fn unreadable() -> proto::McpResult {
    proto::McpResult {
        error: buffa::MessageField::some(proto::McpError::default().with_error(
            "ganja could not read the argument values of this call: one of them is a \
             google.protobuf.Value shape this client does not model",
        )),
        ..Default::default()
    }
}

/// One refused `mcp_args`: the result, then the close every exec ends with.
fn refused_mcp(ask: &decode::ExecAsk, result: proto::McpResult) -> Vec<Vec<u8>> {
    let refused = proto::ClientMessage {
        exec_response: buffa::MessageField::some(proto::ExecResponse {
            id: ask.id,
            exec_id: ask.exec_id.clone(),
            mcp_result: buffa::MessageField::some(result),
            ..Default::default()
        }),
        ..Default::default()
    };

    vec![refused.encode_to_vec(), request::stream_close(ask.id).encode_to_vec()]
}

/// Ends the step and holds the Run open for the engine.
///
/// The events are committed here, in this order: whatever the gather window
/// decoded, then each pending exec's `ToolCallStart`/`Delta`/`End`, then the
/// finish. `session.rs` branches on whether the step produced calls rather
/// than on the finish reason, so `Completed` here drives another step — which
/// is the step whose request resumes this Run.
///
/// One `ToolCallDelta` per call, carrying the whole argument object: the exec
/// arrived whole, so there is nothing to stream and nothing gained by
/// pretending otherwise.
///
/// Returns the events; the fold leaves with the held run.
fn pause(
    mut fold: Fold,
    bridge: &mut Bridge,
    cancel: &CancellationToken,
) -> VecDeque<ProviderEvent> {
    let pending = std::mem::take(&mut bridge.pending);
    bridge.gather = None;

    let mut tail = std::mem::take(&mut fold.ready);
    for exec in &pending {
        tail.push_back(ProviderEvent::ToolCallStart {
            id: exec.call_id.clone(),
            name: exec.tool.clone(),
        });
        tail.push_back(ProviderEvent::ToolCallDelta {
            id: exec.call_id.clone(),
            json: exec.input.to_string(),
        });
        tail.push_back(ProviderEvent::ToolCallEnd { id: exec.call_id.clone() });
    }
    tail.push_back(ProviderEvent::Finish(FinishReason::Completed));

    bridge.held.hold(bridge.key.clone(), fold, pending, cancel);
    tail
}

/// Feeds a recorded body through the pipeline a live turn runs.
///
/// Delivering the whole body as one chunk is the worst case for
/// cancellation — every frame already parsed and waiting — which is exactly
/// what the cancel test wants to prove is still stoppable.
#[cfg(test)]
fn replay(body: Vec<u8>, cancel: CancellationToken) -> BoxStream<'static, ProviderEvent> {
    // The answer receiver is dropped on purpose: replayed bodies carry no
    // exec ask, and one that did would fail the turn visibly rather than
    // hang the test.
    let (answers, _) = mpsc::unbounded();
    events(
        stream::iter([Ok::<Vec<u8>, Infallible>(body)]),
        cancel,
        Duplex::for_tests(answers, Vec::new()),
        None,
    )
}

/// The fold over a caller's own chunks, with or without a bridge: `None`
/// refuses every exec, which is what a fixture replay and the pre-bridge tests
/// want; `Some` drives the real pause and the real resume over in-memory
/// channels rather than a socket.
///
/// The seam is `run`'s own arguments and nothing else: what a test builds this
/// way is byte for byte what [`CursorWire::stream`] builds, which is the whole
/// point of having it.
#[cfg(test)]
fn events<S, C, E>(
    chunks: S,
    cancel: CancellationToken,
    duplex: Duplex,
    bridge: Option<Bridge>,
) -> BoxStream<'static, ProviderEvent>
where
    S: Stream<Item = Result<C, E>> + Send + 'static,
    C: AsRef<[u8]> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    run(Fold::new(normalize(chunks), duplex), cancel, bridge)
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
