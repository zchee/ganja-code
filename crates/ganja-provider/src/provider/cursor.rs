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
//! prompt channel cursor's agent honors. The server's *other* execs are the
//! tools it asks a client to run for it; ganja runs its tools for its own
//! session, so those are answered with a structured refusal naming the kind
//! (`request::refusal_answer`, **D486**) — never run, and never left to
//! hang the turn. What is still deliberately not here is the
//! conversation-state machinery that carries history and tool calls on
//! cursor's content-addressed blob channel; `request`'s module docs say
//! why.
//!
//! The provider rides the uncataloged tier, so a session must be told which
//! model to ask for; [`CursorWire::usable_models`] is the listing that says
//! what the seat may name. Construction reads nothing — grok's posture — and
//! the stored login is read per request, so a login that happens after a
//! session starts is picked up by its next request.

use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use futures::{Stream, StreamExt as _, channel::mpsc, stream, stream::BoxStream};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    provider::{
        ChatRequest, CredentialSource, Presented, Provider, ProviderError, ProviderEvent,
        check_base_url, client, endpoint, is_terminal, retry, shielded, shown_base_url,
    },
};

mod connect;
mod decode;
mod request;

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

/// The identity `GANJA_PROVIDER=cursor` selects.
///
/// Fieldless because the selection layer builds it as a bare name:
/// construction reads nothing, and every request builds a [`CursorWire`]
/// from the stored login at the moment it is needed. Fieldless is also what
/// keeps its `Debug` trivially clean; the wire's own `Debug` is where the
/// no-secrets posture is held.
#[derive(Debug, Default)]
pub struct CursorProvider;

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
        CursorWire::from_stored()?.stream(request, cancel).await
    }
}

/// The wire itself: an endpoint, a client, and the credential source every
/// request resolves afresh.
///
/// Split from [`CursorProvider`] so a test can point it at a loopback
/// socket; the unit struct above is the one the selection layer names.
pub struct CursorWire {
    client: reqwest::Client,
    base_url: String,
    credential: CredentialSource,
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
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self {
            client: client()?,
            base_url,
            credential: CredentialSource::Oauth {
                provider_id: ID,
                refresh,
            },
        })
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
        let built = self.build(
            MODELS_PATH,
            false,
            Vec::new().into(),
            &presented,
            &request::fresh_id()?,
        )?;
        // A listing has no cancel channel of its own, so the retry driver
        // rides under a token nothing fires.
        let never = CancellationToken::new();
        let response = retry::send(&self.client, built, &presented, &never).await?;
        // The retry driver owns every failure up to the first byte; this read
        // after it is the one the unary RPC still makes whole.
        let body = response.bytes().await.map_err(retry::transport)?;

        decode::model_list(&body)
    }

    /// One turn: the run request out on a body held open for the exec
    /// answers the server asks for mid-stream, events in as it produces
    /// them.
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
        let opening = connect::envelope(&request::run_message(&request)?);
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
            let (answers, body) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
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
            events(
                response.bytes_stream().boxed(),
                cancel,
                Duplex {
                    answers,
                    system: request.system.clone(),
                    blobs: HashMap::new(),
                },
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
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", presented.expose()),
            )
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
                if streaming {
                    STREAMING_CONTENT_TYPE
                } else {
                    UNARY_CONTENT_TYPE
                },
            );
        if streaming {
            built = built.header("connect-protocol-version", "1");
        }

        built.body(body).build().map_err(|error| {
            ProviderError::Transport(presented.redact(&format!("malformed request: {error}")))
        })
    }
}

/// The client half of the Run duplex: the sender feeding the held-open
/// request body, and the system prompt the context answer carries on it.
///
/// The fold owns it, so its lifetime is the event stream's: when the stream
/// is dropped — after a clean finish, a failure, or a cancel alike — the
/// sender goes with it, the channel closes, and the request body ends.
/// That is the body's only close, which is what "a cancel mid-duplex ends
/// the stream without a verdict and closes the request body" cashes out to.
struct Duplex {
    answers: mpsc::UnboundedSender<Result<Vec<u8>, Infallible>>,
    system: Option<String>,
    /// The turn's blob store: what the server asked this client to hold
    /// mid-turn, read back by the server's own gets. Per-turn on purpose —
    /// this build carries no conversation state across turns, so every turn
    /// starts the way the plugin's fresh conversation does, holding nothing
    /// (proxy.ts:585) — and a get before any set is answered not-found
    /// rather than failed, because an empty store is a state the server
    /// itself put there.
    blobs: HashMap<Vec<u8>, Vec<u8>>,
}

/// Drives the body's chunks through the Connect splitter and the mapping,
/// handing out each frame's events the moment the frame completes.
///
/// The duplex's answer path rides here too: a frame that decodes to one of
/// the server's asks — the exec channel's context ask, the kv channel's
/// blob exchanges — is answered on the held-open request body before the
/// next frame is read, because the server holds generation until the
/// answer lands. An ask the body can no longer carry an answer to fails the
/// turn readably — an unanswered ask is the silent hang the 2026-08-10
/// live turns died of, once on the exec channel and once on the kv channel,
/// and never again an outcome.
///
/// The cancellation posture is the SSE fold's, verbatim: the token is
/// checked before handing out a buffered event as well as before pulling a
/// new chunk, so a cancel cannot be outrun by frames that were already
/// parsed, and a terminal event drops whatever decoded behind it. Split
/// from [`CursorWire::stream`] so a fixture drives exactly the pipeline a
/// live turn runs — both directions of it — minus the socket.
fn events<S, C, E>(
    chunks: S,
    cancel: CancellationToken,
    duplex: Duplex,
) -> BoxStream<'static, ProviderEvent>
where
    S: Stream<Item = Result<C, E>> + Send + Unpin + 'static,
    C: AsRef<[u8]> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    /// Everything the fold carries between polls.
    struct State<S> {
        chunks: S,
        splitter: connect::Splitter,
        mapping: decode::Mapping,
        cancel: CancellationToken,
        duplex: Duplex,
        /// Events already decoded, not yet handed out.
        ready: VecDeque<ProviderEvent>,
        /// Reused so that mapping a frame does not allocate.
        scratch: Vec<ProviderEvent>,
        done: bool,
    }

    stream::unfold(
        State {
            chunks,
            splitter: connect::Splitter::default(),
            mapping: decode::Mapping::default(),
            cancel,
            duplex,
            ready: VecDeque::new(),
            scratch: Vec::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                // Checked before handing out a buffered event as well as
                // before pulling a new chunk, so that a cancel cannot be
                // outrun by frames that were already parsed.
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

                let chunk = tokio::select! {
                    biased;
                    () = state.cancel.cancelled() => return None,
                    chunk = state.chunks.next() => chunk,
                };

                state.scratch.clear();
                match chunk {
                    Some(Ok(chunk)) => {
                        state.splitter.push(chunk.as_ref());
                        loop {
                            match state.splitter.frame() {
                                Ok(Some(frame)) => {
                                    let Some(ask) = state.mapping.frame(&frame, &mut state.scratch)
                                    else {
                                        continue;
                                    };
                                    // Answered the moment it decodes: the
                                    // server holds generation until the
                                    // reply lands on the body the run
                                    // request opened. Both kinds go out on
                                    // that one channel in frame order, so a
                                    // kv answer can never overtake the
                                    // context answer ahead of it.
                                    // A refusal is two messages where the
                                    // other two asks are one, so every arm
                                    // hands over a list: the throw and the
                                    // stream close must reach the body in
                                    // that order and with nothing between
                                    // them.
                                    let (answers, asked) = match ask {
                                        decode::Ask::Context(ask) => (
                                            vec![request::context_answer(
                                                ask,
                                                state.duplex.system.as_deref(),
                                            )],
                                            "context ask",
                                        ),
                                        decode::Ask::Kv(ask) => (
                                            vec![request::kv_answer(ask, &mut state.duplex.blobs)],
                                            "kv ask",
                                        ),
                                        decode::Ask::Refuse(ask) => {
                                            (request::refusal_answer(&ask), "tool exec")
                                        }
                                    };
                                    let closed = answers.into_iter().any(|answer| {
                                        let enveloped = connect::envelope(&answer);
                                        state.duplex.answers.unbounded_send(Ok(enveloped)).is_err()
                                    });
                                    if closed {
                                        // A body nothing holds open cannot
                                        // carry the answer, and an
                                        // unanswered ask is a hang — so
                                        // the turn fails, readably.
                                        state.done = true;
                                        state.scratch.push(ProviderEvent::Failed(
                                            ProviderError::Transport(format!(
                                                "the request body closed before the server's \
                                                 {asked} could be answered"
                                            )),
                                        ));
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(error) => {
                                    state.done = true;
                                    state.scratch.push(ProviderEvent::Failed(error));
                                    break;
                                }
                            }
                        }
                    }
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
                        state.mapping.truncated(&mut state.scratch);
                    }
                }

                state.ready.extend(state.scratch.drain(..));
            }
        },
    )
    .boxed()
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
        stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(body)]),
        cancel,
        Duplex {
            answers,
            system: None,
            blobs: std::collections::HashMap::new(),
        },
    )
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
